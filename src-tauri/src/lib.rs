//! 苛读桌面阅读器 — Tauri v2 Rust 后端库入口。
//!
//! 模块划分：
//! - `error`    AppError 全局错误（序列化为可读字符串给前端）
//! - `model`    核心数据模型（BookMeta/TocItem/ChapterContent/ReadingProgress/ReaderSettings）
//! - `parser`   格式解析：epub / mobi / txt / fb2 / markdown / cbz，DRM 前置拦截
//! - `state`    内存会话：HashMap<BookId, Arc<LoadedBook>> + 章节 LRU 缓存
//! - `storage`  JSON 持久化（进度/设置），基于 app_data_dir
//! - `commands` Tauri IPC 命令层
//!
//! 安全约束：全 crate 无任何网络代码；书籍数据只走「本地文件 → 内存 → IPC」。

pub mod commands;
#[cfg(test)]
mod contracts;
pub mod error;
pub mod model;
pub mod parser;
pub mod state;
pub mod storage;

use tauri::{AppHandle, Manager, WindowEvent};

use state::AppState;

/// 读取「关闭时最小化到系统托盘」设置；读失败按关闭处理（与旧行为一致）。
fn minimize_to_tray_enabled(app: &AppHandle) -> bool {
    storage::load_settings(app)
        .map(|s| s.minimize_to_tray)
        .unwrap_or(false)
}

/// 从托盘恢复：优先显示阅读窗，否则显示书架窗。
fn restore_from_tray(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("reader") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    } else if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

/// 创建系统托盘图标：右键菜单「显示主窗口 / 退出」，双击恢复窗口。
fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::{TrayIconBuilder, TrayIconEvent};

    let show = MenuItem::with_id(app, "tray-show", "显示主窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "tray-quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    let mut builder = TrayIconBuilder::with_id("main-tray")
        .tooltip("苛读")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "tray-show" => restore_from_tray(app),
            "tray-quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::DoubleClick { .. } = event {
                restore_from_tray(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

/// 系统文件关联 / 命令行可接受的书籍扩展名（与 parser::detect_format 一致）。
const ASSOCIATED_BOOK_EXTS: &[&str] = &[
    "epub", "mobi", "prc", "azw", "azw3", "kf8", "pdf", "fb2", "fbz", "cbz", "txt", "log", "md",
    "markdown",
];

/// 判断路径是否像一本可打开的书（仅看扩展名；解析失败由 open_book 路径兜底报错）。
fn is_book_path(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|ext| {
            let ext = ext.to_ascii_lowercase();
            ASSOCIATED_BOOK_EXTS.contains(&ext.as_str())
        })
        .unwrap_or(false)
}

/// 从 argv 提取书籍路径（跳过 exe 自身与以 `-`/`/` 开头的开关参数）。
fn book_path_from_args<I, S>(args: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .skip(1)
        .map(|s| s.as_ref().to_string())
        .find(|a| !a.is_empty() && !a.starts_with('-') && !a.starts_with('/') && is_book_path(a))
}

/// 异步打开（或聚焦）阅读窗并加载指定书籍路径。
///
/// `open_reader_window` 必须 async（Windows 同步命令里建窗会死锁），
/// 因此统一 spawn，避免 setup / single-instance 回调阻塞事件循环。
fn open_book_path_async(app: &AppHandle, file_path: String) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = commands::windows::open_reader_window(handle, file_path).await;
    });
}

/// 已有实例再启动时：无书路径则前置当前窗；有书路径则打开阅读窗。
fn handle_secondary_launch(app: &AppHandle, argv: Vec<String>) {
    match book_path_from_args(&argv) {
        Some(path) => open_book_path_async(app, path),
        None => {
            let win = app
                .get_webview_window("reader")
                .or_else(|| app.get_webview_window("main"));
            if let Some(win) = win {
                let _ = win.unminimize();
                let _ = win.show();
                let _ = win.set_focus();
            }
        }
    }
}

