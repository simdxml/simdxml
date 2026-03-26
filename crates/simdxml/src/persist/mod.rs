//! Persistent structural index — serialize to `.sxi`, load via mmap.
//!
//! The `.sxi` (SIMD XML Index) format stores the complete `XmlIndex` as flat
//! arrays in a single file. On subsequent loads, the XML is mmap'd and arrays
//! are read from the `.sxi` file, avoiding the entire parse pipeline.
//!
//! # File Format
//!
//! ```text
//! [Header: 64 bytes]
//!   magic: [u8; 4]    = b"SXI\x01"
//!   version: u32       = 1
//!   xml_hash: [u8; 8]  = xxh3-64 of XML bytes
//!   tag_count: u32
//!   text_count: u32
//!   name_count: u16
//!   flags: u16          = bit 0: has_name_index, bits 1-15: reserved
//!   bloom: [u8; 16]     = reserved for Phase 3 bloom filter
//!   padding: [u8; 16]
//!
//! [Offset table: N x u64]  byte offsets of each section
//!
//! [Section 0..12]  structural arrays (tag_starts, tag_ends, ...)
//! [Section 13]     name index (name_ids, name_table, flattened posting lists)
//! ```

use crate::error::{Result, SimdXmlError};
use crate::index::{TagType, TextRange, XmlIndex};
use memmap2::Mmap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

const MAGIC: [u8; 4] = *b"SXI\x01";
const VERSION: u32 = 2;
const HEADER_SIZE: usize = 64;
const NUM_SECTIONS: usize = 14;
const OFFSET_TABLE_SIZE: usize = NUM_SECTIONS * 8;

// Flags
const FLAG_HAS_NAME_INDEX: u16 = 1;
const FLAG_HAS_BLOOM: u16 = 2;

/// Compute xxh3-64 content hash for staleness detection.
fn content_hash(data: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(data)
}

/// Read just the bloom filter from an `.sxi` file header.
///
/// This is very fast (reads only 64 bytes) and can be used to skip files
/// without loading the full index.
pub fn read_bloom(sxi_path: impl AsRef<Path>) -> Result<crate::bloom::TagBloom> {
    let mut buf = [0u8; HEADER_SIZE];
    let mut f = File::open(sxi_path)?;
    std::io::Read::read_exact(&mut f, &mut buf)?;

    if &buf[0..4] != &MAGIC {
        return Err(SimdXmlError::InvalidSxi("bad magic bytes".into()));
    }

    let flags = u16::from_le_bytes(buf[26..28].try_into().unwrap());
    if flags & FLAG_HAS_BLOOM == 0 {
        return Ok(crate::bloom::TagBloom::EMPTY);
    }

    let mut bloom_bytes = [0u8; 16];
    bloom_bytes.copy_from_slice(&buf[28..44]);
    Ok(crate::bloom::TagBloom::from_bytes(bloom_bytes))
}

