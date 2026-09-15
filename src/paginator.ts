/**
 * 分页算法（虚拟分页）。
 *
 * 原理（CSS 多列虚拟分页）：
 * 内容容器使用 CSS 多列布局，文字横向流入若干列；
 * 每一"页"即一屏（单栏模式下=一列；分栏模式下一屏并列显示 N 列，
 * N 由目标栏宽与可用宽度自适应）。用 transform: translateX(-page * step)
 * 平移整个容器实现翻页，step = 一屏宽 + 列间距。
 * 总页数 = ceil(容器 scrollWidth / step)。
 *
 * 优点：不拆分 DOM，排版完全交给浏览器，图片/表格/标题断页由 CSS columns 处理。
 */

export class Paginator {
  /** Notify temporary visual effects when navigation or layout supersedes them. */
  onChange?: (reason: "navigation" | "layout") => void;
  private contentVersion = 0;
  private host: HTMLElement;
  private viewport: HTMLElement;
  private pageIndex = 0;
  private pageCount = 1;
  private gap = 48;
  private step = 0;
  /** 分栏目标栏宽（px）。列数由可用宽度自适应，栏宽≥可用宽度时自然为单栏 */
  private columnWidthPx = 730;
  /** 强制两栏：开启后无论可用宽度多少，分页模式固定两栏 */
  private forceTwoCols = false;
  private ro: ResizeObserver;

  constructor(viewport: HTMLElement, host: HTMLElement) {
    this.viewport = viewport;
    this.host = host;
    // 窗口尺寸变化时重新排版，尽量保持当前页比例
    this.ro = new ResizeObserver(() => this.relayout());
    this.ro.observe(this.viewport);
  }

  /** 设置分栏目标栏宽（纯目标值，列数自适应）。由 applySettings 调用（随后触发 relayout） */
  setColumnWidth(px: number): void {
    this.columnWidthPx = Math.max(1, Math.floor(px));
  }

  /** 设置强制两栏开关。由 applySettings 调用（随后触发 relayout） */
  setForceTwoColumns(on: boolean): void {
    this.forceTwoCols = !!on;
  }

  get currentPage(): number {
    return this.pageIndex;
  }

  get totalPages(): number {
    return this.pageCount;
  }

  /**
   * 设置新内容（HTML 已经过双重清洗），回到第 0 页。
   * beforeLayout：布局前钩子（如裸文本脚注包裹）——标记样式若影响行盒
   * （上标/小字号），必须在首次测量前完成，否则分页错位。
   */
  setContent(html: string, beforeLayout?: (host: HTMLElement) => void): void {
    const version = ++this.contentVersion;
    this.onChange?.("navigation");
    this.host.innerHTML = html;
    beforeLayout?.(this.host);
    this.pageIndex = 0;
    // 等图片尺寸就绪后再测量，否则页数会算少
    this.layout();
    if (Array.from(this.host.querySelectorAll("img")).some(img => !img.complete)) {
      void this.waitForImages().then(() => {
        if (version === this.contentVersion) this.layout();
      });
    }
  }

  /** 翻到下一页；已在最后一页时返回 false（由调用方加载下一章） */
  nextPage(): boolean {
    if (this.pageIndex >= this.pageCount - 1) return false;
    this.pageIndex += 1;
    this.apply();
    return true;
  }

  /**
   * 批量跳页（悬停进度数字的快速翻页）：单次 transform、无动画。
   * 返回实际移动页数（在章首/章尾截断后），剩余页数由调用方跨章消费。
   */
  jumpBy(delta: number): number {
    const target = Math.min(Math.max(this.pageIndex + delta, 0), this.pageCount - 1);
    const moved = target - this.pageIndex;
    if (moved !== 0) {
      this.pageIndex = target;
      this.apply(false);
    }
    return moved;
  }

  /** 翻到上一页；已在第一页返回 false（由调用方加载上一章） */
  prevPage(): boolean {
    if (this.pageIndex <= 0) return false;
    this.pageIndex -= 1;
    this.apply();
    return true;
  }

  goTo(page: number): void {
    this.pageIndex = Math.max(0, Math.min(this.pageCount - 1, Math.floor(page)));
    this.apply();
  }

