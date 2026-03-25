use crate::error::{Result, SimdXmlError};
use crate::index::{TagType, XmlIndex};
use super::ast::*;
use super::parser::parse_xpath_predicate_expr;

/// A node in the XPath result set.
#[derive(Debug, Clone, Copy)]
pub enum XPathNode {
    Element(usize),              // index into tag_starts
    Text(usize),                 // index into text_ranges
    /// (tag_idx, attr_name_hash) — hash used for fast comparison
    Attribute(usize, u64),
}

/// Hash an attribute name for storage in XPathNode.
fn attr_name_hash(name: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325; // FNV-1a
    for b in name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Evaluate a standalone expression (no document context needed).
/// Returns the result as a string representation matching libxml2 format.
pub fn eval_standalone_expr(expr_str: &str) -> Result<StandaloneResult> {
    // Parse as a predicate expression (supports arithmetic, comparisons, functions)
    let parsed = parse_xpath_predicate_expr(expr_str)?;

    // Create a minimal dummy context
    let dummy = b"<r/>";
    let index = crate::index::structural::parse_scalar(dummy)?;
    let node = XPathNode::Element(DOC_ROOT);

    let value = eval_predicate_value(&index, node, &parsed, 1, 1)?;
    Ok(match value {
        XPathValue::Number(n) => StandaloneResult::Number(n),
        XPathValue::String(s) => StandaloneResult::String(s),
        XPathValue::Boolean(b) => StandaloneResult::Boolean(b),
    })
}

/// Result of standalone expression evaluation.
#[derive(Debug, Clone)]
pub enum StandaloneResult {
    Number(f64),
    String(String),
    Boolean(bool),
}

/// Evaluate an XPath expression against an XmlIndex.
pub fn evaluate<'a>(
    index: &'a XmlIndex<'a>,
    expr: &XPathExpr,
) -> Result<Vec<XPathNode>> {
    match expr {
        XPathExpr::LocationPath(path) => eval_location_path(index, path),
        XPathExpr::Union(exprs) => {
            let mut result = Vec::new();
            for e in exprs {
                result.extend(evaluate(index, e)?);
            }
            Ok(result)
        }
        _ => Err(SimdXmlError::XPathEvalError(
            "Only location paths and unions are supported".into(),
        )),
    }
}

/// Extract text results from XPath evaluation.
pub fn eval_text<'a>(
    index: &'a XmlIndex<'a>,
    expr: &XPathExpr,
) -> Result<Vec<&'a str>> {
    let nodes = evaluate(index, expr)?;
    let mut results = Vec::new();
    for node in nodes {
        match node {
            XPathNode::Element(idx) => {
                // For elements, return all text content
                let text = index.all_text(idx);
                if !text.is_empty() {
                    // We need to return owned strings for all_text, but &str for direct text
                    // For now, use direct text
                    for t in index.direct_text(idx) {
                        results.push(t);
                    }
                }
            }
            XPathNode::Text(idx) => {
                results.push(index.text_content(&index.text_ranges[idx]));
            }
            XPathNode::Attribute(tag_idx, _) => {
                // Placeholder — attribute value extraction
            }
        }
    }
    Ok(results)
}

/// Sentinel index for the virtual document root.
const DOC_ROOT: usize = usize::MAX;

fn eval_location_path<'a>(
    index: &'a XmlIndex<'a>,
    path: &LocationPath,
) -> Result<Vec<XPathNode>> {
    let mut context: Vec<XPathNode> = if path.absolute {
        // Virtual document root — child axis returns depth-0 elements
        vec![XPathNode::Element(DOC_ROOT)]
    } else {
        // Relative path: needs a context node. When called from evaluate()
        // (top-level), use DOC_ROOT. When called from evaluate_in_context(),
        // the context is set by the caller.
        // This flag is set by evaluate_in_context.
        return Err(SimdXmlError::XPathEvalError(
            "Relative path needs context (use evaluate_in_context)".into(),
        ));
    };

    for step in &path.steps {
        context = eval_step(index, &context, step)?;
    }

    Ok(context)
}

