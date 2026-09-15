/**
 * 书架视图。
 *
 * 布局：左侧分类栏（全部 / 未分类 / 自定义分类）+ 右侧封面网格。
 * 卡片：封面（shelf-cover:// 直出，404 用格式占位图）、标题、进度条、
 *       「续读」角标（0 < percent < 99.5）或「读完」角标。
 * 交互：点击卡片打开书籍；右键上下文菜单（打开 / 设置分类 / 统计收录 / 移除出书架）。
 *
 * 本模块不持有应用状态：打开书籍等动作通过注入的回调交给 main.ts。
 */
import { api, bumpCoverVersion, shelfCoverUrl } from "./ipc";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import bookArtUrl from "./assets/book-art.svg";
import { FOLLOW_BOOK_FONT } from "./types";
import type { ShelfBook, ShelfCategory, ShelfData, ShelfSortMode, SystemFont } from "./types";
import {
  bookFontMenuLabel,
  confirmModal,
  displayTitle,
  FORMAT_LABEL,
  isStatsExcluded,
  promptModal,
  starRow,
  toast,
} from "./shelf-ui";

// 基础件已提取到 shelf-ui.ts；此处再导出以保持既有外部入口（main.ts 等）
export { displayTitle, toast } from "./shelf-ui";

/** 「未分类」虚拟分类的约定 id（与 model.rs::UNCATEGORIZED_ID 一致）：仅用于拖拽排序占序 */
const UNCATEGORIZED_ID = "__uncategorized__";

/** 书架 → 阅读器的回调契约（main.ts 注入，避免循环依赖） */
export interface ShelfCallbacks {
  /** 点击卡片打开书籍 */
  openBook(filePath: string): void;
  /** 从书架回到阅读视图（若有正在读的书） */
  backToReader(): void;
  /** 数据变化后通知 main 刷新统计等 */
  onShelfChanged(): void;
  /** 单本书字体变化后通知 main：若该书正在阅读则即时生效 */
  onBookFontChanged(bookId: string, fontFamily: string | null): void;
  /** 书架排序下拉改动：main 持久化到设置并广播给设置窗口同步 */
  onSortModeChanged(mode: ShelfSortMode): void;
}

/** 右键菜单项：heading = 分组标题（不可点）；onClick 缺省时为不可点项 */
interface MenuItem {
  label: string;
  danger?: boolean;
  heading?: boolean;
  active?: boolean;
  onClick?: () => void;
}

export class ShelfView {
  private root: HTMLElement;
  private grid: HTMLElement;
  private sideCategories: HTMLElement;
  private data: ShelfData = { categories: [], books: [], uncategorizedOrder: 0 };
  /** 当前选中分类：null = 全部；"" = 未分类 */
  private currentCategory: string | null | "" = null;
  /** 书架排序方式（ReaderSettings.shelfSortMode，main.ts 注入） */
  private sortMode: ShelfSortMode = "recent";
  /** 搜索关键字（书名/作者包含匹配，大小写不敏感；空串 = 不过滤） */
  private searchQuery = "";
  private searchTimer: number | undefined;
  private cb: ShelfCallbacks;
  /** 系统字体列表缓存（首次打开字体子菜单时加载；空数组 = 加载失败/无字体） */
  private fontList: SystemFont[] | null = null;

