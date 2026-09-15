/**
 * 阅读器布局/设置常量与默认值。
 * main.ts 与 settings-window.ts 共用，避免两端默认值漂移。
 * 与 model.rs::ReaderSettings::default 及契约样例对齐。
 */
import type { ReaderSettings } from "./types";

/** 分栏栏宽下限（px）：低于该宽度一栏可读性差；滑杆上限为屏幕宽度 */
export const COLUMN_WIDTH_MIN = 360;
/** 分栏栏宽默认值（px） */
export const COLUMN_WIDTH_DEFAULT = 730;
/** 滚动模式栏宽下限/默认值（px） */
export const SCROLL_WIDTH_MIN = 480;
export const SCROLL_WIDTH_DEFAULT = 720;
/** 书架卡片大小默认值（滑块 0~100，30 = 基准尺寸） */
export const SHELF_CARD_SIZE_DEFAULT = 30;

/** 阅读设置默认值（初始状态与「恢复默认」按钮共用，与 model.rs::ReaderSettings::default 对应） */
export const DEFAULT_SETTINGS: ReaderSettings = {
  fontSizePx: 18,
  lineHeight: 1.6,
  pageMarginPx: 32,
  theme: "light",
  readingMode: "paginated",
  twoPageSpread: false,
  pageTurnEffect: "standard",
  scrollEffect: "standard",
  columnWidthPx: COLUMN_WIDTH_DEFAULT,
  scrollColumnWidthPx: SCROLL_WIDTH_DEFAULT,
  fontFamily: null,
  defaultCategory: null,
  shelfCardSize: SHELF_CARD_SIZE_DEFAULT,
  shelfSortMode: "recent",
  minimizeToTray: false,
};
