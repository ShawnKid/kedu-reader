//! 阅读器设置 IPC 命令（需求 1 + 7）+ 缓存管理（自定义缓存目录 / 清除缓存）。

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::error::AppError;
use crate::model::ReaderSettings;
use crate::storage::{self, AppPrefs};

/// 打开（或聚焦）独立设置窗口。
///
/// 单例语义：窗口已存在时不重复创建，仅还原显示并聚焦。
/// 新窗口加载同一前端产物（index.html），前端按窗口 label=settings
/// 分流到 settings-window.ts 渲染设置页。
///
/// 必须为 async：Windows 上 WebView2 初始化依赖主线程消息泵，
/// 同步命令中调用 WebviewWindowBuilder::build() 会死锁，
/// 表现为窗口弹出但内容永久白屏（Tauri 官方文档 Known issues）。
#[tauri::command]
pub async fn open_settings_window(app: AppHandle) -> Result<(), AppError> {
    use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

    if let Some(win) = app.get_webview_window("settings") {
        // 已存在：居中后还原显示并聚焦（与新建窗口行为一致）
        let _ = win.center();
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
        return Ok(());
    }
    WebviewWindowBuilder::new(&app, "settings", WebviewUrl::App("index.html".into()))
        .title("设置")
        .inner_size(420.0, 560.0)
        .min_inner_size(340.0, 420.0)
        .resizable(true)
        .decorations(false)
        // 关闭阴影：Windows 上同时去除无边框窗口左/右/下三边的 DWM 1px 边框线
        .shadow(false)
        // 窗口真透明：四角圆角由前端 body 圆角绘制（配合 CSS body.win-maximized 铺满直角）
        .transparent(true)
        .center()
        .build()
        .map_err(|e| AppError::other(e.to_string()))?;
    Ok(())
}

/// 更新（保存）全局阅读设置：字号/行距/页边距/主题/阅读模式/正文字体等。
#[tauri::command]
pub fn update_reader_settings(app: AppHandle, settings: ReaderSettings) -> Result<(), AppError> {
    // 边界校验（系统边界输入，防御异常值）
    let mut s = settings;
    s.font_size_px = s.font_size_px.clamp(10, 40);
    s.line_height = s.line_height.clamp(1.0, 3.0);
    s.page_margin_px = s.page_margin_px.clamp(0, 120);
    s.scroll_column_width_px = s.scroll_column_width_px.clamp(480, 2000);
    s.shelf_card_size = s.shelf_card_size.clamp(0, 100);
    // 字体名空串/纯空白归一为 None（视为「默认」）
    s.font_family = s.font_family.filter(|f| !f.trim().is_empty());
    storage::save_settings(&app, &s)
}

/// 读取全局阅读设置；首次启动返回默认值。
#[tauri::command]
pub fn load_reader_settings(app: AppHandle) -> Result<ReaderSettings, AppError> {
    storage::load_settings(&app)
}

// ────────────────────────── 系统字体枚举 ──────────────────────────

/// 系统字体族（设置页「正文字体」下拉框数据源）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemFont {
    /// CSS font-family 族名（写入 ReaderSettings.font_family，Chromium 按字体 name 表解析）。
    pub family: String,
    /// 下拉框展示名（优先简体中文名，如「微软雅黑」，回退英文名）。
    pub display: String,
}

/// OpenType name 表记录 ID：16 = Typographic Family，1 = Family。
const NAME_ID_TYPOGRAPHIC_FAMILY: u16 = 16;
const NAME_ID_FAMILY: u16 = 1;
/// Windows 平台语言 ID：英文(美国) / 简体中文。
const LANG_EN_US: u16 = 0x0409;
const LANG_ZH_CN: u16 = 0x0804;
/// 中文字体判定采样码点：「中」「文」（cmap 能映射两者即视为中文字体）
const CJK_SAMPLE_CODEPOINTS: [u32; 2] = [0x4E2D, 0x6587];

