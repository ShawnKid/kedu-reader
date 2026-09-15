//! 解析器模块：格式探测 + 会话 trait + 分发。
//!
//! 安全约束（对应需求 8）：
//! - 本 crate 不包含任何网络代码，书籍内容只走「文件 → 内存 → IPC」；
//! - DRM 检测在解析入口前置拦截，命中一律返回 `AppError::DrmDetected`。

pub mod cbz;
pub mod covers;
pub mod epub;
pub mod fb2;
pub mod markdown;
pub mod mobi;
pub mod txt;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use base64::Engine as _;
use regex::Regex;

use crate::error::{AppError, AppResult};
use crate::model::{BookFormat, BookMeta, ChapterContent, TocItem};
use crate::state::ResourcePayload;

/// 格式专属解析会话。
/// 打开书籍时构建一次，之后随 `LoadedBook` 驻留内存；
/// `load_chapter` / `load_resource` 均为按需调用（可能较重，由调用方放入 blocking 线程）。
pub trait BookSession: Send + Sync {
    fn load_chapter(&self, index: usize) -> AppResult<ChapterContent>;
    /// 「跟随图书设定」：加载章节并还原书籍自带 CSS（提取 <style>/<link> 净化回注）。
    /// 默认回退普通加载（TXT / MD / FB2 / CBZ 等无内置 CSS 的格式无需还原）。
    fn load_chapter_styled(&self, index: usize) -> AppResult<ChapterContent> {
        self.load_chapter(index)
    }
    /// 原始资源字节（封面 / CBZ 图片 / EPUB 大图）。
    /// reader-res:// 协议直接消费它，避免 base64 往返开销。
    fn load_resource_bytes(&self, path: &str) -> AppResult<(String, Vec<u8>)>;
    /// base64 资源载荷（get_resource IPC 命令用）。
    fn load_resource(&self, path: &str) -> AppResult<ResourcePayload> {
        let (mime_type, bytes) = self.load_resource_bytes(path)?;
        Ok(ResourcePayload {
            mime_type,
            base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }
    /// 书籍总章节数（用于越界校验）。
    fn chapter_count(&self) -> usize;
}

/// 由「文件绝对路径 + 文件大小」生成稳定 id。
/// `DefaultHasher::new()` 使用固定密钥，跨进程结果稳定，进度续读依赖这一点。
pub fn make_book_id(path: &Path, file_size: u64) -> String {
    let mut hasher = DefaultHasher::new();
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .hash(&mut hasher);
    file_size.hash(&mut hasher);
    format!("bk{:016x}", hasher.finish())
}

/// 读文件头部（用于魔数探测），不足 n 字节则返回实际长度。
fn read_head(path: &Path, n: usize) -> AppResult<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; n];
    let mut read = 0usize;
    while read < n {
        match f.read(&mut buf[read..])? {
            0 => break,
            k => read += k,
        }
    }
    buf.truncate(read);
    Ok(buf)
}

/// 格式探测：先按扩展名，再按魔数校验，防止改后缀的错判。
pub fn detect_format(path: &Path) -> AppResult<BookFormat> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    let guessed = match ext.as_str() {
        "epub" => BookFormat::Epub,
        "mobi" | "prc" | "azw" => BookFormat::Mobi,
        "azw3" | "kf8" => BookFormat::Azw3,
        "txt" | "log" => BookFormat::Txt,
        "md" | "markdown" => BookFormat::Markdown,
        "cbz" => BookFormat::Cbz,
        "fb2" | "fbz" => BookFormat::Fb2,
        "pdf" => BookFormat::Pdf,
        other => return Err(AppError::UnsupportedFormat(other.to_string())),
    };

    let head = read_head(path, 72)?;
    match guessed {
        BookFormat::Epub => {
            if head.starts_with(b"PK") {
                Ok(guessed)
            } else {
                Err(AppError::other("EPUB 文件头校验失败（非 ZIP 容器）"))
            }
        }
        BookFormat::Mobi | BookFormat::Azw3 => {
            // PalmDB 魔数位于偏移 60：BOOKMOBI / TEXtREAd
            let magic_ok = head.len() >= 68
                && (&head[60..68] == b"BOOKMOBI" || &head[60..68] == b"TEXtREAd");
            if magic_ok {
                Ok(guessed)
            } else {
                Err(AppError::other("MOBI 文件头校验失败"))
            }
        }
        BookFormat::Cbz => {
            if head.starts_with(b"PK") {
                Ok(guessed)
            } else {
                Err(AppError::other("CBZ 文件头校验失败（非 ZIP 容器）"))
            }
        }
        // TXT / FB2 / PDF 不做魔数强校验
        _ => Ok(guessed),
    }
}

