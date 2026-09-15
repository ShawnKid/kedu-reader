//! 持久化模块（需求 7）：纯 JSON 文件，不使用 sqlite / 数据库。
//!
//! 磁盘布局（`tauri::Manager::path().app_data_dir()` 下）：
//! ```text
//! <app_data_dir>/
//! ├── settings.json                    # 全局 ReaderSettings
//! ├── shelf.json                       # 书架（分类 + 书目）
//! ├── stats.json                       # 每日阅读时长统计
//! └── books/<book_id>/progress.json    # 每本书的 ReadingProgress + 设置快照
//! └── books/<book_id>/cover.dat        # 书架封面字节（魔数嗅探 MIME）
//! ```
//!
//! 写入策略：先写临时文件再 rename，保证进程被杀时不会留下半截 JSON。
//! 书架/统计的多命令并发写入用进程内互斥锁串行化，避免读改写竞态。

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::Manager;

use crate::error::{AppError, AppResult};
use crate::model::{
    AnnotationsFile, BookMeta, DayStat, ReaderSettings, ReadingProgress, SessionEntry, ShelfBook,
    ShelfData, StatsFile,
};

/// 书架/统计 JSON 的读改写互斥锁（进程内串行化，跨命令共享）。
static SHELF_STATS_LOCK: Mutex<()> = Mutex::new(());

/// 每本书落盘的完整内容：进度 + 当时使用的阅读配置（按书记忆）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BookProgressFile {
    pub progress: ReadingProgress,
    pub settings: ReaderSettings,
}

impl Default for BookProgressFile {
    fn default() -> Self {
        Self {
            // progress 的其他字段由调用方填充，这里只提供 serde 默认
            progress: ReadingProgress {
                book_id: String::new(),
                chapter_index: 0,
                anchor: None,
                scroll_ratio: 0.0,
                page_in_chapter: None,
                percent: 0.0,
                updated_at: 0,
            },
            settings: ReaderSettings::default(),
        }
    }
}

/// 应用数据目录（tauri::path 提供的 app_data_dir）。
fn app_data_dir(app: &tauri::AppHandle) -> AppResult<PathBuf> {
    app.path()
        .app_data_dir()
        .map_err(|e| AppError::Storage(format!("无法定位应用数据目录: {}", e)))
}

/// app_data_dir 公开访问（缓存统计等只读场景）。
/// 定位失败（几乎不可能）返回空路径：调用方的目录遍历自然得到 0 / 跳过。
pub fn app_data_dir_for_commands(app: &tauri::AppHandle) -> PathBuf {
    app_data_dir(app).unwrap_or_else(|_| PathBuf::new())
}

/// 单本书的进度目录：books/<book_id>/
fn book_dir(app: &tauri::AppHandle, book_id: &str) -> AppResult<PathBuf> {
    // book_id 由我们生成的 hex 哈希，无路径注入风险；仍做一次防御性过滤
    if book_id.contains('/') || book_id.contains('\\') || book_id.contains("..") {
        return Err(AppError::Storage(format!("非法的书籍 id: {}", book_id)));
    }
    Ok(app_data_dir(app)?.join("books").join(book_id))
}

