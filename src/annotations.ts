/**
 * 笔记（划线 + 想法）与书签（需求 6）。
 *
 * 定位策略：笔记不存 DOM 结构（跨会话/跨字号易碎），存「划线原文 + 前后
 * 各 12 字符上下文」的纯文本指纹。章节渲染后按指纹在正文中搜索定位，
 * 用 <mark class="note-mark"> 包裹文本节点；对 EPUB / TXT / MOBI / FB2 /
 * MD 等所有文字格式通用。PDF 走 canvas 渲染无文本层，不支持划线（书签可用）。
 *
 * 书签 = 隐形笔记：取阅读页渲染的最后一个句子结束符后的 20 字引文作指纹
 * （quote+prefix+suffix，与笔记同构），跳转时按指纹重新定位；无文本/无标点
 * （漫画、PDF）退化为页码/同章兜底。摘要是「首句号后 10 字 … 末句号后 10 字」
 * 的概括式显示。
 *
 * 数据流：前端内存是唯一编辑方，任何增删改后整体写回后端 annotations.json。
 * UI：选中文字弹出两行划线工具条（上行颜色，下行样式+想法/复制）；点颜色即按当前笔型划线
 * 编辑器；侧栏面板列出全部书签/笔记，点击跳转。
 */
import { api } from "./ipc";
import type { AnnotationsFile, Bookmark, Note, NoteColor, NoteStyle } from "./types";

/** 划线上下文消歧长度（前/后各取多少字符） */
const CONTEXT_LEN = 12;

const NOTE_COLORS: NoteColor[] = ["yellow", "green", "blue", "pink", "purple"];
const NOTE_STYLES: NoteStyle[] = ["highlight", "underline", "wavy"];

// ────────────────────────── 文本索引与包裹 ──────────────────────────

/** 章节容器内全部文本节点的线性索引（定位与指纹提取共用） */
export interface TextIndex {
  /** 所有文本节点值拼接 */
  text: string;
  nodes: Text[];
  /** nodes[i] 在 text 中的起始偏移 */
  starts: number[];
}

export function buildTextIndex(root: Node): TextIndex {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, {
    acceptNode(node) {
      const tag = node.parentElement?.tagName;
      return tag === "SCRIPT" || tag === "STYLE" ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT;
    },
  });
  const nodes: Text[] = [];
  const starts: number[] = [];
  let text = "";
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    nodes.push(n as Text);
    starts.push(text.length);
    text += n.nodeValue ?? "";
  }
  return { text, nodes, starts };
}

/** Range 边界 → 线性文本偏移（只处理文本节点边界，浏览器常规行为） */
function globalOffset(index: TextIndex, node: Node, offset: number): number {
  if (node.nodeType !== Node.TEXT_NODE) return -1;
  const i = index.nodes.indexOf(node as Text);
  return i >= 0 ? index.starts[i] + offset : -1;
}

/** 按指纹（quote + 上下文）在线性文本中定位 [start, end) */
function locateQuote(index: TextIndex, quote: string, prefix: string, suffix: string): [number, number] | null {
  if (!quote) return null;
  if (prefix || suffix) {
    const hay = prefix + quote + suffix;
    const p = index.text.indexOf(hay);
    if (p >= 0) return [p + prefix.length, p + prefix.length + quote.length];
  }
  const q = index.text.indexOf(quote);
  return q >= 0 ? [q, q + quote.length] : null;
}

/**
 * 把 [start, end) 内的文本段逐段包进 <mark>。
 * 跨块元素（如跨段落）会产生多个 mark，视觉上是连续划线。
 * 已属于其他划线的文本段跳过（不允许嵌套包裹）。
 * 返回成功包裹的段数。
 */
function wrapOffsetRange(index: TextIndex, start: number, end: number, noteId: string, color: NoteColor, style: NoteStyle): number {
  if (start < 0 || end <= start) return 0;
  let wrapped = 0;
  for (let i = 0; i < index.nodes.length; i++) {
    const nodeStart = index.starts[i];
    const nodeLen = index.nodes[i].nodeValue?.length ?? 0;
    if (nodeLen === 0) continue;
    const nodeEnd = nodeStart + nodeLen;
    if (nodeEnd <= start) continue;
    if (nodeStart >= end) break;
    const segStart = Math.max(nodeStart, start);
    const segEnd = Math.min(nodeEnd, end);
    if (segStart >= segEnd) continue;
    const node = index.nodes[i];
    if (node.parentElement?.closest("mark.note-mark")) continue;

    // 切出 [segStart, segEnd) 对应的独立文本节点
    let target: Text = node;
    if (segStart > nodeStart) target = node.splitText(segStart - nodeStart);
    if (segEnd < nodeEnd) target.splitText(segEnd - segStart);

    const mark = document.createElement("mark");
    mark.className = `note-mark nc-${color} ns-${style}`;
    mark.dataset.noteId = noteId;
    target.parentNode?.insertBefore(mark, target);
    mark.appendChild(target);
    wrapped++;
  }
  return wrapped;
}

