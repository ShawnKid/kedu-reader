//! 阅读记录与统计 IPC 命令（需求 4/5）。

use std::collections::HashMap;

use chrono::{Duration, Local, TimeZone, Timelike};
use tauri::AppHandle;

use crate::error::{AppError, AppResult};
use crate::model::{
    AnnotationsFile, BookFormat, BookTimeStat, ChapterKind, DailyPoint, DayStat, NameValue,
    StatsSummary,
};
use crate::parser;
use crate::storage;

/// 该书是否不计入阅读统计（隐私排除检查，record_reading_time / record_session 共用）。
fn is_stats_excluded(app: &AppHandle, book_id: &str) -> AppResult<bool> {
    let shelf = storage::load_shelf(app)?;
    Ok(shelf
        .books
        .iter()
        .find(|b| b.id == book_id)
        .is_some_and(crate::model::ShelfBook::is_stats_excluded))
}

/// 心跳上报：前端每 30 秒调用一次，累加到今天的统计（需求 4）。
/// 日期由后端本地时区计算，避免前端时钟/时区不一致。
/// title 随心跳落账：书籍日后移除书架，统计页仍能显示真实书名。
/// 隐私排除：书被设为不计入（或书名命中敏感词且用户未手动设置）则不落账。
#[tauri::command]
pub fn record_reading_time(
    app: AppHandle,
    book_id: String,
    seconds: u64,
    title: Option<String>,
) -> Result<(), AppError> {
    if seconds == 0 {
        return Ok(());
    }
    if is_stats_excluded(&app, &book_id)? {
        return Ok(());
    }
    let date = Local::now().format("%Y-%m-%d").to_string();
    storage::record_reading(&app, &book_id, &date, seconds, title.as_deref())
}

/// 会话上报（会话「形状」落账）：一段连续计费阅读结束时调用一次。
/// 与 record_reading_time 相同的隐私排除检查；时长事实仍由心跳负责，
/// 这里只追加会话（开始时刻/时长/进度起止），供时段分布与阅读速度推导。
#[tauri::command]
pub fn record_session(
    app: AppHandle,
    book_id: String,
    started_at: i64,
    seconds: u64,
    title: Option<String>,
    percent_start: Option<f32>,
    percent_end: Option<f32>,
) -> Result<(), AppError> {
    if seconds == 0 {
        return Ok(());
    }
    if is_stats_excluded(&app, &book_id)? {
        return Ok(());
    }
    let date = Local::now().format("%Y-%m-%d").to_string();
    storage::record_session(
        &app,
        &book_id,
        &date,
        started_at,
        seconds,
        title.as_deref(),
        percent_start,
        percent_end,
    )
}

/// 设置某本书是否计入阅读统计（书架右键，隐私开关）。
/// 手动设置会覆盖敏感词自动判定；设为不计入时同步清除该书已落账的
/// 历史时长、标题与会话（否则统计页仍能看到旧数据，隐私目的落空）。
/// 返回 false 表示书籍不在书架中。
#[tauri::command]
pub fn set_book_stats_excluded(
    app: AppHandle,
    book_id: String,
    excluded: bool,
) -> Result<bool, AppError> {
    let updated = storage::with_shelf(&app, |shelf| {
        let Some(book) = shelf.books.iter_mut().find(|b| b.id == book_id) else {
            return Ok(false);
        };
        book.stats_excluded = Some(excluded);
        Ok(true)
    })?;
    if !updated {
        return Ok(false);
    }
    if excluded {
        storage::with_stats(&app, |stats| {
            let mut empty_days: Vec<String> = Vec::new();
            for (date, day) in stats.days.iter_mut() {
                // 会话与时长同步清除：隐私排除传导到全部落账数据
                day.sessions.retain(|s| s.book_id != book_id);
                if day.per_book.remove(&book_id).is_some() {
                    day.total_seconds = day.per_book.values().sum();
                }
                if day.per_book.is_empty() && day.sessions.is_empty() {
                    empty_days.push(date.clone());
                }
            }
            for d in empty_days {
                stats.days.remove(&d);
            }
            stats.book_titles.remove(&book_id);
            Ok(())
        })?;
    }
    Ok(true)
}

