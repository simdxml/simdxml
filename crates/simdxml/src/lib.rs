//! SIMD-accelerated XML parser with XPath 1.0 evaluation.
//!
//! `simdxml` parses XML into flat arrays instead of a DOM tree, then evaluates
//! XPath expressions against those arrays. The approach adapts
//! [simdjson](https://simdjson.org/)'s structural indexing architecture to XML:
//! SIMD instructions classify structural characters (`<`, `>`, `"`, etc.) in
//! parallel, producing a compact index that supports random-access XPath
//! evaluation without building a pointer-heavy tree.
//!
//! The structural index uses ~16 bytes per tag (vs ~35 for a typical DOM node),
//! has better cache locality for axis traversal, and supports all 13 XPath 1.0
//! axes via array operations with O(1) ancestor/descendant checks.
//!
//! # Quick Start
//!
//! ```rust
//! let xml = b"<library><book><title>Rust</title></book></library>";
//! let index = simdxml::parse(xml).unwrap();
//!
//! let titles = index.xpath_text("//title").unwrap();
//! assert_eq!(titles, vec!["Rust"]);
//! ```
//!
//! # Compiled Queries
//!
//! For repeated queries (batch processing, multiple documents), compile the
//! XPath expression once and reuse it:
//!
//! ```rust
//! use simdxml::CompiledXPath;
//!
//! let query = CompiledXPath::compile("//title").unwrap();
//!
//! let docs: Vec<&[u8]> = vec![
//!     b"<r><title>A</title></r>",
//!     b"<r><title>B</title></r>",
//! ];
//! for doc in &docs {
//!     let index = simdxml::parse(doc).unwrap();
//!     let results = query.eval_text(&index).unwrap();
//!     assert_eq!(results.len(), 1);
//! }
//! ```
//!
//! # Scalar Expressions
//!
//! Top-level scalar expressions (`count()`, `string()`, `boolean()`, arithmetic)
//! are supported via [`XmlIndex::eval`]:
//!
//! ```rust
//! let xml = b"<r><item/><item/><item/></r>";
//! let mut index = simdxml::parse(xml).unwrap();
//!
//! match index.eval("count(//item)").unwrap() {
//!     simdxml::XPathResult::Number(n) => assert_eq!(n, 3.0),
//!     _ => panic!("expected number"),
//! }
//! ```
//!
//! # Batch Processing
//!
//! Process many documents with a single compiled query. The batch API
//! handles bloom filter prescanning (skip files that can't match) and
//! lazy parsing (only index tags relevant to the query):
//!
//! ```rust
//! use simdxml::{batch, CompiledXPath};
//!
//! let docs: Vec<&[u8]> = vec![
//!     b"<r><claim>First</claim></r>",
//!     b"<r><other>No claims here</other></r>",
//!     b"<r><claim>Third</claim></r>",
//! ];
//! let query = CompiledXPath::compile("//claim").unwrap();
//! let results = batch::eval_batch_text_bloom(&docs, &query).unwrap();
//!
//! assert_eq!(results[0], vec!["First"]);
//! assert!(results[1].is_empty()); // skipped via bloom filter
//! assert_eq!(results[2], vec!["Third"]);
//! ```
//!
//! # Parallel Parsing
//!
//! Large files can be split across cores for parallel structural indexing.
//! Each chunk is parsed independently, then merged:
//!
//! ```rust
//! # let xml = b"<r><a>1</a><b>2</b></r>";
//! let index = simdxml::parallel::parse_parallel(xml, 4).unwrap();
//! assert!(index.tag_count() > 0);
//! ```
//!
//! # Lazy Parsing
//!
//! When you know the query ahead of time, [`parse_for_xpath`] only indexes
//! tags relevant to the expression — skipping 70-90% of index construction
//! for selective queries on large documents:
//!
//! ```rust
//! let xml = b"<r><a>1</a><b>2</b><c>3</c></r>";
//! let index = simdxml::parse_for_xpath(xml, "//a").unwrap();
//! let texts = index.xpath_text("//a").unwrap();
//! assert_eq!(texts, vec!["1"]);
//! ```
//!
//! # Persistent Indices
//!
//! For files queried repeatedly, [`load_or_parse`] saves the structural index
//! to a `.sxi` sidecar file and reloads it via mmap on subsequent calls:
//!
//! ```rust,no_run
//! let index = simdxml::load_or_parse("large_file.xml").unwrap();
//! // First call: parses and saves large_file.sxi
//! // Subsequent calls: mmap the .sxi, skip parsing entirely
//! ```
//!
//! # Platform Support
//!
//! | Platform | SIMD Backend | Status |
//! |----------|-------------|--------|
//! | aarch64 (Apple Silicon, ARM) | NEON 128-bit | Production |
//! | x86_64 | Scalar (memchr-accelerated) | Working |
//! | x86_64 | SSE4.2 / AVX2 | In progress |
//! | wasm32 | Scalar | Planned |
//!
//! The parser automatically selects the best available backend at runtime
//! (compile-time on aarch64). A scalar fallback is always available.

