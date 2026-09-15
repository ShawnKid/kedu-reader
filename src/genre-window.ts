/**
 * 独立题材设置窗口（Tauri 窗口 label = "genre"）。
 *
 * 与主窗口共用同一前端产物（index.html）：bootstrap.ts 按窗口 label 分流到这里。
 * 单例窗口：书目 id 由后端经 initialization_script 注入 window.__GENRE_BOOK_ID__；
 * 窗口已开时后端改发 "genre-book-changed" 事件切换书目。
 *
 * 数据流：勾选只在本地内存 → 点「保存」调 set_book_genres 落盘 shelf.json，
 * 后端广播 "genres-changed" 供主窗口刷新卡片（本窗口是唯一写入方，忽略该回声）。
 */
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { api } from "./ipc";
import { GENRE_TREE } from "./genres";
import type { ReaderSettings } from "./types";

declare global {
  interface Window {
    __GENRE_BOOK_ID__?: string;
  }
}

let bookId: string | null = null;
let bookTitle = "";
/** 当前勾选的题材（跨重渲染保持） */
const selected = new Set<string>();
/** 搜索防抖定时器 */
let searchTimer: number | undefined;

const byId = <T extends HTMLElement>(id: string): T => document.getElementById(id) as T;

/** 构建窗口 DOM（整体替换 index.html 的阅读器结构） */
function buildDom(): void {
  document.title = "设置题材";
  document.body.classList.add("settings-page", "genre-page");
  document.body.innerHTML = `
    <header id="sw-titlebar" data-tauri-drag-region>
      <span class="sw-title" data-tauri-drag-region>设置题材</span>
      <button id="sw-close" class="win-btn close" title="关闭">
        <svg width="10" height="10" viewBox="0 0 10 10"><path d="M0 0l10 10M10 0L0 10" stroke="currentColor" stroke-width="1"/></svg>
      </button>
    </header>
    <main id="sw-body">
      <div id="gw-head">
        <span id="gw-book" title=""></span>
        <span id="gw-hint">可多选，勾选后点「保存」生效</span>
      </div>
      <div id="gw-search-row">
        <label class="gw-search-box">
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true">
            <circle cx="11" cy="11" r="7"/><path d="M20 20l-3.5-3.5"/>
          </svg>
          <input id="gw-search" type="search" placeholder="搜索题材名称或分组…" autocomplete="off" spellcheck="false" />
        </label>
        <span id="gw-search-meta"></span>
      </div>
      <div id="gw-chips"></div>
      <div id="gw-tree"></div>
      <div id="gw-empty" hidden>没有匹配的题材</div>
      <footer id="gw-foot">
        <span id="gw-count"></span>
        <button id="gw-clear" class="sw-btn">清空</button>
        <button id="gw-save" class="sw-btn accent">保存</button>
      </footer>
    </main>`;
}

/** 渲染已选 chips（标签名 + × 取消勾选） */
function renderChips(): void {
  const box = byId<HTMLDivElement>("gw-chips");
  box.innerHTML = "";
  for (const g of selected) {
    const chip = document.createElement("span");
    chip.className = "gw-chip";
    const text = document.createElement("span");
    text.textContent = g; // textContent 防标签注入
    const x = document.createElement("button");
    x.className = "gw-chip-x";
    x.title = "移除";
    x.textContent = "×";
    x.addEventListener("click", () => {
      selected.delete(g);
      syncTreeChecks();
      renderChips();
    });
    chip.append(text, x);
    box.appendChild(chip);
  }
  box.hidden = selected.size === 0;
  byId<HTMLSpanElement>("gw-count").textContent =
    selected.size > 0 ? `已选 ${selected.size} 个题材` : "未选择题材";
}

/** 把 selected 同步到树中的复选框（chips 取消勾选时用） */
function syncTreeChecks(): void {
  document.querySelectorAll<HTMLInputElement>("#gw-tree input[type='checkbox']").forEach((c) => {
    c.checked = selected.has(c.dataset.genre ?? "");
  });
  refreshGroupCounts();
}

/** 刷新维度/子组头部的「已选 n」计数 */
function refreshGroupCounts(): void {
  document.querySelectorAll<HTMLElement>(".gw-dim, .gw-sub").forEach((group) => {
    const n = group.querySelectorAll<HTMLInputElement>("input[type='checkbox']:checked").length;
    const badge = group.querySelector<HTMLElement>(":scope > .gw-count");
    if (badge) {
      badge.textContent = n > 0 ? `已选 ${n}` : "";
    }
  });
}

