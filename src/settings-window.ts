/**
 * 独立设置窗口（Tauri 窗口 label = "settings"）的页面逻辑。
 *
 * 与主窗口共用同一前端产物（index.html）：bootstrap.ts 按窗口 label 把启动流程
 * 分流到这里，本模块整体替换掉 index.html 携带的阅读器 DOM，只渲染设置页。
 *
 * 数据流（单向，杜绝回声循环）：
 * - 本窗口修改设置 → emit("settings-changed") 广播给主窗口实时应用 + 防抖落盘；
 * - 收到 "settings-changed"（含自己发出的）→ 仅同步控件显示，不再转发。
 */
import { emit, listen } from "@tauri-apps/api/event";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { api } from "./ipc";
import {
  COLUMN_WIDTH_DEFAULT,
  COLUMN_WIDTH_MIN,
  DEFAULT_SETTINGS,
  SCROLL_WIDTH_DEFAULT,
  SCROLL_WIDTH_MIN,
  SHELF_CARD_SIZE_DEFAULT,
} from "./reader-defaults";
import type { CacheInfo, ReaderSettings, ShelfCategory, SystemFont } from "./types";

let settings: ReaderSettings = { ...DEFAULT_SETTINGS };
let saveTimer: number | undefined;
/** 缓存信息（boot 时加载；加载失败保持 null，「存储」区块显示为空但不影响其他设置） */
let cacheInfo: CacheInfo | null = null;
/** 书架分类列表（「默认显示分类」下拉框数据源；加载失败为空 → 仅剩 全部/未分类） */
let shelfCategories: ShelfCategory[] = [];

const byId = <T extends HTMLElement>(id: string): T => document.getElementById(id) as T;

/** 兼容旧配置：栏宽缺失/为 0 回退默认，并钳制到下限 */
function clampSettings(s: ReaderSettings): ReaderSettings {
  if (s.pageTurnEffect !== "natural") s.pageTurnEffect = "standard";
  if (s.scrollEffect !== "natural") s.scrollEffect = "standard";
  s.columnWidthPx = Math.max(COLUMN_WIDTH_MIN, s.columnWidthPx || COLUMN_WIDTH_DEFAULT);
  s.scrollColumnWidthPx = Math.max(SCROLL_WIDTH_MIN, s.scrollColumnWidthPx || SCROLL_WIDTH_DEFAULT);
  s.shelfCardSize = Math.min(100, Math.max(0, Math.round(Number.isFinite(s.shelfCardSize) ? s.shelfCardSize : SHELF_CARD_SIZE_DEFAULT)));
  // 书架排序方式：非法值回退默认「最近阅读」
  if (!["recent", "added", "title", "progress"].includes(s.shelfSortMode)) s.shelfSortMode = "recent";
  return s;
}

/** 设置值 → 下拉框 value：null = 全部，"" = 未分类，其余为分类 id */
function settingToCategoryValue(v: string | null): string {
  return v === null ? "all" : v === "" ? "uncategorized" : v;
}

/** 下拉框 value → 设置值（与 settingToCategoryValue 互逆） */
function categoryValueToSetting(v: string): string | null {
  return v === "all" ? null : v === "uncategorized" ? "" : v;
}

