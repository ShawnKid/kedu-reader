//! 书架 IPC 命令（需求 2/3）：书目增删、分类增删改名、归类。

use tauri::AppHandle;

use crate::error::AppError;
use crate::model::{BookFormat, ShelfBook, ShelfCategory, ShelfData, UNCATEGORIZED_ID};
use crate::parser;
use crate::storage;

static COVER_CHANGE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// 内置候选缓存：协议端为每张缩略图发一次请求，不能每次重解析书文件。
/// key = book_id；value = (文件大小, mtime 秒, 候选字节)。源文件变化时失效。
static COVER_CANDIDATE_CACHE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, (u64, i64, std::sync::Arc<Vec<Vec<u8>>>)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[derive(serde::Serialize)]
pub struct CoverChoice {
    index: usize,
    total: usize,
}

/// 封面切换方向：next 下一张 / prev 上一张（环形回绕）。
#[derive(serde::Deserialize, Default, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum CycleDirection {
    #[default]
    Next,
    Prev,
}

/// 每次从当前实际显示的图片出发前进/后退一项；选择写入独立封面文件，重启后继续。
#[tauri::command]
pub async fn cycle_book_cover(
    app: AppHandle,
    book_id: String,
    direction: Option<CycleDirection>,
) -> Result<CoverChoice, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = COVER_CHANGE_LOCK.lock().map_err(|_| AppError::other("封面操作锁异常"))?;
        // load_cover_candidates 内部校验书架条目
        let candidates = load_cover_candidates(&app, &book_id)?;
        let current = storage::load_cover(&app, &book_id).ok();
        let forward = !matches!(direction, Some(CycleDirection::Prev));
        let index = parser::covers::step_index(&candidates, current.as_deref(), forward)?;
        let target = storage::cover_path(&app, &book_id)?.with_file_name("cover-custom.dat");
        std::fs::create_dir_all(target.parent().unwrap())?;
        let tmp = target.with_extension("tmp");
        std::fs::write(&tmp, &candidates[index])?;
        std::fs::rename(tmp, target)?;
        storage::clear_cover_none(&app, &book_id)?;
        emit_cover_changed(&app, &book_id);
        Ok(CoverChoice { index: index + 1, total: candidates.len() })
    }).await.map_err(|e| AppError::other(format!("封面切换失败: {e}")))?
}

/// 当前 Unix 时间戳（秒）。
fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// 由时间戳生成分类 id（本地生成，无网络；纳秒级精度足够防碰撞）。
fn make_category_id() -> String {
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;
    format!("cat{:016x}", nanos ^ (nanos >> 24))
}

/// 读取整个书架。
#[tauri::command]
pub async fn get_shelf(app: AppHandle) -> Result<ShelfData, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let shelf = storage::load_shelf(&app)?;
        // 旧版本曾用 KF8 头覆盖共享图片基址。每本 MOBI 仅迁移一次，
        // 无法访问书源时保留旧缓存，待书源恢复后重试。
        for book in &shelf.books {
            if !matches!(book.format, BookFormat::Mobi | BookFormat::Azw3) {
                continue;
            }
            let cover = storage::cover_path(&app, &book.id)?;
            let marker = cover.with_extension("mobi-v2");
            if marker.exists() {
                continue;
            }
            if let Ok(bytes) = parser::mobi::extract_cover(std::path::Path::new(&book.file_path)) {
                if let Some(bytes) = bytes {
                    if storage::save_cover(&app, &book.id, &bytes).is_ok() {
                        let _ = std::fs::write(marker, b"2");
                    }
                } else {
                    let _ = std::fs::write(marker, b"none");
                }
            }
        }
        Ok(shelf)
    })
    .await
    .map_err(|e| AppError::other(format!("封面更新任务失败: {e}")))?
}

