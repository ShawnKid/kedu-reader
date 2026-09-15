//! 书籍相关 IPC 命令：open_book / get_toc / get_chapter_content / get_resource / close_book / open_pdf。

use std::path::PathBuf;

use tauri::{AppHandle, State};

use crate::error::{AppError, AppResult};
use crate::model::{BookFormat, BookMeta, ChapterContent, OpenBookResult, ShelfBook, TocItem};
use crate::parser;
use crate::state::{AppState, ResourcePayload};
use crate::storage;

/// 打开书籍（需求 1：open_book(file_path)）。
///
/// 流程：文件读取 + 格式探测 + DRM 检测 + 解析目录 → 入书架/封面 → 注册进内存会话表。
/// 解析与打开后的书架 JSON / 封面读写均放入 blocking 线程池，
/// 避免卡住 Tauri 的 async 运行时（E14）。顺序保持：入书架/封面 → 注册会话 → 返回。
///
/// 安全约束：绝不发起任何网络请求；DRM 书返回 `AppError::DrmDetected`。
#[tauri::command]
pub async fn open_book(
    file_path: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<OpenBookResult, AppError> {
    // spawn_blocking 需要 'static：把路径 move 进闭包
    let result = tauri::async_runtime::spawn_blocking(move || parser::open_book_from_path(&file_path))
        .await
        .map_err(|e| AppError::other(format!("解析任务崩溃: {}", e)))??;

    let (meta, toc, session) = result;
    let session = std::sync::Arc::new(session);

    // 书架登记 + 封面资源读/写是同步 I/O，放入 blocking（E14）。
    // 失败吞吐规则与原先一致：auto_add / save_cover 内部已 best-effort。
    {
        let session = session.clone();
        let meta = meta.clone();
        let app = app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            storage::auto_add_opened_book(&app, &meta);
            if let Some(cover) = &meta.cover_resource {
                if let Ok((_, bytes)) = session.load_resource_bytes(cover) {
                    let _ = storage::save_cover(&app, &meta.id, &bytes);
                }
            }
        })
        .await
        .map_err(|e| AppError::other(format!("书架/封面任务崩溃: {}", e)))?;
    }

    // 同一本书重复打开：insert 会顶掉旧会话，旧 LoadedBook 随之释放
    let session = std::sync::Arc::try_unwrap(session)
        .map_err(|_| AppError::other("打开书籍会话仍被占用"))?;
    state.insert(crate::state::LoadedBook::new(meta.clone(), toc.clone(), session));

    Ok(OpenBookResult { book: meta, toc })
}

/// 获取目录（需求 1：get_toc(book_id)）。
/// open_book 返回体里已带目录；此命令用于丢失前端状态后（如刷新）重新拉取。
#[tauri::command]
pub fn get_toc(book_id: String, state: State<'_, AppState>) -> Result<Vec<TocItem>, AppError> {
    let book = state.get(&book_id)?;
    Ok(book.toc.clone())
}

/// 加载章节内容（需求 1：get_chapter_content(book_id, chapter_index)）。
///
/// `preserve_styles`：「跟随图书设定」时为 true，加载保留书籍自带 CSS 的版本。
/// 两种产出分别缓存（LRU 24 条），互不污染。
#[tauri::command]
pub async fn get_chapter_content(
    book_id: String,
    chapter_index: u32,
    preserve_styles: Option<bool>,
    state: State<'_, AppState>,
) -> Result<ChapterContent, AppError> {
    let book = state.get(&book_id)?;
    let idx = chapter_index as usize;
    if idx >= book.chapter_count() {
        return Err(AppError::ChapterOutOfRange(chapter_index));
    }
    let styled = preserve_styles.unwrap_or(false);
    tauri::async_runtime::spawn_blocking(move || {
        if styled {
            book.chapter_styled(idx)
        } else {
            book.chapter(idx)
        }
    })
    .await
    .map_err(|e| AppError::other(format!("章节加载任务崩溃: {}", e)))?
}

/// 获取书籍内部资源（封面 / CBZ 图片），返回 MIME + base64（需求 3：图片转 base64 走 IPC）。
#[tauri::command]
pub async fn get_resource(
    book_id: String,
    resource_path: String,
    state: State<'_, AppState>,
) -> Result<ResourcePayload, AppError> {
    let book = state.get(&book_id)?;
    tauri::async_runtime::spawn_blocking(move || book.resource(&resource_path))
        .await
        .map_err(|e| AppError::other(format!("资源加载任务崩溃: {}", e)))?
}

/// 关闭书籍（需求 2）：从内存会话表移除。
/// 命令间不持有 Arc 引用，移除后 LoadedBook（ZIP 字节、全文、章节缓存）立即释放。
#[tauri::command]
pub fn close_book(book_id: String, state: State<'_, AppState>) -> Result<bool, AppError> {
    Ok(state.remove(&book_id))
}

/// 构建 PDF 元数据（路径+大小哈希 id，与其他格式同规则保证进度续读）。
///
/// PDF 不进 Rust 解析会话：由前端 pdf.js 渲染，字节走 `book-file://` 协议。
pub fn pdf_meta(file_path: &str) -> AppResult<BookMeta> {
    let path = PathBuf::from(file_path);
    if !path.is_file() {
        return Err(AppError::other(format!("文件不存在: {}", file_path)));
    }
    let format = parser::detect_format(&path)?;
    if format != BookFormat::Pdf {
        return Err(AppError::other("文件不是有效的 PDF"));
    }
    let file_size = std::fs::metadata(&path)?.len();
    Ok(BookMeta {
        id: parser::make_book_id(&path, file_size),
        title: parser::title_from_path(&path),
        author: None,
        publisher: None,
        language: None,
        format: BookFormat::Pdf,
        file_size,
        file_path: path.to_string_lossy().to_string(),
        total_chapters: 0,
        cover_resource: None,
    })
}

/// 确保书籍已登记书架（幂等，已存在则保留原分类/进度）。
///
/// `book-file://` 协议只服务书架内书目（按 id → 登记路径读文件），
/// 所以 PDF 无论「直接打开」还是「书架导入」都必须先登记，否则取不到字节。
pub fn ensure_in_shelf(
    app: &AppHandle,
    book_id: &str,
    title: &str,
    file_path: &str,
    file_size: u64,
) -> AppResult<()> {
    storage::with_shelf(app, |shelf| {
        if shelf.books.iter().any(|b| b.id == book_id) {
            return Ok(());
        }
        shelf.books.push(ShelfBook::new_opened(
            book_id.to_string(),
            title.to_string(),
            None,
            BookFormat::Pdf,
            file_size,
            file_path.to_string(),
            chrono::Utc::now().timestamp(),
        ));
        Ok(())
    })
}

/// 打开 PDF（需求 1）：后端只生成稳定元数据，渲染由前端 pdf.js 完成。
///
/// - book_id 与其他格式同规则（路径+大小哈希），进度续读共用 progress.json；
/// - 自动登记书架（幂等），保证 `book-file://` 协议能按 id 直出字节。
#[tauri::command]
pub async fn open_pdf(app: AppHandle, file_path: String) -> Result<BookMeta, AppError> {
    let meta = tauri::async_runtime::spawn_blocking(move || pdf_meta(&file_path))
        .await
        .map_err(|e| AppError::other(format!("PDF 元数据任务崩溃: {}", e)))??;
    ensure_in_shelf(&app, &meta.id, &meta.title, &meta.file_path, meta.file_size)?;
    Ok(meta)
}
