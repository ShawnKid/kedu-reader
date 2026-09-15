/**
 * IPC 调用封装层：前端唯一与后端通信的出口。
 * 职责边界：前端不做任何电子书解析，全部数据来自这些 invoke 调用。
 *
 * Tauri v2 会自动把 JS 的 camelCase 参数名映射到 Rust 的 snake_case 形参。
 */
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import type {
  AnnotationsFile,
  BookCoverOptions,
  BookMeta,
  BookProgressFile,
  CacheInfo,
  ChapterContent,
  OpenBookResult,
  ReaderSettings,
  ReadingProgress,
  ResourcePayload,
  SearchResult,
  ShelfBook,
  ShelfCategory,
  ShelfData,
  StatsSummary,
  SystemFont,
  TocItem,
} from "./types";

export const api = {
  /** 打开书籍（路径来自 dialog / 拖拽），返回元数据 + 目录 */
  openBook(filePath: string): Promise<OpenBookResult> {
    return invoke("open_book", { filePath });
  },

  /** 重新拉取目录（前端刷新后恢复状态用） */
  getToc(bookId: string): Promise<TocItem[]> {
    return invoke("get_toc", { bookId });
  },

  /** 按需加载章节（EPUB 已清洗/内联图片；TXT 纯文本；CBZ 图片路径列表）。
   *  preserveStyles：「跟随图书设定」时加载保留书籍自带 CSS 的版本（后端独立缓存） */
  getChapterContent(bookId: string, chapterIndex: number, preserveStyles?: boolean): Promise<ChapterContent> {
    return invoke("get_chapter_content", { bookId, chapterIndex, preserveStyles: preserveStyles ?? false });
  },

  /** 取书籍内部资源（封面 / CBZ 图片），MIME + base64 */
  getResource(bookId: string, resourcePath: string): Promise<ResourcePayload> {
    return invoke("get_resource", { bookId, resourcePath });
  },

  /** 关闭书籍：后端释放内存会话 */
  closeBook(bookId: string): Promise<boolean> {
    return invoke("close_book", { bookId });
  },

  /** 保存阅读进度（后端同时落盘当时的设置快照） */
  saveProgress(progress: ReadingProgress, settings: ReaderSettings): Promise<void> {
    return invoke("save_progress", { progress, settings });
  },

  /** 读取上次进度；第一次读返回 null */
  loadProgress(bookId: string): Promise<BookProgressFile | null> {
    return invoke("load_progress", { bookId });
  },

  /** 更新全局阅读设置 */
  updateReaderSettings(settings: ReaderSettings): Promise<void> {
    return invoke("update_reader_settings", { settings });
  },

  /** 读取全局阅读设置 */
  loadReaderSettings(): Promise<ReaderSettings> {
    return invoke("load_reader_settings");
  },

  /** 枚举系统已安装字体（设置页「正文字体」下拉框数据源） */
  listSystemFonts(): Promise<SystemFont[]> {
    return invoke("list_system_fonts");
  },

  /** 打开（或聚焦）独立设置窗口 */
  openSettings(): Promise<void> {
    return invoke("open_settings_window");
  },

  // ────────────────────────── 缓存管理 ──────────────────────────

  /** 读取缓存信息：目录位置 + WebView2 缓存 / 封面缓存占用 */
  getCacheInfo(): Promise<CacheInfo> {
    return invoke("get_cache_info");
  },

  /** 设置自定义缓存目录（null = 恢复默认）；重启应用后生效 */
  setCacheDir(path: string | null): Promise<void> {
    return invoke("set_cache_dir", { path });
  },

  /** 一键清除缓存（WebView2 浏览数据 + 封面缓存），返回清除后的缓存信息 */
  clearCache(): Promise<CacheInfo> {
    return invoke("clear_cache");
  },

  /** 在系统文件管理器中打开用户数据目录 */
  openDataDir(): Promise<void> {
    return invoke("open_data_dir");
  },

  /** 打开 Windows「默认应用」设置，由用户手动关联 epub/mobi */
  openFileDialogAssociationSettings(): Promise<void> {
    return invoke("open_file_association_settings");
  },

  // ────────────────────────── 书架（需求 2/3） ──────────────────────────

  /** 读取整个书架 */
  getShelf(): Promise<ShelfData> {
    return invoke("get_shelf");
  },

  /** 导入书籍到书架（解析元数据 + 提取封面），重复导入幂等 */
  addBookToShelf(filePath: string): Promise<ShelfBook> {
    return invoke("add_book_to_shelf", { filePath });
  },

  /** 设置自选封面；null 恢复内置封面 */
  setBookCover(bookId: string, imagePath: string | null): Promise<void> {
    return invoke("set_book_cover", { bookId, imagePath });
  },

  /** 切换内置封面（direction: next 下一张 / prev 上一张，环形回绕） */
  cycleBookCover(bookId: string, direction: "next" | "prev"): Promise<{ index: number; total: number }> {
    return invoke("cycle_book_cover", { bookId, direction });
  },

  /** 更换封面窗口数据源：内置候选数量 + 当前选中关系 */
  getBookCoverOptions(bookId: string): Promise<BookCoverOptions> {
    return invoke("get_book_cover_options", { bookId });
  },

  /** 按下标选用书内内置封面 */
  setBookCoverCandidate(bookId: string, index: number): Promise<void> {
    return invoke("set_book_cover_candidate", { bookId, index });
  },

  /** 设为无封面（书架显示系统生成的格式占位图） */
  setBookCoverNone(bookId: string): Promise<void> {
    return invoke("set_book_cover_none", { bookId });
  },

  /** 打开（或聚焦）更换封面窗口（label=cover 单例；已开则切换书目） */
  openCoverWindow(bookId: string): Promise<void> {
    return invoke("open_cover_window", { bookId });
  },

  /** 从书架移除书目 */
  removeBookFromShelf(bookId: string): Promise<boolean> {
    return invoke("remove_book_from_shelf", { bookId });
  },

  /** 移动书目到分类（categoryId = null 表示未分类） */
  setBookCategory(bookId: string, categoryId: string | null): Promise<boolean> {
    return invoke("set_book_category", { bookId, categoryId });
  },

  /** 重命名书架显示书名（只改书架条目，不影响原文件元数据） */
  renameBook(bookId: string, title: string): Promise<boolean> {
    return invoke("rename_book", { bookId, title });
  },

  /** 在系统文件管理器中打开书籍文件所在目录并选中该文件 */
  revealBookInFolder(bookId: string): Promise<void> {
    return invoke("reveal_book_in_folder", { bookId });
  },

  /** 设置/清除单本书的正文字体：null = 跟随全局；FOLLOW_BOOK_FONT = 跟随图书设定；具体族名 = 统一排版且优先于全局 */
  setBookFont(bookId: string, fontFamily: string | null): Promise<boolean> {
    return invoke("set_book_font", { bookId, fontFamily });
  },

  /** 新建分类 */
  addCategory(name: string): Promise<ShelfCategory> {
    return invoke("add_category", { name });
  },

  /** 重命名分类 */
  renameCategory(categoryId: string, name: string): Promise<boolean> {
    return invoke("rename_category", { categoryId, name });
  },

  /** 删除分类（其下书籍回到未分类） */
  removeCategory(categoryId: string): Promise<boolean> {
    return invoke("remove_category", { categoryId });
  },

  /** 拖拽排序分类：orderedIds 为拖拽后的自定义分类 id 顺序 */
  reorderCategoryOrder(orderedIds: string[]): Promise<boolean> {
    return invoke("reorder_category", { orderedIds });
  },

  // ────────────────────────── 阅读统计（需求 4/5） ──────────────────────────

  /** 阅读时长心跳上报（前端每 30 秒一次，后端按本地日期落账；title 用于移除书架后统计页仍显示书名） */
  recordReadingTime(bookId: string, seconds: number, title?: string): Promise<void> {
    return invoke("record_reading_time", { bookId, seconds, title: title ?? null });
  },

  /** 阅读会话上报（一段连续计费阅读结束时一次落账：开始时刻/时长/进度起止） */
  recordSession(
    bookId: string,
    startedAt: number,
    seconds: number,
    title?: string,
    percentStart?: number | null,
    percentEnd?: number | null,
  ): Promise<void> {
    return invoke("record_session", {
      bookId,
      startedAt,
      seconds,
      title: title ?? null,
      percentStart: percentStart ?? null,
      percentEnd: percentEnd ?? null,
    });
  },

  /** 全书字数懒计算（幂等：已有值直接返回；null = 书架无此书/无法解析） */
  computeBookCharCount(bookId: string): Promise<number | null> {
    return invoke("compute_book_char_count", { bookId });
  },

  /** 回填 PDF 页数（pdf.js load 拿到 pageCount 后调用） */
  setBookPageCount(bookId: string, count: number): Promise<boolean> {
    return invoke("set_book_page_count", { bookId, count });
  },

  /** 设置/清除星级评分（0~50 半星步进；null = 清除） */
  setBookRating(bookId: string, rating: number | null): Promise<boolean> {
    return invoke("set_book_rating", { bookId, rating });
  },

  /** 打开（或聚焦）题材设置窗口（label=genre 单例；已开则切换书目） */
  openGenreWindow(bookId: string): Promise<void> {
    return invoke("open_genre_window", { bookId });
  },

  /** 从书架打开独立阅读窗口（label=reader 单例；已开则切书并聚焦） */
  openReaderWindow(filePath: string): Promise<void> {
    return invoke("open_reader_window", { filePath });
  },

  /** 阅读窗「返回书架」：把书架窗置于最上（不关阅读窗） */
  focusShelfWindow(): Promise<void> {
    return invoke("focus_shelf_window");
  },

  /** 记录阅读窗最近一次用户主动调整的内尺寸（物理像素） */
  saveReaderWindowSize(width: number, height: number): Promise<void> {
    return invoke("save_reader_window_size", { width, height });
  },

  /** 设置书籍题材标签（多选；后端做净化） */
  setBookGenres(bookId: string, genres: string[]): Promise<boolean> {
    return invoke("set_book_genres", { bookId, genres });
  },

  /** 聚合统计（今日/本周/本月/累计 + 30 天曲线 + 书籍排行） */
  getReadingStats(): Promise<StatsSummary> {
    return invoke("get_reading_stats");
  },

  /** 设置某本书是否计入阅读统计（隐私开关；设为不计入时后端同步清除历史时长） */
  setBookStatsExcluded(bookId: string, excluded: boolean): Promise<boolean> {
    return invoke("set_book_stats_excluded", { bookId, excluded });
  },

  // ────────────────────────── 全文搜索 ──────────────────────────

  /** 全书全文搜索（仅文字书；PDF/CBZ 前端不调用） */
  searchBook(bookId: string, query: string): Promise<SearchResult> {
    return invoke("search_book", { bookId, query });
  },

  // ────────────────────────── 笔记与书签（需求 6） ──────────────────────────

  /** 读取某本书的全部笔记与书签（从未做过返回空结构） */
  getAnnotations(bookId: string): Promise<AnnotationsFile> {
    return invoke("get_annotations", { bookId });
  },

  /** 整体保存某本书的笔记与书签 */
  saveAnnotations(bookId: string, annotations: AnnotationsFile): Promise<void> {
    return invoke("save_annotations", { bookId, annotations });
  },

  // ────────────────────────── PDF ──────────────────────────

  /** 打开 PDF：后端只生成稳定元数据，字节由 book-file:// 协议直出 */
  openPdf(filePath: string): Promise<BookMeta> {
    return invoke("open_pdf", { filePath });
  },
};

