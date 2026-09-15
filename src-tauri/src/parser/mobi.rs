//! MOBI / PRC / AZW3(KF8) 解析模块（需求 1 全格式兼容）。
//!
//! 解析行为说明：
//! - PDB 记录表 + record0（PalmDOC 头 + MOBI 头 + EXTH）；
//! - 解压：PalmDOC LZ77（类型 2）、HUFF/CDIC（类型 17480）、无压缩（1）；
//! - 尾随字节（trailing entries + multibyte）剥离；
//! - 混合封装（hybrid）：EXTH 121 boundary → 打开 KF8 部分；
//! - 章节切分：MOBI6 按 `<mbp:pagebreak>` + filepos 锚点注入；KF8 按
//!   SKEL/FRAG 索引拼装（与 KF8 规范章节划分对齐）；
//! - 目录：NCX INDX 索引（两类通用）→ 兜底 MOBI6 guide → 兜底页节标题；
//! - 资源：图片记录按魔数识别，`rec:<n>` 资源路径协议化（reader-res 直出）；
//! - DRM：PalmDOC 头 encryption 字段非零一律拒绝。
//!
//! 相对完整实现的简化说明：
//! - 文本记录在 open 时全量解压（书级内存 ≈ 解压后 HTML 大小，随 LRU 驱逐释放）；
//! - KF8 的 RESC/PAGE（页spread）与 kindle:flow 内联流未支持（不影响正文）。

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use regex::bytes::Regex as BytesRegex;
use regex::Regex;

use crate::error::{AppError, AppResult};
use crate::model::{BookFormat, BookMeta, ChapterContent, ChapterKind, TocItem};
use crate::parser::{make_book_id, title_from_path, BookSession};

// ────────────────────────── 固定正则缓存（E05） ──────────────────────────
// 模式字面量与原先函数内 Regex::new 完全一致，仅避免每次调用重复编译。

fn re_pagebreak_bytes() -> &'static BytesRegex {
    static RE: OnceLock<BytesRegex> = OnceLock::new();
    RE.get_or_init(|| BytesRegex::new(r"(?i)<\s*(?:mbp:)?pagebreak[^>]*>").expect("pagebreak regex"))
}

fn re_filepos_attr_bytes() -> &'static BytesRegex {
    static RE: OnceLock<BytesRegex> = OnceLock::new();
    RE.get_or_init(|| {
        BytesRegex::new(r#"(?i)<[^<>]+filepos=['"]{0,1}?(\d+)[^<>]*>"#).expect("filepos regex")
    })
}

fn re_frag_anchor_attr() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)\s(id|name|aid)\s*=\s*['"]([^'"]*)['"]"#).expect("frag anchor regex")
    })
}

fn re_reference_tag() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?is)<reference\b[^>]*>"#).expect("reference tag regex"))
}

fn re_type_attr() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)type\s*=\s*["']([^"']*)["']"#).expect("type attr regex"))
}

fn re_filepos_attr() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)filepos\s*=\s*["']?(\d+)"#).expect("filepos attr regex"))
}

fn re_guide_filepos_anchor() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?is)<a\b[^>]*filepos\s*=\s*["']?(\d+)[^>]*>(.*?)</a>"#)
            .expect("guide filepos anchor regex")
    })
}

fn re_strip_tags() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<[^>]*>").expect("strip tags regex"))
}

fn re_heading_fallback() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?is)<h[1-6][^>]*>([\s\S]{0,120}?)</h[1-6]>"#)
            .expect("heading fallback regex")
    })
}

fn re_pagebreak_tag() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)<\s*/?\s*(?:mbp:)?pagebreak[^>]*>").expect("pb tag regex")
    })
}

fn re_kindle_embed() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"kindle:embed:([0-9A-Fa-f]+)(?:\?mime=[^"'\s>]+)?"#).expect("embed regex")
    })
}

fn re_recindex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)\brecindex\s*=\s*["']?(\d+)["']?"#).expect("recindex regex")
    })
}

// ────────────────────────── record0 头字段偏移 ──────────────────────────
// （与 mobi.js 的 PDB/PALMDOC/MOBI/KF8/EXTH_HEADER 定义一致）

fn u16_at(data: &[u8], off: usize) -> u32 {
    if off + 2 > data.len() {
        return 0;
    }
    u16::from_be_bytes([data[off], data[off + 1]]) as u32
}

fn u32_at(data: &[u8], off: usize) -> u32 {
    if off + 4 > data.len() {
        return 0;
    }
    u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

/// MOBI 头内字段（相对 record0 起始的绝对偏移）。
/// mobi.js 的字段表本就按 record0 起始计（'MOBI' 魔数在 16），
/// PalmDOC 头 16 字节已包含在内，不再额外 +16。
const OFF_TEXT_ENCODING: usize = 28;
const OFF_FILE_VERSION: usize = 36;
const OFF_FULL_NAME_OFFSET: usize = 84;
const OFF_FULL_NAME_LENGTH: usize = 88;
const OFF_FIRST_IMAGE: usize = 108; // firstImageIndex（绝对记录号）
const OFF_HUFFCDIC: usize = 112;
const OFF_NUM_HUFFCDIC: usize = 116;
const OFF_EXTH_FLAG: usize = 128;
const OFF_TRAILING_FLAGS: usize = 240; // 版本 >= 5 才有意义
const OFF_INDX: usize = 244;
const OFF_KF8_FDST: usize = 196; // FDST 起始记录号（192 是 FDST 计数）
const OFF_KF8_FRAG: usize = 248;
const OFF_KF8_SKEL: usize = 252;
#[allow(dead_code)]
const OFF_KF8_GUIDE: usize = 260;

/// EXTH 记录类型 → 元数据键（只需用到的）。
const EXTH_AUTHOR: u32 = 100;
const EXTH_PUBLISHER: u32 = 101;
const EXTH_RIGHTS: u32 = 109;
const EXTH_BOUNDARY: u32 = 121;
const EXTH_COVER_OFFSET: u32 = 201;
const EXTH_THUMB_OFFSET: u32 = 202;
const EXTH_TITLE: u32 = 503;
const EXTH_LANGUAGE: u32 = 524;

/// 书架迁移只需要封面，不解压正文或构建 KF8 章节索引。
pub fn extract_cover(path: &Path) -> AppResult<Option<Vec<u8>>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut header = [0; 78];
    file.read_exact(&mut header)?;
    let count = u16_at(&header, 76) as usize;
    if count == 0 { return Ok(None); }
    let mut table = vec![0; count * 8];
    file.read_exact(&mut table)?;
    let mut record = |index: usize| -> AppResult<Vec<u8>> {
        if index >= count { return Err(AppError::other("MOBI 封面记录越界")); }
        let start = u32_at(&table, index * 8) as u64;
        let end = if index + 1 < count { u32_at(&table, (index + 1) * 8) as u64 } else { file_len };
        if start < 78 + table.len() as u64 || end <= start || end > file_len || end - start > 20 * 1024 * 1024 {
            return Err(AppError::other("MOBI 封面记录偏移或大小异常"));
        }
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = vec![0; (end - start) as usize];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    };
    let first = record(0)?;
    if first.get(16..20) != Some(b"MOBI") { return Ok(None); }
    let resource_start = u32_at(&first, OFF_FIRST_IMAGE) as usize;
    let metadata = |header: &[u8]| -> AppResult<HashMap<u32, ExthValue>> {
        if u16_at(header, 12) != 0 { return Err(AppError::DrmDetected); }
        let mut exth = HashMap::new();
        if u32_at(header, OFF_EXTH_FLAG) & 0x40 != 0 {
            parse_exth(header, u32_at(header, 20) as usize + 16,
                u32_at(header, OFF_TEXT_ENCODING) as usize, &mut exth);
        }
        Ok(exth)
    };
    let mut exth = metadata(&first)?;
    if u32_at(&first, OFF_FILE_VERSION) < 8 {
        if let Some(boundary) = exth.get(&EXTH_BOUNDARY).and_then(|v| v.as_uint()).filter(|&b| b > 0 && b < u32::MAX) {
            let second = record(boundary as usize)?;
            if second.get(16..20) == Some(b"MOBI") { exth = metadata(&second)?; }
        }
    }
    cover_offset(&exth).map(|offset| record(resource_start + offset as usize)).transpose()
}

