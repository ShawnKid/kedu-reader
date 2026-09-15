//! 书本内全文搜索命令（仅文字书；PDF 由前端 pdf.js 渲染、CBZ 纯图片，均不经过本命令）。
//!
//! 设计要点：
//! - 检索在 Rust 侧完成：逐章走 `LoadedBook::chapter_for_search`（命中已有阅读缓存
//!   则复用；未命中解析但不写入阅读缓存，避免挤出当前章），大书不必把全文推过 IPC；
//! - 匹配一致性：前端凭 `quote + prefix + suffix` 指纹在已渲染 DOM 中定位，
//!   因此本模块与前端 `buildSearchIndex` 采用同一套文本归一化规则——
//!   连续空白折叠为单个空格，且**块级标签边界计为一个空格**
//!   （`<p>a</p><p>b</p>` 两边都归一成 `"a b"`，跨段命中才能精确定位）；
//! - 大小写不敏感：两侧都做逐字符小写折叠（带偏移映射），
//!   避免某些字符小写后字节长度变化（如 İ → i̇）导致的偏移错位。

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::error::AppError;
use crate::model::ChapterKind;
use crate::parser::unescape_entities;
use crate::state::{AppState, LoadedBook};

/// 明细收集上限：超过后只继续计数（total 仍准确），不再收集指纹与摘要。
const MAX_SEARCH_MATCHES: usize = 500;
/// 查询词长度上限（字符数）。
const MAX_QUERY_CHARS: usize = 100;
/// prefix / suffix 指纹窗口（字符数，与书签指纹同量级）。
const FINGERPRINT_CHARS: usize = 12;
/// 摘要前后文窗口（字符数，保证上下文可读）。
const CONTEXT_CHARS: usize = 40;

/// 单个命中：前端凭 quote+prefix+suffix 指纹在已渲染 DOM 中精确定位。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchMatch {
    pub chapter_index: u32,
    pub chapter_title: String,
    /// 命中文本（归一化后原样返回，前端定位用）
    pub quote: String,
    /// 命中前 ≤12 字（同章同文多命中消歧用）
    pub prefix: String,
    /// 命中后 ≤12 字
    pub suffix: String,
    /// 摘要前段（≤40 字，截断时以 … 开头）
    pub pre: String,
    /// 摘要后段（≤40 字，截断时以 … 结尾）
    pub post: String,
}

/// search_book 返回体。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    /// 按「章节顺序 + 章内出现顺序」排列的命中明细（最多 MAX_SEARCH_MATCHES 条）
    pub matches: Vec<SearchMatch>,
    /// 截断前真实总数
    pub total: usize,
    pub truncated: bool,
}

/// 全书全文搜索。
#[tauri::command]
pub async fn search_book(
    book_id: String,
    query: String,
    state: State<'_, AppState>,
) -> Result<SearchResult, AppError> {
    let book = state.get(&book_id)?;
    tauri::async_runtime::spawn_blocking(move || Ok(search_in_book(&book, &query)))
        .await
        .map_err(|e| AppError::other(format!("搜索任务崩溃: {}", e)))?
}

/// 对整本书执行检索（blocking 线程调用）。
fn search_in_book(book: &LoadedBook, raw_query: &str) -> SearchResult {
    // 查询词归一化：空白折叠 + 截断；小写折叠一次，避免每章重复（E07）
    let query: String = collapse_ws(raw_query).chars().take(MAX_QUERY_CHARS).collect();
    let (lower_query, _) = fold_with_map(&query);
    let mut matches: Vec<SearchMatch> = Vec::new();
    let mut total: usize = 0;

    if !query.is_empty() && !lower_query.is_empty() {
        for idx in 0..book.chapter_count() {
            search_in_chapter(book, idx, &lower_query, &mut matches, &mut total);
        }
    }

    SearchResult {
        truncated: total > MAX_SEARCH_MATCHES,
        total,
        matches,
    }
}

