//! TXT 解析模块：编码检测 + 正则虚拟目录。
//!
//! 流程（Q4 大文件内存优化）：
//! 1. 大小上限校验（默认 200MB），超限直接拒绝，防止 OOM；
//! 2. memmap2 内存映射文件 —— 不再 `std::fs::read` 整份拷贝，
//!    字节按需从页缓存读入，编码检测/正则扫描均直接作用在映射切片上；
//! 3. 编码检测：BOM 优先（UTF-8/UTF-16LE/BE），否则 chardetng 只喂
//!    头部 256KB 采样（统计猜测不需要全量字节），统一解码为 UTF-8 String；
//!    注意：解码后的全文 String 必然驻留内存（约等于原文件大小），
//!    这是章节切片 O(1) 的代价，配合 AppState 的书籍 LRU 驱逐兜底；
//! 4. 正则逐行匹配章节标题（第X章/卷/回、Chapter N、序章/楔子/番外等），
//!    以匹配行起点为切片边界生成虚拟目录；
//! 5. load_chapter 按字节区间切片返回纯文本。

use std::path::Path;
use std::sync::OnceLock;

use memmap2::Mmap;
use regex::Regex;

use crate::error::{AppError, AppResult};
use crate::model::{BookFormat, BookMeta, ChapterContent, ChapterKind, TocItem};
use crate::parser::{decode_to_utf8, make_book_id, title_from_path, BookSession};

/// TXT 文件大小上限：超过直接拒绝（解码全文驻留内存，需给页缓存留余量）。
const MAX_TXT_FILE_SIZE: u64 = 200 * 1024 * 1024; // 200MB

/// 编码检测采样字节数：chardetng 统计只需头部样本，避免把 GBK 文件整个喂进去。
/// （常量保留给章节扫描注释参考；实际检测已收敛到 parser::decode_to_utf8 共享实现。）
const _ENCODING_SAMPLE_SIZE: usize = 256 * 1024;

/// 章节标题正则（逐模式拼接，行首锚定，标题长度上限 48 字符防误匹配长段落）。
const CHAPTER_PATTERNS: &[&str] = &[
    // 中文核心：第X章/节/回/卷/部/集/篇（数字或中文数字）+ 可选标题
    r"[ \t\u{3000}]*第\s*[0-9零一二三四五六七八九十百千万两〇]+\s*[章回节卷部集篇][^\n]{0,48}",
    // 纯 "卷一 xxx" 变体
    r"[ \t\u{3000}]*卷\s*[0-9一二三四五六七八九十百千万两〇]+[^\n]{0,48}",
    // 英文
    r"[ \t]*(?:Chapter|CHAPTER)\s+\d+[^\n]{0,48}",
    // 特殊章节名
    r"[ \t\u{3000}]*(?:序章|序言|序|楔子|前言|自序|凡例|后记|后序|尾声|终章|终局|番外)[^\n]{0,48}",
];

fn chapter_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let combined = format!("(?m)^({})[ \t\u{3000}]*$", CHAPTER_PATTERNS.join("|"));
        Regex::new(&combined).expect("章节正则编译失败")
    })
}

/// 虚拟章节切片（字节偏移，均落在 UTF-8 字符边界上，可直接 &text[a..b]）。
#[derive(Debug, Clone)]
struct TxtChapter {
    title: String,
    start: usize,
    end: usize,
}

pub struct TxtBook {
    book_id: String,
    /// 解码后的全文（UTF-8），驻留内存
    text: String,
    chapters: Vec<TxtChapter>,
}

impl TxtBook {
    pub fn open(path: impl AsRef<Path>, file_size: u64) -> AppResult<(BookMeta, Vec<TocItem>, Box<dyn BookSession>)> {
        let path = path.as_ref();

        // ── 大小上限（Q4）：超限拒绝，防止解码全文把内存打爆 ──
        if file_size > MAX_TXT_FILE_SIZE {
            return Err(AppError::other(format!(
                "TXT 文件过大（{} MB，上限 {} MB），暂不支持打开",
                file_size / 1024 / 1024,
                MAX_TXT_FILE_SIZE / 1024 / 1024
            )));
        }

        // ── 内存映射（Q4）：避免整份文件先拷贝进堆，字节按页缓存按需载入 ──
        let file = std::fs::File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        let bytes: &[u8] = &mmap;

        // ── 编码检测（需求 4）──
        let text = decode_to_utf8(bytes)?;

        // ── 正则虚拟目录 ──
        let chapters = build_virtual_toc(&text);
        let total = chapters.len();

        let book_id = make_book_id(path, file_size);
        let meta = BookMeta {
            id: book_id.clone(),
            title: title_from_path(path),
            author: None,
            publisher: None,
            language: None,
            format: BookFormat::Txt,
            file_size,
            file_path: path.to_string_lossy().to_string(),
            total_chapters: total as u32,
            cover_resource: None,
        };

        let toc: Vec<TocItem> = chapters
            .iter()
            .enumerate()
            .map(|(i, c)| TocItem {
                id: format!("txt-{}", i),
                label: c.title.clone(),
                chapter_index: i as u32,
                anchor: Some(c.start.to_string()), // 字符偏移作为章内定位
                children: vec![],
            })
            .collect();

        Ok((
            meta,
            toc,
            Box::new(TxtBook {
                book_id,
                text,
                chapters,
            }),
        ))
    }
}

impl BookSession for TxtBook {
    fn chapter_count(&self) -> usize {
        self.chapters.len()
    }

    fn load_chapter(&self, index: usize) -> AppResult<ChapterContent> {
        let c = self
            .chapters
            .get(index)
            .ok_or(AppError::ChapterOutOfRange(index as u32))?;
        Ok(ChapterContent {
            book_id: self.book_id.clone(),
            chapter_index: index as u32,
            title: c.title.clone(),
            kind: ChapterKind::Text,
            html: None,
            text: Some(self.text[c.start..c.end].to_string()),
            image_refs: None,
        })
    }

    fn load_resource_bytes(&self, _path: &str) -> AppResult<(String, Vec<u8>)> {
        Err(AppError::ResourceNotFound("TXT 无内嵌资源".into()))
    }
}

/// 扫描全文生成虚拟章节切片。
/// 无任何匹配时整本书作为单章返回。
fn build_virtual_toc(text: &str) -> Vec<TxtChapter> {
    let re = chapter_regex();
    let mut starts: Vec<(usize, String)> = vec![]; // (字节偏移, 标题)

    for m in re.find_iter(text) {
        // 匹配行起点即章节起点；标题 = 匹配行去首尾空白
        starts.push((m.start(), m.as_str().trim().to_string()));
    }
    if starts.is_empty() {
        return vec![TxtChapter {
            title: "全文".to_string(),
            start: 0,
            end: text.len(),
        }];
    }

    let mut chapters = Vec::with_capacity(starts.len());
    for (i, (start, title)) in starts.iter().enumerate() {
        // 章节正文从标题行的下一行开始
        let content_start = text[*start..]
            .find('\n')
            .map(|nl| *start + nl + 1)
            .unwrap_or(text.len());
        let end = starts.get(i + 1).map(|(s, _)| *s).unwrap_or(text.len());
        chapters.push(TxtChapter {
            title: title.clone(),
            start: content_start.min(end),
            end,
        });
    }
    chapters
}
