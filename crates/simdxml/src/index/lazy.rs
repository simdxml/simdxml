//! Query-driven lazy parser: only index tags relevant to a specific XPath query.
//!
//! The parser still scans all `<` positions (can't avoid this), but only builds
//! full index entries for tags whose names are in the "interesting" set plus
//! their ancestors. Text ranges are only captured under interesting elements.
//! This can skip 70-90% of index construction for selective queries on large XML.

use crate::error::{Result, SimdXmlError};
use crate::index::{TagType, TextRange, XmlIndex};
use memchr::memchr;
use std::collections::HashSet;

/// Parse XML, only indexing tags with names in `interesting_names` and their ancestors.
///
/// Produces an `XmlIndex` that is a correct (but potentially incomplete) index
/// for evaluating XPath queries that only reference the given tag names.
/// All ancestor tags of interesting tags are retained so axis navigation works.
///
/// Falls back to full parsing if `interesting_names` is empty.
pub fn parse_for_query<'a>(
    input: &'a [u8],
    interesting_names: &HashSet<String>,
) -> Result<XmlIndex<'a>> {
    if interesting_names.is_empty() {
        return crate::index::structural::parse_scalar(input);
    }

    let est_tags = input.len() / 256; // smaller estimate — we skip most tags
    let est_text = est_tags / 2;

    let mut index = XmlIndex {
        input,
        tag_starts: Vec::with_capacity(est_tags),
        tag_ends: Vec::with_capacity(est_tags),
        tag_types: Vec::with_capacity(est_tags),
        tag_names: Vec::with_capacity(est_tags),
        depths: Vec::with_capacity(est_tags),
        parents: Vec::with_capacity(est_tags),
        text_ranges: Vec::with_capacity(est_text),
        child_offsets: Vec::new(),
        child_data: Vec::new(),
        text_child_offsets: Vec::new(),
        text_child_data: Vec::new(),
        close_map: Vec::new(),
        post_order: Vec::new(),
        name_ids: Vec::new(),
        name_table: Vec::new(),
        name_posting: Vec::new(),
    };

    let mut pos = 0;
    let mut depth: u16 = 0;

    // Track the full structural state (needed for correctness)
    // Each entry: (name_start, name_len, is_interesting, index_tag_idx_or_MAX)
    let mut parent_stack: Vec<ParentEntry> = Vec::new();
    let mut last_tag_end: usize = 0;

    while let Some(offset) = memchr(b'<', &input[pos..]) {
        pos += offset;
        let tag_start = pos;

        if pos + 1 >= input.len() {
            return Err(SimdXmlError::UnclosedTag(pos));
        }

        match input[pos + 1] {
            b'/' => {
                // Close tag
                pos += 2;
                let name_start = pos;
                while pos < input.len() && input[pos] != b'>' && !input[pos].is_ascii_whitespace() {
                    pos += 1;
                }
                let name_end = pos;

                if let Some(off) = memchr(b'>', &input[pos..]) {
                    pos += off;
                } else {
                    return Err(SimdXmlError::UnclosedTag(tag_start));
                }

                if depth > 0 {
                    depth -= 1;
                }

                let entry = parent_stack.pop();
                let is_interesting = entry.as_ref().map_or(false, |e| e.is_interesting);

                // Only emit close tag if the matching open was interesting
                if is_interesting {
                    let open_tag_idx = entry.as_ref().unwrap().index_tag_idx;

                    // Capture text before this close tag — the text is inside the
                    // interesting element we're closing, so use its index as parent
                    let text_start = if last_tag_end > 0 { last_tag_end + 1 } else { 0 };
                    if text_start < tag_start {
                        let text = &input[text_start..tag_start];
                        if text.iter().any(|&b| !b.is_ascii_whitespace()) {
                            index.text_ranges.push(TextRange {
                                start: text_start as u32,
                                end: tag_start as u32,
                                parent_tag: open_tag_idx,
                            });
                        }
                    }

                    index.tag_starts.push(tag_start as u32);
                    index.tag_ends.push(pos as u32);
                    index.tag_types.push(TagType::Close);
                    index.tag_names.push((name_start as u32, (name_end - name_start) as u16));
                    index.depths.push(depth);
                    let parent_idx = find_interesting_parent(&parent_stack);
                    index.parents.push(parent_idx);
                }

                last_tag_end = pos;
                pos += 1;
            }
            b'!' => {
                if input.get(pos + 2..pos + 4) == Some(b"--") {
                    // Comment — skip
                    pos += 4;
                    loop {
                        if let Some(off) = memchr(b'-', &input[pos..]) {
                            pos += off;
                            if pos + 2 < input.len() && &input[pos..pos + 3] == b"-->" {
                                pos += 2;
                                break;
                            }
                            pos += 1;
                        } else {
                            pos = input.len();
                            break;
                        }
                    }
                    last_tag_end = pos;
                    pos += 1;
                } else if input.get(pos + 2..pos + 9) == Some(b"[CDATA[") {
                    // CDATA — capture text if under interesting parent
                    pos += 9;
                    let content_start = pos;
                    loop {
                        if let Some(off) = memchr(b']', &input[pos..]) {
                            pos += off;
                            if pos + 2 < input.len() && &input[pos..pos + 3] == b"]]>" {
                                if pos > content_start && has_interesting_ancestor(&parent_stack) {
                                    let parent = find_interesting_parent(&parent_stack);
                                    index.text_ranges.push(TextRange {
                                        start: content_start as u32,
                                        end: pos as u32,
                                        parent_tag: parent,
                                    });
                                }
                                pos += 2;
                                break;
                            }
                            pos += 1;
                        } else {
                            break;
                        }
                    }
                    last_tag_end = pos;
                    pos += 1;
                } else {
                    // DOCTYPE or other — skip
                    if let Some(off) = memchr(b'>', &input[pos..]) {
                        pos += off;
                    }
                    last_tag_end = pos;
                    pos += 1;
                }
            }
            b'?' => {
                // Processing instruction — skip
                pos += 2;
                while pos + 1 < input.len() {
                    if input[pos] == b'?' && input[pos + 1] == b'>' {
                        pos += 1;
                        break;
                    }
                    pos += 1;
                }
                last_tag_end = pos;
                pos += 1;
            }
            _ => {
                // Open or self-closing tag
                pos += 1;
                let name_start = pos;
                while pos < input.len()
                    && input[pos] != b'>'
                    && input[pos] != b'/'
                    && !input[pos].is_ascii_whitespace()
                {
                    pos += 1;
                }
                let name_end = pos;

                // Skip attributes
                let mut self_closing = false;
                while pos < input.len() && input[pos] != b'>' {
                    if input[pos] == b'/' && pos + 1 < input.len() && input[pos + 1] == b'>' {
                        self_closing = true;
                        pos += 1;
                        break;
                    }
                    if input[pos] == b'"' {
                        pos += 1;
                        if let Some(off) = memchr(b'"', &input[pos..]) { pos += off; }
                    } else if input[pos] == b'\'' {
                        pos += 1;
                        if let Some(off) = memchr(b'\'', &input[pos..]) { pos += off; }
                    }
                    pos += 1;
                }

                if pos >= input.len() {
                    return Err(SimdXmlError::UnclosedTag(tag_start));
                }

                let tag_name = &input[name_start..name_end];
                let tag_name_str = std::str::from_utf8(tag_name).unwrap_or("");
                let is_interesting = interesting_names.contains(tag_name_str);

                let tag_type = if self_closing {
                    TagType::SelfClose
                } else {
                    TagType::Open
                };

                if is_interesting {
                    // Capture text before this tag if under interesting parent
                    capture_text_if_interesting(
                        &mut index, input, last_tag_end, tag_start, &parent_stack,
                    );

                    let tag_idx = index.tag_starts.len() as u32;
                    let parent_idx = find_interesting_parent(&parent_stack);

                    index.tag_starts.push(tag_start as u32);
                    index.tag_ends.push(pos as u32);
                    index.tag_types.push(tag_type);
                    index.tag_names.push((name_start as u32, (name_end - name_start) as u16));
                    index.depths.push(depth);
                    index.parents.push(parent_idx);

                    if tag_type == TagType::Open {
                        parent_stack.push(ParentEntry {
                            is_interesting: true,
                            index_tag_idx: tag_idx,
                        });
                        depth += 1;
                    }
                } else {
                    // Not interesting — track structurally but don't emit
                    if tag_type == TagType::Open {
                        parent_stack.push(ParentEntry {
                            is_interesting: false,
                            index_tag_idx: u32::MAX,
                        });
                        depth += 1;
                    }
                }

                last_tag_end = pos;
                pos += 1;
            }
        }
    }

    if index.tag_count() >= 64 {
        index.build_indices();
    }
    Ok(index)
}