/// 单章检索：提取归一化纯文本后做大小写不敏感查找。
/// 使用 chapter_for_search：不把全文遍历结果写入阅读缓存（E07）。
fn search_in_chapter(
    book: &LoadedBook,
    index: usize,
    lower_query: &str,
    matches: &mut Vec<SearchMatch>,
    total: &mut usize,
) {
    let Ok(ch) = book.chapter_for_search(index) else {
        return; // 单章加载失败跳过，不影响其他章节
    };
    let hay = match ch.kind {
        ChapterKind::Text => ch.text.as_deref().map(collapse_ws),
        ChapterKind::Html => ch.html.as_deref().map(html_to_plain_text),
        ChapterKind::Images => None,
    };
    let Some(hay) = hay else { return };
    if hay.is_empty() {
        return;
    }

    let title = if ch.title.trim().is_empty() {
        format!("第 {} 章", index + 1)
    } else {
        ch.title.clone()
    };

    // 两侧同规则小写折叠；映射表把小写偏移折算回原文偏移
    let (lower_hay, map) = fold_with_map(&hay);
    if lower_query.is_empty() {
        return;
    }

    let mut from = 0usize;
    while let Some(pos) = lower_hay[from..].find(&lower_query) {
        let lstart = from + pos;
        let lend = lstart + lower_query.len();
        // 命中必须落在「折叠分组」边界上：多字符小写展开（如 İ → i̇）的
        // 组内部分匹配会得到空 quote，跳过该位置继续找
        let start_ok = lstart == 0 || map[lstart] != map[lstart - 1];
        let end_ok = lend == lower_hay.len() || map[lend] != map[lend - 1];
        if !start_ok || !end_ok {
            from = lstart + lower_hay[lstart..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        *total += 1;
        if matches.len() < MAX_SEARCH_MATCHES {
            let start = map[lstart];
            let end = map[lend];
            let quote = hay[start..end].to_string();
            let prefix = char_tail(&hay[..start], FINGERPRINT_CHARS);
            let suffix = char_head(&hay[end..], FINGERPRINT_CHARS);
            let (pre, post) = excerpt(&hay, start, end, CONTEXT_CHARS);
            matches.push(SearchMatch {
                chapter_index: index as u32,
                chapter_title: title.clone(),
                quote,
                prefix,
                suffix,
                pre,
                post,
            });
        }
        from = lend;
    }
}

/// 逐字符小写折叠：返回小写文本 + 「小写字节偏移 → 原文字节偏移」映射。
/// 小写展开可能改变字节长度（如 İ → i̇），映射按小写字节逐字节记录；
/// 末项映射到原文末尾，保证 lend==lower.len() 时也能取到原文边界。
fn fold_with_map(hay: &str) -> (String, Vec<usize>) {
    let mut lower = String::with_capacity(hay.len());
    let mut map: Vec<usize> = Vec::with_capacity(hay.len() + 1);
    for (i, c) in hay.char_indices() {
        let base = lower.len();
        for lc in c.to_lowercase() {
            lower.push(lc);
        }
        // 该字符小写展开出的每个字节都映射回原字符起始偏移
        for _ in base..lower.len() {
            map.push(i);
        }
    }
    map.push(hay.len());
    (lower, map)
}

/// 连续空白折叠为单个空格并去首尾。
pub(crate) fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            pending_ws = true;
        } else {
            if pending_ws && !out.is_empty() {
                out.push(' ');
            }
            pending_ws = false;
            out.push(c);
        }
    }
    out
}

/// HTML → 归一化纯文本：块级标签替换为空格、行内标签删除、实体反转义、空白折叠。
/// 与前端 buildSearchIndex 的归一化规则严格一致（跨块边界 = 单个空格）。
pub(crate) fn html_to_plain_text(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut raw = String::with_capacity(html.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            // 按 UTF-8 字符边界推进
            let ch_len = utf8_len(bytes[i]);
            raw.push_str(&html[i..i + ch_len.min(bytes.len() - i)]);
            i += ch_len;
            continue;
        }
        // 标签：找到闭合 '>'（找不到视为文本尾部，直接结束）
        let Some(rel) = html[i..].find('>') else { break };
        let end = i + rel;
        if is_block_tag(&tag_name(&html[i + 1..end])) {
            raw.push(' ');
        }
        i = end + 1;
    }
    collapse_ws(&unescape_entities(&raw))
}