/** 摘除某条笔记的全部 <mark> 并合并相邻文本节点 */
function unwrapNote(container: HTMLElement, noteId: string): void {
  container.querySelectorAll<HTMLElement>(`mark.note-mark[data-note-id="${CSS.escape(noteId)}"]`).forEach((m) => {
    const parent = m.parentNode;
    if (!parent) return;
    while (m.firstChild) parent.insertBefore(m.firstChild, m);
    m.remove();
  });
  container.normalize();
}

// ────────────────────────── 书签摘要提取（旧滚动摘要已由指纹方案取代） ──────────────────────────

/** 跨页 mark 会被列布局拆成多个分片矩形：getBoundingClientRect 返回跨页
 *  并集（left 可能为负），弹窗会定位到窗口边缘。取点击点命中的分片（或
 *  距点击点最近的分片）定位，弹窗才不受划线跨两页影响。 */
function clickedFragmentRect(mark: HTMLElement, x: number, y: number): DOMRect {
  const rects = Array.from(mark.getClientRects());
  if (rects.length <= 1) return mark.getBoundingClientRect();
  let best = rects[0];
  let bestDist = Infinity;
  for (const r of rects) {
    if (x >= r.left && x <= r.right && y >= r.top && y <= r.bottom) return r;
    const dx = x < r.left ? r.left - x : x > r.right ? x - r.right : 0;
    const dy = y < r.top ? r.top - y : y > r.bottom ? y - r.bottom : 0;
    const d = dx * dx + dy * dy;
    if (d < bestDist) {
      bestDist = d;
      best = r;
    }
  }
  return best;
}

// ────────────────────────── 书签指纹（隐形笔记式定位） ──────────────────────────

/** 句子结束符（锚点首选） */
const SENTENCE_END = /[。．.!！?？…]/;
/** 任意标点退化方案：非字母数字（含中日韩文）、非空白即视为标点 */
const PUNCT_FALLBACK = /[^0-9A-Za-z\u3040-\u30ff\u3400-\u4dbf\u4e00-\u9fff\uac00-\ud7af\s]/;

const QUOTE_LEN = 20; // 锚点标点后的引文长度
const SUFFIX_LEN = 12; // 引文后的消歧上下文
const EXCERPT_LEN = 10; // 摘要首/末段长度

/** 书签指纹：结构与 Note 的定位字段一致（quote+prefix+suffix），可复用指纹检索 */
export interface BookmarkFingerprint {
  quote: string;
  prefix: string;
  suffix: string;
  /** 面板摘要：首个标点后 10 字 … 锚点后 10 字（仅一个标点时单段） */
  excerpt: string;
}

/**
 * 收集「通过渲染验证」的标点偏移（升序）。
 *
 * 先句子结束符，无命中再任意标点。verifyChar(全局偏移) 判定该字符是否
 * 渲染在当前页/视口内（调用方注入：分页 = paginator.rectPage === 当前页，
 * 滚动 = 行盒与视口相交），防止跨页文本节点把相邻页的标点误纳入。
 * 含目标字符的文本节点先做一次节点级矩形预筛，通过才逐字精确验证。
 */
export function collectAnchorPositions(
  index: TextIndex,
  verifyChar: (offset: number) => boolean,
  verifyRect: (rect: DOMRect) => boolean,
): number[] {
  for (const re of [SENTENCE_END, PUNCT_FALLBACK]) {
    const hits: number[] = [];
    for (let i = 0; i < index.nodes.length; i++) {
      const node = index.nodes[i];
      const value = node.nodeValue ?? "";
      if (!re.test(value)) continue;
      // 节点级预筛：节点的任一行盒渲染在目标区域内才进入逐字验证
      const range = document.createRange();
      range.selectNodeContents(node);
      let anyRect = false;
      for (const r of range.getClientRects()) {
        if (r.width > 0.5 && r.height > 0.5 && verifyRect(r)) {
          anyRect = true;
          break;
        }
      }
      if (!anyRect) continue;
      for (let off = 0; off < value.length; off++) {
        if (re.test(value[off]) && verifyChar(index.starts[i] + off)) {
          hits.push(index.starts[i] + off);
        }
      }
    }
    if (hits.length > 0) return hits;
  }
  return [];
}

/**
 * 由候选标点位置构造书签指纹。
 *
 * 指纹锚点 = 最后一个候选中标点里「其后 ≥20 字」的最靠后者（不足 20 字的
 * 章末标点按用户规则向前回退）；全部不足 → null（调用方走兜底定位）。
 * 摘要末段独立取「其后 ≥10 字」的最靠后者（阈值不同，末段可能比指纹锚点
 * 更靠后）。prefix 含锚点标点本身，跳转以标点字符为目标。
 */
