//! 核心数据模型（沿用架构设计文档中的定义）。
//!
//! 所有结构体对前端序列化为 camelCase，与前端 TS 类型一一对应。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 支持的电子书格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BookFormat {
    Epub,
    Mobi,
    Azw3,
    Txt,
    /// Markdown（按 ATX 标题分章，pulldown-cmark 渲染为 HTML）
    Markdown,
    Fb2,
    Cbz,
    /// PDF 由前端 pdf.js 渲染，后端仅提供文件字节与稳定 id
    Pdf,
}

/// 书籍元数据（打开书籍后返回给前端的第一份数据）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookMeta {
    /// 稳定 id：由「文件绝对路径 + 文件大小」哈希得到，
    /// 阅读进度续读依赖它的跨会话稳定性。
    pub id: String,
    pub title: String,
    pub author: Option<String>,
    pub publisher: Option<String>,
    pub language: Option<String>,
    pub format: BookFormat,
    pub file_size: u64,
    pub file_path: String,
    pub total_chapters: u32,
    /// 封面资源的内部路径（如 EPUB 内的 "OEBPS/cover.jpg"），
    /// 前端通过 get_resource 命令按需取 base64，不内联在元数据里。
    pub cover_resource: Option<String>,
}

/// 目录树节点。
/// EPUB：来自 NCX / EPUB3 nav；TXT：正则虚拟目录。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TocItem {
    pub id: String,
    pub label: String,
    /// 对应 spine（或 TXT 虚拟切片）的章节下标。
    pub chapter_index: u32,
    /// 章内锚点：EPUB 为元素 id；TXT 为字符偏移（存成字符串传输）。
    pub anchor: Option<String>,
    pub children: Vec<TocItem>,
}

/// 章节内容类型：决定前端用哪种视图渲染。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChapterKind {
    /// EPUB / FB2 / MOBI：已清洗、图片已内联的 HTML。
    Html,
    /// TXT：纯文本，前端按段落渲染。
    Text,
    /// CBZ：有序图片列表，前端逐张调用 get_resource。
    Images,
}

/// 单章内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChapterContent {
    pub book_id: String,
    pub chapter_index: u32,
    pub title: String,
    pub kind: ChapterKind,
    /// kind == Html 时有效。Rust 侧已完成 ammonia 白名单清洗，
    /// 且 <img> 已改写为 data: URI（base64 内联，无临时磁盘文件）。
    pub html: Option<String>,
    /// kind == Text 时有效。
    pub text: Option<String>,
    /// kind == Images 时有效：ZIP 内部资源路径（有序），前端逐张取。
    pub image_refs: Option<Vec<String>>,
}

/// 阅读进度（每本书一份 JSON，持久化到 app_data_dir/books/<id>/progress.json）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadingProgress {
    pub book_id: String,
    pub chapter_index: u32,
    /// 章内定位：EPUB 元素 id / TXT 字符偏移。None 表示章首。
    pub anchor: Option<String>,
    /// 滚动模式：0.0 ~ 1.0
    pub scroll_ratio: f32,
    /// 分页模式：当前页码（章内）。
    pub page_in_chapter: Option<u32>,
    /// 全书百分比，侧栏展示用。
    pub percent: f32,
    /// Unix 时间戳（秒）。
    pub updated_at: i64,
}

