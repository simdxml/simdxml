use crate::error::{Result, SimdXmlError};
use crate::index::{TagType, XmlIndex};
use super::ast::*;
use super::parser::parse_xpath_predicate_expr;

/// Format a number matching libxml2's xmlXPathFormatNumber (%0.15g equivalent).
fn xpath_format_number(n: f64) -> String {
    if n.is_nan() { return "NaN".to_string(); }
    if n == f64::INFINITY { return "Infinity".to_string(); }
    if n == f64::NEG_INFINITY { return "-Infinity".to_string(); }

    // If it's an integer that fits exactly, format without decimal
    if n == n.trunc() && n.abs() < 1e15 {
        return format!("{}", n as i64);
    }

    // Use %0.15g equivalent: 15 significant digits, strip trailing zeros
    let abs_n = n.abs();
    let exp = if abs_n > 0.0 { abs_n.log10().floor() as i32 } else { 0 };

    if exp >= -4 && exp < 15 {
        // Fixed notation
        let decimal_digits = (14 - exp).max(0) as usize;
        let mut s = format!("{:.prec$}", n, prec = decimal_digits);
        if s.contains('.') {
            while s.ends_with('0') { s.pop(); }
            if s.ends_with('.') { s.pop(); }
        }
        s
    } else {
        // Scientific notation
        let mut s = format!("{:.14e}", n);
        // Rust uses e notation; insert + for positive exponent
        if let Some(e_pos) = s.find('e') {
            let exp_part = &s[e_pos + 1..];
            if !exp_part.starts_with('-') && !exp_part.starts_with('+') {
                s.insert(e_pos + 1, '+');
            }
        }
        // Strip trailing zeros in mantissa before 'e'
        if let Some(e_pos) = s.find('e') {
            let (mantissa, exp_str) = s.split_at(e_pos);
            let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
            s = format!("{}{}", mantissa, exp_str);
        }
        // Remove leading zeros from exponent (Rust may produce e+019 etc.)
        if let Some(e_pos) = s.find('e') {
            let sign_start = e_pos + 1;
            let (prefix, exp_part) = s.split_at(sign_start);
            let (sign, digits) = if exp_part.starts_with('-') || exp_part.starts_with('+') {
                (&exp_part[..1], exp_part[1..].trim_start_matches('0'))
            } else {
                ("", exp_part.trim_start_matches('0'))
            };
            let digits = if digits.is_empty() { "0" } else { digits };
            s = format!("{}{}{}",  prefix, sign, digits);
        }
        s
    }
}

/// A node in the XPath result set.
#[derive(Debug, Clone, Copy)]
pub enum XPathNode {
    Element(usize),              // index into tag_starts
    Text(usize),                 // index into text_ranges
    /// (tag_idx, attr_name_hash) — hash used for fast comparison
    Attribute(usize, u64),
    /// (owning_element_idx, prefix_hash) — namespace node
    Namespace(usize, u64),
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

/// Evaluate a predicate expression against a real document (from DOC_ROOT).
pub fn eval_expr_with_doc(index: &XmlIndex, expr_str: &str) -> Result<StandaloneResult> {
    eval_expr_with_context(index, expr_str, XPathNode::Element(DOC_ROOT))
}

/// Evaluate a predicate expression from a specific context node.
pub fn eval_expr_with_context(index: &XmlIndex, expr_str: &str, context: XPathNode) -> Result<StandaloneResult> {
    let parsed = parse_xpath_predicate_expr(expr_str)?;
    let value = eval_predicate_value(index, context, &parsed, 1, 1)?;
    Ok(match value {
        XPathValue::Number(n) => StandaloneResult::Number(n),
        XPathValue::String(s) => StandaloneResult::String(s),
        XPathValue::Boolean(b) => StandaloneResult::Boolean(b),
    })
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
            // Union results must be in document order, deduplicated
            dedup_nodes(&mut result);
            sort_doc_order(index, &mut result);
            Ok(result)
        }
        XPathExpr::FunctionCall(name, args) if name == "id" => {
            eval_id_function(index, args)
        }
        XPathExpr::FilterPath(inner, steps) => {
            let initial = evaluate(index, inner)?;
            let mut context = initial;
            for step in steps {
                context = eval_step(index, &context, step)?;
            }
            Ok(context)
        }
        XPathExpr::GlobalFilter(inner, preds) => {
            let mut result = evaluate(index, inner)?;
            for pred in preds {
                result = apply_predicate(index, &result, pred)?;
            }
            Ok(result)
        }
        _ => Err(SimdXmlError::XPathEvalError(
            "Only location paths, unions, and id() are supported".into(),
        )),
    }
}

/// Extract text results from XPath evaluation.
/// For element nodes, returns all descendant text (recursive).
/// For text nodes, returns the text content directly.
pub fn eval_text<'a>(
    index: &'a XmlIndex<'a>,
    expr: &XPathExpr,
) -> Result<Vec<&'a str>> {
    let nodes = evaluate(index, expr)?;
    let mut results = Vec::with_capacity(nodes.len());
    for node in nodes {
        match node {
            XPathNode::Element(idx) => {
                // Inline CSR text iteration — zero allocation per element
                let text_slice = index.child_text_slice(idx);
                if !text_slice.is_empty() {
                    for &ti in text_slice {
                        let text = index.text_content(&index.text_ranges[ti as usize]);
                        if !text.is_empty() {
                            results.push(text);
                        }
                    }
                } else {
                    // Fallback for small docs without CSR
                    for range in &index.text_ranges {
                        if range.parent_tag == idx as u32 {
                            let text = index.text_content(range);
                            if !text.is_empty() {
                                results.push(text);
                            }
                        }
                    }
                }
            }
            XPathNode::Text(idx) => {
                results.push(index.text_content(&index.text_ranges[idx]));
            }
            _ => {}
        }
    }
    Ok(results)
}

