//! CBZ 漫画解析模块。
//!
//! - ZIP 容器按需解压（不整包解压，图片驻留 zip 字节中）；
//! - 提取图片条目（jpg/png/gif/webp/bmp/avif），按文件名**自然排序**
//!   （img2 < img10，非字典序），生成单章的 image_refs 有序列表；
//! - 前端拿到列表后以 reader-res:// 协议 URL 直接作为 <img src>（原生并行
//!   加载 + 浏览器缓存，不逐张走 IPC base64）。

use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::Mutex;

use zip::ZipArchive;

use crate::error::{AppError, AppResult};
use crate::model::{BookFormat, BookMeta, ChapterContent, ChapterKind, TocItem};
use crate::parser::{guess_mime, make_book_id, title_from_path, BookSession};

const IMAGE_EXTS: [&str; 7] = ["jpg", "jpeg", "png", "gif", "webp", "bmp", "avif"];

pub struct CbzBook {
    book_id: String,
    archive: Mutex<ZipArchive<Cursor<Vec<u8>>>>,
    /// 自然排序后的图片条目名（zip 内路径）
    images: Vec<String>,
    title: String,
}

impl CbzBook {
    pub fn open(path: impl AsRef<Path>, file_size: u64) -> AppResult<(BookMeta, Vec<TocItem>, Box<dyn BookSession>)> {
        let path = path.as_ref();
        let data = std::fs::read(path)?;
        let mut archive = ZipArchive::new(Cursor::new(data)).map_err(|e| match &e {
            zip::result::ZipError::UnsupportedArchive(m)
                if m.to_ascii_lowercase().contains("password") =>
            {
                AppError::DrmDetected
            }
            _ => AppError::Zip(e.to_string()),
        })?;

        // 收集图片条目，跳过 macOS 元数据目录等常见杂质
        let mut images: Vec<String> = vec![];
        for i in 0..archive.len() {
            let file = archive.by_index(i).map_err(|e| AppError::Zip(e.to_string()))?;
            let name = file.name().to_string();
            if file.is_dir() || name.split('/').any(|seg| seg.starts_with("__MACOSX") || seg.starts_with('.'))
            {
                continue;
            }
            let ext = name
                .rsplit('.')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if IMAGE_EXTS.contains(&ext.as_str()) {
                images.push(name);
            }
        }
        if images.is_empty() {
            return Err(AppError::Zip("CBZ 中没有找到图片文件".into()));
        }

        // 自然排序：文件名中的数字段按数值比较（img2 < img10）
        images.sort_by(|a, b| natural_cmp(a, b));

        let book_id = make_book_id(path, file_size);
        let meta = BookMeta {
            id: book_id.clone(),
            title: title_from_path(path),
            author: None,
            publisher: None,
            language: None,
            format: BookFormat::Cbz,
            file_size,
            file_path: path.to_string_lossy().to_string(),
            total_chapters: 1,
            cover_resource: Some(images[0].clone()),
        };

        // 单章模型：整本漫画一个章节，内容为有序图片列表
        let toc = vec![TocItem {
            id: "cbz-root".into(),
            label: "漫画".into(),
            chapter_index: 0,
            anchor: None,
            children: vec![],
        }];

        Ok((
            meta,
            toc,
            Box::new(CbzBook {
                book_id,
                archive: Mutex::new(archive),
                images,
                title: title_from_path(path),
            }),
        ))
    }
}

impl BookSession for CbzBook {
    fn chapter_count(&self) -> usize {
        1
    }

    fn load_chapter(&self, index: usize) -> AppResult<ChapterContent> {
        if index != 0 {
            return Err(AppError::ChapterOutOfRange(index as u32));
        }
        Ok(ChapterContent {
            book_id: self.book_id.clone(),
            chapter_index: 0,
            title: self.title.clone(),
            kind: ChapterKind::Images,
            html: None,
            text: None,
            image_refs: Some(self.images.clone()),
        })
    }

    fn load_resource_bytes(&self, path: &str) -> AppResult<(String, Vec<u8>)> {
        let norm = path.replace('\\', "/");
        if !self.images.contains(&norm) {
            return Err(AppError::ResourceNotFound(norm));
        }
        let mut guard = self
            .archive
            .lock()
            .map_err(|_| AppError::other("CBZ ZIP 锁中毒"))?;
        let mut zf = guard
            .by_name(&norm)
            .map_err(|_| AppError::ResourceNotFound(norm.clone()))?;
        let mut buf = Vec::with_capacity(zf.size() as usize);
        zf.read_to_end(&mut buf)?;
        Ok((guess_mime(&norm).to_string(), buf))
    }
}

/// 自然排序比较：按「字母/数字段」切分，数字段按数值比较。
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let (mut i, mut j) = (0usize, 0usize);

    while i < a_chars.len() && j < b_chars.len() {
        let ca = a_chars[i];
        let cb = b_chars[j];
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            // 取完整数字段
            let sa: u64 = {
                let mut k = i;
                while k < a_chars.len() && a_chars[k].is_ascii_digit() {
                    k += 1;
                }
                a_chars[i..k]
                    .iter()
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            };
            let sb: u64 = {
                let mut k = j;
                while k < b_chars.len() && b_chars[k].is_ascii_digit() {
                    k += 1;
                }
                b_chars[j..k]
                    .iter()
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            };
            if sa != sb {
                return sa.cmp(&sb);
            }
            // 数值相等（前导零差异）：继续比较，跳过数字段
            while i < a_chars.len() && a_chars[i].is_ascii_digit() {
                i += 1;
            }
            while j < b_chars.len() && b_chars[j].is_ascii_digit() {
                j += 1;
            }
        } else {
            // 大小写不敏感的单字符比较（处理汉字时 to_lowercase 返回自身）
            let la = ca.to_lowercase().next().unwrap_or(ca);
            let lb = cb.to_lowercase().next().unwrap_or(cb);
            match la.cmp(&lb) {
                std::cmp::Ordering::Equal => {
                    i += 1;
                    j += 1;
                }
                other => return other,
            }
        }
    }
    (a_chars.len() - i).cmp(&(b_chars.len() - j))
}
