use crate::error::{Result, SimdXmlError};
use crate::index::{TagType, TextRange, XmlIndex};

/// Build an XmlIndex from XML bytes using the scalar (non-SIMD) parser.
/// This is the reference implementation — correct but not fast.
/// Phase 2 replaces this with SIMD for the structural character detection.
pub fn parse_scalar<'a>(input: &'a [u8]) -> Result<XmlIndex<'a>> {
    let mut index = XmlIndex {
        input,
        tag_starts: Vec::new(),
        tag_ends: Vec::new(),
        tag_types: Vec::new(),
        tag_names: Vec::new(),
        depths: Vec::new(),
        parents: Vec::new(),
        text_ranges: Vec::new(),
    };

    let mut pos = 0;
    let mut depth: u16 = 0;
    let mut parent_stack: Vec<u32> = Vec::new(); // stack of open tag indices
    let mut last_tag_end: usize = 0; // for tracking text content

    while pos < input.len() {
        if input[pos] == b'<' {
            // Text content between previous tag end and this tag start
            let text_start = if last_tag_end > 0 {
                last_tag_end + 1
            } else {
                0
            };
            if text_start < pos {
                // Include ALL text nodes, even whitespace-only.
                // XPath node() requires whitespace text nodes.
                {
                    let parent = parent_stack.last().copied().unwrap_or(u32::MAX);
                    index.text_ranges.push(TextRange {
                        start: text_start as u32,
                        end: pos as u32,
                        parent_tag: parent,
                    });
                }
            }

            let tag_start = pos;

            // Determine tag type
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

                    // Skip to >
                    while pos < input.len() && input[pos] != b'>' {
                        pos += 1;
                    }
                    if pos >= input.len() {
                        return Err(SimdXmlError::UnclosedTag(tag_start));
                    }

                    if depth > 0 {
                        depth -= 1;
                    }
                    parent_stack.pop();

                    let tag_idx = index.tag_starts.len();
                    index.tag_starts.push(tag_start as u32);
                    index.tag_ends.push(pos as u32);
                    index.tag_types.push(TagType::Close);
                    index.tag_names.push((name_start as u32, (name_end - name_start) as u16));
                    index.depths.push(depth);
                    index
                        .parents
                        .push(parent_stack.last().copied().unwrap_or(u32::MAX));

                    last_tag_end = pos;
                    pos += 1;
                }
                b'!' => {
                    if input.get(pos + 2..pos + 4) == Some(b"--") {
                        // Comment: <!-- ... -->
                        let tag_idx = index.tag_starts.len();
                        index.tag_starts.push(tag_start as u32);
                        index.tag_types.push(TagType::Comment);
                        index.tag_names.push((0, 0));
                        index.depths.push(depth);
                        index
                            .parents
                            .push(parent_stack.last().copied().unwrap_or(u32::MAX));

                        pos += 4;
                        while pos + 2 < input.len() {
                            if &input[pos..pos + 3] == b"-->" {
                                pos += 2;
                                break;
                            }
                            pos += 1;
                        }
                        index.tag_ends.push(pos as u32);
                        last_tag_end = pos;
                        pos += 1;
                    } else if input.get(pos + 2..pos + 9) == Some(b"[CDATA[") {
                        // CDATA: <![CDATA[ ... ]]>
                        let tag_idx = index.tag_starts.len();
                        let cdata_content_start = pos + 9;
                        index.tag_starts.push(tag_start as u32);
                        index.tag_types.push(TagType::CData);
                        index.tag_names.push((0, 0));
                        index.depths.push(depth);
                        index
                            .parents
                            .push(parent_stack.last().copied().unwrap_or(u32::MAX));

                        pos += 9;
                        let content_start = pos;
                        while pos + 2 < input.len() {
                            if &input[pos..pos + 3] == b"]]>" {
                                // Record CDATA content as text
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
                        }
                        index.tag_ends.push(pos as u32);
                        last_tag_end = pos;
                        pos += 1;
                    } else {
                        // DOCTYPE or other <!...> — skip
                        while pos < input.len() && input[pos] != b'>' {
                            pos += 1;
                        }
                        last_tag_end = pos;
                        pos += 1;
                    }
                }
                b'?' => {
                    // Processing instruction: <?target ... ?>
                    let tag_idx = index.tag_starts.len();
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
                    index
                        .tag_names
                        .push((name_start as u32, (name_end - name_start) as u16));
                    index.depths.push(depth);
                    index
                        .parents
                        .push(parent_stack.last().copied().unwrap_or(u32::MAX));

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
                            pos += 1; // skip /, will hit > next
                            break;
                        }
                        // Skip quoted attribute values
                        if input[pos] == b'"' {
                            pos += 1;
                            while pos < input.len() && input[pos] != b'"' {
                                pos += 1;
                            }
                        } else if input[pos] == b'\'' {
                            pos += 1;
                            while pos < input.len() && input[pos] != b'\'' {
                                pos += 1;
                            }
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
                    index
                        .tag_names
                        .push((name_start as u32, (name_end - name_start) as u16));
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
        } else {
            pos += 1;
        }
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