/// 阅读器设置（全局一份 settings.json；同时随进度落盘一份快照用于按书记忆）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReaderSettings {
    pub font_size_px: u16,
    pub line_height: f32,
    pub page_margin_px: u16,
    pub theme: Theme,
    pub reading_mode: ReadingMode,
    /// 分页模式宽屏双页（模拟实体书）。
    pub two_page_spread: bool,
    pub page_turn_effect: PageTurnEffect,
    /// 滚动模式效果：Natural = 卷轴卷绕（上卷轴直径 66px，内容沿顶部虚拟圆柱卷入），
    /// Standard = 原生滚动裁剪。
    pub scroll_effect: PageTurnEffect,
    /// 分页模式分栏目标栏宽（px）。列数由可用宽度自适应：
    /// round(可用宽/栏宽)，可用宽度不足约一栏时自然退化为单栏。
    pub column_width_px: u16,
    /// 滚动模式正文栏宽（px）：内容列的最大宽度，窄于此值时随窗口收缩，始终水平居中。
    pub scroll_column_width_px: u16,
    pub font_family: Option<String>,
    /// 书架默认显示分类：None = 全部；Some("") = 未分类；Some(id) = 自定义分类
    /// id（分类被删后失效，前端回退「全部」）。
    pub default_category: Option<String>,
    /// 书架卡片大小：0~100 滑块，30 = 基准尺寸（实际缩放 = 值/30，前端钳制 0.5~3）。
    pub shelf_card_size: u8,
    /// 书架排序方式（全局设置，设置页「书架设置」选择）。
    pub shelf_sort_mode: ShelfSortMode,
    /// 关闭主窗口/阅读窗时是否缩小到系统托盘（false = 维持原关闭/退出行为）。
    pub minimize_to_tray: bool,
}

