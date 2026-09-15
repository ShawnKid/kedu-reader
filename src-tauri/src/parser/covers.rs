//! 内置封面候选：默认封面在前，较大的竖版图片优先；去除相同字节和小图。
use crate::error::{AppError, AppResult};
use crate::model::BookFormat;
use std::path::Path;

pub const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Candidates(Vec<((bool, bool, u64), Vec<u8>)>);

impl Candidates {
    pub fn push(&mut self, bytes: Vec<u8>, primary: bool) {
        if bytes.len() as u64 > MAX_IMAGE_BYTES || self.0.iter().any(|(_, b)| *b == bytes) { return; }
        let Some((w, h)) = dimensions(&bytes) else { return; };
        if w == 0 || h == 0 || (!primary && (w < 160 || h < 240)) { return; }
        let portrait = w as u64 * 10 >= h as u64 * 4 && w as u64 * 10 <= h as u64 * 8;
        self.0.push(((primary, portrait, w as u64 * h as u64), bytes));
        self.0.sort_by(|a, b| b.0.cmp(&a.0));
        while self.0.len() > 32 || self.0.iter().map(|(_, b)| b.len()).sum::<usize>() > 64 * 1024 * 1024 {
            self.0.pop();
        }
    }
    pub fn finish(self) -> Vec<Vec<u8>> { self.0.into_iter().map(|(_, b)| b).collect() }
}

fn dimensions(b: &[u8]) -> Option<(u32, u32)> {
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        return match b.get(12..16)? {
            b"VP8X" => {
                let w = b.get(24..27)?; let h = b.get(27..30)?;
                Some((1 + w[0] as u32 + ((w[1] as u32) << 8) + ((w[2] as u32) << 16),
                    1 + h[0] as u32 + ((h[1] as u32) << 8) + ((h[2] as u32) << 16)))
            }
            b"VP8L" if b.get(20) == Some(&0x2f) => {
                let bits = u32::from_le_bytes(b.get(21..25)?.try_into().ok()?);
                Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
            }
            b"VP8 " if b.get(23..26) == Some(&[0x9d, 0x01, 0x2a]) => {
                Some(((u16::from_le_bytes(b.get(26..28)?.try_into().ok()?) & 0x3fff) as u32,
                    (u16::from_le_bytes(b.get(28..30)?.try_into().ok()?) & 0x3fff) as u32))
            }
            _ => None,
        };
    }
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some((u32::from_be_bytes(b.get(16..20)?.try_into().ok()?), u32::from_be_bytes(b.get(20..24)?.try_into().ok()?)));
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        return Some((u16::from_le_bytes(b.get(6..8)?.try_into().ok()?) as u32, u16::from_le_bytes(b.get(8..10)?.try_into().ok()?) as u32));
    }
    if b.starts_with(&[0xff, 0xd8]) {
        let mut p = 2;
        while p + 4 <= b.len() {
            if b[p] != 0xff { return None; }
            while b.get(p) == Some(&0xff) { p += 1; }
            let marker = *b.get(p)?; p += 1;
            if marker == 0xda || marker == 0xd9 { break; }
            if marker == 0x01 || (0xd0..=0xd7).contains(&marker) { continue; }
            let len = u16::from_be_bytes(b.get(p..p + 2)?.try_into().ok()?) as usize;
            if len < 2 || p + len > b.len() { return None; }
            if (0xc0..=0xcf).contains(&marker) && ![0xc4, 0xc8, 0xcc].contains(&marker) && len >= 7 {
                return Some((u16::from_be_bytes(b[p + 5..p + 7].try_into().ok()?) as u32,
                    u16::from_be_bytes(b[p + 3..p + 5].try_into().ok()?) as u32));
            }
            p += len;
        }
    }
    None
}

pub fn load(path: &Path) -> AppResult<Vec<Vec<u8>>> {
    match super::detect_format(path)? {
        BookFormat::Epub => super::epub::cover_candidates(path),
        BookFormat::Mobi | BookFormat::Azw3 => super::mobi::cover_candidates(path),
        _ => {
            let (meta, _, session) = super::open_book_from_path(&path.to_string_lossy())?;
            let Some(resource) = meta.cover_resource else { return Ok(vec![]); };
            Ok(vec![session.load_resource_bytes(&resource)?.1])
        }
    }
}

/// 从当前封面出发切换候选：forward 向后（下一张）/ 向前（上一张），环形回绕。
/// 当前封面不在候选中（如自选图片）时，向后取第一张、向前取最后一张。
pub fn step_index(candidates: &[Vec<u8>], current: Option<&[u8]>, forward: bool) -> AppResult<usize> {
    if candidates.is_empty() { return Err(AppError::other("本书没有可切换的内置封面图片")); }
    let found = current.and_then(|current| candidates.iter().position(|b| b == current));
    Ok(match (found, forward) {
        (Some(i), true) => (i + 1) % candidates.len(),
        (Some(i), false) => (i + candidates.len() - 1) % candidates.len(),
        (None, true) => 0,
        (None, false) => candidates.len() - 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn png(w: u32, h: u32, salt: u8) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend(w.to_be_bytes()); b.extend(h.to_be_bytes()); b.push(salt); b
    }
    #[test]
    fn candidates_deduplicate_filter_and_prefer_portrait() {
        let promo = png(300, 400, 0); let flower = png(600, 913, 1);
        let portrait = png(404, 492, 2);
        let mut c = Candidates::default();
        c.push(promo.clone(), true); c.push(promo.clone(), false);
        c.push(portrait.clone(), false); c.push(flower.clone(), false);
        c.push(png(30, 30, 0), false);
        assert_eq!(c.finish(), vec![promo, flower, portrait]);
    }
    #[test]
    fn stepping_uses_saved_image_and_wraps() {
        let candidates = vec![vec![1], vec![2], vec![3]];
        assert_eq!(step_index(&candidates, Some(&[1]), true).unwrap(), 1);
        assert_eq!(step_index(&candidates, Some(&[2]), true).unwrap(), 2);
        assert_eq!(step_index(&candidates, Some(&[3]), true).unwrap(), 0);
        assert_eq!(step_index(&candidates, Some(&[9]), true).unwrap(), 0);
        assert_eq!(step_index(&candidates, Some(&[9]), false).unwrap(), 2);
        assert_eq!(step_index(&candidates, Some(&[3]), false).unwrap(), 1);
        assert_eq!(step_index(&candidates, Some(&[1]), false).unwrap(), 2);
        assert_eq!(step_index(&[vec![1]], Some(&[1]), true).unwrap(), 0);
        assert_eq!(step_index(&[vec![1]], Some(&[1]), false).unwrap(), 0);
        assert!(step_index(&[], None, true).is_err());
    }

    #[test]
    #[ignore = "requires KEDU_EPUB_FIXTURE and KEDU_COVER_EXPECTED"]
    fn real_epub_advances_from_promotion_to_verified_cover() {
        let path = std::env::var("KEDU_EPUB_FIXTURE").unwrap();
        let expected = std::fs::read(std::env::var("KEDU_COVER_EXPECTED").unwrap()).unwrap();
        let candidates = load(Path::new(&path)).unwrap();
        assert!(candidates.len() >= 2);
        let next = step_index(&candidates, Some(&candidates[0]), true).unwrap();
        assert_eq!(candidates[next], expected);
        assert_eq!(dimensions(&expected), Some((600, 913)));
    }
}