/// 原子写 JSON：tmp 文件 + rename。
fn write_json_atomic<T: Serialize>(path: &PathBuf, value: &T) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| AppError::Storage(format!("JSON 序列化失败: {}", e)))?;
    fs::write(&tmp, json)?;
    // rename 覆盖已存在目标在 Windows 上要求目标不存在或支持替换；
    // std 在 Windows 上对已存在文件会失败，因此先删除旧文件。
    #[cfg(windows)]
    if path.exists() {
        let _ = fs::remove_file(path);
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

// ────────────────────────── 阅读进度 ──────────────────────────

/// 保存进度。settings 快照由前端随进度一并传入（也可以拆成独立命令）。
pub fn save_progress(
    app: &tauri::AppHandle,
    progress: &ReadingProgress,
    settings: &ReaderSettings,
) -> AppResult<()> {
    let path = book_dir(app, &progress.book_id)?.join("progress.json");
    write_json_atomic(
        &path,
        &BookProgressFile {
            progress: progress.clone(),
            settings: settings.clone(),
        },
    )?;
    // 同步书架条目的进度与最近阅读时间（需求 4）：书架卡片直接展示
    let _ = update_shelf_book(app, &progress.book_id, |b| {
        b.percent = progress.percent.clamp(0.0, 100.0);
        // 读完事件化（状态 → 事件）：检测从 <99.5 跨到 >=99.5 的跳变，
        // 记录读完时刻与累计遍数（重读 = 再次跨过，遍数自然递增）。
        let was_finished = b.finished;
        b.finished = b.percent >= 99.5;
        if b.finished && !was_finished {
            b.finished_at = Some(progress.updated_at);
            b.finished_times = b.finished_times.saturating_add(1);
        }
        b.last_read_at = progress.updated_at;
    });
    Ok(())
}

/// 读取进度。文件不存在（第一次读这本书）返回 None。
pub fn load_progress(app: &tauri::AppHandle, book_id: &str) -> AppResult<Option<BookProgressFile>> {
    let path = book_dir(app, book_id)?.join("progress.json");
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| AppError::Storage(format!("读取进度失败: {}", e)))?;
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| AppError::Storage(format!("进度文件损坏: {}", e)))
}

// ────────────────────────── 全局阅读设置 ──────────────────────────

fn settings_path(app: &tauri::AppHandle) -> AppResult<PathBuf> {
    Ok(app_data_dir(app)?.join("settings.json"))
}

/// 保存全局设置。
pub fn save_settings(app: &tauri::AppHandle, settings: &ReaderSettings) -> AppResult<()> {
    write_json_atomic(&settings_path(app)?, settings)
}

/// 读取全局设置；不存在或损坏时返回默认值（保证前端总能拿到可用配置）。
pub fn load_settings(app: &tauri::AppHandle) -> AppResult<ReaderSettings> {
    let path = settings_path(app)?;
    if !path.exists() {
        return Ok(ReaderSettings::default());
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| AppError::Storage(format!("读取设置失败: {}", e)))?;
    match serde_json::from_str(&raw) {
        Ok(s) => Ok(s),
        Err(_) => Ok(ReaderSettings::default()), // 损坏则回退默认，不让阅读器打不开
    }
}

// ────────────────────────── 书架（需求 2/3） ──────────────────────────

fn shelf_path(app: &tauri::AppHandle) -> AppResult<PathBuf> {
    Ok(app_data_dir(app)?.join("shelf.json"))
}

/// 读取整个书架；文件不存在或损坏时返回空书架。
pub fn load_shelf(app: &tauri::AppHandle) -> AppResult<ShelfData> {
    let path = shelf_path(app)?;
    if !path.exists() {
        return Ok(ShelfData::default());
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| AppError::Storage(format!("读取书架失败: {}", e)))?;
    match serde_json::from_str::<ShelfData>(&raw) {
        Ok(mut s) => {
            // 旧版单选 genre 字段的迁移：serde 已把它收进 legacy_genre，这里并入 genres
            for b in &mut s.books {
                if b.genres.is_empty() {
                    if let Some(g) = b.legacy_genre.take() {
                        b.genres.push(g);
                    }
                }
            }
            Ok(s)
        }
        Err(_) => Ok(ShelfData::default()),
    }
}

/// 写回整个书架（调用方需已在 SHELF_STATS_LOCK 保护内做读改写）。
pub fn save_shelf(app: &tauri::AppHandle, shelf: &ShelfData) -> AppResult<()> {
    write_json_atomic(&shelf_path(app)?, shelf)
}

/// 在锁保护下对书架做一次读改写事务。
pub fn with_shelf<T>(
    app: &tauri::AppHandle,
    f: impl FnOnce(&mut ShelfData) -> AppResult<T>,
) -> AppResult<T> {
    let _guard = SHELF_STATS_LOCK
        .lock()
        .map_err(|_| AppError::Storage("书架锁中毒".into()))?;
    let mut shelf = load_shelf(app)?;
    let out = f(&mut shelf)?;
    save_shelf(app, &shelf)?;
    Ok(out)
}