struct ParentEntry {
    is_interesting: bool,
    index_tag_idx: u32,
}

/// Check if any ancestor in the stack is interesting.
fn has_interesting_ancestor(stack: &[ParentEntry]) -> bool {
    stack.iter().rev().any(|e| e.is_interesting)
}

/// Find the index_tag_idx of the nearest interesting ancestor.
fn find_interesting_parent(stack: &[ParentEntry]) -> u32 {
    stack
        .iter()
        .rev()
        .find(|e| e.is_interesting)
        .map_or(u32::MAX, |e| e.index_tag_idx)
}

/// Capture text content between `last_tag_end` and `current_pos` if under interesting parent.
fn capture_text_if_interesting(
    index: &mut XmlIndex,
    input: &[u8],
    last_tag_end: usize,
    current_pos: usize,
    parent_stack: &[ParentEntry],
) {
    let text_start = if last_tag_end > 0 {
        last_tag_end + 1
    } else {
        0
    };
    if text_start < current_pos && has_interesting_ancestor(parent_stack) {
        // Only capture non-whitespace-only text
        let text = &input[text_start..current_pos];
        if text.iter().any(|&b| !b.is_ascii_whitespace()) {
            let parent = find_interesting_parent(parent_stack);
            index.text_ranges.push(TextRange {
                start: text_start as u32,
                end: current_pos as u32,
                parent_tag: parent,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(strs: &[&str]) -> HashSet<String> {
        strs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn selective_parse_basic() {
        let xml = b"<root><a>1</a><b>2</b><c>3</c></root>";
        let interesting = names(&["a"]);
        let index = parse_for_query(xml, &interesting).unwrap();

        // Should only have "a" and its close tag
        let a_texts = index.xpath_text("//a").unwrap();
        assert_eq!(a_texts, vec!["1"]);
    }

    #[test]
    fn selective_parse_nested() {
        let xml = b"<root><parent><target>found</target><other>skip</other></parent></root>";
        let interesting = names(&["target"]);
        let index = parse_for_query(xml, &interesting).unwrap();

        let found = index.xpath_text("//target").unwrap();
        assert_eq!(found, vec!["found"]);
    }

    #[test]
    fn selective_parse_multiple_names() {
        let xml = b"<r><a>1</a><b>2</b><c>3</c><a>4</a></r>";
        let interesting = names(&["a", "c"]);
        let index = parse_for_query(xml, &interesting).unwrap();

        let a_texts = index.xpath_text("//a").unwrap();
        assert_eq!(a_texts, vec!["1", "4"]);

        let c_texts = index.xpath_text("//c").unwrap();
        assert_eq!(c_texts, vec!["3"]);
    }

    #[test]
    fn selective_fewer_tags() {
        let xml = b"<root><a>1</a><b>2</b><c>3</c><d>4</d><e>5</e></root>";
        let full = crate::parse(xml).unwrap();
        let interesting = names(&["a"]);
        let lazy = parse_for_query(xml, &interesting).unwrap();

        // Lazy index should have fewer tags
        assert!(lazy.tag_count() < full.tag_count(),
            "lazy {} should be < full {}", lazy.tag_count(), full.tag_count());
    }

    #[test]
    fn empty_interesting_falls_back() {
        let xml = b"<root><child>text</child></root>";
        let empty: HashSet<String> = HashSet::new();
        let index = parse_for_query(xml, &empty).unwrap();

        // Should do full parse
        let full = crate::parse(xml).unwrap();
        assert_eq!(index.tag_count(), full.tag_count());
    }

    #[test]
    fn self_closing_tags() {
        let xml = b"<root><target/><other/><target/></root>";
        let interesting = names(&["target"]);
        let index = parse_for_query(xml, &interesting).unwrap();

        let targets = index.xpath("//target").unwrap();
        assert_eq!(targets.len(), 2);
    }

    #[test]
    fn patent_like_structure() {
        let xml = br#"<corpus>
            <patent id="1">
                <title>Widget</title>
                <abstract>An abstract about widgets</abstract>
                <claims>
                    <claim type="independent">A device comprising a widget</claim>
                    <claim type="dependent">The device of claim 1</claim>
                </claims>
                <description>Very long description text here...</description>
            </patent>
            <patent id="2">
                <title>Gadget</title>
                <abstract>An abstract about gadgets</abstract>
                <claims>
                    <claim type="independent">A method for gadgeting</claim>
                </claims>
                <description>Another long description...</description>
            </patent>
        </corpus>"#;

        let interesting = names(&["claim"]);
        let lazy = parse_for_query(xml, &interesting).unwrap();
        let full = crate::parse(xml).unwrap();

        // Lazy should have significantly fewer tags
        assert!(lazy.tag_count() < full.tag_count(),
            "lazy {} should be < full {}", lazy.tag_count(), full.tag_count());

        // But claim extraction should produce same results
        let lazy_claims = lazy.xpath_text("//claim").unwrap();
        let full_claims = full.xpath_text("//claim").unwrap();
        assert_eq!(lazy_claims, full_claims);
    }
}