impl Default for ReaderSettings {
    fn default() -> Self {
        Self {
            font_size_px: 18,
            line_height: 1.6,
            page_margin_px: 32,
            theme: Theme::Light,
            reading_mode: ReadingMode::Paginated,
            two_page_spread: false,
            page_turn_effect: PageTurnEffect::Standard,
            scroll_effect: PageTurnEffect::Standard,
            column_width_px: 730,
            scroll_column_width_px: 720,
            font_family: None,
            default_category: None,
            shelf_card_size: 30,
            shelf_sort_mode: ShelfSortMode::Recent,
            minimize_to_tray: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageTurnEffect {
    Natural,
    Standard,
}

#[cfg(test)]
mod page_turn_settings_tests {
    use super::{PageTurnEffect, ReaderSettings};

    #[test]
    fn old_settings_default_to_standard_and_round_trips() {
        let old: ReaderSettings = serde_json::from_str(r#"{"fontSizePx":22}"#).unwrap();
        assert_eq!(old.page_turn_effect, PageTurnEffect::Standard);
        assert_eq!(old.scroll_effect, PageTurnEffect::Standard);
        assert_eq!(old.font_size_px, 22);
        let mut settings = old;
        settings.page_turn_effect = PageTurnEffect::Standard;
        settings.scroll_effect = PageTurnEffect::Standard;
        let json = serde_json::to_value(&settings).unwrap();
        assert_eq!(json["pageTurnEffect"], "standard");
        assert_eq!(json["scrollEffect"], "standard");
        let loaded: ReaderSettings = serde_json::from_value(json).unwrap();
        assert_eq!(loaded.page_turn_effect, PageTurnEffect::Standard);
        assert_eq!(loaded.scroll_effect, PageTurnEffect::Standard);
    }
}

/// 书架排序方式（settings.rs 不做 clamp：反序列化即校验，非法值直接报错）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShelfSortMode {
    /// 最近阅读优先（默认；未读过的按添加时间排在后面）
    Recent,
    /// 添加时间新 → 旧
    Added,
    /// 书名拼音序（前端 localeCompare("zh")）
    Title,
    /// 阅读进度高 → 低
    Progress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    Light,
    Dark,
    /// 护眼（米黄底）
    Sepia,
    /// 护眼绿（淡豆沙绿底）
    Green,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadingMode {
    /// 分页模式（模拟实体书翻页）
    Paginated,
    /// 无限滚动模式
    Scroll,
}

/// open_book 命令的返回体。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenBookResult {
    pub book: BookMeta,
    pub toc: Vec<TocItem>,
}

// ────────────────────────── 书架（需求 2/3） ──────────────────────────

/// 书架分类。空分类允许存在；删除分类时其下书籍回到「未分类」（category_id = None）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShelfCategory {
    pub id: String,
    pub name: String,
    /// 展示顺序
    pub order: u32,
}

/// 书架条目（shelf.json 落盘；封面单独存 books/<id>/cover.dat，按字节魔数出 MIME）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShelfBook {
    pub id: String,
    pub title: String,
    pub author: Option<String>,
    pub format: BookFormat,
    pub file_size: u64,
    pub file_path: String,
    /// None = 未分类
    pub category_id: Option<String>,
    pub added_at: i64,
    pub last_read_at: i64,
    /// 最近一次保存的阅读进度（0~100）
    pub percent: f32,
    /// 阅读完标记：percent >= 99.5
    pub finished: bool,
    /// 单本正文字体（书架右键设置；None = 跟随全局，设置后优先于全局字体）。
    #[serde(default)]
    pub font_family: Option<String>,
    /// 统计收录开关（隐私）：None = 未手动设置，按书名敏感词自动判定；
    /// Some(true) = 用户手动设为不计入；Some(false) = 用户手动设为计入（覆盖自动判定）。
    #[serde(default)]
    pub stats_excluded: Option<bool>,
    /// 最近一次跨过 99.5 的时刻（Unix 秒）：读完事件的「状态 + 计数」编码，
    /// 报告可由此得出「某年读完了几本」「从添加到读完用了几天」。
    #[serde(default)]
    pub finished_at: Option<i64>,
    /// 累计读完遍数（每跨一次 99.5 计一次，重读自然编码在计数里）
    #[serde(default)]
    pub finished_times: u32,
    /// 全书字符数（懒计算，打开书籍后 fire-and-forget 补算；None = 未计算，
    /// Some(0) = 已算但无文本）。读字数/阅读速度的分母。
    #[serde(default)]
    pub char_count: Option<u64>,
    /// PDF 页数（前端 pdf.js load 后回填；文字书无稳定「页」概念，恒为 None）
    #[serde(default)]
    pub page_count: Option<u32>,
    /// 星级评分：十星制 0~100，0.1 星步进（值 = 星数×10，87 = 8.7 星）；None = 未评分。
    /// 旧五星制（0~50 半星步进）数值按「值 = 星数×10」同语义，无需迁移。
    /// 记「当前值 + 评分时间」而非历史流：报告书单只需要最终评价。
    #[serde(default)]
    pub rating: Option<u8>,
    /// 最近评分时间（Unix 秒）：年度书单按「年内评过的书」过滤
    #[serde(default)]
    pub rated_at: Option<i64>,
    /// 题材标签（多选，预设树见前端 src/genres.ts；空 = 未标注）。
    /// 手动标注而非解析元数据 subject：中文书 subject 缺失率高，自动分类不可靠。
    #[serde(default)]
    pub genres: Vec<String>,
    /// 旧版单选题材字段（迁移源：加载时并入 genres，不再序列化）。
    #[serde(default, skip_serializing)]
    pub legacy_genre: Option<String>,
}

/// 隐私敏感词：书名命中且用户未手动设置时，自动不计入统计。
/// 需与前端 src/shelf.ts 的 SENSITIVE_WORDS 保持一致。
pub const SENSITIVE_WORDS: &[&str] = &["淫荡", "少妇白洁", "少年阿宾"];

impl ShelfBook {
    /// 新入书架条目的统一默认值（分类未定、进度 0、统计/评分/题材等均为空）。
    /// 调用方只填身份与文件元数据，避免 commands/storage 各写一份字段表。
    pub fn new_opened(
        id: String,
        title: String,
        author: Option<String>,
        format: BookFormat,
        file_size: u64,
        file_path: String,
        added_at: i64,
    ) -> Self {
        Self {
            id,
            title,
            author,
            format,
            file_size,
            file_path,
            category_id: None,
            added_at,
            last_read_at: 0,
            percent: 0.0,
            finished: false,
            font_family: None,
            stats_excluded: None,
            finished_at: None,
            finished_times: 0,
            char_count: None,
            page_count: None,
            rating: None,
            rated_at: None,
            genres: Vec::new(),
            legacy_genre: None,
        }
    }