  /** 元素在 host 内容坐标系中的横向位置（offsetLeft 相对 offsetParent，需扣除 host 自身偏移） */
  private localLeft(el: HTMLElement): number {
    return el.offsetLeft - this.host.offsetLeft;
  }

  /** 章内锚点（元素 id）所在页；找不到返回 null */
  pageOfAnchor(anchor: string): number | null {
    if (!anchor) return null;
    try {
      const el = this.host.querySelector(`#${CSS.escape(anchor)}`);
      if (!el) return null;
      return this.pageOfContents(el);
    } catch {
      return null; // 非法 id 字符
    }
  }

  /**
   * 指定页第一个带 id 的元素（Q5：保存进度用）。
   *
   * 只返回**实际渲染在本页内**（横向落在 [start, end) 页界）的 id；
   * 本页无 id 时返回 null（进度退化为存页码）。页界公式与 pageOfContents
   * 严格一致（[start-1, end-1)），保证保存的锚点恢复时算回同一页——
   * 旧版只设下界，页内无 id 时会拿到后面页的 id，恢复时跳过头。
   */
  anchorOfPage(page: number): string | null {
    if (page <= 0) return null;
    const start = page * this.step;
    const end = start + this.step;
    let best: string | null = null;
    let bestLeft = Infinity;
    this.host.querySelectorAll<HTMLElement>("[id]").forEach((node) => {
      const left = this.localLeft(node);
      if (left >= start - 1 && left < end - 1 && left < bestLeft) {
        bestLeft = left;
        best = node.id;
      }
    });
    return best;
  }

  /** 页首（含）之前最后一个带 id 的元素（分页 TOC 高亮：当前页所属小节） */
  lastAnchorUpTo(page: number): string | null {
    if (this.step <= 0 || page < 0) return null;
    const end = (page + 1) * this.step - 1;
    let best: string | null = null;
    let bestLeft = -Infinity;
    this.host.querySelectorAll<HTMLElement>("[id]").forEach((node) => {
      const left = this.localLeft(node);
      if (left <= end && left > bestLeft) {
        bestLeft = left;
        best = node.id;
      }
    });
    return best;
  }

  /**
   * 单个矩形所在页码（书签指纹定位的「字符渲染在本页」验证用）。
   * 坐标经 hostRect 抵消当前 transform（两者位移相同，翻页动画中也精确），
   * 页界公式与 pageOfContents 一致。
   */
  rectPage(rect: DOMRect): number | null {
    if (this.step <= 0) return null;
    const left = rect.left - this.host.getBoundingClientRect().left;
    return Math.max(0, Math.min(this.pageCount - 1, Math.floor((left + 1) / this.step)));
  }

  /** 元素（如划线 mark）所在页码；元素不在 host 内返回 null（跳转笔记用） */
  pageOfElement(target: Element): number | null {
    if (this.step <= 0 || !this.host.contains(target)) return null;
    return this.pageOfContents(target);
  }

  /**
   * 元素「首个可见内容行盒」所在页。
   *
   * 不用 offsetLeft：多列 fragmentation 下，卡在页界的块的「首碎片」可能是
   * 落在上一页底部的空盒子（可见文本在下一页），offsetLeft 报告空碎片位置，
   * 导致跳转落后一页。改用 Range 实时矩形取第一个有尺寸的行盒——它是
   * 用户实际看到的文本起点。
   *
   * 坐标用 rect.left - hostRect.left 抵消当前 transform（两者位移相同，
   * 差值恒为布局坐标，翻页动画进行中也精确）。
   */
  private pageOfContents(el: Element): number | null {
    if (this.step <= 0 || !this.host.contains(el)) return null;
    const hostRect = this.host.getBoundingClientRect();
    const range = document.createRange();
    range.selectNodeContents(el);
    let left: number | null = null;
    for (const r of range.getClientRects()) {
      if (r.width > 0.5 && r.height > 0.5) {
        left = r.left - hostRect.left;
        break;
      }
    }
    if (left == null) {
      // 空内容兜底：用盒子自身位置
      const r = (el as HTMLElement).getBoundingClientRect();
      left = r.left - hostRect.left;
    }
    // floor 而非 round：页面 N 的内容横跨 [N*step, (N+1)*step)，round 会让
    // 页面中点之后的元素进位到下一页（+1px 容差对齐子像素噪声）
    return Math.max(0, Math.min(this.pageCount - 1, Math.floor((left + 1) / this.step)));
  }

