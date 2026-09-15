/**
 * 全书全文搜索（前端侧）：归一化文本索引 / 命中定位 / 高亮 / 搜索面板 UI。
 *
 * 匹配一致性约定（与 Rust 侧 commands/search.rs 的归一化规则严格对齐）：
 * - 连续空白（含 &nbsp;）折叠为单个空格；
 * - 块级元素边界（含 <br>）计为一个空格。
 * 搜索命令返回的 quote / prefix 指纹即基于该归一化文本，
 * 前端用同一规则建立索引后即可精确复现命中位置。
 *
 * 高亮使用 CSS Custom Highlight API（零 DOM 改动，不打乱分页器文本索引
 * 与笔记 mark 结构）；WebView2 原生支持，不支持时静默降级（跳转不受影响）。
 */

import { api } from "./ipc";
import type { SearchMatch } from "./types";

// ────────────────────────── 类型 ──────────────────────────

/** 可跳转命中：同一 quote 在章内的出现序号（面板渲染结果时补充，定位消歧用） */
export interface JumpableMatch extends SearchMatch {
  occurrenceInChapter: number;
}

/** 块感知归一化文本索引 */
export interface SearchIndex {
  /** 归一化文本（空白折叠 + 块边界计空格） */
  text: string;
  /** 归一化内容片段所属的文本节点（纯空白分隔不占节点位） */
  nodes: Text[];
  /** nodes[i] 在 text 中的起始偏移 */
  starts: number[];
  /** nodes[i] 内「归一化局部偏移 → 原始局部偏移」映射（用于直接在原节点上建 Range） */
  rawMaps: number[][];
}

/** 本章命中定位结果 */
export interface ChapterHits {
  /** 本章命中切片（保持全书结果顺序） */
  matches: JumpableMatch[];
  /** matches[0] 在全书结果中的全局序号 */
  firstGlobalIndex: number;
  /** 各命中对应的 Range（同序；未定位到为 null） */
  ranges: (Range | null)[];
  /** 当前激活命中的 Range（无则 null） */
  activeRange: Range | null;
}

// ────────────────────────── 归一化索引 ──────────────────────────

/** 块级元素集合（大写 tagName）：与 Rust 侧 is_block_tag 一致 */
const BLOCK_TAGS = new Set([
  "P", "DIV", "BR", "HR",
  "H1", "H2", "H3", "H4", "H5", "H6",
  "LI", "TR", "TD", "TH", "BLOCKQUOTE", "TABLE",
  "SECTION", "ARTICLE", "ASIDE", "FIGURE", "FIGCAPTION",
  "PRE", "UL", "OL", "DL", "DT", "DD",
]);

/** 最近块级祖先（无则 null） */
function closestBlock(node: Node): Element | null {
  let el = node.nodeType === Node.ELEMENT_NODE ? (node as Element) : node.parentElement;
  while (el) {
    if (BLOCK_TAGS.has(el.tagName)) return el;
    el = el.parentElement;
  }
  return null;
}

/**
 * 构建块感知归一化文本索引。
 * 递归按文档序遍历（不能用 TreeWalker：需感知 <br> 等无文本的块级元素）。
 */
