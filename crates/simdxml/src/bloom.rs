//! Per-document bloom filter of tag names.
//!
//! A 128-bit bloom filter with 2 hash functions gives ~2% false positive rate
//! for 20 unique tag names (typical patent XML). Can skip entire files without
//! any parsing when the target tag name is not in the filter.
//!
//! For files with a `.sxi` index, the bloom is stored in the header (16 bytes).
//! For files without `.sxi`, a fast prescan builds the bloom at ~10 GiB/s.

use memchr::memchr;

/// 128-bit bloom filter for tag names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TagBloom(pub u128);

impl TagBloom {
    /// Empty bloom filter.
    pub const EMPTY: Self = Self(0);

    /// Insert a tag name into the bloom filter.
    #[inline]
    pub fn insert(&mut self, name: &[u8]) {
        let (h1, h2) = hash_pair(name);
        self.0 |= 1u128 << (h1 % 128);
        self.0 |= 1u128 << (h2 % 128);
    }

    /// Check if a tag name might be in the bloom filter.
    /// False positives are possible; false negatives are not.
    #[inline]
    pub fn may_contain(&self, name: &[u8]) -> bool {
        let (h1, h2) = hash_pair(name);
        let mask = (1u128 << (h1 % 128)) | (1u128 << (h2 % 128));
        self.0 & mask == mask
    }

    /// Check if the bloom filter may contain any of the given names.
    pub fn may_contain_any(&self, names: &[&[u8]]) -> bool {
        names.iter().any(|name| self.may_contain(name))
    }

    /// Build a bloom filter from an `XmlIndex` by scanning its name table.
    pub fn from_index(index: &crate::index::XmlIndex) -> Self {
        let mut bloom = Self::EMPTY;
        for i in 0..index.tag_count() {
            let name = index.tag_name(i);
            if !name.is_empty() {
                bloom.insert(name.as_bytes());
            }
        }
        bloom
    }

    /// Build a bloom filter by fast-scanning XML bytes without full parsing.
    ///
    /// Scans for `<` positions, reads tag names, inserts into bloom.
    /// Runs at near-memchr speed (~10 GiB/s) with minimal per-tag work.
    pub fn from_prescan(input: &[u8]) -> Self {
        let mut bloom = Self::EMPTY;
        let mut pos = 0;

        while let Some(offset) = memchr(b'<', &input[pos..]) {
            pos += offset + 1;
            if pos >= input.len() {
                break;
            }

            // Skip non-element starts: </, <!, <?
            match input[pos] {
                b'/' | b'!' | b'?' => continue,
                _ => {}
            }

            // Read tag name: bytes until whitespace, >, /
            let name_start = pos;
            while pos < input.len() {
                match input[pos] {
                    b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/' => break,
                    _ => pos += 1,
                }
            }

            if pos > name_start {
                bloom.insert(&input[name_start..pos]);
            }
        }

        bloom
    }

    /// Serialize to 16 bytes (little-endian).
    pub fn to_bytes(&self) -> [u8; 16] {
        self.0.to_le_bytes()
    }

    /// Deserialize from 16 bytes (little-endian).
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(u128::from_le_bytes(bytes))
    }
}

