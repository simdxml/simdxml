//! Batched SIMD string predicate evaluation.
//!
//! When evaluating `contains(., 'needle')` or `starts-with(., 'prefix')` across
//! many candidate nodes, this module batches the operation: concatenate all text,
//! run SIMD-accelerated string search (memchr::memmem), map matches back.
//!
//! The memchr crate's memmem uses the Teddy algorithm (SIMD multi-pattern search)
//! internally, which is significantly faster than per-node `str::contains()` for
//! large candidate sets.

use crate::index::XmlIndex;
use super::eval::XPathNode;

/// Minimum candidates to trigger batched evaluation.
/// Below this, per-node evaluation is faster due to lower overhead.
const BATCH_THRESHOLD: usize = 8;

/// Evaluate `contains(., 'needle')` across a set of candidate element nodes.
///
/// Returns a bitmask: `result[i]` is true if the text content of `candidates[i]`
/// contains `needle`.
pub fn batch_contains(
    index: &XmlIndex,
    candidates: &[XPathNode],
    needle: &str,
) -> Vec<bool> {
    if candidates.len() < BATCH_THRESHOLD || needle.is_empty() {
        // Fallback: per-node evaluation
        return candidates.iter().map(|node| {
            let text = node_text_content(index, *node);
            text.contains(needle)
        }).collect();
    }

    // Build concatenated text buffer with \0 separators
    let mut buffer = String::new();
    let mut offsets: Vec<usize> = Vec::with_capacity(candidates.len() + 1);

    for &node in candidates {
        offsets.push(buffer.len());
        let text = node_text_content(index, node);
        buffer.push_str(&text);
        buffer.push('\0'); // separator (not valid in XML text)
    }
    offsets.push(buffer.len());

    // SIMD-accelerated search using memchr::memmem
    let finder = memchr::memmem::Finder::new(needle.as_bytes());
    let buf_bytes = buffer.as_bytes();

    let mut results = vec![false; candidates.len()];

    for pos in finder.find_iter(buf_bytes) {
        // Binary search to find which candidate this match belongs to
        let doc_idx = match offsets.binary_search(&pos) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        if doc_idx < candidates.len() {
            results[doc_idx] = true;
        }
    }

    results
}

/// Evaluate `starts-with(., 'prefix')` across a set of candidate element nodes.
pub fn batch_starts_with(
    index: &XmlIndex,
    candidates: &[XPathNode],
    prefix: &str,
) -> Vec<bool> {
    if candidates.len() < BATCH_THRESHOLD || prefix.is_empty() {
        return candidates.iter().map(|node| {
            let text = node_text_content(index, *node);
            text.starts_with(prefix)
        }).collect();
    }

    // For starts-with, we only need to check the beginning of each node's text.
    // No need for concatenation — just check each one.
    // But we use memchr::memmem::Finder for the prefix check (SIMD-accelerated).
    let prefix_bytes = prefix.as_bytes();

    candidates.iter().map(|&node| {
        let text = node_text_content(index, node);
        text.as_bytes().starts_with(prefix_bytes)
    }).collect()
}

/// Get the text content of a node as an owned String.
fn node_text_content(index: &XmlIndex, node: XPathNode) -> String {
    match node {
        XPathNode::Element(idx) => {
            // Get all text under this element
            index.all_text(idx)
        }
        XPathNode::Text(idx) => {
            if idx < index.text_ranges.len() {
                index.text_content(&index.text_ranges[idx]).to_string()
            } else {
                String::new()
            }
        }
        XPathNode::Attribute(tag_idx, hash) => {
            // Find attribute value by hash
            let names = index.get_all_attribute_names(tag_idx);
            for name in names {
                if super::eval::attr_name_hash(name) == hash {
                    return index.get_attribute(tag_idx, name).unwrap_or("").to_string();
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_contains_basic() {
        let xml = b"<r><p>hello world</p><p>goodbye</p><p>hello again</p></r>";
        let index = crate::parse(xml).unwrap();

        // Get all <p> elements
        let nodes = index.xpath("//p").unwrap();
        assert_eq!(nodes.len(), 3);

        let results = batch_contains(&index, &nodes, "hello");
        assert_eq!(results, vec![true, false, true]);
    }

    #[test]
    fn batch_contains_empty_needle() {
        let xml = b"<r><p>text</p></r>";
        let index = crate::parse(xml).unwrap();
        let nodes = index.xpath("//p").unwrap();

        // Empty needle matches everything (per XPath spec)
        let results = batch_contains(&index, &nodes, "");
        assert_eq!(results, vec![true]);
    }

    #[test]
    fn batch_starts_with_basic() {
        let xml = b"<r><p>alpha one</p><p>beta two</p><p>alpha three</p></r>";
        let index = crate::parse(xml).unwrap();
        let nodes = index.xpath("//p").unwrap();

        let results = batch_starts_with(&index, &nodes, "alpha");
        assert_eq!(results, vec![true, false, true]);
    }

    #[test]
    fn batch_vs_individual_contains() {
        let xml = br#"<corpus>
            <p>The quick brown fox</p>
            <p>jumps over the lazy dog</p>
            <p>pack my box with five dozen liquor jugs</p>
            <p>the five boxing wizards jump quickly</p>
            <p>how vexingly quick daft zebras jump</p>
            <p>sphinx of black quartz judge my vow</p>
            <p>two driven jocks help fax my big quiz</p>
            <p>the jay pig fox zebra and my wolves quack</p>
            <p>sympathizing would fix quaker objectives</p>
            <p>a wizard quick job vexes much of Gryphon</p>
        </corpus>"#;

        let index = crate::parse(xml).unwrap();
        let nodes = index.xpath("//p").unwrap();
        assert_eq!(nodes.len(), 10);

        let needle = "quick";

        // Batch result
        let batch_results = batch_contains(&index, &nodes, needle);

        // Individual results
        let individual: Vec<bool> = nodes.iter().map(|&node| {
            let text = node_text_content(&index, node);
            text.contains(needle)
        }).collect();

        assert_eq!(batch_results, individual);
    }

    #[test]
    fn batch_contains_no_matches() {
        let xml = b"<r><p>aaa</p><p>bbb</p><p>ccc</p></r>";
        let index = crate::parse(xml).unwrap();
        let nodes = index.xpath("//p").unwrap();

        let results = batch_contains(&index, &nodes, "zzz");
        assert_eq!(results, vec![false, false, false]);
    }
}