export function buildSearchIndex(root: Node): SearchIndex {
  const nodes: Text[] = [];
  const starts: number[] = [];
  const rawMaps: number[][] = [];
  let text = "";
  let lastBlock: Element | null = null;

  /** 追加边界空格（与已有尾部空格去重） */
  const appendBoundary = (): void => {
    if (text.length > 0 && !text.endsWith(" ")) text += " ";
  };

  const addText = (t: Text): void => {
    const raw = t.nodeValue ?? "";
    if (raw.length === 0) return;
    if (raw.trim().length === 0) {
      appendBoundary(); // 纯空白节点 = 边界空白
      return;
    }
    const block = closestBlock(t);
    if (block !== lastBlock) {
      appendBoundary();
      lastBlock = block;
    }
    // 折叠节点内空白：中部空白 → 单空格（映射到其后首个内容字符）
    const map: number[] = [];
    let local = "";
    let pendingWs = false;
    let leadingWs = false;
    let trailingWs = false;
    for (let i = 0; i < raw.length; i++) {
      if (/\s/.test(raw[i])) {
        if (local.length === 0) leadingWs = true;
        pendingWs = true;
        continue;
      }
      if (pendingWs && local.length > 0) {
        local += " ";
        map.push(i);
      }
      pendingWs = false;
      local += raw[i];
      map.push(i);
    }
    trailingWs = pendingWs;
    if (local.length === 0) {
      appendBoundary();
      return;
    }
    // 首部空白：全局文本尚无尾随空格时并入本节点（Range 可从空格处开始）
    if (leadingWs && text.length > 0 && !text.endsWith(" ")) {
      local = ` ${local}`;
      map.unshift(map[0]);
    }
    // 尾部空白：并入全局文本（不占本节点映射位）
    nodes.push(t);
    starts.push(text.length);
    rawMaps.push(map);
    text += local;
    if (trailingWs) appendBoundary();
  };

  const walk = (parent: Node): void => {
    for (let child = parent.firstChild; child; child = child.nextSibling) {
      if (child.nodeType === Node.ELEMENT_NODE) {
        const el = child as Element;
        const tag = el.tagName;
        if (tag === "SCRIPT" || tag === "STYLE") continue;
        if (tag === "BR") {
          appendBoundary(); // 行内换行 = 一个空格（与 Rust 侧一致）
          continue;
        }
        walk(el);
      } else if (child.nodeType === Node.TEXT_NODE) {
        addText(child as Text);
      }
    }
  };

  walk(root);
  return { text, nodes, starts, rawMaps };
}

// ────────────────────────── 命中定位 ──────────────────────────

/**
 * 收集 quote 在归一化文本中的全部出现偏移。
 * 带 prefix 时优先按 `prefix + quote` 整体定位（更锐利）；
 * 结果为空退化为仅按 quote 定位（与书签指纹 locateQuote 同语义）。
 */
export function locateOccurrences(
  index: SearchIndex,
  quote: string,
  prefix: string | null,
): number[] {
  const hay = index.text;
  if (!quote || hay.length === 0) return [];
  const out: number[] = [];
  if (prefix) {
    const needle = prefix + quote;
    let from = 0;
    for (let p = hay.indexOf(needle, from); p >= 0; p = hay.indexOf(needle, from)) {
      out.push(p + prefix.length);
      from = p + 1;
    }
  }
  if (out.length === 0) {
    let from = 0;
    for (let p = hay.indexOf(quote, from); p >= 0; p = hay.indexOf(quote, from)) {
      out.push(p);
      from = p + 1;
    }
  }
  return out;
}

/** 归一化偏移 → 节点内原始偏移（offset 落在节点间边界空格时取邻近内容边界） */
function rawOffsetAt(index: SearchIndex, offset: number, isEnd: boolean): { node: Text; offset: number } | null {
  const { nodes, starts, rawMaps } = index;
  if (nodes.length === 0) return null;
  // 找最后一个 starts[i] <= offset 的节点（starts 单调递增）
  let lo = 0;
  let hi = nodes.length - 1;
  let found = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (starts[mid] <= offset) {
      found = mid;
      lo = mid + 1;
    } else {
      hi = mid - 1;
    }
  }
  if (found < 0) found = 0; // offset 在首个节点前（防御：取首个节点开头）
  const node = nodes[found];
  const map = rawMaps[found];
  const local = offset - starts[found];
  if (local < map.length) return { node, offset: map[local] };
  // 越过本节点内容（位于边界空格处）：取内容末尾 ±1 的原始偏移
  const lastRaw = map[map.length - 1];
  return { node, offset: Math.min(node.data.length, lastRaw + (isEnd ? 1 : 0)) };
}

/** 归一化偏移区间 → DOM Range（不改 DOM；区间完全落在索引之外时返回 null） */
export function rangeAt(index: SearchIndex, start: number, end: number): Range | null {
  if (end <= start || index.nodes.length === 0) return null;
  const from = rawOffsetAt(index, start, false);
  const to = rawOffsetAt(index, end, true);
  if (!from || !to) return null;
  const range = document.createRange();
  try {
    range.setStart(from.node, from.offset);
    range.setEnd(to.node, to.offset);
  } catch {
    return null;
  }
  return range;
}