fn cover_offset(exth: &HashMap<u32, ExthValue>) -> Option<u32> {
    [EXTH_COVER_OFFSET, EXTH_THUMB_OFFSET].into_iter()
        .find_map(|key| exth.get(&key).and_then(|v| v.as_uint()).filter(|&o| o < u32::MAX))
}

pub(super) fn cover_candidates(path: &Path) -> AppResult<Vec<Vec<u8>>> {
    use super::covers::Candidates;
    let mut candidates = Candidates::default();
    if let Some(bytes) = extract_cover(path)? { candidates.push(bytes, true); }
    if std::fs::metadata(path)?.len() > 512 * 1024 * 1024 {
        return Err(AppError::other("MOBI 文件过大，无法扫描候选封面"));
    }
    let pdb = Pdb::parse(std::fs::read(path)?)?;
    let first = pdb.record(0)?;
    let start = u32_at(first, OFF_FIRST_IMAGE) as usize;
    for i in start..pdb.offsets.len() {
        if let Ok(bytes) = pdb.record(i) {
            if bytes.len() as u64 <= super::covers::MAX_IMAGE_BYTES {
                candidates.push(bytes.to_vec(), false);
            }
        }
    }
    Ok(candidates.finish())
}

/// 单条记录字节切片（含起止偏移预计算）。
struct Pdb {
    data: Vec<u8>,
    /// 每条记录的起始偏移（长度 = numRecords）；结束 = 下一条起始或文件尾。
    offsets: Vec<u32>,
}

impl Pdb {
    fn parse(data: Vec<u8>) -> AppResult<Self> {
        if data.len() < 78 {
            return Err(AppError::other("MOBI 文件过小，不是有效的 PalmDB"));
        }
        let num_records = u16_at(&data, 76) as usize;
        if num_records == 0 || 78 + num_records * 8 > data.len() {
            return Err(AppError::other("PalmDB 记录表损坏"));
        }
        let mut offsets = Vec::with_capacity(num_records);
        for i in 0..num_records {
            offsets.push(u32_at(&data, 78 + i * 8));
        }
        Ok(Pdb { data, offsets })
    }

    fn record(&self, index: usize) -> AppResult<&[u8]> {
        let start = *self
            .offsets
            .get(index)
            .ok_or(AppError::other(format!("MOBI 记录越界: {index}")))? as usize;
        let end = self
            .offsets
            .get(index + 1)
            .map(|&e| e as usize)
            .unwrap_or(self.data.len());
        if start >= end || end > self.data.len() {
            return Err(AppError::other(format!("MOBI 记录 {index} 偏移异常")));
        }
        Ok(&self.data[start..end])
    }

    #[allow(dead_code)]
    fn record_magic(&self, index: usize) -> [u8; 4] {
        let mut magic = [0u8; 4];
        if let Ok(rec) = self.record(index) {
            for (i, b) in rec.iter().take(4).enumerate() {
                magic[i] = *b;
            }
        }
        magic
    }
}

// ────────────────────────── 变长整数 ──────────────────────────

/// 从前往后读变长整数（每字节 7 位，最高位 = 继续标志）。
fn get_var_len(data: &[u8], i: usize) -> (u32, usize) {
    let mut value: u32 = 0;
    let mut length = 0usize;
    for &byte in data.iter().skip(i).take(4) {
        value = (value << 7) | (byte & 0x7f) as u32;
        length += 1;
        if byte & 0x80 != 0 {
            break;
        }
    }
    (value, length)
}

/// 从末尾读变长整数（尾随条目长度；最高位 = 起始标志）。
fn get_var_len_from_end(data: &[u8]) -> usize {
    let mut value: usize = 0;
    let tail = &data[data.len().saturating_sub(4)..];
    for &byte in tail {
        if byte & 0x80 != 0 {
            value = 0;
        }
        value = (value << 7) | (byte & 0x7f) as usize;
    }
    value
}

fn count_bits_set(mut x: u32) -> u32 {
    let mut count = 0;
    while x > 0 {
        if x & 1 == 1 {
            count += 1;
        }
        x >>= 1;
    }
    count
}

fn count_unset_end(mut x: u32) -> u32 {
    let mut count = 0;
    while x & 1 == 0 {
        x >>= 1;
        count += 1;
    }
    count
}

// ────────────────────────── 解压 ──────────────────────────

/// PalmDOC LZ77 解压（mobi.js decompressPalmDOC 的直译）。
fn decompress_palmdoc(array: &[u8]) -> Vec<u8> {
    let mut output: Vec<u8> = Vec::with_capacity(array.len() * 2);
    let mut i = 0usize;
    while i < array.len() {
        let byte = array[i];
        if byte == 0 {
            output.push(0);
            i += 1;
        } else if byte <= 8 {
            // 后续 1~8 字节为字面量
            let start = i + 1;
            let end = (start + byte as usize).min(array.len());
            output.extend_from_slice(&array[start..end]);
            i = end;
        } else if byte <= 0x7f {
            output.push(byte);
            i += 1;
        } else if byte <= 0xbf {
            // 长度-距离对
            if i + 1 >= array.len() {
                break;
            }
            let bytes = ((byte as u32) << 8) | array[i + 1] as u32;
            let distance = ((bytes & 0x3fff) >> 3) as usize;
            let length = ((bytes & 0b111) + 3) as usize;
            let base = output.len().checked_sub(distance);
            if let Some(base) = base {
                for j in 0..length {
                    let b = output[base + j];
                    output.push(b);
                }
            }
            i += 2;
        } else {
            // 空格 + 字符
            output.push(32);
            output.push(byte ^ 0x80);
            i += 1;
        }
    }
    output
}

/// 从任意位偏移读 32 位（HUFF 解码用，mobi.js read32Bits）。
fn read32bits(data: &[u8], from: usize) -> u32 {
    let start_byte = from >> 3;
    let end = from + 32;
    let shift = 8 - (from & 7); // (end & 7) == (from & 7)，32 是 8 的倍数
    let mut bits: u64 = 0;
    for i in start_byte..=(end >> 3) {
        bits = (bits << 8) | *data.get(i).unwrap_or(&0) as u64;
    }
    ((bits >> shift) & 0xffff_ffff) as u32
}

/// HUFF/CDIC 解压器。
struct HuffCdic {
    /// table1[byte] = (found, code_length_base, value_base)
    table1: Vec<(bool, u32, u32)>,
    /// table2[code_length] = (min_code, value_offset)；下标 0 弃用
    table2: Vec<(u32, u32)>,
    /// 字典条目：(数据, 是否已解压)
    dictionary: Vec<(Vec<u8>, bool)>,
}

impl HuffCdic {
    fn parse(pdb: &Pdb, huffcdic: usize, num_huffcdic: usize) -> AppResult<Self> {
        let huff_record = pdb.record(huffcdic)?;
        if &huff_record[..4.min(huff_record.len())] != b"HUFF" {
            return Err(AppError::other("无效的 HUFF 记录"));
        }
        let offset1 = u32_at(huff_record, 8) as usize;
        let offset2 = u32_at(huff_record, 12) as usize;

        let table1 = (0..256)
            .map(|i| {
                let x = u32_at(huff_record, offset1 + i * 4);
                (x & 0x80 != 0, x & 0x1f, x >> 8)
            })
            .collect();

        let mut table2 = vec![(0u32, 0u32)];
        for i in 0..32 {
            table2.push((
                u32_at(huff_record, offset2 + i * 8),
                u32_at(huff_record, offset2 + i * 8 + 4),
            ));
        }

        let mut dictionary: Vec<(Vec<u8>, bool)> = vec![];
        for i in 1..num_huffcdic {
            let record = pdb.record(huffcdic + i)?;
            if &record[..4.min(record.len())] != b"CDIC" {
                return Err(AppError::other("无效的 CDIC 记录"));
            }
            let header_len = u32_at(record, 4) as usize;
            let num_entries = u32_at(record, 8);
            let code_length = u32_at(record, 12);
            let n = std::cmp::min(1u32 << code_length, num_entries - dictionary.len() as u32) as usize;
            let buffer = &record[header_len.min(record.len())..];
            for j in 0..n {
                let offset = u16_at(buffer, j * 2) as usize;
                let x = u16_at(buffer, offset);
                let length = (x & 0x7fff) as usize;
                let decompressed = x & 0x8000 != 0;
                let value = buffer
                    .get(offset + 2..offset + 2 + length)
                    .unwrap_or(&[])
                    .to_vec();
                dictionary.push((value, decompressed));
            }
        }

        Ok(HuffCdic { table1, table2, dictionary })
    }

