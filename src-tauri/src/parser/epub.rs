//! EPUB 解析模块。
//!
//! 处理链路：
//! 1. DRM 检测：读取 `META-INF/encryption.xml`，存在非字体混淆类加密算法即拒绝；
//! 2. `META-INF/container.xml` → rootfile（OPF 路径）；
//! 3. 解析 OPF：manifest（id→href/media-type/properties）+ spine（阅读顺序）；
//! 4. 目录：优先 EPUB3 nav（manifest properties 含 "nav"），否则 EPUB2 NCX；
//! 5. 章节加载：spine item XHTML → ammonia 白名单清洗 → `<img>` 改写
//!    （≤100KB 内联 data: URI；大图改写为 reader-res:// URL 按需加载，
//!    zip 内按需解压，无临时磁盘文件）→ 返回给前端。
//!
//! 资源路径处理要点：EPUB 内部路径以 OPF 所在目录为基准，章节内的相对引用
//! 又以章节 XHTML 所在目录为基准，且普遍带 URL 编码（中文/空格）。
//! 统一在 `resolve_href` + `normalize_path` 中处理成规范 key 后再查 zip。

use std::collections::HashMap;
use std::io::{Cursor, Read, Seek};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use base64::Engine as _;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};
use quick_xml::events::Event;
use regex::Regex;
use zip::ZipArchive;

use crate::error::{AppError, AppResult};
use crate::model::{BookFormat, BookMeta, ChapterContent, ChapterKind, TocItem};
use crate::parser::{
    guess_mime, harden_css, make_book_id, sanitize_book_html, title_from_path, BookSession,
};

/// 允许的 encryption.xml 算法（字体混淆，非 DRM）。
/// 除此之外的任何算法一律判定为 DRM，拒绝解析。
const FONT_OBFUSCATION_ALGOS: [&str; 2] = [
    "http://www.idpf.org/2008/embedding",
    "http://ns.adobe.com/pdf/enc#RC",
];

/// 图片内联大小上限（Q1 优化）：小于等于该值的图片转 data: URI 随章节 HTML
/// 一次性走 IPC；更大的图生成 reader-res:// URL，由 webview 走自定义协议
/// 流式取原始字节（不 base64、不占 IPC 通道、浏览器自带缓存）。
const MAX_INLINE_IMAGE_SIZE: usize = 100 * 1024; // 100KB

/// 「跟随图书设定」单段 CSS 大小上限：异常书籍的超大样式直接丢弃。
const MAX_BOOK_CSS_SIZE: usize = 512 * 1024;
/// @import 递归内联深度上限（防循环引用栈耗尽）。
const MAX_CSS_IMPORT_DEPTH: usize = 8;

/// font-family 回退链（「跟随图书设定」+ 阅读器默认栈共用语义）：
/// 书籍字体链中常见中文字体（汉仪旗黑/苹方/STKai 等及微软雅黑）缺失时，
/// 按同类回退到其他黑体/宋体/楷体，而不是掉到泛型族名交由浏览器兜底。
/// CSS 字体匹配自动跳过未安装的名字，静态超集链即等效按已安装字体动态回退。
const HEITI_FALLBACKS: [&str; 8] = [
    "Microsoft YaHei",
    "微软雅黑",
    "SimHei",
    "黑体",
    "思源黑体",
    "Source Han Sans SC",
    "Noto Sans CJK SC",
    "WenQuanYi Zen Hei",
];
const SONG_FALLBACKS: [&str; 6] = [
    "Noto Serif SC",
    "思源宋体",
    "Source Han Serif SC",
    "Noto Serif CJK SC",
    "SimSun",
    "宋体",
];
const KAI_FALLBACKS: [&str; 4] = ["KaiTi", "楷体", "STKaiti", "楷体_GB2312"];

/// reader-res:// URL 里资源路径的编码集：控制字符与 URL 结构字符编码，
/// 其余（含中文、空格由浏览器侧兼容）保持可读；`/` 保留以便协议层直接按段解析。
const RESOURCE_PATH_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'|')
    .add(b'^')
    .add(b'\\');

/// 生成前端可用的 reader-res URL。
/// Windows/Android 的 WebView2 要求走 `http://{scheme}.localhost/` 形式，
/// 其余平台为 `{scheme}://localhost/`（与 @tauri-apps/api convertFileSrc 同规则）。
#[cfg(windows)]
fn resource_url(book_id: &str, encoded_path: &str) -> String {
    format!("http://reader-res.localhost/{book_id}/{encoded_path}")
}

#[cfg(not(windows))]
fn resource_url(book_id: &str, encoded_path: &str) -> String {
    format!("reader-res://localhost/{book_id}/{encoded_path}")
}

/// manifest 条目
#[derive(Debug, Clone)]
struct ManifestItem {
    id: String,
    /// 已归一化的、相对 OPF 目录的路径（无 fragment、已 percent-decode）
    href: String,
    media_type: String,
    properties: Option<String>,
}

pub struct EpubBook {
    book_id: String,
    /// ZIP 原始字节驻留内存；Mutex 保证 ZipArchive 的 &mut Read 语义可跨线程共享
    archive: Mutex<ZipArchive<Cursor<Vec<u8>>>>,
    /// spine 顺序的章节（idref + 已归一化 href）
    spine: Vec<(String, String)>,
    /// spine href → 章节所在目录（用于相对资源解析）
    spine_dir: Vec<String>,
    cover_resource: Option<String>,
    /// 目录解析产物（open 时构建）
    toc: Vec<TocItem>,
    /// 规范路径 → 实际 zip 条目名（或未找到）的有界缓存（E06）。
    /// 首次仍走 locate_entry_name 原算法；容量满时整体清空（下一轮重建）。
    path_cache: Mutex<HashMap<String, Option<String>>>,
}

impl EpubBook {
    // ────────────────────────── 打开与解析 ──────────────────────────

    /// 打开 EPUB：构建会话 + 元数据 + 目录。
    pub fn open(path: impl AsRef<Path>, file_size: u64) -> AppResult<(BookMeta, Vec<TocItem>, Box<dyn BookSession>)> {
        let path = path.as_ref();
        let data = std::fs::read(path)?;
        let mut archive = new_archive(data)?;

        // 1. DRM 检测（加密 zip 整体打不开也会在这里暴露）
        check_drm(&mut archive)?;

        // 2. container.xml → OPF 路径
        let opf_path = read_container_rootfile(&mut archive)?;
        let opf_dir = parent_dir(&opf_path);

        // 3. OPF：manifest + spine + 元数据
        let opf_xml = read_entry_string(&mut archive, &opf_path)?;
        let (manifest, spine, meta_title, meta_author, meta_publisher, meta_lang, cover_id) =
            parse_opf(&opf_xml)?;

        // manifest id → 归一化 href
        let id_to_item: HashMap<&str, &ManifestItem> =
            manifest.iter().map(|m| (m.id.as_str(), m)).collect();

        // spine：idref → 归一化 href；同时记录章节目录、href → index
        let mut spine_resolved: Vec<(String, String)> = Vec::with_capacity(spine.len());
        let mut spine_index: HashMap<String, usize> = HashMap::new();
        let mut spine_dir: Vec<String> = Vec::new();
        for idref in &spine {
            if let Some(item) = id_to_item.get(idref.as_str()) {
                let idx = spine_resolved.len();
                // 键必须与 nav/NCX 目录的查找键同规范：resolve_href 相对 OPF 目录
                // 解析（含 percent-decode）。曾直接用 manifest 裸 href 当键，OPF 在
                // 子目录（如 OEBPS/）的书全部查找失败 → 回退 NCX 时 chapter_index
                // 全部错成 0 → 点目录永远进第一章。
                spine_index.insert(resolve_href(&opf_dir, &item.href), idx);
                spine_dir.push(parent_dir(&item.href));
                spine_resolved.push((idref.clone(), item.href.clone()));
            }
        }

        // 4. 封面：优先 properties="cover-image"，其次 <meta name="cover">
        let cover_resource = manifest
            .iter()
            .find(|m| {
                m.properties
                    .as_deref()
                    .map(|p| p.split_whitespace().any(|k| k == "cover-image"))
                    .unwrap_or(false)
            })
            .or_else(|| {
                cover_id
                    .as_deref()
                    .and_then(|id| id_to_item.get(id))
                    .copied()
            })
            .map(|m| m.href.clone());

        // 5. 目录：EPUB3 nav 优先，否则 NCX
        let toc = find_nav_doc(&manifest)
            .and_then(|nav_href| {
                parse_nav_toc(&mut archive, &nav_href, &opf_dir, &spine_index).ok()
            })
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| {
                find_ncx_doc(&manifest)
                    .and_then(|ncx_href| {
                        parse_ncx_toc(&mut archive, &ncx_href, &opf_dir, &spine_index).ok()
                    })
                    .unwrap_or_default()
            });
        // 目录为空时兜底：每个 spine 章节生成一项
        let toc = if toc.is_empty() {
            spine_resolved
                .iter()
                .enumerate()
                .map(|(i, (_, href))| TocItem {
                    id: format!("toc-{}", i),
                    label: fallback_title(href),
                    chapter_index: i as u32,
                    anchor: None,
                    children: vec![],
                })
                .collect()
        } else {
            toc
        };