/// 从标签内文本提取小写标签名（容忍 `</`、`<!`、`<?` 前缀）。
fn tag_name(inner: &str) -> String {
    let s = inner.trim_start_matches(['/', '!', '?']);
    s.chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// 块级标签集合：与前端 buildSearchIndex 的块边界规则一致。
fn is_block_tag(name: &str) -> bool {
    matches!(
        name,
        "p" | "div" | "br" | "hr"
            | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
            | "li" | "tr" | "td" | "th" | "blockquote" | "table"
            | "section" | "article" | "aside" | "figure" | "figcaption"
            | "pre" | "ul" | "ol" | "dl" | "dt" | "dd"
    )
}

/// UTF-8 首字节 → 字符总长（非法首字节按 1 处理，扫描器只做切分不做校验）。
fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

/// 取字符串末尾至多 n 个字符。
fn char_tail(s: &str, n: usize) -> String {
    let start = s.char_indices().rev().nth(n.saturating_sub(1)).map(|(i, _)| i);
    match start {
        Some(i) => s[i..].to_string(),
        None => s.to_string(),
    }
}

/// 取字符串开头至多 n 个字符。
fn char_head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 摘要：命中前后各至多 ctx 字符，截断侧以 … 起止。
fn excerpt(hay: &str, start: usize, end: usize, ctx: usize) -> (String, String) {
    let pre_chars: Vec<char> = hay[..start].chars().collect();
    let post_chars: Vec<char> = hay[end..].chars().collect();
    let pre = if pre_chars.len() > ctx {
        format!("…{}", pre_chars[pre_chars.len() - ctx..].iter().collect::<String>())
    } else {
        pre_chars.into_iter().collect()
    };
    let post = if post_chars.len() > ctx {
        format!("{}…", post_chars[..ctx].iter().collect::<String>())
    } else {
        post_chars.into_iter().collect()
    };
    (pre, post)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AppResult;
    use crate::model::{BookFormat, BookMeta, ChapterContent};
    use crate::parser::BookSession;

    /// 内存假会话：预置章节内容，供 LoadedBook 构造测试。
    struct DummySession {
        chapters: Vec<ChapterContent>,
    }

    impl BookSession for DummySession {
        fn load_chapter(&self, index: usize) -> AppResult<ChapterContent> {
            self.chapters
                .get(index)
                .cloned()
                .ok_or_else(|| AppError::other("章节不存在"))
        }
        fn load_resource_bytes(&self, _path: &str) -> AppResult<(String, Vec<u8>)> {
            Err(AppError::other("无资源"))
        }
        fn chapter_count(&self) -> usize {
            self.chapters.len()
        }
    }

    fn chapter(index: u32, title: &str, kind: ChapterKind, body: &str) -> ChapterContent {
        ChapterContent {
            book_id: "bk_test".into(),
            chapter_index: index,
            title: title.into(),
            kind,
            html: (kind == ChapterKind::Html).then(|| body.to_string()),
            text: (kind == ChapterKind::Text).then(|| body.to_string()),
            image_refs: None,
        }
    }

    fn make_book(chapters: Vec<ChapterContent>) -> LoadedBook {
        LoadedBook::new(
            BookMeta {
                id: "bk_test".into(),
                title: "测试书".into(),
                author: None,
                publisher: None,
                language: None,
                format: BookFormat::Epub,
                file_size: 1,
                file_path: "C:\\fake.epub".into(),
                total_chapters: chapters.len() as u32,
                cover_resource: None,
            },
            vec![],
            Box::new(DummySession { chapters }),
        )
    }

    // ────────────────── 文本归一化 ──────────────────

    #[test]
    fn collapse_ws_folds_and_trims() {
        assert_eq!(collapse_ws("  a \n\t b  "), "a b");
        assert_eq!(collapse_ws(""), "");
        assert_eq!(collapse_ws("纯文本不动"), "纯文本不动");
    }

    #[test]
    fn html_block_boundary_becomes_space() {
        // 与前端归一化对齐的关键规则：块边界 = 单个空格
        assert_eq!(html_to_plain_text("<p>foo</p><p>bar</p>"), "foo bar");
        assert_eq!(html_to_plain_text("<h1>标题</h1>\n  <p>正文</p>"), "标题 正文");
    }

    #[test]
    fn html_inline_tags_removed() {
        assert_eq!(html_to_plain_text("<p>a<em>b</em>c</p>"), "abc");
        assert_eq!(html_to_plain_text("<p>x<br>y</p>"), "x y"); // br 是块级分隔
    }

    #[test]
    fn html_entities_unescaped_and_ws_folded() {
        assert_eq!(html_to_plain_text("<p>a &amp; b</p>"), "a & b");
        assert_eq!(html_to_plain_text("<p>行一</p>\n<p>行二</p>"), "行一 行二");
    }

    #[test]
    fn html_unclosed_tag_does_not_panic() {
        assert_eq!(html_to_plain_text("<p>abc"), "abc");
        assert_eq!(html_to_plain_text("abc<p>def"), "abc def");
    }

    // ────────────────── 检索 ──────────────────

    #[test]
    fn search_case_insensitive_latin() {
        let book = make_book(vec![chapter(0, "Ch1", ChapterKind::Html, "<p>Hello World, hello!</p>")]);
        let r = search_in_book(&book, "HELLO");
        assert_eq!(r.total, 2);
        assert_eq!(r.matches[0].quote, "Hello");
        assert_eq!(r.matches[0].prefix, "");
        assert_eq!(r.matches[0].suffix, " World, hell"); // 指纹窗口 12 字
        assert_eq!(r.matches[1].prefix, "ello World, ");
        assert_eq!(r.matches[0].chapter_title, "Ch1");
    }

    #[test]
    fn search_chinese_and_cross_tag() {
        let book = make_book(vec![chapter(
            0,
            "第一章",
            ChapterKind::Html,
            "<p>读书破万卷</p><p>下笔如有神</p>",
        )]);
        // 命中跨块边界（万卷 + 下笔 归一为 "万卷 下笔"）
        let r = search_in_book(&book, "卷 下笔");
        assert_eq!(r.total, 1);
        assert_eq!(r.matches[0].quote, "卷 下笔");
        assert_eq!(r.matches[0].pre, "读书破万");
        assert_eq!(r.matches[0].post, "如有神");
        assert_eq!(r.matches[0].chapter_title, "第一章");
    }

    #[test]
    fn search_txt_kind() {
        let book = make_book(vec![chapter(0, "", ChapterKind::Text, "第一段\n第二段带关键词")]);
        let r = search_in_book(&book, "关键词");
        assert_eq!(r.total, 1);
        assert_eq!(r.matches[0].quote, "关键词");
        assert_eq!(r.matches[0].pre, "第一段 第二段带"); // 未截断无省略号
        assert_eq!(r.matches[0].post, "");
        // 空标题回退
        assert_eq!(r.matches[0].chapter_title, "第 1 章");
    }

    #[test]
    fn search_unicode_lowercase_length_change() {
        // İ 小写为 i̇（两字符），偏移映射必须仍能取回原文
        let book = make_book(vec![chapter(0, "T", ChapterKind::Text, "İstanbul")]);
        let r = search_in_book(&book, "i̇stanbul");
        assert_eq!(r.total, 1);
        assert_eq!(r.matches[0].quote, "İstanbul");
    }

    #[test]
    fn search_truncation_keeps_accurate_total() {
        let body = "词 ".repeat(600); // 600 个命中
        let book = make_book(vec![chapter(0, "T", ChapterKind::Text, &body)]);
        let r = search_in_book(&book, "词");
        assert_eq!(r.total, 600);
        assert_eq!(r.matches.len(), MAX_SEARCH_MATCHES);
        assert!(r.truncated);
    }

    #[test]
    fn search_empty_and_no_hit() {
        let book = make_book(vec![chapter(0, "T", ChapterKind::Text, "内容")]);
        let empty = search_in_book(&book, "   ");
        assert_eq!(empty.total, 0);
        assert!(!empty.truncated);
        let miss = search_in_book(&book, "不存在");
        assert_eq!(miss.total, 0);
        assert!(miss.matches.is_empty());
    }

    #[test]
    fn search_across_chapters_ordered() {
        let book = make_book(vec![
            chapter(0, "A", ChapterKind::Text, "目标在前章"),
            chapter(1, "B", ChapterKind::Text, "目标在后章"),
        ]);
        let r = search_in_book(&book, "目标");
        assert_eq!(r.total, 2);
        assert_eq!(r.matches[0].chapter_index, 0);
        assert_eq!(r.matches[1].chapter_index, 1);
    }
}
