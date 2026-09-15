/**
 * IPC 类型定义 —— 与 src-tauri/src/model.rs、state.rs 的 Rust 结构体一一对应。
 * Rust 侧 serde `rename_all = "camelCase"`，此处字段名保持一致。
 */

export type BookFormat =
  | "epub"
  | "mobi"
  | "azw3"
  | "txt"
  | "markdown"
  | "fb2"
  | "cbz"
  | "pdf";

/** model.rs::BookMeta */
export interface BookMeta {
  id: string;
  title: string;
  author: string | null;
  publisher: string | null;
  language: string | null;
  format: BookFormat;
  fileSize: number;
  filePath: string;
  totalChapters: number;
  /** 封面资源内部路径，通过 getResource 取 base64 */
  coverResource: string | null;
}

/** model.rs::TocItem */
export interface TocItem {
  id: string;
  label: string;
  chapterIndex: number;
  /** EPUB 元素 id / TXT 字符偏移（字符串） */
  anchor: string | null;
  children: TocItem[];
}

/** model.rs::ChapterKind */
export type ChapterKind = "html" | "text" | "images";

/** model.rs::ChapterContent */
export interface ChapterContent {
  bookId: string;
  chapterIndex: number;
  title: string;
  kind: ChapterKind;
  /** kind === "html"：后端已清洗并内联图片（data: URI） */
  html: string | null;
  /** kind === "text" */
  text: string | null;
  /** kind === "images"：ZIP 内资源路径列表，逐张调 getResource */
  imageRefs: string[] | null;
}

/** model.rs::Theme */
export type Theme = "light" | "dark" | "sepia" | "green";

/** model.rs::ReadingMode */
export type ReadingMode = "paginated" | "scroll";

/** model.rs::ReadingProgress */
export interface ReadingProgress {
  bookId: string;
  chapterIndex: number;
  anchor: string | null;
  /** 滚动模式 0.0~1.0 */
  scrollRatio: number;
  /** 分页模式章内页码 */
  pageInChapter: number | null;
  /** 全书百分比 */
  percent: number;
  /** Unix 秒 */
  updatedAt: number;
}

/** 书架排序方式（model.rs::ShelfSortMode） */
export type ShelfSortMode = "recent" | "added" | "title" | "progress";

/** 单本字体「跟随图书设定」标记值（ShelfBook.fontFamily 存此值 = 本书还原书籍自带 CSS，无视全局字体选择）。
 *  取 CSS 合法字体名不可能出现的值，后端仅按普通字符串透传落盘 */
export const FOLLOW_BOOK_FONT = "@follow-book";

/** model.rs::ReaderSettings */
export interface ReaderSettings {
  fontSizePx: number;
  lineHeight: number;
  pageMarginPx: number;
  theme: Theme;
  readingMode: ReadingMode;
  twoPageSpread: boolean;
  pageTurnEffect: "natural" | "standard";
  /** 滚动模式效果：natural = 卷轴卷绕（上卷轴直径 66px，滚动内容沿顶部虚拟圆柱卷入）；standard = 原生平滑裁剪 */
  scrollEffect: "natural" | "standard";
  /** 分页模式分栏目标栏宽（px）；列数由可用宽度自适应，栏宽≥可用宽度时自然为单栏 */
  columnWidthPx: number;
  /** 滚动模式正文栏宽（px）：内容列最大宽度，窄于窗口时随窗口收缩，始终居中 */
  scrollColumnWidthPx: number;
  /** null = 跟随图书设定（还原书籍自带 CSS 与字体；无内置样式的书用默认字体栈）；具体族名 = 全文统一排版 */
  fontFamily: string | null;
  /** 书架默认显示分类：null = 全部；"" = 未分类；其他 = 分类 id（失效时前端回退「全部」） */
  defaultCategory: string | null;
  /** 书架卡片大小：0~100 滑块，30 = 基准尺寸（卡片实际缩放 = 值/30，下限 0.5、上限 3） */
  shelfCardSize: number;
  /** 书架排序方式：最近阅读（默认）/ 添加时间 / 书名 / 进度 */
  shelfSortMode: ShelfSortMode;
  /** 关闭主窗口/阅读窗时是否缩小到系统托盘（false = 直接关闭/退出） */
  minimizeToTray: boolean;
}