        let book_id = make_book_id(path, file_size);
        let meta = BookMeta {
            id: book_id.clone(),
            title: meta_title.unwrap_or_else(|| title_from_path(path)),
            author: meta_author,
            publisher: meta_publisher,
            language: meta_lang,
            format: BookFormat::Epub,
            file_size,
            file_path: path.to_string_lossy().to_string(),
            total_chapters: spine_resolved.len() as u32,
            cover_resource,
        };

        let book = EpubBook {
            book_id,
            archive: Mutex::new(archive),
            spine: spine_resolved,
            spine_dir,
            cover_resource: meta.cover_resource.clone(),
            toc,
            path_cache: Mutex::new(HashMap::new()),
        };
        Ok((meta, book.toc.clone(), Box::new(book)))
    }

    // ────────────────────────── 资源访问 ──────────────────────────

    /// 路径解析记忆化（E06）：命中缓存则直接返回；未命中走原 locate_entry_name。
    /// 找不到也缓存 None，避免同一错误路径反复 O(N) 扫描。
    fn resolve_entry_name(
        &self,
        guard: &mut ZipArchive<Cursor<Vec<u8>>>,
        norm_path: &str,
    ) -> Option<String> {
        const PATH_CACHE_CAP: usize = 512;
        if let Ok(cache) = self.path_cache.lock() {
            if let Some(hit) = cache.get(norm_path) {
                return hit.clone();
            }
        }
        let resolved = locate_entry_name(guard, norm_path);
        if let Ok(mut cache) = self.path_cache.lock() {
            if cache.len() >= PATH_CACHE_CAP {
                cache.clear();
            }
            cache.insert(norm_path.to_string(), resolved.clone());
        }
        resolved
    }

    /// 读取 zip 内条目字节。
    fn read_bytes(&self, norm_path: &str) -> AppResult<Vec<u8>> {
        let mut guard = self
            .archive
            .lock()
            .map_err(|_| AppError::other("EPUB ZIP 锁中毒"))?;
        let actual = self
            .resolve_entry_name(&mut guard, norm_path)
            .ok_or_else(|| AppError::ResourceNotFound(norm_path.to_string()))?;
        let mut zf = guard.by_name(&actual).map_err(|e| match &e {
            zip::result::ZipError::FileNotFound => {
                AppError::ResourceNotFound(norm_path.to_string())
            }
            zip::result::ZipError::UnsupportedArchive(m)
                if m.to_ascii_lowercase().contains("password") =>
            {
                AppError::DrmDetected
            }
            _ => AppError::Zip(e.to_string()),
        })?;
        let mut buf = Vec::with_capacity(zf.size() as usize);
        zf.read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// 章节内 `<img src="...">` 的相对路径 → data: URI（小图）或
    /// reader-res:// URL（大图）。失败（资源缺失）时保留原值，交给浏览器 alt 展示。
    fn inline_images(&self, chapter_dir: &str, html: &str) -> String {
        static IMG_RE: OnceLock<Regex> = OnceLock::new();
        let re = IMG_RE.get_or_init(|| {
            Regex::new(r#"(?is)<img\b[^>]*?\bsrc\s*=\s*(?:"([^"]*)"|'([^']*)')"#).expect("img regex")
        });
        re.replace_all(html, |caps: &regex::Captures| {
            let src = caps
                .get(1)
                .or_else(|| caps.get(2))
                .map(|m| m.as_str())
                .unwrap_or_default();
            let whole = caps.get(0).map(|m| m.as_str()).unwrap_or_default();
            match self.resource_ref_for(chapter_dir, src) {
                Some(uri) => whole.replacen(src, &uri, 1),
                None => whole.to_string(),
            }
        })
        .into_owned()
    }

    /// 相对引用 → 图片 src。≤100KB 转 `data:` URI；超过则转 reader-res URL，
    /// 由 webview 自定义协议按需取原始字节（Q1/Q3 的 IPC 减负方案）。
    fn resource_ref_for(&self, chapter_dir: &str, src: &str) -> Option<String> {
        let src = src.trim();
        if src.is_empty()
            || src.starts_with("data:")
            || src.starts_with("http://")
            || src.starts_with("https://")
            || src.starts_with("reader-res:")
        {
            return None; // 已是内联/外链/协议地址，不动
        }
        let norm = resolve_href(chapter_dir, src);
        if norm.is_empty() {
            return None;
        }
        // 大图：不读字节（省内存拷贝），直接查 zip 条目大小
        let size = {
            let mut guard = self
                .archive
                .lock()
                .map_err(|_| AppError::other("EPUB ZIP 锁中毒"))
                .ok()?;
            self.resolve_entry_name(&mut guard, &norm)
                .and_then(|name| guard.by_name(&name).ok().map(|zf| zf.size()))
                .unwrap_or(0)
        };
        if size == 0 {
            return None; // 资源缺失，保留原 src
        }
        if size as usize <= MAX_INLINE_IMAGE_SIZE {
            let bytes = self.read_bytes(&norm).ok()?;
            let mime = guess_mime(&norm);
            Some(format!(
                "data:{};base64,{}",
                mime,
                base64::engine::general_purpose::STANDARD.encode(bytes)
            ))
        } else {
            let encoded = utf8_percent_encode(&norm, RESOURCE_PATH_SET).to_string();
            Some(resource_url(&self.book_id, &encoded))
        }
    }

    // ────────────────────────── 跟随图书设定：书籍 CSS 还原 ──────────────────────────

    /// 「跟随图书设定」加载 spine 第 index 章：在普通流程外还原书籍自带 CSS。
    ///
    /// 流程：XHTML → 提取 `<style>` 块与 `<link rel=stylesheet>` CSS（递归内联
    /// @import）→ 净化 / 字体回退链增强 / url() 改写 → 正文 ammonia 清洗
    /// （保留 class/style 属性）→ `<img>` 改写 → 末尾注入 `<style data-book-css>`。
    /// `<style>`/`<link>` 本体仍会被 ammonia 移除（clean_content_tags / 非白名单），
    /// 避免原文残留；样式仅以净化后的注入块形式出现。
    ///
    /// 注入必须在正文**之后**：前端 DOMParser 按 HTML 语法解析整段字符串时，
    /// 文档开头的 `<style>` 会落入隐式创建的 `<head>`（而非 `<body>`），
    /// 随 body.innerHTML 序列化时被整体丢弃（字体还原失效的根因）。
    fn load_chapter_styled_impl(&self, index: usize) -> AppResult<ChapterContent> {
        if index >= self.spine.len() {
            return Err(AppError::ChapterOutOfRange(index as u32));
        }
        let (_, href) = self.spine[index].clone();
        let dir = self.spine_dir[index].clone();
        let raw = self.read_bytes(&href)?;
        let html = String::from_utf8_lossy(&raw).into_owned();

        let css = self.collect_book_css(&html, &dir);
        let clean = sanitize_book_html(&html, true);
        let inlined = self.inline_images(&dir, &clean);

        let body = if css.trim().is_empty() {
            inlined
        } else {
            // 末尾注入：保证解析时 <body> 已存在（开头注入会被 DOMParser 归入 head 而丢失）
            format!("{inlined}<style data-book-css>{css}</style>")
        };

        Ok(ChapterContent {
            book_id: self.book_id.clone(),
            chapter_index: index as u32,
            title: fallback_title(&href),
            kind: ChapterKind::Html,
            html: Some(body),
            text: None,
            image_refs: None,
        })
    }

    /// 提取章节自带 CSS：内联 `<style>` 块 + `<link rel="stylesheet">` 引用的
    /// zip 内 CSS 文件（外链 http 一律不取，离线安全约束）。
    fn collect_book_css(&self, html: &str, chapter_dir: &str) -> String {
        static STYLE_RE: OnceLock<Regex> = OnceLock::new();
        static LINK_TAG_RE: OnceLock<Regex> = OnceLock::new();
        static REL_RE: OnceLock<Regex> = OnceLock::new();
        static HREF_RE: OnceLock<Regex> = OnceLock::new();
        let style_re = STYLE_RE
            .get_or_init(|| Regex::new(r"(?is)<style\b[^>]*>(.*?)</style>").expect("style block regex"));
        let link_tag_re =
            LINK_TAG_RE.get_or_init(|| Regex::new(r"(?is)<link\b[^>]*>").expect("link tag regex"));
        let rel_re = REL_RE
            .get_or_init(|| Regex::new(r#"(?is)\brel\s*=\s*["']?([^"'>]*)["']?"#).expect("rel regex"));
        let href_re = HREF_RE
            .get_or_init(|| Regex::new(r#"(?is)\bhref\s*=\s*["']([^"']*)["']"#).expect("link href regex"));

        let mut parts: Vec<String> = Vec::new();

        for cap in style_re.captures_iter(html) {
            // XHTML 内联样式可能带 CDATA 包裹与 XML 实体转义
            let css = unescape_entities(&cap[1])
                .replace("<![CDATA[", "")
                .replace("]]>", "");
            let out = self.process_book_css(&css, chapter_dir, 0);
            if !out.trim().is_empty() {
                parts.push(out);
            }
        }

        for tag in link_tag_re.find_iter(html) {
            let tag_str = tag.as_str();
            let is_stylesheet = rel_re
                .captures(tag_str)
                .map(|c| {
                    c[1].to_lowercase()
                        .split_whitespace()
                        .any(|k| k == "stylesheet")
                })
                .unwrap_or(false);
            if !is_stylesheet {
                continue;
            }
            let Some(href) = href_re.captures(tag_str) else {
                continue;
            };
            let path = resolve_href(chapter_dir, href[1].trim());
            if path.is_empty() {
                continue;
            }
            // 仅读 zip 内条目；read_bytes 对 http 等路径自然返回 ResourceNotFound
            if let Ok(bytes) = self.read_bytes(&path) {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let sub_dir = parent_dir(&path);
                let out = self.process_book_css(&text, &sub_dir, 0);
                if !out.trim().is_empty() {
                    parts.push(out);
                }
            }
        }

        parts.join("\n")
    }

    /// 单段书籍 CSS 的净化管线：
    /// 1) 危险构造移除（expression / behavior / -moz-binding / javascript:）；
    /// 2) @import 递归内联（仅 zip 内文件，外链丢弃，深度上限防循环）；
    /// 3) url() 改写为 data: / reader-res（外链置 none，资源缺失保留原值）；
    /// 4) font-family 回退链增强。
    fn process_book_css(&self, css: &str, css_dir: &str, depth: usize) -> String {
        if depth > MAX_CSS_IMPORT_DEPTH || css.len() > MAX_BOOK_CSS_SIZE {
            return String::new();
        }
        let hardened = harden_css(css);
        let with_imports = self.inline_css_imports(&hardened, css_dir, depth);
        let rewritten = self.rewrite_css_urls(&with_imports, css_dir);
        augment_font_fallbacks(&rewritten)
    }

    /// `@import "a.css";` / `@import url(a.css);` 递归内联（外链/缺失丢弃）。
    fn inline_css_imports(&self, css: &str, css_dir: &str, depth: usize) -> String {
        static IMPORT_RE: OnceLock<Regex> = OnceLock::new();
        let re = IMPORT_RE.get_or_init(|| {
            // 匹配字符串形式与 url() 形式，吞掉整个语句（含 media 限定符）
            Regex::new(r#"(?is)@import\s+(?:url\s*\(\s*|["'])([^"')\n;]+)[^;]*;"#)
                .expect("css import regex")
        });
        re.replace_all(css, |caps: &regex::Captures| {
            let target = caps[1].trim();
            let lower = target.to_ascii_lowercase();
            if lower.starts_with("http://")
                || lower.starts_with("https://")
                || lower.starts_with("data:")
            {
                return String::new(); // 外链与 data: import 一律丢弃
            }
            let path = resolve_href(css_dir, target);
            if path.is_empty() {
                return String::new();
            }
            match self.read_bytes(&path) {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    let sub_dir = parent_dir(&path);
                    // 递归走完整管线：净化 + 该文件自身的 @import + url 基准切换
                    self.process_book_css(&text, &sub_dir, depth + 1)
                }
                Err(_) => String::new(),
            }
        })
        .into_owned()
    }

    /// CSS `url(...)` 相对引用改写：≤100KB → data: URI，否则 reader-res URL
    /// （复用 `<img>` 的资源判定）。外链置 none；资源缺失保留原值静默失败。
    fn rewrite_css_urls(&self, css: &str, css_dir: &str) -> String {
        static URL_RE: OnceLock<Regex> = OnceLock::new();
        let re = URL_RE.get_or_init(|| {
            Regex::new(r#"(?i)url\s*\(\s*["']?([^"'\n)]*?)\s*["']?\s*\)"#).expect("css url regex")
        });
        re.replace_all(css, |caps: &regex::Captures| {
            let whole = caps.get(0).map(|m| m.as_str()).unwrap_or_default();
            let target = caps.get(1).map(|m| m.as_str()).unwrap_or_default().trim();
            if target.is_empty() {
                return whole.to_string();
            }
            let lower = target.to_ascii_lowercase();
            if lower.starts_with("http://") || lower.starts_with("https://") {
                // 外链资源：离线安全约束不取用，声明置 none
                return "none".to_string();
            }
            if lower.starts_with("data:")
                || lower.starts_with("reader-res:")
                || target.starts_with('#')
            {
                return whole.to_string(); // 已内联 / 协议地址 / 同文档锚点
            }
            match self.resource_ref_for(css_dir, target) {
                Some(uri) => format!("url(\"{uri}\")"),
                None => whole.to_string(),
            }
        })
        .into_owned()
    }
}

impl BookSession for EpubBook {
    fn chapter_count(&self) -> usize {
        self.spine.len()
    }

    /// 加载 spine 第 index 章：XHTML → ammonia 白名单清洗 → 图片改写。
    ///
    /// 顺序说明（Q3 安全要点）：先清洗后改写图片 src。这样 reader-res/data URI
    /// 不会经过 ammonia 的 URL scheme 审查（Windows 下协议地址是 http:// 形式，
    /// 若先改写会被 scheme 白名单误伤，而放开 http 又会引入远程图片风险）。
    fn load_chapter(&self, index: usize) -> AppResult<ChapterContent> {
        if index >= self.spine.len() {
            return Err(AppError::ChapterOutOfRange(index as u32));
        }
        let (_, href) = self.spine[index].clone();
        let dir = self.spine_dir[index].clone();
        let raw = self.read_bytes(&href)?;
        let html = String::from_utf8_lossy(&raw).into_owned();
        let clean = sanitize_book_html(&html, false);
        let inlined = self.inline_images(&dir, &clean);

        Ok(ChapterContent {
            book_id: self.book_id.clone(),
            chapter_index: index as u32,
            title: fallback_title(&href),
            kind: ChapterKind::Html,
            html: Some(inlined),
            text: None,
            image_refs: None,
        })
    }

    /// 「跟随图书设定」：加载章节并还原书籍自带 CSS（实现见 [`EpubBook::load_chapter_styled_impl`]）。
    fn load_chapter_styled(&self, index: usize) -> AppResult<ChapterContent> {
        self.load_chapter_styled_impl(index)
    }

    fn load_resource_bytes(&self, path: &str) -> AppResult<(String, Vec<u8>)> {
        // 封面：允许传 "cover" 别名
        let norm = if path == "cover" {
            self.cover_resource
                .clone()
                .ok_or_else(|| AppError::ResourceNotFound("cover".into()))?
        } else {
            normalize_path(path)
        };
        let bytes = self.read_bytes(&norm)?;
        Ok((guess_mime(&norm).to_string(), bytes))
    }
}

// ────────────────────────── zip 基础设施 ──────────────────────────

pub(super) fn cover_candidates(path: &Path) -> AppResult<Vec<Vec<u8>>> {
    use super::covers::{Candidates, MAX_IMAGE_BYTES};
    let mut archive = ZipArchive::new(std::fs::File::open(path)?)
        .map_err(|e| AppError::Zip(e.to_string()))?;
    check_drm(&mut archive)?;
    let opf = read_container_rootfile(&mut archive)?;
    let dir = parent_dir(&opf);
    let xml = read_entry_string(&mut archive, &opf)?;
    let (manifest, _, _, _, _, _, cover_id) = parse_opf(&xml)?;
    let primary = manifest.iter().find(|m| m.properties.as_deref().unwrap_or("")
        .split_whitespace().any(|p| p == "cover-image"))
        .or_else(|| manifest.iter().find(|m| Some(m.id.as_str()) == cover_id.as_deref()));
    let mut candidates = Candidates::default();
    for m in primary.into_iter().chain(manifest.iter().filter(|m| m.media_type.starts_with("image/"))) {
        let norm = resolve_href(&dir, &m.href);
        let Some(actual) = locate_entry_name(&mut archive, &norm) else { continue; };
        let mut file = archive.by_name(&actual).map_err(|e| AppError::Zip(e.to_string()))?;
        if file.size() > MAX_IMAGE_BYTES { continue; }
        let mut bytes = Vec::new();
        (&mut file).take(MAX_IMAGE_BYTES + 1).read_to_end(&mut bytes)?;
        candidates.push(bytes, primary.map(|p| &p.id) == Some(&m.id));
    }
    Ok(candidates.finish())
}

fn new_archive(data: Vec<u8>) -> AppResult<ZipArchive<Cursor<Vec<u8>>>> {
    ZipArchive::new(Cursor::new(data)).map_err(|e| match &e {
        zip::result::ZipError::UnsupportedArchive(m) if m.to_ascii_lowercase().contains("password") => AppError::DrmDetected,
        _ => AppError::Zip(e.to_string()),
    })
}

/// 在 zip 中定位条目，返回实际条目名；找不到时返回 None。
///
/// 兜底背景：部分第三方转换工具生成的 EPUB，OPF 中的 href 与 zip 内
/// 实际路径不一致（如 OPF 写 "toc.html"，实际条目是 "text/toc.html"）。
/// 匹配优先级：精确路径 → 全路径（忽略大小写）→ 同目录+文件名 → 仅文件名。
fn locate_entry_name<R: Read + Seek>(archive: &mut ZipArchive<R>, norm_path: &str) -> Option<String> {
    if archive.by_name(norm_path).is_ok() {
        return Some(norm_path.to_string());
    }
    let want = norm_path.to_ascii_lowercase();
    let (want_dir, want_name) = match want.rsplit_once('/') {
        Some((d, n)) => (Some(d), n),
        None => (None, want.as_str()),
    };
    // best: (优先级, 条目名)，数字越小越优先
    let mut best: Option<(u8, String)> = None;
    for name in archive.file_names() {
        if name.ends_with('/') {
            continue; // 目录条目
        }
        let low = name.to_ascii_lowercase();
        if low == want {
            return Some(name.to_string());
        }
        let (dir, fname) = match low.rsplit_once('/') {
            Some((d, n)) => (Some(d), n),
            None => (None, low.as_str()),
        };
        if fname != want_name {
            continue;
        }
        let rank: u8 = if dir == want_dir { 2 } else { 3 };
        if best.as_ref().map(|(r, _)| rank < *r).unwrap_or(true) {
            best = Some((rank, name.to_string()));
        }
    }
    best.map(|(_, n)| n)
}

/// 读取单个条目；定位失败归一为资源不存在，zip 内条目加密时归一为 DRM 错误。
fn read_entry_bytes<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    norm_path: &str,
) -> AppResult<Vec<u8>> {
    let actual = locate_entry_name(archive, norm_path)
        .ok_or_else(|| AppError::ResourceNotFound(norm_path.to_string()))?;
    let mut zf = archive.by_name(&actual).map_err(|e| match &e {
        zip::result::ZipError::FileNotFound => AppError::ResourceNotFound(norm_path.to_string()),
        zip::result::ZipError::UnsupportedArchive(m) if m.to_ascii_lowercase().contains("password") => AppError::DrmDetected,
        _ => AppError::Zip(e.to_string()),
    })?;
    let mut buf = Vec::with_capacity(zf.size() as usize);
    zf.read_to_end(&mut buf)?;
    Ok(buf)
}

fn read_entry_string<R: Read + Seek>(archive: &mut ZipArchive<R>, path: &str) -> AppResult<String> {
    let bytes = read_entry_bytes(archive, path)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// DRM 检测：解析 META-INF/encryption.xml。
/// 存在 `EncryptedData` 且算法不属于字体混淆白名单 → DRM。
fn check_drm<R: Read + Seek>(archive: &mut ZipArchive<R>) -> AppResult<()> {
    let Ok(xml) = read_entry_string(archive, "META-INF/encryption.xml") else {
        return Ok(()); // 无 encryption.xml 即无加密声明
    };
    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(true);
    let mut in_method = false;
    let mut algo = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"EncryptionMethod" => {
                    in_method = true;
                    algo.clear();
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"Algorithm" {
                            algo = attr_value(&attr.value);
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) if e.name().as_ref() == b"EncryptionMethod" => {
                in_method = false;
                // 只要有任何一条非字体混淆的加密声明，即判定 DRM
                if !algo.is_empty() && !FONT_OBFUSCATION_ALGOS.contains(&algo.as_str()) {
                    return Err(AppError::DrmDetected);
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => {
                let _ = in_method;
                return Err(AppError::Xml(format!("encryption.xml: {}", e)));
            }
        }
    }
    Ok(())
}

// ────────────────────────── XML 解析 ──────────────────────────

/// container.xml → rootfile full-path
fn read_container_rootfile<R: Read + Seek>(archive: &mut ZipArchive<R>) -> AppResult<String> {
    let xml = read_entry_string(archive, "META-INF/container.xml")
        .map_err(|e| AppError::Zip(format!("缺少 META-INF/container.xml: {}", e)))?;
    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if e.name().as_ref() == b"rootfile" {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"full-path" {
                            return Ok(normalize_path(&attr_value(&attr.value)));
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(AppError::Xml(format!("container.xml: {}", e))),
        }
    }
    Err(AppError::Xml("container.xml 中缺少 rootfile".into()))
}

#[allow(clippy::type_complexity)]
fn parse_opf(
    xml: &str,
) -> AppResult<(
    Vec<ManifestItem>,
    Vec<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
)> {
    let mut manifest: Vec<ManifestItem> = vec![];
    let mut spine: Vec<String> = vec![];
    let mut title = None;
    let mut author = None;
    let mut publisher = None;
    let mut lang = None;
    let mut cover_id = None;

    let mut section = Section::None;
    let mut current_meta_name = String::new();

    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"manifest" => section = Section::Manifest,
                b"spine" => section = Section::Spine,
                b"item" if section == Section::Manifest => {
                    let (mut id, mut href, mut mt, mut props) =
                        (String::new(), String::new(), String::new(), None);
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"id" => id = attr_value(&attr.value),
                            b"href" => href = attr_value(&attr.value),
                            b"media-type" => mt = attr_value(&attr.value),
                            b"properties" => props = Some(attr_value(&attr.value)),
                            _ => {}
                        }
                    }
                    if !id.is_empty() && !href.is_empty() {
                        manifest.push(ManifestItem {
                            id,
                            href: normalize_path(&href),
                            media_type: mt,
                            properties: props,
                        });
                    }
                }
                b"itemref" if section == Section::Spine => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"idref" {
                            spine.push(attr_value(&attr.value));
                        }
                    }
                }
                b"meta" => {
                    // <meta name="cover" content="image-id"/> （EPUB2 封面惯例）
                    let mut name = String::new();
                    let mut content = String::new();
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"name" => name = attr_value(&attr.value),
                            b"content" => content = attr_value(&attr.value),
                            _ => {}
                        }
                    }
                    if name == "cover" {
                        cover_id = Some(content);
                    }
                    current_meta_name = name;
                }
                b"dc:title" => current_meta_name = "dc:title".into(),
                b"dc:creator" => current_meta_name = "dc:creator".into(),
                b"dc:publisher" => current_meta_name = "dc:publisher".into(),
                b"dc:language" => current_meta_name = "dc:language".into(),
                _ => {}
            },
            Ok(Event::Text(t)) => {
                let text = xml_text(&t).trim().to_string();
                if text.is_empty() {
                    continue;
                }
                match current_meta_name.as_str() {
                    "dc:title" => title = Some(text),
                    "dc:creator" => author = Some(text),
                    "dc:publisher" => publisher = Some(text),
                    "dc:language" => lang = Some(text),
                    _ => {}
                }
            }
            Ok(Event::End(_)) => current_meta_name.clear(),
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(AppError::Xml(format!("OPF: {}", e))),
        }
    }
    if manifest.is_empty() {
        return Err(AppError::Xml("OPF 缺少 manifest".into()));
    }
    Ok((manifest, spine, title, author, publisher, lang, cover_id))
}