    /// 该书是否不计入阅读统计：用户手动设置优先，否则按书名敏感词自动判定。
    pub fn is_stats_excluded(&self) -> bool {
        match self.stats_excluded {
            Some(v) => v,
            None => SENSITIVE_WORDS.iter().any(|w| self.title.contains(w)),
        }
    }
}

/// shelf.json 的完整结构。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ShelfData {
    pub categories: Vec<ShelfCategory>,
    pub books: Vec<ShelfBook>,
    /// 「未分类」虚拟分类在侧栏的展示序位：与 categories[].order 同一数轴，
    /// 0 = 排在所有自定义分类之前（旧数据默认，保持原视觉）。
    /// 仅由拖拽排序写入；「未分类」不落盘为分类行。
    pub uncategorized_order: u32,
}

/// 「未分类」虚拟分类的约定 id：只出现在拖拽排序的 id 列表中，
/// 用于在 categories[].order 数轴上为「未分类」占一个序位（不落盘为分类行）。
pub const UNCATEGORIZED_ID: &str = "__uncategorized__";

// ────────────────────────── 阅读时长统计（需求 4/5） ──────────────────────────

/// 单日统计：总时长 + 分书时长（秒）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DayStat {
    /// YYYY-MM-DD（本地日期）
    pub date: String,
    pub total_seconds: u64,
    pub per_book: HashMap<String, u64>,
    /// 阅读会话（形状）：连续计费段，结束时一次性追加。
    /// 时长事实由 total_seconds/per_book（心跳实时落账）保底；
    /// 会话只负责时段分布、阅读速度、「一口气读完」等形状推导，崩溃最多丢最后一段。
    #[serde(default)]
    pub sessions: Vec<SessionEntry>,
}

/// 一段连续计费的阅读会话（会话 = 最小必要粒度，不落逐页/逐 tick）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEntry {
    pub book_id: String,
    /// Unix 秒：会话开始时刻。跨午夜会话归属开始日（时长事实以心跳切日为准）。
    pub started_at: i64,
    /// 会话计费秒数
    pub seconds: u64,
    /// 会话开始时的全书进度（0~100）
    pub percent_start: Option<f32>,
    /// 会话结束时的全书进度。|end − start| / 100 × char_count 即会话读字数
    /// （回头翻书会低估，可接受的近似），除以 seconds 即阅读速度。
    pub percent_end: Option<f32>,
}

/// stats.json：按日期索引的每日统计。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StatsFile {
    pub days: HashMap<String, DayStat>,
    /// book_id → 最近一次心跳上报的标题。移除书架后统计页仍能显示真实书名。
    pub book_titles: HashMap<String, String>,
}