export function computePageFingerprint(positions: number[], text: string): BookmarkFingerprint | null {
  if (positions.length === 0) return null;
  let anchor = -1;
  for (let i = positions.length - 1; i >= 0; i--) {
    if (text.length - (positions[i] + 1) >= QUOTE_LEN) {
      anchor = positions[i];
      break;
    }
  }
  if (anchor < 0) return null;
  const quote = text.slice(anchor + 1, anchor + 1 + QUOTE_LEN);
  const prefix = text.slice(Math.max(0, anchor - 2), anchor + 1);
  const suffix = text.slice(anchor + 1 + QUOTE_LEN, anchor + 1 + QUOTE_LEN + SUFFIX_LEN);

  // 摘要末段：最后一个「其后 ≥10 字」的标点（指纹锚点必满足，故必存在）
  let lastPos = positions[positions.length - 1];
  for (let i = positions.length - 1; i >= 0; i--) {
    if (text.length - (positions[i] + 1) >= EXCERPT_LEN) {
      lastPos = positions[i];
      break;
    }
  }
  const firstTail = text.slice(positions[0] + 1, positions[0] + 1 + EXCERPT_LEN);
  const lastTail = text.slice(lastPos + 1, lastPos + 1 + EXCERPT_LEN);
  const excerpt = positions[0] === lastPos ? lastTail : `${firstTail}…${lastTail}`;
  return { quote, prefix, suffix, excerpt };
}

/** 线性偏移 → 所在文本节点及节点内偏移（二分；索引过期越界返回 null） */
function nodeAt(index: TextIndex, offset: number): { node: Text; offset: number } | null {
  if (offset < 0 || offset > index.text.length || index.nodes.length === 0) return null;
  let lo = 0;
  let hi = index.starts.length - 1;
  let ans = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (index.starts[mid] <= offset) {
      ans = mid;
      lo = mid + 1;
    } else {
      hi = mid - 1;
    }
  }
  if (ans < 0) return null;
  const node = index.nodes[ans];
  const local = offset - index.starts[ans];
  if (local > (node.nodeValue?.length ?? 0)) return null;
  return { node, offset: local };
}

/** 线性偏移 [start, end) → 跨文本节点 Range（书签跳转目标用） */
export function rangeAtOffset(index: TextIndex, start: number, end: number): Range | null {
  const s = nodeAt(index, start);
  const e = nodeAt(index, end);
  if (!s || !e) return null;
  const range = document.createRange();
  try {
    range.setStart(s.node, s.offset);
    range.setEnd(e.node, e.offset);
  } catch {
    return null;
  }
  return range;
}

/**
 * 在章节线性文本中查找书签指纹的全部命中（锚点标点的偏移，升序）。
 * 优先 prefix+quote 整体匹配（消歧），无命中退化裸 quote（标点在其前 1 位）。
 */
export function locateFingerprintOccurrences(index: TextIndex, quote: string, prefix: string | null): number[] {
  if (!quote) return [];
  const out: number[] = [];
  if (prefix) {
    const hay = prefix + quote;
    let p = index.text.indexOf(hay);
    while (p >= 0) {
      out.push(p + prefix.length - 1);
      p = index.text.indexOf(hay, p + 1);
    }
  }
  if (out.length === 0) {
    let q = index.text.indexOf(quote);
    while (q >= 0) {
      if (q > 0) out.push(q - 1);
      q = index.text.indexOf(quote, q + 1);
    }
  }
  return out;
}

// ────────────────────────── 管理器 ──────────────────────────

/** 当前阅读位置描述（书签判定/新增用） */
export interface PositionInfo {
  chapterIndex: number;
  anchor: string | null;
  pageInChapter: number | null;
  /** 隐形笔记定位指纹；null = 兜底定位（无文本/旧数据） */
  quote: string | null;
  prefix: string | null;
  suffix: string | null;
  excerpt: string;
}

export interface AnnotationHost {
  /** 持久化（main.ts 转发到后端） */
  save(annotations: AnnotationsFile): void;
  /** 章节标题（面板展示用） */
  chapterTitle(index: number): string;
  jumpToNote(note: Note): void;
  jumpToBookmark(bm: Bookmark): void;
  /** 数据变化（书签增删后面板/按钮状态同步） */
  onChanged(): void;
}

export class AnnotationManager {
  private notes: Note[] = [];
  private bookmarks: Bookmark[] = [];
  private bookId = "";
  private cb: AnnotationHost;

  /** 划线弹窗的待处理选区（container：选区所在的章节容器，避免全局查询歧义） */
  private pending: { range: Range; text: string; chapter: number; container: HTMLElement } | null = null;
  /** 本回合已落地的划线 id（工具栏保持打开时连续换色/换样式用；DOM 重建后 pending.range 会失效） */
  private pendingNoteId: string | null = null;
  /** 想法编辑器当前编辑的笔记 id */
  private editingId: string | null = null;
  /** 编辑器锚点矩形（打开时划线的位置）：改颜色/样式后复用，避免弹窗漂移 */
  private editorAnchorRect: DOMRect | null = null;
  private lastColor: NoteColor = "yellow";
  private lastStyle: NoteStyle = "highlight";
  private panelTab: "bookmarks" | "notes" = "bookmarks";

  private selPopup: HTMLElement;
  private noteEditor: HTMLElement;
  private npList: HTMLElement;
  private panel: HTMLElement;
  private containers: HTMLElement[] = [];

