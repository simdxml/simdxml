//! Static analysis of XPath expressions.
//!
//! Extract information from the AST that can be used to optimize parsing
//! and evaluation.

use super::ast::*;
use std::collections::HashSet;

/// Result of analyzing an XPath expression for selective parsing.
pub enum SelectivityHint {
    /// The query references only these specific tag names.
    /// Parsing can skip all other tags.
    Selective(HashSet<String>),
    /// The query uses wildcards, node(), or patterns that require all tags.
    NeedsAll,
}

/// Analyze an XPath expression and return the set of tag names it could match.
///
/// Returns `SelectivityHint::Selective` with the set of tag names if the query
/// only references specific names, or `SelectivityHint::NeedsAll` if wildcards
/// or `node()` tests mean all tags are potentially needed.
pub fn selectivity(expr: &XPathExpr) -> SelectivityHint {
    let mut names = HashSet::new();
    let mut needs_all = false;
    collect_names(expr, &mut names, &mut needs_all);
    if needs_all {
        SelectivityHint::NeedsAll
    } else {
        SelectivityHint::Selective(names)
    }
}

fn collect_names(expr: &XPathExpr, names: &mut HashSet<String>, needs_all: &mut bool) {
    match expr {
        XPathExpr::LocationPath(path) => {
            collect_from_steps(&path.steps, names, needs_all);
        }
        XPathExpr::Union(exprs) => {
            for e in exprs {
                collect_names(e, names, needs_all);
            }
        }
        XPathExpr::FilterPath(inner, steps) => {
            collect_names(inner, names, needs_all);
            collect_from_steps(steps, names, needs_all);
        }
        XPathExpr::GlobalFilter(inner, preds) => {
            collect_names(inner, names, needs_all);
            for p in preds {
                collect_names(p, names, needs_all);
            }
        }
        XPathExpr::BinaryOp(left, _, right) => {
            collect_names(left, names, needs_all);
            collect_names(right, names, needs_all);
        }
        XPathExpr::FunctionCall(_, args) => {
            for arg in args {
                collect_names(arg, names, needs_all);
            }
        }
        XPathExpr::UnaryMinus(inner) => {
            collect_names(inner, names, needs_all);
        }
        // Literals don't reference tags
        XPathExpr::StringLiteral(_) | XPathExpr::NumberLiteral(_) => {}
    }
}

fn collect_from_steps(steps: &[Step], names: &mut HashSet<String>, needs_all: &mut bool) {
    for step in steps {
        match &step.node_test {
            NodeTest::Name(name) => {
                names.insert(name.clone());
            }
            NodeTest::NamespacedName(_, local) => {
                names.insert(local.clone());
            }
            // Wildcard (*) requires all tags — it matches any element
            NodeTest::Wildcard => {
                *needs_all = true;
            }
            // node() with DescendantOrSelf axis is the `//` abbreviation — structural,
            // not a user-written node() test. It doesn't require all tags.
            // But node() on other axes (e.g., `child::node()`) does need all tags.
            NodeTest::Node => {
                if step.axis != Axis::DescendantOrSelf && step.axis != Axis::SelfAxis {
                    *needs_all = true;
                }
            }
            // text(), comment(), processing-instruction() don't filter by tag name
            // but they don't force NeedsAll either — they match non-element nodes
            NodeTest::Text | NodeTest::Comment | NodeTest::PI | NodeTest::PIName(_) => {}
        }

        // Predicates may reference additional tag names
        for pred in &step.predicates {
            collect_names(pred, names, needs_all);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xpath::parser::parse_xpath;

    fn selective_names(xpath: &str) -> Option<HashSet<String>> {
        let expr = parse_xpath(xpath).unwrap();
        match selectivity(&expr) {
            SelectivityHint::Selective(names) => Some(names),
            SelectivityHint::NeedsAll => None,
        }
    }

    #[test]
    fn simple_path() {
        let names = selective_names("//claim").unwrap();
        assert!(names.contains("claim"));
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn multi_step_path() {
        let names = selective_names("/corpus/patent/claims/claim").unwrap();
        assert!(names.contains("corpus"));
        assert!(names.contains("patent"));
        assert!(names.contains("claims"));
        assert!(names.contains("claim"));
    }

    #[test]
    fn with_predicate() {
        let names = selective_names("//claim[@type='independent']").unwrap();
        assert!(names.contains("claim"));
    }

    #[test]
    fn predicate_references_tag() {
        let names = selective_names("//patent[title='Widget']").unwrap();
        assert!(names.contains("patent"));
        assert!(names.contains("title"));
    }

    #[test]
    fn union() {
        let names = selective_names("//claim | //title").unwrap();
        assert!(names.contains("claim"));
        assert!(names.contains("title"));
    }

    #[test]
    fn wildcard_needs_all() {
        assert!(selective_names("//patent/*").is_none());
    }

    #[test]
    fn node_test_needs_all() {
        assert!(selective_names("//patent/node()").is_none());
    }

    #[test]
    fn text_is_selective() {
        // text() doesn't need all tags — it matches text nodes, not elements
        let names = selective_names("//claim/text()").unwrap();
        assert!(names.contains("claim"));
    }

    #[test]
    fn descendant_axis() {
        let names = selective_names("//claims//claim").unwrap();
        assert!(names.contains("claims"));
        assert!(names.contains("claim"));
    }
}