/// 单本书某天的完整统计（调试/详情用，一般走 get_reading_stats）。
#[tauri::command]
pub fn get_day_stat(app: AppHandle, date: String) -> Result<Option<DayStat>, AppError> {
    let stats = storage::load_stats(&app)?;
    Ok(stats.days.get(&date).cloned())
}

/// 聚合统计（需求 5）：今日/本周/本月/累计时长、近 30 天曲线、书籍排行。
/// Rust 端算好聚合结果，前端只做展示，不搬运原始数据。
#[tauri::command]
pub fn get_reading_stats(app: AppHandle) -> Result<StatsSummary, AppError> {
    let stats = storage::load_stats(&app)?;
    let shelf = storage::load_shelf(&app)?;

    let today = Local::now().date_naive();
    let date_str = |d: chrono::NaiveDate| d.format("%Y-%m-%d").to_string();

    // 区间累计：闭区间 [start, today]
    let sum_range = |start: chrono::NaiveDate| -> u64 {
        let mut d = start;
        let mut total = 0u64;
        while d <= today {
            if let Some(day) = stats.days.get(&date_str(d)) {
                total += day.total_seconds;
            }
            d += Duration::days(1);
        }
        total
    };

    let today_seconds = stats.days.get(&date_str(today)).map(|d| d.total_seconds).unwrap_or(0);
    let week_seconds = sum_range(today - Duration::days(6));
    let month_seconds = sum_range(today - Duration::days(29));
    let total_seconds = stats.days.values().map(|d| d.total_seconds).sum();

    // 近 365 天逐日热力图数据（无数据的天补 0，前端画图不用对齐日期）
    let mut daily = Vec::with_capacity(365);
    for i in (0..365).rev() {
        let d = today - Duration::days(i);
        let key = date_str(d);
        let seconds = stats.days.get(&key).map(|s| s.total_seconds).unwrap_or(0);
        daily.push(DailyPoint { date: key, seconds });
    }

    // 书籍排行：跨全部日期聚合 per_book，标题优先书架联查，其次心跳落账的标题
    // （书籍移除书架后仍显示真实书名，而非占位文案）
    let mut per_book: HashMap<String, u64> = HashMap::new();
    for day in stats.days.values() {
        for (id, secs) in &day.per_book {
            *per_book.entry(id.clone()).or_insert(0) += secs;
        }
    }

    // ── 阅读习惯：总阅读天数 / 单日峰值 / 连续天数 / 近 30 天日均 ──
    let total_read_days = stats.days.values().filter(|d| d.total_seconds > 0).count() as u32;
    let peak = stats
        .days
        .values()
        .filter(|d| d.total_seconds > 0)
        .max_by_key(|d| d.total_seconds)
        .cloned();
    let (peak_day_seconds, peak_day_date) = match peak {
        Some(d) => (d.total_seconds, Some(d.date)),
        None => (0, None),
    };
    // 连续天数：从今天往回数；今天还没读则从昨天起算（不断签）
    let has_read = |d: chrono::NaiveDate| -> bool {
        stats
            .days
            .get(&date_str(d))
            .map(|x| x.total_seconds > 0)
            .unwrap_or(false)
    };
    let mut streak_days = 0u32;
    let mut cursor = if has_read(today) { today } else { today - Duration::days(1) };
    while has_read(cursor) {
        streak_days += 1;
        cursor -= Duration::days(1);
    }
    let avg_daily_30 = month_seconds / 30;

    // ── 会话形状：时段桶 / 次数 / 均长 / 最长 / 速度原料 ──
    // 时段桶：会话 [started_at, started_at+seconds) 按本地时间逐小时切分，
    // 跨小时/跨午夜会话精确归桶（比按开始时刻粗分更准）。
    let mut hour_buckets = vec![0u64; 24];
    let mut session_count = 0u64;
    let mut session_total = 0u64;
    let mut session_max = 0u64;
    // 速度原料：book_id → (累计 |Δpercent|, 累计秒)。只累计两端都有 percent 的会话，
    // 之后联查 char_count 折算读字数（char_count 未算/PDF/CBZ 的书自然不参与）。
    let mut deltas: HashMap<String, (f64, u64)> = HashMap::new();
    for day in stats.days.values() {
        for s in &day.sessions {
            if s.seconds == 0 {
                continue;
            }
            session_count += 1;
            session_total += s.seconds;
            session_max = session_max.max(s.seconds);
            // 逐小时切分归桶
            if let Some(mut seg) = Local.timestamp_opt(s.started_at, 0).single() {
                let end_ts = s.started_at + s.seconds as i64;
                while seg.timestamp() < end_ts {
                    let hour = seg.hour() as usize;
                    let hour_end = (seg.timestamp() / 3600 + 1) * 3600;
                    let seg_end = end_ts.min(hour_end);
                    hour_buckets[hour] += (seg_end - seg.timestamp()).max(0) as u64;
                    match Local.timestamp_opt(seg_end, 0).single() {
                        Some(dt) => seg = dt,
                        None => break, // 极端歧义时刻放弃剩余切分；时长账仍在 per_book
                    }
                }
            }
            if let (Some(p0), Some(p1)) = (s.percent_start, s.percent_end) {
                let e = deltas.entry(s.book_id.clone()).or_insert((0.0, 0));
                e.0 += (p1 - p0).abs() as f64;
                e.1 += s.seconds;
            }
        }
    }
    let avg_session_seconds = if session_count > 0 { session_total / session_count } else { 0 };

    // 累计读字数（近似）：Δpercent / 100 × char_count。回头翻书会低估，可接受。
    let mut total_chars_read = 0u64;
    let mut speed_chars = 0u64;
    let mut speed_seconds = 0u64;
    for (id, (delta_pct, secs)) in deltas {
        let Some(cc) = shelf
            .books
            .iter()
            .find(|b| b.id == id)
            .and_then(|b| b.char_count)
        else {
            continue;
        };
        let chars = (delta_pct / 100.0 * cc as f64) as u64;
        total_chars_read += chars;
        speed_chars += chars;
        speed_seconds += secs;
    }
    let avg_speed_cpm = if speed_seconds > 0 {
        Some(speed_chars as f64 / speed_seconds as f64 * 60.0)
    } else {
        None
    };

    // ── 分布：题材（一书多题材按时长均分，不重复计总时长）/ 格式 ──
    let fmt_of = |f: BookFormat| -> &'static str {
        match f {
            BookFormat::Epub => "epub",
            BookFormat::Mobi => "mobi",
            BookFormat::Azw3 => "azw3",
            BookFormat::Txt => "txt",
            BookFormat::Markdown => "markdown",
            BookFormat::Fb2 => "fb2",
            BookFormat::Cbz => "cbz",
            BookFormat::Pdf => "pdf",
        }
    };
    let mut genre_map: HashMap<String, u64> = HashMap::new();
    let mut format_map: HashMap<String, u64> = HashMap::new();
    for (id, secs) in &per_book {
        let Some(b) = shelf.books.iter().find(|b| &b.id == id) else {
            continue;
        };
        if b.genres.is_empty() {
            *genre_map.entry("未标注".to_string()).or_insert(0) += secs;
        } else {
            let share = secs / b.genres.len() as u64; // 整数均分，余数舍去（秒级误差可忽略）
            for g in &b.genres {
                *genre_map.entry(g.clone()).or_insert(0) += share;
            }
        }
        *format_map.entry(fmt_of(b.format).to_string()).or_insert(0) += secs;
    }
    let mut genre_dist: Vec<NameValue> = genre_map
        .into_iter()
        .map(|(name, value)| NameValue { name, value })
        .collect();
    genre_dist.sort_by(|a, b| b.value.cmp(&a.value));
    genre_dist.truncate(10);
    let mut format_dist: Vec<NameValue> = format_map
        .into_iter()
        .map(|(name, value)| NameValue { name, value })
        .collect();
    format_dist.sort_by(|a, b| b.value.cmp(&a.value));

    // ── 笔记与书签总数：遍历 books/*/annotations.json（被移除书架的书也计入） ──
    let mut note_count = 0u64;
    let mut bookmark_count = 0u64;
    let books_dir = storage::app_data_dir_for_commands(&app).join("books");
    if let Ok(entries) = std::fs::read_dir(&books_dir) {
        for e in entries.flatten() {
            let f = e.path().join("annotations.json");
            if !f.is_file() {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&f) else { continue };
            let Ok(a) = serde_json::from_str::<AnnotationsFile>(&raw) else { continue };
            note_count += a.notes.len() as u64;
            bookmark_count += a.bookmarks.len() as u64;
        }
    }

    let title_of = |id: &str| -> String {
        shelf
            .books
            .iter()
            .find(|b| b.id == id)
            .map(|b| b.title.clone())
            .or_else(|| stats.book_titles.get(id).cloned())
            .unwrap_or_else(|| "未在书架的书籍".into())
    };
    let mut top_books: Vec<BookTimeStat> = per_book
        .into_iter()
        .map(|(book_id, seconds)| BookTimeStat {
            title: title_of(&book_id),
            book_id,
            seconds,
        })
        .collect();
    top_books.sort_by(|a, b| b.seconds.cmp(&a.seconds));
    top_books.truncate(10);

    Ok(StatsSummary {
        today_seconds,
        week_seconds,
        month_seconds,
        total_seconds,
        daily,
        top_books,
        book_count: shelf.books.len(),
        finished_count: shelf.books.iter().filter(|b| b.finished).count(),
        streak_days,
        total_read_days,
        peak_day_seconds,
        peak_day_date,
        avg_daily_30,
        hour_buckets,
        session_count,
        avg_session_seconds,
        max_session_seconds: session_max,
        total_chars_read,
        avg_speed_cpm,
        genre_dist,
        format_dist,
        note_count,
        bookmark_count,
    })
}