    /// 解压一段 HUFF 编码字节（mobi.js huffcdic decompress）。
    fn decompress(&self, array: &[u8]) -> Vec<u8> {
        let mut output: Vec<u8> = vec![];
        let bit_length = array.len() * 8;
        let mut i = 0usize;
        while i < bit_length {
            let bits = read32bits(array, i);
            let (found, mut code_length, mut value) = self.table1[(bits >> 24) as usize];
            if !found {
                while code_length < 32
                    && bits.wrapping_shr(32 - code_length) < self.table2[code_length as usize].0
                {
                    code_length += 1;
                }
                value = self.table2[code_length as usize].1;
            }
            i += code_length as usize;
            if i > bit_length {
                break;
            }
            // wrapping 语义对齐 JS：code_length 可能为 0（移位 32 位 = 不移位），
            // 差值可能为负 → 折算成大下标后 get 返回 None → break
            let code = value.wrapping_sub(bits.wrapping_shr(32 - code_length)) as usize;
            let Some((result, decompressed)) = self.dictionary.get(code) else { break };
            if !*decompressed {
                // 结果本身是压缩的：递归解压（字典缓存交给上层；这里直接算）
                let expanded = self.decompress(result);
                output.extend_from_slice(&expanded);
            } else {
                output.extend_from_slice(result);
            }
        }
        output
    }
}

// ────────────────────────── INDX 索引（NCX / SKEL / FRAG） ──────────────────────────

#[derive(Debug, Default, Clone)]
struct IndexEntry {
    name: String,
    /// tag → 变长值列表
    tag_map: HashMap<u32, Vec<u32>>,
}

#[derive(Debug, Default)]
struct IndexData {
    table: Vec<IndexEntry>,
    /// cncx 偏移 → 文本（标签名等）
    cncx: HashMap<u32, String>,
}

/// INDX 头：magic[0,4] length[4] type[8] idxt[20] numRecords[24] encoding[28] numCncx[52]。
fn parse_indx_header(record: &[u8]) -> AppResult<(usize, usize, usize, usize, usize)> {
    if &record[..4.min(record.len())] != b"INDX" {
        return Err(AppError::other("无效的 INDX 记录"));
    }
    Ok((
        u32_at(record, 4) as usize,            // header length
        u32_at(record, 20) as usize,           // idxt
        u32_at(record, 24) as usize,           // numRecords
        u32_at(record, 28) as usize,           // encoding
        u32_at(record, 52) as usize,           // numCncx
    ))
}

/// 解析一个 INDX 索引（mobi.js getIndexData 直译）。
fn get_index_data(pdb: &Pdb, base: usize, indx_index: usize) -> AppResult<IndexData> {
    let indx_record = pdb.record(base + indx_index)?;
    let (header_len, _idxt, num_records, encoding, num_cncx) = parse_indx_header(indx_record)?;

    // TAGX 表紧跟 INDX 头
    let tagx_buffer = &indx_record[header_len.min(indx_record.len())..];
    if &tagx_buffer[..4.min(tagx_buffer.len())] != b"TAGX" {
        return Err(AppError::other("无效的 TAGX 区段"));
    }
    let tagx_len = u32_at(tagx_buffer, 4) as usize;
    let num_tags = (tagx_len - 12) / 4;
    let tag_table: Vec<(u32, u32, u32, u32)> = (0..num_tags)
        .map(|i| {
            let o = 12 + i * 4;
            (
                tagx_buffer[o] as u32,
                tagx_buffer[o + 1] as u32,
                tagx_buffer[o + 2] as u32,
                tagx_buffer[o + 3] as u32,
            )
        })
        .collect();
    let num_control_bytes = u32_at(tagx_buffer, 8) as usize;

    // CNCX：标签文本字典
    let mut cncx: HashMap<u32, String> = HashMap::new();
    let mut cncx_record_offset: u32 = 0;
    for i in 0..num_cncx {
        let record = pdb.record(base + indx_index + num_records + i + 1)?;
        let mut pos = 0usize;
        while pos < record.len() {
            let index = pos;
            let (value, length) = get_var_len(record, pos);
            pos += length;
            let end = (pos + value as usize).min(record.len());
            let result = &record[pos..end];
            pos = end;
            let text = decode_bytes(result, encoding);
            cncx.insert(cncx_record_offset + index as u32, text);
        }
        cncx_record_offset += 0x10000;
    }

    // 索引条目
    let mut table: Vec<IndexEntry> = vec![];
    for i in 0..num_records {
        let record = pdb.record(base + indx_index + 1 + i)?;
        let (_hl, idxt, entry_count, _enc, _nc) = parse_indx_header(record)?;
        for j in 0..entry_count {
            let offset_offset = idxt + 4 + 2 * j;
            let offset = u16_at(record, offset_offset) as usize;
            if offset >= record.len() {
                continue;
            }
            let length = record[offset] as usize;
            let name = String::from_utf8_lossy(
                record.get(offset + 1..offset + 1 + length).unwrap_or(&[]),
            )
            .into_owned();

            let start_pos = offset + 1 + length;
            let mut control_byte_index = 0usize;
            let mut pos = start_pos + num_control_bytes;
            // 收集 (tag, value_count, value_bytes, num_values)
            let mut tags: Vec<(u32, Option<u32>, Option<u32>, u32)> = vec![];
            for &(tag, num_values, mask, end_flag) in &tag_table {
                if end_flag & 1 != 0 {
                    control_byte_index += 1;
                    continue;
                }
                let ctrl = start_pos + control_byte_index;
                let value = u8_at(record, ctrl) & mask;
                if value == mask {
                    if count_bits_set(mask) > 1 {
                        let (v, l) = get_var_len(record, pos);
                        tags.push((tag, None, Some(v), num_values));
                        pos += l;
                    } else {
                        tags.push((tag, Some(1), None, num_values));
                    }
                } else {
                    tags.push((tag, Some(value >> count_unset_end(mask)), None, num_values));
                }
            }

            let mut tag_map: HashMap<u32, Vec<u32>> = HashMap::new();
            for (tag, value_count, value_bytes, num_values) in tags {
                let mut values: Vec<u32> = vec![];
                if let Some(count) = value_count {
                    for _ in 0..count * num_values {
                        let (v, l) = get_var_len(record, pos);
                        values.push(v);
                        pos += l;
                    }
                } else {
                    let mut acc = 0u32;
                    while acc < value_bytes.unwrap_or(0) {
                        let (v, l) = get_var_len(record, pos);
                        values.push(v);
                        pos += l;
                        acc += l as u32;
                    }
                }
                tag_map.insert(tag, values);
            }
            table.push(IndexEntry { name, tag_map });
        }
    }

    Ok(IndexData { table, cncx })
}

fn u8_at(data: &[u8], off: usize) -> u32 {
    data.get(off).copied().unwrap_or(0) as u32
}