/// `reader-res://` 自定义协议（Q1/Q3 优化核心）：
///
/// 大于 100KB 的 EPUB 图片、CBZ 漫画页不再走「base64 → IPC 序列化 → 前端」
/// 通道，而是以普通 `<img src>` URL 由 webview 原生发起请求：
/// - 零拷贝转发原始字节（Content-Type 直出），无 base64 +33% 体积膨胀；
/// - 不占用 invoke 序列化通道，多图并行加载；
/// - WebView2/WKWebView 自带 HTTP 缓存，翻回上一页秒开。
///
/// URL 形式：`http://reader-res.localhost/{book_id}/{资源路径}`（Windows，
/// 与 convertFileSrc 同规则）或 `reader-res://localhost/...`（其余平台）。
fn register_reader_res_protocol(
    app: tauri::Builder<tauri::Wry>,
) -> tauri::Builder<tauri::Wry> {
    app.register_uri_scheme_protocol("reader-res", |ctx, request| {
        let respond = |status: u16, mime: &str, body: Vec<u8>| -> tauri::http::Response<Vec<u8>> {
            tauri::http::Response::builder()
                .status(status)
                .header("Content-Type", mime)
                // The curl snapshot reads image/font bytes with fetch, unlike
                // ordinary <img> display which does not require CORS.
                .header("Access-Control-Allow-Origin", "*")
                .header("Cache-Control", "max-age=31536000, immutable")
                .body(body)
                .expect("构建 reader-res 响应失败")
        };

        // 解析 /{book_id}/{percent-encoded 资源路径}
        let path = request.uri().path().trim_start_matches('/');
        let Some((book_id, res_path)) = path.split_once('/') else {
            return respond(404, "text/plain", b"missing book id".to_vec());
        };
        let res_path =
            percent_encoding::percent_decode_str(res_path).decode_utf8_lossy().into_owned();
        if res_path.is_empty() || res_path.contains("..") {
            return respond(400, "text/plain", b"bad resource path".to_vec());
        }

        let state = ctx.app_handle().state::<AppState>();
        match state.get(book_id) {
            Ok(book) => match book.resource_bytes(&res_path) {
                Ok((mime, bytes)) => respond(200, &mime, bytes),
                Err(e) => respond(404, "text/plain", e.to_string().into_bytes()),
            },
            Err(e) => respond(404, "text/plain", e.to_string().into_bytes()),
        }
    })
}

/// 封面 MIME 魔数嗅探（cover.dat 存原始图片字节）。
fn sniff_image_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") && bytes.len() > 11 && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "application/octet-stream"
    }
}