fn eval_step<'a>(
    index: &'a XmlIndex<'a>,
    context: &[XPathNode],
    step: &Step,
) -> Result<Vec<XPathNode>> {
    let mut result = Vec::new();

    // For each context node, evaluate the axis + node test + predicates.
    // Predicates are applied PER CONTEXT NODE — this is critical for
    // correct position() behavior. //p[1] means "first p child of each parent".
    for &node in context {
        let candidates = match step.axis {
            Axis::Child => eval_child_axis(index, node),
            Axis::Descendant => eval_descendant_axis(index, node, false),
            Axis::DescendantOrSelf => eval_descendant_axis(index, node, true),
            Axis::Parent => eval_parent_axis(index, node),
            Axis::Ancestor => eval_ancestor_axis(index, node, false),
            Axis::AncestorOrSelf => eval_ancestor_axis(index, node, true),
            Axis::FollowingSibling => eval_following_sibling_axis(index, node),
            Axis::PrecedingSibling => eval_preceding_sibling_axis(index, node),
            Axis::Following => eval_following_axis(index, node),
            Axis::Preceding => eval_preceding_axis(index, node),
            Axis::SelfAxis => vec![node],
            Axis::Attribute => eval_attribute_axis(index, node, &step.node_test),
            Axis::Namespace => vec![],
        };

        // Filter by node test
        let mut matched: Vec<XPathNode> = candidates
            .into_iter()
            .filter(|c| matches_node_test(index, *c, &step.node_test))
            .collect();

        // Apply predicates per-context-node
        for pred in &step.predicates {
            matched = apply_predicate(index, &matched, pred)?;
        }

        result.extend(matched);
    }

    // XPath node sets are always unique, in document order.
    // Deduplicate by node identity.
    dedup_nodes(&mut result);

    Ok(result)
}

fn dedup_nodes(nodes: &mut Vec<XPathNode>) {
    let mut seen = std::collections::HashSet::new();
    nodes.retain(|n| {
        let key = match n {
            XPathNode::Element(idx) => (0, *idx),
            XPathNode::Text(idx) => (1, *idx),
            XPathNode::Attribute(idx, h) => (2, *idx),
        };
        seen.insert(key)
    });
}

/// Apply a predicate to filter a node set.
fn apply_predicate<'a>(
    index: &'a XmlIndex<'a>,
    nodes: &[XPathNode],
    pred: &XPathExpr,
) -> Result<Vec<XPathNode>> {
    match pred {
        // Numeric predicate: [1] means position() = 1
        XPathExpr::NumberLiteral(n) => {
            let pos = *n as usize;
            if pos >= 1 && pos <= nodes.len() {
                Ok(vec![nodes[pos - 1]])
            } else {
                Ok(vec![])
            }
        }

        // Comparison: [@attr='value'], [position()=N], etc.
        XPathExpr::BinaryOp(left, op, right) => {
            let mut result = Vec::new();
            for (i, &node) in nodes.iter().enumerate() {
                let left_val = eval_predicate_value(index, node, left, i + 1, nodes.len())?;
                let right_val = eval_predicate_value(index, node, right, i + 1, nodes.len())?;
                if compare_values(&left_val, op, &right_val) {
                    result.push(node);
                }
            }
            Ok(result)
        }

        // Function call as boolean: [contains(., 'text')]
        XPathExpr::FunctionCall(name, args) => {
            let mut result = Vec::new();
            for (i, &node) in nodes.iter().enumerate() {
                let val = eval_function(index, node, name, args, i + 1, nodes.len())?;
                if val.is_truthy() {
                    result.push(node);
                }
            }
            Ok(result)
        }

        // Location path as boolean: [@attr] means "has attribute"
        XPathExpr::LocationPath(_) => {
            let mut result = Vec::new();
            for &node in nodes {
                let sub_nodes = evaluate_in_context(index, node, pred)?;
                if !sub_nodes.is_empty() {
                    result.push(node);
                }
            }
            Ok(result)
        }

        _ => Ok(nodes.to_vec()),
    }
}

/// A value in XPath evaluation (string, number, or boolean).
#[derive(Debug, Clone)]
enum XPathValue {
    String(String),
    Number(f64),
    Boolean(bool),
}

impl XPathValue {
    fn is_truthy(&self) -> bool {
        match self {
            XPathValue::Boolean(b) => *b,
            XPathValue::String(s) => !s.is_empty(),
            XPathValue::Number(n) => *n != 0.0 && !n.is_nan(),
        }
    }

    fn as_string(&self) -> String {
        match self {
            XPathValue::String(s) => s.clone(),
            XPathValue::Number(n) => n.to_string(),
            XPathValue::Boolean(b) => b.to_string(),
        }
    }

    fn as_number(&self) -> f64 {
        match self {
            XPathValue::Number(n) => *n,
            XPathValue::String(s) => s.parse().unwrap_or(f64::NAN),
            XPathValue::Boolean(b) => if *b { 1.0 } else { 0.0 },
        }
    }
}