  constructor(cb: AnnotationHost) {
    this.cb = cb;
    this.selPopup = document.getElementById("sel-popup")!;
    this.noteEditor = document.getElementById("note-editor")!;
    this.panel = document.getElementById("notes-panel")!;
    this.npList = document.getElementById("np-list")!;

    this.bindUi();
  }

  /** 绑定正文容器（滚动视图 / 分页 host），监听选区与划线点击 */
  bindContent(containers: HTMLElement[]): void {
    for (const c of containers) {
      this.containers.push(c);
      c.addEventListener("click", (e) => this.onContentClick(e));
      c.addEventListener("scroll", () => this.hidePopups(), { passive: true });
    }
    // 选区常拖出可见页（分页 clip 外 / 边缘翻页热区 / 视口边缘），
    // 此时 mouseup 落在容器外——必须挂在 document 上才能弹出工具栏
    document.addEventListener("mouseup", (e) => {
      const t = e.target as HTMLElement | null;
      if (t?.closest("#sel-popup, #note-editor, #np-menu")) return;
      this.onMouseUp({ x: e.clientX, y: e.clientY });
    });
  }

  // ─────────────────── 数据生命周期 ───────────────────

  /** 打开书籍时加载该书全部笔记/书签 */
  async loadFor(bookId: string): Promise<void> {
    this.hidePopups();
    this.bookId = bookId;
    try {
      const a = await api.getAnnotations(bookId);
      this.notes = a.notes;
      this.bookmarks = a.bookmarks;
    } catch (err) {
      console.error(err);
      this.notes = [];
      this.bookmarks = [];
    }
    this.renderPanel();
  }

  /** 回到书架/换书时清空内存状态 */
  reset(): void {
    this.hidePopups();
    this.bookId = "";
    this.notes = [];
    this.bookmarks = [];
    this.renderPanel();
  }

  getNotes(): readonly Note[] {
    return this.notes;
  }

  private persist(): void {
    this.cb.save({ notes: this.notes, bookmarks: this.bookmarks });
  }

  // ─────────────────── 划线应用（章节渲染后调用） ───────────────────

  /** 把属于 chapterIndex 的笔记重新包裹到刚渲染好的章节容器上 */
  applyToChapter(chapterIndex: number, container: HTMLElement): void {
    if (!this.bookId) return;
    const chapterNotes = this.notes
      .filter((n) => n.chapterIndex === chapterIndex)
      .sort((a, b) => b.quote.length - a.quote.length); // 长的先包，减少碎片
    for (const note of chapterNotes) {
      const index = buildTextIndex(container); // 每次重建：包裹会拆分文本节点
      const pos = locateQuote(index, note.quote, note.prefix, note.suffix);
      if (pos) wrapOffsetRange(index, pos[0], pos[1], note.id, note.color, note.style);
    }
  }

  // ─────────────────── 选区 → 划线 ───────────────────

  private onMouseUp(pos?: { x: number; y: number }): void {
    const sel = window.getSelection();
    if (!sel || sel.isCollapsed || sel.rangeCount === 0) {
      this.hideSelPopup();
      return;
    }
    const range = sel.getRangeAt(0);
    // 优先用选区起点定位章节容器：跨列/跨节点时 commonAncestor 可能不是章节元素
    const startEl =
      range.startContainer.nodeType === Node.TEXT_NODE
        ? range.startContainer.parentElement
        : (range.startContainer as HTMLElement);
    const anc = range.commonAncestorContainer;
    const ancEl = (anc.nodeType === Node.TEXT_NODE ? anc.parentElement : anc as HTMLElement) ?? null;
    const host = startEl?.closest("[data-chapter]") ?? ancEl?.closest("[data-chapter]");
    // 选区必须完整落在某个章节容器内（滚动模式各 section / 分页 page-host），
    // 且该容器必须是我们绑定的正文容器——目录项也带 data-chapter，需排除
    if (
      !host ||
      !host.contains(range.startContainer) ||
      !host.contains(range.endContainer) ||
      !this.containers.some((c) => c === host || c.contains(host))
    ) {
      this.hideSelPopup();
      return;
    }
    const text = range.toString();
    if (!text.trim()) {
      this.hideSelPopup();
      return;
    }
    this.pending = {
      range: range.cloneRange(),
      text,
      chapter: Number((host as HTMLElement).dataset.chapter),
      container: host as HTMLElement,
    };
    this.pendingNoteId = null;
    // 工具条出现在鼠标松开的位置（上下文菜单式）；不预置选中态，
    // 颜色/样式在用户点击时直接落地到选区
    this.selPopup.querySelectorAll<HTMLElement>(".sp-color, .sp-style").forEach((b) => b.classList.remove("active"));
    if (pos) {
      this.showAtPoint(this.selPopup, pos.x, pos.y);
    } else {
      // 键盘选区等无鼠标坐标时兜底：退回选区起点矩形
      const rect = range.getClientRects()[0] ?? range.getBoundingClientRect();
      this.showAt(this.selPopup, rect);
    }
  }