fn decode_bytes(bytes: &[u8], encoding: usize) -> String {
    // SumatraPDF 容错（issue 2529）：raw 中残留的 NUL 一律替换为空格，
    // 避免浏览器把 U+0000 渲染成替换符
    match encoding {
        65001 => String::from_utf8_lossy(bytes).replace('\0', " "),
        _ => encoding_rs::WINDOWS_1252.decode(bytes).0.replace('\0', " "),
    }
}

/// NCX 条目（getNCX 输出）。
struct NcxItem {
    index: usize,
    label: String,
    /// MOBI6: filepos；KF8: (fid, off)
    filepos: Option<u32>,
    pos: Option<(u32, u32)>,
    parent: Option<u32>,
}

/// 解析 NCX 索引（mobi.js getNCX）。
fn get_ncx(pdb: &Pdb, base: usize, indx_index: usize) -> AppResult<Vec<NcxItem>> {
    let data = get_index_data(pdb, base, indx_index)?;
    let items: Vec<NcxItem> = data
        .table
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let m = &entry.tag_map;
            NcxItem {
                index,
                label: m.get(&3).and_then(|v| v.first()).and_then(|k| data.cncx.get(k)).cloned().unwrap_or_default(),
                filepos: m.get(&1).and_then(|v| v.first()).copied(),
                pos: m.get(&6).map(|v| (v.first().copied().unwrap_or(0), v.get(1).copied().unwrap_or(0))),
                parent: m.get(&21).and_then(|v| v.first()).copied(),
            }
        })
        .collect();
    Ok(items)
}

// ────────────────────────── 章节 ──────────────────────────

/// KF8 章节：skeleton 区间 + 待插入的 fragment 列表。
struct Kf8Section {
    skel_offset: usize,
    skel_length: usize,
    frags: Vec<Kf8Frag>,
}

#[derive(Clone)]
struct Kf8Frag {
    /// 插入点（相对 skeleton 起始）
    insert_offset: usize,
    /// fragment 在 raw 中的绝对区间
    offset: usize,
    length: usize,
    /// frag 索引（NCX pos.fid 对应它）
    index: u32,
}

/// 统一章节表示：MOBI6/KF8 都折算成 raw 的字节区间或 KF8 拼装。
enum Section {
    /// MOBI6 / 无 SKEL 的 KF8：raw 上的字节区间
    Range { start: usize, end: usize },
    Kf8(Kf8Section),
}

// ────────────────────────── 主结构 ──────────────────────────

#[allow(dead_code)] // 元数据字段随解析会话保存，供后续扩展（如书架详情）使用
pub struct MobiBook {
    book_id: String,
    pdb: Pdb,
    /// 部分基址：MOBI6 = 0；KF8 = boundary 记录号
    start: usize,
    /// 资源起始（绝对记录号，record0 偏移 108）
    resource_start: usize,
    encoding: usize, // 1252 | 65001
    /// 全部文本记录解压并去尾随后的字节流
    raw: Vec<u8>,
    sections: Vec<Section>,
    /// MOBI6：全部 filepos 引用（字节偏移，升序）
    filepos_list: Vec<u32>,
    toc: Vec<TocItem>,
    title: String,
    author: Option<String>,
    publisher: Option<String>,
    language: Option<String>,
    cover_resource: Option<String>,
    title_fallbacks: Vec<String>,
}