fn eval_predicate_value(
    index: &XmlIndex,
    node: XPathNode,
    expr: &XPathExpr,
    position: usize,
    size: usize,
) -> Result<XPathValue> {
    match expr {
        XPathExpr::StringLiteral(s) => Ok(XPathValue::String(s.clone())),
        XPathExpr::NumberLiteral(n) => Ok(XPathValue::Number(*n)),
        XPathExpr::FunctionCall(name, args) => eval_function(index, node, name, args, position, size),
        XPathExpr::LocationPath(path) => {
            // Special case: @attr — extract attribute value directly
            if path.steps.len() == 1 && path.steps[0].axis == Axis::Attribute {
                if let XPathNode::Element(idx) = node {
                    if let NodeTest::Name(attr_name) = &path.steps[0].node_test {
                        if let Some(val) = index.get_attribute(idx, attr_name) {
                            return Ok(XPathValue::String(val.to_string()));
                        }
                    }
                }
                return Ok(XPathValue::String(String::new()));
            }

            // General case: evaluate path in context and return string value
            let nodes = evaluate_in_context(index, node, expr)?;
            if let Some(n) = nodes.first() {
                Ok(XPathValue::String(node_string_value(index, *n)))
            } else {
                Ok(XPathValue::String(String::new()))
            }
        }
        XPathExpr::UnaryMinus(inner) => {
            let val = eval_predicate_value(index, node, inner, position, size)?;
            Ok(XPathValue::Number(-val.as_number()))
        }
        XPathExpr::BinaryOp(left, op, right) => {
            let l = eval_predicate_value(index, node, left, position, size)?;
            let r = eval_predicate_value(index, node, right, position, size)?;
            let ln = l.as_number();
            let rn = r.as_number();
            let result = match op {
                BinaryOp::Add => ln + rn,
                BinaryOp::Sub => ln - rn,
                BinaryOp::Mul => ln * rn,
                BinaryOp::Div => ln / rn,
                BinaryOp::Mod => ln % rn,
                _ => {
                    // Comparison operators return boolean
                    return Ok(XPathValue::Boolean(compare_values(&l, op, &r)));
                }
            };
            Ok(XPathValue::Number(result))
        }
        _ => Ok(XPathValue::String(String::new())),
    }
}

