//! Markdown 解析模块（需求 1 全格式兼容）。
//!
//! - 编码检测复用 `parser::decode_to_utf8`（BOM / chardetng，支持 GBK 等）；
//! - 全文渲染：Markdown 通常是短文，整篇作为单章一次渲染，不做章节切分；
//! - 目录：扫描 ATX 标题行（`#` ~ `######`，围栏代码块内的 `#` 不算）生成
//!   树形 TOC，锚点按文档顺序编号；渲染后给对应 `<h1>`~`<h6>` 注入同编号
//!   `id`，目录点击即章内锚点跳转；
//! - 渲染：pulldown-cmark → HTML → ammonia 白名单清洗（共享实现）。
//!   全文单章后脚注定义与引用同处一个 DOM，无需再内嵌注释文本。
//!   图片按 Data URI 内联（Markdown 文本量级小，图片随章传输足够）。

use std::path::Path;

use pulldown_cmark::{html, Options, Parser};
use regex::Regex;
use std::sync::OnceLock;

use crate::error::{AppError, AppResult};
use crate::model::{BookFormat, BookMeta, ChapterContent, ChapterKind, TocItem};
use crate::parser::{decode_to_utf8, make_book_id, sanitize_book_html, title_from_path, BookSession};

/// 文档顺序扫描出的标题（目录项 + 渲染锚点共用）。
#[derive(Debug)]
struct MdHeading {
    level: u8,
    title: String,
}

pub struct MarkdownBook {
    book_id: String,
    text: String,
    headings: Vec<MdHeading>,
}

impl MarkdownBook {
    pub fn open(
        path: impl AsRef<Path>,
        file_size: u64,
    ) -> AppResult<(BookMeta, Vec<TocItem>, Box<dyn BookSession>)> {
        let path = path.as_ref();

        // Markdown 文本量级与 TXT 一致，沿用 200MB 上限防 OOM
        const MAX_MD_FILE_SIZE: u64 = 200 * 1024 * 1024;
        if file_size > MAX_MD_FILE_SIZE {
            return Err(AppError::other(format!(
                "Markdown 文件过大（{} MB，上限 {} MB），暂不支持打开",
                file_size / 1024 / 1024,
                MAX_MD_FILE_SIZE / 1024 / 1024
            )));
        }

        let bytes = std::fs::read(path)?;
        let text = decode_to_utf8(&bytes)?;
        let headings = scan_headings(&text);

        let book_id = make_book_id(path, file_size);
        let meta = BookMeta {
            id: book_id.clone(),
            title: title_from_path(path),
            author: None,
            publisher: None,
            language: None,
            format: BookFormat::Markdown,
            file_size,
            file_path: path.to_string_lossy().to_string(),
            total_chapters: 1,
            cover_resource: None,
        };

        let toc = build_toc(&headings);

        Ok((
            meta,
            toc,
            Box::new(MarkdownBook { book_id, text, headings }),
        ))
    }
}

impl BookSession for MarkdownBook {
    fn chapter_count(&self) -> usize {
        1
    }

    fn load_chapter(&self, index: usize) -> AppResult<ChapterContent> {
        if index != 0 {
            return Err(AppError::ChapterOutOfRange(index as u32));
        }

        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_TABLES);
        opts.insert(Options::ENABLE_FOOTNOTES);
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        opts.insert(Options::ENABLE_TASKLISTS);
        let rendered = {
            let mut out = String::with_capacity(self.text.len() + 64);
            html::push_html(&mut out, Parser::new_ext(&self.text, opts));
            out
        };
        let rendered = inject_heading_ids(rendered, self.headings.len());

        Ok(ChapterContent {
            book_id: self.book_id.clone(),
            chapter_index: 0,
            title: "全文".into(),
            kind: ChapterKind::Html,
            html: Some(sanitize_book_html(&rendered, false)),
            text: None,
            image_refs: None,
        })
    }

    fn load_resource_bytes(&self, _path: &str) -> AppResult<(String, Vec<u8>)> {
        Err(AppError::ResourceNotFound("Markdown 无内嵌资源".into()))
    }
}

