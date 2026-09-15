//! 书籍内存管理模块。
//!
//! 内存模型（对应需求 2）：
//! - `AppState` 持有 `HashMap<BookId, Arc<LoadedBook>>`，打开的书籍驻留内存；
//! - `close_book` 命令从 map 中移除条目，当没有任何 Arc 克隆存活时，
//!   `LoadedBook`（含章节缓存、ZIP 原始字节、解码后的全文）随之释放，防止泄漏；
//! - 命令层只在调用瞬间 clone `Arc<LoadedBook>`，调用结束即归还，
//!   因此 close 后不会有隐藏的引用拖住内存。
//!
//! 章节内容按需加载并做 FIFO 缓存（默认 24 章），大书不会一次性全部展开。
//! 缓存内部为 Arc 共享只读内容；IPC 边界再克隆为独立值。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::error::{AppError, AppResult};
use crate::model::{BookMeta, ChapterContent, TocItem};
use crate::parser::BookSession;

/// 章节缓存容量：超过后淘汰最早的条目。
const CHAPTER_CACHE_CAP: usize = 24;

/// 同时驻留内存的书籍上限（Q2）：超出后按 LRU 驱逐整本
/// （LoadedBook 含 ZIP 原始字节 / 解码全文 / 章节缓存，是内存大头）。
const MAX_OPEN_BOOKS: usize = 4;

/// 章节缓存 key：(章节序号, 是否带书籍样式)。
/// 「跟随图书设定」与普通渲染产出不同 HTML，必须分开缓存避免互相污染。
type ChapterKey = (usize, bool);

/// 章节缓存：内部共享只读内容（E08），命中只克隆 Arc，不克隆 HTML/文本。
/// 容量满时淘汰最早进入的条目（实际策略为 FIFO，非 LRU）。
#[derive(Default)]
struct ChapterCache {
    map: HashMap<ChapterKey, Arc<ChapterContent>>,
    order: VecDeque<ChapterKey>,
}

impl ChapterCache {
    fn get(&self, key: ChapterKey) -> Option<Arc<ChapterContent>> {
        self.map.get(&key).cloned()
    }

    fn put(&mut self, key: ChapterKey, content: Arc<ChapterContent>) {
        if self.map.contains_key(&key) {
            self.map.insert(key, content);
            return;
        }
        // 容量满则淘汰最早进入的章节（FIFO；命中不提升顺序）
        while self.map.len() >= CHAPTER_CACHE_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            } else {
                break;
            }
        }
        self.map.insert(key, content);
        self.order.push_back(key);
    }
}

/// 一本已打开的书：元数据 + 目录 + 格式专属解析会话 + 章节缓存。
pub struct LoadedBook {
    pub meta: BookMeta,
    pub toc: Vec<TocItem>,
    session: Box<dyn BookSession>,
    cache: Mutex<ChapterCache>,
}

impl LoadedBook {
    pub fn new(meta: BookMeta, toc: Vec<TocItem>, session: Box<dyn BookSession>) -> Self {
        Self {
            meta,
            toc,
            session,
            cache: Mutex::new(ChapterCache::default()),
        }
    }

    /// 取章节（IPC 边界）：返回可序列化的独立副本。
    pub fn chapter(&self, index: usize) -> AppResult<ChapterContent> {
        Ok(self.chapter_inner((index, false))?.as_ref().clone())
    }

    /// 「跟随图书设定」取章节：独立缓存维度，产出保留书籍 class/style 与 CSS。
    pub fn chapter_styled(&self, index: usize) -> AppResult<ChapterContent> {
        Ok(self.chapter_inner((index, true))?.as_ref().clone())
    }

    /// 搜索等内部消费者：共享 Arc，不深拷贝 HTML/文本（E08）。
    pub fn chapter_shared(&self, index: usize) -> AppResult<Arc<ChapterContent>> {
        self.chapter_inner((index, false))
    }

    /// 搜索专用（E07）：优先读已有阅读缓存；未命中时解析但不写入阅读缓存，
    /// 避免全文遍历把当前阅读章节挤出 24 项缓存。
    pub fn chapter_for_search(&self, index: usize) -> AppResult<Arc<ChapterContent>> {
        let key = (index, false);
        {
            let cache = self
                .cache
                .lock()
                .map_err(|_| AppError::other("章节缓存锁中毒"))?;
            if let Some(hit) = cache.get(key) {
                return Ok(hit);
            }
        }
        let content = self.session.load_chapter(index)?;
        Ok(Arc::new(content))
    }