/// `shelf-cover://` 协议：书架封面直出。
///
/// URL 路径（Windows 前缀 `http://shelf-cover.localhost/`）：
/// - `/{book_id}`                    当前生效封面（custom 优先，否则默认）
/// - `/{book_id}/default`            系统默认封面（导入时提取的 cover.dat）
/// - `/{book_id}/custom`             用户/内置切换后的独立封面（cover-custom.dat）
/// - `/{book_id}/candidate/{index}`  书内候选内置封面
///
/// book_id 形如 `bk`+hex，不含 `/`；仅按书架登记读取，无目录穿越风险。
fn register_shelf_cover_protocol(
    app: tauri::Builder<tauri::Wry>,
) -> tauri::Builder<tauri::Wry> {
    app.register_uri_scheme_protocol("shelf-cover", |ctx, request| {
        let respond = |status: u16, mime: &str, body: Vec<u8>| -> tauri::http::Response<Vec<u8>> {
            tauri::http::Response::builder()
                .status(status)
                .header("Content-Type", mime)
                .header("Cache-Control", "no-store")
                .body(body)
                .expect("构建 shelf-cover 响应失败")
        };

        let path = request.uri().path().trim_start_matches('/');
        let path = percent_encoding::percent_decode_str(path).decode_utf8_lossy();
        if path.is_empty() || path.contains("..") {
            return respond(400, "text/plain", b"bad book id".to_vec());
        }
        let mut parts = path.split('/');
        let book_id = parts.next().unwrap_or_default();
        if book_id.is_empty() {
            return respond(400, "text/plain", b"bad book id".to_vec());
        }
        let kind = parts.next();
        let bytes = match kind {
            None | Some("") => storage::load_cover(ctx.app_handle(), book_id),
            Some("default") => {
                let path = match storage::cover_path(ctx.app_handle(), book_id) {
                    Ok(p) => p,
                    Err(e) => return respond(404, "text/plain", e.to_string().into_bytes()),
                };
                if !path.exists() {
                    return respond(404, "text/plain", b"no cover".to_vec());
                }
                std::fs::read(path).map_err(Into::into)
            }
            Some("custom") => {
                let path = match storage::cover_path(ctx.app_handle(), book_id) {
                    Ok(p) => p.with_file_name("cover-custom.dat"),
                    Err(e) => return respond(404, "text/plain", e.to_string().into_bytes()),
                };
                if !path.exists() {
                    return respond(404, "text/plain", b"no cover".to_vec());
                }
                std::fs::read(path).map_err(Into::into)
            }
            Some("candidate") => {
                let Some(index) = parts.next().and_then(|s| s.parse::<usize>().ok()) else {
                    return respond(400, "text/plain", b"bad candidate index".to_vec());
                };
                let candidates = match crate::commands::shelf::load_cover_candidates(ctx.app_handle(), book_id)
                {
                    Ok(c) => c,
                    Err(e) => return respond(404, "text/plain", e.to_string().into_bytes()),
                };
                match candidates.into_iter().nth(index) {
                    Some(b) => Ok(b),
                    None => return respond(404, "text/plain", b"candidate not found".to_vec()),
                }
            }
            Some(_) => return respond(400, "text/plain", b"bad cover kind".to_vec()),
        };
        match bytes {
            Ok(bytes) => {
                let mime = sniff_image_mime(&bytes);
                respond(200, mime, bytes)
            }
            Err(_) => respond(404, "text/plain", b"no cover".to_vec()),
        }
    })
}

/// `book-file://` 协议：书源文件字节直出（需求 1 的 PDF 走 pdf.js）。
///
/// URL：`http://book-file.localhost/{book_id}`（Windows）/ `book-file://localhost/{book_id}`。
/// 安全约束：book_id 必须存在于书架（shelf.json），按其登记的 file_path 读文件——
/// 前端无法借该协议读任意本地路径。
///
/// CORS：页面源（http://tauri.localhost）与协议域不同源，而 pdf.js 用 fetch 拉取
/// 文档字节，必须带 Access-Control-Allow-Origin，否则报「Failed to fetch」。
fn register_book_file_protocol(
    app: tauri::Builder<tauri::Wry>,
) -> tauri::Builder<tauri::Wry> {
    app.register_uri_scheme_protocol("book-file", |ctx, request| {
        let respond = |status: u16, mime: &str, body: Vec<u8>| -> tauri::http::Response<Vec<u8>> {
            tauri::http::Response::builder()
                .status(status)
                .header("Content-Type", mime)
                .header("Cache-Control", "no-store")
                // fetch 跨域必需；无凭据请求用 * 即可
                .header("Access-Control-Allow-Origin", "*")
                .header("Access-Control-Allow-Methods", "GET, OPTIONS")
                .header(
                    "Access-Control-Expose-Headers",
                    "Content-Length, Content-Range, Accept-Ranges, Content-Type",
                )
                .body(body)
                .expect("构建 book-file 响应失败")
        };

        // 预检请求直接放行（pdf.js 可能发 OPTIONS 探测）
        if request.method() == tauri::http::Method::OPTIONS {
            return respond(200, "text/plain", Vec::new());
        }

        let book_id = request.uri().path().trim_start_matches('/');
        let book_id = percent_encoding::percent_decode_str(book_id).decode_utf8_lossy();
        let app_handle = ctx.app_handle();
        let file_path = storage::load_shelf(app_handle)
            .ok()
            .and_then(|shelf| shelf.books.into_iter().find(|b| b.id == book_id))
            .map(|b| b.file_path);
        let Some(file_path) = file_path else {
            return respond(404, "text/plain", b"book not in shelf".to_vec());
        };
        match std::fs::read(&file_path) {
            Ok(bytes) => respond(200, "application/octet-stream", bytes),
            Err(e) => respond(404, "text/plain", e.to_string().into_bytes()),
        }
    })
}