/** 构建设置页 DOM（整体替换 index.html 的阅读器结构） */
function buildDom(): void {
  document.title = "设置";
  document.body.classList.add("settings-page");
  document.body.innerHTML = `
    <header id="sw-titlebar" data-tauri-drag-region>
      <span class="sw-title" data-tauri-drag-region>设置</span>
      <button id="sw-close" class="win-btn close" title="关闭">
        <svg width="10" height="10" viewBox="0 0 10 10"><path d="M0 0l10 10M10 0L0 10" stroke="currentColor" stroke-width="1"/></svg>
      </button>
    </header>
    <main id="sw-body">
      <section class="sw-card">
        <h3 class="sw-heading">阅读设置</h3>
        <div class="sw-row">
          <span class="sw-label">正文字体</span>
          <select id="set-font-family" class="sw-select" title="跟随图书设定：还原书籍自带字体与排版（无内置样式的书用默认字体栈）；选择具体字体则全文统一排版"></select>
        </div>
        <div class="sw-row">
          <span class="sw-label">字号</span>
          <button id="font-dec" class="sw-step" title="减小字号" aria-label="减小字号">−</button>
          <input id="set-font" type="range" min="12" max="36" step="1" />
          <button id="font-inc" class="sw-step" title="增大字号" aria-label="增大字号">+</button>
          <output id="val-font" class="sw-val"></output>
        </div>
        <div class="sw-row">
          <span class="sw-label">行距</span>
          <input id="set-line" type="range" min="1.2" max="2.6" step="0.1" />
          <output id="val-line" class="sw-val"></output>
        </div>
        <div class="sw-row">
          <span class="sw-label">页边距</span>
          <input id="set-margin" type="range" min="0" max="96" step="4" />
          <output id="val-margin" class="sw-val"></output>
        </div>
        <div class="sw-row">
          <span class="sw-label">每栏最大栏宽</span>
          <input id="set-col" type="range" min="360" max="1920" step="10" />
          <output id="val-col" class="sw-val"></output>
        </div>
        <div class="sw-row">
          <span class="sw-label">滚动栏宽</span>
          <input id="set-scroll-col" type="range" min="480" max="1920" step="20" />
          <output id="val-scroll-col" class="sw-val"></output>
        </div>
        <div class="sw-row">
          <label class="sw-label" for="set-page-turn">翻页动画</label>
          <select id="set-page-turn" class="sw-select">
            <option value="natural">自然</option>
            <option value="standard">标准</option>
          </select>
        </div>
        <div class="sw-row">
          <label class="sw-label" for="set-scroll-effect">滚动动画</label>
          <select id="set-scroll-effect" class="sw-select">
            <option value="natural">自然</option>
            <option value="standard">标准</option>
          </select>
        </div>
        <div class="sw-row">
          <span class="sw-label">分页强制两栏</span>
          <label class="sw-check">
            <input id="set-force-two" type="checkbox" />
            <span>开启后分页模式固定两栏</span>
          </label>
        </div>
        <div class="sw-row">
          <span class="sw-label">主题</span>
          <div class="theme-row">
            <button class="theme-card" data-theme-pick="light" title="日间">
              <span class="tc-check"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3.2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg></span>
              <span class="tc-swatch"><i></i><i></i><i></i></span>
              <span class="tc-name">日间</span>
            </button>
            <button class="theme-card" data-theme-pick="dark" title="夜间">
              <span class="tc-check"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3.2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg></span>
              <span class="tc-swatch"><i></i><i></i><i></i></span>
              <span class="tc-name">夜间</span>
            </button>
            <button class="theme-card" data-theme-pick="sepia" title="护眼">
              <span class="tc-check"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3.2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg></span>
              <span class="tc-swatch"><i></i><i></i><i></i></span>
              <span class="tc-name">护眼</span>
            </button>
            <button class="theme-card" data-theme-pick="green" title="护眼绿">
              <span class="tc-check"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3.2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg></span>
              <span class="tc-swatch"><i></i><i></i><i></i></span>
              <span class="tc-name">护眼绿</span>
            </button>
          </div>
        </div>
        <div class="sw-row">
          <span class="sw-label"></span>
          <span class="sw-flex"></span>
          <button id="btn-reset-settings" class="sw-btn" title="把所有阅读设置恢复为默认值">恢复默认</button>
        </div>
      </section>

      <section class="sw-card">
        <h3 class="sw-heading">书架设置</h3>
        <div class="sw-row">
          <span class="sw-label">书本大小</span>
          <input id="set-shelf-size" type="range" min="0" max="100" step="1" />
          <output id="val-shelf-size" class="sw-val"></output>
        </div>
        <div class="sw-row">
          <span class="sw-label">默认显示分类</span>
          <select id="set-default-category" class="sw-select" title="进入书架视图时默认选中的分类"></select>
        </div>
        <div class="sw-row">
          <span class="sw-label">排序方式</span>
          <select id="set-shelf-sort" class="sw-select" title="书架卡片的排列顺序">
            <option value="recent">最近阅读</option>
            <option value="added">添加时间</option>
            <option value="title">书名</option>
            <option value="progress">阅读进度</option>
          </select>
        </div>
      </section>

      <section class="sw-card">
        <h3 class="sw-heading">应用</h3>
        <div class="sw-row">
          <span class="sw-label">关闭行为</span>
          <label class="sw-check" title="开启后点右上角关闭不退出进程，缩到系统托盘；托盘可恢复窗口或彻底退出">
            <input id="set-minimize-tray" type="checkbox" />
            <span>关闭窗口时最小化到系统托盘</span>
          </label>
        </div>
      </section>

      <section class="sw-card">
        <h3 class="sw-heading">存储</h3>
        <div class="sw-row">
          <span class="sw-label">缓存目录</span>
          <div class="sw-col2">
            <span id="cache-dir-text" class="sw-path"></span>
            <span id="cache-dir-hint" class="sw-hint" hidden></span>
          </div>
          <button id="btn-cache-dir" class="sw-btn" title="把 WebView2 缓存重定向到其他目录，重启应用后生效">更改…</button>
          <button id="btn-cache-reset" class="sw-btn" hidden title="缓存目录恢复到系统默认位置，重启应用后生效">还原默认</button>
        </div>
        <div class="sw-row">
          <span class="sw-label">缓存占用</span>
          <span id="cache-size-text" class="sw-path"></span>
          <button id="btn-cache-clear" class="sw-btn" title="清除 WebView2 缓存与全部封面缓存，封面在下次打开书时自动重建">清除缓存</button>
        </div>
        <div class="sw-row">
          <span class="sw-label">用户数据</span>
          <span id="data-dir-text" class="sw-path"></span>
          <button id="btn-open-data-dir" class="sw-btn" title="在文件管理器中打开用户数据目录（书架/进度/笔记所在位置）">打开目录</button>
        </div>
      </section>

      <section class="sw-card">
        <h3 class="sw-heading">文件关联</h3>
        <div class="sw-row">
          <span class="sw-label">EPUB / MOBI</span>
          <span class="sw-path" title="安装时不会自动设为默认程序；是否关联由你在系统设置中决定">安装时不会自动设为默认程序。可在系统设置中手动选择苛读。</span>
          <button id="btn-open-file-assoc" class="sw-btn" title="打开 Windows「默认应用」设置，在 .epub / .mobi 等类型中选择苛读">打开系统设置</button>
        </div>
      </section>

      <section class="sw-card">
        <h3 class="sw-heading">关于</h3>
        <div class="sw-about">
          <span>作者：Shawn·Kid</span>
          <span>邮箱：i@kxkid.com</span>
        </div>
      </section>
    </main>`;

  // 栏宽滑杆上限 = 屏幕宽度（栏宽≥可用宽度时自然为单栏）
  byId<HTMLInputElement>("set-col").max = String(window.screen.width);
  byId<HTMLInputElement>("set-scroll-col").max = String(window.screen.width);
}

