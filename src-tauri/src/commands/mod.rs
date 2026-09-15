//! Tauri IPC 命令层。
//!
//! 分文件：
//! - `book.rs`        打开/关闭书籍、目录、章节、资源
//! - `progress.rs`    阅读进度持久化
//! - `settings.rs`    阅读器设置持久化
//! - `annotations.rs` 笔记（划线/想法）与书签
//!
//! 所有命令统一返回 `Result<T, AppError>`，前端拿到的错误是可读字符串。

pub mod annotations;
pub mod book;
pub mod progress;
pub mod search;
pub mod settings;
pub mod shelf;
pub mod stats;
pub mod windows;