/// Sentinel index for the virtual document root.
const DOC_ROOT: usize = usize::MAX;

/// Evaluate id('value') — find element with matching ID attribute.
fn eval_id_function(index: &XmlIndex, args: &[XPathExpr]) -> Result<Vec<XPathNode>> {
    let id_value = match args.first() {
        Some(XPathExpr::StringLiteral(s)) => s.clone(),
        _ => return Ok(vec![]),
    };

    // Search all elements for an "id" attribute matching the value
    for i in 0..index.tag_count() {
        if index.tag_types[i] == TagType::Open || index.tag_types[i] == TagType::SelfClose {
            if let Some(val) = index.get_attribute(i, "id") {
                if val == id_value {
                    return Ok(vec![XPathNode::Element(i)]);
                }
            }
        }
    }

    Ok(vec![])
}

fn eval_location_path<'a>(
    index: &'a XmlIndex<'a>,
    path: &LocationPath,
) -> Result<Vec<XPathNode>> {
    let mut context: Vec<XPathNode> = if path.absolute {
        vec![XPathNode::Element(DOC_ROOT)]
    } else {
        // XPath 1.0 §2: initial context node is the root of the document tree.
        // For relative paths at the top level, start from the document root node,
        // not the document element. child::x from doc root finds the document element.
        vec![XPathNode::Element(DOC_ROOT)]
    };

    // Fuse steps: look ahead for optimizable patterns
    let steps = &path.steps;
    let mut i = 0;
    while i < steps.len() {
        // Pattern: DescendantOrSelf::node() + child::Name(x) [no predicates on desc step]
        //   → fused descendant scan (avoids materializing mega intermediate set)
        if i + 1 < steps.len()
            && steps[i].axis == Axis::DescendantOrSelf
            && steps[i].node_test == NodeTest::Node
            && steps[i].predicates.is_empty()
            && steps[i + 1].axis == Axis::Child
        {
            if steps[i + 1].predicates.is_empty() {
                context = eval_fused_descendant_child(index, &context, &steps[i + 1])?;
            } else {
                // Fused scan + per-parent predicate application
                context = eval_fused_descendant_child_with_preds(
                    index, &context, &steps[i + 1],
                )?;
            }
            i += 2;
        } else {
            context = eval_step(index, &context, &steps[i])?;
            i += 1;
        }
    }

    Ok(context)
}

/// Fused descendant-or-self::node()/child::test[preds] — single pass over all tags.
/// For `//claim`, this scans once for all elements named "claim" instead of
/// materializing every node as an intermediate context set.
fn eval_fused_descendant_child(
    index: &XmlIndex,
    context: &[XPathNode],
    child_step: &Step,
) -> Result<Vec<XPathNode>> {
    let mut result = Vec::new();

    for &ctx_node in context {
        let (scan_start, scan_end) = match ctx_node {
            XPathNode::Element(DOC_ROOT) => (0, index.tag_count()),
            XPathNode::Element(idx) if idx < index.tag_count() => {
                let close = index.matching_close(idx).unwrap_or(index.tag_count());
                (idx, close)
            }
            _ => continue,
        };

        // Single scan: push directly to result (no predicates in fused path)
        match &child_step.node_test {
            NodeTest::Name(name) => {
                // Use inverted index if available and scope is full document
                let posting = index.tags_by_name(name);
                if !posting.is_empty() && scan_start == 0 && scan_end == index.tag_count() {
                    result.extend(posting.iter().map(|&j| XPathNode::Element(j as usize)));
                } else if !posting.is_empty() {
                    let lo = posting.partition_point(|&j| (j as usize) < scan_start);
                    let hi = posting.partition_point(|&j| (j as usize) < scan_end);
                    result.extend(posting[lo..hi].iter().map(|&j| XPathNode::Element(j as usize)));
                } else {
                    for j in scan_start..scan_end {
                        let tt = index.tag_types[j];
                        if (tt == TagType::Open || tt == TagType::SelfClose)
                            && index.tag_name_eq(j, name)
                        {
                            result.push(XPathNode::Element(j));
                        }
                    }
                }
            }
            NodeTest::Text => {
                for (ti, range) in index.text_ranges.iter().enumerate() {
                    let p = range.parent_tag as usize;
                    if p >= scan_start && p < scan_end {
                        result.push(XPathNode::Text(ti));
                    }
                }
            }
            NodeTest::Wildcard => {
                for j in scan_start..scan_end {
                    let tt = index.tag_types[j];
                    if tt == TagType::Open || tt == TagType::SelfClose {
                        result.push(XPathNode::Element(j));
                    }
                }
            }
            NodeTest::Node => {
                for j in scan_start..scan_end {
                    if is_node_tag(index.tag_types[j]) {
                        result.push(XPathNode::Element(j));
                    }
                }
                for (ti, range) in index.text_ranges.iter().enumerate() {
                    let p = range.parent_tag as usize;
                    if p >= scan_start && p < scan_end {
                        result.push(XPathNode::Text(ti));
                    }
                }
            }
            NodeTest::Comment => {
                for j in scan_start..scan_end {
                    if index.tag_types[j] == TagType::Comment {
                        result.push(XPathNode::Element(j));
                    }
                }
            }
            _ => {
                let desc = eval_descendant_axis(index, ctx_node, true);
                for dn in desc {
                    let children = eval_child_axis(index, dn);
                    for c in children {
                        if matches_node_test(index, c, &child_step.node_test) {
                            result.push(c);
                        }
                    }
                }
            }
        }
    }

    // Fused path produces results in document order; dedup only needed for multi-context
    if context.len() > 1 {
        dedup_nodes(&mut result);
        sort_doc_order(index, &mut result);
    }
    Ok(result)
}