fn eval_function(
    index: &XmlIndex,
    node: XPathNode,
    name: &str,
    args: &[XPathExpr],
    position: usize,
    size: usize,
) -> Result<XPathValue> {
    match name {
        "position" => Ok(XPathValue::Number(position as f64)),
        "last" => Ok(XPathValue::Number(size as f64)),
        "count" => {
            if let Some(arg) = args.first() {
                let nodes = evaluate_in_context(index, node, arg)?;
                Ok(XPathValue::Number(nodes.len() as f64))
            } else {
                Ok(XPathValue::Number(0.0))
            }
        }
        "contains" => {
            if args.len() >= 2 {
                let haystack = eval_predicate_value(index, node, &args[0], position, size)?.as_string();
                let needle = eval_predicate_value(index, node, &args[1], position, size)?.as_string();
                Ok(XPathValue::Boolean(haystack.contains(&needle)))
            } else {
                Ok(XPathValue::Boolean(false))
            }
        }
        "starts-with" => {
            if args.len() >= 2 {
                let haystack = eval_predicate_value(index, node, &args[0], position, size)?.as_string();
                let prefix = eval_predicate_value(index, node, &args[1], position, size)?.as_string();
                Ok(XPathValue::Boolean(haystack.starts_with(&prefix)))
            } else {
                Ok(XPathValue::Boolean(false))
            }
        }
        "string-length" => {
            let s = if let Some(arg) = args.first() {
                eval_predicate_value(index, node, arg, position, size)?.as_string()
            } else {
                node_string_value(index, node)
            };
            Ok(XPathValue::Number(s.len() as f64))
        }
        "normalize-space" => {
            let s = if let Some(arg) = args.first() {
                eval_predicate_value(index, node, arg, position, size)?.as_string()
            } else {
                node_string_value(index, node)
            };
            let normalized = s.split_whitespace().collect::<Vec<_>>().join(" ");
            Ok(XPathValue::String(normalized))
        }
        "not" => {
            if let Some(arg) = args.first() {
                let val = eval_predicate_value(index, node, arg, position, size)?;
                Ok(XPathValue::Boolean(!val.is_truthy()))
            } else {
                Ok(XPathValue::Boolean(true))
            }
        }
        "true" => Ok(XPathValue::Boolean(true)),
        "false" => Ok(XPathValue::Boolean(false)),
        "name" | "local-name" => {
            match node {
                XPathNode::Element(idx) if idx != DOC_ROOT => {
                    Ok(XPathValue::String(index.tag_name(idx).to_string()))
                }
                _ => Ok(XPathValue::String(String::new())),
            }
        }
        "string" => {
            if let Some(arg) = args.first() {
                let val = eval_predicate_value(index, node, arg, position, size)?;
                Ok(XPathValue::String(val.as_string()))
            } else {
                Ok(XPathValue::String(node_string_value(index, node)))
            }
        }
        "concat" => {
            let mut result = String::new();
            for arg in args {
                result.push_str(&eval_predicate_value(index, node, arg, position, size)?.as_string());
            }
            Ok(XPathValue::String(result))
        }
        "substring" => {
            if args.len() >= 2 {
                let s = eval_predicate_value(index, node, &args[0], position, size)?.as_string();
                let start = eval_predicate_value(index, node, &args[1], position, size)?.as_number() as usize;
                let start = start.saturating_sub(1); // XPath is 1-indexed
                if args.len() >= 3 {
                    let len = eval_predicate_value(index, node, &args[2], position, size)?.as_number() as usize;
                    Ok(XPathValue::String(s.chars().skip(start).take(len).collect()))
                } else {
                    Ok(XPathValue::String(s.chars().skip(start).collect()))
                }
            } else {
                Ok(XPathValue::String(String::new()))
            }
        }
        "floor" => {
            let n = if let Some(arg) = args.first() {
                eval_predicate_value(index, node, arg, position, size)?.as_number()
            } else { 0.0 };
            Ok(XPathValue::Number(n.floor()))
        }
        "ceiling" => {
            let n = if let Some(arg) = args.first() {
                eval_predicate_value(index, node, arg, position, size)?.as_number()
            } else { 0.0 };
            Ok(XPathValue::Number(n.ceil()))
        }
        "round" => {
            let n = if let Some(arg) = args.first() {
                eval_predicate_value(index, node, arg, position, size)?.as_number()
            } else { 0.0 };
            // XPath round: round half to positive infinity
            Ok(XPathValue::Number(if n.fract() == -0.5 { n.ceil() } else { n.round() }))
        }
        "number" => {
            let val = if let Some(arg) = args.first() {
                eval_predicate_value(index, node, arg, position, size)?
            } else {
                XPathValue::String(node_string_value(index, node))
            };
            Ok(XPathValue::Number(val.as_number()))
        }
        "sum" => {
            // sum() takes a node-set, sums string values as numbers
            Ok(XPathValue::Number(0.0)) // TODO: proper node-set sum
        }
        "translate" => {
            if args.len() >= 3 {
                let s = eval_predicate_value(index, node, &args[0], position, size)?.as_string();
                let from = eval_predicate_value(index, node, &args[1], position, size)?.as_string();
                let to = eval_predicate_value(index, node, &args[2], position, size)?.as_string();
                let from_chars: Vec<char> = from.chars().collect();
                let to_chars: Vec<char> = to.chars().collect();
                let result: String = s.chars().filter_map(|c| {
                    if let Some(pos) = from_chars.iter().position(|&fc| fc == c) {
                        to_chars.get(pos).copied() // replace or remove
                    } else {
                        Some(c)
                    }
                }).collect();
                Ok(XPathValue::String(result))
            } else {
                Ok(XPathValue::String(String::new()))
            }
        }
        "substring-before" => {
            if args.len() >= 2 {
                let s = eval_predicate_value(index, node, &args[0], position, size)?.as_string();
                let needle = eval_predicate_value(index, node, &args[1], position, size)?.as_string();
                if let Some(pos) = s.find(&needle) {
                    Ok(XPathValue::String(s[..pos].to_string()))
                } else {
                    Ok(XPathValue::String(String::new()))
                }
            } else {
                Ok(XPathValue::String(String::new()))
            }
        }
        "substring-after" => {
            if args.len() >= 2 {
                let s = eval_predicate_value(index, node, &args[0], position, size)?.as_string();
                let needle = eval_predicate_value(index, node, &args[1], position, size)?.as_string();
                if let Some(pos) = s.find(&needle) {
                    Ok(XPathValue::String(s[pos + needle.len()..].to_string()))
                } else {
                    Ok(XPathValue::String(String::new()))
                }
            } else {
                Ok(XPathValue::String(String::new()))
            }
        }
        "boolean" => {
            let val = if let Some(arg) = args.first() {
                eval_predicate_value(index, node, arg, position, size)?
            } else {
                XPathValue::Boolean(false)
            };
            Ok(XPathValue::Boolean(val.is_truthy()))
        }
        _ => Ok(XPathValue::String(String::new())),
    }
}