/// Serialize an `XmlIndex` to a `.sxi` file.
///
/// The XML bytes are hashed for staleness detection on future loads.
pub fn serialize_index(
    index: &XmlIndex,
    xml_bytes: &[u8],
    sxi_path: impl AsRef<Path>,
) -> Result<()> {
    let f = File::create(sxi_path)?;
    let mut w = BufWriter::new(f);

    let tag_count = index.tag_count() as u32;
    let text_count = index.text_count() as u32;
    let has_names = !index.name_ids.is_empty();
    let name_count = if has_names { index.name_table.len() as u16 } else { 0 };
    let flags: u16 = if has_names { FLAG_HAS_NAME_INDEX } else { 0 } | FLAG_HAS_BLOOM;
    let xml_hash = content_hash(xml_bytes);
    let bloom = crate::bloom::TagBloom::from_index(index);

    // === Header (64 bytes) ===
    let mut header = [0u8; HEADER_SIZE];
    header[0..4].copy_from_slice(&MAGIC);
    header[4..8].copy_from_slice(&VERSION.to_le_bytes());
    header[8..16].copy_from_slice(&xml_hash.to_le_bytes());
    header[16..20].copy_from_slice(&tag_count.to_le_bytes());
    header[20..24].copy_from_slice(&text_count.to_le_bytes());
    header[24..26].copy_from_slice(&name_count.to_le_bytes());
    header[26..28].copy_from_slice(&flags.to_le_bytes());
    header[28..44].copy_from_slice(&bloom.to_bytes());
    // bytes 44..64: padding
    w.write_all(&header)?;

    // === Compute section sizes and offsets ===
    // Section sizes in bytes — use actual Vec lengths to stay in sync with writes
    let section_sizes: [usize; NUM_SECTIONS] = [
        index.tag_starts.len() * 8,             // 0: tag_starts (u64)
        index.tag_ends.len() * 8,               // 1: tag_ends (u64)
        index.tag_types.len(),                   // 2: tag_types (u8)
        index.tag_names.len() * 10,             // 3: tag_names ((u64, u16) = 10 bytes)
        index.depths.len() * 2,                 // 4: depths (u16)
        index.parents.len() * 4,                // 5: parents (u32)
        index.text_ranges.len() * 20,           // 6: text_ranges (2 x u64 + u32 = 20 bytes)
        index.child_offsets.len() * 4,          // 7: child_offsets (u32)
        index.child_data.len() * 4,             // 8: child_data (u32)
        index.text_child_offsets.len() * 4,     // 9: text_child_offsets (u32)
        index.text_child_data.len() * 4,        // 10: text_child_data (u32)
        index.close_map.len() * 4,              // 11: close_map (u32)
        index.post_order.len() * 4,             // 12: post_order (u32)
        compute_name_section_size(index),        // 13: name index
    ];

    // Offset table: each entry is absolute byte offset from file start
    let mut offsets = [0u64; NUM_SECTIONS];
    let mut pos = (HEADER_SIZE + OFFSET_TABLE_SIZE) as u64;
    for i in 0..NUM_SECTIONS {
        offsets[i] = pos;
        pos += section_sizes[i] as u64;
    }

    // Write offset table
    for &off in &offsets {
        w.write_all(&off.to_le_bytes())?;
    }

    // === Write sections ===

    // 0: tag_starts
    write_u64_slice(&mut w, &index.tag_starts)?;

    // 1: tag_ends
    write_u64_slice(&mut w, &index.tag_ends)?;

    // 2: tag_types (as u8)
    for &tt in &index.tag_types {
        w.write_all(&[tt as u8])?;
    }

    // 3: tag_names ((u64, u16) pairs)
    for &(off, len) in &index.tag_names {
        w.write_all(&off.to_le_bytes())?;
        w.write_all(&len.to_le_bytes())?;
    }

    // 4: depths
    write_u16_slice(&mut w, &index.depths)?;

    // 5: parents
    write_u32_slice(&mut w, &index.parents)?;

    // 6: text_ranges (u64 start, u64 end, u32 parent_tag)
    for range in &index.text_ranges {
        w.write_all(&range.start.to_le_bytes())?;  // u64
        w.write_all(&range.end.to_le_bytes())?;    // u64
        w.write_all(&range.parent_tag.to_le_bytes())?;  // u32
    }

    // 7: child_offsets
    write_u32_slice(&mut w, &index.child_offsets)?;

    // 8: child_data
    write_u32_slice(&mut w, &index.child_data)?;

    // 9: text_child_offsets
    write_u32_slice(&mut w, &index.text_child_offsets)?;

    // 10: text_child_data
    write_u32_slice(&mut w, &index.text_child_data)?;

    // 11: close_map
    write_u32_slice(&mut w, &index.close_map)?;

    // 12: post_order
    write_u32_slice(&mut w, &index.post_order)?;

    // 13: name index
    write_name_section(&mut w, index)?;

    w.flush()?;
    Ok(())
}