/// 用户确认的封面独立于提取缓存；清除缓存、重新解析不会覆盖它。
#[tauri::command]
pub fn set_book_cover(app: AppHandle, book_id: String, image_path: Option<String>) -> Result<(), AppError> {
    let _guard = COVER_CHANGE_LOCK.lock().map_err(|_| AppError::other("封面操作锁异常"))?;
    let shelf = storage::load_shelf(&app)?;
    if !shelf.books.iter().any(|b| b.id == book_id) {
        return Err(AppError::other("书籍不在书架中"));
    }
    let target = storage::cover_path(&app, &book_id)?.with_file_name("cover-custom.dat");
    let Some(path) = image_path else {
        if target.exists() { std::fs::remove_file(target)?; }
        storage::clear_cover_none(&app, &book_id)?;
        emit_cover_changed(&app, &book_id);
        return Ok(());
    };
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.take(20 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 20 * 1024 * 1024 || !is_cover_image(&bytes) {
        return Err(AppError::other("请选择 20 MB 以内的 JPG、PNG、GIF 或 WebP 图片"));
    }
    std::fs::create_dir_all(target.parent().unwrap())?;
    let tmp = target.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, target)?;
    storage::clear_cover_none(&app, &book_id)?;
    emit_cover_changed(&app, &book_id);
    Ok(())
}

/// 设为「无封面」：写标记 + 去掉自定义，书架显示系统生成的格式占位图。
#[tauri::command]
pub fn set_book_cover_none(app: AppHandle, book_id: String) -> Result<(), AppError> {
    let _guard = COVER_CHANGE_LOCK.lock().map_err(|_| AppError::other("封面操作锁异常"))?;
    let shelf = storage::load_shelf(&app)?;
    if !shelf.books.iter().any(|b| b.id == book_id) {
        return Err(AppError::other("书籍不在书架中"));
    }
    let custom = storage::cover_path(&app, &book_id)?.with_file_name("cover-custom.dat");
    if custom.exists() {
        std::fs::remove_file(custom)?;
    }
    let marker = storage::cover_none_marker(&app, &book_id)?;
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&marker, b"1")?;
    emit_cover_changed(&app, &book_id);
    Ok(())
}

/// 封面变更广播：主书架窗刷新缩略图（cover-window 写入后同步）。
fn emit_cover_changed(app: &AppHandle, book_id: &str) {
    use tauri::Emitter;
    let _ = app.emit("cover-changed", serde_json::json!({ "bookId": book_id }));
}

/// 拉取某本书的封面候选（内置图；协议端与封面窗共用）。带源文件指纹缓存。
pub fn load_cover_candidates(app: &AppHandle, book_id: &str) -> Result<Vec<Vec<u8>>, AppError> {
    let shelf = storage::load_shelf(app)?;
    let book = shelf
        .books
        .iter()
        .find(|b| b.id == book_id)
        .ok_or_else(|| AppError::other("书籍不在书架中"))?;
    let path = std::path::Path::new(&book.file_path);
    let meta = std::fs::metadata(path)?;
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    {
        let cache = COVER_CANDIDATE_CACHE
            .lock()
            .map_err(|_| AppError::other("封面候选缓存锁异常"))?;
        if let Some((s, m, list)) = cache.get(book_id) {
            if *s == size && *m == mtime {
                return Ok((**list).clone());
            }
        }
    }
    let candidates = parser::covers::load(path)?;
    if let Ok(mut cache) = COVER_CANDIDATE_CACHE.lock() {
        // 简单容量控制：超过 32 本清空重来（封面窗同时只服务一本书）
        if cache.len() >= 32 {
            cache.clear();
        }
        cache.insert(book_id.to_string(), (size, mtime, std::sync::Arc::new(candidates.clone())));
    }
    Ok(candidates)
}

/// 读取系统默认封面字节（cover.dat；不含 cover-custom）。
fn load_default_cover(app: &AppHandle, book_id: &str) -> Result<Option<Vec<u8>>, AppError> {
    let path = storage::cover_path(app, book_id)?;
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(std::fs::read(path)?))
}