fn compare_values(left: &XPathValue, op: &BinaryOp, right: &XPathValue) -> bool {
    match op {
        BinaryOp::Eq => left.as_string() == right.as_string() || left.as_number() == right.as_number(),
        BinaryOp::Neq => left.as_string() != right.as_string() && left.as_number() != right.as_number(),
        BinaryOp::Lt => left.as_number() < right.as_number(),
        BinaryOp::Gt => left.as_number() > right.as_number(),
        BinaryOp::Lte => left.as_number() <= right.as_number(),
        BinaryOp::Gte => left.as_number() >= right.as_number(),
        _ => false,
    }
}

fn node_string_value(index: &XmlIndex, node: XPathNode) -> String {
    match node {
        XPathNode::Element(idx) if idx != DOC_ROOT => index.all_text(idx),
        XPathNode::Text(idx) => index.text_content(&index.text_ranges[idx]).to_string(),
        XPathNode::Attribute(tag_idx, _) => {
            // The attribute name is stored as the step's node_test
            // For now, we need to get the last-evaluated attribute name
            // This is a limitation — we'd need to pass the attr name through
            String::new()
        }
        _ => String::new(),
    }
}

/// Evaluate an expression in the context of a specific node.
fn evaluate_in_context(
    index: &XmlIndex,
    context_node: XPathNode,
    expr: &XPathExpr,
) -> Result<Vec<XPathNode>> {
    match expr {
        XPathExpr::LocationPath(path) if !path.absolute => {
            // Relative path: evaluate from context node
            let mut context = vec![context_node];
            for step in &path.steps {
                context = eval_step(index, &context, step)?;
            }
            Ok(context)
        }
        XPathExpr::LocationPath(path) => {
            // Absolute path: evaluate from document root
            evaluate(index, expr)
        }
        _ => Ok(vec![]),
    }
}

fn matches_node_test(index: &XmlIndex, node: XPathNode, test: &NodeTest) -> bool {
    match (node, test) {
        (_, NodeTest::Node) => true,
        (_, NodeTest::Wildcard) => matches!(node, XPathNode::Element(_)),
        (XPathNode::Text(_), NodeTest::Text) => true,
        (XPathNode::Element(idx), NodeTest::Name(name)) => index.tag_name(idx) == name,
        (XPathNode::Element(idx), NodeTest::Comment) => {
            index.tag_types[idx] == TagType::Comment
        }
        (XPathNode::Element(idx), NodeTest::PI) => index.tag_types[idx] == TagType::PI,
        (XPathNode::Attribute(_, _), NodeTest::Name(_)) => true, // already matched in axis
        _ => false,
    }
}

// ============================================================================
// Axis implementations — all 13 axes as array operations on XmlIndex
// ============================================================================

fn eval_child_axis(index: &XmlIndex, node: XPathNode) -> Vec<XPathNode> {
    let XPathNode::Element(parent_idx) = node else {
        return vec![];
    };

    let mut result: Vec<XPathNode> = Vec::new();

    if parent_idx == DOC_ROOT {
        // Document root's children are depth-0 elements
        for i in 0..index.tag_count() {
            if index.depths[i] == 0
                && (index.tag_types[i] == TagType::Open
                    || index.tag_types[i] == TagType::SelfClose)
            {
                result.push(XPathNode::Element(i));
            }
        }
        return result;
    }

    // Collect children with byte offsets for document-order sorting
    let mut children_with_pos: Vec<(u32, XPathNode)> = Vec::new();

    // Child elements
    for i in 0..index.tag_count() {
        if index.parents[i] == parent_idx as u32
            && (index.tag_types[i] == TagType::Open
                || index.tag_types[i] == TagType::SelfClose)
        {
            children_with_pos.push((index.tag_starts[i], XPathNode::Element(i)));
        }
    }

    // Child text nodes
    for (i, range) in index.text_ranges.iter().enumerate() {
        if range.parent_tag == parent_idx as u32 {
            children_with_pos.push((range.start, XPathNode::Text(i)));
        }
    }

    // Sort by document position
    children_with_pos.sort_by_key(|(pos, _)| *pos);
    children_with_pos.into_iter().map(|(_, node)| node).collect()
}

