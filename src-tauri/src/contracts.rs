//! 双端 JSON 契约样例（E12）：fixtures/contract/*.json
//! 与 src/types.ts 字段命名对齐；缺省/null 语义与 storage 兼容策略一致。

#[cfg(test)]
mod tests {
    use crate::model::{AnnotationsFile, ReaderSettings, ShelfData};
    use crate::storage::BookProgressFile;
    use crate::error::AppError;
    use serde_json::json;

    fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("fixtures")
            .join("contract")
            .join(name);
        std::fs::read_to_string(path).expect("契约样例可读")
    }

    #[test]
    fn reader_settings_full_round_trip() {
        let raw = fixture("reader-settings.full.json");
        let s: ReaderSettings = serde_json::from_str(&raw).expect("完整设置可反序列化");
        assert_eq!(s.font_size_px, 22);
        assert_eq!(s.theme, crate::model::Theme::Sepia);
        assert_eq!(s.reading_mode, crate::model::ReadingMode::Scroll);
        assert_eq!(
            s.page_turn_effect,
            crate::model::PageTurnEffect::Natural
        );
        assert_eq!(s.scroll_effect, crate::model::PageTurnEffect::Natural);
        assert_eq!(s.shelf_sort_mode, crate::model::ShelfSortMode::Title);
        assert_eq!(s.default_category.as_deref(), Some("cat-fiction"));
        assert!(s.minimize_to_tray);
        let out = serde_json::to_value(&s).unwrap();
        assert_eq!(out["fontSizePx"], 22);
        assert_eq!(out["pageTurnEffect"], "natural");
        assert!(out.get("fontFamily").is_some());
    }

    #[test]
    fn reader_settings_partial_legacy_fills_defaults() {
        let raw = fixture("reader-settings.partial-legacy.json");
        let s: ReaderSettings = serde_json::from_str(&raw).expect("缺省字段走 Default");
        assert_eq!(s.font_size_px, 22);
        let d = ReaderSettings::default();
        assert_eq!(s.line_height, d.line_height);
        assert_eq!(s.page_turn_effect, d.page_turn_effect);
        assert_eq!(s.scroll_effect, d.scroll_effect);
        assert_eq!(s.column_width_px, d.column_width_px);
        assert_eq!(s.font_family, None);
        assert_eq!(s.shelf_sort_mode, d.shelf_sort_mode);
    }

    #[test]
    fn book_progress_contract() {
        let raw = fixture("book-progress.json");
        let p: BookProgressFile = serde_json::from_str(&raw).expect("进度快照可反序列化");
        assert_eq!(p.progress.book_id, "bk0000000000000001");
        assert_eq!(p.progress.chapter_index, 3);
        assert_eq!(p.progress.anchor.as_deref(), Some("para-12"));
        assert!((p.progress.scroll_ratio - 0.42).abs() < 1e-6);
        assert_eq!(p.progress.page_in_chapter, Some(2));
        assert!((p.progress.percent - 37.5).abs() < 1e-6);
        assert_eq!(p.settings.font_size_px, 18);
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["progress"]["chapterIndex"], 3);
        assert_eq!(v["settings"]["readingMode"], "paginated");
    }

    #[test]
    fn shelf_contract_and_legacy_genre_omitted() {
        let raw = fixture("shelf.json");
        let shelf: ShelfData = serde_json::from_str(&raw).expect("书架可反序列化");
        assert_eq!(shelf.categories.len(), 2);
        assert_eq!(shelf.books.len(), 2);
        assert_eq!(shelf.uncategorized_order, 0);
        let b0 = &shelf.books[0];
        assert_eq!(b0.font_family.as_deref(), Some("@follow-book"));
        assert_eq!(b0.rating, Some(87));
        assert_eq!(b0.genres, vec!["小说".to_string(), "科幻".to_string()]);
        assert!(!b0.is_stats_excluded());
        let v = serde_json::to_value(b0).unwrap();
        assert!(v.get("legacyGenre").is_none(), "legacyGenre 不再序列化");
        assert_eq!(v["fileSize"], 102400);
        assert_eq!(v["finishedTimes"], 0);
    }

    #[test]
    fn annotations_contract_legacy_bookmark_defaults() {
        let raw = fixture("annotations.json");
        let a: AnnotationsFile = serde_json::from_str(&raw).expect("批注可反序列化");
        assert_eq!(a.notes.len(), 2);
        assert_eq!(a.bookmarks.len(), 2);
        assert_eq!(a.notes[0].note.as_deref(), Some("读者批注"));
        assert_eq!(a.notes[1].note, None);
        assert_eq!(
            a.bookmarks[0].quote.as_deref(),
            Some("锚点附近引文")
        );
        // 旧书签缺 quote/prefix/suffix → default None
        assert_eq!(a.bookmarks[1].quote, None);
        assert_eq!(a.bookmarks[1].prefix, None);
        assert_eq!(a.bookmarks[1].suffix, None);
        let v = serde_json::to_value(&a.bookmarks[0]).unwrap();
        assert_eq!(v["pageInChapter"], 0);
        assert_eq!(v["chapterIndex"], 1);
    }

    #[test]
    fn ipc_error_serializes_as_string() {
        // 前端 catch(err) 收到的是中文可读字符串，不是 {code,message} 对象
        let e = AppError::DrmDetected;
        let v = serde_json::to_value(&e).unwrap();
        assert!(v.is_string());
        assert!(v.as_str().unwrap().contains("DRM"));
        let e2 = AppError::ChapterOutOfRange(9);
        let s = serde_json::to_value(&e2).unwrap();
        assert_eq!(s.as_str().unwrap(), "章节索引超出范围: 9");
        // 与 types.ts 中 ipc 调用侧「字符串错误」约定一致
        let sample = json!("该电子书包含 DRM 加密保护，无法在本地阅读器中打开");
        assert_eq!(sample.as_str(), Some(serde_json::to_value(&AppError::DrmDetected).unwrap().as_str().unwrap()));
    }
}
