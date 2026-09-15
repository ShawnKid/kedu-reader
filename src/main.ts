/**
 * 阅读器核心 UI 逻辑（书架 main 窗与阅读 reader 窗共用；窗口 label 分流见 bootstrap.ts）。
 *
 * 职责边界：不做任何电子书解析，所有数据经 ipc.ts 的 invoke 调用 Rust 后端。
 * 书架窗：shelf ⇄ stats，点书只请求另开/聚焦阅读窗。
 * 阅读窗：loading → reading ⇄ error，「返回书架」把书架窗置于最上（不关本窗）。
 * PDF 走独立渲染分支（pdf.ts，pdf.js），其他格式走后端解析会话。
 */
import { convertFileSrc } from "@tauri-apps/api/core";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { emit, listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { api, bookFileUrl } from "./ipc";
import { sanitizeHtml } from "./sanitize";
import { Paginator } from "./paginator";
import { PageTurn } from "./page-turn";
import { ScrollCurl } from "./scroll-curl";
import type { PdfSession, PdfZoomMode } from "./pdf";
import { ReadingTimer } from "./readingtime";
import {
  AnnotationManager,
  buildTextIndex,
  collectAnchorPositions,
  computePageFingerprint,
  locateFingerprintOccurrences,
  rangeAtOffset,
  type PositionInfo,
  type TextIndex,
} from "./annotations";
import { FootnoteManager } from "./footnotes";
import {
  SearchPanel,
  buildSearchIndex,
  clearHighlights,
  locateOccurrences,
  rangeAt,
  type JumpableMatch,
} from "./search";
import { ShelfView, displayTitle, toast } from "./shelf";
import { StatsView } from "./stats";
import { processBookStyles } from "./book-css";
import {
  bindTopbar,
  closeTopbarMenu,
  openTopbarMenu,
  relayoutTopbar,
} from "./topbar";
import { FOLLOW_BOOK_FONT } from "./types";
import {
  COLUMN_WIDTH_DEFAULT,
  COLUMN_WIDTH_MIN,
  DEFAULT_SETTINGS,
  SCROLL_WIDTH_DEFAULT,
  SCROLL_WIDTH_MIN,
  SHELF_CARD_SIZE_DEFAULT,
} from "./reader-defaults";
import type { BookMeta, Bookmark, ChapterContent, Note, ReaderSettings, ReadingProgress, TocItem } from "./types";

// ────────────────────────── DOM 工具 ──────────────────────────

const byId = <T extends HTMLElement>(id: string): T => document.getElementById(id) as T;

const el = {
  topbar: byId<HTMLElement>("topbar"),
  layout: byId<HTMLElement>("layout"),
  shelfView: byId<HTMLElement>("shelf-view"),
  statsView: byId<HTMLElement>("stats-view"),
  bookTitle: byId<HTMLElement>("book-title"),
  percent: byId<HTMLElement>("percent"),
  toc: byId<HTMLElement>("toc"),
  tocList: byId<HTMLElement>("toc-list"),
  reader: byId<HTMLElement>("reader"),
  scrollView: byId<HTMLElement>("scroll-view"),
  scrollCurl: byId<HTMLElement>("scroll-curl"),
  pageViewport: byId<HTMLElement>("page-viewport"),
  pageHost: byId<HTMLElement>("page-host"),
  pdfView: byId<HTMLElement>("pdf-view"),
  pdfCanvas: byId<HTMLCanvasElement>("pdf-canvas"),
  pdfPageLabel: byId<HTMLElement>("pdf-page-label"),
  pdfPrev: byId<HTMLButtonElement>("pdf-prev"),
  pdfNext: byId<HTMLButtonElement>("pdf-next"),
  welcome: byId<HTMLElement>("welcome"),
  loading: byId<HTMLElement>("loading"),
  loadingText: byId<HTMLElement>("loading-text"),
  errorBox: byId<HTMLElement>("error-box"),
  errorText: byId<HTMLElement>("error-text"),
  dropOverlay: byId<HTMLElement>("drop-overlay"),
  btnShelf: byId<HTMLButtonElement>("btn-shelf"),
  appBrand: byId<HTMLElement>("app-brand"),
  btnToc: byId<HTMLButtonElement>("btn-toc"),
  btnSearch: byId<HTMLButtonElement>("btn-search"),
  searchPanel: byId<HTMLElement>("search-panel"),
  searchInput: byId<HTMLInputElement>("search-input"),
  searchMeta: byId<HTMLElement>("search-meta"),
  searchResults: byId<HTMLElement>("search-results"),
  btnSearchClose: byId<HTMLButtonElement>("btn-search-close"),
  btnBookmark: byId<HTMLButtonElement>("btn-bookmark"),
  btnViewBookmarks: byId<HTMLButtonElement>("btn-view-bookmarks"),
  btnNotes: byId<HTMLButtonElement>("btn-notes"),
  notesPanel: byId<HTMLElement>("notes-panel"),
  bookmarkFlag: byId<HTMLElement>("bookmark-flag"),
  btnMode: byId<HTMLButtonElement>("btn-mode"),
  btnPdfFit: byId<HTMLButtonElement>("btn-pdf-fit"),
  btnPdfActual: byId<HTMLButtonElement>("btn-pdf-actual"),
  btnSettings: byId<HTMLButtonElement>("btn-settings"),
  btnMore: byId<HTMLButtonElement>("btn-more"),
  progressBar: byId<HTMLElement>("progress-bar"),
  btnWelcomeImport: byId<HTMLButtonElement>("btn-welcome-import"),
  btnWelcomeShelf: byId<HTMLButtonElement>("btn-welcome-shelf"),
  zonePrev: byId<HTMLDivElement>("page-zone-prev"),
  zoneNext: byId<HTMLDivElement>("page-zone-next"),
  btnRetry: byId<HTMLButtonElement>("btn-retry"),
  winMin: byId<HTMLButtonElement>("win-min"),
  winMax: byId<HTMLButtonElement>("win-max"),
  winClose: byId<HTMLButtonElement>("win-close"),
};

/** 当前窗口身份：书架（main）还是独立阅读窗（reader） */
const appWin = getCurrentWindow();
const IS_READER_WINDOW = appWin.label === "reader";

/** 落盘阅读窗内尺寸（逻辑像素，与 WebviewWindowBuilder::inner_size 同单位）。
 *  仅在非最大化时写入，保证「用户最后主动调整的大小」可还原。 */
async function saveReaderWindowSize(): Promise<void> {
  try {
    if (await appWin.isMaximized()) return;
    const phys = await appWin.innerSize();
    const scale = await appWin.scaleFactor();
    if (scale > 0) await api.saveReaderWindowSize(phys.width / scale, phys.height / scale);
  } catch {
    /* 尺寸记忆失败不影响阅读 */
  }
}

// ────────────────────────── 应用状态 ──────────────────────────

/** 正文默认字体栈（与 styles.css #reader 的 --reader-font-family 保持一致）。
 *  含黑体系回退链：微软雅黑缺失时按链回退到 SimHei/黑体/思源黑体等其他黑体 */
const READER_FONT_STACK =
  '"Georgia", "Noto Serif SC", "Microsoft YaHei", "微软雅黑", "SimHei", "黑体", "思源黑体", "Source Han Sans SC", serif';

interface AppState {
  book: BookMeta | null;
  toc: TocItem[];
  settings: ReaderSettings;
  /** 单本正文字体（书架右键设置，来自 shelf.json）：具体字体优先于 settings.fontFamily；
   *  FOLLOW_BOOK_FONT = 跟随图书设定（无视全局强制还原书籍 CSS）；null = 跟随全局 */
  perBookFont: string | null;
  /** 当前章（滚动模式下指视口顶部所在章） */
  currentChapter: number;
  /** 分页模式当前页（章内） */
  currentPage: number;
  /** 滚动模式本章滚动比例 */
  scrollRatio: number;
  busy: boolean;
  /** 当前书是否为 PDF（渲染走 pdf.ts 分支） */
  isPdf: boolean;
  /** PDF 当前页（1-based）与总页数 */
  pdfPage: number;
  pdfPageCount: number;
}

const state: AppState = {
  book: null,
  toc: [],
  settings: { ...DEFAULT_SETTINGS },
  perBookFont: null,
  currentChapter: 0,
  currentPage: 0,
  scrollRatio: 0,
  busy: false,
  isPdf: false,
  pdfPage: 1,
  pdfPageCount: 0,
};

const paginator = new Paginator(el.pageViewport, el.pageHost);
const pageTurn = new PageTurn(el.pageViewport, el.pageHost);
paginator.onChange = reason => {
  if (reason === "layout") pageTurn.layoutChanged();
  else if (!pageTurn.moving) pageTurn.cancel();
};
/** 滚动模式「自然」效果：卷轴卷绕（上卷轴直径 66 / 下卷轴直径 0，见 scroll-curl.ts） */
const scrollCurl = new ScrollCurl(el.scrollCurl, el.scrollView);
const readingTimer = new ReadingTimer();
let activePdf: PdfSession | null = null;

/** 笔记（划线/想法）与书签管理器（需求 6） */
const annotations = new AnnotationManager({
  save: (a) => {
    if (state.book) void api.saveAnnotations(state.book.id, a).catch(console.error);
  },
  chapterTitle: (i) =>
    state.toc.find((t) => t.chapterIndex === i)?.label ??
    (state.isPdf ? `第 ${i + 1} 页` : `第 ${i + 1} 章`),
  jumpToNote: (note: Note) => jumpToNote(note),
  jumpToBookmark: (bm: Bookmark) => jumpToBookmark(bm),
  onChanged: () => refreshBookmarkBtn(),
});
annotations.bindContent([el.scrollView, el.pageHost]);

/**
 * 脚注悬浮预览与跳转（EPUB 3 语义 noteref + 老格式序号锚点，见 footnotes.ts）。
 * 注释体识别时已保证与标记同章节容器，因此这里直接在当前渲染容器内定位。
 */
const footnotes = new FootnoteManager((anchorId) => {
  if (state.settings.readingMode === "paginated") {
    const p = paginator.pageOfAnchor(anchorId);
    if (p != null) {
      pageTurn.cancel(); // 用户直接跳页：打断进行中的翻页动画，避免快照拍到跳页位置
      paginator.goTo(p);
      state.currentPage = p;
      updatePercent();
      saveProgressSoon();
    }
  } else {
    el.scrollView.querySelector(`#${CSS.escape(anchorId)}`)?.scrollIntoView({ block: "start" });
    saveProgressSoon();
  }
});
footnotes.bindContent([el.scrollView, el.pageHost]);

/** 滚动模式：已加载章节的 section 集合（按章号索引） */
const scrollSections = new Map<number, HTMLElement>();

// ────────────────────────── 全文搜索面板 ──────────────────────────

/** 全书全文搜索侧边栏（仅文字书；PDF/CBZ 无搜索按钮与快捷键） */
const searchPanel = new SearchPanel(
  el.searchPanel,
  el.searchInput,
  el.searchMeta,
  el.searchResults,
  el.btnSearchClose,
);
searchPanel.onJump = (match) => void jumpToSearchMatch(match);
searchPanel.onOpen = () => {
  // 面板互斥：搜索侧栏与笔记面板不同开（目录允许共存，分列两侧互不遮挡）
  if (!el.notesPanel.classList.contains("hidden")) {
    show(el.notesPanel, false);
    el.notesPanel.dataset.tab = "";
    syncNotesPanelBtns();
  }
  syncSearchBtn();
  if (state.settings.readingMode === "paginated") paginator.relayout();
};
searchPanel.onClose = () => {
  clearHighlights();
  syncSearchBtn();
  if (state.settings.readingMode === "paginated") paginator.relayout();
};

// 顶栏收纳/溢出菜单：topbar.ts（bindTopbar 在 boot 时注入）

// ────────────────────────── 视图状态切换 ──────────────────────────

function show(el: HTMLElement, on = true): void {
  el.classList.toggle("hidden", !on);
}

type AppView = "shelf" | "reader" | "stats";
let currentView: AppView = "shelf";

/** 各视图根节点（showView 入场动画目标） */
const VIEW_ROOTS: Record<AppView, HTMLElement> = {
  shelf: el.shelfView,
  reader: el.layout,
  stats: el.statsView,
};

function showView(view: AppView): void {
  pageTurn.cancel();
  currentView = view;
  // 标记当前视图，供 CSS 按页面定制顶栏样式（如书架页无分割线设计）
  document.body.dataset.view = view;
  annotations.hidePopups();
  footnotes.hide();
  closePdfZoomMenu();
  closeTopbarMenu();
  show(el.shelfView, view === "shelf");
  show(el.layout, view === "reader");
  show(el.statsView, view === "stats");
  // 入场动效：crossfade + 轻微上浮（重挂动画类，重复切换也能重放）
  const root = VIEW_ROOTS[view];
  root.classList.remove("view-enter");
  void root.offsetWidth;
  root.classList.add("view-enter");
  // 顶栏按钮随视图显隐（目录/模式/设置只在阅读态有效）
  const readerOnly = document.querySelectorAll<HTMLElement>(".reader-only");
  readerOnly.forEach((n) => (n.style.display = view === "reader" ? "" : "none"));
  // PDF 专属按钮 + 对 PDF 无意义按钮的显隐（必须在 forEach 之后，否则被恢复）
  syncPdfButtons();
  // 已在书架页时「书架」按钮无意义，隐藏（统计页仍需它返回）
  el.btnShelf.style.display = view === "shelf" ? "none" : "";
  // 进度数字与进度条只在阅读页顶栏显示（书架/统计页均不显示）
  el.percent.style.display = view === "reader" ? "" : "none";
  el.progressBar.style.opacity = view === "reader" ? "" : "0";
  if (view === "shelf") {
    // 回到主页后标题栏不再显示上一本书名
    el.bookTitle.textContent = "";
    void shelf.refresh(state.settings.defaultCategory);
  } else if (view === "stats") {
    void statsView.refresh();
  }
  relayoutTopbar();
  // 视图切换影响卷绕带启停（仅阅读态 + 滚动模式 + 自然效果）
  syncScrollCurl();
}

/** 书架数据可能已变（直接打开自动入架 / 拖拽导入等），广播给书架窗刷新 */
function notifyShelfChanged(): void {
  void emit("shelf-changed").catch(() => {});
}

/** 请求打开一本书：书架窗另起/聚焦阅读窗；阅读窗本地加载 */
function requestOpenBook(filePath: string): void {
  if (IS_READER_WINDOW) {
    void openBook(filePath);
    return;
  }
  void api.openReaderWindow(filePath).catch((err) => {
    console.error(err);
    toast(String(err));
  });
}

/** 「返回书架」：阅读窗把书架窗还原并置于最上；书架窗切回 shelf 视图 */
function returnToShelf(): void {
  saveProgressNow();
  if (IS_READER_WINDOW) {
    void api.focusShelfWindow().catch(console.error);
    return;
  }
  showView("shelf");
}

/** 书架 / 统计子视图实例（回调注入避免与 main 循环依赖） */
const shelf = new ShelfView(el.shelfView, {
  openBook: (filePath) => requestOpenBook(filePath),
  backToReader: () => {
    // 统计入口在书架侧栏底部，点击切到统计视图
    showView("stats");
  },
  onShelfChanged: () => {},
  onBookFontChanged: (bookId, fontFamily) => {
    // 右键改的就是当前打开的书 → 单本字体即时生效（applySettings 内部重排分页）
    if (state.book?.id === bookId) {
      const prevFollow = isFollowBookCss();
      state.perBookFont = fontFamily;
      applySettings();
      // 跟随图书 ⇄ 统一排版翻转：章节 HTML 维度不同（有无书籍 CSS），需重载
      if (isFollowBookCss() !== prevFollow) void loadChapter(state.currentChapter);
    } else if (!IS_READER_WINDOW) {
      // 书架窗改字体：阅读窗可能正开着这本书，广播过去即时生效
      void emit("book-font-changed", { bookId, fontFamily }).catch(() => {});
    }
  },
  onSortModeChanged: (mode) => {
    // 书架快速排序下拉：落盘 + 广播（设置窗口同步显示）
    state.settings.shelfSortMode = mode;
    void api.updateReaderSettings(state.settings).catch(console.error);
    void emit("settings-changed", state.settings).catch(() => {});
  },
});

const statsView = new StatsView(el.statsView);
statsView.onBack = () => showView("shelf");

function setState(kind: "welcome" | "loading" | "reading" | "error", message?: string): void {
  if (kind !== "reading") pageTurn.cancel();
  show(el.welcome, kind === "welcome");
  show(el.loading, kind === "loading");
  show(el.errorBox, kind === "error");
  const reading = kind === "reading";
  show(el.toc, reading && !state.isPdf && el.toc.dataset.open === "1");
  show(el.scrollView, reading && !state.isPdf && state.settings.readingMode === "scroll");
  show(el.pageViewport, reading && !state.isPdf && state.settings.readingMode === "paginated");
  show(el.pdfView, reading && state.isPdf);
  show(el.zonePrev, reading && !state.isPdf && state.settings.readingMode === "paginated");
  show(el.zoneNext, reading && !state.isPdf && state.settings.readingMode === "paginated");
  syncTocBtn();
  syncPdfButtons();
  if (kind === "error" && message) el.errorText.textContent = message;
  if (kind === "loading" && message) el.loadingText.textContent = message;
  // 阅读状态/PDF 分支变化时同步卷绕带（welcome/loading/error/PDF 均停用）
  syncScrollCurl();
}

/** PDF 顶栏按钮显隐：缩放按钮仅 PDF 阅读态显示；「查看笔记」「滚动/分页」对 PDF 无意义，隐藏。
 *  注意非阅读态时不得恢复 notes/mode（书架/统计页的显隐由 showView 的 reader-only 统一管）。 */
function syncPdfButtons(): void {
  const pdfReading = currentView === "reader" && state.isPdf;
  el.btnPdfFit.style.display = pdfReading ? "" : "none";
  el.btnPdfActual.style.display = pdfReading ? "" : "none";
  // 仅在阅读视图内切换：文本态恢复「查看笔记/滚动」，PDF 态隐藏（书架/统计页归 showView 的 reader-only 管）
  if (currentView === "reader") {
    const display = pdfReading ? "none" : "";
    el.btnNotes.style.display = display;
    el.btnMode.style.display = display;
    // 全文搜索仅文字书可用（PDF 无文本层、CBZ 纯图片）
    el.btnSearch.style.display = pdfReading || !isSearchableBook() ? "none" : "";
  }
  relayoutTopbar();
}

/** 同步缩放模式按钮高亮（当前档「已选择」第三态填充） */
function updatePdfZoomButtons(): void {
  const mode = activePdf?.zoomMode ?? "fit-page";
  el.btnPdfFit.classList.toggle("tb-active", mode === "fit-page");
  el.btnPdfActual.classList.toggle("tb-active", mode === "actual");
}

/** PDF 缩放右键菜单。独立 id #pdf-menu（勿与 #ctx-menu/#np-menu 共享，
 *  否则会被其他模块的 document 级 mousedown 关闭逻辑误删）。 */
function openPdfZoomMenu(x: number, y: number): void {
  closePdfZoomMenu();
  const menu = document.createElement("div");
  menu.id = "pdf-menu";
  const mode = activePdf?.zoomMode ?? "fit-page";
  const items: Array<{ label: string; mode: PdfZoomMode; title: string }> = [
    { label: "适合页面", mode: "fit-page", title: "整页完整显示在窗口内（默认）" },
    { label: "实际大小", mode: "actual", title: "原始尺寸 (100%)" },
    { label: "宽度适应", mode: "fit-width", title: "按窗口宽度铺满" },
  ];
  for (const it of items) {
    const item = document.createElement("div");
    item.className = "ctx-item" + (mode === it.mode ? " active" : "");
    item.textContent = it.label;
    item.title = it.title;
    item.addEventListener("click", (e) => {
      // 阻止冒泡：document 全局监听会在同一次 click 里关闭新菜单
      e.stopPropagation();
      closePdfZoomMenu();
      activePdf?.setZoom(it.mode);
      updatePdfZoomButtons();
    });
    menu.appendChild(item);
  }
  document.body.appendChild(menu);
  // 防止溢出屏幕
  const rect = menu.getBoundingClientRect();
  menu.style.left = `${Math.min(x, window.innerWidth - rect.width - 8)}px`;
  menu.style.top = `${Math.min(y, window.innerHeight - rect.height - 8)}px`;
}

function closePdfZoomMenu(): void {
  document.getElementById("pdf-menu")?.remove();
}

// ────────────────────────── 设置 ──────────────────────────

/** 兼容旧配置：栏宽缺失/为 0 回退默认，并钳制到下限 */
function clampColumnWidth(s: ReaderSettings): ReaderSettings {
  if (s.pageTurnEffect !== "natural") s.pageTurnEffect = "standard";
  if (s.scrollEffect !== "natural") s.scrollEffect = "standard";
  s.columnWidthPx = Math.max(COLUMN_WIDTH_MIN, s.columnWidthPx || COLUMN_WIDTH_DEFAULT);
  s.scrollColumnWidthPx = Math.max(SCROLL_WIDTH_MIN, s.scrollColumnWidthPx || SCROLL_WIDTH_DEFAULT);
  s.shelfCardSize = Math.min(100, Math.max(0, Math.round(Number.isFinite(s.shelfCardSize) ? s.shelfCardSize : SHELF_CARD_SIZE_DEFAULT)));
  // 书架排序方式：非法值回退默认「最近阅读」
  if (!["recent", "added", "title", "progress"].includes(s.shelfSortMode)) s.shelfSortMode = "recent";
  return s;
}

/** 书架卡片缩放：30 = 基准 100%；1~30 线性映射 50%~100%（平滑变小），>30 为 v/30，上限 3 */
function shelfCardScale(v: number): number {
  if (v <= 1) return 0.5;
  if (v <= 30) return 0.5 + ((v - 1) / 29) * 0.5;
  return Math.min(3, v / 30);
}

function applySettings(): void {
  pageTurn.cancel();
  const s = state.settings;
  // 主题切换动画：换肤瞬间挂 html.theme-anim，让全局颜色平滑过渡（详见 styles.css）
  const prevTheme = document.body.dataset.theme;
  document.body.dataset.theme = s.theme;
  if (prevTheme && prevTheme !== s.theme) {
    document.documentElement.classList.add("theme-anim");
    window.setTimeout(() => document.documentElement.classList.remove("theme-anim"), 260);
  }
  el.reader.style.setProperty("--reader-font-size", `${s.fontSizePx}px`);
  el.reader.style.setProperty("--reader-line-height", String(s.lineHeight));
  // 正文字体：单本设置优先级最高，未设置时跟随全局；所选字体在前，默认栈殿后保证缺字回退。
  // 「跟随图书设定」哨兵不是字体名：单本命中哨兵时按未设置处理（字体交给书籍自带 CSS）
  // （字体名过滤双引号防 CSS 值逃逸）
  const perBookFamily = state.perBookFont === FOLLOW_BOOK_FONT ? null : state.perBookFont;
  const effectiveFamily = perBookFamily ?? s.fontFamily;
  el.reader.style.setProperty(
    "--reader-font-family",
    effectiveFamily
      ? `"${effectiveFamily.replaceAll('"', "")}", ${READER_FONT_STACK}`
      : READER_FONT_STACK,
  );
  el.reader.style.setProperty("--reader-margin", `${s.pageMarginPx}px`);
  // 模式按钮：滚动 ⇄ 分页双图标切换
  el.btnMode.querySelector(".ic-scroll")?.classList.toggle("hidden", s.readingMode !== "scroll");
  el.btnMode.querySelector(".ic-page")?.classList.toggle("hidden", s.readingMode === "scroll");

  // 每栏最大栏宽传给分页器（纯目标值，列数自适应）；强制两栏开关同步给分页器
  paginator.setColumnWidth(s.columnWidthPx);
  paginator.setForceTwoColumns(s.twoPageSpread);
  // 滚动模式正文栏宽（.scroll-chapter 经 CSS 变量取值）
  el.reader.style.setProperty("--scroll-column-width", `${s.scrollColumnWidthPx}px`);
  // 书架卡片大小（#shelf-grid 及卡片全部尺寸经 calc 缩放）
  document.documentElement.style.setProperty("--shelf-scale", String(shelfCardScale(s.shelfCardSize)));

  // 主题换肤 + 分页排版依赖字号/边距，需要重排
  if (state.book) paginator.relayout();
  // 字号/行距/字体/边距等都会改变行盒，卷绕克隆与行测量整体失效重建
  scrollCurl.invalidate();
  syncScrollCurl();
}

/** 滚动模式卷绕效果开关：仅文本书 + 滚动模式 + 效果=自然 时接管顶部卷绕 */
function syncScrollCurl(): void {
  scrollCurl.setEnabled(
    currentView === "reader" && !!state.book && !state.isPdf &&
    state.settings.readingMode === "scroll" && state.settings.scrollEffect === "natural",
  );
}

let saveSettingsTimer: number | undefined;
function onSettingsChanged(): void {
  applySettings();
  if (!state.book) return;
  clearTimeout(saveSettingsTimer);
  saveSettingsTimer = window.setTimeout(() => {
    void api.updateReaderSettings(state.settings).catch(console.error);
    saveProgressSoon();
  }, 400);
}

// ────────────────────────── 打开书籍流程 ──────────────────────────

/** 查询某本书在书架中单独设置的正文字体：null = 跟随全局；FOLLOW_BOOK_FONT = 跟随图书设定 */
async function loadPerBookFont(bookId: string): Promise<string | null> {
  try {
    const shelf = await api.getShelf();
    return shelf.books.find((b) => b.id === bookId)?.fontFamily ?? null;
  } catch {
    return null;
  }
}

async function openBook(filePath: string): Promise<void> {
  if (state.busy) return;
  const isPdf = /\.pdf$/i.test(filePath);
  if (isPdf) {
    await openPdfBook(filePath);
    return;
  }
  state.busy = true;
  setState("loading", "正在解析书籍…");
  showView("reader");
  try {
    // 旧书退出：通知后端释放内存 + 停计时
    await releaseCurrentBook();
    scrollSections.clear();
    el.scrollView.innerHTML = "";
    scrollCurl.invalidate(); // 旧书的卷绕克隆与行缓存全部作废
    el.tocList.innerHTML = "";

    const { book, toc } = await api.openBook(filePath);
    state.book = book;
    state.toc = toc;
    state.currentChapter = 0;
    state.currentPage = 0;
    state.scrollRatio = 0;
    state.isPdf = false;
    // 后端 open_book 已 auto_add 入架：通知书架窗刷新，双击关联打开也能立刻看到
    notifyShelfChanged();
    // 单本正文字体（书架右键设置）：按 book.id 从书架查询，null = 跟随全局
    state.perBookFont = await loadPerBookFont(book.id);

    el.bookTitle.textContent = displayTitle(book.title) + (book.author ? ` · ${book.author}` : "");
    renderToc();

    // 笔记/书签数据（需求 6）：需在渲染章节前就绪
    await annotations.loadFor(book.id);
    // 全文搜索绑定新书（同时清空上一本残留的查询与结果）
    searchPanel.setBook(book.id);

    // 恢复上次阅读位置（含按书记忆的设置快照）
    const saved = await api.loadProgress(book.id);
    if (saved) {
      // 皮肤/正文字体/书架默认分类/翻页滚动动画是全局配置，不入按书记忆：快照里的旧值不得覆盖全局当前值
      state.settings = {
        ...state.settings,
        ...saved.settings,
        theme: state.settings.theme,
        fontFamily: state.settings.fontFamily,
        defaultCategory: state.settings.defaultCategory,
        pageTurnEffect: state.settings.pageTurnEffect,
        scrollEffect: state.settings.scrollEffect,
      };
      state.currentChapter = Math.min(saved.progress.chapterIndex, Math.max(0, book.totalChapters - 1));
    }
    applySettings();
    // 同步独立设置窗口（按书快照可能覆盖全局值）
    void emit("settings-changed", state.settings).catch(() => {});
    setState("reading");
    el.toc.dataset.open = "1";
    show(el.toc, true);
    syncTocBtn();
    readingTimer.start(book.id, book.title, () => buildProgress()?.percent ?? null);
    // 全书字数懒计算（旧书首开补算，幂等）：报告「读字数/阅读速度」的分母。
    // fire-and-forget，不阻塞打开流程；PDF 无文本层不适用。
    void api.computeBookCharCount(book.id).catch(() => {});

    await loadChapter(state.currentChapter, { progress: saved?.progress ?? null });
  } catch (err) {
    console.error(err);
    setState("error", String(err));
  } finally {
    state.busy = false;
  }
}

/** PDF 模块懒加载（E04）：非 PDF 阅读路径不解析 pdfjs。Promise 缓存保证只加载一次。 */
let pdfModulePromise: Promise<typeof import("./pdf")> | null = null;
function loadPdfModule(): Promise<typeof import("./pdf")> {
  if (!pdfModulePromise) pdfModulePromise = import("./pdf");
  return pdfModulePromise;
}

/** PDF 分支：后端只出元数据，渲染交给 pdf.js，字节走 book-file:// 协议 */
async function openPdfBook(filePath: string): Promise<void> {
  state.busy = true;
  setState("loading", "正在打开 PDF…");
  showView("reader");
  try {
    await releaseCurrentBook();
    const book = await api.openPdf(filePath);
    state.book = book;
    state.toc = [];
    state.isPdf = true;
    state.currentChapter = 0;
    // PDF 后端 ensure_in_shelf 已登记：同样通知书架窗刷新
    notifyShelfChanged();
    // PDF 无文本层，字体设置不适用
    state.perBookFont = null;
    el.bookTitle.textContent = displayTitle(book.title);
    renderToc();
    // PDF 无笔记/书签面板入口（顶栏按钮已隐藏），上一本书残留的面板直接关闭
    if (!el.notesPanel.classList.contains("hidden")) {
      show(el.notesPanel, false);
      el.notesPanel.dataset.tab = "";
      syncNotesPanelBtns();
    }
    await annotations.loadFor(book.id);

    // 动态导入 pdf.js；若期间已切走目标书则放弃后续初始化
    const pdfMod = await loadPdfModule();
    if (state.book?.id !== book.id) return;

    const saved = await api.loadProgress(book.id);
    if (saved) {
      // 皮肤/正文字体/书架默认分类/翻页滚动动画是全局配置，不入按书记忆（同 openBook）
      state.settings = {
        ...state.settings,
        ...saved.settings,
        theme: state.settings.theme,
        fontFamily: state.settings.fontFamily,
        defaultCategory: state.settings.defaultCategory,
        pageTurnEffect: state.settings.pageTurnEffect,
        scrollEffect: state.settings.scrollEffect,
      };
    }
    applySettings();
    // 同步独立设置窗口（按书快照可能覆盖全局值）
    void emit("settings-changed", state.settings).catch(() => {});

    activePdf = new pdfMod.PdfSession(el.pdfView, el.pdfCanvas);
    // 新会话默认宽度适应：同步缩放按钮高亮
    updatePdfZoomButtons();
    const { pageCount } = await activePdf.load(bookFileUrl(book.id));
    state.pdfPageCount = pageCount;
    // PDF 页数回填（报告「最厚的一本」用）：只有 pdf.js 知道页数，拿到即落盘
    void api.setBookPageCount(book.id, pageCount).catch(() => {});
    // 进度约定：chapterIndex = 当前页 - 1（与其他格式共用 ReadingProgress）
    state.pdfPage = Math.min(Math.max(1, (saved?.progress.chapterIndex ?? 0) + 1), pageCount);
    activePdf.onPageRendered = (page, total) => {
      state.pdfPage = page;
      state.pdfPageCount = total;
      el.pdfPageLabel.textContent = `${page} / ${total}`;
      const percent = total > 0 ? (page / total) * 100 : 0;
      el.percent.textContent = `${percent.toFixed(1)}%`;
      el.progressBar.style.width = `${Math.min(100, Math.max(0, percent))}%`;
      refreshBookmarkBtn();
      saveProgressSoon();
    };
    el.pdfPrev.onclick = () => void renderPdfPage(state.pdfPage - 1);
    el.pdfNext.onclick = () => void renderPdfPage(state.pdfPage + 1);

    setState("reading");
    readingTimer.start(book.id, book.title, () => buildProgress()?.percent ?? null);
    await renderPdfPage(state.pdfPage);
  } catch (err) {
    console.error(err);
    setState("error", String(err));
  } finally {
    state.busy = false;
  }
}

async function renderPdfPage(page: number): Promise<void> {
  // 不检查 state.busy：打开流程中（busy=true）就要渲染首页，
  // 且 PdfSession.render 自带「渲染中合并请求」的串行化，重入安全。
  if (!activePdf) return;
  try {
    await activePdf.render(page);
  } catch (err) {
    console.error("PDF 页渲染失败", err);
  }
}

/** 退出当前书：停计时、销毁 PDF 会话、清空笔记/书签内存态、通知后端释放解析会话 */
async function releaseCurrentBook(): Promise<void> {
  readingTimer.stop();
  pageTurn.cancel();
  pageTurn.clearResourceCache();
  if (activePdf) {
    activePdf.destroy();
    activePdf = null;
  }
  annotations.reset();
  // 全文搜索：清结果与高亮并正常收起面板（走 close() 同步顶栏按钮态，
  // 保证再开书时搜索默认关闭、图标不残留高亮）
  searchPanel.reset();
  searchPanel.close();
  if (state.book) {
    void api.closeBook(state.book.id).catch(() => {});
  }
  state.book = null;
  state.isPdf = false;
}

async function pickAndOpen(): Promise<void> {
  const path = await openFileDialog({
    multiple: false,
    filters: [
      {
        name: "电子书",
        extensions: ["epub", "mobi", "prc", "azw", "azw3", "kf8", "pdf", "fb2", "fbz", "cbz", "txt", "log", "md", "markdown"],
      },
    ],
  });
  if (typeof path === "string") requestOpenBook(path);
}

/** 拖入文件加入书架（不直接打开）：逐个导入，最后切到书架展示结果。 */
async function addDroppedToShelf(paths: string[]): Promise<void> {
  if (paths.length === 0) return;
  const failed: string[] = [];
  for (const p of paths) {
    try {
      await api.addBookToShelf(p);
    } catch (err) {
      console.error(err);
      failed.push(p);
    }
  }
  if (!IS_READER_WINDOW) showView("shelf");
  else if (failed.length < paths.length) notifyShelfChanged();
  if (failed.length > 0) {
    const names = failed.map((p) => p.split(/[\\/]/).pop()).join("\n");
    toast(`以下文件无法识别，未能加入书架：\n${names}`);
  }
}

// ────────────────────────── 章节加载与渲染 ──────────────────────────

interface LoadOptions {
  anchor?: string | null;
  page?: number | null;
  progress?: ReadingProgress | null;
  /** 翻到上一章末尾（分页模式向前进） */
  toEnd?: boolean;
  /** 渲染完成后定位到这条笔记的划线（面板跳转用） */
  locateNoteId?: string;
  /** 渲染完成后按指纹定位书签（面板跳转用） */
  fingerprint?: Bookmark;
}

async function loadChapter(index: number, opts: LoadOptions = {}): Promise<void> {
  const book = state.book;
  if (!book || index < 0 || index >= book.totalChapters) return;
  state.busy = true;
  // 章节热加载不切加载层：setState("loading") 会隐藏目录与正文（闪白），
  // 且分页 layout() 在 display:none 下测不到尺寸，锚点定位退化为第 0 页
  // （目录小章节跳到大章节第一页的根因）。仅无可见内容时才显示加载层。
  const readerVisible =
    !el.toc.classList.contains("hidden") ||
    !el.pageViewport.classList.contains("hidden") ||
    !el.scrollView.classList.contains("hidden");
  if (readerVisible) {
    // 保证目标渲染容器可见（模式切换场景）：layout 依赖真实视口尺寸
    if (state.settings.readingMode === "paginated") {
      show(el.pageViewport, true);
      show(el.scrollView, false);
    } else {
      show(el.scrollView, true);
      show(el.pageViewport, false);
    }
  } else {
    setState("loading", "正在加载章节…");
  }
  try {
    const content = await api.getChapterContent(book.id, index, isFollowBookCss());
    state.currentChapter = index;
    state.currentPage = 0;
    state.scrollRatio = 0;

    if (state.settings.readingMode === "paginated") {
      renderPaginated(content, opts);
      // 章节渲染后重新包裹本章划线（需求 6）；data-chapter 供选区定位章节
      el.pageHost.dataset.chapter = String(index);
      annotations.applyToChapter(index, el.pageHost);
      if (opts.locateNoteId) locateNotePaginated(opts.locateNoteId);
    } else {
      await renderScrollFromScratch(index, content, opts);
      // 滚动模式：清掉分页容器的章节标记，避免章节容器查询误配
      delete el.pageHost.dataset.chapter;
      if (opts.locateNoteId) locateNoteScroll(opts.locateNoteId);
    }
    // 搜索命中：章节 DOM 重建后恢复本章高亮（无搜索结果时为 no-op）
    if (isSearchableBook()) {
      const sroot =
        state.settings.readingMode === "paginated" ? el.pageHost : scrollSections.get(index);
      if (sroot) searchPanel.onChapterRendered(sroot);
    }
    if (opts.fingerprint?.quote) locateBookmarkFingerprint(opts.fingerprint);
    updatePercent();
    setState("reading");
    saveProgressSoon();
  } catch (err) {
    setState("error", String(err));
  } finally {
    state.busy = false;
  }
}

/** 分页模式：定位到划线 mark 所在页 */
function locateNotePaginated(noteId: string): void {
  const mark = el.pageHost.querySelector(`mark.note-mark[data-note-id="${CSS.escape(noteId)}"]`);
  if (!mark) return;
  const page = paginator.pageOfElement(mark);
  if (page != null) {
    paginator.goTo(page);
    state.currentPage = page;
  }
}

/** 滚动模式：滚动到划线 mark 居中 */
function locateNoteScroll(noteId: string): void {
  const mark = el.scrollView.querySelector(`mark.note-mark[data-note-id="${CSS.escape(noteId)}"]`);
  mark?.scrollIntoView({ block: "center" });
}

// ────────────────────────── 书签指纹定位（隐形笔记） ──────────────────────────

/** Range 首个可见行盒所在页（指纹消歧 / 跳转用） */
function firstVisibleRectPage(range: Range): number | null {
  for (const r of range.getClientRects()) {
    if (r.width > 0.5 && r.height > 0.5) return paginator.rectPage(r);
  }
  return null;
}

/** 多命中消歧：选「句号所在页」与已存页码最近者；无页提示取首个 */
function pickFingerprintOccurrence(index: TextIndex, bm: Bookmark): number | null {
  const occ = locateFingerprintOccurrences(index, bm.quote!, bm.prefix);
  if (occ.length === 0) return null;
  if (occ.length === 1 || bm.pageInChapter == null) return occ[0];
  let best = occ[0];
  let bestDist = Infinity;
  for (const p of occ) {
    const range = rangeAtOffset(index, p, p + 1);
    const page = range ? firstVisibleRectPage(range) : null;
    const d = page == null ? Infinity : Math.abs(page - bm.pageInChapter);
    if (d < bestDist) {
      bestDist = d;
      best = p;
    }
  }
  return best;
}

/** 书签指纹跳转：在刚渲染好的章内定位指纹，对齐到句号处 */
function locateBookmarkFingerprint(bm: Bookmark): void {
  if (!bm.quote) return;
  if (state.settings.readingMode === "paginated") {
    const index = buildTextIndex(el.pageHost);
    const occ = pickFingerprintOccurrence(index, bm);
    if (occ == null) return;
    const range = rangeAtOffset(index, occ, occ + 1);
    if (!range) return;
    const page = firstVisibleRectPage(range);
    if (page != null) {
      paginator.goTo(page);
      state.currentPage = paginator.currentPage;
    }
    return;
  }
  // 滚动模式：loadChapter 后仅渲染目标一章，在其 section 内定位，
  // 等一帧布局稳定后把句号对齐到视口顶部下方 80px
  const section = scrollSections.get(bm.chapterIndex);
  if (!section) return;
  const index = buildTextIndex(section);
  const occ = locateFingerprintOccurrences(index, bm.quote, bm.prefix)[0];
  if (occ == null) return;
  const range = rangeAtOffset(index, occ, occ + 1);
  if (!range) return;
  requestAnimationFrame(() => {
    const rect = range.getBoundingClientRect();
    if (rect.height > 0 || rect.width > 0) el.scrollView.scrollTop += rect.top - 80;
  });
}

/** 面板 → 笔记跳转 */
function jumpToNote(note: Note): void {
  if (!state.book || note.chapterIndex >= state.book.totalChapters) return;
  annotations.hidePopups();
  void loadChapter(note.chapterIndex, { locateNoteId: note.id });
}

/** 面板 → 书签跳转（PDF 按 chapterIndex=页号-1 约定） */
function jumpToBookmark(bm: Bookmark): void {
  if (!state.book) return;
  annotations.hidePopups();
  if (state.isPdf) {
    void renderPdfPage(bm.chapterIndex + 1);
    return;
  }
  // 指纹书签：章内文本坐标定位；旧数据/兜底书签：anchor 优先、页码兜底
  if (bm.quote) {
    void loadChapter(bm.chapterIndex, { fingerprint: bm });
    return;
  }
  void loadChapter(bm.chapterIndex, { anchor: bm.anchor, page: bm.pageInChapter });
}

/** 「跟随图书设定」判定（三态）：
 *  - 单本字体 = FOLLOW_BOOK_FONT 哨兵 → 强制还原书籍自带 CSS（无视全局选择）；
 *  - 单本字体为具体字体 → 统一排版；
 *  - 单本未设置（null）→ 跟随全局：全局也未选具体字体时还原书籍 CSS，否则统一排版 */
function isFollowBookCss(): boolean {
  if (state.perBookFont === FOLLOW_BOOK_FONT) return true;
  if (state.perBookFont) return false;
  return !state.settings.fontFamily;
}

/** 章节内容 → 统一转成 HTML（html/text/images 三种 kind） */
function contentToHtml(content: ChapterContent): string {
  switch (content.kind) {
    case "html":
      // 后端 ammonia 已清洗；前端 DOM 白名单再过滤一层（防御纵深）
      // 「跟随图书设定」时保留 class/style 属性与 <style data-book-css> 注入块
      return sanitizeHtml(content.html ?? "", { preserveStyles: isFollowBookCss() });
    case "text":
      return (content.text ?? "")
        .split(/\n+/)
        .filter((p) => p.trim().length > 0)
        .map((p) => `<p>${escapeHtml(p.trim())}</p>`)
        .join("\n");
    case "images":
      // CBZ：先渲染占位，图片随后异步逐张加载
      return (content.imageRefs ?? [])
        .map((_, i) => `<div class="cbz-page" data-img-index="${i}"><span class="cbz-loading">加载中…</span></div>`)
        .join("\n");
  }
}

function escapeHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

/**
 * reader-res 协议 URL（Q1/Q3）：图片资源不再逐张走 IPC base64。
 * convertFileSrc 按平台生成 `http://reader-res.localhost/...`（Windows）
 * 或 `reader-res://localhost/...`，由 Rust 端注册的自定义协议直出原始字节。
 */
function resourceUrl(ref: string): string {
  // 资源路径是 zip 内部路径：逐段 encodeURIComponent（保留 /），协议端会 percent-decode
  const encoded = ref.replace(/\\/g, "/").split("/").map(encodeURIComponent).join("/");
  return convertFileSrc(encoded, "reader-res");
}

/** CBZ 图片加载：直接以协议 URL 作为 img src，webview 原生并行请求 + 缓存 */
function loadCbzImages(container: HTMLElement, content: ChapterContent): void {
  const pages = container.querySelectorAll<HTMLElement>(".cbz-page");
  (content.imageRefs ?? []).forEach((ref, i) => {
    const page = pages[i];
    if (!page) return;
    page.innerHTML = "";
    const img = document.createElement("img");
    img.src = resourceUrl(ref);
    img.alt = `第 ${i + 1} 页`;
    img.loading = "lazy"; // 漫画长列表：视口外的页不抢先加载
    img.addEventListener("error", () => {
      page.innerHTML = `<span class="cbz-loading">第 ${i + 1} 页加载失败</span>`;
    });
    page.appendChild(img);
  });
}

function renderPaginated(content: ChapterContent, opts: LoadOptions): void {
  // 裸文本脚注包裹放在布局前：上标标记影响行盒，测量后再改会错页
  // 「跟随图书设定」：样式作用域化必须在 paginator 测量前落地，避免分页错位
  paginator.setContent(processBookStyles(contentToHtml(content)), (host) => footnotes.enhance(host));
  if (content.kind === "images") loadCbzImages(el.pageHost, content);

  // Q5 恢复优先级：锚点 > 页码 > 章末 > 章首。
  // 锚点定位与「字号/窗口变化导致页数变化」无关，是对齐 Rust 端持久化进度的首选；
  // 页码（pageInChapter）仅作 anchor 缺失/失效时的兜底，并钳制到当前总页数内。
  const savedPage = opts.page ?? opts.progress?.pageInChapter ?? null;
  const anchor = opts.anchor ?? opts.progress?.anchor ?? null;
  if (anchor) {
    const p = paginator.pageOfAnchor(anchor);
    if (p != null) paginator.goTo(p);
    else if (savedPage != null) paginator.goTo(savedPage);
  } else if (savedPage != null) {
    paginator.goTo(savedPage);
  } else if (opts.toEnd) {
    paginator.goTo(paginator.totalPages - 1);
  }
  state.currentPage = paginator.currentPage;
}

/** 滚动模式从指定章开始渲染（清空后加载第一章，后续章节由滚动事件续载） */
async function renderScrollFromScratch(index: number, content: ChapterContent, opts: LoadOptions): Promise<void> {
  scrollSections.clear();
  el.scrollView.innerHTML = "";
  el.scrollView.scrollTop = 0;
  const section = appendScrollSection(index, content);
  if (content.kind === "images") loadCbzImages(section, content);

  if (opts.progress?.scrollRatio) {
    // 等一帧布局稳定后按比例恢复
    requestAnimationFrame(() => {
      const max = el.scrollView.scrollHeight - el.scrollView.clientHeight;
      el.scrollView.scrollTop = max * opts.progress!.scrollRatio;
    });
  } else if (opts.anchor) {
    try {
      section.querySelector(`#${CSS.escape(opts.anchor)}`)?.scrollIntoView();
    } catch {
      /* 非法锚点忽略 */
    }
  }
  // 章节重载后布局全新：卷绕克隆与行缓存作废，下一帧按新 DOM 重建
  scrollCurl.invalidate();
}

/** 向滚动流追加一章 */
function appendScrollSection(index: number, content: ChapterContent): HTMLElement {
  const section = document.createElement("section");
  section.className = "scroll-chapter";
  section.dataset.chapter = String(index);
  section.innerHTML = `<h2 class="chapter-title">${escapeHtml(content.title)}</h2>${processBookStyles(contentToHtml(content))}`;
  el.scrollView.appendChild(section);
  scrollSections.set(index, section);
  // 渲染后包裹本章划线（需求 6）
  annotations.applyToChapter(index, section);
  // 裸文本脚注配对（无标记转换本：[N] 文本 ↔ 注释段落）
  footnotes.enhance(section);
  return section;
}

// ────────────────────────── 翻页（分页模式） ──────────────────────────

async function turnNext(): Promise<void> {
  await animateTurn(1, turnNextCore);
}

async function turnPrev(): Promise<void> {
  await animateTurn(-1, turnPrevCore);
}

async function animateTurn(direction: 1 | -1, move: () => Promise<void>): Promise<void> {
  if (!state.book || (state.busy && !pageTurn.active)) return;
  const bookId = state.book.id;
  const totalChapters = state.book.totalChapters;
  const atEnd = () => direction === 1
    ? paginator.currentPage === paginator.totalPages - 1 && state.currentChapter === totalChapters - 1
    : paginator.currentPage === 0 && state.currentChapter === 0;
  if (state.isPdf || state.settings.readingMode !== "paginated" ||
      state.settings.pageTurnEffect === "standard") {
    await move();
    return;
  }
  footnotes.hide();
  await pageTurn.play(direction, move, () => state.book?.id === bookId && !state.busy && !atEnd());
}

async function turnNextCore(): Promise<void> {
  if (state.busy || !state.book) return;
  footnotes.hide();
  if (state.isPdf) {
    await renderPdfPage(state.pdfPage + 1);
    return;
  }
  if (state.settings.readingMode === "scroll") {
    // 滚动模式：跳下一章
    if (state.currentChapter + 1 < state.book.totalChapters) await loadChapter(state.currentChapter + 1);
    return;
  }
  if (paginator.nextPage()) {
    state.currentPage = paginator.currentPage;
    updatePercent();
    saveProgressSoon();
  } else if (state.currentChapter + 1 < state.book.totalChapters) {
    await loadChapter(state.currentChapter + 1); // 章末 → 下一章
  }
}

async function turnPrevCore(): Promise<void> {
  if (state.busy || !state.book) return;
  footnotes.hide();
  if (state.isPdf) {
    await renderPdfPage(state.pdfPage - 1);
    return;
  }
  if (state.settings.readingMode === "scroll") {
    if (state.currentChapter > 0) await loadChapter(state.currentChapter - 1, { toEnd: true });
    return;
  }
  if (paginator.prevPage()) {
    state.currentPage = paginator.currentPage;
    updatePercent();
    saveProgressSoon();
  } else if (state.currentChapter > 0) {
    await loadChapter(state.currentChapter - 1, { toEnd: true }); // 章首 → 上一章末页
  }
}

// ────────────────── 悬停进度数字的快速翻页（累积页数 + 批量跳转） ──────────────────

const WHEEL_NOTCH = 100; // 一格滚轮的像素增量（常见浏览器 deltaY≈100/格）
const SEEK_TICK_MS = 50; // 快速翻页的消费间隔
const PAGE_WHEEL_COOLDOWN = 250; // 正文区翻页后的惯性吸收冷却

let seekPending = 0; // 待消费页数（整数，正=向后）
let seekCarry = 0; // 像素→页换算的小数累积（不足一格先记着）
let seekTimer: number | null = null;

function stopSeekTimer(): void {
  if (seekTimer !== null) {
    window.clearInterval(seekTimer);
    seekTimer = null;
  }
}

async function seekConsume(): Promise<void> {
  if (!state.book || currentView !== "reader" || seekPending === 0) {
    if (seekPending === 0) stopSeekTimer();
    return;
  }
  if (state.busy) return; // 章节加载中，等下一 tick
  const want = seekPending;
  if (state.isPdf) {
    const target = Math.min(Math.max(state.pdfPage + want, 1), state.pdfPageCount);
    seekPending -= target - state.pdfPage;
    if (seekPending === 0) stopSeekTimer();
    await renderPdfPage(target);
    return;
  }
  if (state.settings.readingMode === "scroll") {
    // 滚动模式没有「页」概念：一格滚轮 = 一章
    const target = Math.min(Math.max(state.currentChapter + want, 0), state.book.totalChapters - 1);
    seekPending -= target - state.currentChapter;
    if (seekPending === 0) stopSeekTimer();
    if (target !== state.currentChapter) await loadChapter(target);
    return;
  }
  // 分页模式：章内一次 transform 跳 N 页（无动画），剩余页数跨章消费
  pageTurn.cancel(); // 用户直接跳页：打断进行中的翻页动画，避免快照拍到跳页位置
  const moved = paginator.jumpBy(want);
  seekPending -= moved;
  if (moved !== 0) {
    state.currentPage = paginator.currentPage;
    updatePercent();
    saveProgressSoon();
  }
  if (seekPending !== 0) {
    const next = state.currentChapter + Math.sign(seekPending);
    if (next < 0 || next >= state.book.totalChapters) {
      seekPending = 0; // 已到书首/书尾，丢弃剩余
      stopSeekTimer();
    } else {
      // 跳入相邻章（向后进章首、向前进章末），剩余页数下一 tick 继续
      await loadChapter(next, { toEnd: seekPending < 0 });
    }
  }
  if (seekPending === 0) stopSeekTimer();
}

// ────────────────────────── 滚动模式事件 ──────────────────────────

let scrollTick = false;
function onScroll(): void {
  if (scrollTick || state.settings.readingMode !== "scroll" || !state.book) return;
  scrollTick = true;
  requestAnimationFrame(() => {
    scrollTick = false;
    const sv = el.scrollView;
    const max = sv.scrollHeight - sv.clientHeight;
    if (max > 0) state.scrollRatio = Math.min(1, sv.scrollTop / max);

    // 视口顶部所在章 = 当前章（用于目录高亮与进度保存）
    let current = state.currentChapter;
    for (const [idx, section] of scrollSections) {
      if (section.offsetTop <= sv.scrollTop + sv.clientHeight * 0.3) current = idx;
    }
    if (current !== state.currentChapter) {
      state.currentChapter = current;
      updateTocHighlight();
      updatePercent();
      saveProgressSoon();
    } else {
      // 同章内滚动（Markdown 全文单章等）：锚点级目录高亮跟随
      updateTocHighlight();
      // 书签按钮按指纹判定，需等滚动停稳再扫描（防抖，见 scheduleBookmarkRefresh）
      scheduleBookmarkRefresh();
    }

    // 接近底部：追加下一章（无限滚动；向前回翻用上一章按钮）
    const book = state.book;
    if (book && state.scrollRatio > 0.9 && !state.busy) {
      const nextIdx = Math.max(...scrollSections.keys()) + 1;
      if (nextIdx < book.totalChapters && !scrollSections.has(nextIdx)) {
        state.busy = true;
        void api
          .getChapterContent(book.id, nextIdx, isFollowBookCss())
          .then((c) => {
            const section = appendScrollSection(nextIdx, c);
            if (c.kind === "images") loadCbzImages(section, c);
          })
          .catch(console.error)
          .finally(() => (state.busy = false));
      }
    }
    saveProgressSoon();
  });
}

// ────────────────────────── 进度持久化 ──────────────────────────

let saveTimer: number | undefined;
function saveProgressSoon(): void {
  clearTimeout(saveTimer);
  saveTimer = window.setTimeout(() => saveProgressNow(), 500);
}

function buildProgress(): ReadingProgress | null {
  const book = state.book;
  if (!book) return null;
  if (state.isPdf) {
    // PDF：chapterIndex 存「当前页 - 1」，percent 按页号折算
    return {
      bookId: book.id,
      chapterIndex: Math.max(0, state.pdfPage - 1),
      anchor: null,
      scrollRatio: 0,
      pageInChapter: null,
      percent: state.pdfPageCount > 0 ? (state.pdfPage / state.pdfPageCount) * 100 : 0,
      updatedAt: Math.floor(Date.now() / 1000),
    };
  }
  const paginated = state.settings.readingMode === "paginated";
  // Q5：分页模式保存「当前页内第一个带 id 元素」作为锚点（本页无 id 时为
  // null）。页码会随字号/窗口尺寸漂移，锚点才是与 Rust 端持久化对齐的稳定
  // 坐标；锚点缺失（如 TXT 无 id、页内无 id 元素）时退化为纯页码。
  const anchor = paginated ? paginator.anchorOfPage(state.currentPage) : null;
  return {
    bookId: book.id,
    chapterIndex: state.currentChapter,
    anchor,
    scrollRatio: state.scrollRatio,
    pageInChapter: paginated ? state.currentPage : null,
    percent: computePercent(),
    updatedAt: Math.floor(Date.now() / 1000),
  };
}

function saveProgressNow(): void {
  const progress = buildProgress();
  if (!progress) return;
  void api.saveProgress(progress, state.settings).catch(console.error);
}

function computePercent(): number {
  const total = state.book?.totalChapters ?? 1;
  if (state.settings.readingMode === "paginated") {
    // 分页按「当前页末尾」折算（读到哪页 = 读完该页内容）：
    // 旧公式 (currentPage/totalPages) 按页首折算，最后一页永远差 1/N——
    // 章节少的短书误差肉眼可见（末章仅 1 页时停在 (N-1)/N，如 13/14 = 92.9%），
    // 且永远跨不过 99.5 的读完线。与滚动模式语义对齐：
    // 滚动到章底 scrollRatio = 1 同样给整章记满。
    const pages = Math.max(1, paginator.totalPages);
    const frac = Math.min(1, (state.currentPage + 1) / pages);
    return Math.min(100, ((state.currentChapter + frac) / total) * 100);
  }
  return Math.min(100, ((state.currentChapter + state.scrollRatio) / total) * 100);
}

function updatePercent(): void {
  const percent = computePercent();
  el.percent.textContent = `${percent.toFixed(1)}%`;
  // 顶栏细进度条（同源数据，视觉反馈）
  el.progressBar.style.width = `${Math.min(100, Math.max(0, percent))}%`;
  refreshBookmarkBtn();
  updateTocHighlight();
}

// ────────────────────────── 书签（需求 6，隐形笔记式指纹定位） ──────────────────────────

/** 分页：单个矩形是否渲染在当前页 */
function rectOnPage(r: DOMRect): boolean {
  return paginator.rectPage(r) === state.currentPage;
}

/** 分页：线性偏移处的字符是否渲染在当前页（1 字符 Range 精确验证） */
function charOnPage(index: TextIndex, offset: number): boolean {
  const range = rangeAtOffset(index, offset, offset + 1);
  if (!range) return false;
  for (const r of range.getClientRects()) {
    if (r.width > 0.5 && r.height > 0.5 && rectOnPage(r)) return true;
  }
  return false;
}

/** 滚动：线性偏移处的字符行盒是否与视口相交 */
function charInViewport(index: TextIndex, offset: number, viewportH: number): boolean {
  const range = rangeAtOffset(index, offset, offset + 1);
  if (!range) return false;
  for (const r of range.getClientRects()) {
    if (r.height > 0.5 && r.bottom > 0 && r.top < viewportH) return true;
  }
  return false;
}

/** 当前阅读位置（书签判定/新增用） */
function currentBookmarkPos(): PositionInfo | null {
  const book = state.book;
  if (!book) return null;
  if (state.isPdf) {
    // PDF 无文本层 → 兜底定位（同页即同位，chapterIndex = 页号 - 1）
    return { chapterIndex: Math.max(0, state.pdfPage - 1), anchor: null, pageInChapter: null, quote: null, prefix: null, suffix: null, excerpt: `第 ${state.pdfPage} 页` };
  }
  const chapterTitle = state.toc.find((t) => t.chapterIndex === state.currentChapter)?.label ?? `第 ${state.currentChapter + 1} 章`;
  if (state.settings.readingMode === "paginated") {
    // CBZ 占位文本（「加载中…」）含标点，不能作为指纹锚点 → 整体走兜底
    const isCbz = el.pageHost.querySelector(".cbz-page") != null;
    const index = buildTextIndex(el.pageHost);
    const fp = isCbz
      ? null
      : computePageFingerprint(
          collectAnchorPositions(index, (off) => charOnPage(index, off), rectOnPage),
          index.text,
        );
    return {
      chapterIndex: state.currentChapter,
      anchor: null,
      pageInChapter: state.currentPage,
      quote: fp?.quote ?? null,
      prefix: fp?.prefix ?? null,
      suffix: fp?.suffix ?? null,
      excerpt: fp?.excerpt || chapterTitle,
    };
  }
  // 滚动模式：视口内最后一个通过验证的标点为锚点（所属 section 定章号）。
  // 章边界视口例外：指纹/摘要以「锚点所在章」为界，取该章内可见标点。
  const viewportH = el.scrollView.clientHeight;
  let best: { positions: number[]; text: string; chapter: number } | null = null;
  for (const [idx, section] of scrollSections) {
    const sr = section.getBoundingClientRect();
    if (sr.bottom < 0 || sr.top > viewportH) continue; // 整章不在视口，快速剔除
    if (section.querySelector(".cbz-page")) continue; // CBZ 占位文本不作指纹锚点
    const index = buildTextIndex(section);
    const positions = collectAnchorPositions(
      index,
      (off) => charInViewport(index, off, viewportH),
      (r) => r.bottom > 0 && r.top < viewportH,
    );
    if (positions.length > 0) best = { positions, text: index.text, chapter: idx };
  }
  const fp = best ? computePageFingerprint(best.positions, best.text) : null;
  return {
    chapterIndex: best?.chapter ?? state.currentChapter,
    anchor: null,
    pageInChapter: null,
    quote: fp?.quote ?? null,
    prefix: fp?.prefix ?? null,
    suffix: fp?.suffix ?? null,
    excerpt: fp?.excerpt || chapterTitle,
  };
}

function refreshBookmarkBtn(): void {
  const pos = currentBookmarkPos();
  const marked = pos !== null && annotations.hasBookmarkAt(pos);
  // 图标化按钮：只同步 title 与高亮态，不改 textContent（会抹掉 SVG）
  el.btnBookmark.title = marked ? "移除当前页书签（右键正文同效）" : "添加当前页书签（右键正文同效）";
  el.btnBookmark.classList.toggle("marked", marked);
  // 右上角书签旗标（当前位置有书签时显示）
  show(el.bookmarkFlag, marked);
  // 按钮视觉宽度不再变化，无需触发顶栏重排
}

let bookmarkRefreshTimer: number | null = null;
/** 滚动模式书签按钮防抖刷新：指纹扫描需遍历视口内 section，滚动停稳后再做 */
function scheduleBookmarkRefresh(): void {
  if (bookmarkRefreshTimer !== null) window.clearTimeout(bookmarkRefreshTimer);
  bookmarkRefreshTimer = window.setTimeout(() => {
    bookmarkRefreshTimer = null;
    if (state.book && currentView === "reader" && state.settings.readingMode === "scroll") refreshBookmarkBtn();
  }, 200);
}

function toggleBookmark(): void {
  const pos = currentBookmarkPos();
  if (!pos) return;
  annotations.toggleBookmarkAt(pos);
  refreshBookmarkBtn();
}

/** 顶栏「已打开」第三态同步：目录按钮跟随目录面板显隐（tb-active 由 CSS 定义） */
function syncTocBtn(): void {
  el.btnToc.classList.toggle("tb-active", !el.toc.classList.contains("hidden"));
}

/** 查看书签/笔记面板；再次点击同 tab 收起 */
function toggleNotesPanel(tab: "bookmarks" | "notes"): void {
  const open = !el.notesPanel.classList.contains("hidden");
  if (open && el.notesPanel.dataset.tab === tab) {
    show(el.notesPanel, false);
    el.notesPanel.dataset.tab = "";
    syncNotesPanelBtns();
    if (state.settings.readingMode === "paginated") paginator.relayout();
    return;
  }
  // 与搜索面板互斥：打开/切换书签/笔记面板时收起搜索侧栏
  if (searchPanel.isOpen) searchPanel.close();
  annotations.openPanel(tab);
  el.notesPanel.dataset.tab = tab;
  syncNotesPanelBtns();
  if (state.settings.readingMode === "paginated") paginator.relayout();
}

/** 顶栏「已打开」第三态同步：查看书签/查看笔记按钮跟随面板当前 tab */
function syncNotesPanelBtns(): void {
  const tab = el.notesPanel.classList.contains("hidden") ? "" : (el.notesPanel.dataset.tab ?? "");
  el.btnViewBookmarks.classList.toggle("tb-active", tab === "bookmarks");
  el.btnNotes.classList.toggle("tb-active", tab === "notes");
}

// ────────────────────────── 全文搜索 ──────────────────────────

/** 当前打开的书是否支持全文搜索（PDF 无文本层、CBZ 纯图片，均无搜索入口） */
function isSearchableBook(): boolean {
  return !!state.book && !state.isPdf && state.book.format !== "cbz";
}

/** 顶栏「已打开」第三态同步：搜索按钮跟随面板显隐 */
function syncSearchBtn(): void {
  el.btnSearch.classList.toggle("tb-active", searchPanel.isOpen);
}

/** 跳转到搜索命中：跨章先加载章节，再按 quote+prefix 指纹定位页码/滚动位置 */
async function jumpToSearchMatch(match: JumpableMatch): Promise<void> {
  if (!state.book || state.busy || !isSearchableBook()) return;
  if (match.chapterIndex !== state.currentChapter) {
    await loadChapter(match.chapterIndex);
    // 加载失败或被并发打断（章节未切到目标）则放弃本次跳转
    if (state.currentChapter !== match.chapterIndex || state.busy) return;
  }
  const root =
    state.settings.readingMode === "paginated" ? el.pageHost : scrollSections.get(match.chapterIndex);
  if (!root) return;
  // 激活命中可能已变化，先重建本章高亮（含激活项强调色）
  searchPanel.onChapterRendered(root);
  const index = buildSearchIndex(root);
  const offs = locateOccurrences(index, match.quote, match.prefix);
  const pos = offs[match.occurrenceInChapter] ?? offs[0];
  if (pos == null) return;
  const range = rangeAt(index, pos, pos + match.quote.length);
  if (!range) return;
  if (state.settings.readingMode === "paginated") {
    const page = firstVisibleRectPage(range);
    if (page != null) {
      pageTurn.cancel(); // 用户直接跳页：打断进行中的翻页动画，避免快照拍到跳页位置
      paginator.goTo(page);
      state.currentPage = paginator.currentPage;
      updatePercent();
      saveProgressSoon();
    }
  } else {
    // 滚动模式：命中所在文本滚动到视口居中
    (range.startContainer.parentElement ?? el.scrollView).scrollIntoView({ block: "center" });
    saveProgressSoon();
  }
}

// ────────────────────────── 目录侧边栏 ──────────────────────────

function renderToc(): void {
  el.tocList.innerHTML = "";
  for (const item of state.toc) el.tocList.appendChild(renderTocItem(item, 0));
}

function renderTocItem(item: TocItem, depth: number): HTMLElement {
  const node = document.createElement("div");
  node.className = "toc-item";
  node.dataset.chapter = String(item.chapterIndex);
  if (item.anchor) node.dataset.anchor = item.anchor;
  node.style.paddingLeft = `${12 + depth * 16}px`;
  node.textContent = item.label;
  node.addEventListener("click", (e) => {
    // 子项嵌套在父项 DOM 内：必须阻止冒泡，否则点击小章节会先跳小章节、
    // 再冒泡触发父章节的 loadChapter（闪一下又跳回大章节第一页的根因）
    e.stopPropagation();
    // 同章内跳转（EPUB 小节常与父章共用同一 spine 章号）：直接定位锚点，
    // 不重载章节——重载既闪白，也会因重新排版丢失锚点页
    if (state.book && !state.busy && item.chapterIndex === state.currentChapter && item.anchor) {
      if (state.settings.readingMode === "paginated") {
        const p = paginator.pageOfAnchor(item.anchor);
        if (p != null) {
          pageTurn.cancel(); // 用户直接跳页：打断进行中的翻页动画，避免快照拍到跳页位置
          paginator.goTo(p);
          state.currentPage = p;
          updatePercent();
          saveProgressSoon();
          return;
        }
      } else {
        const target = el.scrollView.querySelector(`#${CSS.escape(item.anchor)}`);
        if (target) {
          target.scrollIntoView({ block: "start" });
          saveProgressSoon();
          return;
        }
      }
    }
    void loadChapter(item.chapterIndex, { anchor: item.anchor });
  });
  for (const child of item.children) node.appendChild(renderTocItem(child, depth + 1));
  return node;
}

function updateTocHighlight(): void {
  const items = Array.from(el.tocList.querySelectorAll<HTMLElement>(".toc-item"));
  // 同一章对应多个目录项（Markdown 全文单章、EPUB 章内小节）时，仅按章号
  // 匹配会把整组全部点亮；此时只点亮当前位置对应的锚点项
  const currentAnchor = currentTocAnchor();
  const anchorMatched = currentAnchor != null && items.some((n) => n.dataset.anchor === currentAnchor);
  for (const n of items) {
    let active = Number(n.dataset.chapter) === state.currentChapter;
    if (active && anchorMatched) active = n.dataset.anchor === currentAnchor;
    n.classList.toggle("active", active);
  }
}

/** 当前位置对应的目录锚点：分页取当前页所属小节（页尾前最后一个锚点）；
 *  滚动取视口顶上方最后一个锚点，两模式语义一致 */
function currentTocAnchor(): string | null {
  if (state.isPdf) return null;
  if (state.settings.readingMode === "paginated") {
    return paginator.lastAnchorUpTo(state.currentPage);
  }
  const section = scrollSections.get(state.currentChapter);
  if (!section) return null;
  const viewTop = el.scrollView.getBoundingClientRect().top;
  let best: string | null = null;
  section.querySelectorAll<HTMLElement>("[id]").forEach((node) => {
    if (node.getBoundingClientRect().top - viewTop <= 80) best = node.id;
  });
  return best;
}

// ────────────────────────── 事件绑定 ──────────────────────────

function bindEvents(): void {
  // ── 自制标题栏：窗口控制 ──
  const appWindow = appWin;
  el.winMin.addEventListener("click", () => void appWindow.minimize());
  el.winMax.addEventListener("click", () => void appWindow.toggleMaximize());
  el.winClose.addEventListener("click", () => void appWindow.close());
  // 最大化/还原图标切换（拖拽区的双击最大化也走同一状态）+ 顶栏收纳重算
  const syncMaxIcon = async () => {
    const maximized = await appWindow.isMaximized();
    el.winMax.querySelector(".ic-max")?.classList.toggle("hidden", maximized);
    el.winMax.querySelector(".ic-restore")?.classList.toggle("hidden", !maximized);
    // 圆角窗口：最大化铺满直角，还原恢复圆角
    document.body.classList.toggle("win-maximized", maximized);
  };
  void syncMaxIcon();
  // 阅读窗尺寸记忆：用户拖拽边缘后的非最大化内尺寸（防抖落盘）
  let sizeSaveTimer: number | undefined;
  appWindow.onResized(() => {
    void syncMaxIcon();
    relayoutTopbar();
    if (!IS_READER_WINDOW) return;
    window.clearTimeout(sizeSaveTimer);
    sizeSaveTimer = window.setTimeout(() => {
      void saveReaderWindowSize();
    }, 400);
  });

  el.btnShelf.addEventListener("click", () => returnToShelf());
  // 品牌字「苛读」：按住移动超过阈值才启动窗口拖拽，原地双击回主页。
  // 不能 mousedown 立即 startDragging——系统模态拖拽循环会吞掉后续 mouseup，
  // 双击事件永远无法成立；位移阈值方案让两种手势互不干扰。
  let brandDown: { x: number; y: number } | null = null;
  const BRAND_DRAG_THRESHOLD = 4;
  el.appBrand.addEventListener("mousedown", (e) => {
    brandDown = e.button === 0 ? { x: e.clientX, y: e.clientY } : null;
  });
  window.addEventListener("mousemove", (e) => {
    if (!brandDown) return;
    const dx = e.clientX - brandDown.x;
    const dy = e.clientY - brandDown.y;
    if (dx * dx + dy * dy > BRAND_DRAG_THRESHOLD * BRAND_DRAG_THRESHOLD) {
      brandDown = null;
      void appWindow.startDragging();
    }
  });
  window.addEventListener("mouseup", () => {
    brandDown = null;
  });
  el.appBrand.addEventListener("dblclick", () => {
    brandDown = null;
    returnToShelf();
  });
  el.btnRetry.addEventListener("click", () => void pickAndOpen());

  el.btnToc.addEventListener("click", () => {
    const open = el.toc.classList.contains("hidden");
    show(el.toc, open);
    el.toc.dataset.open = open ? "1" : "0";
    syncTocBtn();
    paginator.relayout();
  });

  // 书签切换（需求 6）：添加/移除当前页书签
  el.btnBookmark.addEventListener("click", () => toggleBookmark());
  // 查看书签 / 查看笔记面板（需求 6 拆分）
  el.btnViewBookmarks.addEventListener("click", () => toggleNotesPanel("bookmarks"));
  el.btnNotes.addEventListener("click", () => toggleNotesPanel("notes"));
  // 全文搜索侧边栏（仅文字书，按钮显隐由 syncPdfButtons 按格式裁剪）
  el.btnSearch.addEventListener("click", () => searchPanel.toggle());
  // 右键正文弹出菜单：PDF → 缩放切换；文本格式 → 书签 / 本书字体 / 统计收录 / 设置分类
  el.reader.addEventListener("contextmenu", (e) => {
    if (!state.book || state.busy || currentView !== "reader") return;
    e.preventDefault();
    if (state.isPdf) {
      // PDF 无文本层：书签/笔记/字体菜单无意义，弹缩放切换
      closePdfZoomMenu();
      openPdfZoomMenu(e.clientX, e.clientY);
      return;
    }
    const pos = currentBookmarkPos();
    const marked = pos !== null && annotations.hasBookmarkAt(pos);
    void shelf.openReaderMenu(e.clientX, e.clientY, state.book.id, {
      label: marked ? "移除书签" : "添加书签",
      onClick: () => toggleBookmark(),
    });
  });

  el.btnPdfFit.addEventListener("click", () => {
    activePdf?.setZoom("fit-page");
    updatePdfZoomButtons();
  });
  el.btnPdfActual.addEventListener("click", () => {
    activePdf?.setZoom("actual");
    updatePdfZoomButtons();
  });
  // PDF 缩放菜单关闭：点外 / Esc（与 #np-menu 同模式，捕获阶段先于菜单 click）
  document.addEventListener(
    "mousedown",
    (e) => {
      if (!(e.target as HTMLElement).closest("#pdf-menu")) closePdfZoomMenu();
    },
    true,
  );
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") closePdfZoomMenu();
  });

  el.btnMode.addEventListener("click", () => {
    state.settings.readingMode = state.settings.readingMode === "paginated" ? "scroll" : "paginated";
    onSettingsChanged();
    // 广播给独立设置窗口，保持其控件显示同步
    void emit("settings-changed", state.settings).catch(console.error);
    if (state.book) void loadChapter(state.currentChapter);
  });

  // 设置：打开独立设置窗口（单例，重复点击仅聚焦）
  el.btnSettings.addEventListener("click", () => void api.openSettings());
  // 整条边缘热区点击翻页（内部按钮 pointer-events:none，事件统一走热区）
  el.zonePrev.addEventListener("click", () => void turnPrev());
  el.zoneNext.addEventListener("click", () => void turnNext());

  // 顶栏溢出菜单：「···」点击展开；点外/关闭菜单
  el.btnMore.addEventListener("click", (e) => {
    e.stopPropagation();
    if (document.getElementById("tb-menu")) closeTopbarMenu();
    else openTopbarMenu();
  });
  document.addEventListener(
    "mousedown",
    (e) => {
      const menu = document.getElementById("tb-menu");
      if (menu && !menu.contains(e.target as Node) && !(e.target as HTMLElement).closest("#btn-more")) {
        closeTopbarMenu();
      }
    },
    true,
  );
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") closeTopbarMenu();
  });

  // 欢迎页行动按钮
  el.btnWelcomeImport.addEventListener("click", () => void pickAndOpen());
  el.btnWelcomeShelf.addEventListener("click", () => returnToShelf());

  // 更换封面窗口改封面后：主书架窗刷新缩略图
  void listen<{ bookId: string }>("cover-changed", (e) => {
    if (IS_READER_WINDOW) return;
    const id = e.payload?.bookId;
    if (typeof id === "string" && id) shelf.refreshBookCover(id);
  });

  // 直接打开书籍（含系统关联双击）后端已入架：书架窗立即重拉，避免必须切视图才可见
  void listen("shelf-changed", () => {
    if (IS_READER_WINDOW) return;
    void shelf.refresh(state.settings.defaultCategory);
  });

  // 独立设置窗口（或本窗顶栏）改设置后实时应用；阅读模式/跟随图书变化需重载当前章
  void listen<ReaderSettings>("settings-changed", (e) => {
    const prevMode = state.settings.readingMode;
    const prevSort = state.settings.shelfSortMode;
    const prevFollow = isFollowBookCss();
    state.settings = clampColumnWidth({ ...e.payload });
    applySettings();
    // 书架排序方式变化：书架视图（含隐藏态）立即按新顺序重绘
    if (state.settings.shelfSortMode !== prevSort) shelf.setSortMode(state.settings.shelfSortMode);
    if (
      state.book &&
      (state.settings.readingMode !== prevMode || isFollowBookCss() !== prevFollow)
    ) {
      void loadChapter(state.currentChapter);
    }
  });

  // 阅读窗：书架窗点开另一本书 → 本窗切书并前置
  void listen<string>("reader-open-book", (e) => {
    if (!IS_READER_WINDOW) return;
    const path = e.payload;
    if (typeof path === "string" && path) void openBook(path);
  });

  // 阅读窗：书架窗改了当前书的单本字体
  void listen<{ bookId: string; fontFamily: string | null }>("book-font-changed", (e) => {
    if (!IS_READER_WINDOW || state.book?.id !== e.payload.bookId) return;
    const prevFollow = isFollowBookCss();
    state.perBookFont = e.payload.fontFamily;
    applySettings();
    if (isFollowBookCss() !== prevFollow) void loadChapter(state.currentChapter);
  });

  // 键盘快捷键翻页
  window.addEventListener("keydown", (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "o") {
      e.preventDefault();
      void pickAndOpen();
      return;
    }
    // Ctrl+D：添加/移除当前页书签（需阻止 WebView2 默认行为）
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "d") {
      if (state.book && currentView === "reader" && !state.busy) {
        e.preventDefault();
        toggleBookmark();
      }
      return;
    }
    // Ctrl+F：全书全文搜索（仅文字书；需阻止 WebView2 默认查找行为）
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "f") {
      if (state.book && currentView === "reader" && !state.busy && isSearchableBook()) {
        e.preventDefault();
        searchPanel.open();
      }
      return;
    }
    // ESC：分层退出——先关弹出菜单/搜索/目录/笔记面板，均未打开则保存进度返回书架
    if (e.key === "Escape" && state.book && currentView === "reader") {
      // 弹出菜单由各自的 document 级 ESC 监听负责关闭，此处让行避免同时回书架
      if (document.getElementById("tb-menu") || document.getElementById("pdf-menu")) return;
      // 搜索面板：输入框内的 ESC 已由面板自行关闭（事件冒泡到此处面板已隐藏，
      // 但焦点仍在面板内，需拦截避免继续穿透到「返回书架」）
      if (!el.searchPanel.classList.contains("hidden") || el.searchPanel.contains(e.target as Node)) {
        searchPanel.close();
        return;
      }
      if (!el.toc.classList.contains("hidden")) {
        show(el.toc, false);
        el.toc.dataset.open = "0";
        syncTocBtn();
        return;
      }
      if (!el.notesPanel.classList.contains("hidden")) {
        show(el.notesPanel, false);
        el.notesPanel.dataset.tab = "";
        syncNotesPanelBtns();
        if (state.settings.readingMode === "paginated") paginator.relayout();
        return;
      }
      returnToShelf();
      return;
    }
    if (!state.book || currentView !== "reader") return;
    // 滚动模式：空格 = ↓ 方向键的自然滚动步进（40px/次，与 Chromium 方向键一致，
    // 按住可连续滚动），不再触发跳章；↑ 同步处理保证行为一致
    if (
      state.settings.readingMode === "scroll" &&
      (e.key === " " || e.key === "ArrowDown" || e.key === "ArrowUp")
    ) {
      e.preventDefault();
      const d = e.key === "ArrowUp" ? -40 : 40;
      // 自然效果时走卷绕模块的插值通道（顺滑卷动），标准效果保持原生步进
      if (scrollCurl.active) scrollCurl.nudge(d);
      else el.scrollView.scrollBy({ top: d });
      return;
    }
    if (e.key === "ArrowRight" || e.key === "PageDown" || e.key === " ") {
      e.preventDefault();
      void turnNext();
    } else if (e.key === "ArrowLeft" || e.key === "PageUp") {
      e.preventDefault();
      void turnPrev();
    }
  });

  // 正文区滚轮/触摸板翻页（分页+PDF 视口）：增量累积满一格才翻一页，
  // 翻页后进入冷却期吸收滚轮惯性，防止轻碰滚轮误翻多页。
  // 滚动模式下此视口隐藏，不会触发（自然滚动）。
  let pageWheelAcc = 0;
  let pageWheelLockUntil = 0;
  el.pageViewport.addEventListener(
    "wheel",
    (e) => {
      e.preventDefault();
      const now = performance.now();
      if (now < pageWheelLockUntil) return; // 冷却期：丢弃惯性增量
      const d = Math.abs(e.deltaX) > Math.abs(e.deltaY) ? e.deltaX : e.deltaY;
      pageWheelAcc += d;
      if (Math.abs(pageWheelAcc) >= WHEEL_NOTCH) {
        const dir = pageWheelAcc > 0 ? 1 : -1;
        pageWheelAcc = 0;
        pageWheelLockUntil = now + PAGE_WHEEL_COOLDOWN;
        if (dir > 0) void turnNext();
        else void turnPrev();
      }
    },
    { passive: false },
  );

  // 悬停顶栏进度数字滚轮快速翻书：一格=1页，增量累积不丢弃，
  // 每 50ms 批量跳转一次（章内单次 transform，跨章链式加载），停手即停。
  // 键盘 ←/→（PageUp/Down、空格）本就全局翻页，此处仅补滚轮入口
  el.percent.addEventListener(
    "wheel",
    (e) => {
      e.preventDefault();
      if (!state.book || currentView !== "reader") return;
      const d = Math.abs(e.deltaX) > Math.abs(e.deltaY) ? e.deltaX : e.deltaY;
      seekCarry += d / WHEEL_NOTCH;
      const whole = Math.trunc(seekCarry);
      if (whole !== 0) {
        seekCarry -= whole;
        seekPending += whole;
        if (seekTimer === null) {
          seekTimer = window.setInterval(() => void seekConsume(), SEEK_TICK_MS);
        }
      }
    },
    { passive: false },
  );

  // 滚动模式滚动监听
  el.scrollView.addEventListener("scroll", onScroll, { passive: true });

  // 窗口拖拽文件（Tauri v2 webview 事件）。
  // 注意：WebView2 会把页面内 HTML5 拖拽（拖动书内图片/选中文本）也转发到本事件，
  // 此时 paths 为空数组 —— 必须按「携带文件路径」过滤，
  // 否则阅读页拖动图片会误显「松开加入书架」遮罩。
  let dragHasFiles = false;
  void getCurrentWebview().onDragDropEvent((event) => {
    const p = event.payload;
    if (p.type === "enter") {
      dragHasFiles = p.paths.length > 0;
      show(el.dropOverlay, dragHasFiles);
    } else if (p.type === "over") {
      // over 事件不带 paths：跟随 enter 判定的文件拖拽状态
      show(el.dropOverlay, dragHasFiles);
    } else if (p.type === "drop") {
      show(el.dropOverlay, false);
      // 拖入文件不直接打开：全部加入书架（解析失败的书会被后端拒绝）
      if (dragHasFiles) void addDroppedToShelf(p.paths);
      dragHasFiles = false;
    } else {
      show(el.dropOverlay, false);
      dragHasFiles = false;
    }
  });

  // 书内图片/书架封面无拖拽用途：禁用图片原生 HTML5 拖拽（拖动只产生拖影且无放置目标，
  // 还会触发 WebView2 拖放手势），从源头消除阅读页拖图误触「松开加入书架」
  document.addEventListener("dragstart", (e) => {
    if (e.target instanceof HTMLImageElement) e.preventDefault();
  });

  // 关窗时尽力保存进度 + 通知后端释放。
  // 注意：saveProgressSoon 是 500ms 防抖，关窗时定时器会被杀掉，
  // 这里直接发起保存（fire-and-forget），尽力让最后一页位置落盘。
  window.addEventListener("beforeunload", () => {
    clearTimeout(saveTimer);
    saveProgressNow();
    if (IS_READER_WINDOW) {
      // 关阅读窗：再落一次用户最后调整的尺寸（onResized 防抖可能被杀掉）
      void saveReaderWindowSize();
    }
    if (state.book) void api.closeBook(state.book.id).catch(() => {});
  });
}

// ────────────────────────── 启动 ──────────────────────────

async function boot(): Promise<void> {
  bindTopbar(el.topbar, el.btnMore);
  // 任务栏/Alt-Tab：用高清 PNG 设置窗口图标，避免系统把 ICO 小尺寸层拉糊
  void appWin
    .setIcon("/logo/苛读.png")
    .catch(() => {
      /* 开发态或资源未就绪时忽略 */
    });
  try {
    state.settings = await api.loadReaderSettings();
  } catch {
    // 后端已兜底返回默认值，这里仅为极端情况保底
  }
  state.settings = clampColumnWidth(state.settings);
  shelf.setSortMode(state.settings.shelfSortMode);
  applySettings();
  bindEvents();
  if (IS_READER_WINDOW) {
    // 独立阅读窗：默认进阅读视图（无书时 showView 隐藏 shelf 即可）
    showView("reader");
    const injected = (window as unknown as { __READER_BOOK_PATH__?: string }).__READER_BOOK_PATH__;
    if (typeof injected === "string" && injected) void openBook(injected);
  } else {
    showView("shelf"); // 书架窗默认进入书架
  }
}

// 主窗口 boot：设置/分类窗口由 bootstrap.ts 分流，不会加载到本模块
void boot();
