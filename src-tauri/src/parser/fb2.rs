//! FB2（FictionBook 2.x）解析模块（需求 1 全格式兼容）。
//!
//! 结构：单个 XML 文档。
//! - 元数据：`<description><title-info>`（book-title / author）与
//!   `<publish-info><publisher>`；
//! - 正文：`<body>` 下的顶层 `<section>` → 一章；`<title>` 内的 `<p>` 拼接章节标题；
//!   无 section 时整个 body 作为单章；
//! - 图片：`<image xlink:href="#id">` 引用 `<binary id=".." content-type="image/..">`
//!   的 base64 数据。≤100KB 内联为 data: URI；大图改写为 reader-res URL，
//!   资源路径约定为 `bin:<id>`，由 `load_resource_bytes` 解码返回；
//! - 封面：`<coverpage><image xlink:href="#id">`；
//! - 编码：XML 声明 encoding 优先，否则 BOM/chardetng 兜底。

use std::collections::HashMap;
use std::path::Path;

use base64::Engine as _;
use regex::Regex;
use std::sync::OnceLock;

use crate::error::{AppError, AppResult};
use crate::model::{BookFormat, BookMeta, ChapterContent, ChapterKind, TocItem};
use crate::parser::{
    decode_to_utf8, escape_attr, make_book_id, sanitize_book_html, title_from_path, unescape_entities,
    BookSession,
};

/// 图片内联上限（与 EPUB 一致）：≤100KB 内联，大图走 reader-res 协议。
const MAX_INLINE_IMAGE_SIZE: usize = 100 * 1024;

pub struct Fb2Book {
    book_id: String,
    /// 顶层 section 切片（字节区间，指向 xml 字符串）
    chapters: Vec<Fb2Section>,
    /// binary id → (mime, base64 文本切片区间)。base64 解码延迟到取用时。
    binaries: Vec<Fb2Binary>,
    /// FB2 规范注释体：正文 `<a type="note" l:href="#id">` 引用的
    /// `<section id="...">` → 注释纯文本（渲染时内嵌到引用上供悬浮预览）。
    note_targets: HashMap<String, String>,
    cover_id: Option<String>,
    xml: String,
    title: String,
}

struct Fb2Section {
    title: String,
    start: usize,
    end: usize,
}

struct Fb2Binary {
    id: String,
    mime: String,
    start: usize,
    end: usize,
}

impl Fb2Book {
    pub fn open(
        path: impl AsRef<Path>,
        file_size: u64,
    ) -> AppResult<(BookMeta, Vec<TocItem>, Box<dyn BookSession>)> {
        let path = path.as_ref();
        const MAX_FB2_FILE_SIZE: u64 = 200 * 1024 * 1024;
        if file_size > MAX_FB2_FILE_SIZE {
            return Err(AppError::other(format!(
                "FB2 文件过大（{} MB，上限 {} MB），暂不支持打开",
                file_size / 1024 / 1024,
                MAX_FB2_FILE_SIZE / 1024 / 1024
            )));
        }

        let raw = std::fs::read(path)?;
        let xml = decode_fb2(&raw)?;
        let (title, author, publisher, cover_id) = parse_description(&xml);

        let mut book = Fb2Book {
            book_id: String::new(),
            chapters: Vec::new(),
            binaries: Vec::new(),
            note_targets: collect_note_targets(&xml),
            cover_id,
            xml: String::new(),
            title: title.clone().unwrap_or_else(|| title_from_path(path)),
        };
        book.binaries = parse_binaries(&xml);
        book.chapters = parse_sections(&xml);
        let total = book.chapters.len();

        let book_id = make_book_id(path, file_size);
        book.book_id = book_id.clone();
        // 解析完成后再 move 进会话，避免 200MB 级 XML 在 open 期间双倍驻留
        book.xml = xml;

        let meta = BookMeta {
            id: book_id.clone(),
            title: book.title.clone(),
            author,
            publisher,
            language: None,
            format: BookFormat::Fb2,
            file_size,
            file_path: path.to_string_lossy().to_string(),
            total_chapters: total as u32,
            cover_resource: book.cover_id.as_ref().map(|id| format!("bin:{id}")),
        };

        let toc: Vec<TocItem> = book
            .chapters
            .iter()
            .enumerate()
            .map(|(i, s)| TocItem {
                id: format!("fb2-{}", i),
                label: if s.title.is_empty() { format!("第 {} 节", i + 1) } else { s.title.clone() },
                chapter_index: i as u32,
                anchor: None,
                children: vec![],
            })
            .collect();

        Ok((meta, toc, Box::new(book)))
    }