/// A self-contained index that owns both the XML bytes and the structural index.
///
/// Loads a pre-built `.sxi` file and the corresponding XML. Dereferences to
/// `XmlIndex` so it can be used anywhere `&XmlIndex` is expected.
pub struct OwnedXmlIndex {
    // SAFETY: `inner` borrows from `xml_data`. Both live in this struct,
    // and `inner` is listed first so it is dropped before `xml_data`.
    inner: XmlIndex<'static>,
    // Must not be moved/dropped while inner references it.
    // Listed second: dropped after inner.
    _xml_data: XmlStorage,
}

enum XmlStorage {
    Mapped(Mmap),
    Owned(Vec<u8>),
}

impl XmlStorage {
    fn as_slice(&self) -> &[u8] {
        match self {
            XmlStorage::Mapped(m) => m,
            XmlStorage::Owned(v) => v,
        }
    }
}

impl std::ops::Deref for OwnedXmlIndex {
    type Target = XmlIndex<'static>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl OwnedXmlIndex {
    /// Access the underlying XmlIndex.
    pub fn as_index(&self) -> &XmlIndex<'_> {
        &self.inner
    }
}

/// Load a `.sxi` index file and the corresponding XML file.
///
/// The XML file is memory-mapped. The structural arrays are read from the
/// `.sxi` file into Vecs (cheap: ~microseconds for typical documents).
/// Returns error if the XML content hash doesn't match (stale index).
pub fn load_index(
    sxi_path: impl AsRef<Path>,
    xml_path: impl AsRef<Path>,
) -> Result<OwnedXmlIndex> {
    let xml_file = File::open(xml_path)?;
    let xml_mmap = unsafe { Mmap::map(&xml_file)? };
    load_index_with_xml(sxi_path, XmlStorage::Mapped(xml_mmap))
}

/// Load a `.sxi` index using XML bytes already in memory.
pub fn load_index_with_bytes(
    sxi_path: impl AsRef<Path>,
    xml_bytes: Vec<u8>,
) -> Result<OwnedXmlIndex> {
    load_index_with_xml(sxi_path, XmlStorage::Owned(xml_bytes))
}