/// HTML 剥标签后的字符计数（粗粒度：去掉 <> 标签，连续空白按 1 字符计，
/// 避免 EPUB 源码缩进/换行虚增字数）。
fn strip_html_count(html: &str) -> u64 {
    let mut count = 0u64;
    let mut in_tag = false;
    let mut in_ws = false;
    for c in html.chars() {
        if in_tag {
            if c == '>' {
                in_tag = false;
            }
            continue;
        }
        if c.is_whitespace() {
            if !in_ws {
                count += 1;
                in_ws = true;
            }
            continue;
        }
        in_ws = false;
        count += 1;
    }
    count
}

/// 全书字数懒计算（打开书籍后 fire-and-forget 调用，旧书首开补算）。
///
/// 幂等：书架无此书返回 None；已有值直接返回（一次计算终身复用，
/// 不在解析器 open() 里顺手算——每次打开都跑违背性能原则）。
/// 逐章累计：Text 数字符、Html 剥标签后计数、Images（CBZ）计 0；
/// 单章加载失败跳过。PDF 不走 Rust 解析，前端不会对 PDF 调用本命令。
#[tauri::command]
pub async fn compute_book_char_count(
    app: AppHandle,
    book_id: String,
) -> Result<Option<u64>, AppError> {
    let shelf = storage::load_shelf(&app)?;
    let Some(book) = shelf.books.iter().find(|b| b.id == book_id) else {
        return Ok(None);
    };
    if let Some(n) = book.char_count {
        return Ok(Some(n));
    }
    let file_path = book.file_path.clone();

    let counted = tauri::async_runtime::spawn_blocking(move || -> AppResult<Option<u64>> {
        // 打不开（文件丢失/损坏）不算失败，只是没有字数
        let Ok(parsed) = parser::open_book_from_path(&file_path) else {
            return Ok(None);
        };
        let (_meta, _toc, session) = parsed;
        let mut total: u64 = 0;
        for idx in 0..session.chapter_count() {
            let Ok(ch) = session.load_chapter(idx) else {
                continue; // 单章失败跳过，不影响总数
            };
            match ch.kind {
                ChapterKind::Text => {
                    if let Some(t) = ch.text {
                        total += t.chars().count() as u64;
                    }
                }
                ChapterKind::Html => {
                    if let Some(h) = ch.html {
                        total += strip_html_count(&h);
                    }
                }
                ChapterKind::Images => {}
            }
        }
        Ok(Some(total))
    })
    .await
    .map_err(|e| AppError::other(format!("字数统计任务崩溃: {}", e)))??;

    if let Some(total) = counted {
        storage::update_shelf_book(&app, &book_id, |b| b.char_count = Some(total))?;
    }
    Ok(counted)
}