/** 构建题材树：维度（可折叠）→ 子组（可折叠）→ 标签复选框 */
function renderTree(): void {
  const tree = byId<HTMLDivElement>("gw-tree");
  tree.innerHTML = "";

  const makeHead = (label: string, cls: string): HTMLButtonElement => {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = cls;
    const arrow = document.createElement("span");
    arrow.className = "gw-arrow";
    arrow.textContent = "▾";
    const name = document.createElement("span");
    name.className = "gw-head-name";
    name.textContent = label; // textContent 防注入
    const count = document.createElement("span");
    count.className = "gw-count";
    btn.append(arrow, name, count);
    return btn;
  };

  for (const dim of GENRE_TREE) {
    const dimEl = document.createElement("section");
    dimEl.className = "gw-dim";
    const dimHead = makeHead(dim.label, "gw-dim-head");
    const dimBody = document.createElement("div");
    dimBody.className = "gw-body";
    dimHead.addEventListener("click", () => {
      dimEl.classList.toggle("collapsed");
      dimBody.classList.toggle("collapsed");
    });

    for (const sub of dim.sub) {
      const subEl = document.createElement("div");
      subEl.className = "gw-sub";
      const subHead = makeHead(sub.label, "gw-sub-head");
      const subBody = document.createElement("div");
      subBody.className = "gw-tag-grid";
      subHead.addEventListener("click", () => {
        subEl.classList.toggle("collapsed");
        subBody.classList.toggle("collapsed");
      });

      for (const tag of sub.tags) {
        const label = document.createElement("label");
        label.className = "gw-tag";
        const input = document.createElement("input");
        input.type = "checkbox";
        input.dataset.genre = tag;
        input.checked = selected.has(tag);
        input.addEventListener("change", () => {
          if (input.checked) selected.add(tag);
          else selected.delete(tag);
          renderChips();
          refreshGroupCounts();
        });
        const text = document.createElement("span");
        text.textContent = tag; // textContent 防注入
        label.append(input, text);
        subBody.appendChild(label);
      }

      subEl.append(subHead, subBody);
      dimBody.appendChild(subEl);
    }

    dimEl.append(dimHead, dimBody);
    tree.appendChild(dimEl);
  }

  // 树重建后若有搜索词则重新过滤（切书时保留搜索）
  const q = byId<HTMLInputElement>("gw-search")?.value ?? "";
  if (q.trim()) applyFilter(q);
}

/**
 * 按关键词过滤题材树：
 * - 标签名 / 维度名 / 子组名任一包含关键词即命中（忽略大小写）
 * - 命中标签只显示该标签；命中分组则展开并显示该分组下全部标签
 * - 空关键词恢复完整树
 */
function applyFilter(raw: string): void {
  const q = raw.trim().toLowerCase();
  const tree = byId<HTMLDivElement>("gw-tree");
  const meta = byId<HTMLSpanElement>("gw-search-meta");
  const empty = byId<HTMLDivElement>("gw-empty");
  if (!q) {
    tree.querySelectorAll<HTMLElement>(".gw-dim, .gw-sub, .gw-tag").forEach((el) => {
      el.hidden = false;
    });
    tree.querySelectorAll<HTMLElement>(".gw-dim, .gw-sub").forEach((el) => {
      el.classList.remove("collapsed");
    });
    tree.querySelectorAll<HTMLElement>(".gw-body, .gw-tag-grid").forEach((el) => {
      el.classList.remove("collapsed");
    });
    meta.textContent = "";
    empty.hidden = true;
    tree.hidden = false;
    return;
  }

  let matchTags = 0;
  for (const dimEl of tree.querySelectorAll<HTMLElement>(".gw-dim")) {
    const dimName = dimEl.querySelector(".gw-head-name")?.textContent?.toLowerCase() ?? "";
    const dimHit = dimName.includes(q);
    let dimAny = dimHit;

    for (const subEl of dimEl.querySelectorAll<HTMLElement>(":scope > .gw-body > .gw-sub")) {
      const subName = subEl.querySelector(".gw-head-name")?.textContent?.toLowerCase() ?? "";
      // 分组名命中 → 该组标签全部展开可见
      const subHit = dimHit || subName.includes(q);
      let subAny = subHit;

      for (const tagEl of subEl.querySelectorAll<HTMLElement>(".gw-tag")) {
        const tagName = tagEl.querySelector("span")?.textContent?.toLowerCase() ?? "";
        const show = subHit || tagName.includes(q);
        tagEl.hidden = !show;
        if (show) {
          subAny = true;
          dimAny = true;
          matchTags += 1;
        }
      }

      subEl.hidden = !subAny;
      // 命中时强制展开，方便立刻点选
      if (subAny) {
        subEl.classList.remove("collapsed");
        subEl.querySelector(".gw-tag-grid")?.classList.remove("collapsed");
      }
    }

    dimEl.hidden = !dimAny;
    if (dimAny) {
      dimEl.classList.remove("collapsed");
      dimEl.querySelector(".gw-body")?.classList.remove("collapsed");
    }
  }

  empty.hidden = matchTags > 0;
  tree.hidden = matchTags === 0;
  meta.textContent = matchTags > 0 ? `匹配 ${matchTags} 个` : "";
}