  private showAt(popup: HTMLElement, rect: DOMRect): void {
    popup.classList.remove("hidden");
    const w = popup.offsetWidth;
    const h = popup.offsetHeight;
    const x = Math.max(8, Math.min(rect.left, window.innerWidth - w - 8));
    const y = rect.top - h - 8 > 0 ? rect.top - h - 8 : rect.bottom + 8;
    popup.style.left = `${x}px`;
    popup.style.top = `${y}px`;
  }

  /** 光标锚点显示（右键菜单式：左上角贴光标，溢出时夹回视口内） */
  private showAtPoint(popup: HTMLElement, x: number, y: number): void {
    popup.classList.remove("hidden");
    const w = popup.offsetWidth;
    const h = popup.offsetHeight;
    const px = Math.max(8, Math.min(x + 4, window.innerWidth - w - 8));
    const py = Math.max(8, Math.min(y + 4, window.innerHeight - h - 8));
    popup.style.left = `${px}px`;
    popup.style.top = `${py}px`;
  }

  private hideSelPopup(): void {
    this.selPopup.classList.add("hidden");
    this.pending = null;
    this.pendingNoteId = null;
  }

  hidePopups(): void {
    this.closeItemMenu();
    this.selPopup.classList.add("hidden");
    this.noteEditor.classList.add("hidden");
    this.pending = null;
    this.pendingNoteId = null;
    this.editingId = null;
    this.editorAnchorRect = null;
  }

  /** 按章节号找当前已渲染的章节容器（滚动模式 section / 分页 page-host） */
  private chapterContainer(chapterIndex: number): HTMLElement | null {
    for (const c of this.containers) {
      if (c.dataset.chapter != null) {
        if (Number(c.dataset.chapter) === chapterIndex) return c;
        continue;
      }
      const sec = c.querySelector<HTMLElement>(`section[data-chapter="${chapterIndex}"]`);
      if (sec) return sec;
    }
    return null;
  }

  /** 选区若恰好落在某条已有划线上则返回该笔记（点击 = 改样式的前提判断） */
  private noteUnderPending(): Note | null {
    const pending = this.pending;
    if (!pending) return null;
    const marks = Array.from(pending.container.querySelectorAll<HTMLElement>("mark.note-mark")).filter((m) =>
      pending.range.intersectsNode(m),
    );
    if (marks.length === 0) return null;
    const id = marks[0].dataset.noteId;
    if (!id || !marks.every((m) => m.dataset.noteId === id)) return null;
    return this.notes.find((n) => n.id === id) ?? null;
  }

  /** 把颜色/样式应用到待处理选区：已有划线 → 改样式；否则新建划线 */
  private applyToPending(color: NoteColor, style: NoteStyle, opts?: { keepOpen?: boolean }): Note | null {
    if (!this.pending) return null;
    // DOM 包裹/重包后 pending.range 会失效，优先用本回合已落地的笔记 id 续改
    let note: Note | null = this.noteUnderPending() ?? this.findNoteById(this.pendingNoteId) ?? null;
    if (note) {
      this.restyleNote(note, { color, style });
    } else {
      note = this.createFromPending(color, style);
    }
    if (note) this.pendingNoteId = note.id;
    // 无论是否保持工具栏，都清掉系统蓝色选区，只留下划线颜色
    window.getSelection()?.removeAllRanges();
    if (!opts?.keepOpen) {
      this.hideSelPopup();
      return note;
    }
    this.selPopup.querySelectorAll<HTMLElement>(".sp-color").forEach((x) =>
      x.classList.toggle("active", x.dataset.color === color),
    );
    this.selPopup.querySelectorAll<HTMLElement>(".sp-style").forEach((x) =>
      x.classList.toggle("active", x.dataset.style === style),
    );
    return note;
  }

  /** 从待处理选区创建划线 */
  private createFromPending(color: NoteColor, style: NoteStyle): Note | null {
    const pending = this.pending;
    if (!pending || !this.bookId) return null;
    const index = buildTextIndex(pending.container);
    const { range } = pending;
    const start = globalOffset(index, range.startContainer, range.startOffset);
    const end = globalOffset(index, range.endContainer, range.endOffset);
    if (start < 0 || end <= start) return null;
    const note: Note = {
      id: crypto.randomUUID(),
      bookId: this.bookId,
      chapterIndex: pending.chapter,
      quote: index.text.slice(start, end),
      prefix: index.text.slice(Math.max(0, start - CONTEXT_LEN), start),
      suffix: index.text.slice(end, end + CONTEXT_LEN),
      color,
      style,
      note: null,
      createdAt: Math.floor(Date.now() / 1000),
    };
    if (wrapOffsetRange(index, start, end, note.id, note.color, note.style) === 0) return null;
    this.notes.push(note);
    this.persist();
    this.renderPanel();
    return note;
  }

  // ─────────────────── 划线点击 → 想法编辑器 ───────────────────

  private onContentClick(e: MouseEvent): void {
    const mark = (e.target as HTMLElement).closest?.("mark.note-mark") as HTMLElement | null;
    if (!mark) return;
    const noteId = mark.dataset.noteId;
    const note = this.notes.find((n) => n.id === noteId);
    if (!note) return;
    window.getSelection()?.removeAllRanges();
    // 划线可能跨页被拆成多个分片：用点击命中的分片定位，而非元素并集矩形
    this.openEditor(note, clickedFragmentRect(mark, e.clientX, e.clientY));
  }