/// 启动早期应用自定义缓存目录（仅 Windows）：
///
/// 读 prefs.json 的 cache_dir → 注入 `WEBVIEW2_USER_DATA_FOLDER` 环境变量，
/// WebView2 环境创建时（首次建窗）即使用该目录，main + settings 全部窗口统一生效。
///
/// 必须在 `run()` 最开头、任何线程与 webview 创建之前调用（改目录后重启才生效）。
/// 同时 best-effort 清理旧默认位置的 EBWebView —— 此时 webview 尚未创建、无文件锁，
/// 幂等无害（换回默认目录后旧缓存会随使用重建）。
fn apply_webview_cache_env() {
    #[cfg(windows)]
    {
        if let Some(custom) = storage::read_cache_dir_override_sync() {
            let _ = std::fs::create_dir_all(&custom);
            std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", &custom);
            // 旧默认位置残留清理（仅当默认位置 != 自定义位置）
            if let Some(base) = std::env::var_os("APPDATA") {
                let default_dir = std::path::PathBuf::from(base)
                    .join(storage::APP_IDENTIFIER)
                    .join("EBWebView");
                if default_dir != custom {
                    let _ = std::fs::remove_dir_all(&default_dir);
                }
            }
        }
    }
}

/// Tauri 应用装配：注册状态、插件与全部 IPC 命令。
pub fn run() {
    apply_webview_cache_env();
    let builder = tauri::Builder::default()
        // 必须最先注册：第二次启动（双击 .epub/.mobi）由它转发 argv 后立即退出
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            handle_secondary_launch(app, argv);
        }))
        .plugin(tauri_plugin_dialog::init()) // 原生文件选择对话框
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            // 书籍
            commands::book::open_book,
            commands::book::get_toc,
            commands::book::get_chapter_content,
            commands::book::get_resource,
            commands::book::close_book,
            commands::book::open_pdf,
            // 全文搜索（仅文字书）
            commands::search::search_book,
            // 进度
            commands::progress::save_progress,
            commands::progress::load_progress,
            // 设置
            commands::settings::update_reader_settings,
            commands::settings::load_reader_settings,
            commands::settings::open_settings_window,
            commands::settings::list_system_fonts,
            // 缓存管理（自定义缓存目录 / 清除缓存）
            commands::settings::get_cache_info,
            commands::settings::set_cache_dir,
            commands::settings::clear_cache,
            commands::settings::open_data_dir,
            commands::settings::open_file_association_settings,
            // 书架（需求 2/3）
            commands::shelf::get_shelf,
            commands::shelf::add_book_to_shelf,
            commands::shelf::set_book_cover,
            commands::shelf::set_book_cover_none,
            commands::shelf::cycle_book_cover,
            commands::shelf::get_book_cover_options,
            commands::shelf::set_book_cover_candidate,
            commands::shelf::open_cover_window,
            commands::shelf::remove_book_from_shelf,
            commands::shelf::set_book_category,
            commands::shelf::set_book_font,
            commands::shelf::rename_book,
            commands::shelf::reveal_book_in_folder,
            commands::shelf::add_category,
            commands::shelf::rename_category,
            commands::shelf::remove_category,
            commands::shelf::reorder_category,
            commands::shelf::format_display_name,
            // 阅读记录与统计（需求 4/5）
            commands::stats::record_reading_time,
            commands::stats::record_session,
            commands::stats::set_book_stats_excluded,
            commands::stats::get_day_stat,
            commands::stats::get_reading_stats,
            commands::stats::compute_book_char_count,
            commands::stats::set_book_page_count,
            commands::stats::set_book_rating,
            commands::stats::set_book_genres,
            commands::stats::open_genre_window,
            // 笔记与书签（需求 6）
            commands::annotations::get_annotations,
            commands::annotations::save_annotations,
            // 独立阅读窗口 / 书架联动
            commands::windows::open_reader_window,
            commands::windows::focus_shelf_window,
            commands::windows::save_reader_window_size,
        ])
        // 窗口生命周期：
        // - 书架窗关闭：若阅读窗仍在则改为隐藏（保证「返回书架」可用），否则退出
        // - 开启「关闭到托盘」后：书架/阅读窗关闭改为隐藏进托盘，进程驻留
        // - 阅读窗销毁：释放全部书籍会话（解析会话只服务阅读），不退出应用
        // - 书架窗销毁：清理会话并退出（无阅读窗时）
        .on_window_event(|window, event| match event {
            WindowEvent::CloseRequested { api, .. } if window.label() == "main" => {
                let app = window.app_handle();
                if app.get_webview_window("reader").is_some() {
                    // 阅读中点书架关窗：只隐藏，阅读窗与「返回书架」继续可用
                    api.prevent_close();
                    let _ = window.hide();
                } else if minimize_to_tray_enabled(app) {
                    // 关闭到系统托盘：进程驻留，托盘可恢复/退出
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
            WindowEvent::CloseRequested { api, .. } if window.label() == "reader" => {
                let app = window.app_handle();
                if minimize_to_tray_enabled(app) {
                    // 书架已隐藏/不存在时关阅读窗 → 一并进托盘，避免被强制拉回主窗
                    let main_visible = app
                        .get_webview_window("main")
                        .map(|m| m.is_visible().unwrap_or(false))
                        .unwrap_or(false);
                    if !main_visible {
                        api.prevent_close();
                        let _ = window.hide();
                    }
                }
            }
            WindowEvent::Destroyed => {
                let label = window.label();
                let app = window.app_handle();
                let state: tauri::State<AppState> = app.state();
                if label == "reader" {
                    state.clear();
                    // 阅读窗关掉：若书架曾被隐藏则还原，避免只剩隐藏窗的僵尸进程
                    if let Some(main) = app.get_webview_window("main") {
                        if !main.is_visible().unwrap_or(true) {
                            let _ = main.unminimize();
                            let _ = main.show();
                            let _ = main.set_focus();
                        }
                    } else {
                        app.exit(0);
                    }
                } else if label == "main" {
                    let reader_alive = app.get_webview_window("reader").is_some();
                    if !reader_alive {
                        state.clear();
                        app.exit(0);
                    }
                }
            }
            _ => {}
        })
        // 启动：创建托盘 + 首次启动（资源管理器双击 .epub/.mobi）加载书籍
        .setup(|app| {
            let _ = setup_tray(app.handle());
            let args: Vec<String> = std::env::args().collect();
            if let Some(path) = book_path_from_args(&args) {
                open_book_path_async(app.handle(), path);
            }
            Ok(())
        });
    let builder = register_shelf_cover_protocol(register_reader_res_protocol(builder));
    register_book_file_protocol(builder)
        .run(tauri::generate_context!())
        .expect("Tauri 应用启动失败");
}

#[cfg(test)]
mod file_assoc_tests {
    use super::{book_path_from_args, is_book_path};

    #[test]
    fn recognizes_associated_extensions() {
        assert!(is_book_path(r"C:\书\a.epub"));
        assert!(is_book_path("book.MOBI"));
        assert!(is_book_path("/home/u/book.azw3"));
        assert!(!is_book_path("readme.txt.bak"));
        assert!(!is_book_path(r"C:\no-ext"));
        assert!(!is_book_path(""));
    }

    #[test]
    fn extracts_book_path_from_argv() {
        let path = book_path_from_args([
            r"C:\App\苛读.exe",
            r"C:\Users\me\Documents\三体.epub",
        ]);
        assert_eq!(path.as_deref(), Some(r"C:\Users\me\Documents\三体.epub"));

        // 开关参数被跳过，取第一个书籍路径
        let path = book_path_from_args([
            "kedu-reader",
            "--verbose",
            r"D:\books\foo.mobi",
            r"D:\books\bar.epub",
        ]);
        assert_eq!(path.as_deref(), Some(r"D:\books\foo.mobi"));

        // 仅启动无路径
        assert_eq!(book_path_from_args(["kedu-reader"]), None);
        // 非书文件不认
        assert_eq!(
            book_path_from_args(["kedu-reader", r"C:\temp\note.docx"]),
            None
        );
    }
}
