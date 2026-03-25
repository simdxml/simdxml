//! simdxml — SIMD-accelerated XML parser with full XPath 1.0 support.
//!
//! The world's first production SIMD XML parser. Uses a two-pass structural
//! indexing architecture (adapted from simdjson) to parse XML at 2-3 GB/s,
//! then evaluates XPath 1.0 expressions against flat arrays instead of a DOM.
//!
//! # Quick Start
//!
//! ```rust
//! use simdxml::{parse, xpath};
//!
//! let xml = b"<patent><claim>A device for...</claim></patent>";
//! let index = parse(xml).unwrap();
//!
//! // One-shot XPath
//! let texts = index.xpath_text("//claim").unwrap();
//! assert_eq!(texts, vec!["A device for..."]);
//!
//! // Compiled XPath (reusable across documents)
//! let expr = xpath::CompiledXPath::compile("//claim").unwrap();
//! let texts = expr.eval_text(&index).unwrap();
//! ```

pub mod batch;
pub mod bloom;
pub mod error;
pub mod index;
pub mod persist;
pub mod simd;
pub mod xpath;

pub use bloom::TagBloom;
pub use error::{Result, SimdXmlError};
pub use index::XmlIndex;
pub use persist::OwnedXmlIndex;
pub use xpath::CompiledXPath;

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
    /// Evaluate an XPath expression and return text content of matches.
    pub fn xpath_text(&'a self, xpath_expr: &str) -> Result<Vec<&'a str>> {
        let expr = xpath::parse_xpath(xpath_expr)?;
        xpath::eval_text(self, &expr)
    }

    /// Evaluate an XPath expression and return matching nodes.
    pub fn xpath(&self, xpath_expr: &str) -> Result<Vec<xpath::XPathNode>> {
        let expr = xpath::parse_xpath(xpath_expr)?;
        xpath::evaluate(self, &expr)
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