impl MobiBook {
    pub fn open(
        path: impl AsRef<Path>,
        file_size: u64,
        format: BookFormat,
    ) -> AppResult<(BookMeta, Vec<TocItem>, Box<dyn BookSession>)> {
        let path = path.as_ref();
        const MAX_MOBI_FILE_SIZE: u64 = 500 * 1024 * 1024;
        if file_size > MAX_MOBI_FILE_SIZE {
            return Err(AppError::other(format!(
                "MOBI 文件过大（{} MB，上限 {} MB），暂不支持打开",
                file_size / 1024 / 1024,
                MAX_MOBI_FILE_SIZE / 1024 / 1024
            )));
        }

        let pdb = Pdb::parse(std::fs::read(path)?)?;
        // SumatraPDF（issue 1315）：Print Replica / AZW4 是 MOBI 包装的 PDF，正文不是 HTML
        if pdb
            .record(1)
            .map(|r| r.len() >= 4 && &r[..4] == b"%MOP")
            .unwrap_or(false)
        {
            return Err(AppError::other("Print Replica / AZW4（PDF 包装）暂不支持"));
        }
        let rec0 = pdb.record(0)?;

        // ── PalmDOC 头 + DRM 检测 ──
        let mut compression = u16_at(rec0, 0);
        let mut num_text_records = u16_at(rec0, 8) as usize;
        let mut encryption = u16_at(rec0, 12);
        if encryption != 0 {
            return Err(AppError::DrmDetected);
        }

        // ── MOBI 头（老 PalmDOC（TEXtREAd）没有 MOBI 头，按纯 PalmDOC 处理） ──
        let has_mobi = rec0.len() >= 20 && &rec0[16..20] == b"MOBI";
        let mut start: usize = 0;
        let mut encoding: usize = 1252;
        let mut resource_start: usize = 0;
        let mut huffcdic: usize = 0;
        let mut num_huffcdic: usize = 0;
        let mut trailing_flags: u32 = 0;
        let mut indx: usize = u32::MAX as usize;
        let mut exth: HashMap<u32, ExthValue> = HashMap::new();
        let mut embedded_title: Option<String> = None;

        if has_mobi {
            let mobi_len = u32_at(rec0, 20) as usize;
            encoding = u32_at(rec0, OFF_TEXT_ENCODING) as usize;
            resource_start = u32_at(rec0, OFF_FIRST_IMAGE) as usize;
            huffcdic = u32_at(rec0, OFF_HUFFCDIC) as usize;
            num_huffcdic = u32_at(rec0, OFF_NUM_HUFFCDIC) as usize;
            trailing_flags = u32_at(rec0, OFF_TRAILING_FLAGS);
            indx = u32_at(rec0, OFF_INDX) as usize;

            // EXTH
            if u32_at(rec0, OFF_EXTH_FLAG) & 0x40 != 0 {
                let exth_start = mobi_len + 16;
                parse_exth(rec0, exth_start, encoding, &mut exth);
            }

            // 内嵌标题
            let title_offset = u32_at(rec0, OFF_FULL_NAME_OFFSET) as usize;
            let title_length = u32_at(rec0, OFF_FULL_NAME_LENGTH) as usize;
            if title_length > 0 && title_offset + title_length <= rec0.len() {
                embedded_title = Some(decode_bytes(&rec0[title_offset..title_offset + title_length], encoding));
            }

            // 混合封装：EXTH 121 boundary → 尝试打开 KF8 部分
            if format != BookFormat::Azw3 {
                let boundary = match exth.get(&EXTH_BOUNDARY) {
                    Some(ExthValue::Uint(b)) if *b < 0xffff_ffff => *b,
                    _ => 0,
                };
                if boundary != 0 {
                    let rec = pdb.record(boundary as usize)?;
                    if rec.len() >= 20 && &rec[16..20] == b"MOBI" {
                        // 跳转后整个 PalmDOC + MOBI 头按 KF8 部分重读，
                        // 两部分的压缩类型 / 文本记录数 / 加密位互相独立
                        compression = u16_at(rec, 0);
                        num_text_records = u16_at(rec, 8) as usize;
                        encryption = u16_at(rec, 12);
                        if encryption != 0 {
                            return Err(AppError::DrmDetected);
                        }
                        let mobi_len2 = u32_at(rec, 20) as usize;
                        encoding = u32_at(rec, OFF_TEXT_ENCODING) as usize;
                        // Hybrid 的图片由两套正文共享，以首个 MOBI 头的资源起点为准。
                        // KF8 头的 resourceStart 用于其辅助记录，不能覆盖图片基址。
                        huffcdic = u32_at(rec, OFF_HUFFCDIC) as usize;
                        num_huffcdic = u32_at(rec, OFF_NUM_HUFFCDIC) as usize;
                        trailing_flags = u32_at(rec, OFF_TRAILING_FLAGS);
                        indx = u32_at(rec, OFF_INDX) as usize;
                        exth.clear();
                        if u32_at(rec, OFF_EXTH_FLAG) & 0x40 != 0 {
                            parse_exth(rec, mobi_len2 + 16, encoding, &mut exth);
                        }
                        let to = u32_at(rec, OFF_FULL_NAME_OFFSET) as usize;
                        let tl = u32_at(rec, OFF_FULL_NAME_LENGTH) as usize;
                        embedded_title = if tl > 0 && to + tl <= rec.len() {
                            Some(decode_bytes(&rec[to..to + tl], encoding))
                        } else {
                            None
                        };
                        start = boundary as usize;
                    }
                }
            }
        }

        let version = if has_mobi { u32_at(pdb.record(start)?, OFF_FILE_VERSION) } else { 0 };
        let is_kf8 = version >= 8;

        // ── 解压器 ──
        let huff = if compression == 17480 {
            if huffcdic == 0 || num_huffcdic == 0 {
                return Err(AppError::other("HUFF/CDIC 头缺失"));
            }
            Some(HuffCdic::parse(&pdb, start + huffcdic, num_huffcdic)?)
        } else {
            None
        };
        let remove_trailing = |data: &[u8]| -> Vec<u8> {
            let mut out = data.to_vec();
            let num_entries = count_bits_set(trailing_flags >> 1);
            for _ in 0..num_entries {
                let len = get_var_len_from_end(&out);
                if len == 0 || len > out.len() {
                    break;
                }
                out.truncate(out.len() - len);
            }
            if trailing_flags & 1 != 0 && !out.is_empty() {
                let len = (out[out.len() - 1] & 0b11) as usize + 1;
                out.truncate(out.len().saturating_sub(len));
            }
            out
        };
        let decompress = |data: &[u8]| -> AppResult<Vec<u8>> {
            match compression {
                1 => Ok(data.to_vec()),
                2 => Ok(decompress_palmdoc(data)),
                17480 => Ok(huff.as_ref().map(|h| h.decompress(data)).unwrap_or_default()),
                other => Err(AppError::other(format!("未知压缩类型: {other}"))),
            }
        };

        // ── 全量解压文本记录 ──
        let mut raw: Vec<u8> = Vec::new();
        for i in 0..num_text_records {
            let record = pdb.record(start + i + 1)?;
            let trimmed = remove_trailing(record);
            raw.extend_from_slice(&decompress(&trimmed)?);
        }

        // ── 章节切分 ──
        let mut sections: Vec<Section> = vec![];
        let mut filepos_list: Vec<u32> = vec![];
        let mut kf8_frag_info: Vec<(u32, usize, usize)> = vec![]; // (frag index, offset, length) 供 NCX 定位

        if is_kf8 {
            let fdst = u32_at(pdb.record(start)?, OFF_KF8_FDST) as usize;
            let skel_index = u32_at(pdb.record(start)?, OFF_KF8_SKEL) as usize;
            let frag_index = u32_at(pdb.record(start)?, OFF_KF8_FRAG) as usize;

            let _ = fdst; // 全量解压方案下 FDST 仅用于完整性校验，跳过
            let skel_data = get_index_data(&pdb, start, skel_index)?;
            let frag_data = get_index_data(&pdb, start, frag_index)?;

            let mut frag_table: Vec<Kf8Frag> = frag_data
                .table
                .iter()
                .filter_map(|entry| {
                    let insert_offset: usize = entry.name.parse().ok()?;
                    let m = &entry.tag_map;
                    Some(Kf8Frag {
                        insert_offset,
                        index: m.get(&4)?.first().copied()?,
                        offset: m.get(&6)?.first().copied()? as usize,
                        length: m.get(&6).and_then(|v| v.get(1)).copied()? as usize,
                    })
                })
                .collect();
            frag_table.sort_by_key(|f| f.insert_offset);

            // 每个 skeleton 条目占 frag_table 中连续 numFrag 个 fragment
            let mut frag_start = 0usize;
            for entry in &skel_data.table {
                let m = &entry.tag_map;
                let num_frag = m.get(&1).and_then(|v| v.first()).copied().unwrap_or(0) as usize;
                let skel_offset = m.get(&6).and_then(|v| v.first()).copied().unwrap_or(0) as usize;
                let skel_length = m.get(&6).and_then(|v| v.get(1)).copied().unwrap_or(0) as usize;
                let frags: Vec<Kf8Frag> = frag_table
                    .iter()
                    .skip(frag_start)
                    .take(num_frag)
                    .cloned()
                    .collect();
                frag_start += num_frag;
                if !frags.is_empty() {
                    // NCX 锚点定位用：frag 数据在 raw 中的绝对起点 =
                    // skel_offset + skel_length + frag.offset（frag 数据紧跟该节骨架文本之后）
                    for f in &frags {
                        kf8_frag_info.push((f.index, skel_offset + skel_length + f.offset, f.length));
                    }
                    sections.push(Section::Kf8(Kf8Section { skel_offset, skel_length, frags }));
                }
            }
        }

        if sections.is_empty() {
            // MOBI6：按 <mbp:pagebreak> 切分（字节级，正则只匹配 ASCII 标签）
            let pb_re = re_pagebreak_bytes();
            let mut starts: Vec<usize> = vec![0];
            for m in pb_re.find_iter(&raw) {
                starts.push(m.start());
            }
            for (i, &s) in starts.iter().enumerate() {
                let e = starts.get(i + 1).copied().unwrap_or(raw.len());
                sections.push(Section::Range { start: s, end: e });
            }

            // 收集 filepos 引用（`<a filepos=...>` 等）
            let fp_re = re_filepos_attr_bytes();
            let mut set: Vec<u32> = fp_re
                .captures_iter(&raw)
                .filter_map(|c| {
                    c.get(1)
                        .and_then(|d| std::str::from_utf8(d.as_bytes()).ok())
                        .and_then(|s| s.parse().ok())
                })
                .collect();
            set.sort_unstable();
            set.dedup();
            filepos_list = set;
        }

        // ── 元数据 ──
        let exth_str = |k: u32| -> Option<String> {
            exth.get(&k).and_then(|v| v.as_string().cloned())
        };
        let title = exth_str(EXTH_TITLE)
            .or(embedded_title)
            .map(|t| crate::parser::unescape_entities(&t))
            .unwrap_or_else(|| title_from_path(path));
        let author = exth_str(EXTH_AUTHOR).map(|s| crate::parser::unescape_entities(&s));
        let publisher = exth_str(EXTH_PUBLISHER);
        let language = exth_str(EXTH_LANGUAGE);

        // 封面：EXTH 201（缩略图 202 兜底）→ 绝对记录 = resource_start + offset
        let cover_resource = cover_offset(&exth)
            .map(|o| format!("rec:{o}"));

        // ── 目录 ──
        let mut toc: Vec<TocItem> = vec![];
        // 1) NCX
        if indx < 0xffff_ffff as usize {
            if let Ok(items) = get_ncx(&pdb, start, indx) {
                toc = build_toc_from_ncx(&items, is_kf8, &sections, &kf8_frag_info, &raw, encoding);
                // NCX 的 filepos 一并注册为锚点（正文里未必有对应 <a filepos> 引用，
                // 不并入则 load_chapter 不注入锚点 → 目录点击无法精确定位到条目处）
                filepos_list.extend(items.iter().filter_map(|i| i.filepos));
                filepos_list.sort_unstable();
                filepos_list.dedup();
            }
        }
        // 2) MOBI6 guide（<reference type="toc">）
        if toc.is_empty() && !is_kf8 {
            toc = build_toc_from_guide(&raw, encoding, &sections, &filepos_list);
        }
        // 3) 兜底：每节取第一个标题
        let title_fallbacks: Vec<String> = sections
            .iter()
            .map(|s| {
                let bytes = match s {
                    Section::Range { start, end } => &raw[*start..(*end).min(raw.len())],
                    Section::Kf8(s) => &raw[s.skel_offset..(s.skel_offset + s.skel_length).min(raw.len())],
                };
                section_fallback_title(bytes, encoding)
            })
            .collect();
        if toc.is_empty() {
            toc = title_fallbacks
                .iter()
                .enumerate()
                .map(|(i, t)| TocItem {
                    id: format!("mb-{}", i),
                    label: if t.is_empty() { format!("第 {} 节", i + 1) } else { t.clone() },
                    chapter_index: i as u32,
                    anchor: None,
                    children: vec![],
                })
                .collect();
        }

        let book_id = make_book_id(path, file_size);
        let meta = BookMeta {
            id: book_id.clone(),
            title: title.clone(),
            author,
            publisher,
            language,
            format,
            file_size,
            file_path: path.to_string_lossy().to_string(),
            total_chapters: sections.len() as u32,
            cover_resource,
        };

        Ok((
            meta,
            toc.clone(),
            Box::new(MobiBook {
                book_id,
                pdb,
                start,
                resource_start,
                encoding,
                raw,
                sections,
                filepos_list,
                toc,
                title,
                author: None,
                publisher: None,
                language: None,
                cover_resource: None,
                title_fallbacks,
            }),
        ))
    }