fn eval_descendant_axis(index: &XmlIndex, node: XPathNode, include_self: bool) -> Vec<XPathNode> {
    let XPathNode::Element(start_idx) = node else {
        return if include_self { vec![node] } else { vec![] };
    };

    if start_idx == DOC_ROOT {
        // Descendants of document root = all elements + text nodes
        let mut result = Vec::new();
        for i in 0..index.tag_count() {
            if index.tag_types[i] == TagType::Open || index.tag_types[i] == TagType::SelfClose {
                result.push(XPathNode::Element(i));
            }
        }
        for i in 0..index.text_ranges.len() {
            result.push(XPathNode::Text(i));
        }
        return result;
    }

    let mut result = Vec::new();
    if include_self {
        result.push(node);
    }

    let _start_depth = index.depths[start_idx];

    // All tags after start_idx until depth returns to start_depth
    let close_idx = index.matching_close(start_idx).unwrap_or(index.tag_count());
    for i in (start_idx + 1)..close_idx {
        if index.tag_types[i] == TagType::Open || index.tag_types[i] == TagType::SelfClose {
            result.push(XPathNode::Element(i));
        }
    }

    // Descendant text nodes
    for (i, range) in index.text_ranges.iter().enumerate() {
        let parent = range.parent_tag as usize;
        if parent >= start_idx && parent < close_idx {
            result.push(XPathNode::Text(i));
        }
    }

    result
}

fn eval_parent_axis(index: &XmlIndex, node: XPathNode) -> Vec<XPathNode> {
    match node {
        XPathNode::Element(idx) => {
            let parent = index.parents[idx];
            if parent != u32::MAX {
                vec![XPathNode::Element(parent as usize)]
            } else {
                vec![]
            }
        }
        XPathNode::Text(idx) => {
            let parent = index.text_ranges[idx].parent_tag;
            if parent != u32::MAX {
                vec![XPathNode::Element(parent as usize)]
            } else {
                vec![]
            }
        }
        _ => vec![],
    }
}

fn eval_ancestor_axis(index: &XmlIndex, node: XPathNode, include_self: bool) -> Vec<XPathNode> {
    let mut result = Vec::new();
    if include_self {
        result.push(node);
    }

    let mut current = match node {
        XPathNode::Element(idx) => index.parents[idx],
        XPathNode::Text(idx) => index.text_ranges[idx].parent_tag,
        _ => u32::MAX,
    };

    while current != u32::MAX {
        result.push(XPathNode::Element(current as usize));
        current = index.parents[current as usize];
    }

    result
}

fn eval_following_sibling_axis(index: &XmlIndex, node: XPathNode) -> Vec<XPathNode> {
    let XPathNode::Element(idx) = node else {
        return vec![];
    };

    let parent = index.parents[idx];
    let depth = index.depths[idx];
    let mut result = Vec::new();

    for i in (idx + 1)..index.tag_count() {
        if index.parents[i] == parent
            && index.depths[i] == depth
            && (index.tag_types[i] == TagType::Open
                || index.tag_types[i] == TagType::SelfClose)
        {
            result.push(XPathNode::Element(i));
        }
    }

    result
}

fn eval_preceding_sibling_axis(index: &XmlIndex, node: XPathNode) -> Vec<XPathNode> {
    let XPathNode::Element(idx) = node else {
        return vec![];
    };

    let parent = index.parents[idx];
    let depth = index.depths[idx];
    let mut result = Vec::new();

    for i in (0..idx).rev() {
        if index.parents[i] == parent
            && index.depths[i] == depth
            && (index.tag_types[i] == TagType::Open
                || index.tag_types[i] == TagType::SelfClose)
        {
            result.push(XPathNode::Element(i));
        }
    }

    result
}

fn eval_following_axis(index: &XmlIndex, node: XPathNode) -> Vec<XPathNode> {
    let XPathNode::Element(idx) = node else {
        return vec![];
    };
    if idx == DOC_ROOT || idx >= index.tag_count() {
        return vec![];
    }

    let close = index.matching_close(idx).unwrap_or(idx);
    let mut result = Vec::new();

    for i in (close.saturating_add(1))..index.tag_count() {
        if index.tag_types[i] == TagType::Open || index.tag_types[i] == TagType::SelfClose {
            result.push(XPathNode::Element(i));
        }
    }

    result
}