/// 解析入口：探测格式 → DRM 检测 → 构建对应解析会话。
/// 返回 (元数据, 目录, 会话)。
pub fn open_book_from_path(
    path_str: &str,
) -> AppResult<(BookMeta, Vec<TocItem>, Box<dyn BookSession>)> {
    let path = PathBuf::from(path_str);
    if !path.is_file() {
        return Err(AppError::other(format!("文件不存在: {}", path_str)));
    }
    let file_size = std::fs::metadata(&path)?.len();
    let format = detect_format(&path)?;

    match format {
        BookFormat::Epub => epub::EpubBook::open(path, file_size),
        BookFormat::Txt => txt::TxtBook::open(path, file_size),
        BookFormat::Markdown => markdown::MarkdownBook::open(path, file_size),
        BookFormat::Cbz => cbz::CbzBook::open(path, file_size),
        BookFormat::Mobi | BookFormat::Azw3 => mobi::MobiBook::open(path, file_size, format),
        BookFormat::Fb2 => fb2::Fb2Book::open(path, file_size),
        // PDF 由前端 pdf.js 渲染（openBook 前按扩展名拦截），不应走到这里
        BookFormat::Pdf => Err(AppError::UnsupportedFormat(
            "PDF 由阅读视图直接加载，请勿直接调用解析".into(),
        )),
    }
}

/// 文本编码检测：BOM 优先，否则 chardetng 统计猜测后用 encoding_rs 解码。
/// （encoding_rs 的 decode 自带 BOM 嗅探，但这里显式处理以便区分错误信息。）
pub(crate) fn decode_to_utf8(bytes: &[u8]) -> AppResult<String> {
    // 1. BOM 检测
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        let (s, _, had_err) = encoding_rs::UTF_8.decode(&bytes[3..]);
        if had_err {
            return Err(AppError::Encoding("UTF-8 解码存在非法字节".into()));
        }
        return Ok(s.into_owned());
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let (s, _, _) = encoding_rs::UTF_16LE.decode(&bytes[2..]);
        return Ok(s.into_owned());
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let (s, _, _) = encoding_rs::UTF_16BE.decode(&bytes[2..]);
        return Ok(s.into_owned());
    }

    // 2. chardetng 统计猜测（GBK/Big5/Shift-JIS/EUC-KR/Latin-1 等）。
    //    只喂头部采样：统计检测不需要全量字节，
    //    256KB 足以覆盖中文文本的频率特征，避免大文件全量扫描耗时。
    const ENCODING_SAMPLE_SIZE: usize = 256 * 1024;
    let sample = &bytes[..bytes.len().min(ENCODING_SAMPLE_SIZE)];
    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(sample, true);
    let enc = detector.guess(None, true);
    let (s, _, had_err) = enc.decode(bytes);
    if had_err {
        return Err(AppError::Encoding(format!(
            "以 {} 解码失败（编码检测不确定，文件可能已损坏）",
            enc.name()
        )));
    }
    Ok(s.into_owned())
}

/// ammonia 白名单清洗（供 epub / markdown / fb2 / mobi 共用）：
/// - 去掉 script/style/iframe/on* 事件等所有危险内容（书籍可能携带恶意 HTML）；
/// - 允许 id 属性（目录锚点跳转依赖它）；
/// - img src 的 data: URI 与 reader-res 协议在清洗后才改写，不会被 scheme 审查误伤。
///
/// `preserve_styles`（「跟随图书设定」模式）：额外放行 `class` / `style` 属性。
/// 两者均为惰性呈现属性，无脚本执行面；`style` 值中的动态构造
/// （expression / behavior / javascript: 等）由 [`harden_css`] 统一移除。
/// 注意 `<style>` 块不在此放行（ammonia clean_content_tags 连内容移除），
/// 书籍 CSS 由 EPUB 会话在清洗前提取、净化后单独回注。
pub(crate) fn sanitize_book_html(html: &str, preserve_styles: bool) -> String {
    use ammonia::Builder;
    use std::collections::{HashMap, HashSet};

    let tags: HashSet<&str> = [
        "p", "div", "span", "br", "hr", "h1", "h2", "h3", "h4", "h5", "h6",
        "em", "strong", "b", "i", "u", "s", "sup", "sub", "small", "big",
        "blockquote", "q", "cite", "pre", "code",
        "ul", "ol", "li", "dl", "dt", "dd",
        "table", "thead", "tbody", "tfoot", "tr", "th", "td", "caption",
        "img", "figure", "figcaption", "a", "ruby", "rt", "rp",
        // EPUB 3 脚注/尾注语义容器：<aside epub:type="footnote">
        "aside",
    ]
    .into_iter()
    .collect();

    // epub:type / role：脚注识别依赖（noteref / footnote / doc-noteref 等语义值），
    // data-fn-text：后端内嵌的注释体文本（跨章脚注悬浮预览，Markdown / FB2）。
    // 三者均为惰性描述属性，无脚本执行面
    let mut generic_attrs: HashSet<&str> = [
        "id", "lang", "dir", "title", "epub:type", "role", "data-fn-text",
    ]
    .into_iter()
    .collect();
    if preserve_styles {
        // 「跟随图书设定」：class 承载书籍排版类名（如 kindle-cn-kai 楷体段），
        // style 承载内联呈现属性。均为惰性属性，动态构造由 harden_css 兜底。
        generic_attrs.insert("class");
        generic_attrs.insert("style");
    }
    let schemes: HashSet<&str> = ["data", "mailto"].into_iter().collect();

    let img_attrs: HashSet<&str> = ["src", "alt", "width", "height"].into_iter().collect();
    let a_attrs: HashSet<&str> = ["href", "title"].into_iter().collect();
    let mut tag_attrs: HashMap<&str, HashSet<&str>> = HashMap::new();
    tag_attrs.insert("img", img_attrs);
    tag_attrs.insert("a", a_attrs);

    let cleaned = Builder::default()
        .tags(tags)
        .generic_attributes(generic_attrs)
        .tag_attributes(tag_attrs)
        .url_schemes(schemes)
        .url_relative(ammonia::UrlRelative::PassThrough)
        .link_rel(None)
        .clean(html)
        .to_string();

    if preserve_styles {
        harden_css(&cleaned)
    } else {
        cleaned
    }
}