pub mod batch;
pub mod bloom;
pub mod error;
pub mod index;
pub mod parallel;
pub mod persist;
#[doc(hidden)]
pub mod simd;
pub mod xpath;

pub use bloom::TagBloom;
pub use error::{Result, SimdXmlError};
pub use index::XmlIndex;
pub use persist::OwnedXmlIndex;
pub use xpath::CompiledXPath;
pub use xpath::XPathResult;

/// Parse XML bytes and build a structural index.
///
/// This is the main entry point. Returns an `XmlIndex` that can be
/// queried with XPath expressions.
///
/// Uses the fastest available parser for the input:
/// - NEON two-stage classifier for attribute-dense XML (>1 tag per 8 bytes)
/// - memchr-based scanner for text-heavy/mixed XML (sparse tags)
///
/// Both produce identical structural indices and pass 327/327 XPath conformance.
pub fn parse(input: &[u8]) -> Result<XmlIndex<'_>> {
    // Heuristic: sample first 4KB to detect attribute-heavy XML.
    // High quote-to-tag ratio means lots of attribute content to scan.
    // NEON two-stage processes quotes vectorially; memchr scans them byte-at-a-time.
    let sample = &input[..input.len().min(4096)];
    let lt_count = memchr::memchr_iter(b'<', sample).count();
    let qt_count = memchr::memchr_iter(b'"', sample).count();
    let quote_ratio = qt_count as f64 / lt_count.max(1) as f64;

    if quote_ratio > 5.0 {
        // Attribute-heavy: NEON two-stage wins (vectorized quote skipping)
        index::structural::parse_two_stage(input)
    } else {
        // Text-heavy/mixed: memchr jump-based scanner wins (skips text regions)
        index::structural::parse_scalar(input)
    }
}

/// Parse XML with query-driven optimization: only index tags relevant to the
/// given XPath expression. Falls back to full parse if the query uses wildcards.
///
/// For selective queries like `//claim/text()`, this can be 2-5x faster than
/// full parsing because it skips index construction for irrelevant tags.
pub fn parse_for_xpath<'a>(input: &'a [u8], xpath_str: &str) -> Result<XmlIndex<'a>> {
    let compiled = CompiledXPath::compile(xpath_str)?;
    match compiled.interesting_names() {
        Some(names) => index::lazy::parse_for_query(input, &names),
        None => parse(input),
    }
}

/// Load a pre-built `.sxi` index if it exists and is fresh, otherwise parse
/// and save the index for next time. Returns an `OwnedXmlIndex` that derefs
/// to `XmlIndex`.
pub fn load_or_parse(xml_path: impl AsRef<std::path::Path>) -> Result<OwnedXmlIndex> {
    let xml_path = xml_path.as_ref();
    let sxi_path = xml_path.with_extension("sxi");

    // Try loading existing .sxi
    if sxi_path.exists() {
        match persist::load_index(&sxi_path, xml_path) {
            Ok(owned) => return Ok(owned),
            Err(SimdXmlError::StaleSxi) => { /* fall through to re-parse */ }
            Err(e) => return Err(e),
        }
    }

    // Parse from scratch, serialize for next time
    let xml_bytes = std::fs::read(xml_path)?;
    let mut index = parse(&xml_bytes)?;
    index.build_name_index();
    persist::serialize_index(&index, &xml_bytes, &sxi_path)?;

    // Load the freshly-written .sxi (so we get an OwnedXmlIndex)
    persist::load_index_with_bytes(&sxi_path, xml_bytes)
}