  /** 打开想法编辑器（新建或修改同一弹窗） */
  private openEditor(note: Note, rect: DOMRect): void {
    this.hideSelPopup();
    this.editingId = note.id;
    this.editorAnchorRect = rect;
    const quote = this.noteEditor.querySelector("#ne-quote")!;
    const text = this.noteEditor.querySelector("#ne-text") as HTMLTextAreaElement;
    quote.textContent = note.quote;
    text.value = note.note ?? "";
    this.noteEditor.querySelectorAll<HTMLElement>(".sp-color").forEach((b) => b.classList.toggle("active", b.dataset.color === note.color));
    this.noteEditor.querySelectorAll<HTMLElement>(".sp-style").forEach((b) => b.classList.toggle("active", b.dataset.style === note.style));
    this.showAt(this.noteEditor, rect);
    text.focus();
  }

  private findNoteById(id: string | null): Note | undefined {
    return id ? this.notes.find((n) => n.id === id) : undefined;
  }

  /** 修改已有划线的颜色/样式：摘除旧 mark 后按指纹重新定位包裹 */
  private restyleNote(note: Note, patch: { color?: NoteColor; style?: NoteStyle }): void {
    Object.assign(note, patch);
    const container = this.chapterContainer(note.chapterIndex);
    if (!container) {
      this.persist();
      this.renderPanel();
      return;
    }
    unwrapNote(container, note.id);
    const index = buildTextIndex(container);
    const pos = locateQuote(index, note.quote, note.prefix, note.suffix);
    if (pos) wrapOffsetRange(index, pos[0], pos[1], note.id, note.color, note.style);
    this.persist();
    this.renderPanel();
  }

  private deleteNote(note: Note): void {
    this.notes = this.notes.filter((n) => n.id !== note.id);
    // 只在正文容器里摘除（目录项也带 data-chapter，不能全局查）
    for (const c of this.containers) unwrapNote(c, note.id);
    this.persist();
    this.renderPanel();
    this.cb.onChanged();
  }

  // ─────────────────── UI 事件绑定 ───────────────────

