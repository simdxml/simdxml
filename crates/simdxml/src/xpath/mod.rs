pub mod ast;
pub mod eval;
pub mod parser;

pub use ast::XPathExpr;
pub use eval::{evaluate, evaluate_from_context, eval_text, eval_standalone_expr, StandaloneResult, XPathNode};
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
}