    fn chapter_inner(&self, key: ChapterKey) -> AppResult<Arc<ChapterContent>> {
        {
            let cache = self
                .cache
                .lock()
                .map_err(|_| AppError::other("章节缓存锁中毒"))?;
            if let Some(hit) = cache.get(key) {
                return Ok(hit);
            }
        }
        let content = if key.1 {
            self.session.load_chapter_styled(key.0)?
        } else {
            self.session.load_chapter(key.0)?
        };
        let content = Arc::new(content);
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| AppError::other("章节缓存锁中毒"))?;
        cache.put(key, content.clone());
        Ok(content)
    }

    /// 取书籍内部资源（封面、CBZ 图片等），返回 MIME + 原始字节。
    pub fn resource_bytes(&self, path: &str) -> AppResult<(String, Vec<u8>)> {
        self.session.load_resource_bytes(path)
    }

    /// 取书籍内部资源（封面、CBZ 图片等），返回 base64 载荷（IPC 用）。
    pub fn resource(&self, path: &str) -> AppResult<ResourcePayload> {
        self.session.load_resource(path)
    }

    /// 总章节数（越界校验用）。
    pub fn chapter_count(&self) -> usize {
        self.session.chapter_count()
    }
}

/// 资源载荷：MIME + base64 文本（经 IPC 传给前端）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcePayload {
    pub mime_type: String,
    pub base64: String,
}

/// 全局应用状态：Tauri `.manage()` 注入。
///
/// 内存释放链路（Q2）：
/// - `insert` 超过 `MAX_OPEN_BOOKS` 时 LRU 驱逐整本；
/// - `get` 命中即触活（移到队尾），命令层用完即还 Arc，不长期持有；
/// - `remove` 显式关闭；窗口销毁时 `clear()` 清空全部会话；
/// - Arc 归零后 LoadedBook（ZIP 字节/全文/缓存）立即由 RAII 释放。
#[derive(Default)]
pub struct AppState {
    books: Arc<Mutex<HashMap<String, Arc<LoadedBook>>>>,
    /// LRU 访问序：队尾 = 最近使用。
    order: Mutex<VecDeque<String>>,
}

impl AppState {
    /// 注册/替换一本已打开的书（重复打开同一本会顶掉旧会话）。
    /// 超出上限时驱逐最久未使用的一本。
    pub fn insert(&self, book: LoadedBook) -> Arc<LoadedBook> {
        let arc = Arc::new(book);
        let id = arc.meta.id.clone();
        if let Ok(mut books) = self.books.lock() {
            books.insert(id.clone(), arc.clone());
            if let Ok(mut order) = self.order.lock() {
                order.retain(|b| b != &id);
                order.push_back(id);
                // LRU 驱逐：最久未使用的整本移出（在飞 Arc 克隆归还后内存即释放）
                while books.len() > MAX_OPEN_BOOKS {
                    let Some(victim) = order.pop_front() else { break };
                    books.remove(&victim);
                }
            }
        }
        arc
    }

    pub fn get(&self, book_id: &str) -> AppResult<Arc<LoadedBook>> {
        {
            // 触活：命中即移到 LRU 队尾
            if let Ok(mut order) = self.order.lock() {
                if order.iter().any(|b| b == book_id) {
                    order.retain(|b| b != book_id);
                    order.push_back(book_id.to_string());
                }
            }
        }
        self.books
            .lock()
            .map_err(|_| AppError::other("书籍表锁中毒"))?
            .get(book_id)
            .cloned()
            .ok_or_else(|| AppError::BookNotOpen(book_id.to_string()))
    }

    /// 关闭书籍：移除条目。若无外部 Arc 克隆，内存立即释放。
    pub fn remove(&self, book_id: &str) -> bool {
        let removed = self
            .books
            .lock()
            .map(|mut books| books.remove(book_id).is_some())
            .unwrap_or(false);
        if removed {
            if let Ok(mut order) = self.order.lock() {
                order.retain(|b| b != book_id);
            }
        }
        removed
    }

    /// 清空全部书籍会话（窗口销毁 / 应用退出前调用）。
    pub fn clear(&self) {
        if let Ok(mut books) = self.books.lock() {
            books.clear();
        }
        if let Ok(mut order) = self.order.lock() {
            order.clear();
        }
    }

    pub fn len(&self) -> usize {
        self.books.lock().map(|b| b.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