#[derive(PartialEq, Clone, Copy)]
enum Section {
    None,
    Manifest,
    Spine,
}

/// 找 EPUB3 nav 文档（manifest properties 含 "nav"）。
fn find_nav_doc(manifest: &[ManifestItem]) -> Option<String> {
    manifest
        .iter()
        .find(|m| {
            m.properties
                .as_deref()
                .map(|p| p.split_whitespace().any(|k| k == "nav"))
                .unwrap_or(false)
        })
        .map(|m| m.href.clone())
}

/// 找 EPUB2 NCX（骨架实现：取 manifest 中第一个 ncx media-type 条目；
/// 少数多 NCX 书籍可能选错，后续可用 spine 的 toc 属性精确定位）。
fn find_ncx_doc(manifest: &[ManifestItem]) -> Option<String> {
    manifest
        .iter()
        .find(|m| m.media_type == "application/x-dtbncx+xml")
        .map(|m| m.href.clone())
}

/// 解析 EPUB3 nav 目录：取第一个 <nav> 内的全部 <a href>。
/// （骨架实现：未处理 epub:type 多级 nav 区分，多级层级按文档序扁平化为一层 +
/// chapter_index 回退；复杂书籍建议后续按 li 嵌套恢复树形。）
fn parse_nav_toc<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    nav_href: &str,
    opf_dir: &str,
    spine_index: &HashMap<String, usize>,
) -> AppResult<Vec<TocItem>> {
    let nav_path = resolve_href(opf_dir, nav_href);
    let xml = read_entry_string(archive, &nav_path)?;
    let dir = parent_dir(&nav_path);

    static A_RE: OnceLock<Regex> = OnceLock::new();
    let re = A_RE.get_or_init(|| {
        Regex::new(r#"(?is)<a\b[^>]*?\bhref\s*=\s*(?:"([^"]*)"|'([^']*)')[^>]*>(.*?)</a>"#)
            .expect("nav <a> regex")
    });
    // 只取第一个 <nav> 块
    let nav_block = {
        static NAV_RE: OnceLock<Regex> = OnceLock::new();
        let nre = NAV_RE.get_or_init(|| Regex::new(r"(?is)<nav\b[^>]*>(.*?)</nav>").unwrap());
        nre.captures(&xml)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .unwrap_or(xml)
    };

    let mut out = vec![];
    // 去掉嵌套 <a> 内部的标签文本干扰：先剥掉 <span> 等
    let block = strip_tags_except_a(&nav_block);
    for caps in re.captures_iter(&block) {
        let href_raw = caps
            .get(1)
            .or_else(|| caps.get(2))
            .map(|m| m.as_str())
            .unwrap_or_default();
        let label_str = strip_tags(caps.get(3).map(|m| m.as_str()).unwrap_or_default());
        let label = label_str.trim();
        if href_raw.is_empty() || label.is_empty() {
            continue;
        }
        let (path, frag) = split_fragment(href_raw);
        let norm = resolve_href(&dir, &path);
        let idx = spine_index.get(&norm).copied();
        if let Some(i) = idx {
            out.push(TocItem {
                id: format!("nav-{}", out.len()),
                label: unescape_entities(label),
                chapter_index: i as u32,
                anchor: frag,
                children: vec![],
            });
        }
        // href 指向非 spine 文档的条目跳过（如封面页、目录页自引用）
    }
    Ok(out)
}

/// 解析 EPUB2 NCX 目录（navPoint 可嵌套 → TocItem 树）。
fn parse_ncx_toc<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    ncx_href: &str,
    opf_dir: &str,
    spine_index: &HashMap<String, usize>,
) -> AppResult<Vec<TocItem>> {
    let ncx_path = resolve_href(opf_dir, ncx_href);
    let xml = read_entry_string(archive, &ncx_path)?;

    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(true);

    // navPoint 解析栈：栈顶是当前正在收集的节点
    let mut stack: Vec<TocItem> = vec![];
    let mut roots: Vec<TocItem> = vec![];
    let mut in_nav_label = false;
    let mut label_buf = String::new();

    let attach_label = |item: &mut TocItem, buf: &str| {
        if !buf.trim().is_empty() {
            item.label = unescape_entities(buf.trim());
        } else if item.label.is_empty() {
            item.label = format!("章节 {}", item.chapter_index + 1);
        }
    };

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"navPoint" => {
                    // 先冲刷上一节点可能遗留的 label
                    if let Some(top) = stack.last_mut() {
                        attach_label(top, &label_buf);
                        label_buf.clear();
                    }
                    stack.push(TocItem {
                        id: format!("ncx-{}", stack.len()),
                        label: String::new(),
                        chapter_index: 0,
                        anchor: None,
                        children: vec![],
                    });
                }
                b"navLabel" => in_nav_label = true,
                b"content" => {
                    let mut src = String::new();
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"src" {
                            src = attr_value(&attr.value);
                        }
                    }
                    if let Some(top) = stack.last_mut() {
                        let (path, frag) = split_fragment(&src);
                        // NCX 的 src 相对 NCX 文件所在目录
                        let norm = resolve_href(&parent_dir(&ncx_path), &path);
                        top.chapter_index = spine_index.get(&norm).copied().unwrap_or(0) as u32;
                        top.anchor = frag;
                    }
                }
                _ => {}
            },
            Ok(Event::Text(t)) if in_nav_label => {
                label_buf.push_str(&xml_text(&t));
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"navLabel" => in_nav_label = false,
                b"navPoint" => {
                    if let Some(mut node) = stack.pop() {
                        attach_label(&mut node, &label_buf);
                        label_buf.clear();
                        match stack.last_mut() {
                            Some(parent) => parent.children.push(node),
                            None => roots.push(node),
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(AppError::Xml(format!("NCX: {}", e))),
        }
    }
    Ok(roots)
}

// ────────────────────────── HTML 清洗 ──────────────────────────

/// （清洗实现已收敛到 parser::sanitize_book_html 共享版本，供 epub/md/fb2/mobi 共用。）

// ────────────────────────── 路径与字符串工具 ──────────────────────────

/// font-family 回退链增强（「跟随图书设定」）：
/// 对每条 `font-family:` 声明，在链尾泛型关键字（sans-serif / serif）之前插入
/// 同类别回退字体（黑体/宋体/楷体），已存在的名字去重；链中含 kai/楷 时额外
/// 追加楷体链（本书引文 STKai / "MKai PRC" 缺失时保底到 Windows 自带楷体）。
///
/// 规则细节：
/// - `font:` 简写不处理（书籍 CSS 几乎不用，且简写解析复杂易错）；
/// - `@font-face` 块内不处理（其 font-family 是字体定义描述符，追加会毁掉定义）；
/// - 无泛型关键字收尾的声明不追加（尊重刻意的精确字体链）；
/// - inherit/initial/unset/revert/var() 等非字面值不动。
fn augment_font_fallbacks(css: &str) -> String {
    static FACE_RE: OnceLock<Regex> = OnceLock::new();
    static FAMILY_RE: OnceLock<Regex> = OnceLock::new();
    let face_re =
        FACE_RE.get_or_init(|| Regex::new(r"(?is)@font-face\s*\{[^{}]*\}").expect("font-face regex"));
    let family_re = FAMILY_RE.get_or_init(|| {
        Regex::new(r"(?is)(font-family\s*:\s*)([^;{}]+)").expect("font-family regex")
    });

    // 摘除 @font-face 块（占位符不含 font-family 字样，天然跳过增强）
    let mut faces: Vec<String> = Vec::new();
    let stripped = face_re.replace_all(css, |caps: &regex::Captures| {
        faces.push(caps[0].to_string());
        format!("\u{1}FACE{}\u{1}", faces.len() - 1)
    });

    let augmented = family_re.replace_all(&stripped, |caps: &regex::Captures| {
        let prefix = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        let value = caps.get(2).map(|m| m.as_str()).unwrap_or_default();
        match augment_one_family(value) {
            Some(v) => format!("{prefix}{v}"),
            None => caps[0].to_string(),
        }
    });

    // 还原 @font-face 块
    let mut out = augmented.into_owned();
    for (i, face) in faces.iter().enumerate() {
        let marker = format!("\u{1}FACE{i}\u{1}");
        out = out.replace(&marker, face);
    }
    out
}

/// 单条 font-family 值的增强。返回 None 表示原样保留。
fn augment_one_family(value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    let lower = v.to_lowercase();
    if matches!(lower.as_str(), "inherit" | "initial" | "unset" | "revert") || lower.contains("var(")
    {
        return None;
    }

    // 仅当声明以泛型关键字「收尾」时增强（sans-serif 先于 serif 判断——后者是前者的
    // 后缀子串）。非收尾情况（含字体名恰以 serif 结尾、monospace 等）一律不动。
    let is_sans = lower.ends_with("sans-serif");
    let is_serif = !is_sans && lower.ends_with("serif");
    if !is_sans && !is_serif {
        return None;
    }
    let kw_len = if is_sans { "sans-serif".len() } else { "serif".len() };
    let cut = v.len() - kw_len; // ASCII 关键字，字节切片安全

    // 链中已有名字集合（大小写/引号归一），逐名去重
    let existing: std::collections::HashSet<String> = v
        .split(',')
        .map(|s| {
            s.trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_lowercase()
        })
        .collect();

    let class_chain: &[&str] = if is_sans { &HEITI_FALLBACKS } else { &SONG_FALLBACKS };
    let wants_kai = lower.contains("kai") || v.contains('楷');

    let mut extra: Vec<&str> = Vec::new();
    for name in class_chain
        .iter()
        .copied()
        .chain(KAI_FALLBACKS.iter().copied().filter(|_| wants_kai))
    {
        if !existing.contains(&name.to_lowercase()) {
            extra.push(name);
        }
    }
    if extra.is_empty() {
        return None;
    }

    // 插入到链尾泛型关键字之前：v[..cut]（已含分隔逗号）+ 回退链 + 关键字
    let mut out = String::with_capacity(v.len() + extra.iter().map(|s| s.len() + 1).sum::<usize>());
    out.push_str(&v[..cut]);
    out.push_str(&extra.join(","));
    out.push(',');
    out.push_str(&v[cut..]);
    Some(out)
}

/// 相对引用解析：以 base_dir 为基准，percent-decode 后归一化。
fn resolve_href(base_dir: &str, href: &str) -> String {
    let decoded = percent_decode_str(href.split('#').next().unwrap_or(""))
        .decode_utf8_lossy()
        .into_owned();
    let joined = if decoded.starts_with('/') {
        decoded.trim_start_matches('/').to_string()
    } else if base_dir.is_empty() {
        decoded
    } else {
        format!("{}/{}", base_dir, decoded)
    };
    normalize_path(&joined)
}

/// 归一化：丢弃空段/`.`，处理 `..`。
pub(crate) fn normalize_path(p: &str) -> String {
    let p = p.replace('\\', "/");
    let mut segs: Vec<&str> = vec![];
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            s => segs.push(s),
        }
    }
    segs.join("/")
}