fn load_index_with_xml(
    sxi_path: impl AsRef<Path>,
    xml_data: XmlStorage,
) -> Result<OwnedXmlIndex> {
    let sxi_bytes = std::fs::read(sxi_path)?;
    let xml_bytes = xml_data.as_slice();

    // === Parse header ===
    if sxi_bytes.len() < HEADER_SIZE + OFFSET_TABLE_SIZE {
        return Err(SimdXmlError::InvalidSxi("file too small".into()));
    }

    let magic = &sxi_bytes[0..4];
    if magic != MAGIC {
        return Err(SimdXmlError::InvalidSxi("bad magic bytes".into()));
    }

    let version = u32::from_le_bytes(sxi_bytes[4..8].try_into().unwrap());
    if version != VERSION {
        return Err(SimdXmlError::InvalidSxi(format!("unsupported version {}", version)));
    }

    let stored_hash = u64::from_le_bytes(sxi_bytes[8..16].try_into().unwrap());
    let actual_hash = content_hash(xml_bytes);
    if stored_hash != actual_hash {
        return Err(SimdXmlError::StaleSxi);
    }

    let tag_count = u32::from_le_bytes(sxi_bytes[16..20].try_into().unwrap()) as usize;
    let text_count = u32::from_le_bytes(sxi_bytes[20..24].try_into().unwrap()) as usize;
    let name_count = u16::from_le_bytes(sxi_bytes[24..26].try_into().unwrap()) as usize;
    let flags = u16::from_le_bytes(sxi_bytes[26..28].try_into().unwrap());
    let has_names = flags & FLAG_HAS_NAME_INDEX != 0;

    // === Read offset table ===
    let ot_start = HEADER_SIZE;
    let mut offsets = [0u64; NUM_SECTIONS];
    for i in 0..NUM_SECTIONS {
        let base = ot_start + i * 8;
        offsets[i] = u64::from_le_bytes(sxi_bytes[base..base + 8].try_into().unwrap());
    }

    // Helper to read a section as &[u8]
    let section = |i: usize| -> &[u8] {
        let start = offsets[i] as usize;
        let end = if i + 1 < NUM_SECTIONS {
            offsets[i + 1] as usize
        } else {
            sxi_bytes.len()
        };
        &sxi_bytes[start..end.min(sxi_bytes.len())]
    };

    // === Read sections into Vecs ===

    let tag_starts = read_u64_vec(section(0), tag_count);
    let tag_ends = read_u64_vec(section(1), tag_count);

    let tag_types: Vec<TagType> = section(2)[..tag_count]
        .iter()
        .map(|&b| TagType::from_u8(b).unwrap_or(TagType::Open))
        .collect();

    let tag_names = read_tag_names(section(3), tag_count);
    let depths = read_u16_vec(section(4), tag_count);
    let parents = read_u32_vec(section(5), tag_count);
    let text_ranges = read_text_ranges(section(6), text_count);

    // Derive element counts from section byte sizes
    let section_len = |i: usize| -> usize {
        let start = offsets[i] as usize;
        let end = if i + 1 < NUM_SECTIONS {
            offsets[i + 1] as usize
        } else {
            sxi_bytes.len()
        };
        end.saturating_sub(start)
    };

    let child_offsets = read_u32_vec(section(7), section_len(7) / 4);
    let child_data = read_u32_vec(section(8), section_len(8) / 4);

    let text_child_offsets = read_u32_vec(section(9), section_len(9) / 4);
    let text_child_data = read_u32_vec(section(10), section_len(10) / 4);

    let close_map = read_u32_vec(section(11), tag_count);
    let post_order = read_u32_vec(section(12), tag_count);

    // Name index
    let (name_ids, name_table, name_posting) = if has_names && name_count > 0 {
        read_name_section(section(13), tag_count, name_count)
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

    // SAFETY: We extend the lifetime of `input` to 'static. This is safe because:
    // - `xml_data` is stored in the same struct as the XmlIndex
    // - `xml_data` is dropped after `inner` (field ordering)
    // - `xml_data` is never exposed mutably
    let input: &'static [u8] = unsafe {
        std::mem::transmute::<&[u8], &'static [u8]>(xml_data.as_slice())
    };

    let inner = XmlIndex {
        input,
        tag_starts,
        tag_ends,
        tag_types,
        tag_names,
        depths,
        parents,
        text_ranges,
        child_offsets,
        child_data,
        text_child_offsets,
        text_child_data,
        close_map,
        post_order,
        name_ids,
        name_table,
        name_posting,
    };

    Ok(OwnedXmlIndex {
        inner,
        _xml_data: xml_data,
    })
}

// === Serialization helpers ===

fn write_u32_slice(w: &mut impl Write, data: &[u32]) -> Result<()> {
    for &v in data {
        w.write_all(&v.to_le_bytes())?;
    }
    Ok(())
}

fn write_u64_slice(w: &mut impl Write, data: &[u64]) -> Result<()> {
    for &v in data {
        w.write_all(&v.to_le_bytes())?;
    }
    Ok(())
}

fn write_u16_slice(w: &mut impl Write, data: &[u16]) -> Result<()> {
    for &v in data {
        w.write_all(&v.to_le_bytes())?;
    }
    Ok(())
}

fn compute_name_section_size(index: &XmlIndex) -> usize {
    if index.name_ids.is_empty() {
        return 0;
    }
    let n = index.tag_count();
    let name_count = index.name_table.len();
    // name_ids: n x u16
    // name_table: name_count x (u64 + u16) = 10 bytes each
    // posting_offsets: (name_count + 1) x u32
    // posting_data: total entries x u32
    let total_posting: usize = index.name_posting.iter().map(|p| p.len()).sum();
    n * 2 + name_count * 10 + (name_count + 1) * 4 + total_posting * 4
}

