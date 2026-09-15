//! 阅读进度 IPC 命令（需求 1 + 7：JSON 文件持久化，不使用数据库）。

use tauri::AppHandle;

use crate::error::AppError;
use crate::model::{ReaderSettings, ReadingProgress};
use crate::storage::{self, BookProgressFile};

/// 保存进度：progress + 当时使用的阅读设置快照，落盘到 books/<book_id>/progress.json。
#[tauri::command]
pub fn save_progress(
    app: AppHandle,
    progress: ReadingProgress,
    settings: ReaderSettings,
) -> Result<(), AppError> {
    storage::save_progress(&app, &progress, &settings)
}

/// 读取进度：第一次读这本书返回 None（前端从第 1 章开始）。
#[tauri::command]
pub fn load_progress(app: AppHandle, book_id: String) -> Result<Option<BookProgressFile>, AppError> {
    storage::load_progress(&app, &book_id)
}