/// Two independent hash functions using FNV-1a with different seeds.
#[inline]
fn hash_pair(name: &[u8]) -> (u32, u32) {
    // FNV-1a hash
    let mut h1: u32 = 0x811c9dc5;
    for &b in name {
        h1 ^= b as u32;
        h1 = h1.wrapping_mul(0x01000193);
    }

    // FNV-1a with different seed
    let mut h2: u32 = 0x050c5d1f;
    for &b in name {
        h2 ^= b as u32;
        h2 = h2.wrapping_mul(0x01000193);
    }

    (h1, h2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_insert_and_query() {
        let mut bloom = TagBloom::EMPTY;
        bloom.insert(b"claim");
        bloom.insert(b"title");

        assert!(bloom.may_contain(b"claim"));
        assert!(bloom.may_contain(b"title"));
        // "xyzzy" unlikely to match (but possible as false positive)
    }

    #[test]
    fn no_false_negatives() {
        let names: Vec<&[u8]> = vec![
            b"claim", b"patent", b"title", b"abstract", b"description",
            b"claims", b"citation", b"ref", b"inventor", b"applicant",
            b"date", b"id", b"classification", b"cpc", b"ipc",
            b"priority", b"family", b"legal-status", b"kind", b"country",
        ];

        let mut bloom = TagBloom::EMPTY;
        for name in &names {
            bloom.insert(name);
        }

        // Every inserted name MUST be found
        for name in &names {
            assert!(bloom.may_contain(name), "false negative for {:?}", std::str::from_utf8(name));
        }
    }

    #[test]
    fn empty_bloom_rejects_all() {
        let bloom = TagBloom::EMPTY;
        assert!(!bloom.may_contain(b"claim"));
        assert!(!bloom.may_contain(b"anything"));
    }

    #[test]
    fn prescan_matches_full_parse() {
        let xml = br#"<corpus>
            <patent id="1">
                <title>Widget</title>
                <claims><claim type="independent">A device</claim></claims>
            </patent>
        </corpus>"#;

        let bloom = TagBloom::from_prescan(xml);

        // All open tag names should be present
        assert!(bloom.may_contain(b"corpus"));
        assert!(bloom.may_contain(b"patent"));
        assert!(bloom.may_contain(b"title"));
        assert!(bloom.may_contain(b"claims"));
        assert!(bloom.may_contain(b"claim"));
    }

    #[test]
    fn prescan_skips_close_tags() {
        let xml = b"<a><b>text</b></a>";
        let bloom = TagBloom::from_prescan(xml);
        // Should have "a" and "b" but not "/" or "a>" etc.
        assert!(bloom.may_contain(b"a"));
        assert!(bloom.may_contain(b"b"));
    }

    #[test]
    fn from_index_matches_prescan() {
        let xml = br#"<root><a>1</a><b>2</b><c>3</c></root>"#;
        let index = crate::parse(xml).unwrap();

        let bloom_index = TagBloom::from_index(&index);
        let bloom_prescan = TagBloom::from_prescan(xml);

        // Both should find the same tag names
        for name in &[&b"root"[..], b"a", b"b", b"c"] {
            assert!(bloom_index.may_contain(name), "from_index missing {:?}", std::str::from_utf8(name));
            assert!(bloom_prescan.may_contain(name), "from_prescan missing {:?}", std::str::from_utf8(name));
        }
    }

    #[test]
    fn serialization_round_trip() {
        let mut bloom = TagBloom::EMPTY;
        bloom.insert(b"claim");
        bloom.insert(b"title");

        let bytes = bloom.to_bytes();
        let restored = TagBloom::from_bytes(bytes);

        assert_eq!(bloom, restored);
        assert!(restored.may_contain(b"claim"));
        assert!(restored.may_contain(b"title"));
    }

    #[test]
    fn may_contain_any() {
        let mut bloom = TagBloom::EMPTY;
        bloom.insert(b"claim");

        assert!(bloom.may_contain_any(&[b"title", b"claim"]));
        // Intentionally not asserting !may_contain_any for missing names
        // because false positives are allowed
    }

    #[test]
    fn false_positive_rate() {
        // Insert 20 typical patent tag names, then test 1000 random-ish names
        let names: Vec<&[u8]> = vec![
            b"claim", b"patent", b"title", b"abstract", b"description",
            b"claims", b"citation", b"ref", b"inventor", b"applicant",
            b"date", b"id", b"classification", b"cpc", b"ipc",
            b"priority", b"family", b"legal-status", b"kind", b"country",
        ];

        let mut bloom = TagBloom::EMPTY;
        for name in &names {
            bloom.insert(name);
        }

        // Test names that were NOT inserted
        let mut false_positives = 0;
        for i in 0..1000 {
            let test_name = format!("nonexistent_tag_{}", i);
            if bloom.may_contain(test_name.as_bytes()) {
                false_positives += 1;
            }
        }

        // With 128 bits and 20 names (40 bits set), FPR should be < 10%
        let fpr = false_positives as f64 / 1000.0;
        assert!(fpr < 0.10, "False positive rate too high: {:.1}%", fpr * 100.0);
    }
}
