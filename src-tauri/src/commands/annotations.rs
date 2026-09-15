//! 笔记（划线 + 想法）与书签的 IPC 命令（需求 6）。
//!
//! 数据全部存 books/<book_id>/annotations.json；前端是唯一「编辑方」，
//! 这里只做整文件读写，解析/定位逻辑全在前端。

use tauri::AppHandle;

use crate::error::AppError;
use crate::model::AnnotationsFile;
use crate::storage;

/// 读取某本书的全部笔记与书签（从未做过返回空结构）。
#[tauri::command]
pub fn get_annotations(app: AppHandle, book_id: String) -> Result<AnnotationsFile, AppError> {
    storage::load_annotations(&app, &book_id)
}

/// 整体保存某本书的笔记与书签（前端本地改完后一次性写回）。
#[tauri::command]
pub fn save_annotations(
    app: AppHandle,
    book_id: String,
    annotations: AnnotationsFile,
) -> Result<(), AppError> {
    storage::save_annotations(&app, &book_id, &annotations)
}