/// 更换封面窗口的数据源：内置候选数量 + 当前选中关系。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverOptions {
    pub book_id: String,
    pub title: String,
    /// 书籍格式（无封面占位图上的格式印记）
    pub format: BookFormat,
    /// 系统默认封面是否存在（无封面书/PDF 可能没有）
    pub has_default: bool,
    /// 内置候选张数（书内提取图）
    pub candidate_count: usize,
    /// 当前选中：default / candidate:{index} / custom / none
    pub selection: String,
    /// cover-custom.dat 存在
    pub has_custom: bool,
    /// 当前 custom 是否匹配某张内置候选（匹配时 UI 归入内置区）
    pub custom_is_builtin: bool,
}

/// 枚举封面选项（更换封面独立窗口打开/刷新时调用）。
#[tauri::command]
pub async fn get_book_cover_options(app: AppHandle, book_id: String) -> Result<CoverOptions, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = COVER_CHANGE_LOCK.lock().map_err(|_| AppError::other("封面操作锁异常"))?;
        let shelf = storage::load_shelf(&app)?;
        let book = shelf
            .books
            .iter()
            .find(|b| b.id == book_id)
            .ok_or_else(|| AppError::other("书籍不在书架中"))?
            .clone();
        let candidates = load_cover_candidates(&app, &book_id).unwrap_or_default();
        let has_default = load_default_cover(&app, &book_id)?.is_some();
        let none_on = storage::cover_none_marker(&app, &book_id)?.exists();
        let custom_path = storage::cover_path(&app, &book_id)?.with_file_name("cover-custom.dat");
        let custom = if custom_path.exists() {
            Some(std::fs::read(&custom_path)?)
        } else {
            None
        };
        let custom_is_builtin = custom
            .as_ref()
            .map(|c| candidates.iter().any(|b| b == c))
            .unwrap_or(false);
        let selection = if none_on {
            "none".to_string()
        } else {
            match &custom {
                None => {
                    if has_default {
                        "default".to_string()
                    } else {
                        "none".to_string()
                    }
                }
                Some(bytes) => {
                    if let Some(i) = candidates.iter().position(|b| b == bytes) {
                        format!("candidate:{i}")
                    } else {
                        "custom".to_string()
                    }
                }
            }
        };
        Ok(CoverOptions {
            book_id,
            title: book.title,
            format: book.format,
            has_default,
            candidate_count: candidates.len(),
            selection,
            has_custom: custom.is_some(),
            custom_is_builtin,
        })
    })
    .await
    .map_err(|e| AppError::other(format!("封面选项加载失败: {e}")))?
}

/// 按下标选用书内内置封面（写入 cover-custom.dat，与 cycle 同语义）。
#[tauri::command]
pub async fn set_book_cover_candidate(
    app: AppHandle,
    book_id: String,
    index: usize,
) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = COVER_CHANGE_LOCK.lock().map_err(|_| AppError::other("封面操作锁异常"))?;
        let shelf = storage::load_shelf(&app)?;
        if !shelf.books.iter().any(|b| b.id == book_id) {
            return Err(AppError::other("书籍不在书架中"));
        }
        let candidates = load_cover_candidates(&app, &book_id)?;
        let target_bytes = candidates
            .get(index)
            .ok_or_else(|| AppError::other("封面序号越界"))?;
        let target = storage::cover_path(&app, &book_id)?.with_file_name("cover-custom.dat");
        std::fs::create_dir_all(target.parent().unwrap())?;
        let tmp = target.with_extension("tmp");
        std::fs::write(&tmp, target_bytes)?;
        std::fs::rename(tmp, target)?;
        storage::clear_cover_none(&app, &book_id)?;
        emit_cover_changed(&app, &book_id);
        Ok(())
    })
    .await
    .map_err(|e| AppError::other(format!("设置内置封面失败: {e}")))?
}