/// 回填 PDF 页数（前端 pdf.js load 拿到 pageCount 后调用；幂等覆盖无妨）。
#[tauri::command]
pub fn set_book_page_count(
    app: AppHandle,
    book_id: String,
    count: u32,
) -> Result<bool, AppError> {
    storage::update_shelf_book(&app, &book_id, |b| b.page_count = Some(count))
}

/// 设置/清除星级评分（0~50，半星步进：45 = 4.5 星；None = 清除）。
/// 同时记录评分时间：年度书单按「年内评过的书」过滤。
/// 读完才可评：未读完的书拒绝新评分（清除与修改已有评分不受限，
/// 避免进度回退后旧评分锁死）；与前端书架菜单的入口限制同规则。
#[tauri::command]
pub fn set_book_rating(
    app: AppHandle,
    book_id: String,
    rating: Option<u8>,
) -> Result<bool, AppError> {
    if let Some(r) = rating {
        if r > 100 {
            return Err(AppError::other("评分取值 0~100（十星制，0.1 星步进）"));
        }
    }
    let shelf = storage::load_shelf(&app)?;
    let Some(book) = shelf.books.iter().find(|b| b.id == book_id) else {
        return Ok(false);
    };
    if rating.is_some() && !book.finished && book.rating.is_none() {
        return Err(AppError::other("读完一本书后才能评分"));
    }
    storage::update_shelf_book(&app, &book_id, |b| {
        b.rating = rating;
        b.rated_at = rating.map(|_| chrono::Utc::now().timestamp());
    })
}

