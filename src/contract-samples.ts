/**
 * 双端契约样例的 TS 侧类型钉扎（E12）。
 * 仅类型层，不引入运行时依赖；`npm run typecheck` 即校验字段漂移。
 * 样例 JSON 见 fixtures/contract/。
 */
import type {
  AnnotationsFile,
  BookProgressFile,
  ReaderSettings,
  ShelfData,
} from "./types";

export const readerSettingsFull: ReaderSettings = {
  fontSizePx: 22,
  lineHeight: 1.8,
  pageMarginPx: 40,
  theme: "sepia",
  readingMode: "scroll",
  twoPageSpread: true,
  pageTurnEffect: "natural",
  scrollEffect: "natural",
  columnWidthPx: 600,
  scrollColumnWidthPx: 680,
  fontFamily: "Microsoft YaHei",
  defaultCategory: "cat-fiction",
  shelfCardSize: 45,
  shelfSortMode: "title",
  minimizeToTray: true,
};

export const bookProgressSample: BookProgressFile = {
  progress: {
    bookId: "bk0000000000000001",
    chapterIndex: 3,
    anchor: "para-12",
    scrollRatio: 0.42,
    pageInChapter: 2,
    percent: 37.5,
    updatedAt: 1768000000,
  },
  settings: {
    fontSizePx: 18,
    lineHeight: 1.6,
    pageMarginPx: 32,
    theme: "light",
    readingMode: "paginated",
    twoPageSpread: false,
    pageTurnEffect: "standard",
    scrollEffect: "standard",
    columnWidthPx: 730,
    scrollColumnWidthPx: 720,
    fontFamily: null,
    defaultCategory: null,
    shelfCardSize: 30,
    shelfSortMode: "recent",
    minimizeToTray: false,
  },
};

export const shelfSample: ShelfData = {
  categories: [
    { id: "cat-fiction", name: "小说", order: 0 },
    { id: "cat-tech", name: "技术", order: 1 },
  ],
  books: [
    {
      id: "bk0000000000000001",
      title: "契约样例书",
      author: "作者甲",
      format: "epub",
      fileSize: 102400,
      filePath: "F:\\books\\sample.epub",
      categoryId: "cat-fiction",
      addedAt: 1767000000,
      lastReadAt: 1768000000,
      percent: 37.5,
      finished: false,
      fontFamily: "@follow-book",
      statsExcluded: null,
      finishedAt: null,
      finishedTimes: 0,
      charCount: 123456,
      pageCount: null,
      rating: 87,
      ratedAt: 1767900000,
      genres: ["小说", "科幻"],
    },
  ],
  uncategorizedOrder: 0,
};

export const annotationsSample: AnnotationsFile = {
  notes: [
    {
      id: "nt-1",
      bookId: "bk0000000000000001",
      chapterIndex: 0,
      quote: "被划线的原文",
      prefix: "前文上下文",
      suffix: "后文上下文",
      color: "yellow",
      style: "highlight",
      note: "读者批注",
      createdAt: 1767900000,
    },
  ],
  bookmarks: [
    {
      id: "bm-legacy",
      bookId: "bk0000000000000001",
      chapterIndex: 0,
      anchor: null,
      pageInChapter: null,
      quote: null,
      prefix: null,
      suffix: null,
      excerpt: "旧书签缺 quote/prefix/suffix",
      createdAt: 1700000000,
    },
  ],
};