    /// 资源记录绝对下标 = resource_start + n（mobi.js loadResource 语义）。
    fn resource_record(&self, n: usize) -> usize {
        self.resource_start + n
    }

    /// reader-res 资源 URL（复用 epub.rs 的 Windows/其他平台规则）。
    fn resource_url(&self, n: usize) -> String {
        let path = format!("rec:{}", n);
        #[cfg(windows)]
        {
            format!("http://reader-res.localhost/{}/{path}", self.book_id)
        }
        #[cfg(not(windows))]
        {
            format!("reader-res://localhost/{}/{path}", self.book_id)
        }
    }

    /// KF8: kindle:embed:XXXX → 资源记录号（1-based）。
    fn kindle_embed_record(&self, hex: &str) -> Option<usize> {
        let id = usize::from_str_radix(hex, 32).ok()?;
        Some(self.resource_record(id - 1))
    }
}

// ────────────────────────── EXTH ──────────────────────────

#[derive(Debug, Clone)]
enum ExthValue {
    Uint(u32),
    Str(String),
}

impl ExthValue {
    fn as_uint(&self) -> Option<u32> {
        match self {
            ExthValue::Uint(v) => Some(*v),
            _ => None,
        }
    }
    fn as_string(&self) -> Option<&String> {
        match self {
            ExthValue::Str(s) => Some(s),
            _ => None,
        }
    }
}

fn parse_exth(rec0: &[u8], start: usize, encoding: usize, out: &mut HashMap<u32, ExthValue>) {
    if start + 12 > rec0.len() || &rec0[start..start + 4] != b"EXTH" {
        return;
    }
    let count = u32_at(rec0, start + 8);
    let mut offset = start + 12;
    for _ in 0..count {
        if offset + 8 > rec0.len() {
            break;
        }
        let rtype = u32_at(rec0, offset);
        let rlen = (u32_at(rec0, offset + 4) as usize).max(8);
        let data = &rec0[(offset + 8).min(rec0.len())..(offset + rlen).min(rec0.len())];
        let value = match rtype {
            EXTH_BOUNDARY | EXTH_COVER_OFFSET | EXTH_THUMB_OFFSET => ExthValue::Uint(u32_at(data, 0)),
            EXTH_AUTHOR | EXTH_PUBLISHER | EXTH_TITLE | EXTH_LANGUAGE | EXTH_RIGHTS => {
                ExthValue::Str(decode_bytes(data, encoding).trim().to_string())
            }
            _ => ExthValue::Str(decode_bytes(data, encoding)),
        };
        match out.entry(rtype) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(value);
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                // 多值（如多 author）：保留第一个即可
            }
        }
        offset += rlen;
    }
}

// ────────────────────────── 目录构建 ──────────────────────────

/// NCX → TocItem 树。
/// KF8：pos = (fid, off)，fid 定位 frag → 章节，off 定位 frag 内 id；
/// MOBI6：tag1 = filepos 字节偏移 → 章节 + 注入锚点。
fn build_toc_from_ncx(
    items: &[NcxItem],
    is_kf8: bool,
    sections: &[Section],
    kf8_frag_info: &[(u32, usize, usize)],
    raw: &[u8],
    encoding: usize,
) -> Vec<TocItem> {
    // 预计算：MOBI6 filepos → 章节
    // filepos 归属：属于「end 大于它的第一个区间」，即 [start, end) 左闭右开；
    // filepos 恰为某章起点（pagebreak 处）时属于该章而非前一章。越界钳到最后一章。
    let section_of_filepos = |filepos: u32| -> Option<u32> {
        let fp = filepos as u64;
        sections
            .iter()
            .position(|s| match s {
                Section::Range { end, .. } => (*end as u64) > fp,
                _ => false,
            })
            .map(|i| i as u32)
            .or_else(|| sections.len().checked_sub(1).map(|l| l as u32))
    };
    // KF8 frag index → 章节号
    let section_of_fid = |fid: u32| -> Option<u32> {
        // frags 按 section 顺序排列：找到包含该 frag 的最后一段
        let mut result: Option<u32> = None;
        let mut acc = 0usize;
        for (i, s) in sections.iter().enumerate() {
            if let Section::Kf8(sec) = s {
                let count = sec.frags.len();
                if sec.frags.iter().any(|f| f.index == fid) {
                    result = Some(i as u32);
                }
                acc += count;
            }
        }
        let _ = acc;
        result
    };

    fn get_children(
        items: &[NcxItem],
        parent_index: usize,
        is_kf8: bool,
        sections: &[Section],
        kf8_frag_info: &[(u32, usize, usize)],
        raw: &[u8],
        encoding: usize,
        section_of_filepos: &dyn Fn(u32) -> Option<u32>,
        section_of_fid: &dyn Fn(u32) -> Option<u32>,
    ) -> Vec<TocItem> {
        let mut out = vec![];
        for item in items {
            if item.parent != Some(parent_index as u32) {
                continue;
            }
            let (chapter_index, anchor) = if is_kf8 {
                match item.pos {
                    Some((fid, off)) => match section_of_fid(fid) {
                        Some(ci) => (ci, kf8_anchor_at(kf8_frag_info, raw, encoding, fid, off)),
                        None => (0, None),
                    },
                    None => (0, None),
                }
            } else {
                match item.filepos {
                    Some(fp) => (section_of_filepos(fp).unwrap_or(0), Some(format!("filepos{fp}"))),
                    None => (0, None),
                }
            };
            out.push(TocItem {
                id: format!("mbncx-{}", item.index),
                label: if item.label.is_empty() { "未命名".into() } else { item.label.clone() },
                chapter_index,
                anchor,
                children: get_children(
                    items,
                    item.index,
                    is_kf8,
                    sections,
                    kf8_frag_info,
                    raw,
                    encoding,
                    section_of_filepos,
                    section_of_fid,
                ),
            });
        }
        out
    }

    // 根：headingLevel（tag4）== 0 或没有 parent 的项
    let roots: Vec<&NcxItem> = items
        .iter()
        .filter(|i| i.parent.is_none())
        .collect();
    if roots.is_empty() {
        return vec![];
    }
    let mut out = vec![];
    for item in roots {
        let (chapter_index, anchor) = if is_kf8 {
            match item.pos {
                Some((fid, off)) => match section_of_fid(fid) {
                    Some(ci) => (ci, kf8_anchor_at(kf8_frag_info, raw, encoding, fid, off)),
                    None => (0, None),
                },
                None => (0, None),
            }
        } else {
            match item.filepos {
                Some(fp) => (section_of_filepos(fp).unwrap_or(0), Some(format!("filepos{fp}"))),
                None => (0, None),
            }
        };
        out.push(TocItem {
            id: format!("mbncx-{}", item.index),
            label: if item.label.is_empty() { "未命名".into() } else { item.label.clone() },
            chapter_index,
            anchor,
            children: get_children(
                items,
                item.index,
                is_kf8,
                sections,
                kf8_frag_info,
                raw,
                encoding,
                &section_of_filepos,
                &section_of_fid,
            ),
        });
    }
    out
}