// Convenience methods on XmlIndex
impl<'a> XmlIndex<'a> {
    /// Evaluate an XPath expression, returning either a node set or scalar value.
    ///
    /// Handles all XPath 1.0 expression types: location paths, functions like
    /// `string()`, `count()`, `boolean()`, arithmetic, comparisons, etc.
    pub fn eval(&mut self, xpath_expr: &str) -> Result<xpath::XPathResult> {
        let expr = xpath::parse_xpath(xpath_expr)?;
        // Build CSR indices if the query has multi-step paths (child axis needs them).
        // Single-step descendant queries (//name) use the fused path and don't need CSR.
        if Self::needs_indices(&expr) {
            self.ensure_indices();
        }
        // Handle relative paths with doc context
        match &expr {
            xpath::XPathExpr::LocationPath(ref path) if !path.absolute => {
                if let Some(doc_elem) = self.document_element() {
                    let nodes = xpath::evaluate_from_context(
                        self, &expr, xpath::XPathNode::Element(doc_elem))?;
                    return Ok(xpath::XPathResult::NodeSet(nodes));
                }
            }
            _ => {}
        }
        xpath::eval_xpath(self, &expr)
    }

    /// Extract raw XML for each matching XPath node.
    pub fn xpath_raw(&'a self, xpath_expr: &str) -> Result<Vec<&'a str>> {
        let nodes = self.xpath(xpath_expr)?;
        Ok(nodes.iter().map(|node| match *node {
            xpath::XPathNode::Element(idx) => self.raw_xml(idx),
            xpath::XPathNode::Text(idx) => self.text_content(&self.text_ranges[idx]),
            xpath::XPathNode::Attribute(tag_idx, _) | xpath::XPathNode::Namespace(tag_idx, _) => {
                self.raw_tag(tag_idx)
            }
        }).collect())
    }

    /// Evaluate an XPath expression and return the XPath string-value of each match.
    ///
    /// For element nodes, this is the concatenation of all descendant text —
    /// equivalent to XPath's `string()` function. This is what you usually want
    /// for display: `<a>hello <b>world</b></a>` → `"hello world"`.
    ///
    /// See `xpath_text` for direct child text only.
    pub fn xpath_string(&self, xpath_expr: &str) -> Result<Vec<String>> {
        let nodes = self.xpath(xpath_expr)?;
        let mut results = Vec::with_capacity(nodes.len());
        for node in &nodes {
            match *node {
                xpath::XPathNode::Element(idx) => {
                    results.push(self.all_text(idx));
                }
                xpath::XPathNode::Text(idx) => {
                    results.push(self.text_content(&self.text_ranges[idx]).to_string());
                }
                xpath::XPathNode::Attribute(tag_idx, _) => {
                    // Get all attribute values and concatenate (simplified)
                    let raw = self.raw_tag(tag_idx);
                    results.push(raw.to_string());
                }
                xpath::XPathNode::Namespace(_, _) => {}
            }
        }
        Ok(results)
    }

    /// Evaluate an XPath expression and return direct child text of matches.
    ///
    /// For elements like `<a>hello <b>world</b></a>`, returns `["hello "]`
    /// (only direct text nodes, not descendant element text).
    ///
    /// For best performance, call `ensure_indices()` before querying.
    /// Works without indices via linear scan fallback, just slower.
    pub fn xpath_text(&'a self, xpath_expr: &str) -> Result<Vec<&'a str>> {
        let expr = xpath::parse_xpath(xpath_expr)?;
        let nodes = self.eval_with_doc_context(&expr)?;
        xpath::extract_text(self, &nodes)
    }

    /// Evaluate an XPath expression and return matching nodes.
    ///
    /// For best performance, call `ensure_indices()` before querying.
    /// Works without indices via linear scan fallback, just slower.
    pub fn xpath(&self, xpath_expr: &str) -> Result<Vec<xpath::XPathNode>> {
        let expr = xpath::parse_xpath(xpath_expr)?;
        self.eval_with_doc_context(&expr)
    }

    /// Evaluate an expression, using document element as context for relative paths.
    /// Handles unions containing relative paths correctly.
    fn eval_with_doc_context(&self, expr: &xpath::XPathExpr) -> Result<Vec<xpath::XPathNode>> {
        match expr {
            xpath::XPathExpr::LocationPath(ref path) if !path.absolute => {
                if let Some(doc_elem) = self.document_element() {
                    return xpath::evaluate_from_context(
                        self, expr, xpath::XPathNode::Element(doc_elem));
                }
                xpath::evaluate(self, expr)
            }
            xpath::XPathExpr::Union(ref exprs) => {
                // Evaluate each union branch with doc context
                let mut result = Vec::new();
                for e in exprs {
                    result.extend(self.eval_with_doc_context(e)?);
                }
                // Dedup and sort in document order
                result.sort_by_key(|n| match n {
                    xpath::XPathNode::Element(i) => (*i, 0u32),
                    xpath::XPathNode::Text(i) => (*i, 1),
                    xpath::XPathNode::Attribute(i, _) => (*i, 2),
                    xpath::XPathNode::Namespace(i, _) => (*i, 3),
                });
                result.dedup_by(|a, b| match (a, b) {
                    (xpath::XPathNode::Element(i), xpath::XPathNode::Element(j)) => i == j,
                    (xpath::XPathNode::Text(i), xpath::XPathNode::Text(j)) => i == j,
                    _ => false,
                });
                Ok(result)
            }
            _ => xpath::evaluate(self, expr),
        }
    }

    /// Check if an expression needs CSR indices (multi-step paths use child axis).
    fn needs_indices(expr: &xpath::XPathExpr) -> bool {
        match expr {
            xpath::XPathExpr::LocationPath(path) => path.steps.len() > 1,
            xpath::XPathExpr::Union(exprs) => exprs.iter().any(Self::needs_indices),
            xpath::XPathExpr::FilterPath(inner, _) => Self::needs_indices(inner),
            xpath::XPathExpr::GlobalFilter(inner, _) => Self::needs_indices(inner),
            xpath::XPathExpr::FunctionCall(_, args) => args.iter().any(Self::needs_indices),
            _ => false,
        }
    }

    /// Find the document element (first depth-0 Open/SelfClose tag).
    fn document_element(&self) -> Option<usize> {
        (0..self.tag_count()).find(|&i| {
            self.depths[i] == 0
                && (self.tag_types[i] == index::TagType::Open
                    || self.tag_types[i] == index::TagType::SelfClose)
        })
    }

    /// Evaluate a predicate expression (string, number, boolean) in document context.
    pub fn eval_expr(&self, expr_str: &str) -> Result<xpath::StandaloneResult> {
        xpath::eval_expr_with_doc(self, expr_str)
    }

    /// Evaluate a predicate expression from a specific element context.
    pub fn eval_expr_from(&self, expr_str: &str, context_idx: usize) -> Result<xpath::StandaloneResult> {
        xpath::eval_expr_with_context(self, expr_str, xpath::XPathNode::Element(context_idx))
    }

    /// Evaluate a relative XPath from a specific element context node.
    pub fn xpath_from(&self, xpath_expr: &str, context_idx: usize) -> Result<Vec<xpath::XPathNode>> {
        let expr = xpath::parse_xpath(xpath_expr)?;
        let context_node = xpath::XPathNode::Element(context_idx);
        xpath::evaluate_from_context(self, &expr, context_node)
    }
}