/// 打开（或聚焦）更换封面窗口（label = "cover"）。
///
/// 单例语义：窗口已存在时不重复创建，仅通过事件切换书目并聚焦。
/// 书目 id 经 initialization_script 注入 window.__COVER_BOOK_ID__。
///
/// 必须为 async：Windows 上同步命令中 build() 会死锁（同 open_settings_window）。
#[tauri::command]
pub async fn open_cover_window(app: AppHandle, book_id: String) -> Result<(), AppError> {
    use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

    if let Some(win) = app.get_webview_window("cover") {
        let _ = win.emit("cover-book-changed", book_id);
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
        return Ok(());
    }
    let init = format!(
        "window.__COVER_BOOK_ID__ = {};",
        serde_json::to_string(&book_id).unwrap_or_else(|_| "\"\"".into())
    );
    WebviewWindowBuilder::new(&app, "cover", WebviewUrl::App("index.html".into()))
        .title("更换封面")
        .inner_size(520.0, 640.0)
        .min_inner_size(400.0, 480.0)
        .resizable(true)
        .decorations(false)
        .shadow(false)
        .transparent(true)
        .initialization_script(&init)
        .center()
        .build()
        .map_err(|e| AppError::other(e.to_string()))?;
    Ok(())
}

fn is_cover_image(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xff, 0xd8, 0xff])
        || bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")
        || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
}

/// 导入书籍到书架（需求 2 的「+ 添加书籍」）。
///
/// PDF 走轻量分支：不解析（解析器不处理 PDF，渲染在前端 pdf.js），
/// 只构建元数据并登记书架。其他格式：完整解析一次以获得元数据 + 封面 →
/// 封面落盘 → 书目写入 shelf.json。解析会话随即丢弃（不驻留内存）。
#[tauri::command]
pub async fn add_book_to_shelf(app: AppHandle, file_path: String) -> Result<ShelfBook, AppError> {
    // PDF：扩展名直判（大小写不敏感），轻量元数据 + 登记书架
    if file_path.to_ascii_lowercase().ends_with(".pdf") {
        let meta = crate::commands::book::pdf_meta(&file_path)?;
        crate::commands::book::ensure_in_shelf(
            &app,
            &meta.id,
            &meta.title,
            &meta.file_path,
            meta.file_size,
        )?;
        let shelf = storage::load_shelf(&app)?;
        return shelf
            .books
            .into_iter()
            .find(|b| b.id == meta.id)
            .ok_or_else(|| AppError::other("PDF 登记书架失败"));
    }

    let app_for_task = app.clone();
    let parsed = tauri::async_runtime::spawn_blocking(move || {
        parser::open_book_from_path(&file_path)
    })
    .await
    .map_err(|e| AppError::other(format!("解析任务崩溃: {}", e)))??;

    let (meta, _toc, session) = parsed;

    // 封面提取：失败不阻断导入（没有封面的书用格式占位图）
    if let Some(cover) = &meta.cover_resource {
        if let Ok((_, bytes)) = session.load_resource_bytes(cover) {
            let _ = storage::save_cover(&app_for_task, &meta.id, &bytes);
        }
    }
    drop(session); // 立即释放解析会话内存

    let book = ShelfBook::new_opened(
        meta.id.clone(),
        meta.title.clone(),
        meta.author.clone(),
        meta.format,
        meta.file_size,
        meta.file_path.clone(),
        now_secs(),
    );

    // 已存在则返回已有条目（重复导入幂等）
    storage::with_shelf(&app, |shelf| {
        if let Some(existing) = shelf.books.iter().find(|b| b.id == book.id) {
            return Ok(existing.clone());
        }
        shelf.books.push(book.clone());
        Ok(book.clone())
    })
}

/// 从书架移除书目（进度/封面文件保留，重新导入可续读）。
#[tauri::command]
pub fn remove_book_from_shelf(app: AppHandle, book_id: String) -> Result<bool, AppError> {
    storage::with_shelf(&app, |shelf| {
        let before = shelf.books.len();
        shelf.books.retain(|b| b.id != book_id);
        Ok(shelf.books.len() != before)
    })
}