/** 切换到指定书目：拉取书架取书名与已标题材，重建勾选状态 */
async function loadBook(id: string): Promise<void> {
  const shelf = await api.getShelf();
  const book = shelf.books.find((b) => b.id === id);
  if (!book) throw new Error("书籍不在书架中");
  bookId = book.id;
  bookTitle = book.title;
  selected.clear();
  for (const g of book.genres) selected.add(g);

  const nameEl = byId<HTMLSpanElement>("gw-book");
  nameEl.textContent = bookTitle;
  nameEl.title = bookTitle;
  renderTree();
  renderChips();
}

function bindEvents(): void {
  const close = () => void getCurrentWindow().close();
  byId<HTMLButtonElement>("sw-close").addEventListener("click", close);
  window.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    // 搜索框有内容时优先清空搜索，避免误关窗口
    const search = byId<HTMLInputElement>("gw-search");
    if (search && search.value) {
      search.value = "";
      applyFilter("");
      search.focus();
      e.preventDefault();
      return;
    }
    if (document.activeElement === search) {
      // 焦点在空搜索框上：先失焦，再按 Escape 才关窗
      search.blur();
      e.preventDefault();
      return;
    }
    close();
  });

  const search = byId<HTMLInputElement>("gw-search");
  search.addEventListener("input", () => {
    window.clearTimeout(searchTimer);
    const v = search.value;
    searchTimer = window.setTimeout(() => applyFilter(v), 120);
  });

  byId<HTMLButtonElement>("gw-clear").addEventListener("click", () => {
    selected.clear();
    syncTreeChecks();
    renderChips();
  });

  byId<HTMLButtonElement>("gw-save").addEventListener("click", async () => {
    if (!bookId) return;
    const btn = byId<HTMLButtonElement>("gw-save");
    btn.disabled = true;
    btn.textContent = "保存中…";
    try {
      await api.setBookGenres(bookId, [...selected]);
      await getCurrentWindow().close();
    } catch (err) {
      console.error("保存题材失败:", err);
      btn.disabled = false;
      btn.textContent = "保存";
      byId<HTMLSpanElement>("gw-count").textContent = `保存失败：${String(err)}`;
    }
  });

  // 后端切换单例窗口的书目（窗口已开时再次右键「设置题材」）
  void listen<string>("genre-book-changed", (e) => {
    void loadBook(e.payload).catch(console.error);
  });
  // 本窗口是题材的唯一写入方，忽略自己触发的 genres-changed 回声
}

/** 题材窗口启动入口（main.ts 按窗口 label 调用） */
export async function bootGenreWindow(): Promise<void> {
  buildDom();
  // 圆角窗口：拖拽区双击可最大化/还原，随窗口尺寸同步直角/圆角
  const appWindow = getCurrentWindow();
  const syncMaximized = async () => {
    document.body.classList.toggle("win-maximized", await appWindow.isMaximized());
  };
  void syncMaximized();
  appWindow.onResized(() => void syncMaximized());
  // 主题跟随全局设置（夜间/护眼等皮肤同样作用于本窗口）
  try {
    const settings: ReaderSettings = await api.loadReaderSettings();
    document.body.dataset.theme = settings.theme;
  } catch {
    // 主题读取失败保持默认
  }
  bindEvents();
  const id = window.__GENRE_BOOK_ID__ ?? "";
  if (id) {
    await loadBook(id).catch((err) => {
      console.error("题材窗口加载书目失败:", err);
      byId<HTMLSpanElement>("gw-count").textContent = String(err);
    });
  }
}