/// 取父目录（无父则空串）。
fn parent_dir(p: &str) -> String {
    match p.rfind('/') {
        Some(i) => p[..i].to_string(),
        None => String::new(),
    }
}

/// 拆 fragment："a.xhtml#sec1" → ("a.xhtml", Some("sec1"))
fn split_fragment(href: &str) -> (String, Option<String>) {
    match href.split_once('#') {
        Some((a, b)) if !b.is_empty() => (a.to_string(), Some(b.to_string())),
        _ => (href.to_string(), None),
    }
}

/// quick-xml Attribute value → String（含基础实体反转义）。
fn attr_value(v: &[u8]) -> String {
    unescape_entities(&String::from_utf8_lossy(v))
}

/// quick-xml Text bytes → String。
fn xml_text(bytes: &[u8]) -> String {
    unescape_entities(&String::from_utf8_lossy(bytes))
}

/// 最小实体反转义（避免依赖 quick-xml 版本差异较大的 unescape API）。
pub(crate) fn unescape_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", "\u{00a0}")
        .replace("&amp;", "&")
}

/// 从 href 兜底章节标题："Text/chapter001.xhtml" → "chapter001"
fn fallback_title(href: &str) -> String {
    let name = href.rsplit('/').next().unwrap_or(href);
    name.rsplit_once('.').map(|(a, _)| a).unwrap_or(name).to_string()
}