/// KF8：在 frag (fid) 内 off 偏移处找 id/name 属性 → 锚点字符串。
fn kf8_anchor_at(
    kf8_frag_info: &[(u32, usize, usize)],
    raw: &[u8],
    encoding: usize,
    fid: u32,
    off: u32,
) -> Option<String> {
    let (_, frag_offset, frag_length) = *kf8_frag_info.iter().find(|(i, _, _)| *i == fid)?;
    let start = frag_offset + off as usize;
    let end = (frag_offset + frag_length).min(raw.len());
    if start >= end {
        return None;
    }
    let snippet = decode_bytes(&raw[start..end.min(start + 512)], encoding);
    let re = re_frag_anchor_attr();
    let caps = re.captures(&snippet)?;
    let attr = caps[1].to_ascii_lowercase();
    let value = caps[2].to_string();
    if value.is_empty() {
        return None;
    }
    // 只有 id/name 能映射为元素定位（aid 由前端按属性查询）
    if attr == "aid" {
        Some(format!("aid:{value}"))
    } else {
        Some(value)
    }
}

/// MOBI6 guide 目录：section 0 中的 `<reference type="...toc...">` 指向目录节，
/// 目录节内的 `<a filepos=N>标题</a>` 生成条目。
fn build_toc_from_guide(
    raw: &[u8],
    encoding: usize,
    sections: &[Section],
    filepos_list: &[u32],
) -> Vec<TocItem> {
    let first = match sections.first() {
        Some(Section::Range { start, end }) => &raw[*start..(*end).min(raw.len())],
        _ => return vec![],
    };
    let first_html = decode_bytes(first, encoding);

    let ref_re = re_reference_tag();
    let type_re = re_type_attr();
    let fp_re = re_filepos_attr();

    let mut toc_filepos: Option<u32> = None;
    for m in ref_re.find_iter(&first_html) {
        let tag = m.as_str();
        let t = type_re.captures(tag).map(|c| c[1].to_lowercase()).unwrap_or_default();
        if t.split_whitespace().any(|k| k.contains("toc")) {
            if let Some(c) = fp_re.captures(tag) {
                toc_filepos = c[1].parse().ok();
            }
            break;
        }
    }
    let Some(toc_filepos) = toc_filepos else { return vec![] };

    // 定位目录节并解码（[start, end) 左闭右开，与 section_of_filepos 同语义）
    let Some((s_start, s_end)) = sections.iter().find_map(|s| match s {
        Section::Range { start, end } if (*start as u64) <= toc_filepos as u64 && (toc_filepos as u64) < *end as u64 => {
            Some((*start, *end))
        }
        _ => None,
    }) else {
        return vec![];
    };
    let html = decode_bytes(&raw[s_start..s_end.min(raw.len())], encoding);

    // 目录节内的 filepos 锚点列表（<a filepos=..>text</a>）
    let a_re = re_guide_filepos_anchor();
    let strip = re_strip_tags();
    let mut out = vec![];
    for caps in a_re.captures_iter(&html) {
        let Ok(fp) = caps[1].parse::<u32>() else { continue };
        let label = crate::parser::unescape_entities(strip.replace_all(&caps[2], "").trim());
        if label.is_empty() {
            continue;
        }
        // filepos 必须是已注册锚点
        if filepos_list.binary_search(&fp).is_err() {
            continue;
        }
        let chapter_index = sections
            .iter()
            .position(|s| match s {
                Section::Range { end, .. } => (*end as u64) > fp as u64,
                _ => false,
            })
            .unwrap_or(0) as u32;
        out.push(TocItem {
            id: format!("mbguide-{}", out.len()),
            label,
            chapter_index,
            anchor: Some(format!("filepos{fp}")),
            children: vec![],
        });
    }
    out
}

/// 章节兜底标题：第一个 h1~h6 文本。
fn section_fallback_title(bytes: &[u8], encoding: usize) -> String {
    let prefix = &bytes[..bytes.len().min(4096)];
    let html = decode_bytes(prefix, encoding);
    let re = re_heading_fallback();
    if let Some(caps) = re.captures(&html) {
        let strip = re_strip_tags();
        let text = strip.replace_all(&caps[1], "").trim().to_string();
        return crate::parser::unescape_entities(&text);
    }
    String::new()
}

// ────────────────────────── BookSession ──────────────────────────

impl BookSession for MobiBook {
    fn chapter_count(&self) -> usize {
        self.sections.len()
    }

