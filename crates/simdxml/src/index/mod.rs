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
    pub tag_types: Vec<TagType>,

    /// Tag name: (byte offset, length) into input
    pub(crate) tag_names: Vec<(u32, u16)>,

    /// Nesting depth of each tag (0 = root level)
    pub depths: Vec<u16>,

    /// Index of parent tag (into tag_starts array). Root tags have parent = u32::MAX.
    pub(crate) parents: Vec<u32>,

    /// Text content ranges: (start_offset, end_offset) for text between tags
    pub(crate) text_ranges: Vec<TextRange>,

    // === Precomputed indices (built by `build_indices()`) ===

    /// CSR children: offsets[i]..offsets[i+1] into child_data gives children of tag i.
    pub(crate) child_offsets: Vec<u32>,
    /// Flat array of child tag indices, referenced by child_offsets.
    pub(crate) child_data: Vec<u32>,

    /// CSR text children: text_offsets[i]..text_offsets[i+1] into text_data.
    pub(crate) text_child_offsets: Vec<u32>,
    /// Flat array of text range indices, referenced by text_child_offsets.
    pub(crate) text_child_data: Vec<u32>,

    /// Matching close tag for each open tag. u32::MAX = no match.
    pub(crate) close_map: Vec<u32>,

    /// Post-order number for each tag. Enables O(1) ancestor/descendant checks:
    /// A is ancestor of B iff pre(A) < pre(B) AND post(A) > post(B).
    /// (Pre-order number is just the tag index itself.)
    pub(crate) post_order: Vec<u32>,

    // === Tag name interning ===

    /// Interned name ID per tag. Same name → same ID. u16::MAX = no name.
    pub(crate) name_ids: Vec<u16>,
    /// Unique name strings: name_id → (byte_offset, length) in input.
    pub(crate) name_table: Vec<(u32, u16)>,
    /// Inverted index: name_id → sorted list of tag indices (Open/SelfClose only).
    pub(crate) name_posting: Vec<Vec<u32>>,
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

/// Interns tag name byte slices to u16 IDs during parsing.
/// Uses linear search over a small table (typically 20-200 unique names).
/// Zero heap allocation per intern call — just byte comparison.
pub(crate) struct NameInterner<'a> {
    input: &'a [u8],
    table: Vec<(u32, u16)>, // (offset, len) for each interned name
}

impl<'a> NameInterner<'a> {
    pub fn new(input: &'a [u8]) -> Self {
        Self { input, table: Vec::with_capacity(64) }
    }

    /// Intern a name, returning its ID. Zero allocation — compares bytes directly.
    #[inline]
    pub fn intern(&mut self, name_bytes: &[u8], offset: u32, len: u16) -> u16 {
        // Linear scan — fast for typical XML with <200 unique tag names
        for (id, &(off, l)) in self.table.iter().enumerate() {
            if l == len && &self.input[off as usize..off as usize + l as usize] == name_bytes {
                return id as u16;
            }
        }
        let id = self.table.len() as u16;
        self.table.push((offset, len));
        id
    }