/// 按 id 修改单本书目（不存在则静默忽略——书架是可选功能，进度保存不该因它失败）。
pub fn update_shelf_book(
    app: &tauri::AppHandle,
    book_id: &str,
    f: impl FnOnce(&mut ShelfBook),
) -> AppResult<bool> {
    with_shelf(app, |shelf| {
        let Some(book) = shelf.books.iter_mut().find(|b| b.id == book_id) else {
            return Ok(false);
        };
        f(book);
        Ok(true)
    })
}

/// 打开成功即自动入书架（幂等：已在书架则跳过，分类/进度不丢）。
/// 返回 true 表示是新插入的书（调用方随后提取封面落盘）。
pub fn auto_add_opened_book(app: &tauri::AppHandle, meta: &BookMeta) -> bool {
    with_shelf(app, |shelf| {
        if shelf.books.iter().any(|b| b.id == meta.id) {
            return Ok(false);
        }
        shelf.books.push(ShelfBook::new_opened(
            meta.id.clone(),
            meta.title.clone(),
            meta.author.clone(),
            meta.format,
            meta.file_size,
            meta.file_path.clone(),
            chrono::Utc::now().timestamp(),
        ));
        Ok(true)
    })
    .unwrap_or(false) // 书架写失败不阻断打开书籍
}

/// 书目封面落盘路径：books/<book_id>/cover.dat。
pub fn cover_path(app: &tauri::AppHandle, book_id: &str) -> AppResult<PathBuf> {
    Ok(book_dir(app, book_id)?.join("cover.dat"))
}

/// 更新提取封面；字节一致时跳过。用户指定的封面单独保存，不受影响。
pub fn save_cover(
    app: &tauri::AppHandle,
    book_id: &str,
    bytes: &[u8],
) -> AppResult<bool> {
    let path = cover_path(app, book_id)?;
    if fs::read(&path).ok().as_deref() == Some(bytes) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, bytes)?;
    Ok(true)
}

/// 书目「无封面」标记：存在时 shelf-cover 直接 404，书架走格式占位图。
pub fn cover_none_marker(app: &tauri::AppHandle, book_id: &str) -> AppResult<PathBuf> {
    Ok(book_dir(app, book_id)?.join("cover-none"))
}

/// 清除「无封面」标记（选用默认/内置/自定义封面时）。
pub fn clear_cover_none(app: &tauri::AppHandle, book_id: &str) -> AppResult<()> {
    let marker = cover_none_marker(app, book_id)?;
    if marker.exists() {
        let _ = fs::remove_file(marker);
    }
    Ok(())
}

/// 读封面字节（shelf-cover 协议用）。存在 cover-none 标记时视为无封面。
pub fn load_cover(app: &tauri::AppHandle, book_id: &str) -> AppResult<Vec<u8>> {
    if cover_none_marker(app, book_id)?.exists() {
        return Err(AppError::ResourceNotFound(format!("封面: {}", book_id)));
    }
    let custom = book_dir(app, book_id)?.join("cover-custom.dat");
    if custom.exists() {
        return Ok(fs::read(custom)?);
    }
    let path = cover_path(app, book_id)?;
    if !path.exists() {
        return Err(AppError::ResourceNotFound(format!("封面: {}", book_id)));
    }
    Ok(fs::read(&path)?)
}

// ────────────────────────── 阅读时长统计（需求 4/5） ──────────────────────────

fn stats_path(app: &tauri::AppHandle) -> AppResult<PathBuf> {
    Ok(app_data_dir(app)?.join("stats.json"))
}

/// 读取统计数据；文件不存在或损坏时返回空。
pub fn load_stats(app: &tauri::AppHandle) -> AppResult<StatsFile> {
    let path = stats_path(app)?;
    if !path.exists() {
        return Ok(StatsFile::default());
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| AppError::Storage(format!("读取统计失败: {}", e)))?;
    match serde_json::from_str(&raw) {
        Ok(s) => Ok(s),
        Err(_) => Ok(StatsFile::default()),
    }
}

