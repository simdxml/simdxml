pub mod structural;
pub mod tags;

/// The structural index — flat arrays, no DOM tree.
///
/// Built from XML bytes in one pass (scalar or SIMD). Enables random-access
/// evaluation of all 13 XPath 1.0 axes via array operations instead of
/// pointer-chasing through a DOM.
///
/// Memory: ~16 bytes per tag vs ~35 bytes per node in a typical DOM.
pub struct XmlIndex<'a> {
    /// Original XML bytes (borrowed, not copied)
    pub(crate) input: &'a [u8],

    /// Byte offset of each '<' (start of each tag/comment/PI)
    pub(crate) tag_starts: Vec<u32>,

    /// Byte offset of each '>' (end of each tag/comment/PI)
    pub(crate) tag_ends: Vec<u32>,

    /// Tag type classification
    pub(crate) tag_types: Vec<TagType>,

    /// Tag name: (byte offset, length) into input
    pub(crate) tag_names: Vec<(u32, u16)>,

    /// Nesting depth of each tag (0 = root level)
    pub(crate) depths: Vec<u16>,

    /// Index of parent tag (into tag_starts array). Root tags have parent = u32::MAX.
    pub(crate) parents: Vec<u32>,

    /// Text content ranges: (start_offset, end_offset) for text between tags
    pub(crate) text_ranges: Vec<TextRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagType {
    Open,      // <tag>
    Close,     // </tag>
    SelfClose, // <tag/>
    Comment,   // <!-- ... -->
    CData,     // <![CDATA[ ... ]]>
    PI,        // <?target ... ?>
}

/// A text content node between tags.
#[derive(Debug, Clone, Copy)]
pub struct TextRange {
    /// Byte offset of text start
    pub start: u32,
    /// Byte offset of text end (exclusive)
    pub end: u32,
    /// Index of the parent open tag
    pub parent_tag: u32,
}

impl<'a> XmlIndex<'a> {
    /// Get the tag name as a string slice.
    pub fn tag_name(&self, tag_idx: usize) -> &'a str {
        if tag_idx >= self.tag_names.len() {
            return "";
        }
        let (offset, len) = self.tag_names[tag_idx];
        let bytes = &self.input[offset as usize..(offset + len as u32) as usize];
        std::str::from_utf8(bytes).unwrap_or("")
    }

    /// Get the text content of a text range.
    pub fn text_content(&self, range: &TextRange) -> &'a str {
        let bytes = &self.input[range.start as usize..range.end as usize];
        std::str::from_utf8(bytes).unwrap_or("")
    }

    /// Number of tags in the index.
    pub fn tag_count(&self) -> usize {
        self.tag_starts.len()
    }

    /// Number of text content ranges.
    pub fn text_count(&self) -> usize {
        self.text_ranges.len()
    }

    /// Find the index of the close tag matching an open tag.
    pub fn matching_close(&self, open_idx: usize) -> Option<usize> {
        if open_idx >= self.tag_count() {
            return None;
        }
        if self.tag_types[open_idx] == TagType::SelfClose {
            return Some(open_idx);
        }
        if self.tag_types[open_idx] != TagType::Open {
            return None;
        }
        let depth = self.depths[open_idx];
        let name = self.tag_name(open_idx);
        for i in (open_idx + 1)..self.tag_count() {
            if self.tag_types[i] == TagType::Close
                && self.depths[i] == depth
                && self.tag_name(i) == name
            {
                return Some(i);
            }
        }
        None
    }

    /// Get children (direct child open/self-close tags) of a tag.
    pub fn children(&self, parent_idx: usize) -> Vec<usize> {
        let mut result = Vec::new();
        for i in 0..self.tag_count() {
            if self.parents[i] == parent_idx as u32
                && (self.tag_types[i] == TagType::Open
                    || self.tag_types[i] == TagType::SelfClose)
            {
                result.push(i);
            }
        }
        result
    }

    /// Get text content directly under a tag (not nested).
    pub fn direct_text(&self, tag_idx: usize) -> Vec<&'a str> {
        self.text_ranges
            .iter()
            .filter(|r| r.parent_tag == tag_idx as u32)
            .map(|r| self.text_content(r))
            .collect()
    }

    /// Get all text content under a tag (including nested).
    pub fn all_text(&self, tag_idx: usize) -> String {
        let close_idx = self.matching_close(tag_idx).unwrap_or(tag_idx);
        let start = self.tag_ends[tag_idx] as usize + 1;
        let end = self.tag_starts[close_idx] as usize;
        if start >= end || start >= self.input.len() {
            return String::new();
        }
        // Strip all tags, keep only text
        let mut result = String::new();
        let slice = &self.input[start..end.min(self.input.len())];
        let mut in_tag = false;
        for &b in slice {
            if b == b'<' {
                in_tag = true;
            } else if b == b'>' {
                in_tag = false;
            } else if !in_tag {
                result.push(b as char);
            }
        }
        result
    }
}
