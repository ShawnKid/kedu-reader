//! 全局错误类型。
//!
//! 约束：所有业务函数返回 `Result<T, AppError>`；
//! `AppError` 实现 `serde::Serialize`，前端通过 IPC 拿到的是可读的中文错误字符串。

use serde::{Serialize, Serializer};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("文件读取失败: {0}")]
    Io(#[from] std::io::Error),

    #[error("ZIP 容器解析失败: {0}")]
    Zip(String),

    #[error("XML 解析失败: {0}")]
    Xml(String),

    #[error("文本编码检测失败: {0}")]
    Encoding(String),

    /// DRM 加密电子书：按安全约束直接拒绝解析，绝不尝试绕过。
    #[error("该电子书包含 DRM 加密保护，无法在本地阅读器中打开")]
    DrmDetected,

    #[error("不支持的电子书格式: {0}")]
    UnsupportedFormat(String),

    #[error("书籍未打开或已被关闭: {0}")]
    BookNotOpen(String),

    #[error("章节索引超出范围: {0}")]
    ChapterOutOfRange(u32),

    #[error("书籍资源不存在: {0}")]
    ResourceNotFound(String),

    #[error("本地持久化读写失败: {0}")]
    Storage(String),

    #[error("{0}")]
    Other(String),
}

impl AppError {
    /// 便捷构造，用于各类字符串错误。
    pub fn other<S: Into<String>>(msg: S) -> Self {
        AppError::Other(msg.into())
    }
}

// Tauri IPC 要求错误类型实现 Serialize。
// 这里序列化为纯字符串，前端 `catch(err)` 直接得到可读信息。
impl Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