/** commands/settings.rs::SystemFont（系统字体枚举项，设置页「正文字体」下拉框） */
export interface SystemFont {
  /** CSS font-family 族名（写入 ReaderSettings.fontFamily） */
  family: string;
  /** 下拉框展示名（优先简体中文名，如「微软雅黑」） */
  display: string;
}

/** open_book 返回体（model.rs::OpenBookResult） */
export interface OpenBookResult {
  book: BookMeta;
  toc: TocItem[];
}

/** state.rs::ResourcePayload */
export interface ResourcePayload {
  mimeType: string;
  base64: string;
}

/** storage.rs::BookProgressFile（load_progress 返回体） */
export interface BookProgressFile {
  progress: ReadingProgress;
  settings: ReaderSettings;
}

// ────────────────────────── 书架（需求 2/3） ──────────────────────────

/** model.rs::ShelfCategory */
export interface ShelfCategory {
  id: string;
  name: string;
  order: number;
}

/** model.rs::ShelfBook */
export interface ShelfBook {
  id: string;
  title: string;
  author: string | null;
  format: BookFormat;
  fileSize: number;
  filePath: string;
  /** null = 未分类 */
  categoryId: string | null;
  /** Unix 秒 */
  addedAt: number;
  lastReadAt: number;
  /** 0~100 */
  percent: number;
  finished: boolean;
  /** 单本正文字体（书架右键设置）：null = 跟随全局；FOLLOW_BOOK_FONT = 跟随图书设定（无视全局选择，强制还原书籍自带 CSS）；具体族名 = 全文统一排版且优先于全局字体 */
  fontFamily: string | null;
  /** 统计收录开关（隐私）：null = 未手动设置（按书名敏感词自动判定）；true = 不计入；false = 计入 */
  statsExcluded: boolean | null;
  /** 最近一次跨过 99.5 的时刻（Unix 秒）；null = 该事件之前未发生过（旧数据无记录） */
  finishedAt: number | null;
  /** 累计读完遍数（重读再次跨过 99.5 会 +1） */
  finishedTimes: number;
  /** 全书字符数（懒计算；null = 未计算） */
  charCount: number | null;
  /** PDF 页数（前端 pdf.js 回填；文字书恒为 null） */
  pageCount: number | null;
  /** 星级评分 0~50（半星步进，45 = 4.5 星）；null = 未评分 */
  rating: number | null;
  /** 最近评分时间（Unix 秒） */
  ratedAt: number | null;
  /** 题材标签（多选，预设树见 src/genres.ts；空数组 = 未标注） */
  genres: string[];
}

/** model.rs::ShelfData */
export interface ShelfData {
  categories: ShelfCategory[];
  books: ShelfBook[];
  /** 「未分类」虚拟分类的展示序位：与 categories[].order 同一数轴，0 = 排最前（旧数据默认） */
  uncategorizedOrder: number;
}

// ────────────────────────── 阅读统计（需求 4/5） ──────────────────────────

/** model.rs::DailyPoint */
export interface DailyPoint {
  date: string;
  seconds: number;
}

/** model.rs::BookTimeStat */
export interface BookTimeStat {
  bookId: string;
  title: string;
  seconds: number;
}

/** model.rs::NameValue（分类/格式分布横条项） */
export interface NameValue {
  name: string;
  value: number;
}

/** model.rs::StatsSummary（get_reading_stats 聚合结果，全部实时聚合） */
export interface StatsSummary {
  todaySeconds: number;
  weekSeconds: number;
  monthSeconds: number;
  totalSeconds: number;
  /** 近 365 天（含今天，无数据补 0），日历热力图用 */
  daily: DailyPoint[];
  topBooks: BookTimeStat[];
  bookCount: number;
  finishedCount: number;
  /** 连续阅读天数（今天没读不断签，从昨天起算） */
  streakDays: number;
  /** 有阅读记录的总天数 */
  totalReadDays: number;
  /** 单日峰值 */
  peakDaySeconds: number;
  peakDayDate: string | null;
  /** 近 30 天日均 */
  avgDaily30: number;
  /** 24 小时时段桶（秒），会话按本地时间逐小时切分归桶 */
  hourBuckets: number[];
  /** 会话总数 */
  sessionCount: number;
  /** 平均单次会话时长（秒） */
  avgSessionSeconds: number;
  /** 最长单次会话时长（秒） */
  maxSessionSeconds: number;
  /** 累计读字数（近似） */
  totalCharsRead: number;
  /** 平均阅读速度（字/分）；null = 无可计字数的会话 */
  avgSpeedCpm: number | null;
  /** 题材时长分布（降序 Top10；未标注单独归项） */
  genreDist: NameValue[];
  /** 格式时长分布（降序） */
  formatDist: NameValue[];
  /** 全库笔记数 */
  noteCount: number;
  /** 全库书签数 */
  bookmarkCount: number;
}