fn write_name_section(w: &mut impl Write, index: &XmlIndex) -> Result<()> {
    if index.name_ids.is_empty() {
        return Ok(());
    }

    // name_ids
    write_u16_slice(w, &index.name_ids)?;

    // name_table
    for &(off, len) in &index.name_table {
        w.write_all(&off.to_le_bytes())?;
        w.write_all(&len.to_le_bytes())?;
    }

    // posting lists as CSR: offsets then data
    let name_count = index.name_table.len();
    let mut offsets = Vec::with_capacity(name_count + 1);
    let mut pos: u32 = 0;
    for posting in &index.name_posting {
        offsets.push(pos);
        pos += posting.len() as u32;
    }
    offsets.push(pos);
    write_u32_slice(w, &offsets)?;

    for posting in &index.name_posting {
        write_u32_slice(w, posting)?;
    }

    Ok(())
}

// === Deserialization helpers ===

fn read_u64_vec(data: &[u8], count: usize) -> Vec<u64> {
    let mut v = Vec::with_capacity(count);
    for i in 0..count {
        let base = i * 8;
        if base + 8 > data.len() { break; }
        v.push(u64::from_le_bytes(data[base..base + 8].try_into().unwrap()));
    }
    v
}

fn read_u32_vec(data: &[u8], count: usize) -> Vec<u32> {
    let mut v = Vec::with_capacity(count);
    for i in 0..count {
        let base = i * 4;
        if base + 4 > data.len() { break; }
        v.push(u32::from_le_bytes(data[base..base + 4].try_into().unwrap()));
    }
    v
}

fn read_u16_vec(data: &[u8], count: usize) -> Vec<u16> {
    let mut v = Vec::with_capacity(count);
    for i in 0..count {
        let base = i * 2;
        if base + 2 > data.len() { break; }
        v.push(u16::from_le_bytes(data[base..base + 2].try_into().unwrap()));
    }
    v
}

fn read_tag_names(data: &[u8], count: usize) -> Vec<(u64, u16)> {
    let mut v = Vec::with_capacity(count);
    for i in 0..count {
        let base = i * 10;
        if base + 10 > data.len() { break; }
        let off = u64::from_le_bytes(data[base..base + 8].try_into().unwrap());
        let len = u16::from_le_bytes(data[base + 8..base + 10].try_into().unwrap());
        v.push((off, len));
    }
    v
}

fn read_text_ranges(data: &[u8], count: usize) -> Vec<TextRange> {
    let mut v = Vec::with_capacity(count);
    for i in 0..count {
        let base = i * 20;
        if base + 20 > data.len() { break; }
        v.push(TextRange {
            start: u64::from_le_bytes(data[base..base + 8].try_into().unwrap()),
            end: u64::from_le_bytes(data[base + 8..base + 16].try_into().unwrap()),
            parent_tag: u32::from_le_bytes(data[base + 16..base + 20].try_into().unwrap()),
        });
    }
    v
}