/// 设置书籍题材标签（多选）。标签集由前端 src/genres.ts 预设树定义，
/// 后端只做基础净化：trim、去空、去重、单标签 ≤ 30 字符、总数上限 24。
#[tauri::command]
pub fn set_book_genres(app: AppHandle, book_id: String, genres: Vec<String>) -> Result<bool, AppError> {
    let mut cleaned: Vec<String> = Vec::new();
    for g in genres {
        let g = g.trim().to_string();
        if g.is_empty() || g.chars().count() > 30 || cleaned.contains(&g) {
            continue;
        }
        cleaned.push(g);
        if cleaned.len() >= 24 {
            break;
        }
    }
    let ok = storage::update_shelf_book(&app, &book_id, |b| b.genres = cleaned.clone())?;
    if ok {
        // 广播：主窗口刷新书架卡片，题材窗口同步勾选状态
        use tauri::Emitter;
        let _ = app.emit(
            "genres-changed",
            serde_json::json!({ "bookId": book_id, "genres": cleaned }),
        );
    }
    Ok(ok)
}

/// 打开（或聚焦）题材设置窗口（label = "genre"）。
///
/// 单例语义：窗口已存在时不重复创建，仅通过事件切换书目并聚焦。
/// 书目 id 经 initialization_script 注入 window.__GENRE_BOOK_ID__
/// （自定义协议下 URL query 兼容性无保障，注入脚本时序最可靠）。
///
/// 必须为 async：Windows 上同步命令中 build() 会死锁（同 open_settings_window）。
#[tauri::command]
pub async fn open_genre_window(app: AppHandle, book_id: String) -> Result<(), AppError> {
    use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

    if let Some(win) = app.get_webview_window("genre") {
        let _ = win.emit("genre-book-changed", book_id);
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
        return Ok(());
    }
    let init = format!(
        "window.__GENRE_BOOK_ID__ = {};",
        serde_json::to_string(&book_id).unwrap_or_else(|_| "\"\"".into())
    );
    WebviewWindowBuilder::new(&app, "genre", WebviewUrl::App("index.html".into()))
        .title("设置题材")
        .inner_size(540.0, 680.0)
        .min_inner_size(440.0, 500.0)
        .resizable(true)
        .decorations(false)
        .shadow(false)
        // 窗口真透明：四角圆角由前端 body 圆角绘制
        .transparent(true)
        .initialization_script(&init)
        .center()
        .build()
        .map_err(|e| AppError::other(e.to_string()))?;
    Ok(())
}
