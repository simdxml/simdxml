use crate::error::{Result, SimdXmlError};
use crate::index::{TagType, TextRange, XmlIndex};
use memchr::memchr;

/// Build an XmlIndex from XML bytes.
/// Uses SIMD-accelerated byte scanning (via memchr) for finding structural
/// characters, with sequential processing for tag classification and index building.
/// Phase 2 replaces this with SIMD for the structural character detection.
pub fn parse_scalar<'a>(input: &'a [u8]) -> Result<XmlIndex<'a>> {
    // Estimate tag count: ~1 tag per 100-200 bytes for typical XML.
    // Pre-allocating avoids repeated Vec reallocation during parsing.
    let est_tags = input.len() / 128;
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
    let mut parent_stack: Vec<u32> = Vec::new(); // stack of open tag indices
    let mut last_tag_end: usize = 0; // for tracking text content

    // Main loop: use SIMD-accelerated memchr to find '<' positions
    while let Some(offset) = memchr(b'<', &input[pos..]) {
        pos += offset;

        {
            // Text content between previous tag end and this tag start
            let text_start = if last_tag_end > 0 {
                last_tag_end + 1
            } else {
                0
            };
            if text_start < pos {
                let parent = parent_stack.last().copied().unwrap_or(u32::MAX);
                index.text_ranges.push(TextRange {
                    start: text_start as u32,
                    end: pos as u32,
                    parent_tag: parent,
                });
            }
        }

        let tag_start = pos;

        if pos + 1 >= input.len() {
            return Err(SimdXmlError::UnclosedTag(pos));
        }

        match input[pos + 1] {
                b'/' => {
                    // Close tag: </name>
                    pos += 2;
                    let name_start = pos;
                    while pos < input.len() && input[pos] != b'>' && !input[pos].is_ascii_whitespace() {
                        pos += 1;
                    }
                    let name_end = pos;

                    // Skip to > (SIMD-accelerated)
                    if let Some(off) = memchr(b'>', &input[pos..]) {
                        pos += off;
                    } else {
                        return Err(SimdXmlError::UnclosedTag(tag_start));
                    }

                    if depth > 0 {
                        depth -= 1;
                    }
                    parent_stack.pop();


                    index.tag_starts.push(tag_start as u32);
                    index.tag_ends.push(pos as u32);
                    index.tag_types.push(TagType::Close);
                    index.tag_names.push((name_start as u32, (name_end - name_start) as u16));
                    index.depths.push(depth);
                    index.parents.push(parent_stack.last().copied().unwrap_or(u32::MAX));

                    last_tag_end = pos;
                    pos += 1;
                }
                b'!' => {
                    if input.get(pos + 2..pos + 4) == Some(b"--") {
                        // Comment: <!-- ... -->
                        index.tag_starts.push(tag_start as u32);
                        index.tag_types.push(TagType::Comment);
                        index.tag_names.push((0, 0));
                        index.depths.push(depth);
                        index.parents.push(parent_stack.last().copied().unwrap_or(u32::MAX));

                        pos += 4;
                        // SIMD-accelerated: find '-' then check for '-->'
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
                        index.tag_ends.push(pos as u32);
                        last_tag_end = pos;
                        pos += 1;
                    } else if input.get(pos + 2..pos + 9) == Some(b"[CDATA[") {
                        // CDATA: <![CDATA[ ... ]]>
                        index.tag_starts.push(tag_start as u32);
                        index.tag_types.push(TagType::CData);
                        index.tag_names.push((0, 0));
                        index.depths.push(depth);
                        index.parents.push(parent_stack.last().copied().unwrap_or(u32::MAX));

                        pos += 9;
                        let content_start = pos;
                        // SIMD-accelerated: find ']' then check for ']]>'
                        loop {
                            if let Some(off) = memchr(b']', &input[pos..]) {
                                pos += off;
                                if pos + 2 < input.len() && &input[pos..pos + 3] == b"]]>" {
                                    let parent = parent_stack.last().copied().unwrap_or(u32::MAX);
                                    if pos > content_start {
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
                        index.tag_ends.push(pos as u32);
                        last_tag_end = pos;
                        pos += 1;
                    } else {
                        // DOCTYPE or other <!...> — skip (SIMD-accelerated)
                        if let Some(off) = memchr(b'>', &input[pos..]) {
                            pos += off;
                        }
                        last_tag_end = pos;
                        pos += 1;
                    }
                }
                b'?' => {
                    // Processing instruction: <?target ... ?>
                    let _tag_idx = index.tag_starts.len();
                    pos += 2;
                    let name_start = pos;
                    while pos < input.len()
                        && input[pos] != b'?'
                        && input[pos] != b'>'
                        && !input[pos].is_ascii_whitespace()
                    {
                        pos += 1;
                    }
                    let name_end = pos;


                    index.tag_starts.push(tag_start as u32);
                    index.tag_types.push(TagType::PI);
                    index.tag_names.push((name_start as u32, (name_end - name_start) as u16));
                    index.depths.push(depth);
                    index.parents.push(parent_stack.last().copied().unwrap_or(u32::MAX));

                    // Skip to ?>
                    while pos + 1 < input.len() {
                        if input[pos] == b'?' && input[pos + 1] == b'>' {
                            pos += 1;
                            break;
                        }
                        pos += 1;
                    }
                    index.tag_ends.push(pos as u32);
                    last_tag_end = pos;
                    pos += 1;
                }
                _ => {
                    // Open tag or self-closing tag: <name ...> or <name .../>
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

                    // Skip attributes to find > or />
                    let mut self_closing = false;
                    while pos < input.len() && input[pos] != b'>' {
                        if input[pos] == b'/' && pos + 1 < input.len() && input[pos + 1] == b'>' {
                            self_closing = true;
                            pos += 1;
                            break;
                        }
                        // Skip quoted attribute values (memchr for closing quote)
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

                    let tag_type = if self_closing {
                        TagType::SelfClose
                    } else {
                        TagType::Open
                    };

                    let tag_idx = index.tag_starts.len() as u32;
                    let parent = parent_stack.last().copied().unwrap_or(u32::MAX);


                    index.tag_starts.push(tag_start as u32);
                    index.tag_ends.push(pos as u32);
                    index.tag_types.push(tag_type);
                    index.tag_names.push((name_start as u32, (name_end - name_start) as u16));
                    index.depths.push(depth);
                    index.parents.push(parent);

                    if tag_type == TagType::Open {
                        parent_stack.push(tag_idx);
                        depth += 1;
                    }

                    last_tag_end = pos;
                    pos += 1;
                }
            }
        }

    // Only build CSR indices for documents large enough to benefit.
    // Small docs (< 64 tags) are faster with direct linear scans.
    if index.tag_count() >= 64 {
        index.build_indices();
    }
    Ok(index)
}

/// Two-stage SIMD parser: Stage 1 classifies all bytes with NEON,
/// Stage 2 walks the bitmasks to build the structural index.
pub fn parse_two_stage<'a>(input: &'a [u8]) -> Result<XmlIndex<'a>> {
    let structural = crate::simd::classify_structural(input);
    let est_tags = input.len() / 128;
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

    let mut depth: u16 = 0;
    let mut parent_stack: Vec<u32> = Vec::new();
    let mut last_tag_end: usize = 0;

    // Pre-collect gt positions for fast lookup of matching '>'
    let gt_positions: Vec<usize> = structural.gt_positions().collect();
    let mut gt_idx = 0;

    // Stage 2: walk '<' positions from Stage 1
    for lt_pos in structural.lt_positions() {
        // Text content between previous tag end and this '<'
        let text_start = if last_tag_end > 0 { last_tag_end + 1 } else { 0 };
        if text_start < lt_pos {
            let parent = parent_stack.last().copied().unwrap_or(u32::MAX);
            index.text_ranges.push(TextRange {
                start: text_start as u32,
                end: lt_pos as u32,
                parent_tag: parent,
            });
        }

        let tag_start = lt_pos;
        if tag_start + 1 >= input.len() { break; }

        // Find the matching '>' for this '<'
        while gt_idx < gt_positions.len() && gt_positions[gt_idx] <= lt_pos {
            gt_idx += 1;
        }
        let gt_pos = if gt_idx < gt_positions.len() {
            gt_positions[gt_idx]
        } else {
            return Err(SimdXmlError::UnclosedTag(tag_start));
        };

        match input[tag_start + 1] {
            b'/' => {
                // Close tag
                let name_start = tag_start + 2;
                let mut name_end = name_start;
                while name_end < gt_pos && !input[name_end].is_ascii_whitespace() {
                    name_end += 1;
                }

                if depth > 0 { depth -= 1; }
                parent_stack.pop();

                index.tag_starts.push(tag_start as u32);
                index.tag_ends.push(gt_pos as u32);
                index.tag_types.push(TagType::Close);
                index.tag_names.push((name_start as u32, (name_end - name_start) as u16));
                index.depths.push(depth);
                index.parents.push(parent_stack.last().copied().unwrap_or(u32::MAX));

                last_tag_end = gt_pos;
            }
            b'!' => {
                if input.get(tag_start + 2..tag_start + 4) == Some(b"--") {
                    // Comment: <!-- ... -->
                    // Find --> (the gt_pos might be inside the comment)
                    let mut end = tag_start + 4;
                    while end + 2 < input.len() {
                        if &input[end..end + 3] == b"-->" {
                            end += 2;
                            break;
                        }
                        end += 1;
                    }

                    index.tag_starts.push(tag_start as u32);
                    index.tag_ends.push(end as u32);
                    index.tag_types.push(TagType::Comment);
                    index.tag_names.push((0, 0));
                    index.depths.push(depth);
                    index.parents.push(parent_stack.last().copied().unwrap_or(u32::MAX));

                    last_tag_end = end;
                    // Advance gt_idx past comment end
                    while gt_idx < gt_positions.len() && gt_positions[gt_idx] <= end {
                        gt_idx += 1;
                    }
                } else if input.get(tag_start + 2..tag_start + 9) == Some(b"[CDATA[") {
                    // CDATA
                    let content_start = tag_start + 9;
                    let mut end = content_start;
                    while end + 2 < input.len() {
                        if &input[end..end + 3] == b"]]>" {
                            let parent = parent_stack.last().copied().unwrap_or(u32::MAX);
                            if end > content_start {
                                index.text_ranges.push(TextRange {
                                    start: content_start as u32,
                                    end: end as u32,
                                    parent_tag: parent,
                                });
                            }
                            end += 2;
                            break;
                        }
                        end += 1;
                    }

                    index.tag_starts.push(tag_start as u32);
                    index.tag_ends.push(end as u32);
                    index.tag_types.push(TagType::CData);
                    index.tag_names.push((0, 0));
                    index.depths.push(depth);
                    index.parents.push(parent_stack.last().copied().unwrap_or(u32::MAX));

                    last_tag_end = end;
                    while gt_idx < gt_positions.len() && gt_positions[gt_idx] <= end {
                        gt_idx += 1;
                    }
                } else {
                    // DOCTYPE — skip
                    last_tag_end = gt_pos;
                }
            }
            b'?' => {
                // Processing instruction
                let name_start = tag_start + 2;
                let mut name_end = name_start;
                while name_end < input.len()
                    && input[name_end] != b'?'
                    && input[name_end] != b'>'
                    && !input[name_end].is_ascii_whitespace()
                {
                    name_end += 1;
                }

                // Find ?>
                let mut end = name_end;
                while end + 1 < input.len() {
                    if input[end] == b'?' && input[end + 1] == b'>' {
                        end += 1;
                        break;
                    }
                    end += 1;
                }

                index.tag_starts.push(tag_start as u32);
                index.tag_ends.push(end as u32);
                index.tag_types.push(TagType::PI);
                index.tag_names.push((name_start as u32, (name_end - name_start) as u16));
                index.depths.push(depth);
                index.parents.push(parent_stack.last().copied().unwrap_or(u32::MAX));

                last_tag_end = end;
                while gt_idx < gt_positions.len() && gt_positions[gt_idx] <= end {
                    gt_idx += 1;
                }
            }
            _ => {
                // Open or self-closing tag
                let name_start = tag_start + 1;
                let mut name_end = name_start;
                while name_end < gt_pos
                    && input[name_end] != b'>'
                    && input[name_end] != b'/'
                    && !input[name_end].is_ascii_whitespace()
                {
                    name_end += 1;
                }

                // Check for self-closing: look for /> before >
                let self_closing = gt_pos > 0 && input[gt_pos - 1] == b'/';
                let tag_type = if self_closing { TagType::SelfClose } else { TagType::Open };

                let tag_idx = index.tag_starts.len() as u32;
                let parent = parent_stack.last().copied().unwrap_or(u32::MAX);

                index.tag_starts.push(tag_start as u32);
                index.tag_ends.push(gt_pos as u32);
                index.tag_types.push(tag_type);
                index.tag_names.push((name_start as u32, (name_end - name_start) as u16));
                index.depths.push(depth);
                index.parents.push(parent);

                if tag_type == TagType::Open {
                    parent_stack.push(tag_idx);
                    depth += 1;
                }

                last_tag_end = gt_pos;
            }
        }
    }

    if index.tag_count() >= 64 {
        index.build_indices();
    }
    Ok(index)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_element() {
        let xml = b"<root>hello</root>";
        let index = parse_scalar(xml).unwrap();
        assert_eq!(index.tag_count(), 2); // open + close
        assert_eq!(index.tag_name(0), "root");
        assert_eq!(index.tag_types[0], TagType::Open);
        assert_eq!(index.tag_types[1], TagType::Close);
        assert_eq!(index.depths[0], 0);
    }

    #[test]
    fn test_nested() {
        let xml = b"<root><child>text</child></root>";
        let index = parse_scalar(xml).unwrap();
        assert_eq!(index.tag_count(), 4);
        assert_eq!(index.tag_name(0), "root");
        assert_eq!(index.tag_name(1), "child");
        assert_eq!(index.depths[0], 0); // root
        assert_eq!(index.depths[1], 1); // child
        assert_eq!(index.parents[1], 0); // child's parent is root
    }

    #[test]
    fn test_self_closing() {
        let xml = b"<root><br/></root>";
        let index = parse_scalar(xml).unwrap();
        assert_eq!(index.tag_count(), 3);
        assert_eq!(index.tag_types[1], TagType::SelfClose);
        assert_eq!(index.tag_name(1), "br");
    }

    #[test]
    fn test_comment() {
        let xml = b"<root><!-- comment --><child/></root>";
        let index = parse_scalar(xml).unwrap();
        assert!(index
            .tag_types
            .iter()
            .any(|t| *t == TagType::Comment));
    }

    #[test]
    fn test_cdata() {
        let xml = b"<root><![CDATA[hello <world>]]></root>";
        let index = parse_scalar(xml).unwrap();
        assert!(index.tag_types.iter().any(|t| *t == TagType::CData));
    }

    #[test]
    fn test_processing_instruction() {
        let xml = b"<?xml version=\"1.0\"?><root/>";
        let index = parse_scalar(xml).unwrap();
        assert!(index.tag_types.iter().any(|t| *t == TagType::PI));
    }

    #[test]
    fn test_text_content() {
        let xml = b"<root>hello world</root>";
        let index = parse_scalar(xml).unwrap();
        assert_eq!(index.text_ranges.len(), 1);
        assert_eq!(index.text_content(&index.text_ranges[0]), "hello world");
    }

    #[test]
    fn test_attributes() {
        let xml = b"<root attr=\"value\">text</root>";
        let index = parse_scalar(xml).unwrap();
        assert_eq!(index.tag_count(), 2);
        assert_eq!(index.tag_name(0), "root");
    }

    #[test]
    fn test_multiple_children() {
        let xml = b"<root><a>1</a><b>2</b><c>3</c></root>";
        let index = parse_scalar(xml).unwrap();
        let children = index.children(0);
        assert_eq!(children.len(), 3);
    }

    #[test]
    fn test_deep_nesting() {
        let xml = b"<a><b><c><d>deep</d></c></b></a>";
        let index = parse_scalar(xml).unwrap();
        assert_eq!(index.depths[3], 3); // d is at depth 3
        assert_eq!(index.tag_name(3), "d");
    }

    #[test]
    fn test_all_text() {
        let xml = b"<root>hello <b>bold</b> world</root>";
        let index = parse_scalar(xml).unwrap();
        let text = index.all_text(0);
        assert!(text.contains("hello"));
        assert!(text.contains("bold"));
        assert!(text.contains("world"));
    }

    #[test]
    fn test_matching_close() {
        let xml = b"<root><a>text</a></root>";
        let index = parse_scalar(xml).unwrap();
        let close = index.matching_close(0).unwrap();
        assert_eq!(index.tag_name(close), "root");
        assert_eq!(index.tag_types[close], TagType::Close);
    }
}