fn read_name_section(
    data: &[u8],
    tag_count: usize,
    name_count: usize,
) -> (Vec<u16>, Vec<(u64, u16)>, Vec<Vec<u32>>) {
    let mut pos = 0;

    // name_ids: tag_count x u16
    let name_ids = read_u16_vec(&data[pos..], tag_count);
    pos += tag_count * 2;

    // name_table: name_count x (u64 + u16)
    let name_table = read_tag_names(&data[pos..], name_count);
    pos += name_count * 10;

    // posting offsets: (name_count + 1) x u32
    let posting_offsets = read_u32_vec(&data[pos..], name_count + 1);
    pos += (name_count + 1) * 4;

    // posting data
    let total_posting = posting_offsets.last().copied().unwrap_or(0) as usize;
    let posting_data = read_u32_vec(&data[pos..], total_posting);

    // Reconstruct Vec<Vec<u32>> from CSR
    let mut name_posting = Vec::with_capacity(name_count);
    for i in 0..name_count {
        let start = posting_offsets[i] as usize;
        let end = posting_offsets[i + 1] as usize;
        name_posting.push(posting_data[start..end].to_vec());
    }

    (name_ids, name_table, name_posting)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("simdxml_test_{}", name))
    }

    /// Helper: round-trip through .sxi and verify all structural arrays match.
    fn assert_index_eq(original: &XmlIndex, loaded: &XmlIndex) {
        assert_eq!(original.tag_count(), loaded.tag_count(), "tag_count mismatch");
        assert_eq!(original.text_count(), loaded.text_count(), "text_count mismatch");
        assert_eq!(original.tag_starts, loaded.tag_starts, "tag_starts mismatch");
        assert_eq!(original.tag_ends, loaded.tag_ends, "tag_ends mismatch");
        assert_eq!(original.tag_types, loaded.tag_types, "tag_types mismatch");
        assert_eq!(original.tag_names, loaded.tag_names, "tag_names mismatch");
        assert_eq!(original.depths, loaded.depths, "depths mismatch");
        assert_eq!(original.parents, loaded.parents, "parents mismatch");
        assert_eq!(original.close_map, loaded.close_map, "close_map mismatch");
        assert_eq!(original.post_order, loaded.post_order, "post_order mismatch");
        assert_eq!(original.child_offsets, loaded.child_offsets, "child_offsets mismatch");
        assert_eq!(original.child_data, loaded.child_data, "child_data mismatch");
        assert_eq!(original.text_child_offsets, loaded.text_child_offsets);
        assert_eq!(original.text_child_data, loaded.text_child_data);
        for (i, (a, b)) in original.text_ranges.iter().zip(loaded.text_ranges.iter()).enumerate() {
            assert_eq!((a.start, a.end, a.parent_tag), (b.start, b.end, b.parent_tag),
                "text_range[{}] mismatch", i);
        }
    }

    #[test]
    fn round_trip_basic() {
        let xml = b"<root><child>text</child><empty/></root>";
        let index = parse(xml).unwrap();
        let sxi_path = temp_path("basic.sxi");
        serialize_index(&index, xml, &sxi_path).unwrap();

        let owned = load_index_with_bytes(&sxi_path, xml.to_vec()).unwrap();
        assert_index_eq(&index, owned.as_index());

        let orig = index.xpath_text("//child").unwrap();
        let from_sxi = owned.xpath_text("//child").unwrap();
        assert_eq!(orig, from_sxi);

        std::fs::remove_file(&sxi_path).ok();
    }

    #[test]
    fn round_trip_with_name_index() {
        let xml = b"<root><a>1</a><b>2</b><a>3</a></root>";
        let mut index = parse(xml).unwrap();
        index.build_name_index();

        let sxi_path = temp_path("named.sxi");
        serialize_index(&index, xml, &sxi_path).unwrap();

        let owned = load_index_with_bytes(&sxi_path, xml.to_vec()).unwrap();
        let loaded = owned.as_index();

        assert_eq!(index.name_ids, loaded.name_ids);
        assert_eq!(index.name_table, loaded.name_table);
        assert_eq!(index.name_posting, loaded.name_posting);

        std::fs::remove_file(&sxi_path).ok();
    }

    #[test]
    fn stale_detection() {
        let xml = b"<root><child/></root>";
        let index = parse(xml).unwrap();
        let sxi_path = temp_path("stale.sxi");
        serialize_index(&index, xml, &sxi_path).unwrap();

        let modified_xml = b"<root><other/></root>";
        let result = load_index_with_bytes(&sxi_path, modified_xml.to_vec());
        assert!(matches!(result, Err(SimdXmlError::StaleSxi)));

        std::fs::remove_file(&sxi_path).ok();
    }

    #[test]
    fn invalid_magic() {
        let sxi_path = temp_path("bad_magic.sxi");
        std::fs::write(&sxi_path, b"NOT_SXI_FILE_PADDING_TO_FILL_HEADER_PLUS_OFFSET_TABLE_AREA____\
            0000000000000000000000000000000000000000000000000000000000000000\
            0000000000000000000000000000000000000000000000000000000000000000\
            00000000000000000000000000000000").unwrap();
        let result = load_index_with_bytes(&sxi_path, vec![]);
        assert!(matches!(result, Err(SimdXmlError::InvalidSxi(_))));
        std::fs::remove_file(&sxi_path).ok();
    }

    #[test]
    fn empty_document() {
        let xml = b"<r/>";
        let index = parse(xml).unwrap();
        let sxi_path = temp_path("empty.sxi");
        serialize_index(&index, xml, &sxi_path).unwrap();

        let owned = load_index_with_bytes(&sxi_path, xml.to_vec()).unwrap();
        assert_index_eq(&index, owned.as_index());

        std::fs::remove_file(&sxi_path).ok();
    }

    #[test]
    fn real_file_io_mmap() {
        // Write XML to a real temp file, serialize .sxi, then load both via file paths
        // (exercises the mmap codepath in load_index)
        let xml = br#"<corpus>
            <doc id="1"><title>First</title><body>Hello world</body></doc>
            <doc id="2"><title>Second</title><body>Goodbye world</body></doc>
        </corpus>"#;

        let xml_path = temp_path("real_io.xml");
        let sxi_path = temp_path("real_io.sxi");

        std::fs::write(&xml_path, xml).unwrap();

        // Parse from file, serialize
        let xml_bytes = std::fs::read(&xml_path).unwrap();
        let mut index = parse(&xml_bytes).unwrap();
        index.build_name_index();
        serialize_index(&index, &xml_bytes, &sxi_path).unwrap();

        // Load via file paths (mmap'd XML)
        let owned = load_index(&sxi_path, &xml_path).unwrap();

        // Verify XPath works correctly through mmap'd data
        let titles = owned.xpath_text("//title/text()").unwrap();
        assert_eq!(titles, vec!["First", "Second"]);

        let bodies = owned.xpath_text("//body").unwrap();
        assert_eq!(bodies, vec!["Hello world", "Goodbye world"]);

        let by_attr = owned.xpath_text("//doc[@id='2']/title").unwrap();
        assert_eq!(by_attr, vec!["Second"]);

        std::fs::remove_file(&xml_path).ok();
        std::fs::remove_file(&sxi_path).ok();
    }

    #[test]
    fn load_or_parse_creates_and_reuses_sxi() {
        let xml = b"<root><item>data</item></root>";
        let xml_path = temp_path("load_or_parse.xml");
        let sxi_path = xml_path.with_extension("sxi");

        std::fs::write(&xml_path, xml).unwrap();

        // First call: should parse and create .sxi
        let owned1 = crate::load_or_parse(&xml_path).unwrap();
        assert!(sxi_path.exists(), ".sxi file should be created");
        assert_eq!(owned1.xpath_text("//item").unwrap(), vec!["data"]);

        // Second call: should load from .sxi (faster)
        let owned2 = crate::load_or_parse(&xml_path).unwrap();
        assert_eq!(owned2.xpath_text("//item").unwrap(), vec!["data"]);

        std::fs::remove_file(&xml_path).ok();
        std::fs::remove_file(&sxi_path).ok();
    }

    #[test]
    fn comments_cdata_pi() {
        let xml = b"<root><!-- comment --><![CDATA[raw <data>]]><?target instr?><child>text</child></root>";
        let index = parse(xml).unwrap();
        let sxi_path = temp_path("mixed_nodes.sxi");
        serialize_index(&index, xml, &sxi_path).unwrap();

        let owned = load_index_with_bytes(&sxi_path, xml.to_vec()).unwrap();
        assert_index_eq(&index, owned.as_index());

        // Verify all tag types survived round-trip
        for (i, (a, b)) in index.tag_types.iter().zip(owned.tag_types.iter()).enumerate() {
            assert_eq!(a, b, "tag_type[{}] mismatch: {:?} vs {:?}", i, a, b);
        }

        std::fs::remove_file(&sxi_path).ok();
    }

    #[test]
    fn deeply_nested() {
        // 20 levels of nesting
        let mut xml = String::new();
        for i in 0..20 { xml.push_str(&format!("<l{}>", i)); }
        xml.push_str("leaf");
        for i in (0..20).rev() { xml.push_str(&format!("</l{}>", i)); }

        let xml_bytes = xml.as_bytes();
        let index = parse(xml_bytes).unwrap();
        let sxi_path = temp_path("deep.sxi");
        serialize_index(&index, xml_bytes, &sxi_path).unwrap();

        let owned = load_index_with_bytes(&sxi_path, xml_bytes.to_vec()).unwrap();
        assert_index_eq(&index, owned.as_index());

        // Verify depths round-tripped correctly
        assert_eq!(index.depths, owned.depths);
        // Deepest tag should be at depth 19
        let max_depth = owned.depths.iter().max().copied().unwrap_or(0);
        assert_eq!(max_depth, 19);

        std::fs::remove_file(&sxi_path).ok();
    }

    #[test]
    fn attributes_preserved() {
        let xml = br#"<root xmlns:ns="http://example.com">
            <item id="1" class="a" ns:val="x">text1</item>
            <item id="2" class="b" ns:val="y">text2</item>
        </root>"#;

        let index = parse(xml).unwrap();
        let sxi_path = temp_path("attrs.sxi");
        serialize_index(&index, xml, &sxi_path).unwrap();

        let owned = load_index_with_bytes(&sxi_path, xml.to_vec()).unwrap();

        // Attribute access should work through loaded index
        let items = owned.xpath_text("//item[@id='2']").unwrap();
        assert_eq!(items, vec!["text2"]);

        let by_class = owned.xpath_text("//item[@class='a']").unwrap();
        assert_eq!(by_class, vec!["text1"]);

        std::fs::remove_file(&sxi_path).ok();
    }

    #[test]
    fn xpath_comprehensive_equivalence() {
        // Test a wide range of XPath patterns through .sxi round-trip
        let xml = br#"<corpus>
            <patent id="1">
                <title>Widget</title>
                <claims>
                    <claim type="independent" num="1">A device comprising a widget</claim>
                    <claim type="dependent" num="2">The device of claim 1</claim>
                </claims>
                <description>Detailed description here</description>
            </patent>
            <patent id="2">
                <title>Gadget</title>
                <claims>
                    <claim type="independent" num="1">A method for gadgeting</claim>
                </claims>
            </patent>
        </corpus>"#;

        let mut index = parse(xml).unwrap();
        index.build_name_index();
        let sxi_path = temp_path("comprehensive.sxi");
        serialize_index(&index, xml, &sxi_path).unwrap();

        let owned = load_index_with_bytes(&sxi_path, xml.to_vec()).unwrap();

        let queries = [
            // Axes
            "//patent",
            "//claim",
            "//title/text()",
            "/corpus/patent/claims/claim",
            "//claim/ancestor::patent",
            "//title/following-sibling::*",
            "//description/preceding-sibling::*",
            // Predicates
            "//claim[@type='independent']",
            "//claim[@num='1']",
            "//patent[@id='2']/title",
            // Wildcards
            "//patent/*",
            "/corpus/*/title",
            // Multi-step
            "//claims/claim[@type='dependent']",
            "//patent[claims/claim[@type='independent']]/title",
        ];

        for q in &queries {
            let orig_nodes = index.xpath(q).unwrap();
            let loaded_nodes = owned.xpath(q).unwrap();
            assert_eq!(orig_nodes.len(), loaded_nodes.len(),
                "Node count mismatch for {}: {} vs {}", q, orig_nodes.len(), loaded_nodes.len());

            let orig_text = index.xpath_text(q).unwrap();
            let loaded_text = owned.xpath_text(q).unwrap();
            assert_eq!(orig_text, loaded_text, "XPath text mismatch for: {}", q);
        }

        std::fs::remove_file(&sxi_path).ok();
    }
}