/// 移动书目到分类（category_id = None 表示移回「未分类」）。
#[tauri::command]
pub fn set_book_category(
    app: AppHandle,
    book_id: String,
    category_id: Option<String>,
) -> Result<bool, AppError> {
    if let Some(cid) = &category_id {
        let exists = storage::with_shelf(&app, |shelf| {
            Ok(shelf.categories.iter().any(|c| &c.id == cid))
        })?;
        if !exists {
            return Err(AppError::other("分类不存在"));
        }
    }
    storage::update_shelf_book(&app, &book_id, |b| b.category_id = category_id.clone())
}

/// 设置/清除单本书的正文字体（书架右键「设置本书字体」）。
///
/// `None` = 跟随全局；具体族名覆盖全局字体优先生效；
/// 前端哨兵 `@follow-book`（types.ts::FOLLOW_BOOK_FONT）= 跟随图书设定，
/// 语义由前端解释，后端仅按普通字符串透传落盘。返回是否找到该书。
#[tauri::command]
pub fn set_book_font(
    app: AppHandle,
    book_id: String,
    font_family: Option<String>,
) -> Result<bool, AppError> {
    // 空串/纯空白归一为 None（与 update_reader_settings 的字体校验同规则）
    let font_family = font_family.filter(|f| !f.trim().is_empty());
    storage::update_shelf_book(&app, &book_id, |b| b.font_family = font_family.clone())
}

/// 重命名书架显示书名（只改 shelf.json 条目，不动原文件；阅读页标题仍来自文件元数据）。
#[tauri::command]
pub fn rename_book(app: AppHandle, book_id: String, title: String) -> Result<bool, AppError> {
    let title = title.trim().to_string();
    if title.is_empty() {
        return Err(AppError::other("书名不能为空"));
    }
    storage::update_shelf_book(&app, &book_id, |b| b.title = title.clone())
}

/// 在系统文件管理器中打开书籍文件所在目录并选中该文件。
#[tauri::command]
pub fn reveal_book_in_folder(app: AppHandle, book_id: String) -> Result<(), AppError> {
    let file_path = storage::with_shelf(&app, |shelf| {
        Ok(shelf
            .books
            .iter()
            .find(|b| b.id == book_id)
            .map(|b| b.file_path.clone()))
    })?
    .ok_or_else(|| AppError::other("书籍不在书架中"))?;
    if !std::path::Path::new(&file_path).exists() {
        return Err(AppError::other("文件不存在或已被移动"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // explorer /select 只认反斜杠路径：归一化分隔符（导入路径可能混入正斜杠）
        let path = file_path.replace('/', "\\");
        // 不能用普通 .arg()：std 对含空格的参数会整体加引号并转义内部引号，
        // explorer 解析不了该形式（表现为直接打开「此电脑」）。
        // raw_arg 原样拼接命令行 explorer.exe /select,"F:\a b\书.epub"，
        // 是 explorer 唯一可靠接受的写法；Windows 文件路径不允许含引号字符，无注入面
        std::process::Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{}\"", path))
            .spawn()
            .map_err(|e| AppError::other(format!("无法打开文件目录: {}", e)))?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = file_path;
        Err(AppError::other("当前平台不支持打开文件目录"))
    }
}

/// 新建分类。
#[tauri::command]
pub fn add_category(app: AppHandle, name: String) -> Result<ShelfCategory, AppError> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::other("分类名不能为空"));
    }
    storage::with_shelf(&app, |shelf| {
        if shelf.categories.iter().any(|c| c.name == name) {
            return Err(AppError::other("分类已存在"));
        }
        // 新分类排在末尾：序位取「最大分类序位」与「未分类序位」的较大者 + 1，
        // 避免把「未分类」拖到最底后新分类插到它前面
        let order = shelf
            .categories
            .iter()
            .map(|c| c.order)
            .chain(std::iter::once(shelf.uncategorized_order))
            .max()
            .unwrap_or(0)
            + 1;
        let cat = ShelfCategory { id: make_category_id(), name, order };
        shelf.categories.push(cat.clone());
        Ok(cat)
    })
}