// ────────────────────────── 笔记与书签（需求 6） ──────────────────────────

/** model.rs::NoteStyle */
export type NoteStyle = "highlight" | "underline" | "wavy";

/** model.rs::NoteColor */
export type NoteColor = "yellow" | "green" | "blue" | "pink" | "purple";

/** model.rs::Note */
export interface Note {
  id: string;
  bookId: string;
  chapterIndex: number;
  /** 被划线的原文 */
  quote: string;
  /** 划线前的上下文（同文多处出现时消歧） */
  prefix: string;
  /** 划线后的上下文 */
  suffix: string;
  color: NoteColor;
  style: NoteStyle;
  /** 想法内容；null = 纯划线 */
  note: string | null;
  /** Unix 秒 */
  createdAt: number;
}

/** model.rs::Bookmark */
export interface Bookmark {
  id: string;
  bookId: string;
  /** 文字书：章节下标；PDF：当前页 - 1 */
  chapterIndex: number;
  /** 旧数据 / 兜底定位锚点 */
  anchor: string | null;
  /** 分页模式章内页码（指纹跳转消歧提示 + 兜底） */
  pageInChapter: number | null;
  /** 隐形笔记定位指纹：锚点标点后的引文；null = 兜底定位（无文本/旧数据） */
  quote: string | null;
  /** 锚点标点及其前至多 2 字 */
  prefix: string | null;
  /** quote 后至多 12 字 */
  suffix: string | null;
  /** 书签处一小段文字（列表展示用） */
  excerpt: string;
  /** Unix 秒 */
  createdAt: number;
}

/** model.rs::AnnotationsFile（get_annotations 返回体） */
export interface AnnotationsFile {
  notes: Note[];
  bookmarks: Bookmark[];
}

/** 缓存信息（get_cache_info / clear_cache 返回体，设置页「存储」区块展示） */
export interface CacheInfo {
  /** 当前生效（或重启后生效）的 WebView2 用户数据目录 */
  webviewDir: string;
  /** WebView2 用户数据目录总占用（字节） */
  webviewSizeBytes: number;
  /** 全部封面缓存占用（字节） */
  coversSizeBytes: number;
  /** 是否为自定义缓存目录 */
  custom: boolean;
  /** 用户数据目录（settings/shelf/stats/prefs.json + books/ 所在位置） */
  dataDir: string;
}

/** commands/shelf.rs::CoverOptions（更换封面窗口数据源） */
export interface BookCoverOptions {
  bookId: string;
  title: string;
  /** 书籍格式（无封面占位印记） */
  format: BookFormat;
  /** 系统默认封面（cover.dat）是否存在 */
  hasDefault: boolean;
  /** 书内内置候选张数 */
  candidateCount: number;
  /** "default" | "candidate:{index}" | "custom" | "none" */
  selection: string;
  /** cover-custom.dat 是否存在 */
  hasCustom: boolean;
  /** 当前 custom 是否等于某张内置候选 */
  customIsBuiltin: boolean;
}

// ────────────────────────── 全文搜索 ──────────────────────────

/** commands/search.rs::SearchMatch（前端凭 quote+prefix 指纹在已渲染 DOM 中定位） */
export interface SearchMatch {
  chapterIndex: number;
  chapterTitle: string;
  /** 命中文本（归一化后原样返回） */
  quote: string;
  /** 命中前 ≤12 字（同章同文多命中消歧） */
  prefix: string;
  /** 命中后 ≤12 字 */
  suffix: string;
  /** 摘要前段（≤40 字，截断时以 … 开头） */
  pre: string;
  /** 摘要后段（≤40 字，截断时以 … 结尾） */
  post: string;
}

/** commands/search.rs::SearchResult（search_book 返回体） */
export interface SearchResult {
  /** 按「章节顺序 + 章内出现顺序」排列，最多 500 条 */
  matches: SearchMatch[];
  /** 截断前真实总数 */
  total: number;
  truncated: boolean;
}