    fn load_chapter(&self, index: usize) -> AppResult<ChapterContent> {
        let section = self
            .sections
            .get(index)
            .ok_or(AppError::ChapterOutOfRange(index as u32))?;

        // 1) 取原始字节（KF8 需拼装 fragment）
        let assembled: Vec<u8>;
        let bytes: &[u8] = match section {
            Section::Range { start, end } => &self.raw[*start..(*end).min(self.raw.len())],
            Section::Kf8(sec) => {
                let skel_start = sec.skel_offset;
                let skel_end = (skel_start + sec.skel_length).min(self.raw.len());
                let mut buf: Vec<u8> = self.raw[skel_start..skel_end].to_vec();
                // loadSection 约定：
                // - frag 数据区紧跟该节骨架文本之后（绝对起点 = skel_offset + skel_length + frag.offset）
                // - 插入点 name 是全书 raw 的绝对偏移，需减去 skel_offset 相对化
                let frag_base = sec.skel_offset + sec.skel_length;
                let mut inserted: usize = 0;
                for frag in &sec.frags {
                    let at = frag.insert_offset.saturating_sub(sec.skel_offset) + inserted;
                    let frag_start = frag_base + frag.offset;
                    let frag_bytes = self
                        .raw
                        .get(frag_start..(frag_start + frag.length).min(self.raw.len()))
                        .unwrap_or(&[]);
                    let at = at.min(buf.len());
                    buf.splice(at..at, frag_bytes.iter().copied());
                    inserted += frag_bytes.len();
                }
                assembled = buf;
                &assembled
            }
        };

        // 2) MOBI6：注入 filepos 锚点（`<a id="fileposN"></a>`）
        let injected: Vec<u8>;
        let bytes: &[u8] = match section {
            Section::Range { start, end } if !self.filepos_list.is_empty() => {
                let (s, e) = (*start, (*end).min(self.raw.len()));
                let mut buf = self.raw[s..e].to_vec();
                let mut inserted: usize = 0;
                for &fp in &self.filepos_list {
                    let fpu = fp as usize;
                    if fpu < s || fpu >= e {
                        continue;
                    }
                    let at = fpu - s + inserted;
                    let tag = format!(r#"<a id="filepos{fp}"></a>"#);
                    let at = at.min(buf.len());
                    buf.splice(at..at, tag.bytes());
                    inserted += tag.len();
                }
                injected = buf;
                &injected
            }
            _ => bytes,
        };

        // 3) 解码 + 清理 pagebreak 标签
        let mut html = decode_bytes(bytes, self.encoding);
        let pb_re = re_pagebreak_tag();
        html = pb_re.replace_all(&html, "").into_owned();

        // 4) 资源引用改写（先于 ammonia；CSP 在 webview 层拦截真正的外链 http）
        //    MOBI6: recindex="N"；KF8: kindle:embed:XXXX
        let is_kf8 = matches!(section, Section::Kf8(_));
        if is_kf8 {
            let re = re_kindle_embed();
            html = re
                .replace_all(&html, |caps: &regex::Captures| match self.kindle_embed_record(&caps[1]) {
                    Some(rec) => self.resource_url(rec),
                    None => String::new(),
                })
                .into_owned();
        } else {
            let re = re_recindex();
            html = re
                .replace_all(&html, |caps: &regex::Captures| {
                    let n: usize = caps[1].parse().unwrap_or(0);
                    if n == 0 {
                        return String::new();
                    }
                    format!(r#"src="{}""#, self.resource_url(self.resource_record(n - 1) - self.resource_start))
                })
                .into_owned();
        }

        // 5) ammonia 清洗（放行 http scheme：reader-res 走 http 形式；
        //    真正的外链图片由 webview CSP img-src 白名单拦截）
        let html = sanitize_mobi_html(&html);

        let title = self
            .title_fallbacks
            .get(index)
            .cloned()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| format!("第 {} 节", index + 1));

        Ok(ChapterContent {
            book_id: self.book_id.clone(),
            chapter_index: index as u32,
            title,
            kind: ChapterKind::Html,
            html: Some(html),
            text: None,
            image_refs: None,
        })
    }

    fn load_resource_bytes(&self, path: &str) -> AppResult<(String, Vec<u8>)> {
        // 资源路径约定：rec:<n>（n 为 0-based 资源序号）
        let n = path
            .strip_prefix("rec:")
            .and_then(|s| s.parse::<usize>().ok())
            .ok_or_else(|| AppError::ResourceNotFound(path.to_string()))?;
        let record = self.resource_record(n);
        let bytes = self.pdb.record(record)?;
        if bytes.len() < 4 {
            return Err(AppError::ResourceNotFound(path.to_string()));
        }
        let magic = [bytes[0], bytes[1], bytes[2], bytes[3]];
        let (skip, mime): (usize, &str) = match magic {
            [0xFF, 0xD8, 0xFF, _] => (0, "image/jpeg"),
            [0x89, b'P', b'N', b'G'] => (0, "image/png"),
            [b'G', b'I', b'F', b'8'] => (0, "image/gif"),
            [b'B', b'M', _, _] => (0, "image/bmp"),
            [b'F', b'O', b'N', b'T'] => (12, "font/ttf"),
            [b'V', b'I', b'D', b'E'] | [b'A', b'U', b'D', b'I'] => (12, "application/octet-stream"),
            _ => (0, "application/octet-stream"),
        };
        Ok((mime.to_string(), bytes[skip..].to_vec()))
    }
}

/// MOBI 专用清洗：在共享白名单基础上放行 http（reader-res 资源 URL 形式），
/// 并保留 aid 属性（KF8 NCX 锚点）。真正的远程图片由 CSP 拦截。
fn sanitize_mobi_html(html: &str) -> String {
    use ammonia::Builder;
    use std::collections::{HashMap, HashSet};

    let tags: HashSet<&str> = [
        "p", "div", "span", "br", "hr", "h1", "h2", "h3", "h4", "h5", "h6",
        "em", "strong", "b", "i", "u", "s", "sup", "sub", "small", "big",
        "blockquote", "q", "cite", "pre", "code",
        "ul", "ol", "li", "dl", "dt", "dd",
        "table", "thead", "tbody", "tfoot", "tr", "th", "td", "caption",
        "img", "figure", "figcaption", "a", "ruby", "rt", "rp",
    ]
    .into_iter()
    .collect();

    let generic_attrs: HashSet<&str> = ["id", "lang", "dir", "title", "aid"].into_iter().collect();
    let schemes: HashSet<&str> = ["data", "mailto", "http", "reader-res"].into_iter().collect();

    let img_attrs: HashSet<&str> = ["src", "alt", "width", "height"].into_iter().collect();
    let a_attrs: HashSet<&str> = ["href", "title"].into_iter().collect();
    let mut tag_attrs: HashMap<&str, HashSet<&str>> = HashMap::new();
    tag_attrs.insert("img", img_attrs);
    tag_attrs.insert("a", a_attrs);

    Builder::default()
        .tags(tags)
        .generic_attributes(generic_attrs)
        .tag_attributes(tag_attrs)
        .url_schemes(schemes)
        .url_relative(ammonia::UrlRelative::PassThrough)
        .link_rel(None)
        .clean(html)
        .to_string()
}


#[cfg(test)]
mod cover_tests {
    use super::*;

    fn set_u32(data: &mut [u8], offset: usize, value: u32) {
        data[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }

    #[test]
    fn hybrid_cover_uses_original_resource_base() {
        fn header(version: u32, resources: u32, boundary: Option<u32>) -> Vec<u8> {
            let mut h = vec![0; 320];
            h[1] = 1; // uncompressed
            h[9] = 1; // one text record
            h[16..20].copy_from_slice(b"MOBI");
            set_u32(&mut h, 20, 264);
            set_u32(&mut h, OFF_TEXT_ENCODING, 65001);
            set_u32(&mut h, OFF_FILE_VERSION, version);
            set_u32(&mut h, OFF_FIRST_IMAGE, resources);
            set_u32(&mut h, OFF_EXTH_FLAG, 0x40);
            set_u32(&mut h, OFF_INDX, u32::MAX);
            set_u32(&mut h, OFF_KF8_SKEL, 2);
            set_u32(&mut h, OFF_KF8_FRAG, 2);
            h[280..284].copy_from_slice(b"EXTH");
            set_u32(&mut h, 284, if boundary.is_some() { 36 } else { 24 });
            set_u32(&mut h, 288, if boundary.is_some() { 2 } else { 1 });
            set_u32(&mut h, 292, EXTH_COVER_OFFSET);
            set_u32(&mut h, 296, 12);
            if let Some(b) = boundary {
                set_u32(&mut h, 304, EXTH_BOUNDARY);
                set_u32(&mut h, 308, 12);
                set_u32(&mut h, 312, b);
            }
            h
        }
        let expected = b"\xff\xd8\xffcorrect cover";
        let mut index = vec![0; 68];
        index[..4].copy_from_slice(b"INDX");
        set_u32(&mut index, 4, 56);
        index[56..60].copy_from_slice(b"TAGX");
        set_u32(&mut index, 60, 12);
        let records = vec![
            header(6, 2, Some(4)), b"<p>old</p>".to_vec(), expected.to_vec(),
            b"\xff\xd8\xffwrong glyph".to_vec(), header(8, 3, None),
            b"<p>new</p>".to_vec(), index,
        ];
        let mut data = vec![0; 78 + records.len() * 8];
        data[60..68].copy_from_slice(b"BOOKMOBI");
        data[76..78].copy_from_slice(&(records.len() as u16).to_be_bytes());
        for (i, r) in records.iter().enumerate() {
            let offset = data.len() as u32;
            set_u32(&mut data, 78 + i * 8, offset);
            data.extend(r);
        }
        let path = std::env::temp_dir().join(format!("kedu-hybrid-cover-{}.mobi", std::process::id()));
        std::fs::write(&path, &data).unwrap();
        assert_eq!(extract_cover(&path).unwrap().unwrap(), expected);
        let result = MobiBook::open(&path, data.len() as u64, BookFormat::Mobi);
        std::fs::remove_file(path).unwrap();
        let (meta, _, book) = result.unwrap();
        assert_eq!(meta.cover_resource.as_deref(), Some("rec:0"));
        let (mime, bytes) = book.load_resource_bytes(meta.cover_resource.as_ref().unwrap()).unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(bytes, expected);
        assert!(book.load_chapter(0).unwrap().html.unwrap().contains("new"));
    }

    #[test]
    #[ignore = "requires KEDU_MOBI_FIXTURE and KEDU_COVER_EXPECTED local files"]
    fn real_mobi_cover_matches_verified_image() {
        let path = std::env::var("KEDU_MOBI_FIXTURE").unwrap();
        let expected = std::fs::read(std::env::var("KEDU_COVER_EXPECTED").unwrap()).unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        assert_eq!(extract_cover(Path::new(&path)).unwrap().unwrap(), expected);
        let (meta, _, book) = MobiBook::open(path, size, BookFormat::Mobi).unwrap();
        let (_, bytes) = book.load_resource_bytes(meta.cover_resource.as_ref().unwrap()).unwrap();
        assert_eq!(bytes, expected);
    }
}