/// 判断字体是否覆盖中文（用于列表排序：中文字体排最前）
fn face_has_cjk(face: &ttf_parser::Face) -> bool {
    CJK_SAMPLE_CODEPOINTS
        .iter()
        .all(|&cp| char::from_u32(cp).is_some_and(|c| face.glyph_index(c).is_some()))
}

/// 单个 name 记录的择优状态：(是否来自 ID 16, 文本)。ID 16 优先于 ID 1。
type PickedName = (bool, String);

/// 记录择优：ID 16 总是覆盖；ID 1 仅在空位时写入。
fn pick_name(cur: &mut Option<PickedName>, name_id: u16, text: String) {
    let is_typo = name_id == NAME_ID_TYPOGRAPHIC_FAMILY;
    if cur.as_ref().is_none_or(|(t, _)| is_typo && !*t) {
        *cur = Some((is_typo, text));
    }
}

/// 从单个 face 提取 (族名, 展示名)：族名取英文名（跨 locale 稳定），展示名优先中文名。
/// 只有单语言名时互相回退；两者皆缺返回 None。
fn face_names(face: &ttf_parser::Face) -> Option<(String, String)> {
    let mut en: Option<PickedName> = None;
    let mut zh: Option<PickedName> = None;
    for name in face.names() {
        // 只认 Windows 平台记录：该平台族名必有且编码规范（UTF-16BE）
        if name.platform_id != ttf_parser::PlatformId::Windows {
            continue;
        }
        if name.name_id != NAME_ID_TYPOGRAPHIC_FAMILY && name.name_id != NAME_ID_FAMILY {
            continue;
        }
        let Some(text) = name.to_string() else { continue };
        if text.trim().is_empty() {
            continue;
        }
        match name.language_id {
            LANG_EN_US => pick_name(&mut en, name.name_id, text),
            LANG_ZH_CN => pick_name(&mut zh, name.name_id, text),
            _ => {}
        }
    }
    let family = en
        .as_ref()
        .map(|(_, t)| t.clone())
        .or_else(|| zh.clone().map(|(_, t)| t))?;
    let display = zh
        .map(|(_, t)| t)
        .or_else(|| en.map(|(_, t)| t))
        .unwrap_or_else(|| family.clone());
    Some((family, display))
}

/// 扫描系统字体目录，返回按展示名排序的去重字体族列表。
/// 单文件解析失败自动跳过；全部失败返回空列表（前端回退为仅「默认」项）。
fn scan_system_fonts() -> Vec<SystemFont> {
    // 系统字体目录（WINDIR 缺省 C:\Windows）+ 每用户字体目录（Win10 1809+，可能不存在）
    let mut dirs = Vec::with_capacity(2);
    let win_dir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_string());
    dirs.push(PathBuf::from(win_dir).join("Fonts"));
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local).join(r"Microsoft\Windows\Fonts"));
    }

    // 族名 -> (展示名, 是否中文字体)；map 天然去重（同一族的 Regular/Bold/Italic 与多文件 face 归一）
    let mut fonts: std::collections::HashMap<String, (String, bool)> = std::collections::HashMap::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_font = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| matches!(e.to_ascii_lowercase().as_str(), "ttf" | "otf" | "ttc"))
                .unwrap_or(false);
            if !is_font {
                continue;
            }
            // 内存映射避免整文件读入：解析只触碰 name/cmap 表所在页
            let Ok(file) = fs::File::open(&path) else { continue };
            // SAFETY：文件由本函数打开且只读；字体文件不会被并发修改
            let Ok(mmap) = (unsafe { memmap2::Mmap::map(&file) }) else { continue };
            let data = &mmap[..];
            // ttc 集合取字体数，普通字体视为 1
            let face_count = ttf_parser::fonts_in_collection(data).unwrap_or(1);
            for index in 0..face_count {
                let Ok(face) = ttf_parser::Face::parse(data, index) else { continue };
                if let Some((family, display)) = face_names(&face) {
                    let has_cjk = face_has_cjk(&face);
                    fonts
                        .entry(family)
                        .and_modify(|e| e.1 = e.1 || has_cjk)
                        .or_insert((display, has_cjk));
                }
            }
        }
    }

    let mut list: Vec<(SystemFont, bool)> = fonts
        .into_iter()
        .map(|(family, (display, has_cjk))| (SystemFont { family, display }, has_cjk))
        .collect();
    // 中文字体排最前，组内按展示名排序
    list.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.display.to_lowercase().cmp(&b.0.display.to_lowercase()))
    });
    list.into_iter().map(|(f, _)| f).collect()
}