/// get_reading_stats 的聚合结果（Rust 端算好，前端只做展示）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsSummary {
    pub today_seconds: u64,
    pub week_seconds: u64,
    pub month_seconds: u64,
    /// 有记录以来的总时长
    pub total_seconds: u64,
    /// 最近 365 天（含今天，无数据的天补 0）逐日时长（前端日历热力图用）
    pub daily: Vec<DailyPoint>,
    /// 按累计时长排序的书籍排行
    pub top_books: Vec<BookTimeStat>,
    /// 书架总书目数
    pub book_count: usize,
    /// 读完的书目数
    pub finished_count: usize,
    // ── 阅读习惯（全部实时聚合，无新落账） ──
    /// 连续阅读天数：从今天往回数（今天还没读则从昨天起算，不断签）
    pub streak_days: u32,
    /// 有阅读记录的总天数
    pub total_read_days: u32,
    /// 单日峰值时长与日期
    pub peak_day_seconds: u64,
    pub peak_day_date: Option<String>,
    /// 近 30 天日均时长
    pub avg_daily_30: u64,
    // ── 时段分布（由会话推导，不落账） ──
    /// 24 小时时段桶（秒）：会话按本地时间逐小时切分归桶
    pub hour_buckets: Vec<u64>,
    // ── 会话与速度 ──
    /// 会话总数
    pub session_count: u64,
    /// 平均单次会话时长
    pub avg_session_seconds: u64,
    /// 最长单次会话时长
    pub max_session_seconds: u64,
    /// 累计读字数（近似）：Σ |percent_end − percent_start| / 100 × char_count。
    /// 仅统计已有字数的书（PDF/CBZ/未算字数的书自然不参与）；回头翻书会低估。
    pub total_chars_read: u64,
    /// 平均阅读速度（字/分）：参与字数计算的会话总字数 / 总秒数 × 60
    pub avg_speed_cpm: Option<f64>,
    // ── 分布 ──
    /// 题材时长分布（一书多题材按时长均分，降序 Top 10；未标注单独归项）
    pub genre_dist: Vec<NameValue>,
    /// 格式时长分布（epub/txt/pdf…，降序）
    pub format_dist: Vec<NameValue>,
    // ── 笔记与书签 ──
    /// 全库笔记数（遍历 books/*/annotations.json）
    pub note_count: u64,
    /// 全库书签数
    pub bookmark_count: u64,
}

/// 通用「名称-数值」项（分类/格式分布横条用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NameValue {
    pub name: String,
    pub value: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyPoint {
    pub date: String,
    pub seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookTimeStat {
    pub book_id: String,
    pub title: String,
    pub seconds: u64,
}

// ────────────────────────── 笔记与书签（需求 6） ──────────────────────────

/// 划线样式：背景高亮 / 直线下划线 / 波浪线。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoteStyle {
    Highlight,
    Underline,
    Wavy,
}

/// 划线颜色（黄/绿/蓝/粉/紫）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoteColor {
    Yellow,
    Green,
    Blue,
    Pink,
    Purple,
}

/// 一条笔记：对某句话的划线 + 可选的想法文字。
///
/// 定位策略：不存 DOM 结构（跨会话易碎），存「划线文本 + 前后各 ~12 字符的
/// 上下文」的纯文本指纹。前端章节渲染后按指纹在正文里搜索定位并重新包裹，
/// 对 EPUB / TXT / MOBI / FB2 / MD 等所有文字格式通用。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Note {
    /// 前端生成的 uuid
    pub id: String,
    pub book_id: String,
    pub chapter_index: u32,
    /// 被划线的原文
    pub quote: String,
    /// 划线前的上下文（用于同文多处出现时消歧）
    pub prefix: String,
    /// 划线后的上下文
    pub suffix: String,
    pub color: NoteColor,
    pub style: NoteStyle,
    /// 想法内容；None = 纯划线
    pub note: Option<String>,
    /// Unix 时间戳（秒）
    pub created_at: i64,
}

/// 一枚书签。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bookmark {
    pub id: String,
    pub book_id: String,
    /// 文字书：章节下标；PDF：当前页 - 1（与 ReadingProgress 同约定）
    pub chapter_index: u32,
    /// 章内锚点（旧数据 / 兜底定位），可空
    pub anchor: Option<String>,
    /// 分页模式章内页码，可空（指纹跳转的消歧提示 + 兜底）
    pub page_in_chapter: Option<u32>,
    /// 隐形笔记定位指纹：锚点标点后的引文；None = 兜底定位（无文本/旧数据）
    #[serde(default)]
    pub quote: Option<String>,
    /// 锚点标点及其前至多 2 字
    #[serde(default)]
    pub prefix: Option<String>,
    /// quote 后至多 12 字
    #[serde(default)]
    pub suffix: Option<String>,
    /// 书签所在处的一小段文字（列表展示用，可空）
    pub excerpt: String,
    pub created_at: i64,
}

/// books/<book_id>/annotations.json 的完整结构（笔记 + 书签合一份）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AnnotationsFile {
    pub notes: Vec<Note>,
    pub bookmarks: Vec<Bookmark>,
}