  /** 窗口 resize 后重新测量；按比例保留阅读位置 */
  relayout(): void {
    if (!this.host.innerHTML) return;
    const ratio = this.pageCount > 1 ? this.pageIndex / (this.pageCount - 1) : 0;
    this.layout();
    this.pageIndex = Math.round(ratio * (this.pageCount - 1));
    this.apply(true, "layout");
  }

  private layout(): void {
    const vw = this.viewport.clientWidth;
    const vh = this.viewport.clientHeight;
    if (vw <= 0 || vh <= 0) return;

    // host 的水平 margin 是页边距，列实际可用宽度 = 视口宽 - 左右边距。
    // 必须让「列宽 + 列间距 = 步长」与浏览器实际列布局一致，
    // 否则每页错位，右侧会漏出下一页的文字。
    const cs = getComputedStyle(this.host);
    const mL = parseFloat(cs.marginLeft) || 0;
    const mR = parseFloat(cs.marginRight) || 0;
    const availW = Math.max(100, vw - mL - mR);

    // 分栏：强制两栏开关优先；否则按目标栏宽自适应列数（round），可用宽度
    // 不足约一栏时 round 结果为 1，即自然退化为单栏。每列实际宽度由浏览器按
    // (availW - gap*(cols-1)) / cols 均分，与步长计算保持一致。
    const cols = this.forceTwoCols ? 2 : Math.max(1, Math.round(availW / this.columnWidthPx));

    // 列高必须用视口的「内容高度」：clientHeight 含上下 padding（页边距），
    // 而 host 排在 top padding 之后，若直接用 clientHeight 当列高，
    // 每列底部会被视口裁掉一个 padding 高度——该处文字属于本页，
    // 翻页后不会重现，造成跳读。
    const vcs = getComputedStyle(this.viewport);
    const padT = parseFloat(vcs.paddingTop) || 0;
    const padB = parseFloat(vcs.paddingBottom) || 0;
    const colH = Math.max(100, vh - padT - padB);

    this.host.style.width = `${availW}px`;
    this.host.style.height = `${colH}px`;
    // columnCount 精确指定列数（column-width 会触发浏览器自行动态调整），
    // 浏览器将可用宽度均分为 cols 列，与上方步长计算保持一致
    this.host.style.columnCount = String(cols);
    this.host.style.columnGap = `${this.gap}px`;

    // 一「页」= 一屏（含 cols 列）；步长 = 一屏宽 + 列间距。
    // 触发一次同步回流，保证 scrollWidth 反映列布局
    void this.host.offsetHeight;
    const contentWidth = this.host.scrollWidth;
    this.step = availW + this.gap;
    this.pageCount = Math.max(1, Math.ceil((contentWidth + this.gap) / this.step));
    this.pageIndex = Math.min(this.pageIndex, this.pageCount - 1);
    this.apply(true, "layout");
  }

  private apply(animate = true, reason: "navigation" | "layout" = "navigation"): void {
    this.onChange?.(reason);
    if (animate) {
      this.host.style.transform = `translateX(${-this.pageIndex * this.step}px)`;
      return;
    }
    // 快速跳页：临时禁用过渡，避免连续跳转时动画糊成一团
    this.host.style.transition = "none";
    this.host.style.transform = `translateX(${-this.pageIndex * this.step}px)`;
    void this.host.offsetHeight; // 强制同步，确保恢复过渡后不会从旧位置补动画
    this.host.style.transition = "";
  }

  private waitForImages(): Promise<void> {
    const imgs = Array.from(this.host.querySelectorAll("img"));
    const pending = imgs.filter((img) => !img.complete);
    if (pending.length === 0) return Promise.resolve();
    return Promise.all(
      pending.map(
        (img) =>
          new Promise<void>((resolve) => {
            img.addEventListener("load", () => resolve(), { once: true });
            img.addEventListener("error", () => resolve(), { once: true });
          }),
      ),
    ).then(() => undefined);
  }
}