/** 把当前设置同步到控件显示与窗口主题 */
function syncControls(): void {
  byId<HTMLSelectElement>("set-font-family").value = settings.fontFamily ?? "";
  byId<HTMLInputElement>("set-font").value = String(settings.fontSizePx);
  byId<HTMLInputElement>("set-line").value = String(settings.lineHeight);
  byId<HTMLInputElement>("set-margin").value = String(settings.pageMarginPx);
  byId<HTMLInputElement>("set-col").value = String(settings.columnWidthPx);
  byId<HTMLInputElement>("set-scroll-col").value = String(settings.scrollColumnWidthPx);
  byId<HTMLInputElement>("set-force-two").checked = settings.twoPageSpread;
  byId<HTMLSelectElement>("set-page-turn").value = settings.pageTurnEffect;
  byId<HTMLSelectElement>("set-scroll-effect").value = settings.scrollEffect;
  byId<HTMLInputElement>("set-shelf-size").value = String(settings.shelfCardSize);
  byId<HTMLOutputElement>("val-font").textContent = `${settings.fontSizePx}px`;
  byId<HTMLOutputElement>("val-line").textContent = settings.lineHeight.toFixed(1);
  byId<HTMLOutputElement>("val-margin").textContent = `${settings.pageMarginPx}px`;
  byId<HTMLOutputElement>("val-col").textContent = `${settings.columnWidthPx}px`;
  byId<HTMLOutputElement>("val-scroll-col").textContent = `${settings.scrollColumnWidthPx}px`;
  // 书本大小：显示实际缩放百分比（与 main.ts::shelfCardScale 同式：1~30 → 50%~100%，>30 → v/30）
  const scale = settings.shelfCardSize <= 1 ? 0.5
    : settings.shelfCardSize <= 30 ? 0.5 + ((settings.shelfCardSize - 1) / 29) * 0.5
    : Math.min(3, settings.shelfCardSize / 30);
  byId<HTMLOutputElement>("val-shelf-size").textContent = `${Math.round(scale * 100)}%`;
  document.body.dataset.theme = settings.theme;
  document
    .querySelectorAll<HTMLButtonElement>(".theme-card")
    .forEach((d) => d.classList.toggle("active", d.dataset.themePick === settings.theme));
  // 默认显示分类：选项由 renderCategoryOptions 异步填充，未就绪时跳过（避免 value 落空显示为空白）
  const catSel = byId<HTMLSelectElement>("set-default-category");
  const catVal = settingToCategoryValue(settings.defaultCategory);
  if (catSel.querySelector(`option[value='${catVal}']`)) catSel.value = catVal;
  byId<HTMLSelectElement>("set-shelf-sort").value = settings.shelfSortMode;
  byId<HTMLInputElement>("set-minimize-tray").checked = settings.minimizeToTray;
}

