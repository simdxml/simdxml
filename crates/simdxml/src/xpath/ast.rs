//! XPath 1.0 Abstract Syntax Tree.
//!
//! This module defines the AST types produced by the XPath parser and consumed
//! by the evaluation engine. The AST closely follows the XPath 1.0
//! grammar: expressions are either location paths, literals, function calls,
//! binary/unary operators, unions, or filter expressions.
//!
//! Location paths are sequences of [`Step`]s, each with an [`Axis`], a [`NodeTest`],
//! and zero or more predicate expressions.

/// A complete XPath 1.0 expression.
#[derive(Debug, Clone, PartialEq)]
pub enum XPathExpr {
    /// A location path like `/a/b/c` or `//a/b`.
    LocationPath(LocationPath),
    /// A string literal: `'hello'` or `"hello"`.
    StringLiteral(String),
    /// A numeric literal: `42`, `3.14`.
    NumberLiteral(f64),
    /// A function call: `contains(., 'text')`, `count(//item)`.
    FunctionCall(String, Vec<XPathExpr>),
    /// A binary operator expression: `a or b`, `a = b`, `a + b`.
    BinaryOp(Box<XPathExpr>, BinaryOp, Box<XPathExpr>),
    /// Unary negation: `-number`.
    UnaryMinus(Box<XPathExpr>),
    /// Union of node sets: `a | b`.
    Union(Vec<XPathExpr>),
    /// Filter/path expression: `id('x')/p[1]` -- a primary expression followed by path steps.
    FilterPath(Box<XPathExpr>, Vec<Step>),
    /// Global filter: `(expr)[pred]` -- evaluate the inner expression, then filter the entire result.
    GlobalFilter(Box<XPathExpr>, Vec<XPathExpr>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocationPath {
    /// True if path starts from root (/)
    pub absolute: bool,
    /// The steps in the path
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// Axis: child, descendant, parent, ancestor, etc.
    pub axis: Axis,
    /// Node test: name, *, text(), node(), etc.
    pub node_test: NodeTest,
    /// Predicates: [position()=1], [contains(., 'text')], etc.
    pub predicates: Vec<XPathExpr>,
}

/// The 13 XPath 1.0 axes, defining traversal direction from a context node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Direct children of the context node (`child::` or default axis).
    Child,
    /// All descendants (children, grandchildren, etc.) of the context node.
    Descendant,
    /// The parent of the context node (`parent::` or `..`).
    Parent,
    /// All ancestors up to and including the document root.
    Ancestor,
    /// Siblings after the context node in document order.
    FollowingSibling,
    /// Siblings before the context node in document order.
    PrecedingSibling,
    /// All nodes after the context node's closing tag in document order.
    Following,
    /// All nodes before the context node's opening tag in document order.
    Preceding,
    /// The context node itself (`self::` or `.`).
    SelfAxis,
    /// The context node plus all its descendants. Used by the `//` abbreviation.
    DescendantOrSelf,
    /// The context node plus all its ancestors.
    AncestorOrSelf,
    /// Attributes of the context element (`attribute::` or `@`).
    Attribute,
    /// Namespace nodes of the context element (rarely used).
    Namespace,
}

/// What to match at each step.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeTest {
    /// Match a specific tag name
    Name(String),
    /// Match any element: *
    Wildcard,
    /// Match text nodes: text()
    Text,
    /// Match any node: node()
    Node,
    /// Match comment nodes: comment()
    Comment,
    /// Match processing instructions: processing-instruction() or processing-instruction('name')
    PI,
    /// Match processing instruction with specific target name
    PIName(String),
    /// Namespace-prefixed name: prefix:name
    NamespacedName(String, String),
}

/// Binary operators in XPath 1.0 expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// Logical OR: `a or b`.
    Or,
    /// Logical AND: `a and b`.
    And,
    /// Equality: `a = b`.
    Eq,
    /// Inequality: `a != b`.
    Neq,
    /// Less than: `a < b`.
    Lt,
    /// Greater than: `a > b`.
    Gt,
    /// Less than or equal: `a <= b`.
    Lte,
    /// Greater than or equal: `a >= b`.
    Gte,
    /// Addition: `a + b`.
    Add,
    /// Subtraction: `a - b`.
    Sub,
    /// Multiplication: `a * b` (in expression context, not wildcard).
    Mul,
    /// Division: `a div b`.
    Div,
    /// Modulo: `a mod b`.
    Mod,
}