/// Fused descendant scan WITH per-parent predicate application.
/// Finds all matching elements, groups by parent, applies predicates per group.
/// Avoids materializing the descendant-or-self mega-nodeset.
fn eval_fused_descendant_child_with_preds(
    index: &XmlIndex,
    context: &[XPathNode],
    child_step: &Step,
) -> Result<Vec<XPathNode>> {
    let mut result = Vec::new();

    for &ctx_node in context {
        let (scan_start, scan_end) = match ctx_node {
            XPathNode::Element(DOC_ROOT) => (0, index.tag_count()),
            XPathNode::Element(idx) if idx < index.tag_count() => {
                let close = index.matching_close(idx).unwrap_or(index.tag_count());
                (idx, close)
            }
            _ => continue,
        };

        // Collect all matching elements in one scan
        let mut all_matches: Vec<XPathNode> = Vec::new();
        match &child_step.node_test {
            NodeTest::Name(name) => {
                for j in scan_start..scan_end {
                    let tt = index.tag_types[j];
                    if (tt == TagType::Open || tt == TagType::SelfClose)
                        && index.tag_name_eq(j, name)
                    {
                        all_matches.push(XPathNode::Element(j));
                    }
                }
            }
            _ => {
                for j in scan_start..scan_end {
                    if matches_node_test(index, XPathNode::Element(j), &child_step.node_test) {
                        all_matches.push(XPathNode::Element(j));
                    }
                }
            }
        }

        // Group by parent, apply predicates per group.
        // (XPath //p[1] means "first p child of EACH parent")
        // Must handle non-consecutive same-parent elements (e.g., <a><p/><b><p/></b><p/></a>
        // where the first and third <p/> share parent <a> but aren't adjacent in scan order).
        let mut parent_groups: std::collections::HashMap<u32, Vec<XPathNode>> =
            std::collections::HashMap::new();
        for &m in &all_matches {
            let parent = match m {
                XPathNode::Element(idx) if idx < index.tag_count() => index.parents[idx],
                _ => u32::MAX,
            };
            parent_groups.entry(parent).or_default().push(m);
        }
        for (_parent, mut group) in parent_groups {
            for pred in &child_step.predicates {
                group = apply_predicate(index, &group, pred)?;
            }
            result.extend(group);
        }
    }

    // Always sort — HashMap grouping doesn't preserve document order
    dedup_nodes(&mut result);
    sort_doc_order(index, &mut result);
    Ok(result)
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
            Axis::Namespace => eval_namespace_axis(index, node),
        };

        // Filter by node test (skip for attribute/namespace axes which pre-filter)
        let mut matched: Vec<XPathNode> = if step.axis == Axis::Attribute || step.axis == Axis::Namespace {
            candidates
        } else {
            candidates.into_iter()
                .filter(|c| matches_node_test(index, *c, &step.node_test))
                .collect()
        };

        // Apply predicates per-context-node
        for pred in &step.predicates {
            matched = apply_predicate(index, &matched, pred)?;
        }

        result.extend(matched);
    }

    // XPath node sets are always unique, in document order.
    // Skip dedup/sort for single-context-node (already unique and ordered).
    if context.len() > 1 {
        dedup_nodes(&mut result);
        sort_doc_order(index, &mut result);
    }

    Ok(result)
}

fn dedup_nodes(nodes: &mut Vec<XPathNode>) {
    let mut seen = std::collections::HashSet::new();
    nodes.retain(|n| {
        let key = match n {
            XPathNode::Element(idx) => (0, *idx),
            XPathNode::Text(idx) => (1, *idx),
            XPathNode::Attribute(idx, _h) => (2, *idx),
            XPathNode::Namespace(idx, h) => (3, *idx ^ (*h as usize)),
        };
        seen.insert(key)
    });
}

fn node_doc_pos(index: &XmlIndex, node: &XPathNode) -> u32 {
    match node {
        XPathNode::Element(idx) if *idx < index.tag_count() => index.tag_starts[*idx],
        XPathNode::Text(idx) if *idx < index.text_ranges.len() => index.text_ranges[*idx].start,
        XPathNode::Attribute(idx, _) if *idx < index.tag_count() => index.tag_starts[*idx],
        XPathNode::Namespace(idx, _) if *idx < index.tag_count() => index.tag_starts[*idx],
        _ => u32::MAX,
    }
}

fn sort_doc_order(index: &XmlIndex, nodes: &mut Vec<XPathNode>) {
    nodes.sort_by_key(|n| node_doc_pos(index, n));
}

