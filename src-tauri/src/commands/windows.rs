//! 独立阅读窗口（label = "reader"）与书架窗口（label = "main"）联动。
//!
//! 从书架打开书籍时不关闭书架，另起阅读窗口；阅读窗口尺寸记忆用户
//! 最后一次主动调整的值。点「返回书架」只把书架窗置于阅读窗之上。

use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::error::AppError;
use crate::storage;

/// 默认阅读窗口尺寸（从未主动调整过时使用）。
const DEFAULT_W: f64 = 1000.0;
const DEFAULT_H: f64 = 720.0;

/// 打开（或聚焦）独立阅读窗口，并打开指定书籍。
///
/// 单例语义：窗口已存在时不重复创建，向其广播 `reader-open-book` 切书并聚焦。
/// 新窗口经 initialization_script 注入 `window.__READER_BOOK_PATH__`。
///
/// 必须为 async：Windows 上同步命令中 build() 会死锁（同 open_settings_window）。
#[tauri::command]
pub async fn open_reader_window(app: AppHandle, file_path: String) -> Result<(), AppError> {
    if let Some(win) = app.get_webview_window("reader") {
        let _ = win.emit("reader-open-book", file_path);
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
        return Ok(());
    }

    let size = storage::load_reader_window_size(&app);
    let init = format!(
        "window.__READER_BOOK_PATH__ = {};",
        serde_json::to_string(&file_path).unwrap_or_else(|_| "\"\"".into())
    );

    WebviewWindowBuilder::new(&app, "reader", WebviewUrl::App("index.html".into()))
        .title("阅读")
        .inner_size(
            if size.width > 0.0 { size.width } else { DEFAULT_W },
            if size.height > 0.0 { size.height } else { DEFAULT_H },
        )
        .min_inner_size(640.0, 480.0)
        .resizable(true)
        .decorations(false)
        .shadow(false)
        .transparent(true)
        // 始终屏幕居中：先前相对书架窗 physical 坐标偏移在高 DPI 下单位不一致，会落到右下角
        .center()
        .initialization_script(&init)
        .build()
        .map_err(|e| AppError::other(e.to_string()))?;
    Ok(())
}

/// 阅读窗口点「返回书架」：保存进度后把书架窗还原并置于最上。
/// 不关闭阅读窗。
#[tauri::command]
pub async fn focus_shelf_window(app: AppHandle) -> Result<(), AppError> {
    let Some(win) = app.get_webview_window("main") else {
        return Err(AppError::other("书架窗口不存在"));
    };
    let _ = win.unminimize();
    let _ = win.show();
    let _ = win.set_focus();
    Ok(())
}

/// 记录阅读窗口最近一次用户主动调整的内尺寸。
/// 仅非最大化时由前端在 resize/close 时调用。
#[tauri::command]
pub fn save_reader_window_size(width: f64, height: f64, app: AppHandle) -> Result<(), AppError> {
    if !width.is_finite() || !height.is_finite() {
        return Ok(());
    }
    storage::save_reader_window_size(&app, width, height)
}