/// 扫描 ATX 标题行（1~6 个 `#` 后跟空格/制表/行尾）；围栏代码块（``` / ~~~）
/// 内的 `#` 注释行不算标题。返回文档顺序的标题列表。
fn scan_headings(text: &str) -> Vec<MdHeading> {
    let mut headings: Vec<MdHeading> = vec![];
    let mut fence: Option<u8> = None; // 当前围栏字符（b'`' / b'~'）

    for line in text.lines() {
        let trimmed = line.trim_start();
        // 围栏状态机：``` 或 ~~~ 开启/关闭
        if let Some(ch) = fence {
            if trimmed.starts_with(ch as char) && trimmed.chars().all(|c| c == ch as char) {
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") {
            fence = Some(b'`');
            continue;
        }
        if trimmed.starts_with("~~~") {
            fence = Some(b'~');
            continue;
        }
        // ATX 标题：1~6 个 # 后跟空格或行尾
        let hashes = trimmed.chars().take_while(|&c| c == '#').count();
        if hashes >= 1 && hashes <= 6 {
            let rest = &trimmed[hashes..];
            if rest.starts_with(' ') || rest.starts_with('\t') || rest.is_empty() {
                let title = strip_md_inline(rest.trim());
                headings.push(MdHeading {
                    level: hashes as u8,
                    title: if title.is_empty() { "未命名章节".into() } else { title },
                });
            }
        }
    }
    headings
}

/// 标题 → 树形 TOC：锚点按文档顺序编号（md-h-0, md-h-1, …），与
/// `inject_heading_ids` 注入的 id 一一对应；层级按标题级别嵌套。
fn build_toc(headings: &[MdHeading]) -> Vec<TocItem> {
    let mut root: Vec<TocItem> = vec![];
    // (级别, 待归位节点)：遇到更浅/同级标题时逐层弹出挂到父级
    let mut stack: Vec<(u8, TocItem)> = vec![];

    for (i, h) in headings.iter().enumerate() {
        let item = TocItem {
            id: format!("md-h-{i}"),
            label: h.title.clone(),
            chapter_index: 0,
            anchor: Some(format!("md-h-{i}")),
            children: vec![],
        };
        while stack.last().is_some_and(|(lvl, _)| *lvl >= h.level) {
            let (_, done) = stack.pop().unwrap();
            match stack.last_mut() {
                Some((_, parent)) => parent.children.push(done),
                None => root.push(done),
            }
        }
        stack.push((h.level, item));
    }
    while let Some((_, done)) = stack.pop() {
        match stack.last_mut() {
            Some((_, parent)) => parent.children.push(done),
            None => root.push(done),
        }
    }
    root
}

/// 按文档顺序给 `<h1>`~`<h6>` 开标签注入 `id="md-h-{n}"`，与 TOC 锚点对应。
/// pulldown-cmark 输出的标题开标签无任何属性，直接按出现顺序编号即可。
fn inject_heading_ids(html: String, heading_count: usize) -> String {
    static H_RE: OnceLock<Regex> = OnceLock::new();
    let re = H_RE.get_or_init(|| Regex::new(r"<h([1-6])>").expect("md heading regex"));
    let mut i = 0usize;
    re.replace_all(&html, |caps: &regex::Captures| {
        let tag = &caps[0];
        if i >= heading_count {
            return tag.to_string();
        }
        let id = format!(r##"<h{} id="md-h-{i}">"##, &caps[1]);
        i += 1;
        id
    })
    .into_owned()
}

/// 去掉标题中的行内标记（**粗体**、*斜体*、`代码`、链接等），得到纯文本标题。
fn strip_md_inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' | '_' | '`' | '~' => {}
            '[' => {
                // [text](url) → text
                let mut inner = String::new();
                let mut depth = 1;
                for c2 in chars.by_ref() {
                    match c2 {
                        ']' => {
                            depth -= 1;
                            break;
                        }
                        _ => inner.push(c2),
                    }
                }
                let _ = depth;
                // 跳过 (url)
                if chars.peek() == Some(&'(') {
                    for c2 in chars.by_ref() {
                        if c2 == ')' {
                            break;
                        }
                    }
                }
                out.push_str(&inner);
            }
            '!' if chars.peek() == Some(&'[') => {
                // ![alt](url) → 跳过
                chars.next();
                for c2 in chars.by_ref() {
                    if c2 == ')' {
                        break;
                    }
                }
            }
            '\\' => {}
            _ => out.push(c),
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 围栏代码块内的 `#` 不是标题；标题按级别嵌套成树，锚点编号与文档顺序一致。
    #[test]
    fn headings_scan_and_toc_nest() {
        let text = "# 一\n正文\n```\n# 不是标题\n```\n## 1.1\n### 1.1.1\n## 1.2\n# 二\n";
        let hs = scan_headings(text);
        assert_eq!(hs.len(), 5, "围栏内 # 不应计入: {hs:?}");
        assert_eq!(hs[0].title, "一");
        assert_eq!(hs[1].level, 2);

        let toc = build_toc(&hs);
        assert_eq!(toc.len(), 2, "顶层只有 # 一 和 # 二");
        assert_eq!(toc[0].anchor.as_deref(), Some("md-h-0"));
        assert_eq!(toc[0].children.len(), 2);
        assert_eq!(toc[0].children[0].children.len(), 1, "1.1.1 嵌套在 1.1 下");
        assert_eq!(toc[0].children[0].children[0].anchor.as_deref(), Some("md-h-2"));
        assert_eq!(toc[1].anchor.as_deref(), Some("md-h-4"));
    }

    /// 渲染后的标题开标签按顺序获得与 TOC 一致的 id。
    #[test]
    fn heading_ids_injected_in_document_order() {
        let text = "# 甲\n## 乙\n### 丙\n";
        let hs = scan_headings(text);
        let opts = Options::empty();
        let mut rendered = String::new();
        pulldown_cmark::html::push_html(&mut rendered, Parser::new_ext(text, opts));
        let out = inject_heading_ids(rendered, hs.len());
        assert!(out.contains(r##"<h1 id="md-h-0">"##), "{out}");
        assert!(out.contains(r##"<h2 id="md-h-1">"##), "{out}");
        assert!(out.contains(r##"<h3 id="md-h-2">"##), "{out}");
    }

    /// 无标题文档：单章全文，目录为空。
    #[test]
    fn no_headings_single_chapter() {
        let hs = scan_headings("普通段落，没有标题。\n");
        assert!(hs.is_empty());
        assert!(build_toc(&hs).is_empty());
    }
}