  private bindUi(): void {
    // 工具条按钮按下不抢选区
    this.selPopup.addEventListener("mousedown", (e) => e.preventDefault());

    // 选区工具条：点颜色立即按该色落地划线并保持工具栏打开（可连续换色预览）；
    // 点笔型落地样式并收起。已有划线换色时保留原笔型
    this.selPopup.querySelectorAll<HTMLElement>(".sp-color").forEach((b) =>
      b.addEventListener("click", () => {
        this.lastColor = b.dataset.color as NoteColor;
        const existing = this.noteUnderPending() ?? this.findNoteById(this.pendingNoteId);
        this.applyToPending(this.lastColor, existing?.style ?? this.lastStyle, { keepOpen: true });
      }),
    );
    this.selPopup.querySelectorAll<HTMLElement>(".sp-style").forEach((b) =>
      b.addEventListener("click", () => {
        this.lastStyle = b.dataset.style as NoteStyle;
        const existing = this.noteUnderPending() ?? this.findNoteById(this.pendingNoteId);
        this.applyToPending(existing?.color ?? this.lastColor, this.lastStyle);
      }),
    );
    this.selPopup.querySelector("#sp-note")?.addEventListener("click", () => {
      const rect = this.selPopup.getBoundingClientRect();
      const note = this.noteUnderPending() ?? this.findNoteById(this.pendingNoteId) ?? this.createFromPending(this.lastColor, this.lastStyle);
      if (note) {
        window.getSelection()?.removeAllRanges();
        this.hideSelPopup();
        this.openEditor(note, rect);
      }
    });
    this.selPopup.querySelector("#sp-copy")?.addEventListener("click", () => {
      if (this.pending?.text) void navigator.clipboard.writeText(this.pending.text);
      this.hideSelPopup();
    });

    // 编辑器：颜色/样式实时生效，保存/删除/取消
    // 注意：改样式后必须用打开时缓存的划线矩形重新定位；
    // 若传编辑器自身矩形，showAt 会把弹窗逐次上移（BUG：工具框换位置）
    this.noteEditor.querySelectorAll<HTMLElement>(".sp-color").forEach((b) =>
      b.addEventListener("click", () => {
        const note = this.findNoteById(this.editingId);
        if (note) {
          this.restyleNote(note, { color: b.dataset.color as NoteColor });
          this.openEditor(note, this.editorAnchorRect ?? this.noteEditor.getBoundingClientRect());
        }
      }),
    );
    this.noteEditor.querySelectorAll<HTMLElement>(".sp-style").forEach((b) =>
      b.addEventListener("click", () => {
        const note = this.findNoteById(this.editingId);
        if (note) {
          this.restyleNote(note, { style: b.dataset.style as NoteStyle });
          this.openEditor(note, this.editorAnchorRect ?? this.noteEditor.getBoundingClientRect());
        }
      }),
    );
    this.noteEditor.querySelector("#ne-save")?.addEventListener("click", () => this.saveEditor());
    this.noteEditor.querySelector("#ne-cancel")?.addEventListener("click", () => this.closeEditor());
    this.noteEditor.querySelector("#ne-delete")?.addEventListener("click", () => {
      const note = this.findNoteById(this.editingId);
      if (note) this.deleteNote(note);
      this.closeEditor();
    });

    // 点弹窗外 / Esc 关闭（#np-menu 有自己的 menuCloser，且 mousedown 时移除自身
    // 会让 pointerup 落空、click 无法派发 → 菜单项"点击没反应"，故排除）
    document.addEventListener("mousedown", (e) => {
      const t = e.target as HTMLElement;
      if (!t.closest("#sel-popup") && !t.closest("#note-editor") && !t.closest("#np-menu")) this.hidePopups();
    });
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape") this.hidePopups();
    });

    // 面板 tab 切换
    this.panel.querySelectorAll<HTMLElement>(".np-tab").forEach((b) =>
      b.addEventListener("click", () => {
        this.panelTab = (b.dataset.nptab as "bookmarks" | "notes") ?? "bookmarks";
        this.panel.querySelectorAll<HTMLElement>(".np-tab").forEach((x) => x.classList.toggle("active", x === b));
        this.renderPanel();
      }),
    );
  }

  private saveEditor(): void {
    const note = this.findNoteById(this.editingId);
    if (note) {
      const text = this.noteEditor.querySelector("#ne-text") as HTMLTextAreaElement;
      note.note = text.value.trim() || null;
      this.persist();
      this.renderPanel();
    }
    this.closeEditor();
  }

  private closeEditor(): void {
    // 取消保存时保留划线本身，只关弹窗
    this.noteEditor.classList.add("hidden");
    this.editingId = null;
    this.editorAnchorRect = null;
  }

  // ─────────────────── 书签 ───────────────────

  private samePosition(b: Bookmark, pos: PositionInfo): boolean {
    if (b.chapterIndex !== pos.chapterIndex) return false;
    // 指纹书签：章内文本坐标（quote+prefix 精确匹配）
    if (b.quote != null || pos.quote != null) {
      return b.quote != null && pos.quote != null && b.quote === pos.quote && b.prefix === pos.prefix;
    }
    // 兜底书签（旧数据 / 无文本）：anchor → pageInChapter → 同章
    if (b.anchor != null || pos.anchor != null) return b.anchor === pos.anchor;
    if (b.pageInChapter != null || pos.pageInChapter != null) return b.pageInChapter === pos.pageInChapter;
    return true;
  }

  hasBookmarkAt(pos: PositionInfo): boolean {
    return this.bookmarks.some((b) => this.samePosition(b, pos));
  }

  /** 切换当前位置书签；返回 true = 现在已加书签 */
  toggleBookmarkAt(pos: PositionInfo): boolean {
    if (!this.bookId) return false;
    const existing = this.bookmarks.find((b) => this.samePosition(b, pos));
    if (existing) {
      this.bookmarks = this.bookmarks.filter((b) => b.id !== existing.id);
      this.persist();
      this.renderPanel();
      this.cb.onChanged();
      return false;
    }
    this.bookmarks.push({
      id: crypto.randomUUID(),
      bookId: this.bookId,
      chapterIndex: pos.chapterIndex,
      anchor: pos.anchor,
      pageInChapter: pos.pageInChapter,
      quote: pos.quote,
      prefix: pos.prefix,
      suffix: pos.suffix,
      excerpt: pos.excerpt,
      createdAt: Math.floor(Date.now() / 1000),
    });
    this.persist();
    this.renderPanel();
    this.cb.onChanged();
    return true;
  }

  /** 面板中删除书签 */
  private deleteBookmark(bm: Bookmark): void {
    this.bookmarks = this.bookmarks.filter((b) => b.id !== bm.id);
    this.persist();
    this.renderPanel();
    this.cb.onChanged();
  }

  // ─────────────────── 面板条目右键菜单（复用书架 .ctx-item 样式） ───────────────────

  /** 文档级关闭监听（openItemMenu 注册，closeItemMenu 移除） */
  private menuCloser: ((e: MouseEvent) => void) | null = null;
  /** 当前打开的面板菜单元素（引用管理；独立 id #np-menu，勿与书架 #ctx-menu 共享——
   *  曾经共享 id + getElementById 删除，书架菜单点击时被本类 mousedown 监听误删，
   *  导致"右键菜单点击没反应"，2026-09-07 修复） */
  private itemMenu: HTMLElement | null = null;

  private openItemMenu(x: number, y: number, items: { label: string; danger?: boolean; onClick: () => void }[]): void {
    this.closeItemMenu();
    const menu = document.createElement("div");
    menu.id = "np-menu";
    for (const it of items) {
      const item = document.createElement("div");
      item.className = "ctx-item" + (it.danger ? " danger" : "");
      item.textContent = it.label;
      item.addEventListener("click", (e) => {
        e.stopPropagation();
        this.closeItemMenu();
        it.onClick();
      });
      menu.appendChild(item);
    }
    document.body.appendChild(menu);
    // 防止溢出屏幕
    const rect = menu.getBoundingClientRect();
    menu.style.left = `${Math.min(x, window.innerWidth - rect.width - 8)}px`;
    menu.style.top = `${Math.min(y, window.innerHeight - rect.height - 8)}px`;
    this.itemMenu = menu;
    this.menuCloser = (e) => {
      if (!(e.target as HTMLElement).closest("#np-menu")) this.closeItemMenu();
    };
    document.addEventListener("mousedown", this.menuCloser, true);
  }

  private closeItemMenu(): void {
    this.itemMenu?.remove();
    this.itemMenu = null;
    if (this.menuCloser) {
      document.removeEventListener("mousedown", this.menuCloser, true);
      this.menuCloser = null;
    }
  }

  // ─────────────────── 侧栏面板 ───────────────────

  /** 打开面板并定位到指定 tab（顶栏「查看书签 / 查看笔记」按钮用） */
  openPanel(tab: "bookmarks" | "notes"): void {
    this.panelTab = tab;
    this.panel.classList.remove("hidden");
    this.panel.querySelectorAll<HTMLElement>(".np-tab").forEach((x) =>
      x.classList.toggle("active", x.dataset.nptab === tab),
    );
    this.renderPanel();
  }

  renderPanel(): void {
    const list = this.npList;
    list.innerHTML = "";
    if (this.panelTab === "bookmarks") {
      const items = [...this.bookmarks].sort((a, b) => a.chapterIndex - b.chapterIndex || a.createdAt - b.createdAt);
      if (items.length === 0) {
        list.innerHTML = `<div class="np-empty">暂无书签<br /><span style="font-size:12px">点击顶栏「添加书签」、正文右键或 Ctrl+D</span></div>`;
        return;
      }
      for (const bm of items) list.appendChild(this.bookmarkItem(bm));
    } else {
      const items = [...this.notes].sort((a, b) => a.chapterIndex - b.chapterIndex || a.createdAt - b.createdAt);
      if (items.length === 0) {
        list.innerHTML = `<div class="np-empty">暂无笔记<br /><span style="font-size:12px">选中正文文字即可划线（荧光笔 / 横线 / 波浪线）并写想法</span></div>`;
        return;
      }
      for (const note of items) list.appendChild(this.noteItem(note));
    }
  }

  private itemShell(onClick: () => void, onDelete: () => void, onContextMenu?: (e: MouseEvent) => void): HTMLElement {
    const item = document.createElement("div");
    item.className = "np-item";
    item.addEventListener("click", onClick);
    if (onContextMenu) item.addEventListener("contextmenu", onContextMenu);
    const del = document.createElement("button");
    del.className = "np-del";
    del.title = "删除";
    del.textContent = "×";
    del.addEventListener("click", (e) => {
      e.stopPropagation();
      onDelete();
    });
    item.appendChild(del);
    return item;
  }

  private bookmarkItem(bm: Bookmark): HTMLElement {
    const item = this.itemShell(
      () => this.cb.jumpToBookmark(bm),
      () => this.deleteBookmark(bm),
      (e) => {
        e.preventDefault();
        e.stopPropagation();
        this.openItemMenu(e.clientX, e.clientY, [
          { label: "删除书签", danger: true, onClick: () => this.deleteBookmark(bm) },
        ]);
      },
    );
    const label = bm.excerpt || `第 ${bm.chapterIndex + 1} 章`;
    item.innerHTML = `<div class="np-quote">🔖 ${escapeText(label)}</div>`;
    const meta = document.createElement("div");
    meta.className = "np-meta";
    meta.textContent = `${this.cb.chapterTitle(bm.chapterIndex)} · ${formatTime(bm.createdAt)}`;
    item.appendChild(meta);
    return item;
  }

  private noteItem(note: Note): HTMLElement {
    const item = this.itemShell(
      () => this.cb.jumpToNote(note),
      () => this.deleteNote(note),
    );
    const dot = `<span class="np-dot hl-${note.color}"></span>`;
    const quote = document.createElement("div");
    quote.className = "np-quote";
    quote.innerHTML = `${dot}${escapeText(truncate(note.quote, 60))}`;
    item.appendChild(quote);
    if (note.note) {
      const body = document.createElement("div");
      body.className = "np-note";
      body.textContent = note.note;
      item.appendChild(body);
    }
    const meta = document.createElement("div");
    meta.className = "np-meta";
    meta.textContent = this.cb.chapterTitle(note.chapterIndex);
    item.appendChild(meta);
    return item;
  }
}

function escapeText(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function truncate(s: string, n: number): string {
  return s.length > n ? `${s.slice(0, n)}…` : s;
}

/** unix 秒 → "2026-09-03 14:05"（书签创建时间展示用） */
function formatTime(unixSeconds: number): string {
  const d = new Date(unixSeconds * 1000);
  if (Number.isNaN(d.getTime())) return "";
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

// 供面板色点复用的颜色清单（与 CSS .hl-* 对应）
export { NOTE_COLORS, NOTE_STYLES };