    pub fn into_table(self) -> Vec<(u32, u16)> {
        self.table
    }
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
    /// Build precomputed indices for fast XPath evaluation.
    /// Called once after structural parsing. O(n) time, flat memory layout.
    pub(crate) fn build_indices(&mut self) {
        let n = self.tag_count();

        // 1. Count children per parent (two-pass CSR build)
        let mut child_counts = vec![0u32; n + 1];
        for i in 0..n {
            let tt = self.tag_types[i];
            if tt == TagType::Close || tt == TagType::CData {
                continue;
            }
            let parent = self.parents[i];
            if parent != u32::MAX && (parent as usize) < n {
                child_counts[parent as usize] += 1;
            }
        }

        // Prefix sum → offsets
        let mut child_offsets = vec![0u32; n + 1];
        for i in 0..n {
            child_offsets[i + 1] = child_offsets[i] + child_counts[i];
        }
        let total_children = child_offsets[n] as usize;
        let mut child_data = vec![0u32; total_children];

        // Fill child_data (second pass)
        let mut write_pos = child_offsets.clone();
        for i in 0..n {
            let tt = self.tag_types[i];
            if tt == TagType::Close || tt == TagType::CData {
                continue;
            }
            let parent = self.parents[i];
            if parent != u32::MAX && (parent as usize) < n {
                let p = parent as usize;
                child_data[write_pos[p] as usize] = i as u32;
                write_pos[p] += 1;
            }
        }

        // 2. CSR for text children
        let mut text_counts = vec![0u32; n + 1];
        for range in &self.text_ranges {
            let parent = range.parent_tag;
            if parent != u32::MAX && (parent as usize) < n {
                text_counts[parent as usize] += 1;
            }
        }
        let mut text_child_offsets = vec![0u32; n + 1];
        for i in 0..n {
            text_child_offsets[i + 1] = text_child_offsets[i] + text_counts[i];
        }
        let total_text = text_child_offsets[n] as usize;
        let mut text_child_data = vec![0u32; total_text];
        let mut text_write_pos = text_child_offsets.clone();
        for (ti, range) in self.text_ranges.iter().enumerate() {
            let parent = range.parent_tag;
            if parent != u32::MAX && (parent as usize) < n {
                let p = parent as usize;
                text_child_data[text_write_pos[p] as usize] = ti as u32;
                text_write_pos[p] += 1;
            }
        }

        // 3. Close map using a stack (O(n))
        let mut close_map = vec![u32::MAX; n];
        let mut stack: Vec<usize> = Vec::new();
        for i in 0..n {
            match self.tag_types[i] {
                TagType::Open => stack.push(i),
                TagType::Close => {
                    if let Some(open_idx) = stack.pop() {
                        close_map[open_idx] = i as u32;
                    }
                }
                TagType::SelfClose => close_map[i] = i as u32,
                _ => {}
            }
        }

        self.child_offsets = child_offsets;
        self.child_data = child_data;
        self.text_child_offsets = text_child_offsets;
        self.text_child_data = text_child_data;
        self.close_map = close_map;

        // 4. Post-order numbering (O(n)):
        // Assign increasing post-order numbers. An open tag gets its number
        // when its close tag is encountered. Enables O(1) ancestor checks:
        //   A is ancestor of B iff pre(A) < pre(B) AND post(A) > post(B)
        // where pre-order number is just the tag index.
        let mut post_order = vec![0u32; n];
        let mut post_counter: u32 = 0;
        let mut open_stack: Vec<usize> = Vec::new();
        for i in 0..n {
            match self.tag_types[i] {
                TagType::Open => open_stack.push(i),
                TagType::Close => {
                    if let Some(open_idx) = open_stack.pop() {
                        post_order[open_idx] = post_counter;
                    }
                    post_order[i] = post_counter;
                    post_counter += 1;
                }
                TagType::SelfClose | TagType::Comment | TagType::PI | TagType::CData => {
                    post_order[i] = post_counter;
                    post_counter += 1;
                }
            }
        }
        self.post_order = post_order;

        // Name interning + inverted index left empty — built on demand
        // via build_name_index() for repeated-query workloads.
    }

    /// Get child tag indices for a parent (from precomputed CSR index).
    #[inline]
    pub(crate) fn child_tag_slice(&self, parent_idx: usize) -> &[u32] {
        if self.child_offsets.len() < 2 || parent_idx + 1 >= self.child_offsets.len() {
            return &[];
        }
        let start = self.child_offsets[parent_idx] as usize;
        let end = self.child_offsets[parent_idx + 1] as usize;
        &self.child_data[start..end]
    }

    /// Get child text range indices for a parent (from precomputed CSR index).
    #[inline]
    pub(crate) fn child_text_slice(&self, parent_idx: usize) -> &[u32] {
        if self.text_child_offsets.len() < 2 || parent_idx + 1 >= self.text_child_offsets.len() {
            return &[];
        }
        let start = self.text_child_offsets[parent_idx] as usize;
        let end = self.text_child_offsets[parent_idx + 1] as usize;
        &self.text_child_data[start..end]
    }

    /// Whether precomputed CSR indices are available.
    #[inline]
    pub(crate) fn has_indices(&self) -> bool {
        !self.child_offsets.is_empty()
    }

    /// O(1) ancestor check using pre/post numbering.
    /// Returns true if `ancestor_idx` is an ancestor of `descendant_idx`.
    #[inline]
    pub(crate) fn is_ancestor(&self, ancestor_idx: usize, descendant_idx: usize) -> bool {
        if self.post_order.is_empty() { return false; }
        // pre(A) < pre(B) AND post(A) > post(B)
        ancestor_idx < descendant_idx
            && self.post_order[ancestor_idx] > self.post_order[descendant_idx]
    }

    /// O(1) descendant check. Returns true if `node_idx` is a descendant of `ancestor_idx`.
    #[inline]
    pub(crate) fn is_descendant_of(&self, node_idx: usize, ancestor_idx: usize) -> bool {
        self.is_ancestor(ancestor_idx, node_idx)
    }

    /// Build inverted name index for repeated query workloads.
    /// Call this once before evaluating many XPath expressions on the same document.
    pub fn build_name_index(&mut self) {
        if !self.name_posting.is_empty() { return; }
        let n = self.tag_count();
        let mut interner = NameInterner::new(self.input);
        self.name_ids = Vec::with_capacity(n);
        for i in 0..n {
            let (off, len) = self.tag_names[i];
            if len > 0 {
                let name_bytes = &self.input[off as usize..off as usize + len as usize];
                self.name_ids.push(interner.intern(name_bytes, off, len));
            } else {
                self.name_ids.push(u16::MAX);
            }
        }
        self.name_table = interner.into_table();
        let num_names = self.name_table.len();
        let mut posting: Vec<Vec<u32>> = vec![Vec::new(); num_names];
        for i in 0..n {
            let nid = self.name_ids[i];
            if nid != u16::MAX && (nid as usize) < num_names {
                let tt = self.tag_types[i];
                if tt == TagType::Open || tt == TagType::SelfClose {
                    posting[nid as usize].push(i as u32);
                }
            }
        }
        self.name_posting = posting;
    }