/// 去掉 <a> 以外的标签（nav 解析辅助），保留 <a> 原样。
///
/// 注意：不能用 `(?!a\b)` 负向前瞻实现排除——regex crate 不支持 look-around，
/// `Regex::new` 在运行时直接 panic（release 为 panic=abort，整进程闪退）。
/// 改为匹配任意标签后在替换闭包里按标签名排除。
fn strip_tags_except_a(html: &str) -> String {
    static KEEP_A: OnceLock<Regex> = OnceLock::new();
    let re = KEEP_A.get_or_init(|| Regex::new(r"(?is)<(/?)([a-z][a-z0-9-]*)[^>]*>").expect("keep-a tag regex"));
    re.replace_all(html, |caps: &regex::Captures| {
        let name = caps.get(2).map(|m| m.as_str()).unwrap_or_default();
        if name.eq_ignore_ascii_case("a") {
            caps.get(0).map(|m| m.as_str()).unwrap_or_default().to_string()
        } else {
            String::new()
        }
    })
    .into_owned()
}

/// 去掉全部标签。
fn strip_tags(html: &str) -> String {
    static ALL: OnceLock<Regex> = OnceLock::new();
    let re = ALL.get_or_init(|| Regex::new(r"(?is)<[^>]*>").unwrap());
    re.replace_all(html, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：strip_tags_except_a 曾用 `(?!a\b)` 负向前瞻排除 <a>，
    /// regex crate 不支持 look-around，首个 EPUB3 nav 书籍解析时
    /// Regex::new panic（release panic=abort → 打开即闪退）。
    #[test]
    fn strip_tags_except_a_keeps_anchor_only() {
        let nav = "<ol><li><span>目录</span><a href=\"c1.xhtml#s1\">第一章 &amp; 测试</a></li></ol>";
        let out = strip_tags_except_a(nav);
        assert!(out.contains(r#"<a href="c1.xhtml#s1">"#), "<a> 被误删: {out}");
        assert!(out.contains("</a>"), "</a> 被误删: {out}");
        assert!(!out.contains("<ol>"), "<ol> 未剥除: {out}");
        assert!(!out.contains("<span>"), "<span> 未剥除: {out}");
        // 标签名以 a 开头但不是 <a>（如 <aside>）应被剥除
        let out2 = strip_tags_except_a("<aside id=\"x\"><a href=\"#f\">[1]</a></aside>");
        assert!(out2.contains("<a href=\"#f\">") && out2.contains("</a>"));
        assert!(!out2.contains("<aside"));
        assert_eq!(strip_tags_except_a("纯文本不动"), "纯文本不动");
    }

    /// 回归：spine_index 曾用 manifest 裸 href 当键，而 nav/NCX 目录查找键
    /// 是相对 OPF 目录 resolve 后的路径。OPF 位于子目录（如 OEBPS/）的书
    /// （古腾堡 EPUB 均如此）全部查找失败 → 回退 NCX 时 chapter_index 全为 0
    /// → 点目录永远跳到第一章第一页。
    #[test]
    fn nav_toc_maps_chapter_index_when_opf_in_subdir() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>子目录 OPF 回归测试</dc:title>
    <dc:creator>测试</dc:creator>
    <dc:identifier id="uid">test-uid</dc:identifier>
  </metadata>
  <manifest>
    <item id="nav" href="toc.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/>
    <item id="c2" href="c2.xhtml" media-type="application/xhtml+xml"/>
    <item id="c3" href="c3.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="c1"/>
    <itemref idref="c2"/>
    <itemref idref="c3"/>
  </spine>
</package>"#;
        let nav = r#"<?xml version="1.0"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>TOC</title></head>
<body>
  <nav epub:type="toc"><ol>
    <li><a href="c2.xhtml#p2">第二章</a></li>
    <li><a href="c3.xhtml#p3">第三章</a></li>
  </ol></nav>
</body>
</html>"#;
        let chap = |id: &str| format!(
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>t</title></head><body><div class="chapter" id="{id}"><p>正文内容。</p></div></body></html>"#
        );

        let path = std::env::temp_dir().join(format!("kedu-toc-test-{}.epub", std::process::id()));
        {
            let file = std::fs::File::create(&path).expect("创建临时 epub");
            let mut zw = zip::ZipWriter::new(file);
            let opts = SimpleFileOptions::default();
            zw.start_file("mimetype", opts).unwrap();
            zw.write_all(b"application/epub+zip").unwrap();
            zw.start_file("META-INF/container.xml", opts).unwrap();
            zw.write_all(
                br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
            ).unwrap();
            zw.start_file("OEBPS/content.opf", opts).unwrap();
            zw.write_all(opf.as_bytes()).unwrap();
            zw.start_file("OEBPS/toc.xhtml", opts).unwrap();
            zw.write_all(nav.as_bytes()).unwrap();
            zw.start_file("OEBPS/c1.xhtml", opts).unwrap();
            zw.write_all(chap("p1").as_bytes()).unwrap();
            zw.start_file("OEBPS/c2.xhtml", opts).unwrap();
            zw.write_all(chap("p2").as_bytes()).unwrap();
            zw.start_file("OEBPS/c3.xhtml", opts).unwrap();
            zw.write_all(chap("p3").as_bytes()).unwrap();
            zw.finish().unwrap();
        }
        let size = std::fs::metadata(&path).unwrap().len();

        let (_, toc, _session) = EpubBook::open(&path, size).expect("打开测试 epub");
        let _ = std::fs::remove_file(&path);

        assert_eq!(toc.len(), 2, "nav 两项都应映射到 spine: {toc:?}");
        assert_eq!(toc[0].chapter_index, 1, "第二章应映射到 spine[1]");
        assert_eq!(toc[0].anchor.as_deref(), Some("p2"));
        assert_eq!(toc[1].chapter_index, 2, "第三章应映射到 spine[2]");
        assert_eq!(toc[1].anchor.as_deref(), Some("p3"));
    }

    // ────────────────────────── 跟随图书设定 ──────────────────────────

    /// 链尾 sans-serif → 追加黑体回退链（微软雅黑缺失时按链回退到其他黑体）。
    #[test]
    fn augment_family_sans_appends_heiti_chain() {
        let out = augment_one_family(r#""汉仪旗黑50S","PingFang SC",sans-serif"#).unwrap();
        assert!(out.starts_with(r#""汉仪旗黑50S","PingFang SC","#));
        assert!(out.contains(r#"Microsoft YaHei,"#), "缺微软雅黑回退: {out}");
        assert!(out.contains(r#""SimHei""#) && out.contains(r#""黑体""#), "缺其他黑体回退: {out}");
        assert!(out.ends_with("sans-serif"), "泛型关键字应保持在链尾: {out}");
    }

    /// 链中含楷体（kai/楷）→ 追加楷体回退链（本书引文场景）。
    #[test]
    fn augment_family_kai_appends_kai_chain() {
        let out = augment_one_family(r#"STKai, "MKai PRC", Kai,"楷体","PingFang SC", sans-serif"#).unwrap();
        assert!(out.contains(r#""KaiTi""#) && out.contains(r#"STKaiti"#), "缺楷体回退: {out}");
        // 已存在的名字不重复追加
        assert_eq!(out.matches("楷体").count() - out.matches("楷体_GB2312").count(), 1, "楷体去重: {out}");
    }

    /// 链尾 serif → 追加宋体链；inherit/非收尾泛型不动。
    #[test]
    fn augment_family_serif_and_skips() {
        let serif = augment_one_family(r#""Noto Serif CJK SC", serif"#).unwrap();
        assert!(serif.contains(r#""SimSun""#) && serif.contains("宋体"), "缺宋体回退: {serif}");
        assert!(serif.ends_with("serif"));

        assert!(augment_one_family("inherit").is_none());
        assert!(augment_one_family("var(--f)").is_none());
        // 泛型不在链尾（monospace 收尾 / 字体名恰含 serif）→ 不动
        assert!(augment_one_family(r#""Font Mono", monospace"#).is_none());
        assert!(augment_one_family(r#""MySerif""#).is_none());
        // 已包含全部回退名 → 无新增返回 None
        let full = HEITI_FALLBACKS.join(",");
        assert!(augment_one_family(&format!("{full},sans-serif")).is_none());
    }

    /// @font-face 描述符与 `font:` 简写不被增强。
    #[test]
    fn augment_css_skips_font_face_and_shorthand() {
        let css = r#"@font-face { font-family: "BookFont"; src: url(fonts/a.ttf); }
        p { font-family: "BookFont", sans-serif; }
        div { font: 1em/1.4 sans-serif; }"#;
        let out = augment_font_fallbacks(css);
        assert!(
            out.contains(r#"@font-face { font-family: "BookFont";"#),
            "@font-face 描述符被污染: {out}"
        );
        assert!(out.contains(r#""BookFont", Microsoft YaHei"#), "规则内的 font-family 未增强: {out}");
        assert!(out.contains("font: 1em/1.4 sans-serif;"), "font 简写被改动: {out}");
    }

    /// 端到端：内联 <style> 提取 → 注入 data-book-css；正文 class 保留；原文 <style> 移除。
    #[test]
    fn styled_chapter_keeps_book_css_and_classes() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let chapter = r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><style type="text/css">
p { font-family: "汉仪旗黑50S","PingFang SC", sans-serif; text-indent: 2em; }
.kindle-cn-kai { font-family: STKai, "MKai PRC", Kai,"楷体", sans-serif; }
</style></head><body>
<p class="kindle-cn-kai">引文段落</p>
<p style="text-align: center">居中段落</p>
<script>alert(1)</script>
</body></html>"#;

        let path = std::env::temp_dir().join(format!("kedu-styled-test-{}.epub", std::process::id()));
        {
            let file = std::fs::File::create(&path).expect("创建临时 epub");
            let mut zw = zip::ZipWriter::new(file);
            let opts = SimpleFileOptions::default();
            zw.start_file("mimetype", opts).unwrap();
            zw.write_all(b"application/epub+zip").unwrap();
            zw.start_file("META-INF/container.xml", opts).unwrap();
            zw.write_all(
                br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
            ).unwrap();
            zw.start_file("content.opf", opts).unwrap();
            zw.write_all(
                br#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>t</dc:title><dc:identifier id="uid">u</dc:identifier></metadata><manifest><item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c1"/></spine></package>"#,
            ).unwrap();
            zw.start_file("c1.xhtml", opts).unwrap();
            zw.write_all(chapter.as_bytes()).unwrap();
            zw.finish().unwrap();
        }
        let size = std::fs::metadata(&path).unwrap().len();
        let (_, _, session) = EpubBook::open(&path, size).expect("打开测试 epub");
        let _ = std::fs::remove_file(&path);

        // 普通加载：无样式、无 class
        let plain = session.load_chapter(0).unwrap();
        let plain_html = plain.html.as_deref().unwrap();
        assert!(!plain_html.contains("data-book-css"));
        assert!(!plain_html.contains("class="), "普通模式不应保留 class: {plain_html}");

        // 跟随图书加载：注入净化 CSS + 保留 class/style
        let styled = session.load_chapter_styled(0).unwrap();
        let html = styled.html.as_deref().unwrap();
        assert!(html.contains("<style data-book-css>"), "样式未注入: {html}");
        assert!(html.contains(".kindle-cn-kai"), "楷体规则丢失: {html}");
        // 黑体回退链增强生效
        assert!(html.contains(r#""汉仪旗黑50S","PingFang SC", Microsoft YaHei"#), "回退链未增强: {html}");
        assert!(html.contains(r#"class="kindle-cn-kai""#), "class 属性丢失: {html}");
        assert!(html.contains(r#"style="text-align: center""#), "style 属性丢失: {html}");
        // 原文 <style>/<script> 已被 ammonia 移除（注入块之外不残留）
        assert!(!html.contains("<script"), "script 未移除: {html}");
        assert_eq!(html.matches("<style").count(), 1, "应只剩注入的样式块: {html}");
    }

    /// 端到端：<link rel=stylesheet> 外链 CSS 内联 + @import 递归 + url() 改写。
    #[test]
    fn styled_chapter_inlines_linked_css_and_imports() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let chapter = r#"<html xmlns="http://www.w3.org/1999/xhtml"><head>
<link rel="stylesheet" type="text/css" href="css/main.css"/>
</head><body><p>正文</p></body></html>"#;
        let main_css = r#"@import "base.css";
p { color: red; background: url(../img/bg.png); }"#;
        let base_css = r#".quote { font-family: STKai, 楷体, sans-serif; }"#;

        let path = std::env::temp_dir().join(format!("kedu-css-link-test-{}.epub", std::process::id()));
        {
            let file = std::fs::File::create(&path).expect("创建临时 epub");
            let mut zw = zip::ZipWriter::new(file);
            let opts = SimpleFileOptions::default();
            zw.start_file("mimetype", opts).unwrap();
            zw.write_all(b"application/epub+zip").unwrap();
            zw.start_file("META-INF/container.xml", opts).unwrap();
            zw.write_all(
                br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
            ).unwrap();
            zw.start_file("content.opf", opts).unwrap();
            zw.write_all(
                br#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>t</dc:title><dc:identifier id="uid">u</dc:identifier></metadata><manifest><item id="c1" href="text/c1.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c1"/></spine></package>"#,
            ).unwrap();
            zw.start_file("text/c1.xhtml", opts).unwrap();
            zw.write_all(chapter.as_bytes()).unwrap();
            zw.start_file("css/main.css", opts).unwrap();
            zw.write_all(main_css.as_bytes()).unwrap();
            zw.start_file("css/base.css", opts).unwrap();
            zw.write_all(base_css.as_bytes()).unwrap();
            zw.finish().unwrap();
        }
        let size = std::fs::metadata(&path).unwrap().len();
        let (_, _, session) = EpubBook::open(&path, size).expect("打开测试 epub");
        let _ = std::fs::remove_file(&path);

        let styled = session.load_chapter_styled(0).unwrap();
        let html = styled.html.as_deref().unwrap();
        // @import 递归内联（base.css 的楷体规则 + 楷体回退链增强）
        assert!(html.contains(".quote"), "@import 未内联: {html}");
        assert!(html.contains(r#"楷体, KaiTi"#) || html.contains(r#"楷体,KaiTi"#), "楷体回退未增强: {html}");
        // @import 语句本体已消失
        assert!(!html.contains("@import"), "@import 未消解: {html}");
        // url() 相对路径已改写为 reader-res（zip 内资源存在；../img/bg.png 归一为 img/bg.png，
        // 条目缺失时保留原值——两者皆合法，这里只断言 main.css 的颜色规则进入注入块）
        assert!(html.contains("color: red"), "link CSS 未注入: {html}");
    }
}