    /// 章节 HTML：切片 → 注释引用改写 → 图片引用改写 → 清洗。
    fn render_section(&self, slice: &str) -> String {
        let linked = rewrite_note_refs(slice, &self.note_targets);
        let rewritten = self.rewrite_images(&linked);
        sanitize_book_html(&rewritten, false)
    }

    /// `<image xlink:href="#id">` → data: URI（小图）或 reader-res URL（大图）。
    fn rewrite_images(&self, slice: &str) -> String {
        static IMG_RE: once_cell_lazy::Lazy<regex::Regex> = once_cell_lazy::Lazy::new(|| {
            regex::Regex::new(r#"(?is)<image\b[^>]*?\bxlink:href\s*=\s*["']#([^"']+)["'][^>]*>"#)
                .expect("fb2 image regex")
        });
        IMG_RE.replace_all(slice, |caps: &regex::Captures| {
            let id = &caps[1];
            match self.binary_bytes(id) {
                Some((mime, bytes)) if bytes.len() <= MAX_INLINE_IMAGE_SIZE => {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                    format!(r#"<img src="data:{mime};base64,{b64}"/>"#)
                }
                Some(_) => {
                    // 大图走 reader-res 协议（资源路径约定 bin:<id>）
                    let url = crate::parser::resource_url(&self.book_id, &format!("bin:{id}"));
                    format!(r#"<img src="{url}" alt="{id}"/>"#)
                }
                None => String::new(), // 缺失的图片直接丢弃
            }
        })
        .into_owned()
    }

    /// 解码 binary id 对应字节。
    fn binary_bytes(&self, id: &str) -> Option<(String, Vec<u8>)> {
        let b = self.binaries.iter().find(|b| b.id == id)?;
        let b64 = &self.xml[b.start..b.end];
        let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
        base64::engine::general_purpose::STANDARD
            .decode(cleaned.as_bytes())
            .ok()
            .map(|bytes| (b.mime.clone(), bytes))
    }
}

impl BookSession for Fb2Book {
    fn chapter_count(&self) -> usize {
        self.chapters.len()
    }

    fn load_chapter(&self, index: usize) -> AppResult<ChapterContent> {
        let s = self
            .chapters
            .get(index)
            .ok_or(AppError::ChapterOutOfRange(index as u32))?;
        let html = self.render_section(&self.xml[s.start..s.end]);
        Ok(ChapterContent {
            book_id: self.book_id.clone(),
            chapter_index: index as u32,
            title: if s.title.is_empty() { format!("第 {} 节", index + 1) } else { s.title.clone() },
            kind: ChapterKind::Html,
            html: Some(html),
            text: None,
            image_refs: None,
        })
    }

    fn load_resource_bytes(&self, path: &str) -> AppResult<(String, Vec<u8>)> {
        // 资源路径约定：bin:<binary id>
        let id = path.strip_prefix("bin:").ok_or_else(|| AppError::ResourceNotFound(path.to_string()))?;
        self.binary_bytes(id)
            .map(|(mime, bytes)| (mime, bytes))
            .ok_or_else(|| AppError::ResourceNotFound(path.to_string()))
    }
}

/// FB2 编码：XML 声明 encoding 优先，否则 BOM/chardetng。
fn decode_fb2(bytes: &[u8]) -> AppResult<String> {
    // XML 声明（<?xml ... encoding="xxx"?>）通常在头部 128 字节内
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(128)]);
    if let Some(pos) = head.find("encoding=") {
        let rest = &head[pos + 9..];
        if let Some(end) = rest.find(['"', '\'']) {
            let enc_name = rest[end + 1..]
                .split(['"', '\''])
                .next()
                .unwrap_or("");
            if !enc_name.is_empty() && !enc_name.eq_ignore_ascii_case("utf-8") {
                if let Some(enc) = encoding_rs::Encoding::for_label(enc_name.as_bytes()) {
                    let (s, _, _) = enc.decode(bytes);
                    return Ok(s.into_owned());
                }
            }
        }
    }
    decode_to_utf8(bytes)
}

/// 元数据：title-info / publish-info 扫描（非流式，用区间定位避免深度嵌套状态机）。
type DescMeta = (Option<String>, Option<String>, Option<String>, Option<String>);

fn parse_description(xml: &str) -> DescMeta {
    let mut meta: DescMeta = (None, None, None, None);

    let Some(start) = xml.find("<description") else {
        return meta;
    };
    let Some(end) = xml[start..].find("</description>").map(|e| start + e) else {
        return meta;
    };
    let desc = &xml[start..end];

    // book-title
    meta.0 = tag_text(desc, "book-title");
    // publisher
    meta.2 = tag_text(desc, "publisher");
    // coverpage: <image xlink:href="#id">
    {
        static RE: once_cell_lazy::Lazy<regex::Regex> = once_cell_lazy::Lazy::new(|| {
            regex::Regex::new(r#"(?is)<coverpage[\s\S]*?<image[^>]*href\s*=\s*["']#([^"']+)["']"#)
                .expect("coverpage regex")
        });
        if let Some(c) = RE.captures(desc) {
            meta.3 = Some(c[1].to_string());
        }
    }
    // author：first-name + last-name（可能多个，取第一个）
    {
        if let Some(a_start) = desc.find("<author") {
            let a_end = desc[a_start..]
                .find("</author>")
                .map(|e| a_start + e)
                .unwrap_or(desc.len());
            let author_xml = &desc[a_start..a_end];
            let first = tag_text(author_xml, "first-name").unwrap_or_default();
            let last = tag_text(author_xml, "last-name").unwrap_or_default();
            let nickname = tag_text(author_xml, "nickname");
            let joined = format!("{} {}", first.trim(), last.trim()).trim().to_string();
            meta.1 = Some(if joined.is_empty() {
                nickname.unwrap_or_else(|| "佚名".into())
            } else {
                joined
            });
        }
    }
    meta
}

/// 取第一个 <tag>…</tag> 的文本（去内嵌标签与实体）。
fn tag_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}", tag);
    let s = xml.find(&open)? + open.len();
    let s = xml[s..].find('>')? + s + 1;
    let e = xml[s..].find(&format!("</{}>", tag))? + s;
    let inner = &xml[s..e];
    let plain = regex::Regex::new(r"(?s)<[^>]*>").ok()?.replace_all(inner, "");
    Some(crate::parser::unescape_entities(plain.trim()))
}