/// 重命名分类。
#[tauri::command]
pub fn rename_category(
    app: AppHandle,
    category_id: String,
    name: String,
) -> Result<bool, AppError> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::other("分类名不能为空"));
    }
    storage::with_shelf(&app, |shelf| {
        match shelf.categories.iter_mut().find(|c| c.id == category_id) {
            Some(c) => {
                c.name = name;
                Ok(true)
            }
            None => Ok(false),
        }
    })
}

/// 删除分类：其下书籍回到「未分类」，不会误删书目。
#[tauri::command]
pub fn remove_category(app: AppHandle, category_id: String) -> Result<bool, AppError> {
    storage::with_shelf(&app, |shelf| {
        let before = shelf.categories.len();
        shelf.categories.retain(|c| c.id != category_id);
        if shelf.categories.len() == before {
            return Ok(false);
        }
        for b in shelf.books.iter_mut() {
            if b.category_id.as_deref() == Some(category_id.as_str()) {
                b.category_id = None;
            }
        }
        Ok(true)
    })
}

/// 拖拽排序分类：ordered_ids 为拖拽后的侧栏分类 id 顺序（含虚拟「未分类」UNCATEGORIZED_ID，
/// 各 id 按列表位次从 1 递增占序）；未出现在列表中的分类按原 order 相对顺序接在末尾。
#[tauri::command]
pub fn reorder_category(app: AppHandle, ordered_ids: Vec<String>) -> Result<bool, AppError> {
    storage::with_shelf(&app, |shelf| {
        // (分类下标, 新 order)：被拖拽的按列表位次排前，其余按原相对顺序接后
        let mut new_orders: Vec<(usize, u32)> = Vec::new();
        let mut dragged: Vec<usize> = Vec::new();
        let mut uncategorized_order: Option<u32> = None;
        for (i, c) in shelf.categories.iter().enumerate() {
            if let Some(pos) = ordered_ids.iter().position(|x| x == &c.id) {
                new_orders.push((i, pos as u32 + 1));
                dragged.push(i);
            }
        }
        // 虚拟「未分类」同样按列表位次占序（不落盘为分类行）
        if let Some(pos) = ordered_ids.iter().position(|x| x == UNCATEGORIZED_ID) {
            uncategorized_order = Some(pos as u32 + 1);
        }
        let mut rest: Vec<usize> = (0..shelf.categories.len())
            .filter(|i| !dragged.contains(i))
            .collect();
        rest.sort_by_key(|&i| shelf.categories[i].order);
        for (k, i) in rest.into_iter().enumerate() {
            new_orders.push((i, ordered_ids.len() as u32 + k as u32 + 1));
        }
        let mut changed = new_orders
            .iter()
            .any(|&(i, order)| shelf.categories[i].order != order);
        for (i, order) in new_orders {
            shelf.categories[i].order = order;
        }
        if let Some(o) = uncategorized_order {
            if shelf.uncategorized_order != o {
                shelf.uncategorized_order = o;
                changed = true;
            }
        }
        Ok(changed)
    })
}

/// 书籍格式显示名（书架卡片角标）。
#[tauri::command]
pub fn format_display_name(format: BookFormat) -> String {
    match format {
        BookFormat::Epub => "EPUB".into(),
        BookFormat::Mobi => "MOBI".into(),
        BookFormat::Azw3 => "AZW3".into(),
        BookFormat::Txt => "TXT".into(),
        BookFormat::Markdown => "MD".into(),
        BookFormat::Fb2 => "FB2".into(),
        BookFormat::Cbz => "CBZ".into(),
        BookFormat::Pdf => "PDF".into(),
    }
}