/** 字节数人性化显示（B/KB/MB/GB，1 位小数） */
function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let v = bytes;
  let u = -1;
  do {
    v /= 1024;
    u++;
  } while (v >= 1024 && u < units.length - 1);
  return `${v.toFixed(1)} ${units[u]}`;
}

/** 渲染「存储」区块（cacheInfo 未加载成功时保持空置） */
function renderCache(): void {
  if (!cacheInfo) return;
  const dirText = byId<HTMLSpanElement>("cache-dir-text");
  dirText.textContent = cacheInfo.webviewDir;
  dirText.title = cacheInfo.webviewDir;
  byId<HTMLSpanElement>("cache-size-text").textContent =
    `WebView2 缓存 ${formatBytes(cacheInfo.webviewSizeBytes)} · 封面 ${formatBytes(cacheInfo.coversSizeBytes)}`;
  byId<HTMLButtonElement>("btn-cache-reset").hidden = !cacheInfo.custom;
  const dataText = byId<HTMLSpanElement>("data-dir-text");
  dataText.textContent = cacheInfo.dataDir;
  dataText.title = cacheInfo.dataDir;
}

/** 「存储」区块行内提示（重启生效提示 / 错误信息） */
function showCacheHint(text: string, isError = false): void {
  const hint = byId<HTMLSpanElement>("cache-dir-hint");
  hint.textContent = text;
  hint.classList.toggle("error", isError);
  hint.hidden = false;
}

/** 提交一次本窗口发起的修改：实时广播给主窗口 + 防抖落盘 */
function commitLocal(): void {
  syncControls();
  void emit("settings-changed", settings).catch(console.error);
  clearTimeout(saveTimer);
  saveTimer = window.setTimeout(() => {
    void api.updateReaderSettings(settings).catch(console.error);
  }, 400);
}