/// 在锁保护下对统计做一次读改写事务（record_reading 的并发安全由它保证）。
pub fn with_stats<T>(
    app: &tauri::AppHandle,
    f: impl FnOnce(&mut StatsFile) -> AppResult<T>,
) -> AppResult<T> {
    let _guard = SHELF_STATS_LOCK
        .lock()
        .map_err(|_| AppError::Storage("统计锁中毒".into()))?;
    let mut stats = load_stats(app)?;
    let out = f(&mut stats)?;
    write_json_atomic(&stats_path(app)?, &stats)?;
    Ok(out)
}

/// 累加某本书今天的阅读秒数（需求 4）。title 用于移除书架后统计页仍显示真实书名。
pub fn record_reading(
    app: &tauri::AppHandle,
    book_id: &str,
    date: &str,
    seconds: u64,
    title: Option<&str>,
) -> AppResult<()> {
    with_stats(app, |stats| {
        let day = stats.days.entry(date.to_string()).or_insert_with(|| DayStat {
            date: date.to_string(),
            total_seconds: 0,
            per_book: std::collections::HashMap::new(),
            sessions: Vec::new(),
        });
        day.total_seconds += seconds;
        *day.per_book.entry(book_id.to_string()).or_insert(0) += seconds;
        if let Some(t) = title {
            if !t.trim().is_empty() {
                stats.book_titles.insert(book_id.to_string(), t.to_string());
            }
        }
        Ok(())
    })
}

/// 追加一条阅读会话（会话「形状」落账，结束时调用一次）。
/// 与 record_reading 的分工：时长事实由心跳实时落账（崩溃最多丢 30s），
/// 会话只在结束时追加（崩溃最多丢最后一段的形状），冗余换安全。
/// 隐私排除检查由命令层完成后才调用到这里。
pub fn record_session(
    app: &tauri::AppHandle,
    book_id: &str,
    date: &str,
    started_at: i64,
    seconds: u64,
    title: Option<&str>,
    percent_start: Option<f32>,
    percent_end: Option<f32>,
) -> AppResult<()> {
    with_stats(app, |stats| {
        let day = stats.days.entry(date.to_string()).or_insert_with(|| DayStat {
            date: date.to_string(),
            total_seconds: 0,
            per_book: std::collections::HashMap::new(),
            sessions: Vec::new(),
        });
        day.sessions.push(SessionEntry {
            book_id: book_id.to_string(),
            started_at,
            seconds,
            percent_start,
            percent_end,
        });
        if let Some(t) = title {
            if !t.trim().is_empty() {
                stats.book_titles.insert(book_id.to_string(), t.to_string());
            }
        }
        Ok(())
    })
}

// ────────────────────────── 笔记与书签（需求 6） ──────────────────────────

/// 笔记/书签 JSON 的读改写互斥锁（与书架锁同思路，进程内串行化）。
static ANNOTATIONS_LOCK: Mutex<()> = Mutex::new(());

fn annotations_path(app: &tauri::AppHandle, book_id: &str) -> AppResult<PathBuf> {
    Ok(book_dir(app, book_id)?.join("annotations.json"))
}

/// 读取某本书的全部笔记与书签；文件不存在（从未做过笔记）返回空。
pub fn load_annotations(app: &tauri::AppHandle, book_id: &str) -> AppResult<AnnotationsFile> {
    let path = annotations_path(app, book_id)?;
    if !path.exists() {
        return Ok(AnnotationsFile::default());
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| AppError::Storage(format!("读取笔记失败: {}", e)))?;
    match serde_json::from_str(&raw) {
        Ok(a) => Ok(a),
        Err(_) => Ok(AnnotationsFile::default()),
    }
}