fn eval_preceding_axis(index: &XmlIndex, node: XPathNode) -> Vec<XPathNode> {
    let XPathNode::Element(idx) = node else {
        return vec![];
    };
    if idx == DOC_ROOT || idx >= index.tag_count() {
        return vec![];
    }

    let mut result = Vec::new();

    // All elements before this one, excluding ancestors
    let ancestors: Vec<u32> = {
        let mut a = Vec::new();
        let mut current = index.parents[idx];
        while current != u32::MAX {
            a.push(current);
            current = index.parents[current as usize];
        }
        a
    };

    for i in (0..idx).rev() {
        if (index.tag_types[i] == TagType::Open || index.tag_types[i] == TagType::SelfClose)
            && !ancestors.contains(&(i as u32))
        {
            result.push(XPathNode::Element(i));
        }
    }

    result
}

fn eval_attribute_axis(
    index: &XmlIndex,
    node: XPathNode,
    test: &NodeTest,
) -> Vec<XPathNode> {
    let XPathNode::Element(idx) = node else {
        return vec![];
    };

    match test {
        NodeTest::Name(name) => {
            if index.get_attribute(idx, name).is_some() {
                vec![XPathNode::Attribute(idx, 0)]
            } else {
                vec![]
            }
        }
        NodeTest::Wildcard => {
            // TODO: return all attributes
            vec![]
        }
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::structural::parse_scalar;
    use crate::xpath::parser::parse_xpath;

    fn query_text<'a>(xml: &'a [u8], xpath: &str) -> Vec<String> {
        let index = parse_scalar(xml).unwrap();
        let expr = parse_xpath(xpath).unwrap();
        let nodes = evaluate(&index, &expr).unwrap();
        let mut results = Vec::new();
        for node in nodes {
            match node {
                XPathNode::Element(idx) => {
                    for t in index.direct_text(idx) {
                        results.push(t.to_string());
                    }
                }
                XPathNode::Text(idx) => {
                    results.push(index.text_content(&index.text_ranges[idx]).to_string());
                }
                _ => {}
            }
        }
        results
    }

    fn query_names<'a>(xml: &'a [u8], xpath: &str) -> Vec<String> {
        let index = parse_scalar(xml).unwrap();
        let expr = parse_xpath(xpath).unwrap();
        let nodes = evaluate(&index, &expr).unwrap();
        nodes
            .iter()
            .filter_map(|n| match n {
                XPathNode::Element(idx) => Some(index.tag_name(*idx).to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_simple_child() {
        let names = query_names(b"<root><a/><b/><c/></root>", "/root/*");
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_specific_child() {
        let names = query_names(b"<root><a/><b/><c/></root>", "/root/b");
        assert_eq!(names, vec!["b"]);
    }

    #[test]
    fn test_text_content() {
        let texts = query_text(b"<root><item>hello</item><item>world</item></root>", "/root/item");
        assert_eq!(texts, vec!["hello", "world"]);
    }

    #[test]
    fn test_descendant() {
        let names = query_names(
            b"<root><a><b><c/></b></a></root>",
            "//c",
        );
        assert_eq!(names, vec!["c"]);
    }

    #[test]
    fn test_text_node() {
        let texts = query_text(
            b"<root>hello</root>",
            "/root/text()",
        );
        assert_eq!(texts, vec!["hello"]);
    }

    #[test]
    fn test_wildcard() {
        let names = query_names(
            b"<root><a/><b/></root>",
            "/root/*",
        );
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn test_deep_path() {
        let texts = query_text(
            b"<patent><claims><claim>Claim 1 text</claim></claims></patent>",
            "/patent/claims/claim",
        );
        assert_eq!(texts, vec!["Claim 1 text"]);
    }

    #[test]
    fn test_descendant_deep() {
        let names = query_names(
            b"<a><b><c><d/></c></b><e><d/></e></a>",
            "//d",
        );
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn test_following_sibling() {
        let xml = b"<root><a/><b/><c/></root>";
        let index = parse_scalar(xml).unwrap();
        // Get siblings following <a>
        let a_idx = 1; // <a> is at index 1 (after <root>)
        let siblings = eval_following_sibling_axis(&index, XPathNode::Element(a_idx));
        assert_eq!(siblings.len(), 2); // b and c
    }

    #[test]
    fn test_preceding_sibling() {
        let xml = b"<root><a/><b/><c/></root>";
        let index = parse_scalar(xml).unwrap();
        let c_idx = 3; // <c> is at index 3
        let siblings = eval_preceding_sibling_axis(&index, XPathNode::Element(c_idx));
        assert_eq!(siblings.len(), 2); // a and b (in reverse order)
    }

    #[test]
    fn test_parent() {
        let xml = b"<root><child/></root>";
        let index = parse_scalar(xml).unwrap();
        let parents = eval_parent_axis(&index, XPathNode::Element(1)); // child's parent
        assert_eq!(parents.len(), 1);
        match parents[0] {
            XPathNode::Element(idx) => assert_eq!(index.tag_name(idx), "root"),
            _ => panic!("Expected element"),
        }
    }

    #[test]
    fn test_ancestor() {
        let xml = b"<a><b><c/></b></a>";
        let index = parse_scalar(xml).unwrap();
        let ancestors = eval_ancestor_axis(&index, XPathNode::Element(2), false); // c's ancestors
        assert_eq!(ancestors.len(), 2); // b and a
    }

    // --- Predicate tests ---

    #[test]
    fn test_position_predicate() {
        let names = query_names(
            b"<root><a/><b/><c/></root>",
            "/root/*[1]",
        );
        assert_eq!(names, vec!["a"]);
    }

    #[test]
    fn test_position_predicate_last() {
        let names = query_names(
            b"<root><a/><b/><c/></root>",
            "/root/*[3]",
        );
        assert_eq!(names, vec!["c"]);
    }

    #[test]
    fn test_position_function_predicate() {
        let names = query_names(
            b"<root><a/><b/><c/></root>",
            "/root/*[position()=2]",
        );
        assert_eq!(names, vec!["b"]);
    }

    #[test]
    fn test_last_function_predicate() {
        let names = query_names(
            b"<root><a/><b/><c/></root>",
            "/root/*[position()=last()]",
        );
        assert_eq!(names, vec!["c"]);
    }

    #[test]
    fn test_attribute_value_predicate() {
        let texts = query_text(
            b"<root><item type='a'>first</item><item type='b'>second</item></root>",
            "/root/item[@type='b']",
        );
        assert_eq!(texts, vec!["second"]);
    }

    #[test]
    fn test_contains_predicate() {
        let texts = query_text(
            b"<root><p>hello world</p><p>goodbye</p></root>",
            "/root/p[contains(., 'world')]",
        );
        assert_eq!(texts, vec!["hello world"]);
    }

    #[test]
    fn test_starts_with_predicate() {
        let texts = query_text(
            b"<root><p>hello world</p><p>goodbye</p></root>",
            "/root/p[starts-with(., 'hello')]",
        );
        assert_eq!(texts, vec!["hello world"]);
    }

    #[test]
    fn test_multiple_predicates() {
        let xml = b"<root><item type='a'>first</item><item type='b'>second</item><item type='a'>third</item></root>";
        // All items with type='a', then take the first
        let texts = query_text(xml, "/root/item[@type='a'][1]");
        assert_eq!(texts, vec!["first"]);
    }

    // --- Patent-specific tests ---

    #[test]
    fn test_patent_claims() {
        let xml = include_bytes!("../../../../testdata/small.xml");
        let texts = query_text(xml, "//claim");
        assert_eq!(texts.len(), 3);
        assert!(texts[0].contains("prosthetic arm device"));
    }

    #[test]
    fn test_patent_independent_claims() {
        let xml = include_bytes!("../../../../testdata/small.xml");
        let texts = query_text(xml, "//claim[@type='independent']");
        assert_eq!(texts.len(), 1);
        assert!(texts[0].contains("prosthetic arm device"));
    }

    #[test]
    fn test_patent_dependent_claims() {
        let xml = include_bytes!("../../../../testdata/small.xml");
        let texts = query_text(xml, "//claim[@type='dependent']");
        assert_eq!(texts.len(), 2);
    }

    #[test]
    fn test_patent_title() {
        let xml = include_bytes!("../../../../testdata/small.xml");
        let texts = query_text(xml, "/patent/title");
        assert_eq!(texts, vec!["Prosthetic Arm Device"]);
    }

    #[test]
    fn test_patent_description_paragraphs() {
        let xml = include_bytes!("../../../../testdata/small.xml");
        let texts = query_text(xml, "/patent/description/p");
        assert_eq!(texts.len(), 2);
    }

    #[test]
    fn test_patent_first_claim() {
        let xml = include_bytes!("../../../../testdata/small.xml");
        let texts = query_text(xml, "//claim[1]");
        assert_eq!(texts.len(), 1);
    }
}