/**
 * 在已渲染章节容器内定位本章全部命中并应用高亮。
 * 相同 quote 的多条命中按出现序（occurrenceInChapter）贪心分配位置。
 */
export function computeChapterHits(
  root: Node,
  results: JumpableMatch[] | null,
  activeGlobalIdx: number,
): ChapterHits {
  const index = buildSearchIndex(root);
  const all = results ?? [];
  // 找到包含激活命中（或首个命中）的章节切片：结果按章节连续排列，扫描一次即可
  const chapter = activeGlobalIdx >= 0 ? all[activeGlobalIdx]?.chapterIndex : all[0]?.chapterIndex;
  const matches: JumpableMatch[] = [];
  let firstGlobalIndex = -1;
  if (chapter != null) {
    for (let i = 0; i < all.length; i++) {
      if (all[i].chapterIndex !== chapter) continue;
      if (firstGlobalIndex < 0) firstGlobalIndex = i;
      matches.push(all[i]);
    }
  }

  const ranges: (Range | null)[] = [];
  const quoteOffsets = new Map<string, number[]>();
  let activeRange: Range | null = null;
  for (let i = 0; i < matches.length; i++) {
    const m = matches[i];
    const offs = locateOccurrences(index, m.quote, m.prefix);
    let pos = offs[m.occurrenceInChapter];
    if (pos == null) {
      // prefix 整体定位未覆盖该序号（上下文与指纹不一致时），退化为按 quote 的第 occ 个
      let q = quoteOffsets.get(m.quote);
      if (!q) {
        q = locateOccurrences(index, m.quote, null);
        quoteOffsets.set(m.quote, q);
      }
      pos = q[m.occurrenceInChapter] ?? q[0];
    }
    const range = pos != null ? rangeAt(index, pos, pos + m.quote.length) : null;
    ranges.push(range);
    if (firstGlobalIndex >= 0 && activeGlobalIdx === firstGlobalIndex + i && range) {
      activeRange = range;
    }
  }
  return { matches, firstGlobalIndex, ranges, activeRange };
}

// ────────────────────────── 高亮 ──────────────────────────

const HIGHLIGHT_HITS = "search-hits";
const HIGHLIGHT_ACTIVE = "search-active";

function supportsHighlights(): boolean {
  return typeof CSS !== "undefined" && "highlights" in CSS;
}

/** 设置正文高亮：ranges 为本章全部命中，active 为当前命中（不同配色强调） */
export function setHighlights(ranges: Range[], active: Range | null): void {
  if (!supportsHighlights()) return;
  CSS.highlights.delete(HIGHLIGHT_HITS);
  CSS.highlights.delete(HIGHLIGHT_ACTIVE);
  if (ranges.length > 0) CSS.highlights.set(HIGHLIGHT_HITS, new Highlight(...ranges));
  if (active) CSS.highlights.set(HIGHLIGHT_ACTIVE, new Highlight(active));
}

/** 清除正文高亮 */
export function clearHighlights(): void {
  if (!supportsHighlights()) return;
  CSS.highlights.delete(HIGHLIGHT_HITS);
  CSS.highlights.delete(HIGHLIGHT_ACTIVE);
}

// ────────────────────────── 搜索面板 ──────────────────────────

/** 输入防抖（ms）：避免每敲一个字都全书的扫一遍 */
const SEARCH_DEBOUNCE_MS = 250;

/** 独立搜索侧边栏（#search-panel）：输入 → 全书检索 → 结果列表 → 跳转回调 */
export class SearchPanel {
  private readonly panel: HTMLElement;
  private readonly input: HTMLInputElement;
  private readonly meta: HTMLElement;
  private readonly listEl: HTMLElement;
  private readonly closeBtn: HTMLButtonElement;