/// Apply a predicate to filter a node set.
fn apply_predicate<'a>(
    index: &'a XmlIndex<'a>,
    nodes: &[XPathNode],
    pred: &XPathExpr,
) -> Result<Vec<XPathNode>> {
    match pred {
        // Numeric predicate: [N] is true when position() == N (exact equality)
        XPathExpr::NumberLiteral(n) => {
            let n = *n;
            if n.is_nan() || n.is_infinite() || n < 1.0 || n > nodes.len() as f64 || n != n.trunc() {
                Ok(vec![])
            } else {
                let pos = n as usize;
                if pos >= 1 && pos <= nodes.len() {
                    Ok(vec![nodes[pos - 1]])
                } else {
                    Ok(vec![])
                }
            }
        }

        // Unary minus in predicate: [-N] means position at negative index = empty
        XPathExpr::UnaryMinus(inner) => {
            if nodes.is_empty() {
                return Ok(vec![]);
            }
            // Evaluate the inner expression as a number
            let val = eval_predicate_value(index, nodes[0], inner, 1, nodes.len())?;
            let n = -(val.as_number());
            if n.is_nan() || n.is_infinite() || n < 1.0 || n > nodes.len() as f64 {
                Ok(vec![])
            } else {
                let pos = n.round() as usize;
                if pos >= 1 && pos <= nodes.len() {
                    Ok(vec![nodes[pos - 1]])
                } else {
                    Ok(vec![])
                }
            }
        }

        // Comparison: [@attr='value'], [position()=N], etc.
        XPathExpr::BinaryOp(left, op, right) => {
            let mut result = Vec::new();
            for (i, &node) in nodes.iter().enumerate() {
                // For or/and with LocationPath operands, evaluate as node-set existence
                let left_val = if matches!(op, BinaryOp::Or | BinaryOp::And) {
                    if let XPathExpr::LocationPath(_) = left.as_ref() {
                        let nodes = evaluate_in_context(index, node, left)?;
                        XPathValue::Boolean(!nodes.is_empty())
                    } else {
                        eval_predicate_value(index, node, left, i + 1, nodes.len())?
                    }
                } else {
                    eval_predicate_value(index, node, left, i + 1, nodes.len())?
                };
                let right_val = if matches!(op, BinaryOp::Or | BinaryOp::And) {
                    if let XPathExpr::LocationPath(_) = right.as_ref() {
                        let nodes = evaluate_in_context(index, node, right)?;
                        XPathValue::Boolean(!nodes.is_empty())
                    } else {
                        eval_predicate_value(index, node, right, i + 1, nodes.len())?
                    }
                } else {
                    eval_predicate_value(index, node, right, i + 1, nodes.len())?
                };
                if compare_values(&left_val, op, &right_val) {
                    result.push(node);
                }
            }
            Ok(result)
        }

        // Function call in predicate: if result is a number, treat as positional
        // (XPath 1.0 §3.4: if result is number, compare to position())
        XPathExpr::FunctionCall(name, args) => {
            let mut result = Vec::new();
            for (i, &node) in nodes.iter().enumerate() {
                let val = eval_function(index, node, name, args, i + 1, nodes.len())?;
                let keep = match &val {
                    XPathValue::Number(n) => {
                        // Numeric predicate: position() == n (exact)
                        let pos = (i + 1) as f64;
                        pos == *n
                    }
                    _ => val.is_truthy(),
                };
                if keep {
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
            XPathValue::Number(n) => xpath_format_number(*n),
            XPathValue::Boolean(b) => b.to_string(),
        }
    }

    fn as_number(&self) -> f64 {
        match self {
            XPathValue::Number(n) => *n,
            XPathValue::String(s) => s.trim().parse().unwrap_or(f64::NAN),
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
            // For comparison operators with LocationPath operands, implement
            // XPath 1.0 §3.4 node-set comparison semantics.
            let is_comparison = matches!(op, BinaryOp::Eq | BinaryOp::Neq
                | BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Lte | BinaryOp::Gte);
            let left_is_path = matches!(left.as_ref(), XPathExpr::LocationPath(_));
            let right_is_path = matches!(right.as_ref(), XPathExpr::LocationPath(_));

            if is_comparison && (left_is_path || right_is_path) {
                // Node-set comparison: if either side is a node-set,
                // compare each member. "nodeset = value" is true if ANY member matches.
                let left_nodes = if left_is_path {
                    evaluate_in_context(index, node, left)?
                } else {
                    vec![]
                };
                let right_nodes = if right_is_path {
                    evaluate_in_context(index, node, right)?
                } else {
                    vec![]
                };

                let result = if left_is_path && right_is_path {
                    // Both are node-sets: true if any pair of string values matches
                    let left_vals: Vec<String> = left_nodes.iter().map(|n| node_string_value(index, *n)).collect();
                    let right_vals: Vec<String> = right_nodes.iter().map(|n| node_string_value(index, *n)).collect();
                    left_vals.iter().any(|lv| right_vals.iter().any(|rv| {
                        compare_values(&XPathValue::String(lv.clone()), op, &XPathValue::String(rv.clone()))
                    }))
                } else if left_is_path {
                    let r = eval_predicate_value(index, node, right, position, size)?;
                    left_nodes.iter().any(|n| {
                        let lv = XPathValue::String(node_string_value(index, *n));
                        compare_values(&lv, op, &r)
                    })
                } else {
                    let l = eval_predicate_value(index, node, left, position, size)?;
                    right_nodes.iter().any(|n| {
                        let rv = XPathValue::String(node_string_value(index, *n));
                        compare_values(&l, op, &rv)
                    })
                };
                return Ok(XPathValue::Boolean(result));
            }

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
                // For LocationPath arguments (e.g., not(@x)), check node-set existence
                // rather than string truthiness. XPath 1.0 §4.3: boolean(node-set) is
                // true iff the node-set is non-empty.
                if let XPathExpr::LocationPath(_) = arg {
                    let nodes = evaluate_in_context(index, node, arg)?;
                    Ok(XPathValue::Boolean(nodes.is_empty()))
                } else {
                    let val = eval_predicate_value(index, node, arg, position, size)?;
                    Ok(XPathValue::Boolean(!val.is_truthy()))
                }
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
            // XPath substring(string, startPos [, length])
            // Positions are 1-indexed, round() is applied to startPos and length.
            // The returned string is chars from position round(startPos) to
            // round(startPos) + round(length) - 1.
            if args.len() >= 2 {
                let s = eval_predicate_value(index, node, &args[0], position, size)?.as_string();
                let start_raw = eval_predicate_value(index, node, &args[1], position, size)?.as_number();

                if start_raw.is_nan() {
                    return Ok(XPathValue::String(String::new()));
                }

                let chars: Vec<char> = s.chars().collect();

                if args.len() >= 3 {
                    let len_raw = eval_predicate_value(index, node, &args[2], position, size)?.as_number();

                    if len_raw.is_nan() || len_raw == f64::NEG_INFINITY {
                        return Ok(XPathValue::String(String::new()));
                    }

                    // XPath: substring(s, p, n) returns chars at positions
                    // where position >= round(p) and position < round(p) + round(n)
                    let p = start_raw.round();
                    let n = len_raw.round();
                    let end = p + n;

                    if end == f64::NEG_INFINITY || p == f64::INFINITY {
                        return Ok(XPathValue::String(String::new()));
                    }

                    let start_idx = (p.max(1.0) as i64 - 1).max(0) as usize;
                    let end_idx = if end.is_infinite() {
                        chars.len()
                    } else {
                        ((end as i64 - 1).max(0) as usize).min(chars.len())
                    };

                    if start_idx >= end_idx || start_idx >= chars.len() {
                        Ok(XPathValue::String(String::new()))
                    } else {
                        Ok(XPathValue::String(chars[start_idx..end_idx].iter().collect()))
                    }
                } else {
                    // No length — from start to end
                    let start_idx = (start_raw.round().max(1.0) as i64 - 1).max(0) as usize;
                    if start_idx >= chars.len() {
                        Ok(XPathValue::String(String::new()))
                    } else {
                        Ok(XPathValue::String(chars[start_idx..].iter().collect()))
                    }
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
            if let Some(arg) = args.first() {
                let nodes = evaluate_in_context(index, node, arg)?;
                let total: f64 = nodes.iter()
                    .map(|n| node_string_value(index, *n).parse::<f64>().unwrap_or(0.0))
                    .sum();
                Ok(XPathValue::Number(total))
            } else {
                Ok(XPathValue::Number(0.0))
            }
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
            if let Some(arg) = args.first() {
                // For LocationPath arguments, check node-set non-emptiness
                if let XPathExpr::LocationPath(_) = arg {
                    let nodes = evaluate_in_context(index, node, arg)?;
                    Ok(XPathValue::Boolean(!nodes.is_empty()))
                } else {
                    let val = eval_predicate_value(index, node, arg, position, size)?;
                    Ok(XPathValue::Boolean(val.is_truthy()))
                }
            } else {
                Ok(XPathValue::Boolean(false))
            }
        }
        "lang" => {
            // lang(string) — true if context node's xml:lang matches the argument.
            // Walks ancestors looking for xml:lang attribute, case-insensitive match.
            if let Some(arg) = args.first() {
                let target = eval_predicate_value(index, node, arg, position, size)?.as_string();
                if target.is_empty() {
                    return Ok(XPathValue::Boolean(false));
                }
                let target_lower = target.to_ascii_lowercase();
                // Walk node and ancestors looking for xml:lang
                let mut current = match node {
                    XPathNode::Element(idx) if idx != DOC_ROOT => Some(idx),
                    _ => None,
                };
                while let Some(idx) = current {
                    if let Some(lang_val) = index.get_attribute(idx, "xml:lang") {
                        let lang_lower = lang_val.to_ascii_lowercase();
                        let matches = lang_lower == target_lower
                            || (lang_lower.starts_with(&target_lower)
                                && lang_lower.as_bytes().get(target_lower.len()) == Some(&b'-'));
                        return Ok(XPathValue::Boolean(matches));
                    }
                    let parent = index.parents[idx];
                    current = if parent != u32::MAX { Some(parent as usize) } else { None };
                }
                Ok(XPathValue::Boolean(false))
            } else {
                Ok(XPathValue::Boolean(false))
            }
        }
        _ => Ok(XPathValue::String(String::new())),
    }
}

fn compare_values(left: &XPathValue, op: &BinaryOp, right: &XPathValue) -> bool {
    match op {
        BinaryOp::Eq => {
            // XPath equality rules (sec 3.4):
            // If either is boolean, convert other to boolean and compare
            // If either is number, convert other to number and compare
            // Otherwise compare as strings
            match (left, right) {
                (XPathValue::Boolean(a), _) => *a == right.is_truthy(),
                (_, XPathValue::Boolean(b)) => left.is_truthy() == *b,
                _ if matches!(left, XPathValue::Number(_)) || matches!(right, XPathValue::Number(_)) => {
                    let ln = left.as_number();
                    let rn = right.as_number();
                    if ln.is_nan() || rn.is_nan() {
                        false // NaN != NaN
                    } else {
                        ln == rn
                    }
                }
                _ => left.as_string() == right.as_string(),
            }
        }
        BinaryOp::Neq => !compare_values(left, &BinaryOp::Eq, right),
        BinaryOp::Lt => left.as_number() < right.as_number(),
        BinaryOp::Gt => left.as_number() > right.as_number(),
        BinaryOp::Lte => left.as_number() <= right.as_number(),
        BinaryOp::Gte => left.as_number() >= right.as_number(),
        BinaryOp::Or => left.is_truthy() || right.is_truthy(),
        BinaryOp::And => left.is_truthy() && right.is_truthy(),
        _ => false,
    }
}

fn node_string_value(index: &XmlIndex, node: XPathNode) -> String {
    match node {
        XPathNode::Element(idx) if idx != DOC_ROOT => {
            XmlIndex::decode_entities(&index.all_text(idx)).into_owned()
        }
        XPathNode::Text(idx) => {
            XmlIndex::decode_entities(index.text_content(&index.text_ranges[idx])).into_owned()
        }
        XPathNode::Attribute(tag_idx, name_hash) => {
            // Find the attribute by matching the name hash against all attrs on this tag
            for attr_name in index.get_all_attribute_names(tag_idx) {
                if attr_name_hash(attr_name) == name_hash {
                    if let Some(val) = index.get_attribute(tag_idx, attr_name) {
                        return val.to_string();
                    }
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

/// Evaluate an expression in the context of a specific node.
/// Evaluate an expression from a specific context node (public API).
pub fn evaluate_from_context(
    index: &XmlIndex,
    expr: &XPathExpr,
    context_node: XPathNode,
) -> Result<Vec<XPathNode>> {
    match expr {
        XPathExpr::LocationPath(path) if !path.absolute => {
            let mut context = vec![context_node];
            // Use the same step fusion as eval_location_path
            let steps = &path.steps;
            let mut i = 0;
            while i < steps.len() {
                if i + 1 < steps.len()
                    && steps[i].axis == Axis::DescendantOrSelf
                    && steps[i].node_test == NodeTest::Node
                    && steps[i].predicates.is_empty()
                    && steps[i + 1].axis == Axis::Child
                {
                    if steps[i + 1].predicates.is_empty() {
                        context = eval_fused_descendant_child(index, &context, &steps[i + 1])?;
                    } else {
                        context = eval_fused_descendant_child_with_preds(index, &context, &steps[i + 1])?;
                    }
                    i += 2;
                } else {
                    context = eval_step(index, &context, &steps[i])?;
                    i += 1;
                }
            }
            Ok(context)
        }
        XPathExpr::LocationPath(_) => evaluate(index, expr),
        XPathExpr::Union(exprs) => {
            let mut result = Vec::new();
            for e in exprs {
                result.extend(evaluate_from_context(index, e, context_node)?);
            }
            dedup_nodes(&mut result);
            sort_doc_order(index, &mut result);
            Ok(result)
        }
        _ => evaluate(index, expr),
    }
}

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
        XPathExpr::LocationPath(_) => {
            // Absolute path: evaluate from document root
            evaluate(index, expr)
        }
        _ => Ok(vec![]),
    }
}

#[inline]
fn matches_node_test(index: &XmlIndex, node: XPathNode, test: &NodeTest) -> bool {
    match (node, test) {
        (_, NodeTest::Node) => true,
        (XPathNode::Element(idx), NodeTest::Wildcard) => {
            // Wildcard matches elements (Open/SelfClose) but NOT DOC_ROOT, comments, PIs, CData
            idx != DOC_ROOT && idx < index.tag_count()
                && (index.tag_types[idx] == TagType::Open || index.tag_types[idx] == TagType::SelfClose)
        }
        (XPathNode::Namespace(_, _), NodeTest::Wildcard) => true,
        (XPathNode::Attribute(_, _), NodeTest::Wildcard) => true,
        (XPathNode::Text(_), NodeTest::Text) => true,
        (XPathNode::Element(idx), NodeTest::Name(name)) if idx < index.tag_count() => {
            // Name test matches only elements (Open/SelfClose), not PI/Comment/CData
            (index.tag_types[idx] == TagType::Open || index.tag_types[idx] == TagType::SelfClose)
                && index.tag_name_eq(idx, name)
        }
        (XPathNode::Element(idx), NodeTest::Comment) if idx < index.tag_count() => {
            index.tag_types[idx] == TagType::Comment
        }
        (XPathNode::Element(idx), NodeTest::PI) if idx < index.tag_count() => {
            index.tag_types[idx] == TagType::PI
        }
        // Attribute name test: only matches in attribute axis context.
        // This is handled by eval_attribute_axis — if we get here from
        // another axis (self, ancestor-or-self), attributes don't match
        // element name tests.
        (XPathNode::Attribute(_, _), NodeTest::Name(_)) => false,
        // Namespace nodes: match by prefix name or wildcard
        (XPathNode::Namespace(_, hash), NodeTest::Name(name)) => hash == attr_name_hash(name),
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

    if parent_idx == DOC_ROOT {
        // Document root's children are depth-0 elements/comments/PIs
        let mut children_with_pos: Vec<(u32, XPathNode)> = Vec::new();
        for i in 0..index.tag_count() {
            if index.depths[i] == 0 && is_node_tag(index.tag_types[i])
                && index.tag_types[i] != TagType::Close
            {
                children_with_pos.push((index.tag_starts[i], XPathNode::Element(i)));
            }
        }
        children_with_pos.sort_by_key(|(pos, _)| *pos);
        return children_with_pos.into_iter().map(|(_, node)| node).collect();
    }

    // Use precomputed CSR child indices — O(children_count) instead of O(N)
    // Falls back to linear scan for small documents (no CSR built).
    if !index.has_indices() {
        // Linear scan fallback for small documents
        let mut children_with_pos: Vec<(u32, XPathNode)> = Vec::new();
        for i in 0..index.tag_count() {
            if index.parents[i] == parent_idx as u32 && is_node_tag(index.tag_types[i]) {
                children_with_pos.push((index.tag_starts[i], XPathNode::Element(i)));
            }
        }
        for (i, range) in index.text_ranges.iter().enumerate() {
            if range.parent_tag == parent_idx as u32 {
                children_with_pos.push((range.start, XPathNode::Text(i)));
            }
        }
        children_with_pos.sort_by_key(|(pos, _)| *pos);
        return children_with_pos.into_iter().map(|(_, node)| node).collect();
    }

    let tags = index.child_tag_slice(parent_idx);
    let texts = index.child_text_slice(parent_idx);

    // Fast path: no text children → just return tag children (already in doc order)
    if texts.is_empty() {
        return tags.iter().map(|&i| XPathNode::Element(i as usize)).collect();
    }

    // Merge tag children and text children in document order
    let mut result = Vec::with_capacity(tags.len() + texts.len());
    let mut ti = 0;
    let mut xi = 0;
    while ti < tags.len() && xi < texts.len() {
        let tag_pos = index.tag_starts[tags[ti] as usize];
        let txt_pos = index.text_ranges[texts[xi] as usize].start;
        if tag_pos < txt_pos {
            result.push(XPathNode::Element(tags[ti] as usize));
            ti += 1;
        } else {
            result.push(XPathNode::Text(texts[xi] as usize));
            xi += 1;
        }
    }
    while ti < tags.len() {
        result.push(XPathNode::Element(tags[ti] as usize));
        ti += 1;
    }
    while xi < texts.len() {
        result.push(XPathNode::Text(texts[xi] as usize));
        xi += 1;
    }
    result
}

#[inline(always)]
fn is_node_tag(tt: TagType) -> bool {
    // CData is not included: its content is already a text range (text node).
    matches!(tt, TagType::Open | TagType::SelfClose | TagType::Comment | TagType::PI)
}

fn eval_descendant_axis(index: &XmlIndex, node: XPathNode, include_self: bool) -> Vec<XPathNode> {
    let XPathNode::Element(start_idx) = node else {
        return if include_self { vec![node] } else { vec![] };
    };

    if start_idx == DOC_ROOT {
        // Descendants of document root = all node types, in document order
        let mut items: Vec<(u32, XPathNode)> = Vec::new();
        if include_self {
            items.push((0, XPathNode::Element(DOC_ROOT)));
        }
        for i in 0..index.tag_count() {
            if is_node_tag(index.tag_types[i]) {
                items.push((index.tag_starts[i], XPathNode::Element(i)));
            }
        }
        for i in 0..index.text_ranges.len() {
            // Skip root-level text (whitespace between PI and root element)
            if index.text_ranges[i].parent_tag == u32::MAX {
                continue;
            }
            items.push((index.text_ranges[i].start, XPathNode::Text(i)));
        }
        items.sort_by_key(|(pos, _)| *pos);
        return items.into_iter().map(|(_, node)| node).collect();
    }

    let mut items: Vec<(u32, XPathNode)> = Vec::new();
    if include_self {
        items.push((index.tag_starts[start_idx], node));
    }

    let close_idx = index.matching_close(start_idx).unwrap_or(index.tag_count());
    for i in (start_idx + 1)..close_idx {
        if is_node_tag(index.tag_types[i]) {
            items.push((index.tag_starts[i], XPathNode::Element(i)));
        }
    }

    // Descendant text nodes
    for (i, range) in index.text_ranges.iter().enumerate() {
        let parent = range.parent_tag as usize;
        if parent >= start_idx && parent < close_idx {
            items.push((range.start, XPathNode::Text(i)));
        }
    }

    items.sort_by_key(|(pos, _)| *pos);
    items.into_iter().map(|(_, node)| node).collect()
}

fn eval_parent_axis(index: &XmlIndex, node: XPathNode) -> Vec<XPathNode> {
    match node {
        XPathNode::Element(idx) if idx == DOC_ROOT => vec![],
        XPathNode::Element(idx) => {
            let parent = index.parents[idx];
            if parent != u32::MAX {
                vec![XPathNode::Element(parent as usize)]
            } else {
                // Root element's parent is the document root
                vec![XPathNode::Element(DOC_ROOT)]
            }
        }
        XPathNode::Text(idx) => {
            let parent = index.text_ranges[idx].parent_tag;
            if parent != u32::MAX {
                vec![XPathNode::Element(parent as usize)]
            } else {
                vec![XPathNode::Element(DOC_ROOT)]
            }
        }
        XPathNode::Attribute(tag_idx, _) => vec![XPathNode::Element(tag_idx)],
        XPathNode::Namespace(elem_idx, _) => vec![XPathNode::Element(elem_idx)],
    }
}

fn eval_ancestor_axis(index: &XmlIndex, node: XPathNode, include_self: bool) -> Vec<XPathNode> {
    let mut result = Vec::new();
    if include_self {
        result.push(node);
    }

    let mut current = match node {
        XPathNode::Element(idx) if idx == DOC_ROOT => u32::MAX,
        XPathNode::Element(idx) if idx < index.tag_count() => index.parents[idx],
        XPathNode::Text(idx) => index.text_ranges[idx].parent_tag,
        XPathNode::Attribute(tag_idx, _) if tag_idx < index.tag_count() => tag_idx as u32,
        XPathNode::Namespace(elem_idx, _) if elem_idx < index.tag_count() => elem_idx as u32,
        _ => u32::MAX,
    };

    while current != u32::MAX && (current as usize) < index.tag_count() {
        result.push(XPathNode::Element(current as usize));
        current = index.parents[current as usize];
    }

    // Include the document root node as the ultimate ancestor
    if !matches!(node, XPathNode::Element(DOC_ROOT)) {
        result.push(XPathNode::Element(DOC_ROOT));
    }

    result
}

fn eval_following_sibling_axis(index: &XmlIndex, node: XPathNode) -> Vec<XPathNode> {
    let (idx, parent_tag) = match node {
        XPathNode::Element(i) if i == DOC_ROOT || i >= index.tag_count() => return vec![],
        XPathNode::Element(i) => (i, index.parents[i]),
        XPathNode::Text(i) => {
            let p = index.text_ranges[i].parent_tag;
            // Return sibling tags and text nodes after this text node
            let mut result = Vec::new();
            let my_pos = index.text_ranges[i].start;
            if p != u32::MAX {
                let parent_idx = p as usize;
                // Sibling tags
                for &child in index.child_tag_slice(parent_idx) {
                    if index.tag_starts[child as usize] > my_pos {
                        result.push(XPathNode::Element(child as usize));
                    }
                }
                // Sibling text nodes
                for &ti in index.child_text_slice(parent_idx) {
                    if index.text_ranges[ti as usize].start > my_pos {
                        result.push(XPathNode::Text(ti as usize));
                    }
                }
                sort_doc_order(index, &mut result);
            }
            return result;
        }
        _ => return vec![],
    };

    let mut result = Vec::new();
    // Use CSR if available for element siblings
    if index.has_indices() && parent_tag != u32::MAX {
        let parent_idx = parent_tag as usize;
        for &child in index.child_tag_slice(parent_idx) {
            if (child as usize) > idx {
                result.push(XPathNode::Element(child as usize));
            }
        }
        // Include sibling text nodes
        let my_pos = index.tag_starts[idx];
        for &ti in index.child_text_slice(parent_idx) {
            if index.text_ranges[ti as usize].start > my_pos {
                result.push(XPathNode::Text(ti as usize));
            }
        }
        sort_doc_order(index, &mut result);
    } else {
        let depth = index.depths[idx];
        for i in (idx + 1)..index.tag_count() {
            if index.parents[i] == parent_tag
                && index.depths[i] == depth
                && is_node_tag(index.tag_types[i])
            {
                result.push(XPathNode::Element(i));
            }
        }
    }

    result
}

fn eval_preceding_sibling_axis(index: &XmlIndex, node: XPathNode) -> Vec<XPathNode> {
    let (idx, parent_tag) = match node {
        XPathNode::Element(i) if i == DOC_ROOT || i >= index.tag_count() => return vec![],
        XPathNode::Element(i) => (i, index.parents[i]),
        XPathNode::Text(i) => {
            let p = index.text_ranges[i].parent_tag;
            let mut result = Vec::new();
            let my_pos = index.text_ranges[i].start;
            if p != u32::MAX {
                let parent_idx = p as usize;
                for &child in index.child_tag_slice(parent_idx) {
                    if index.tag_starts[child as usize] < my_pos {
                        result.push(XPathNode::Element(child as usize));
                    }
                }
                for &ti in index.child_text_slice(parent_idx) {
                    if index.text_ranges[ti as usize].start < my_pos {
                        result.push(XPathNode::Text(ti as usize));
                    }
                }
                sort_doc_order(index, &mut result);
            }
            return result;
        }
        _ => return vec![],
    };

    let mut result = Vec::new();
    if index.has_indices() && parent_tag != u32::MAX {
        let parent_idx = parent_tag as usize;
        for &child in index.child_tag_slice(parent_idx) {
            if (child as usize) < idx {
                result.push(XPathNode::Element(child as usize));
            }
        }
        let my_pos = index.tag_starts[idx];
        for &ti in index.child_text_slice(parent_idx) {
            if index.text_ranges[ti as usize].start < my_pos {
                result.push(XPathNode::Text(ti as usize));
            }
        }
        sort_doc_order(index, &mut result);
    } else {
        let depth = index.depths[idx];
        for i in (0..idx).rev() {
            if index.parents[i] == parent_tag
                && index.depths[i] == depth
                && is_node_tag(index.tag_types[i])
            {
                result.push(XPathNode::Element(i));
            }
        }
    }

    result
}

fn eval_following_axis(index: &XmlIndex, node: XPathNode) -> Vec<XPathNode> {
    let idx = match node {
        XPathNode::Element(i) => i,
        XPathNode::Namespace(i, _) | XPathNode::Attribute(i, _) => i,
        _ => return vec![],
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
    let idx = match node {
        XPathNode::Element(i) => i,
        XPathNode::Namespace(i, _) | XPathNode::Attribute(i, _) => i,
        _ => return vec![],
    };
    if idx == DOC_ROOT || idx >= index.tag_count() {
        return vec![];
    }

    let mut result = Vec::new();

    // Collect in document order: all elements before this one, excluding ancestors.
    if !index.post_order.is_empty() {
        // O(1) is_ancestor check via pre/post numbering
        for i in 0..idx {
            if (index.tag_types[i] == TagType::Open || index.tag_types[i] == TagType::SelfClose)
                && !index.is_ancestor(i, idx)
            {
                result.push(XPathNode::Element(i));
            }
        }
    } else {
        // Fallback: build ancestor set via parent chain
        let mut ancestors = std::collections::HashSet::new();
        let mut current = index.parents[idx];
        while current != u32::MAX {
            ancestors.insert(current);
            current = index.parents[current as usize];
        }
        for i in 0..idx {
            if (index.tag_types[i] == TagType::Open || index.tag_types[i] == TagType::SelfClose)
                && !ancestors.contains(&(i as u32))
            {
                result.push(XPathNode::Element(i));
            }
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
                vec![XPathNode::Attribute(idx, attr_name_hash(name))]
            } else {
                vec![]
            }
        }
        NodeTest::Wildcard | NodeTest::Node => {
            // Return all attributes
            index.get_all_attribute_names(idx).iter()
                .map(|name| XPathNode::Attribute(idx, attr_name_hash(name)))
                .collect()
        }
        _ => vec![],
    }
}

/// Namespace axis: returns in-scope namespace nodes for an element.
/// Walks ancestors to collect inherited xmlns: declarations, plus the built-in `xml` namespace.
fn eval_namespace_axis(index: &XmlIndex, node: XPathNode) -> Vec<XPathNode> {
    let XPathNode::Element(idx) = node else {
        return vec![];
    };
    if idx == DOC_ROOT || idx >= index.tag_count() {
        return vec![];
    }

    // Collect all in-scope namespaces by walking up from this element.
    // Later declarations override earlier ones (closer ancestor wins).
    let mut ns_map: Vec<(String, u64)> = Vec::new();
    let mut seen_prefixes = std::collections::HashSet::new();

    let mut current = Some(idx);
    while let Some(cur_idx) = current {
        if cur_idx < index.tag_count()
            && (index.tag_types[cur_idx] == TagType::Open || index.tag_types[cur_idx] == TagType::SelfClose)
        {
            for (prefix, _uri) in index.get_namespace_decls(cur_idx) {
                if seen_prefixes.insert(prefix.to_string()) {
                    ns_map.push((prefix.to_string(), attr_name_hash(prefix)));
                }
            }
        }
        let parent = index.parents[cur_idx];
        current = if parent != u32::MAX { Some(parent as usize) } else { None };
    }

    // Always include the built-in `xml` namespace
    if seen_prefixes.insert("xml".to_string()) {
        ns_map.push(("xml".to_string(), attr_name_hash("xml")));
    }

    // Return namespace nodes (owned by this element)
    ns_map.into_iter()
        .map(|(_, hash)| XPathNode::Namespace(idx, hash))
        .collect()
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
        assert_eq!(ancestors.len(), 3); // b, a, and document root
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