/// 整体保存某本书的笔记与书签（前端是唯一写方，整文件覆盖即可）。
pub fn save_annotations(
    app: &tauri::AppHandle,
    book_id: &str,
    annotations: &AnnotationsFile,
) -> AppResult<()> {
    let _guard = ANNOTATIONS_LOCK
        .lock()
        .map_err(|_| AppError::Storage("笔记锁中毒".into()))?;
    write_json_atomic(&annotations_path(app, book_id)?, annotations)
}

// ────────────────────────── 应用偏好（缓存目录等） ──────────────────────────

/// 应用标识（与 tauri.conf.json identifier 同步；启动早期没有 AppHandle，
/// 需要手动定位 app_data_dir 读取 prefs.json 时使用）。
pub const APP_IDENTIFIER: &str = "net.kedu.reader.desktop";

/// 应用级偏好（prefs.json）。独立于 ReaderSettings：
/// 不进每本书的设置快照、不受设置窗口「恢复默认」影响。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppPrefs {
    /// 自定义缓存目录（WebView2 user data folder）。None = 使用默认位置。
    pub cache_dir: Option<String>,
    /// 阅读窗口最近一次用户主动调整后的内尺寸（物理像素）。
    pub reader_window: Option<ReaderWindowPrefs>,
}

/// 阅读窗口尺寸记忆（仅在非最大化时由用户拖拽边缘/角落写入）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReaderWindowPrefs {
    pub width: f64,
    pub height: f64,
}

impl Default for ReaderWindowPrefs {
    fn default() -> Self {
        Self {
            width: 1000.0,
            height: 720.0,
        }
    }
}

fn prefs_path(app: &tauri::AppHandle) -> AppResult<PathBuf> {
    Ok(app_data_dir(app)?.join("prefs.json"))
}

/// 读取应用偏好；文件不存在或损坏时返回默认值。
pub fn load_prefs(app: &tauri::AppHandle) -> AppResult<AppPrefs> {
    let path = prefs_path(app)?;
    if !path.exists() {
        return Ok(AppPrefs::default());
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| AppError::Storage(format!("读取应用偏好失败: {}", e)))?;
    match serde_json::from_str(&raw) {
        Ok(p) => Ok(p),
        Err(_) => Ok(AppPrefs::default()),
    }
}

/// 保存应用偏好。
pub fn save_prefs(app: &tauri::AppHandle, prefs: &AppPrefs) -> AppResult<()> {
    write_json_atomic(&prefs_path(app)?, prefs)
}

/// 读取阅读窗口尺寸记忆；未记过时返回默认 1000×720。
pub fn load_reader_window_size(app: &tauri::AppHandle) -> ReaderWindowPrefs {
    load_prefs(app)
        .ok()
        .and_then(|p| p.reader_window)
        .unwrap_or_default()
}

/// 写入阅读窗口尺寸记忆（覆盖 cache_dir 等其他字段）。
pub fn save_reader_window_size(app: &tauri::AppHandle, width: f64, height: f64) -> AppResult<()> {
    let mut prefs = load_prefs(app).unwrap_or_default();
    prefs.reader_window = Some(ReaderWindowPrefs {
        width: width.max(640.0),
        height: height.max(480.0),
    });
    save_prefs(app, &prefs)
}

/// 不依赖 AppHandle 的启动早期读取（run() 开头、Builder 之前调用）。
///
/// app_data_dir 在 Windows 上 = `%APPDATA%\<identifier>`，此处手动拼出。
/// 非 Windows 返回 None（自定义缓存目录是 WebView2 专属能力，其他平台 no-op）。
pub fn read_cache_dir_override_sync() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let base = std::env::var_os("APPDATA")?;
        let path = PathBuf::from(base).join(APP_IDENTIFIER).join("prefs.json");
        let raw = fs::read_to_string(path).ok()?;
        let prefs: AppPrefs = serde_json::from_str(&raw).ok()?;
        prefs.cache_dir.and_then(|d| {
            let p = PathBuf::from(d);
            if p.is_absolute() {
                Some(p)
            } else {
                None
            }
        })
    }
    #[cfg(not(windows))]
    {
        None
    }
}