function bindEvents(): void {
  // 正文字体：value 为 CSS 族名，空串 = 跟随图书设定（还原书籍自带 CSS）
  byId<HTMLSelectElement>("set-font-family").addEventListener("change", () => {
    settings.fontFamily = byId<HTMLSelectElement>("set-font-family").value || null;
    commitLocal();
  });

  const onInput = (id: string, apply: (v: number) => void) => {
    byId<HTMLInputElement>(id).addEventListener("input", () => {
      apply(Number(byId<HTMLInputElement>(id).value));
      commitLocal();
    });
  };
  onInput("set-font", (v) => (settings.fontSizePx = v));
  // 字号步进器（−/+）：滑杆快捷微调，步长 1，钳制在滑杆量程内
  const stepFont = (delta: number): void => {
    settings.fontSizePx = Math.min(36, Math.max(12, settings.fontSizePx + delta));
    commitLocal();
  };
  byId<HTMLButtonElement>("font-dec").addEventListener("click", () => stepFont(-1));
  byId<HTMLButtonElement>("font-inc").addEventListener("click", () => stepFont(1));
  onInput("set-line", (v) => (settings.lineHeight = v));
  onInput("set-margin", (v) => (settings.pageMarginPx = v));
  // 每栏最大栏宽：列数自适应（round(可用宽/栏宽)），栏宽≥可用宽度即单栏
  onInput("set-col", (v) => (settings.columnWidthPx = Math.floor(v)));
  // 滚动栏宽：滚动模式正文列的最大宽度（始终居中）
  onInput("set-scroll-col", (v) => (settings.scrollColumnWidthPx = Math.floor(v)));
  // 书本大小：书架卡片缩放（30 = 基准尺寸）
  onInput("set-shelf-size", (v) => (settings.shelfCardSize = Math.floor(v)));
  // 分页强制两栏：开启后无论窗口多宽固定两栏
  byId<HTMLSelectElement>("set-page-turn").addEventListener("change", () => {
    settings.pageTurnEffect = byId<HTMLSelectElement>("set-page-turn").value === "standard" ? "standard" : "natural";
    commitLocal();
  });
  // 滚动效果：自然 = 卷轴卷绕（内容沿顶部虚拟卷轴卷入），标准 = 原生滚动裁剪
  byId<HTMLSelectElement>("set-scroll-effect").addEventListener("change", () => {
    settings.scrollEffect = byId<HTMLSelectElement>("set-scroll-effect").value === "standard" ? "standard" : "natural";
    commitLocal();
  });
  byId<HTMLInputElement>("set-force-two").addEventListener("change", () => {
    settings.twoPageSpread = byId<HTMLInputElement>("set-force-two").checked;
    commitLocal();
  });

  document.querySelectorAll<HTMLButtonElement>(".theme-card").forEach((b) => {
    b.addEventListener("click", () => {
      settings.theme = b.dataset.themePick as ReaderSettings["theme"];
      commitLocal();
    });
  });

  // 默认显示分类：null = 全部，"" = 未分类，其余为分类 id
  byId<HTMLSelectElement>("set-default-category").addEventListener("change", () => {
    settings.defaultCategory = categoryValueToSetting(byId<HTMLSelectElement>("set-default-category").value);
    commitLocal();
  });

  // 书架排序方式
  byId<HTMLSelectElement>("set-shelf-sort").addEventListener("change", () => {
    settings.shelfSortMode = byId<HTMLSelectElement>("set-shelf-sort").value as ReaderSettings["shelfSortMode"];
    commitLocal();
  });

  // 关闭到系统托盘（后端读 settings.json 实时生效，无需重启）
  byId<HTMLInputElement>("set-minimize-tray").addEventListener("change", () => {
    settings.minimizeToTray = byId<HTMLInputElement>("set-minimize-tray").checked;
    commitLocal();
  });

  // 恢复默认：广播后由主窗口自行处理阅读模式变化的章节重载
  byId<HTMLButtonElement>("btn-reset-settings").addEventListener("click", () => {
    settings = { ...DEFAULT_SETTINGS };
    commitLocal();
  });

  // ── 存储：缓存目录与清除缓存 ──
  // 更改缓存目录：选目录 → 校验并写入 prefs → 提示重启生效
  byId<HTMLButtonElement>("btn-cache-dir").addEventListener("click", async () => {
    const picked = await openFileDialog({ directory: true, multiple: false });
    if (typeof picked !== "string") return; // 用户取消
    try {
      await api.setCacheDir(picked);
      cacheInfo = await api.getCacheInfo();
      renderCache();
      showCacheHint("已保存，重启应用后生效");
    } catch (err) {
      showCacheHint(String(err), true);
    }
  });
  // 恢复默认缓存目录
  byId<HTMLButtonElement>("btn-cache-reset").addEventListener("click", async () => {
    try {
      await api.setCacheDir(null);
      cacheInfo = await api.getCacheInfo();
      renderCache();
      showCacheHint("已恢复默认，重启应用后生效");
    } catch (err) {
      showCacheHint(String(err), true);
    }
  });
  // 一键清除：WebView2 浏览数据 + 封面缓存；用返回值刷新占用显示
  byId<HTMLButtonElement>("btn-cache-clear").addEventListener("click", async () => {
    const btn = byId<HTMLButtonElement>("btn-cache-clear");
    btn.disabled = true;
    btn.textContent = "清除中…";
    try {
      cacheInfo = await api.clearCache();
      renderCache();
    } catch (err) {
      console.error("清除缓存失败:", err);
      showCacheHint(String(err), true);
    } finally {
      btn.disabled = false;
      btn.textContent = "清除缓存";
    }
  });
  // 用户数据：一键在系统文件管理器中打开数据目录
  byId<HTMLButtonElement>("btn-open-data-dir").addEventListener("click", async () => {
    try {
      await api.openDataDir();
    } catch (err) {
      showCacheHint(String(err), true);
    }
  });

  // 文件关联：只打开系统入口，不在应用内改注册表
  byId<HTMLButtonElement>("btn-open-file-assoc").addEventListener("click", async () => {
    try {
      await api.openFileDialogAssociationSettings();
    } catch (err) {
      showCacheHint(String(err), true);
    }
  });

  // 自制标题栏：关闭按钮 + Esc
  const close = () => void getCurrentWindow().close();
  byId<HTMLButtonElement>("sw-close").addEventListener("click", close);
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape") close();
  });

  // 圆角窗口：拖拽区双击可最大化/还原，随窗口尺寸同步直角/圆角
  const appWindow = getCurrentWindow();
  const syncMaximized = async () => {
    document.body.classList.toggle("win-maximized", await appWindow.isMaximized());
  };
  void syncMaximized();
  appWindow.onResized(() => void syncMaximized());

  // 主窗口改动（如顶栏切换阅读模式）→ 仅同步显示，不回发
  void listen<ReaderSettings>("settings-changed", (e) => {
    settings = clampSettings({ ...e.payload });
    syncControls();
  });
}