/// CSS 危险构造硬化（「跟随图书设定」的 style 属性 / 书籍 CSS 二次防线）：
/// 移除 IE 动态属性、行为绑定与脚本协议等可执行面；残值成为无效声明被浏览器忽略。
pub(crate) fn harden_css(css: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)expression\s*\(|behavior\s*:|-moz-binding|javascript\s*:").expect("css harden regex")
    });
    re.replace_all(css, "").into_owned()
}

/// 根据文件名/资源名猜测 MIME（EPUB manifest 可能缺失 media-type，兜底用）。
pub fn guess_mime(name: &str) -> &'static str {
    let lower = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match lower.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "xhtml" | "html" | "htm" => "application/xhtml+xml",
        "css" => "text/css",
        "ncx" => "application/x-dtbncx+xml",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// 文件名兜底标题（无元数据时使用）。
pub fn title_from_path(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "未命名书籍".to_string())
}

/// HTML 属性值转义（注释体文本内嵌 data-fn-text 属性时用）。
pub(crate) fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// HTML 实体反转义（MOBI/FB2 元数据与目录文本用）。
/// 覆盖常见命名实体 + 十进制/十六进制数字实体；未知实体原样保留。
pub fn unescape_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        // 实体最长约 10 字符（&#x0000;），找不到合法分号则当普通字符
        let matched = tail
            .find(';')
            .filter(|&e| (2..=12).contains(&e))
            .and_then(|e| {
                let ent = &tail[1..e];
                let decoded = match ent {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some('\u{a0}'),
                    _ => {
                        let num = ent
                            .strip_prefix("#x")
                            .or_else(|| ent.strip_prefix("#X"))
                            .map(|h| u32::from_str_radix(h, 16).ok())
                            .unwrap_or_else(|| ent.strip_prefix('#').map(|d| d.parse::<u32>().ok()).unwrap_or(None));
                        num.and_then(char::from_u32)
                    }
                };
                decoded.map(|c| (c, e + 1))
            });
        match matched {
            Some((c, len)) => {
                out.push(c);
                rest = &tail[len..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// reader-res 资源 URL（Windows 与其他平台两种形式，与 epub.rs 同规则）。
pub fn resource_url(book_id: &str, path: &str) -> String {
    #[cfg(windows)]
    {
        format!("http://reader-res.localhost/{book_id}/{}", encode_res_path(path))
    }
    #[cfg(not(windows))]
    {
        format!("reader-res://localhost/{book_id}/{}", encode_res_path(path))
    }
}

/// 极简资源路径百分号编码（只处理会破坏 URL 的字符）。
fn encode_res_path(path: &str) -> String {
    path.replace(' ', "%20")
        .replace('#', "%23")
        .replace('?', "%3F")
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 脚注语义必须穿透清洗链（前端悬浮预览依赖 epub:type / role / aside）。
    #[test]
    fn sanitize_keeps_epub_footnote_semantics() {
        let html = concat!(
            r##"<p>正文<sup><a epub:type="noteref" href="#fn1">[1]</a></sup></p>"##,
            r##"<aside epub:type="footnote" id="fn1"><p><a href="#fnref1">&#8617;</a>注释内容</p></aside>"##,
            r##"<a role="doc-noteref" href="#fn2">2</a>"##,
            // 危险内容仍要被拦下
            r##"<a onclick="evil()" href="javascript:alert(1)">x</a>"##,
        );
        let out = sanitize_book_html(html, false);
        assert!(out.contains(r#"epub:type="noteref""#), "noteref 属性丢失: {out}");
        assert!(out.contains("<aside"), "aside 标签丢失: {out}");
        assert!(out.contains(r#"epub:type="footnote""#));
        assert!(out.contains(r#"role="doc-noteref""#));
        assert!(!out.to_lowercase().contains("onclick"));
        assert!(!out.to_lowercase().contains("javascript:"));
    }
}