  /** 最近一次搜索结果（含章内序号），供 main.ts 跳转定位 */
  private _results: JumpableMatch[] | null = null;
  /** 当前激活命中在 _results 中的全局序号；-1 = 无 */
  private _activeIndex = -1;
  /** 防过期结果回写的自增 token */
  private runToken = 0;
  private debounceTimer: number | null = null;
  private lastQuery = "";
  private bookId: string | null = null;

  /** 点击结果跳转（main.ts 注入：负责跨章加载与视图定位） */
  onJump: (match: JumpableMatch, globalIndex: number) => void = () => {};
  /** 面板打开后回调（main.ts 编排面板互斥 / relayout / 唤起顶栏） */
  onOpen: () => void = () => {};
  /** 面板关闭后回调（main.ts 编排清理 / relayout） */
  onClose: () => void = () => {};

  constructor(
    panel: HTMLElement,
    input: HTMLInputElement,
    meta: HTMLElement,
    listEl: HTMLElement,
    closeBtn: HTMLButtonElement,
  ) {
    this.panel = panel;
    this.input = input;
    this.meta = meta;
    this.listEl = listEl;
    this.closeBtn = closeBtn;

    this.input.addEventListener("input", () => {
      if (this.debounceTimer !== null) window.clearTimeout(this.debounceTimer);
      this.debounceTimer = window.setTimeout(() => {
        this.debounceTimer = null;
        void this.runSearch();
      }, SEARCH_DEBOUNCE_MS);
    });
    this.input.addEventListener("keydown", (e) => {
      if (e.key === "Escape") {
        e.preventDefault();
        this.close();
      } else if (e.key === "Enter") {
        e.preventDefault();
        void this.runSearch().then(() => this.jumpRelative(this.lastQuery === this.input.value.trim() ? 1 : 0));
      }
    });
    this.closeBtn.addEventListener("click", () => this.close());
    // 结果项点击（事件委托，500 条列表只需一个监听器）
    this.listEl.addEventListener("click", (e) => {
      const item = (e.target as HTMLElement).closest<HTMLElement>(".sp-item");
      const idx = item?.dataset.idx;
      if (idx == null) return;
      const i = Number(idx);
      this.setActive(i);
      this.onJump(this._results![i], i);
    });
  }

  get isOpen(): boolean {
    return !this.panel.classList.contains("hidden");
  }

  get results(): JumpableMatch[] | null {
    return this._results;
  }

  get activeIndex(): number {
    return this._activeIndex;
  }

  get query(): string {
    return this.lastQuery;
  }

  /** 绑定当前书籍（关书 / 换书时置 null 并 reset） */
  setBook(bookId: string | null): void {
    this.bookId = bookId;
    this.reset();
  }

  open(): void {
    if (!this.isOpen) {
      this.panel.classList.remove("hidden");
      this.onOpen();
    }
    this.input.focus();
    this.input.select();
  }

  close(): void {
    if (!this.isOpen) return;
    this.panel.classList.add("hidden");
    this.input.blur();
    this.onClose();
  }

  toggle(): void {
    if (this.isOpen) this.close();
    else this.open();
  }

  /** 清空输入、结果与高亮（关书 / 回书架 / 打开新书） */
  reset(): void {
    if (this.debounceTimer !== null) {
      window.clearTimeout(this.debounceTimer);
      this.debounceTimer = null;
    }
    this.runToken++;
    this._results = null;
    this._activeIndex = -1;
    this.lastQuery = "";
    this.input.value = "";
    this.meta.textContent = "";
    this.listEl.innerHTML = "";
    clearHighlights();
  }

  /** 执行全书搜索（空查询清空结果） */
  async runSearch(): Promise<void> {
    const query = this.input.value.trim();
    if (!query || !this.bookId) {
      this.lastQuery = query;
      this.renderResults(null, 0, false);
      return;
    }
    const token = ++this.runToken;
    this.lastQuery = query;
    this.meta.textContent = "正在搜索…";
    try {
      const result = await api.searchBook(this.bookId, query);
      if (token !== this.runToken) return; // 过期结果丢弃
      const matches = annotateOccurrences(result.matches);
      this._results = matches;
      this._activeIndex = -1;
      this.renderResults(matches, result.total, result.truncated);
    } catch (err) {
      if (token !== this.runToken) return;
      this._results = null;
      this._activeIndex = -1;
      this.listEl.innerHTML = "";
      this.meta.textContent = `搜索失败：${String(err)}`;
    }
  }