/// 枚举系统已安装字体（设置页「正文字体」下拉框数据源）。
///
/// 扫描涉及几十~上百个字体文件的磁盘 IO，放 blocking 线程执行避免拖慢 runtime。
#[tauri::command]
pub async fn list_system_fonts() -> Result<Vec<SystemFont>, AppError> {
    tauri::async_runtime::spawn_blocking(scan_system_fonts)
        .await
        .map_err(|e| AppError::other(format!("字体枚举失败: {}", e)))
}

// ────────────────────────── 缓存管理 ──────────────────────────
//
// 磁盘缓存 = WebView2 用户数据目录（reader-res:// 图片 HTTP 缓存等，大头）
//          + 书籍封面 books/<id>/cover.dat（小，可从书源重建）。
// 书架/进度/笔记/统计 JSON 是用户数据，不在此列。

/// 缓存信息（设置页展示）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheInfo {
    /// 当前生效（或重启后生效）的 WebView2 用户数据目录。
    pub webview_dir: String,
    /// WebView2 用户数据目录总占用（递归求和；目录不存在则为 0）。
    pub webview_size_bytes: u64,
    /// 全部封面缓存占用（books/*/cover.dat 求和）。
    pub covers_size_bytes: u64,
    /// 是否为自定义缓存目录（控制前端「恢复默认」按钮显隐）。
    pub custom: bool,
    /// 用户数据目录（app_data_dir：settings/shelf/stats/prefs.json + books/）。
    pub data_dir: String,
}

/// 递归统计目录字节数；任何错误（权限/不存在）按 0 跳过。
fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total += dir_size(&entry.path());
        } else {
            total += meta.len();
        }
    }
    total
}

/// 解析当前 WebView2 用户数据目录：prefs.cache_dir 优先，否则默认 app_data_dir/EBWebView。
fn webview_data_dir(app: &AppHandle, prefs: &AppPrefs) -> PathBuf {
    if let Some(dir) = &prefs.cache_dir {
        return PathBuf::from(dir);
    }
    storage::app_data_dir_for_commands(app).join("EBWebView")
}

/// 封面缓存总占用。
fn covers_size(app: &AppHandle) -> u64 {
    let books = storage::app_data_dir_for_commands(app).join("books");
    let Ok(entries) = fs::read_dir(&books) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| e.path().join("cover.dat"))
        .filter(|p| p.is_file())
        .map(|p| p.metadata().map(|m| m.len()).unwrap_or(0))
        .sum()
}

/// 组装缓存信息。
fn cache_info(app: &AppHandle, prefs: &AppPrefs) -> CacheInfo {
    let dir = webview_data_dir(app, prefs);
    CacheInfo {
        webview_dir: dir.to_string_lossy().into_owned(),
        webview_size_bytes: dir_size(&dir),
        covers_size_bytes: covers_size(app),
        custom: prefs.cache_dir.is_some(),
        data_dir: storage::app_data_dir_for_commands(app).to_string_lossy().into_owned(),
    }
}

/// 读取缓存信息：目录位置 + 各项占用。
#[tauri::command]
pub fn get_cache_info(app: AppHandle) -> Result<CacheInfo, AppError> {
    let prefs = storage::load_prefs(&app)?;
    Ok(cache_info(&app, &prefs))
}

