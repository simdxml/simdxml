pub mod analyze;
pub mod ast;
pub mod eval;
pub mod parser;
pub mod simd_pred;

pub use ast::XPathExpr;
pub use eval::{evaluate, evaluate_from_context, eval_text, eval_standalone_expr, eval_expr_with_doc, eval_expr_with_context, StandaloneResult, XPathNode};
pub use parser::{parse_xpath, parse_xpath_predicate_expr};

use crate::error::Result;
use crate::index::XmlIndex;

/// Compiled XPath expression — reusable across documents.
pub struct CompiledXPath {
    expr: XPathExpr,
}

impl CompiledXPath {
    /// Compile an XPath expression.
    pub fn compile(xpath: &str) -> Result<Self> {
        let expr = parse_xpath(xpath)?;
        Ok(Self { expr })
    }

    /// Evaluate and return matching nodes.
    pub fn eval<'a>(&self, index: &'a XmlIndex<'a>) -> Result<Vec<XPathNode>> {
        evaluate(index, &self.expr)
    }

    /// Evaluate and return text content of matching nodes.
    pub fn eval_text<'a>(&self, index: &'a XmlIndex<'a>) -> Result<Vec<&'a str>> {
        eval_text(index, &self.expr)
    }

    /// Analyze this expression for query-driven lazy parsing.
    ///
    /// Returns the set of tag names referenced, or `None` if the query
    /// uses wildcards/node() and requires all tags.
    pub fn interesting_names(&self) -> Option<std::collections::HashSet<String>> {
        match analyze::selectivity(&self.expr) {
            analyze::SelectivityHint::Selective(names) => Some(names),
            analyze::SelectivityHint::NeedsAll => None,
        }
    }

    /// Access the underlying parsed expression.
    pub fn expr(&self) -> &XPathExpr {
        &self.expr
    }
}
