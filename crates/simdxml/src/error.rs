//! Error types for XML parsing, XPath evaluation, and index persistence.

use thiserror::Error;

/// All errors produced by simdxml.
#[derive(Debug, Error)]
pub enum SimdXmlError {
    /// A structural XML parse error at a specific byte offset.
    /// Returned when the parser encounters malformed markup.
    #[error("XML parse error at byte {offset}: {message}")]
    ParseError { offset: usize, message: String },

    /// The XPath expression string could not be parsed.
    /// Contains details about the syntax error.
    #[error("XPath parse error: {0}")]
    XPathParseError(String),

    /// An error occurred during XPath evaluation (e.g., type mismatch,
    /// unsupported function, or invalid axis traversal).
    #[error("XPath evaluation error: {0}")]
    XPathEvalError(String),

    /// A `<` was found without a matching `>`. The byte offset points to the `<`.
    #[error("Unclosed tag at byte {0}")]
    UnclosedTag(usize),

    /// A `</name>` close tag does not match the expected open tag.
    #[error("Mismatched close tag: expected </{expected}>, got </{found}> at byte {offset}")]
    MismatchedCloseTag {
        expected: String,
        found: String,
        offset: usize,
    },

    /// The input is not valid XML (e.g., not valid UTF-8 or structurally broken).
    #[error("Invalid XML: {0}")]
    InvalidXml(String),

    /// An I/O error from reading XML files or `.sxi` index files.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// The `.sxi` serialized index file is corrupt or has an incompatible format.
    #[error("Invalid .sxi file: {0}")]
    InvalidSxi(String),

    /// The `.sxi` index is stale: the XML file has been modified since the
    /// index was built. Re-parse with [`crate::load_or_parse`] to rebuild.
    #[error("Stale .sxi index: XML content has changed since index was built")]
    StaleSxi,
}

/// Alias for `Result<T, SimdXmlError>`.
pub type Result<T> = std::result::Result<T, SimdXmlError>;
