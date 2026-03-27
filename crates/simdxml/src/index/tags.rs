// Tag-level utilities — attribute parsing, namespace resolution.
// Will be expanded in later phases.

use crate::index::XmlIndex;

impl<'a> XmlIndex<'a> {
    /// Extract an attribute value from a tag by attribute name.
    /// Zero-allocation: searches raw bytes directly with memchr.
    #[inline]
    pub fn get_attribute(&self, tag_idx: usize, attr_name: &str) -> Option<&'a str> {
        if tag_idx >= self.tag_count() { return None; }
        let start = self.tag_starts[tag_idx] as usize;
        let end = self.tag_ends[tag_idx] as usize;
        let tag_bytes = &self.input[start..=end];
        let name = attr_name.as_bytes();
        if name.is_empty() { return None; }
        let first = name[0];

        // SIMD-accelerated: use memchr to jump to first byte of attr name
        let mut pos = 0;
        while let Some(off) = memchr::memchr(first, &tag_bytes[pos..]) {
            pos += off;
            if pos + name.len() + 1 >= tag_bytes.len() { break; }

            // Check full name match + '=' suffix + whitespace boundary
            if &tag_bytes[pos..pos + name.len()] == name
                && tag_bytes[pos + name.len()] == b'='
                && (pos == 0 || tag_bytes[pos - 1].is_ascii_whitespace())
            {
                let val_start = pos + name.len() + 1;
                let quote = tag_bytes[val_start];
                if quote == b'"' || quote == b'\'' {
                    let content_start = val_start + 1;
                    if let Some(qoff) = memchr::memchr(quote, &tag_bytes[content_start..]) {
                        let abs_start = start + content_start;
                        return Some(unsafe {
                            std::str::from_utf8_unchecked(&self.input[abs_start..abs_start + qoff])
                        });
                    }
                }
            }
            pos += 1;
        }
        None
    }

    /// Get all attribute names on a tag. Zero-allocation scan of raw bytes.
    pub fn get_all_attribute_names(&self, tag_idx: usize) -> Vec<&'a str> {
        if tag_idx >= self.tag_count() { return Vec::new(); }
        let start = self.tag_starts[tag_idx] as usize;
        let end = self.tag_ends[tag_idx] as usize;
        let tag_bytes = &self.input[start..=end];
        let mut result = Vec::new();

        // Skip past tag name
        let mut pos = 1; // skip '<'
        while pos < tag_bytes.len()
            && tag_bytes[pos] != b'>'
            && tag_bytes[pos] != b'/'
            && !tag_bytes[pos].is_ascii_whitespace()
        {
            pos += 1;
        }

        // Scan for name=value patterns
        while pos < tag_bytes.len() && tag_bytes[pos] != b'>' {
            if tag_bytes[pos] == b'/' { break; }
            if tag_bytes[pos].is_ascii_whitespace() { pos += 1; continue; }

            let attr_name_start = pos;
            while pos < tag_bytes.len()
                && tag_bytes[pos] != b'='
                && tag_bytes[pos] != b'>'
                && !tag_bytes[pos].is_ascii_whitespace()
            {
                pos += 1;
            }
            let attr_name_end = pos;
            if pos < tag_bytes.len() && tag_bytes[pos] == b'=' {
                pos += 1;
                if pos < tag_bytes.len() && (tag_bytes[pos] == b'"' || tag_bytes[pos] == b'\'') {
                    let quote = tag_bytes[pos];
                    pos += 1;
                    if let Some(off) = memchr::memchr(quote, &tag_bytes[pos..]) {
                        pos += off + 1;
                    }
                    if attr_name_end > attr_name_start {
                        let abs_start = start + attr_name_start;
                        let abs_end = start + attr_name_end;
                        if let Ok(name) = std::str::from_utf8(&self.input[abs_start..abs_end]) {
                            if !name.starts_with("xmlns") {
                                result.push(name);
                            }
                        }
                    }
                } else {
                    pos += 1;
                }
            } else {
                pos += 1;
            }
        }
        result
    }

    /// Get all attributes as (name, value) pairs in a single pass.
    /// More efficient than calling get_all_attribute_names + get_attribute separately.
    pub fn attributes(&self, tag_idx: usize) -> Vec<(&'a str, &'a str)> {
        if tag_idx >= self.tag_count() { return Vec::new(); }
        let start = self.tag_starts[tag_idx] as usize;
        let end = self.tag_ends[tag_idx] as usize;
        let tag_bytes = &self.input[start..=end];
        let mut result = Vec::new();

        // Skip past '<' and tag name
        let mut pos = 1;
        while pos < tag_bytes.len()
            && tag_bytes[pos] != b'>'
            && tag_bytes[pos] != b'/'
            && !tag_bytes[pos].is_ascii_whitespace()
        {
            pos += 1;
        }

        // Scan for name=value patterns
        while pos < tag_bytes.len() && tag_bytes[pos] != b'>' {
            if tag_bytes[pos] == b'/' { break; }
            if tag_bytes[pos].is_ascii_whitespace() { pos += 1; continue; }

            let attr_name_start = pos;
            while pos < tag_bytes.len()
                && tag_bytes[pos] != b'='
                && tag_bytes[pos] != b'>'
                && !tag_bytes[pos].is_ascii_whitespace()
            {
                pos += 1;
            }
            let attr_name_end = pos;
            if pos < tag_bytes.len() && tag_bytes[pos] == b'=' {
                pos += 1;
                if pos < tag_bytes.len() && (tag_bytes[pos] == b'"' || tag_bytes[pos] == b'\'') {
                    let quote = tag_bytes[pos];
                    pos += 1;
                    let val_start = pos;
                    if let Some(off) = memchr::memchr(quote, &tag_bytes[pos..]) {
                        let val_end = pos + off;
                        pos = val_end + 1;
                        if attr_name_end > attr_name_start {
                            let abs_name_start = start + attr_name_start;
                            let abs_name_end = start + attr_name_end;
                            let abs_val_start = start + val_start;
                            let abs_val_end = start + val_end;
                            // Safety: XML attribute names and values are valid UTF-8
                            let name = unsafe {
                                std::str::from_utf8_unchecked(&self.input[abs_name_start..abs_name_end])
                            };
                            let value = unsafe {
                                std::str::from_utf8_unchecked(&self.input[abs_val_start..abs_val_end])
                            };
                            result.push((name, value));
                        }
                    } else {
                        break; // malformed
                    }
                } else {
                    pos += 1;
                }
            } else {
                pos += 1;
            }
        }
        result
    }

    /// Extract namespace declarations (xmlns:prefix="uri") from a tag.
    /// Returns Vec<(prefix, uri)>. Does not include inherited namespaces.
    pub fn get_namespace_decls(&self, tag_idx: usize) -> Vec<(&'a str, &'a str)> {
        let start = self.tag_starts[tag_idx] as usize;
        let end = self.tag_ends[tag_idx] as usize;
        let tag_str = std::str::from_utf8(&self.input[start..=end]).unwrap_or("");
        let mut result = Vec::new();

        let mut pos = 0;
        while pos < tag_str.len() {
            if let Some(idx) = tag_str[pos..].find("xmlns:") {
                let abs_idx = pos + idx;
                let after = &tag_str[abs_idx + 6..];
                if let Some(eq) = after.find('=') {
                    let prefix = &after[..eq];
                    let rest = &after[eq + 1..];
                    let (quote, rest) = if rest.starts_with('"') {
                        ('"', &rest[1..])
                    } else if rest.starts_with('\'') {
                        ('\'', &rest[1..])
                    } else {
                        pos = abs_idx + 6;
                        continue;
                    };
                    if let Some(end_q) = rest.find(quote) {
                        let uri = &rest[..end_q];
                        // Return slices from original input
                        let prefix_offset = start + abs_idx + 6;
                        let uri_offset = start + abs_idx + 6 + eq + 2; // +2 for = and quote
                        let prefix_slice = std::str::from_utf8(
                            &self.input[prefix_offset..prefix_offset + prefix.len()]
                        ).unwrap_or(prefix);
                        let uri_slice = std::str::from_utf8(
                            &self.input[uri_offset..uri_offset + uri.len()]
                        ).unwrap_or(uri);
                        result.push((prefix_slice, uri_slice));
                        pos = abs_idx + 6 + eq + 2 + end_q + 1;
                    } else {
                        pos = abs_idx + 6;
                    }
                } else {
                    pos = abs_idx + 6;
                }
            } else {
                break;
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use crate::index::structural::parse_scalar;

    #[test]
    fn test_get_attribute() {
        let xml = b"<root lang=\"en\" type='main'>text</root>";
        let index = parse_scalar(xml).unwrap();
        assert_eq!(index.get_attribute(0, "lang"), Some("en"));
        assert_eq!(index.get_attribute(0, "type"), Some("main"));
        assert_eq!(index.get_attribute(0, "missing"), None);
    }

    #[test]
    fn test_attribute_on_self_closing() {
        let xml = b"<br class=\"clear\"/>";
        let index = parse_scalar(xml).unwrap();
        assert_eq!(index.get_attribute(0, "class"), Some("clear"));
    }

    // --- Brief 28: Python bindings API tests ---

    #[test]
    fn test_parent() {
        let xml = b"<root><a><b/></a></root>";
        let mut index = parse_scalar(xml).unwrap();
        index.ensure_indices();
        // root has no parent
        assert_eq!(index.parent(0), None);
        // <a> parent is <root>
        let a_idx = (0..index.tag_count()).find(|&i| index.tag_name(i) == "a").unwrap();
        assert_eq!(index.parent(a_idx), Some(0));
        // <b/> parent is <a>
        let b_idx = (0..index.tag_count()).find(|&i| index.tag_name(i) == "b").unwrap();
        assert_eq!(index.parent(b_idx), Some(a_idx));
        // OOB
        assert_eq!(index.parent(9999), None);
    }

    #[test]
    fn test_child_position() {
        let xml = b"<root><a/><b/><c/></root>";
        let mut index = parse_scalar(xml).unwrap();
        index.ensure_indices();
        let a = (0..index.tag_count()).find(|&i| index.tag_name(i) == "a").unwrap();
        let b = (0..index.tag_count()).find(|&i| index.tag_name(i) == "b").unwrap();
        let c = (0..index.tag_count()).find(|&i| index.tag_name(i) == "c").unwrap();
        assert_eq!(index.child_position(a), Some(0));
        assert_eq!(index.child_position(b), Some(1));
        assert_eq!(index.child_position(c), Some(2));
        assert_eq!(index.child_position(0), None); // root
    }

    #[test]
    fn test_child_slice_count_at() {
        let xml = b"<root><a/><b/><c/></root>";
        let mut index = parse_scalar(xml).unwrap();
        index.ensure_indices();
        assert_eq!(index.child_count(0), 3);
        assert_eq!(index.child_slice(0).len(), 3);
        let a = (0..index.tag_count()).find(|&i| index.tag_name(i) == "a").unwrap();
        assert_eq!(index.child_at(0, 0), Some(a));
        assert_eq!(index.child_at(0, 3), None);
    }

    #[test]
    fn test_direct_text_first() {
        let xml = b"<p>Hello <b>world</b> more</p>";
        let mut index = parse_scalar(xml).unwrap();
        index.ensure_indices();
        assert_eq!(index.direct_text_first(0), Some("Hello "));
        // <b> has text "world"
        let b = (0..index.tag_count()).find(|&i| index.tag_name(i) == "b").unwrap();
        assert_eq!(index.direct_text_first(b), Some("world"));
    }

    #[test]
    fn test_direct_text_first_empty() {
        let xml = b"<root><child/></root>";
        let mut index = parse_scalar(xml).unwrap();
        index.ensure_indices();
        let child = (0..index.tag_count()).find(|&i| index.tag_name(i) == "child").unwrap();
        assert_eq!(index.direct_text_first(child), None);
    }

    #[test]
    fn test_tail_text() {
        let xml = b"<p>Hello <b>bold</b> and <i>italic</i> end</p>";
        let mut index = parse_scalar(xml).unwrap();
        index.ensure_indices();
        let b = (0..index.tag_count()).find(|&i| index.tag_name(i) == "b").unwrap();
        let i_idx = (0..index.tag_count()).find(|&i| index.tag_name(i) == "i").unwrap();
        assert_eq!(index.tail_text(b), Some(" and "));
        assert_eq!(index.tail_text(i_idx), Some(" end"));
        // root has no tail
        assert_eq!(index.tail_text(0), None);
    }

    #[test]
    fn test_tail_text_none() {
        let xml = b"<root><a/><b/></root>";
        let mut index = parse_scalar(xml).unwrap();
        index.ensure_indices();
        // <b/> is last child, no text after it (just </root>)
        let b = (0..index.tag_count()).find(|&i| index.tag_name(i) == "b").unwrap();
        assert_eq!(index.tail_text(b), None);
    }

    #[test]
    fn test_attributes_basic() {
        let xml = b"<root lang=\"en\" type='main'>text</root>";
        let index = parse_scalar(xml).unwrap();
        let attrs = index.attributes(0);
        assert_eq!(attrs, vec![("lang", "en"), ("type", "main")]);
    }

    #[test]
    fn test_attributes_self_closing() {
        let xml = b"<br class=\"clear\" id=\"top\"/>";
        let index = parse_scalar(xml).unwrap();
        let attrs = index.attributes(0);
        assert_eq!(attrs, vec![("class", "clear"), ("id", "top")]);
    }

    #[test]
    fn test_attributes_empty() {
        let xml = b"<root>text</root>";
        let index = parse_scalar(xml).unwrap();
        let attrs = index.attributes(0);
        assert!(attrs.is_empty());
    }

    #[test]
    fn test_attributes_oob() {
        let xml = b"<root/>";
        let index = parse_scalar(xml).unwrap();
        assert!(index.attributes(999).is_empty());
    }

    #[test]
    fn test_attributes_single() {
        let xml = b"<item key=\"value\">text</item>";
        let index = parse_scalar(xml).unwrap();
        let attrs = index.attributes(0);
        assert_eq!(attrs, vec![("key", "value")]);
    }

    #[test]
    fn test_attributes_includes_xmlns() {
        // Unlike get_all_attribute_names which filters xmlns, attributes() includes them
        let xml = b"<root xmlns:ns=\"http://example.com\" attr=\"val\"/>";
        let index = parse_scalar(xml).unwrap();
        let attrs = index.attributes(0);
        assert_eq!(attrs.len(), 2);
        assert_eq!(attrs[0], ("xmlns:ns", "http://example.com"));
        assert_eq!(attrs[1], ("attr", "val"));
    }

    #[test]
    fn test_tail_text_self_closing() {
        let xml = b"<root><br/> text after</root>";
        let mut index = parse_scalar(xml).unwrap();
        index.ensure_indices();
        let br = (0..index.tag_count()).find(|&i| index.tag_name(i) == "br").unwrap();
        assert_eq!(index.tail_text(br), Some(" text after"));
    }
}