  /** 跳到相邻命中（delta=0 首个 / 1 下一个；循环） */
  jumpRelative(delta: number): void {
    const results = this._results;
    if (!results || results.length === 0) return;
    const base = this._activeIndex < 0 ? (delta > 0 ? -1 : 0) : this._activeIndex;
    const next = (base + Math.max(delta, 0) + 1) % results.length;
    this.setActive(next);
    this.onJump(results[next], next);
  }

  /** 同步激活结果项（列表高亮 + 滚动可见）；-1 = 清除 */
  setActive(globalIndex: number): void {
    this._activeIndex = globalIndex;
    const items = this.listEl.querySelectorAll<HTMLElement>(".sp-item");
    items.forEach((item) => {
      const on = Number(item.dataset.idx) === globalIndex;
      item.classList.toggle("active", on);
      if (on) item.scrollIntoView({ block: "nearest" });
    });
  }

  /**
   * 章节渲染完成后重建本章高亮（DOM 已换新，Range 全部失效）。
   * root 由 main.ts 传入（分页模式 page-host / 滚动模式当前 section）。
   */
  onChapterRendered(root: Node): void {
    if (!this._results || this._activeIndex < 0) {
      clearHighlights();
      return;
    }
    const hits = computeChapterHits(root, this._results, this._activeIndex);
    setHighlights(hits.ranges.filter((r): r is Range => r != null), hits.activeRange);
  }

  /** 渲染结果列表与统计行 */
  private renderResults(matches: JumpableMatch[] | null, total: number, truncated: boolean): void {
    this.listEl.innerHTML = "";
    if (!matches || matches.length === 0) {
      if (this.lastQuery) {
        this.meta.textContent = "未找到匹配内容";
        const empty = document.createElement("div");
        empty.className = "sp-empty";
        empty.textContent = "没有找到包含该关键词的内容，换个关键词试试。";
        this.listEl.appendChild(empty);
      } else {
        this.meta.textContent = "";
      }
      return;
    }
    const chapterCount = new Set(matches.map((m) => m.chapterIndex)).size;
    this.meta.textContent = truncated
      ? `共 ${total} 处 · ${chapterCount} 章（仅显示前 ${matches.length} 处）`
      : `共 ${total} 处 · ${chapterCount} 章`;
    const frag = document.createDocumentFragment();
    matches.forEach((m, i) => frag.appendChild(this.renderItem(m, i)));
    this.listEl.appendChild(frag);
    // 恢复激活项标记
    if (this._activeIndex >= 0) this.setActive(this._activeIndex);
  }

  private renderItem(m: JumpableMatch, idx: number): HTMLElement {
    const item = document.createElement("div");
    item.className = "sp-item";
    item.dataset.idx = String(idx);

    const head = document.createElement("div");
    head.className = "sp-item-head";
    const chapter = document.createElement("span");
    chapter.className = "sp-item-chapter";
    chapter.textContent = m.chapterTitle;
    const no = document.createElement("span");
    no.className = "sp-item-no";
    no.textContent = String(idx + 1);
    head.append(chapter, no);

    const body = document.createElement("div");
    body.className = "sp-item-text";
    body.append(
      document.createTextNode(m.pre),
      Object.assign(document.createElement("mark"), { className: "sp-hit", textContent: m.quote }),
      document.createTextNode(m.post),
    );

    item.append(head, body);
    return item;
  }
}

/** 给 Rust 返回的命中补充「同章同文出现序号」（结果按章节 + 文档序排列，扫一遍即可） */
function annotateOccurrences(matches: SearchMatch[]): JumpableMatch[] {
  const counters = new Map<string, number>();
  return matches.map((m) => {
    const key = `${m.chapterIndex}\u0000${m.quote}`;
    const occ = counters.get(key) ?? 0;
    counters.set(key, occ + 1);
    return { ...m, occurrenceInChapter: occ };
  });
}