    /// Look up the interned name ID for a name string. Returns None if not found.
    #[inline]
    pub(crate) fn name_id(&self, name: &str) -> Option<u16> {
        let name_bytes = name.as_bytes();
        for (id, &(off, len)) in self.name_table.iter().enumerate() {
            if len as usize == name_bytes.len()
                && &self.input[off as usize..off as usize + len as usize] == name_bytes
            {
                return Some(id as u16);
            }
        }
        None
    }

    /// Get the posting list (sorted tag indices) for a name. O(1) lookup.
    #[inline]
    pub(crate) fn tags_by_name(&self, name: &str) -> &[u32] {
        if let Some(id) = self.name_id(name) {
            if (id as usize) < self.name_posting.len() {
                return &self.name_posting[id as usize];
            }
        }
        &[]
    }

    /// Fast tag name comparison (avoids UTF-8 validation on the hot path).
    #[inline(always)]
    pub fn tag_name_eq(&self, tag_idx: usize, name: &str) -> bool {
        let (off, len) = self.tag_names[tag_idx];
        let name_bytes = name.as_bytes();
        if name_bytes.len() != len as usize { return false; }
        &self.input[off as usize..off as usize + len as usize] == name_bytes
    }

    /// Get the tag name as a string slice.
    #[inline]
    pub fn tag_name(&self, tag_idx: usize) -> &'a str {
        if tag_idx >= self.tag_names.len() {
            return "";
        }
        let (offset, len) = self.tag_names[tag_idx];
        let bytes = &self.input[offset as usize..(offset + len as u32) as usize];
        // Safety: XML input is validated during parsing; tag names are always valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }

    /// Get the text content of a text range.
    #[inline]
    pub fn text_content(&self, range: &TextRange) -> &'a str {
        let bytes = &self.input[range.start as usize..range.end as usize];
        // Safety: text content comes from valid XML input.
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }

    /// Decode XML entities in a string. Returns borrowed if no entities present.
    pub fn decode_entities(s: &str) -> std::borrow::Cow<'_, str> {
        if !s.contains('&') {
            return std::borrow::Cow::Borrowed(s);
        }
        let mut result = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '&' {
                let mut entity = String::new();
                for ec in chars.by_ref() {
                    if ec == ';' { break; }
                    entity.push(ec);
                }
                match entity.as_str() {
                    "amp" => result.push('&'),
                    "lt" => result.push('<'),
                    "gt" => result.push('>'),
                    "apos" => result.push('\''),
                    "quot" => result.push('"'),
                    e if e.starts_with('#') => {
                        let num = &e[1..];
                        let code = if let Some(hex) = num.strip_prefix('x') {
                            u32::from_str_radix(hex, 16).ok()
                        } else {
                            num.parse::<u32>().ok()
                        };
                        if let Some(ch) = code.and_then(char::from_u32) {
                            result.push(ch);
                        }
                    }
                    _ => { result.push('&'); result.push_str(&entity); result.push(';'); }
                }
            } else {
                result.push(c);
            }
        }
        std::borrow::Cow::Owned(result)
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
        // Use precomputed close_map if available
        if !self.close_map.is_empty() {
            let close = self.close_map[open_idx];
            return if close != u32::MAX { Some(close as usize) } else { None };
        }
        // Fallback: linear scan (used before build_indices)
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
        if self.has_indices() {
            self.child_tag_slice(parent_idx).iter().map(|&i| i as usize).collect()
        } else {
            (0..self.tag_count())
                .filter(|&i| self.parents[i] == parent_idx as u32
                    && (self.tag_types[i] == TagType::Open || self.tag_types[i] == TagType::SelfClose))
                .collect()
        }
    }

    /// Get text content directly under a tag (not nested).
    pub fn direct_text(&self, tag_idx: usize) -> Vec<&'a str> {
        if self.has_indices() {
            self.child_text_slice(tag_idx).iter()
                .map(|&ti| self.text_content(&self.text_ranges[ti as usize]))
                .collect()
        } else {
            self.text_ranges.iter()
                .filter(|r| r.parent_tag == tag_idx as u32)
                .map(|r| self.text_content(r))
                .collect()
        }
    }

    /// Get all text content under a tag (including nested).
    /// Uses precomputed text ranges instead of byte-by-byte tag stripping.
    pub fn all_text(&self, tag_idx: usize) -> String {
        let close_idx = self.matching_close(tag_idx).unwrap_or(tag_idx);
        let tag_start = self.tag_starts[tag_idx];
        let tag_end = if close_idx == tag_idx {
            self.tag_ends[tag_idx]
        } else {
            self.tag_starts[close_idx]
        };

        let mut result = String::new();
        for range in &self.text_ranges {
            if range.start >= tag_start && range.end <= tag_end {
                result.push_str(self.text_content(range));
            }
        }
        result
    }
}