/// 扫描 <binary id=".." content-type="..">…</binary>，记录 base64 数据区间。
fn parse_binaries(xml: &str) -> Vec<Fb2Binary> {
    static RE: once_cell_lazy::Lazy<regex::Regex> = once_cell_lazy::Lazy::new(|| {
        regex::Regex::new(r#"(?is)<binary\b[^>]*\bid\s*=\s*["']([^"']+)["'][^>]*\bcontent-type\s*=\s*["']([^"']+)["'][^>]*>([\s\S]*?)</binary>"#)
            .expect("fb2 binary regex")
    });
    RE.captures_iter(xml)
        .map(|c| {
            let data_start = c.get(3).unwrap().start();
            let data_end = c.get(3).unwrap().end();
            Fb2Binary {
                id: c[1].to_string(),
                mime: c[2].to_string(),
                start: data_start,
                end: data_end,
            }
        })
        .collect()
}

/// 顶层 <section> 切片：只取 <body> 的一级子 section；没有 section 时整个 body 单章。
/// 标题来自 section 内第一个 <title>（多 <p> 拼接，同时从切片中保留原结构）。
fn parse_sections(xml: &str) -> Vec<Fb2Section> {
    // 定位第一个 <body>
    // 只取第一个 body（正文 body）：FB2 的注释体是其后的独立 <body name="notes">，
    // 若区间取到最后一个 </body>，注释 section 会被误切成正文章节（目录被污染）
    let body_start = xml.find("<body");
    let body_end = body_start.and_then(|s| xml[s..].find("</body>").map(|e| s + e));
    let (body_start, body_end) = match (body_start, body_end) {
        (Some(s), Some(e)) => (s, e),
        _ => return vec![Fb2Section { title: "全文".into(), start: 0, end: xml.len() }],
    };
    let body = &xml[body_start..body_end];

    // 扫描一级 <section>（通过深度计数处理嵌套）
    let mut sections: Vec<Fb2Section> = vec![];
    let bytes = body.as_bytes();
    let mut i = 0usize;
    while i < body.len() {
        if bytes[i] == b'<' {
            if body[i..].starts_with("<section") {
                // 找当前 section 的结束（计数嵌套）
                let rel_end = find_section_end(body, i);
                let inner_start = body[i..].find('>').map(|g| i + g + 1).unwrap_or(i);
                let title = section_title(&body[inner_start..rel_end]);
                sections.push(Fb2Section { title, start: inner_start, end: rel_end });
                i = rel_end;
                continue;
            }
        }
        i += 1;
    }

    if sections.is_empty() {
        return vec![Fb2Section {
            title: "全文".into(),
            start: body_start,
            end: body_end,
        }];
    }
    sections
}

/// 从 `<section...>` 开始向后匹配嵌套，返回闭合 `</section>` 之后的下标。
fn find_section_end(xml: &str, start: usize) -> usize {
    let mut depth = 0usize;
    let mut i = start;
    let bytes = xml.as_bytes();
    while i < xml.len() {
        if bytes[i] == b'<' {
            if xml[i..].starts_with("<section") {
                depth += 1;
            } else if xml[i..].starts_with("</section>") {
                depth -= 1;
                if depth == 0 {
                    return i + "</section>".len();
                }
            }
        }
        i += 1;
    }
    xml.len()
}

/// section 内第一个 <title> 的文本（多个 <p> 用空格拼接）。
fn section_title(slice: &str) -> String {
    static RE: once_cell_lazy::Lazy<regex::Regex> = once_cell_lazy::Lazy::new(|| {
        regex::Regex::new(r"(?is)<title\b[^>]*>([\s\S]*?)</title>").expect("fb2 title regex")
    });
    let Some(c) = RE.captures(slice) else { return String::new() };
    let inner = &c[1];
    static P_RE: once_cell_lazy::Lazy<regex::Regex> = once_cell_lazy::Lazy::new(|| {
        regex::Regex::new(r"(?is)<p\b[^>]*>([\s\S]*?)</p>").expect("fb2 title p regex")
    });
    let parts: Vec<String> = if P_RE.is_match(inner) {
        P_RE
            .captures_iter(inner)
            .map(|pc| strip_tags(&pc[1]))
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        vec![strip_tags(inner)]
    };
    crate::parser::unescape_entities(parts.join(" ").trim())
}

fn strip_tags(html: &str) -> String {
    static RE: once_cell_lazy::Lazy<regex::Regex> = once_cell_lazy::Lazy::new(|| {
        regex::Regex::new(r"(?is)<[^>]*>").expect("fb2 strip regex")
    });
    RE.replace_all(html, "").into_owned()
}

/// FB2 注释体长度上限（内嵌 data-fn-text 的文本）。
const MAX_NOTE_TEXT: usize = 2000;

/// 收集全文档 `<section id="...">` → 注释纯文本。
/// FB2 规范：正文用 `<a type="note" l:href="#id">` 引用，
/// 注释体在 `<body name="notes">` 的带 id section 里——它是独立 body，
/// 渲染时无法并入引用所在章，因此把文本内嵌到引用元素上（见 rewrite_note_refs）。
fn collect_note_targets(xml: &str) -> HashMap<String, String> {
    static SEC_RE: OnceLock<Regex> = OnceLock::new();
    let re = SEC_RE.get_or_init(|| {
        Regex::new(r#"(?is)<section\b[^>]*?\bid\s*=\s*["']([^"']+)["'][^>]*>"#)
            .expect("fb2 note section regex")
    });
    let mut map: HashMap<String, String> = HashMap::new();
    for caps in re.captures_iter(xml) {
        let id = unescape_entities(&caps[1]);
        if map.contains_key(&id) {
            continue; // 同 id 重复取首个
        }
        let tag_end = caps.get(0).unwrap().end();
        let end = find_section_end(xml, caps.get(0).unwrap().start());
        if end <= tag_end {
            continue;
        }
        // 段落间保留换行，供前端预览弹窗按 pre-wrap 展示
        let raw = xml[tag_end..end].replace("</p>", "\n").replace("<empty-line/>", "\n");
        let plain = strip_tags(&raw);
        let lines: Vec<String> = plain
            .lines()
            .map(|l| unescape_entities(l.trim()))
            .filter(|l| !l.is_empty())
            .collect();
        let mut text = lines.join("\n");
        if text.chars().count() > MAX_NOTE_TEXT {
            text = text.chars().take(MAX_NOTE_TEXT).collect();
        }
        if !text.is_empty() {
            map.insert(id, text);
        }
    }
    map
}

/// 注释引用改写：
/// 1. `l:href` → `href`（ammonia 白名单只认 href，不改写则 FB2 全部链接失效）；
/// 2. 指向注释 section 的引用内嵌 data-fn-text（注释体在独立 body，本章 DOM 取不到）。
fn rewrite_note_refs(slice: &str, targets: &HashMap<String, String>) -> String {
    static A_RE: OnceLock<Regex> = OnceLock::new();
    let re = A_RE.get_or_init(|| Regex::new(r"(?is)<a\b[^>]*>").expect("fb2 anchor regex"));
    static LHREF_RE: OnceLock<Regex> = OnceLock::new();
    let lh = LHREF_RE.get_or_init(|| {
        Regex::new(r#"(?i)\bl:href\s*=\s*("([^"]*)"|'([^']*)')"#).expect("fb2 l:href regex")
    });
    re.replace_all(slice, |caps: &regex::Captures| {
        let tag = caps.get(0).map(|m| m.as_str()).unwrap_or_default();
        let Some(hc) = lh.captures(tag) else { return tag.to_string() };
        let value = unescape_entities(
            hc.get(2)
                .or_else(|| hc.get(3))
                .map(|m| m.as_str())
                .unwrap_or_default(),
        );
        let out = tag.replacen("l:href", "href", 1);
        match value.strip_prefix('#').and_then(|id| targets.get(id)) {
            Some(text) => out.replacen("<a", &format!(r#"<a data-fn-text="{}""#, escape_attr(text)), 1),
            None => out,
        }
    })
    .into_owned()
}

/// once_cell 的精简别名（避免直接依赖 once_cell crate，std OnceLock 不能用于 Lazy Regex 闭包，
/// 这里用 OnceLock 手写一个极简 Lazy）。
mod once_cell_lazy {
    pub struct Lazy<T>(std::sync::OnceLock<T>, fn() -> T);
    impl<T> Lazy<T> {
        pub const fn new(f: fn() -> T) -> Self {
            Lazy(std::sync::OnceLock::new(), f)
        }
    }
    impl<T> std::ops::Deref for Lazy<T> {
        type Target = T;
        fn deref(&self) -> &T {
            self.0.get_or_init(self.1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_xml() -> String {
        concat!(
            r##"<FictionBook><body><section><p>正文<a type="note" l:href="#n1">1</a>后续</p>"##,
            r##"<p>见注<a l:href='#n2'>[2]</a></p>"##,
            r##"<p>外链<a l:href="http://example.com">官网</a></p></section></body>"##,
            r##"<body name="notes"><section id="n1"><p>注释 &quot;内容&quot;</p><p>第二段</p></section>"##,
            r##"<section id="n2"><p>第二章注释</p></section></body></FictionBook>"##,
        )
        .to_string()
    }

    /// FB2 规范注释：l:href 引用改写为 href 且内嵌注释文本；外链只改写不加文本。
    #[test]
    fn note_refs_get_href_and_embedded_text() {
        let xml = sample_xml();
        let targets = collect_note_targets(&xml);
        assert_eq!(targets.get("n1").map(String::as_str), Some("注释 \"内容\"\n第二段"));
        assert_eq!(targets.get("n2").map(String::as_str), Some("第二章注释"));

        let out = rewrite_note_refs(
            r##"<p>正文<a type="note" l:href="#n1">1</a>后续</p>"##,
            &targets,
        );
        assert!(out.contains(r##"href="#n1""##), "href 改写丢失: {out}");
        assert!(
            out.contains("data-fn-text=\"注释 &quot;内容&quot;\n第二段\""),
            "内嵌文本丢失: {out}"
        );

        let out2 = rewrite_note_refs(r#"<p><a l:href='#n2'>[2]</a></p>"#, &targets);
        assert!(out2.contains(r#"href='#n2'"#));
        assert!(out2.contains(r#"data-fn-text="第二章注释""#));

        // 外链：只改 l:href → href，不内嵌
        let out3 = rewrite_note_refs(r#"<a l:href="http://example.com">官网</a>"#, &targets);
        assert!(out3.contains(r#"href="http://example.com""#));
        assert!(!out3.contains("data-fn-text"));

        // 无 l:href 的锚点原样保留
        let plain = r##"<a href="#x">y</a>"##;
        assert_eq!(rewrite_note_refs(plain, &targets), plain);
    }
}