/** 书源文件协议 URL（pdf.js 加载用）：按 book_id 从书架登记路径直出字节 */
export function bookFileUrl(bookId: string): string {
  return convertFileSrc(bookId, "book-file");
}

/**
 * 书架封面协议 URL：无封面时协议端返回 404，调用方需 onerror 兜底。
 *
 * WebView2 会缓存自定义协议响应（即使响应带 Cache-Control: no-store），
 * 仅靠固定 URL 更换封面后拿不到新图，因此必须让 URL 随内容变化：
 * - 会话纪元：每次启动应用重新生成，规避跨启动的磁盘缓存残留；
 * - 单书记数：该书面封被更换（自选/切换内置）时自增，render 后立即取新图。
 */
const coverSessionEpoch = Date.now();
const coverVersions = new Map<string, number>();

/** 标记某本书的封面文件已变化：下一次 shelfCoverUrl 生成全新 URL */
export function bumpCoverVersion(bookId: string): void {
  coverVersions.set(bookId, (coverVersions.get(bookId) ?? 0) + 1);
}

export function shelfCoverUrl(bookId: string): string {
  const v = coverVersions.get(bookId) ?? 0;
  return `${convertFileSrc(bookId, "shelf-cover")}?v=${coverSessionEpoch}.${v}`;
}

/** 系统默认封面（cover.dat，不含 custom） */
export function coverDefaultUrl(bookId: string): string {
  return `${convertFileSrc(`${bookId}/default`, "shelf-cover")}?v=${coverSessionEpoch}`;
}

/** 书内第 index 张内置候选（0-based） */
export function coverCandidateUrl(bookId: string, index: number): string {
  return `${convertFileSrc(`${bookId}/candidate/${index}`, "shelf-cover")}?v=${coverSessionEpoch}`;
}

/** 当前自定义/切换后的独立封面文件（cover-custom.dat） */
export function coverCustomUrl(bookId: string): string {
  const v = coverVersions.get(bookId) ?? 0;
  return `${convertFileSrc(`${bookId}/custom`, "shelf-cover")}?v=${coverSessionEpoch}.${v}`;
}