/// 设置自定义缓存目录（WebView2 user data folder）。
///
/// - `Some(path)`：校验目录可创建、可写后写入 prefs.json，重启后生效；
/// - `None`：恢复默认位置，重启后生效。
#[tauri::command]
pub fn set_cache_dir(app: AppHandle, path: Option<String>) -> Result<(), AppError> {
    let mut prefs = storage::load_prefs(&app).unwrap_or_default();
    let Some(path) = path else {
        prefs.cache_dir = None;
        storage::save_prefs(&app, &prefs)?;
        return Ok(());
    };
    let dir = PathBuf::from(&path);
    if !dir.is_absolute() {
        return Err(AppError::Storage(format!("缓存目录必须是绝对路径: {}", path)));
    }
    // 校验：能创建 + 能写删探测文件（拒绝只读盘 / 网络盘等不可写目录）
    fs::create_dir_all(&dir)
        .map_err(|e| AppError::Storage(format!("无法创建缓存目录 {}: {}", path, e)))?;
    let probe = dir.join(".kdir-probe");
    fs::write(&probe, b"ok")
        .map_err(|e| AppError::Storage(format!("缓存目录不可写 {}: {}", path, e)))?;
    let _ = fs::remove_file(&probe);
    prefs.cache_dir = Some(path);
    storage::save_prefs(&app, &prefs)
}

/// 一键清除缓存：清 WebView2 全部浏览数据（含磁盘缓存）+ 删除全部封面缓存，
/// 返回清除后的缓存信息（封面归零；WebView2 目录大小由系统内部回收节奏决定）。
#[tauri::command]
pub fn clear_cache(app: AppHandle) -> Result<CacheInfo, AppError> {
    // main/settings 共享同一 WebView2 环境，从 main 清即全部清
    let win = app
        .get_webview_window("main")
        .ok_or_else(|| AppError::other("主窗口不存在"))?;
    win.clear_all_browsing_data()
        .map_err(|e| AppError::other(format!("清除浏览数据失败: {}", e)))?;

    // 封面缓存：删除 books/*/cover.dat，下次打开对应书时自动重建
    let books = storage::app_data_dir_for_commands(&app).join("books");
    if let Ok(entries) = fs::read_dir(&books) {
        for entry in entries.flatten() {
            let cover = entry.path().join("cover.dat");
            if cover.is_file() {
                let _ = fs::remove_file(&cover);
            }
            let _ = fs::remove_file(cover.with_extension("mobi-v2"));
        }
    }

    let prefs = storage::load_prefs(&app)?;
    Ok(cache_info(&app, &prefs))
}

/// 在系统文件管理器中打开用户数据目录（app_data_dir）。
#[tauri::command]
pub fn open_data_dir(app: AppHandle) -> Result<(), AppError> {
    let dir = storage::app_data_dir_for_commands(&app);
    // 全新安装首次使用时目录可能尚未落盘，先确保存在再打开
    fs::create_dir_all(&dir)
        .map_err(|e| AppError::Storage(format!("无法创建数据目录: {}", e)))?;
    #[cfg(windows)]
    {
        // 参数直传路径（无 shell 解析），路径来自 app_data_dir，无注入面
        std::process::Command::new("explorer")
            .arg(&dir)
            .spawn()
            .map_err(|e| AppError::other(format!("无法打开数据目录: {}", e)))?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        Err(AppError::other("当前平台不支持打开数据目录"))
    }
}

/// 打开 Windows「默认应用」设置入口，由用户手动选择苛读作为 epub/mobi 默认程序。
///
/// 不改写注册表 UserChoice；安装包仅以 rank=None 登记 ProgID。
/// 注：`registeredAppUser` 深链经 explorer 打开会落到文档夹，故只开总页。
#[tauri::command]
pub fn open_file_association_settings(_app: AppHandle) -> Result<(), AppError> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg("ms-settings:defaultapps")
            .spawn()
            .map(|_| ())
            .map_err(|e| AppError::other(format!("无法打开系统默认应用设置: {}", e)))?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err(AppError::other("当前平台不支持打开文件关联设置"))
    }
}