/** 渲染「正文字体」下拉框选项：首项固定「跟随图书设定」，其后为系统字体（按展示名排序） */
function renderFontOptions(fonts: SystemFont[]): void {
  const select = byId<HTMLSelectElement>("set-font-family");
  select.innerHTML = "";
  const options: Array<{ family: string; display: string }> = [
    { family: "", display: "跟随图书设定" },
    ...fonts,
  ];
  // 当前字体已卸载（不在枚举结果中）时追加原样选项，保证回显
  if (settings.fontFamily && !fonts.some((f) => f.family === settings.fontFamily)) {
    options.push({ family: settings.fontFamily, display: settings.fontFamily });
  }
  for (const o of options) {
    const opt = document.createElement("option");
    opt.value = o.family;
    opt.textContent = o.display; // textContent 防字体名注入
    select.appendChild(opt);
  }
  select.value = settings.fontFamily ?? "";
}

/** 渲染「默认显示分类」下拉框：全部 / 未分类 / 自定义分类（按 order 排序） */
function renderCategoryOptions(): void {
  const select = byId<HTMLSelectElement>("set-default-category");
  select.innerHTML = "";
  const options: Array<{ value: string; label: string }> = [
    { value: "all", label: "全部" },
    { value: "uncategorized", label: "未分类" },
    ...[...shelfCategories].sort((a, b) => a.order - b.order).map((c) => ({ value: c.id, label: c.name })),
  ];
  for (const o of options) {
    const opt = document.createElement("option");
    opt.value = o.value;
    opt.textContent = o.label; // textContent 防分类名注入
    select.appendChild(opt);
  }
  // 所存分类已被删除：回退为「全部」（同步修正内存值，随下次落盘写入）
  if (settings.defaultCategory && !shelfCategories.some((c) => c.id === settings.defaultCategory)) {
    settings.defaultCategory = null;
  }
  select.value = settingToCategoryValue(settings.defaultCategory);
}

/** 拉取书架分类并重建「默认显示分类」下拉框（boot 与窗口重新聚焦时调用） */
async function loadCategoryOptions(): Promise<void> {
  try {
    shelfCategories = (await api.getShelf()).categories;
  } catch (err) {
    console.error("书架分类加载失败:", err);
    shelfCategories = [];
  }
  renderCategoryOptions();
}

/** 设置窗口启动入口（main.ts 按窗口 label 调用） */
export async function bootSettingsWindow(): Promise<void> {
  buildDom();
  try {
    settings = clampSettings(await api.loadReaderSettings());
  } catch {
    // 后端已兜底返回默认值，这里仅为极端情况保底
  }
  syncControls();
  bindEvents();
  // 正文字体列表独立加载（枚举失败时仅剩「跟随图书设定」项，不影响其他设置）
  void api
    .listSystemFonts()
    .then(renderFontOptions)
    .catch((err) => console.error("字体列表加载失败:", err));
  // 书架分类列表独立加载（失败时仅剩 全部/未分类 两项）；重新聚焦时刷新，
  // 覆盖「开窗期间用户在书架新建/删除分类」的场景
  void loadCategoryOptions();
  window.addEventListener("focus", () => void loadCategoryOptions());
  // 缓存信息独立加载（失败不影响其他设置项展示）
  try {
    cacheInfo = await api.getCacheInfo();
    renderCache();
  } catch {
    // 保持空置
  }
}
