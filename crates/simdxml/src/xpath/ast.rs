/// XPath 1.0 Abstract Syntax Tree types.

/// A complete XPath expression.
#[derive(Debug, Clone, PartialEq)]
pub enum XPathExpr {
    /// Location path: /a/b/c or //a/b
    LocationPath(LocationPath),
    /// String literal: 'hello'
    StringLiteral(String),
    /// Number literal: 42, 3.14
    NumberLiteral(f64),
    /// Function call: contains(., 'text')
    FunctionCall(String, Vec<XPathExpr>),
    /// Binary operator: a or b, a and b, a = b, etc.
    BinaryOp(Box<XPathExpr>, BinaryOp, Box<XPathExpr>),
    /// Unary negation: -number
    UnaryMinus(Box<XPathExpr>),
    /// Union: a | b
    Union(Vec<XPathExpr>),
    /// Filter/path expression: id('x')/p[1] — primary expr followed by path steps
    FilterPath(Box<XPathExpr>, Vec<Step>),
    /// Global filter: (expr)[pred] — evaluate expr, then filter whole result with predicates
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

/// The 13 XPath 1.0 axes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Child,
    Descendant,
    Parent,
    Ancestor,
    FollowingSibling,
    PrecedingSibling,
    Following,
    Preceding,
    SelfAxis,
    DescendantOrSelf,
    AncestorOrSelf,
    Attribute,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    And,
    Eq,
    Neq,
    Lt,
    Gt,
    Lte,
    Gte,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}
