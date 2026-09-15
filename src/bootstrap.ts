/**
 * 前端窗口入口：按 Tauri 窗口 label 分流。
 *
 * 设置/分类窗口只动态加载自身模块，不再经过 main.ts 顶层
 * （避免 Paginator/Shelf/搜索/笔记等阅读器初始化）。
 * 书架（main）与阅读（reader）共用 main.ts，由其按 label 分流启动形态。
 *
 * CSS 与右键屏蔽在此统一，保证各窗口首帧行为一致。
 */
import { getCurrentWindow } from "@tauri-apps/api/window";
import "./styles.css";

// 屏蔽 WebView2 默认右键菜单（返回/刷新/打印等）；应用内自定义菜单自行 preventDefault。
document.addEventListener("contextmenu", (e) => e.preventDefault());

const label = getCurrentWindow().label;

async function bootHidden(boot: () => Promise<unknown>): Promise<void> {
  // 动态 chunk 加载期间先隐藏阅读器初始 DOM（否则会闪现欢迎页）
  document.documentElement.style.visibility = "hidden";
  try {
    await boot();
  } catch (err) {
    console.error(`${label} window boot failed:`, err);
  } finally {
    document.documentElement.style.visibility = "";
  }
}

if (label === "settings") {
  void bootHidden(() => import("./settings-window").then((m) => m.bootSettingsWindow()));
} else if (label === "genre") {
  void bootHidden(() => import("./genre-window").then((m) => m.bootGenreWindow()));
} else if (label === "cover") {
  void bootHidden(() => import("./cover-window").then((m) => m.bootCoverWindow()));
} else {
  // 书架 / 阅读窗：main.ts 末尾自行 boot()，内部按 label 区分
  void import("./main");
}