  constructor(root: HTMLElement, cb: ShelfCallbacks) {
    this.root = root;
    this.cb = cb;
    this.grid = root.querySelector("#shelf-grid") as HTMLElement;
    this.sideCategories = root.querySelector("#shelf-cats") as HTMLElement;

    // 导入按钮
    const btnImport = root.querySelector("#btn-shelf-import") as HTMLButtonElement;
    btnImport.addEventListener("click", () => void this.importBooks());
    // 统计入口
    const btnStats = root.querySelector("#btn-shelf-stats") as HTMLButtonElement;
    btnStats.addEventListener("click", () => this.cb.backToReader());
    // 设置：打开独立设置窗口（与阅读页顶栏入口同源，单例）
    const btnSettings = root.querySelector("#btn-shelf-settings") as HTMLButtonElement;
    btnSettings.addEventListener("click", () => void api.openSettings());

    // 快速排序按钮：只显示「排序图标+排序」，点击弹出规则菜单（当前项高亮）
    const sortBtn = root.querySelector("#shelf-sort") as HTMLButtonElement;
    sortBtn.addEventListener("click", (e) => {
      // 必须阻止冒泡：document 的全局 click 监听会立即关闭刚打开的菜单
      e.stopPropagation();
      const rect = sortBtn.getBoundingClientRect();
      this.openMenu(rect.right, rect.bottom + 6, this.sortMenuItems());
      // openMenu 内部会先 closeMenu（清掉 .open），故在其之后挂高亮态
      sortBtn.classList.add("open");
    });

    // 搜索框：150ms 防抖后按关键字重绘网格（仅过滤卡片，不动分类栏与总数）
    const search = root.querySelector("#shelf-search") as HTMLInputElement;
    search.addEventListener("input", () => {
      window.clearTimeout(this.searchTimer);
      this.searchTimer = window.setTimeout(() => {
        const q = search.value.trim().toLowerCase();
        if (q === this.searchQuery) return;
        this.searchQuery = q;
        this.renderGrid();
      }, 150);
    });
    // Escape 一键清空并恢复全部
    search.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && search.value) {
        search.value = "";
        this.searchQuery = "";
        this.renderGrid();
      }
      // 阻止冒泡：书架页的全局 Escape 关菜单逻辑不受影响，但避免触发其他快捷键
      e.stopPropagation();
    });

    // 全局点击关闭右键菜单
    document.addEventListener("click", () => this.closeMenu());
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape") this.closeMenu();
    });
  }

  /**
   * 刷新书架数据并重绘。
   * 显式传入 defaultCategory（进入书架视图）时按「默认显示分类」重置选中项；
   * 不传（书架内部数据变化重绘）时保持当前选中分类。
   */
  async refresh(defaultCategory?: string | null): Promise<void> {
    try {
      this.data = await api.getShelf();
    } catch (err) {
      console.error(err);
      this.data = { categories: [], books: [], uncategorizedOrder: 0 };
    }
    if (defaultCategory !== undefined) {
      // null = 全部；"" = 未分类；分类 id 已删（失效）时回退「全部」
      this.currentCategory =
        defaultCategory && !this.data.categories.some((c) => c.id === defaultCategory) ? null : defaultCategory;
    }
    this.render();
  }

  /** 更新排序方式并重绘网格（设置窗口改动实时生效；书架隐藏态重绘开销可忽略） */
  setSortMode(mode: ShelfSortMode): void {
    if (this.sortMode === mode) return;
    this.sortMode = mode;
    this.renderGrid();
  }

  /** 快速排序菜单项：当前规则高亮；选择后重排并持久化 */
  private sortMenuItems(): MenuItem[] {
    const item = (value: ShelfSortMode, label: string): MenuItem => ({
      label,
      active: this.sortMode === value,
      onClick: () => {
        if (value === this.sortMode) return;
        this.sortMode = value;
        this.renderGrid();
        this.cb.onSortModeChanged(value);
      },
    });
    return [item("recent", "最近阅读"), item("added", "添加时间"), item("title", "书名"), item("progress", "阅读进度")];
  }

  // ────────────────────────── 渲染 ──────────────────────────

  private render(): void {
    this.renderSidebar();
    this.renderGrid();
    this.renderHeader();
  }

  /** 主区头部（图2）：标题 = 当前分类名，计数 = 该分类书目数（不含搜索过滤） */
  private renderHeader(): void {
    const title = this.root.querySelector("#shelf-title") as HTMLElement | null;
    const count = this.root.querySelector("#shelf-count") as HTMLElement | null;
    if (!title || !count) return;
    let label = "全部图书";
    let n = this.data.books.length;
    if (this.currentCategory === "") {
      label = "未分类";
      n = this.data.books.filter((b) => !b.categoryId || !this.data.categories.some((c) => c.id === b.categoryId)).length;
    } else if (this.currentCategory !== null) {
      label = this.data.categories.find((c) => c.id === this.currentCategory)?.name ?? "未分类";
      n = this.data.books.filter((b) => b.categoryId === this.currentCategory).length;
    }
    // #shelf-title 的首个文本节点是标题文字（后随 #shelf-count 计数 span，不能整块覆盖）
    const textNode = title.firstChild;
    if (textNode && textNode.nodeType === Node.TEXT_NODE) textNode.textContent = label;
    count.textContent = `(${n})`;
  }

  private renderSidebar(): void {
    const cats = this.sideCategories;
    cats.innerHTML = "";
    cats.appendChild(this.catItem("全部", this.currentCategory === null, () => {
      this.currentCategory = null;
      this.render();
    }));
    // 「未分类」与自定义分类按 order 同一数轴合并排序（未分类可参与拖拽排序）
    const merged = [
      ...this.data.categories.map((c) => ({ id: c.id, name: c.name, order: c.order, virtual: false })),
      { id: UNCATEGORIZED_ID, name: "未分类", order: this.data.uncategorizedOrder, virtual: true },
    ].sort((a, b) => a.order - b.order);
    for (const c of merged) {
      // 长按拖拽会吞掉随后的合成 click：dragMoved 标记当次点击已被拖拽消费
      let dragMoved = false;
      const catValue = c.virtual ? "" : c.id; // 未分类的视图哨兵值 = ""
      const item = this.catItem(c.name, this.currentCategory === catValue, () => {
        if (dragMoved) {
          dragMoved = false;
          return;
        }
        this.currentCategory = catValue;
        this.render();
      });
      item.dataset.catId = c.id;
      this.wireCategoryDrag(item, () => {
        dragMoved = true;
      });
      // 自定义分类右键：重命名 / 删除（虚拟「未分类」无菜单）
      if (!c.virtual) {
        item.addEventListener("contextmenu", (e) => {
          e.preventDefault();
          this.showCategoryMenu(e, c.id, c.name);
        });
      }
      cats.appendChild(item);
    }
    // 新建分类入口：位于最后一个分类的下方（仅一枚 + 图标，hover 淡入）
    const addCat = document.createElement("div");
    addCat.className = "shelf-cat shelf-cat-add";
    addCat.title = "新建分类";
    addCat.innerHTML =
      '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"><path d="M12 5v14M5 12h14"/></svg>';
    addCat.addEventListener("click", () => void this.addCategory());
    cats.appendChild(addCat);
  }

  private catItem(label: string, active: boolean, onClick: () => void): HTMLElement {
    const div = document.createElement("div");
    div.className = "shelf-cat" + (active ? " active" : "");
    div.textContent = label;
    div.addEventListener("click", onClick);
    return div;
  }

  /**
   * 分类长按拖拽排序：按住 320ms 进入拖拽（防误触），拖动时实时重排 DOM 预览，
   * 松手后把新顺序写回 data.categories 并落盘。
   * 「未分类」与自定义分类可拖；「全部 / 新建分类」无 data-cat-id，固定不可拖。
   *
   * 实现要点：
   * - 不用 setPointerCapture——insertBefore 移动节点会触发「从文档移除」从而释放 capture，
   *   后续 pointermove/pointerup 再也收不到；改为挂 document 监听。
   * - 拖拽中给 el 加 .dragging（CSS pointer-events:none），elementFromPoint 才能命中落点
   *   而不是被拖拽项自己挡住。
   */
  private wireCategoryDrag(el: HTMLElement, onDragged: () => void): void {
    const LONG_PRESS_MS = 320;
    const MOVE_CANCEL_PX = 6; // 长按生效前移动超过该距离视为误触，取消长按
    el.addEventListener("pointerdown", (down) => {
      if (down.button !== 0) return; // 右键留给上下文菜单
      const startX = down.clientX;
      const startY = down.clientY;
      const pointerId = down.pointerId;
      let dragging = false;

      const cleanup = () => {
        window.clearTimeout(timer);
        document.removeEventListener("pointermove", onMove);
        document.removeEventListener("pointerup", onUp);
        document.removeEventListener("pointercancel", onUp);
        el.classList.remove("dragging");
      };

      const onMove = (ev: PointerEvent) => {
        if (ev.pointerId !== pointerId) return;
        if (!dragging) {
          if (Math.hypot(ev.clientX - startX, ev.clientY - startY) > MOVE_CANCEL_PX) {
            cleanup();
          }
          return;
        }
        // 指针下的分类项作为落点：按指针在目标上/下半部决定插到其前/后
        const over = document.elementFromPoint(ev.clientX, ev.clientY)?.closest?.(".shelf-cat[data-cat-id]");
        if (over && over !== el && this.sideCategories.contains(over)) {
          const rect = (over as HTMLElement).getBoundingClientRect();
          const before = ev.clientY < rect.top + rect.height / 2;
          this.sideCategories.insertBefore(el, before ? over : over.nextSibling);
        }
      };

      const onUp = (ev: PointerEvent) => {
        if (ev.pointerId !== pointerId) return;
        const wasDragging = dragging;
        cleanup();
        if (!wasDragging) return;
        onDragged(); // 标记拖拽已消费随后的合成 click
        // DOM 顺序即新顺序：虚拟「未分类」占序写 uncategorizedOrder，真实分类写 order 并落盘
        const ids = [...this.sideCategories.querySelectorAll<HTMLElement>(".shelf-cat[data-cat-id]")].map(
          (n) => n.dataset.catId!,
        );
        const byId = new Map(this.data.categories.map((c) => [c.id, c] as const));
        const reordered: ShelfCategory[] = [];
        ids.forEach((id, i) => {
          if (id === UNCATEGORIZED_ID) {
            this.data.uncategorizedOrder = i + 1;
            return;
          }
          const c = byId.get(id);
          if (c) {
            c.order = i + 1;
            reordered.push(c);
          }
        });
        this.data.categories = reordered;
        this.render();
        void api.reorderCategoryOrder(ids).catch((err) => toast(String(err)));
      };

      const timer = window.setTimeout(() => {
        dragging = true;
        el.classList.add("dragging");
      }, LONG_PRESS_MS);

      document.addEventListener("pointermove", onMove);
      document.addEventListener("pointerup", onUp);
      document.addEventListener("pointercancel", onUp);
    });
  }

  private renderGrid(): void {
    const grid = this.grid;
    grid.innerHTML = "";
    let books = this.sortedBooks();
    if (this.currentCategory === "") {
      books = books.filter((b) => !b.categoryId || !this.data.categories.some((c) => c.id === b.categoryId));
    } else if (this.currentCategory !== null) {
      books = books.filter((b) => b.categoryId === this.currentCategory);
    }
    // 搜索过滤：书名/作者包含匹配（大小写不敏感）
    if (this.searchQuery) {
      books = books.filter(
        (b) => b.title.toLowerCase().includes(this.searchQuery) || (b.author ?? "").toLowerCase().includes(this.searchQuery),
      );
    }

    if (books.length === 0) {
      const empty = document.createElement("div");
      empty.className = "shelf-empty";
      empty.textContent = this.searchQuery ? `没有找到与「${this.searchQuery}」匹配的书籍` : "书架空空如也，点击左侧「添加图书」导入";
      grid.appendChild(empty);
      return;
    }

    // 「续读」只标注最近阅读的一本：全库范围内未读完且打开过的书中 lastReadAt 最大者
    // （从全量书计算而非当前过滤视图，避免被过滤掉时角标错误地落到别的书上）
    const continueId = this.data.books
      .filter((b) => !b.finished && b.lastReadAt > 0)
      .reduce<ShelfBook | null>((best, b) => (!best || b.lastReadAt > best.lastReadAt ? b : best), null)?.id ?? null;
    // 逐卡 stagger 入场（前 12 张生效，长列表尾部不拖泥带水；搜索过滤时不重放）
    const stagger = this.searchQuery ? false : true;
    books.forEach((b, i) => {
      const card = this.card(b, continueId);
      if (stagger && i < 12) {
        card.classList.add("card-enter");
        card.style.animationDelay = `${i * 30}ms`;
        card.addEventListener("animationend", () => card.classList.remove("card-enter"), { once: true });
      }
      grid.appendChild(card);
    });
  }

  /** 全部书籍按当前排序方式排序（固定方向，各档末位同以添加时间新→旧兜底） */
  private sortedBooks(): ShelfBook[] {
    const books = [...this.data.books];
    switch (this.sortMode) {
      case "added":
        books.sort((a, b) => b.addedAt - a.addedAt);
        break;
      case "title":
        // 中文按拼音序（locale "zh"），同名回落添加时间
        books.sort((a, b) => a.title.localeCompare(b.title, "zh") || b.addedAt - a.addedAt);
        break;
      case "progress":
        books.sort((a, b) => b.percent - a.percent || b.lastReadAt - a.lastReadAt);
        break;
      case "recent":
      default:
        // 默认：最近阅读优先，未读过的按添加时间排在后面
        books.sort((a, b) => b.lastReadAt - a.lastReadAt || b.addedAt - a.addedAt);
        break;
    }
    return books;
  }

  private card(b: ShelfBook, continueId: string | null): HTMLElement {
    const card = document.createElement("div");
    card.className = "shelf-card";

    // 封面：shelf-cover 协议直出；失败用格式占位
    const cover = document.createElement("div");
    cover.className = "shelf-cover";
    const img = document.createElement("img");
    img.draggable = false;
    img.src = shelfCoverUrl(b.id);
    img.alt = b.title;
    img.addEventListener("error", () => {
      cover.classList.add("placeholder");
      // 只替换 img 自身，不能动 cover.innerHTML：此时「读完/续读/评分」角标
      // 已追加到 cover 上（无封面书必走这里，innerHTML 会把角标一并抹掉）
      // 无封面统一图形：logo/book.svg 描摹稿（纯线稿、全透明底），夜间主题反相
      const art = document.createElement("img");
      art.className = "ph-art";
      art.src = bookArtUrl;
      art.draggable = false;
      art.alt = "";
      const fmt = document.createElement("span");
      fmt.className = "ph-format";
      fmt.textContent = FORMAT_LABEL[b.format];
      img.replaceWith(art, fmt);
    });
    cover.appendChild(img);

    // 角标：「续读」仅标注最近阅读的一本；读完不用角标，改由绿色进度条表达
    if (continueId && b.id === continueId) {
      cover.appendChild(Object.assign(document.createElement("span"), { className: "badge badge-continue", textContent: "续读" }));
    }
    // 评分角标（半星步进，左下角不与右上角「续读」重叠）
    if (b.rating !== null) {
      cover.appendChild(
        Object.assign(document.createElement("span"), {
          className: "badge badge-rating",
          textContent: `★ ${(b.rating / 10).toFixed(1).replace(/\.0$/, "")}`,
        }),
      );
    }

    // 进度条：读完的书填充为绿色（替代原「读完」角标的表达）
    const bar = document.createElement("div");
    bar.className = "shelf-progress";
    const fill = document.createElement("div");
    fill.className = "shelf-progress-fill" + (b.finished ? " done" : "");
    fill.style.width = `${Math.min(100, b.percent).toFixed(1)}%`;
    bar.appendChild(fill);

    const title = document.createElement("div");
    title.className = "shelf-title";
    title.textContent = displayTitle(b.title);
    title.title = b.title + (b.author ? ` · ${b.author}` : "");

    const meta = document.createElement("div");
    meta.className = "shelf-meta";
    meta.textContent = b.finished ? "已读完" : `${b.percent.toFixed(0)}%`;

    card.append(cover, title, meta, bar);
    card.addEventListener("click", () => this.cb.openBook(b.filePath));
    card.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      this.showBookMenu(e, b);
    });
    return card;
  }

  // ────────────────────────── 上下文菜单 ──────────────────────────

  private closeMenu(): void {
    document.getElementById("ctx-menu")?.remove();
    // 排序按钮的打开态高亮随菜单一起复位（覆盖 点外/Esc/选中 三条关闭路径）
    this.root.querySelector(".shelf-sort-btn.open")?.classList.remove("open");
  }

  private openMenu(x: number, y: number, items: MenuItem[], scrollable = false): void {
    this.closeMenu();
    const menu = document.createElement("div");
    menu.id = "ctx-menu";
    if (scrollable) menu.classList.add("ctx-scroll");
    for (const it of items) {
      if (it.heading) {
        const head = document.createElement("div");
        head.className = "ctx-heading";
        head.textContent = it.label;
        // 分组标题不可点：吞掉 click，避免冒泡到 document 触发"点外关闭"
        head.addEventListener("click", (e) => e.stopPropagation());
        menu.appendChild(head);
        continue;
      }
      const item = document.createElement("div");
      item.className = "ctx-item" + (it.danger ? " danger" : "") + (it.active ? " active" : "");
      item.textContent = it.label;
      const onClick = it.onClick;
      if (onClick) {
        item.addEventListener("click", (e) => {
          // 必须阻止冒泡：document 上的全局 click 监听会关菜单，若不同步阻断，
          // 在本 handler 里同步打开的二级菜单（设置分类/设置本书字体）会被同一次
          // click 冒泡立即移除，表现为"点击没反应"
          e.stopPropagation();
          this.closeMenu();
          onClick();
        });
      }
      menu.appendChild(item);
    }
    document.body.appendChild(menu);
    // 防止溢出屏幕
    const rect = menu.getBoundingClientRect();
    menu.style.left = `${Math.min(x, window.innerWidth - rect.width - 8)}px`;
    menu.style.top = `${Math.min(y, window.innerHeight - rect.height - 8)}px`;
  }

  private showBookMenu(e: MouseEvent, b: ShelfBook): void {
    // 正文字体仅对有文本层的格式有意义（PDF/CBZ 无文本，不提供设置入口）
    const canSetFont = b.format !== "pdf" && b.format !== "cbz";
    // 统计收录开关（隐私）：当前不计入则显示「计入」，反之亦然
    const statsExcluded = isStatsExcluded(b);
    const statsItem: MenuItem = {
      label: statsExcluded ? "计入读书统计" : "不计入读书统计",
      onClick: () => void this.setStatsExcluded(b, !statsExcluded),
    };
    // 评分：读完（进度≥99.5）才可评；未读完且未评过直接不显示入口
    //（已评过的书保留修改/清除入口，进度回退不锁死）
    const canRate = b.finished || b.rating !== null;
    const items: MenuItem[] = [
      { label: "打开", onClick: () => this.cb.openBook(b.filePath) },
      { label: "重命名", onClick: () => void this.renameBook(b) },
      { label: "更换封面", onClick: () => void this.openCoverWindow(b) },
      { label: "打开文件所在目录", onClick: () => void this.revealBookInFolder(b.id) },
    ];
    // 无文本层格式（PDF/CBZ）不支持字体：不显示该功能而非置灰占位
    if (canSetFont) {
      const fontLabel = bookFontMenuLabel(b.fontFamily);
      items.push({
        label: fontLabel ? `设置本书字体（${fontLabel}）` : "设置本书字体",
        onClick: () => void this.openFontMenu(b, e.clientX, e.clientY),
      });
    }
    if (canRate) {
      items.push({ label: "设置评分", onClick: () => this.openRatingPanel(b, e.clientX, e.clientY) });
    }
    items.push(
      { label: "设置题材", onClick: () => void this.openGenreWindow(b) },
      statsItem,
      { label: "设置分类", onClick: () => this.openCategoryMenu(b, e.clientX, e.clientY) },
      { label: "移除出书架", danger: true, onClick: () => void this.removeBook(b.id) },
    );
    this.openMenu(e.clientX, e.clientY, items);
  }

  /** 「设置评分」豆瓣式五星弹层：悬停预览（半星步进），点击落星，可清除 */
  private openRatingPanel(b: ShelfBook, x: number, y: number): void {
    this.closeRatingPanel();
    this.closeMenu();
    const panel = document.createElement("div");
    panel.id = "rating-panel";

    const label = document.createElement("div");
    label.className = "rp-label";

    const stars = document.createElement("div");
    stars.className = "rp-stars";
    const fill = starRow("rp-layer rp-fill");
    stars.append(starRow("rp-layer rp-bg"), fill);

    const word = (v: number): string =>
      v <= 2 ? "很差" : v <= 4 ? "较差" : v <= 6 ? "还行" : v <= 8 ? "推荐" : "力荐";
    const show = (v: number): void => {
      fill.style.width = `${v * 10}%`;
      label.textContent = v > 0 ? `${word(v)} ${v.toFixed(1).replace(/\.0$/, "")}` : "未评分";
    };
    show(b.rating ?? 0);

    // 悬停/点击共用：十星制 0.1 星步进（0~100，与后端取值一致）
    const valueAt = (e: PointerEvent | MouseEvent): number => {
      const rect = stars.getBoundingClientRect();
      return Math.min(10, Math.max(0.1, Math.round(((e.clientX - rect.left) / rect.width) * 100) / 10));
    };
    stars.addEventListener("pointermove", (e) => show(valueAt(e)));
    stars.addEventListener("pointerleave", () => show(b.rating ?? 0));
    stars.addEventListener("click", (e) => {
      e.stopPropagation();
      const v = valueAt(e);
      this.closeRatingPanel();
      // 星数 → 模型值（值 = 星数×10，如 9.3 星 → 93），后端 u8 取值 0~100
      void this.setBookRating(b, Math.round(v * 10));
    });

    panel.append(label, stars);
    if (b.rating !== null) {
      const clear = document.createElement("button");
      clear.className = "rp-clear";
      clear.textContent = "清除评分";
      clear.addEventListener("click", (e) => {
        e.stopPropagation();
        this.closeRatingPanel();
        void this.setBookRating(b, null);
      });
      panel.appendChild(clear);
    }
    // 点面板内部不关闭（面板自身的点外/Esc 关闭在 closeRatingPanel 解绑）
    panel.addEventListener("click", (e) => e.stopPropagation());

    document.body.appendChild(panel);
    this.ratingPanel = panel;
    document.addEventListener("click", this.onPanelOutside);
    window.addEventListener("keydown", this.onPanelEsc);
    // 防止溢出屏幕
    const rect = panel.getBoundingClientRect();
    panel.style.left = `${Math.min(x, window.innerWidth - rect.width - 8)}px`;
    panel.style.top = `${Math.min(y, window.innerHeight - rect.height - 8)}px`;
  }

  private ratingPanel: HTMLElement | null = null;
  private onPanelOutside = (): void => this.closeRatingPanel();
  private onPanelEsc = (e: KeyboardEvent): void => {
    if (e.key === "Escape") this.closeRatingPanel();
  };

  private closeRatingPanel(): void {
    if (!this.ratingPanel) return;
    this.ratingPanel.remove();
    this.ratingPanel = null;
    document.removeEventListener("click", this.onPanelOutside);
    window.removeEventListener("keydown", this.onPanelEsc);
  }

  /** 打开题材设置窗口（label=genre 单例；已开则切换到该书） */
  private openGenreWindow(b: ShelfBook): void {
    api.openGenreWindow(b.id).catch((err) => toast(String(err)));
  }

  /**
   * 阅读页右键菜单：书签（main 注入首项）+ 设置本书字体 + 统计收录 + 设置分类。
   * 书架数据缺该书时先刷新一次（打开即自动入架，理论上必有；仍缺则只显示书签项）。
   */
  async openReaderMenu(x: number, y: number, bookId: string, bookmarkItem: MenuItem): Promise<void> {
    let target = this.data.books.find((bk) => bk.id === bookId);
    if (!target) {
      await this.refresh();
      target = this.data.books.find((bk) => bk.id === bookId);
    }
    const items: MenuItem[] = [bookmarkItem];
    if (target) {
      const book = target;
      // 无文本层格式（PDF/CBZ）不支持字体：不显示该功能而非置灰占位
      const canSetFont = book.format !== "pdf" && book.format !== "cbz";
      if (canSetFont) {
        const fontLabel = bookFontMenuLabel(book.fontFamily);
        items.push({
          label: fontLabel ? `设置本书字体（${fontLabel}）` : "设置本书字体",
          onClick: () => void this.openFontMenu(book, x, y),
        });
      }
      const statsExcluded = isStatsExcluded(book);
      items.push({
        label: statsExcluded ? "计入读书统计" : "不计入读书统计",
        onClick: () => void this.setStatsExcluded(book, !statsExcluded),
      });
      items.push({ label: "设置分类", onClick: () => this.openCategoryMenu(book, x, y) });
    }
    this.openMenu(x, y, items);
  }

  /**
   * 单本书字体选择子菜单：「跟随全局」/「跟随图书设定」+ 系统字体列表（当前项高亮）。
   * 字体列表懒加载并缓存；当前字体已卸载时追加原样选项保证可重选。
   */
  private async openFontMenu(b: ShelfBook, x: number, y: number): Promise<void> {
    if (this.fontList === null) {
      try {
        this.fontList = await api.listSystemFonts();
      } catch (err) {
        console.error(err);
        this.fontList = [];
      }
    }
    const isFollowBook = b.fontFamily === FOLLOW_BOOK_FONT;
    const items: MenuItem[] = [
      { label: "跟随全局", active: !b.fontFamily, onClick: () => void this.setBookFont(b, null) },
      {
        label: "跟随图书设定",
        active: isFollowBook,
        onClick: () => void this.setBookFont(b, FOLLOW_BOOK_FONT),
      },
      { label: "── 系统字体 ──", heading: true },
    ];
    for (const f of this.fontList) {
      const active = f.family === b.fontFamily;
      items.push({
        label: active ? `✓ ${f.display}` : f.display,
        active,
        onClick: () => void this.setBookFont(b, f.family),
      });
    }
    if (b.fontFamily && !isFollowBook && !this.fontList.some((f) => f.family === b.fontFamily)) {
      items.push({ label: `✓ ${b.fontFamily}`, active: true, onClick: () => void this.setBookFont(b, b.fontFamily) });
    }
    this.openMenu(x, y, items, true);
  }

  /**
   * 单本书「设置分类」子菜单：未分类 + 全部分类（当前项打勾高亮），
   * 底部提供「新建分类并移入…」走自定义模态输入框。
   */
  private openCategoryMenu(b: ShelfBook, x: number, y: number): void {
    const items: MenuItem[] = [
      {
        label: "未分类",
        active: !b.categoryId || !this.data.categories.some((c) => c.id === b.categoryId),
        onClick: () => void this.moveBook(b.id, null),
      },
    ];
    for (const c of [...this.data.categories].sort((a, b2) => a.order - b2.order)) {
      const active = b.categoryId === c.id;
      items.push({
        label: active ? `✓ ${c.name}` : c.name,
        active,
        onClick: () => void this.moveBook(b.id, c.id),
      });
    }
    items.push({ label: "── 更多 ──", heading: true });
    items.push({ label: "+ 新建分类并移入…", onClick: () => void this.newCategoryAndMove(b.id) });
    this.openMenu(x, y, items, true);
  }

  /** 保存单本书字体：落盘 shelf.json + 本地同步 + 通知 main（正在阅读时即时生效） */
  private async setBookFont(b: ShelfBook, fontFamily: string | null): Promise<void> {
    try {
      const ok = await api.setBookFont(b.id, fontFamily);
      if (!ok) throw new Error("书籍不在书架中");
      b.fontFamily = fontFamily;
      this.cb.onBookFontChanged(b.id, fontFamily);
    } catch (err) {
      toast(String(err));
    }
  }

  /**
   * 切换统计收录（隐私开关）：落盘 shelf.json + 本地同步。
   * 设为不计入时后端会清除该书已落账的历史时长与标题，属破坏性操作，先确认。
   */
  private async setStatsExcluded(b: ShelfBook, excluded: boolean): Promise<void> {
    if (
      excluded &&
      !(await confirmModal({
        title: "不计入读书统计",
        message: `「${b.title}」将不计入阅读统计，并清除其历史统计时长。`,
      }))
    ) {
      return;
    }
    try {
      const ok = await api.setBookStatsExcluded(b.id, excluded);
      if (!ok) throw new Error("书籍不在书架中");
      b.statsExcluded = excluded;
    } catch (err) {
      toast(String(err));
    }
  }

  /** 保存评分：落盘 shelf.json + 本地同步（rated_at 由后端记录）+ 重绘角标 */
  private async setBookRating(b: ShelfBook, rating: number | null): Promise<void> {
    try {
      const ok = await api.setBookRating(b.id, rating);
      if (!ok) throw new Error("书籍不在书架中");
      b.rating = rating;
      this.render();
    } catch (err) {
      toast(String(err));
    }
  }

  /** 题材窗口保存后同步内存书架并重绘（main.ts 的 genres-changed 监听转发到这里） */
  updateBookGenres(bookId: string, genres: string[]): void {
    const book = this.data.books.find((b) => b.id === bookId);
    if (!book) return;
    book.genres = genres;
    this.render();
  }

  private showCategoryMenu(e: MouseEvent, id: string, name: string): void {
    this.openMenu(e.clientX, e.clientY, [
      {
        label: "重命名",
        onClick: () => void this.renameCategory(id, name),
      },
      {
        label: "删除分类（书籍移到未分类）",
        danger: true,
        onClick: () => void this.removeCategory(id, name),
      },
    ]);
  }

  // ────────────────────────── 动作 ──────────────────────────

  private async importBooks(): Promise<void> {
    const paths = await openFileDialog({
      multiple: true,
      filters: [
        {
          name: "电子书",
          extensions: ["epub", "mobi", "prc", "azw", "azw3", "kf8", "pdf", "fb2", "fbz", "cbz", "txt", "log", "md", "markdown"],
        },
      ],
    });
    if (!paths) return;
    const list = typeof paths === "string" ? [paths] : paths;
    let imported = 0;
    for (const p of list) {
      try {
        await api.addBookToShelf(p);
        imported += 1;
      } catch (err) {
        console.error(err);
        toast(`导入失败：${p}\n${err}`);
      }
    }
    if (imported > 0) {
      await this.refresh();
      this.cb.onShelfChanged();
    }
  }

  /** 重命名分类：自定义模态输入框（预填原名） */
  private async renameCategory(id: string, oldName: string): Promise<void> {
    const newName = await promptModal({ title: "重命名分类", value: oldName, okText: "重命名" });
    if (!newName || newName === oldName) return;
    try {
      await api.renameCategory(id, newName);
      await this.refresh();
    } catch (err) {
      toast(String(err));
    }
  }

  /** 重命名书架显示书名：模态输入框预填当前书名，编辑后确认落盘（只改书架条目） */
  private async renameBook(b: ShelfBook): Promise<void> {
    const newTitle = await promptModal({ title: "重命名", value: b.title, okText: "重命名" });
    if (!newTitle || newTitle === b.title) return;
    try {
      const ok = await api.renameBook(b.id, newTitle);
      if (!ok) throw new Error("书籍不在书架中");
      b.title = newTitle;
      this.render();
    } catch (err) {
      toast(String(err));
    }
  }

  /** 打开（或聚焦）更换封面独立窗口 */
  private openCoverWindow(b: ShelfBook): void {
    api.openCoverWindow(b.id).catch((err) => toast(String(err)));
  }

  /** 封面窗口/后端变更封面后刷新缩略图（main.ts 的 cover-changed 监听转发到这里） */
  refreshBookCover(bookId: string): void {
    bumpCoverVersion(bookId);
    this.render();
  }

  /** 在系统文件管理器中打开书籍文件所在目录（路径由后端按书架登记查询并校验存在） */
  private async revealBookInFolder(bookId: string): Promise<void> {
    try {
      await api.revealBookInFolder(bookId);
    } catch (err) {
      toast(String(err));
    }
  }

  private async addCategory(): Promise<void> {
    const name = await promptModal({ title: "新建分类", placeholder: "分类名称" });
    if (!name) return;
    try {
      await api.addCategory(name);
      await this.refresh();
    } catch (err) {
      toast(String(err));
    }
  }

  private async newCategoryAndMove(bookId: string): Promise<void> {
    const name = await promptModal({ title: "新建分类并移入", placeholder: "分类名称" });
    if (!name) return;
    try {
      const cat = await api.addCategory(name);
      await api.setBookCategory(bookId, cat.id);
      await this.refresh();
    } catch (err) {
      toast(String(err));
    }
  }

  /** 删除分类（危险操作先确认）：分类下书籍自动移到未分类 */
  private async removeCategory(id: string, name: string): Promise<void> {
    const ok = await confirmModal({
      title: "删除分类",
      message: `删除分类「${name}」？其下书籍将移到未分类。`,
      okText: "删除",
      danger: true,
    });
    if (!ok) return;
    try {
      await api.removeCategory(id);
      await this.refresh();
    } catch (err) {
      toast(String(err));
    }
  }

  private async moveBook(bookId: string, categoryId: string | null): Promise<void> {
    try {
      await api.setBookCategory(bookId, categoryId);
      await this.refresh();
    } catch (err) {
      toast(String(err));
    }
  }

  private async removeBook(bookId: string): Promise<void> {
    const ok = await confirmModal({
      title: "移除出书架",
      message: "从书架移除这本书？（阅读进度保留）",
      okText: "移除",
      danger: true,
    });
    if (!ok) return;
    try {
      await api.removeBookFromShelf(bookId);
      await this.refresh();
      this.cb.onShelfChanged();
    } catch (err) {
      toast(String(err));
    }
  }
}
