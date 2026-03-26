use nom::{
    branch::alt,
    bytes::complete::{tag, take_while1},
    character::complete::{char, multispace0},
    sequence::{delimited, pair, preceded},
    IResult,
};

use super::ast::*;
use crate::error::{Result, SimdXmlError};

/// Parse a standalone predicate expression (for arithmetic, string, boolean evaluation).
pub fn parse_xpath_predicate_expr(input: &str) -> Result<XPathExpr> {
    let input = input.trim();
    match predicate_expr(input) {
        Ok(("", expr)) => Ok(expr),
        Ok((rest, _)) => Err(SimdXmlError::XPathParseError(format!(
            "Unexpected trailing input: '{rest}'"
        ))),
        Err(e) => Err(SimdXmlError::XPathParseError(format!("{e}"))),
    }
}

/// Parse an XPath 1.0 expression string.
pub fn parse_xpath(input: &str) -> Result<XPathExpr> {
    let input = input.trim();
    match xpath_expr(input) {
        Ok(("", expr)) => Ok(expr),
        Ok((rest, _)) => Err(SimdXmlError::XPathParseError(format!(
            "Unexpected trailing input: '{rest}'"
        ))),
        Err(e) => Err(SimdXmlError::XPathParseError(format!("{e}"))),
    }
}

fn xpath_expr(input: &str) -> IResult<&str, XPathExpr> {
    alt((parenthesized_filter, function_path_expr, union_expr, location_path_expr))(input)
}

/// Function call optionally followed by a path: id('x')/p[1]
fn function_path_expr(input: &str) -> IResult<&str, XPathExpr> {
    let (input, func) = function_call_expr(input)?;
    // Check for following path steps (e.g., id('x')/p or id('x')//p)
    if input.starts_with('/') {
        let (input, steps) = parse_continuation_steps(input)?;
        if steps.is_empty() {
            Ok((input, func))
        } else {
            Ok((input, XPathExpr::FilterPath(Box::new(func), steps)))
        }
    } else {
        // Check for predicates: id('x')[1]
        let (input, preds) = predicates(input)?;
        if preds.is_empty() {
            Ok((input, func))
        } else {
            Ok((input, XPathExpr::GlobalFilter(Box::new(func), preds)))
        }
    }
}

/// Parenthesized filter: (expr)[pred] or (expr)/path — evaluate expr, then filter or continue path
fn parenthesized_filter(input: &str) -> IResult<&str, XPathExpr> {
    let (input, _) = char('(')(input)?;
    let (input, _) = multispace0(input)?;
    let (input, inner) = xpath_expr(input)?;
    let (input, _) = multispace0(input)?;
    let (input, _) = char(')')(input)?;

    // Check for path continuation: (.)/foo or (.)//foo
    if input.starts_with('/') {
        let (input, steps) = parse_continuation_steps(input)?;
        if steps.is_empty() {
            Ok((input, inner))
        } else {
            // Check for predicates after the path steps
            let (input, preds) = predicates(input)?;
            let expr = XPathExpr::FilterPath(Box::new(inner), steps);
            if preds.is_empty() {
                Ok((input, expr))
            } else {
                Ok((input, XPathExpr::GlobalFilter(Box::new(expr), preds)))
            }
        }
    } else {
        let (input, preds) = predicates(input)?;
        if preds.is_empty() {
            // Just parenthesized, no filter
            Ok((input, inner))
        } else {
            // GlobalFilter: evaluate inner, then apply predicates to the whole result set
            Ok((input, XPathExpr::GlobalFilter(Box::new(inner), preds)))
        }
    }
}

fn union_expr(input: &str) -> IResult<&str, XPathExpr> {
    let (input, first) = location_path_expr(input)?;
    let (input, rest) = nom::multi::many0(preceded(
        delimited(multispace0, char('|'), multispace0),
        location_path_expr,
    ))(input)?;

    if rest.is_empty() {
        Ok((input, first))
    } else {
        let mut all = vec![first];
        all.extend(rest);
        Ok((input, XPathExpr::Union(all)))
    }
}

/// Parse continuation steps after a primary expression: /p, //p, /p[1]/q
fn parse_continuation_steps(input: &str) -> IResult<&str, Vec<Step>> {
    let mut steps = Vec::new();
    let mut input = input;

    loop {
        if input.starts_with("//") {
            input = &input[2..];
            steps.push(Step {
                axis: Axis::DescendantOrSelf,
                node_test: NodeTest::Node,
                predicates: vec![],
            });
            let (rest, s) = step(input)?;
            steps.push(s);
            input = rest;
        } else if input.starts_with('/') {
            input = &input[1..];
            if input.is_empty() || input.starts_with('|') || input.starts_with(')') {
                break;
            }
            let (rest, s) = step(input)?;
            steps.push(s);
            input = rest;
        } else {
            break;
        }
    }

    Ok((input, steps))
}

fn location_path_expr(input: &str) -> IResult<&str, XPathExpr> {
    let (input, path) = location_path(input)?;
    Ok((input, XPathExpr::LocationPath(path)))
}

fn location_path(input: &str) -> IResult<&str, LocationPath> {
    alt((absolute_path, abbreviated_descendant_path, relative_path))(input)
}

/// Absolute path: /step/step/...
fn absolute_path(input: &str) -> IResult<&str, LocationPath> {
    let (input, _) = char('/')(input)?;

    // Check for // at root
    if input.starts_with('/') {
        let input = &input[1..]; // consume second /
        let (input, first) = step(input)?;
        let mut steps = vec![
            Step {
                axis: Axis::DescendantOrSelf,
                node_test: NodeTest::Node,
                predicates: vec![],
            },
            first,
        ];
        // Parse remaining steps with // support
        let (input, more) = parse_continuation_steps(input)?;
        steps.extend(more);
        Ok((
            input,
            LocationPath {
                absolute: true,
                steps,
            },
        ))
    } else if input.is_empty() || input.starts_with('|') || input.starts_with(')') || input.starts_with(']') {
        // Bare / — select root
        Ok((
            input,
            LocationPath {
                absolute: true,
                steps: vec![],
            },
        ))
    } else {
        let (input, first) = step(input)?;
        let mut steps = vec![first];
        let (input, more) = parse_continuation_steps(input)?;
        steps.extend(more);
        Ok((
            input,
            LocationPath {
                absolute: true,
                steps,
            },
        ))
    }
}

/// Abbreviated descendant: //step/step/...
fn abbreviated_descendant_path(input: &str) -> IResult<&str, LocationPath> {
    let (input, _) = tag("//")(input)?;
    let (input, first) = step(input)?;
    let mut all_steps = vec![
        Step {
            axis: Axis::DescendantOrSelf,
            node_test: NodeTest::Node,
            predicates: vec![],
        },
        first,
    ];
    let (input, more) = parse_continuation_steps(input)?;
    all_steps.extend(more);
    Ok((
        input,
        LocationPath {
            absolute: true,
            steps: all_steps,
        },
    ))
}

/// Relative path: step/step/...
fn relative_path(input: &str) -> IResult<&str, LocationPath> {
    let (input, first) = step(input)?;
    let mut steps = vec![first];
    let (input, more) = parse_continuation_steps(input)?;
    steps.extend(more);
    Ok((
        input,
        LocationPath {
            absolute: false,
            steps,
        },
    ))
}

/// A single step: axis::nodetest[predicate]
fn step(input: &str) -> IResult<&str, Step> {
    let (input, _) = multispace0(input)?;

    // Check for abbreviated axes
    if input.starts_with("..") {
        let (input, _) = tag("..")(input)?;
        return Ok((
            input,
            Step {
                axis: Axis::Parent,
                node_test: NodeTest::Node,
                predicates: vec![],
            },
        ));
    }
    if input.starts_with('.') && !input[1..].starts_with('.') {
        let (input, _) = char('.')(input)?;
        return Ok((
            input,
            Step {
                axis: Axis::SelfAxis,
                node_test: NodeTest::Node,
                predicates: vec![],
            },
        ));
    }

    // Check for @attr (abbreviated attribute axis)
    if input.starts_with('@') {
        let (input, _) = char('@')(input)?;
        let (input, test) = node_test(input)?;
        let (input, preds) = predicates(input)?;
        return Ok((
            input,
            Step {
                axis: Axis::Attribute,
                node_test: test,
                predicates: preds,
            },
        ));
    }

    // Check for explicit axis:: syntax
    if let Ok((rest, axis)) = axis_specifier(input) {
        let (rest, test) = node_test(rest)?;
        let (rest, preds) = predicates(rest)?;
        return Ok((
            rest,
            Step {
                axis,
                node_test: test,
                predicates: preds,
            },
        ));
    }

    // Default: child axis
    let (input, test) = node_test(input)?;
    let (input, preds) = predicates(input)?;
    Ok((
        input,
        Step {
            axis: Axis::Child,
            node_test: test,
            predicates: preds,
        },
    ))
}

fn axis_specifier(input: &str) -> IResult<&str, Axis> {
    let (input, name) = take_while1(|c: char| c.is_alphanumeric() || c == '-')(input)?;
    let (input, _) = tag("::")(input)?;
    let (input, _) = multispace0(input)?;  // XPath allows whitespace after ::
    let axis = match name {
        "child" => Axis::Child,
        "descendant" => Axis::Descendant,
        "parent" => Axis::Parent,
        "ancestor" => Axis::Ancestor,
        "following-sibling" => Axis::FollowingSibling,
        "preceding-sibling" => Axis::PrecedingSibling,
        "following" => Axis::Following,
        "preceding" => Axis::Preceding,
        "self" => Axis::SelfAxis,
        "descendant-or-self" => Axis::DescendantOrSelf,
        "ancestor-or-self" => Axis::AncestorOrSelf,
        "attribute" => Axis::Attribute,
        "namespace" => Axis::Namespace,
        _ => {
            return Err(nom::Err::Error(nom::error::Error::new(
                input,
                nom::error::ErrorKind::Tag,
            )));
        }
    };
    Ok((input, axis))
}

fn node_test(input: &str) -> IResult<&str, NodeTest> {
    alt((
        node_type_test,
        wildcard_test,
        name_test,
    ))(input)
}

fn node_type_test(input: &str) -> IResult<&str, NodeTest> {
    let (input, name) = take_while1(|c: char| c.is_alphanumeric() || c == '-')(input)?;
    let (input, _) = multispace0(input)?;
    let (input, _) = char('(')(input)?;
    let (input, _) = multispace0(input)?;

    match name {
        "text" => {
            let (input, _) = char(')')(input)?;
            Ok((input, NodeTest::Text))
        }
        "node" => {
            let (input, _) = char(')')(input)?;
            Ok((input, NodeTest::Node))
        }
        "comment" => {
            let (input, _) = char(')')(input)?;
            Ok((input, NodeTest::Comment))
        }
        "processing-instruction" => {
            // processing-instruction() or processing-instruction('name')
            if input.starts_with(')') {
                let (input, _) = char(')')(input)?;
                Ok((input, NodeTest::PI))
            } else {
                // Parse string argument: 'name' or "name"
                let (input, pi_name) = alt((
                    |i| {
                        let (i, _) = char('\'')(i)?;
                        let (i, s) = nom::bytes::complete::take_while(|c| c != '\'')(i)?;
                        let (i, _) = char('\'')(i)?;
                        Ok((i, s))
                    },
                    |i| {
                        let (i, _) = char('"')(i)?;
                        let (i, s) = nom::bytes::complete::take_while(|c| c != '"')(i)?;
                        let (i, _) = char('"')(i)?;
                        Ok((i, s))
                    },
                ))(input)?;
                let (input, _) = multispace0(input)?;
                let (input, _) = char(')')(input)?;
                Ok((input, NodeTest::PIName(pi_name.to_string())))
            }
        }
        _ => {
            Err(nom::Err::Error(nom::error::Error::new(
                input,
                nom::error::ErrorKind::Tag,
            )))
        }
    }
}

fn wildcard_test(input: &str) -> IResult<&str, NodeTest> {
    // Handle both * and prefix:*
    if let Some(rest) = input.strip_prefix('*') {
        return Ok((rest, NodeTest::Wildcard));
    }
    // prefix:* — namespace wildcard
    let (rest, prefix) = take_while1(|c: char| c.is_alphanumeric() || c == '-' || c == '_')(input)?;
    let (rest, _) = char(':')(rest)?;
    let (rest, _) = char('*')(rest)?;
    Ok((rest, NodeTest::NamespacedName(prefix.to_string(), "*".to_string())))
}

fn name_test(input: &str) -> IResult<&str, NodeTest> {
    let (input, name) = take_while1(|c: char| c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == ':')(input)?;
    if let Some(colon_pos) = name.find(':') {
        let prefix = &name[..colon_pos];
        let local = &name[colon_pos + 1..];
        Ok((
            input,
            NodeTest::NamespacedName(prefix.to_string(), local.to_string()),
        ))
    } else {
        Ok((input, NodeTest::Name(name.to_string())))
    }
}

fn predicates(input: &str) -> IResult<&str, Vec<XPathExpr>> {
    nom::multi::many0(predicate)(input)
}

fn predicate(input: &str) -> IResult<&str, XPathExpr> {
    delimited(
        pair(multispace0, char('[')),
        preceded(multispace0, predicate_expr),
        pair(multispace0, char(']')),
    )(input)
}

/// XPath 1.0 operator precedence (lowest to highest):
/// or < and < equality < relational < additive < multiplicative < unary

fn predicate_expr(input: &str) -> IResult<&str, XPathExpr> {
    or_expr(input)
}

fn or_expr(input: &str) -> IResult<&str, XPathExpr> {
    let (input, left) = and_expr(input)?;
    let (input, _) = multispace0(input)?;
    if input.starts_with("or") && input[2..].starts_with(|c: char| c.is_whitespace()) {
        let (input, _) = tag("or")(input)?;
        let (input, _) = multispace0(input)?;
        let (input, right) = or_expr(input)?;
        Ok((input, XPathExpr::BinaryOp(Box::new(left), BinaryOp::Or, Box::new(right))))
    } else {
        Ok((input, left))
    }
}

fn and_expr(input: &str) -> IResult<&str, XPathExpr> {
    let (input, left) = equality_expr(input)?;
    let (input, _) = multispace0(input)?;
    if input.starts_with("and") && input[3..].starts_with(|c: char| c.is_whitespace()) {
        let (input, _) = tag("and")(input)?;
        let (input, _) = multispace0(input)?;
        let (input, right) = and_expr(input)?;
        Ok((input, XPathExpr::BinaryOp(Box::new(left), BinaryOp::And, Box::new(right))))
    } else {
        Ok((input, left))
    }
}

fn equality_expr(input: &str) -> IResult<&str, XPathExpr> {
    let (mut input, mut left) = relational_expr(input)?;
    loop {
        let (rest, _) = multispace0(input)?;
        if let Ok((rest, op)) = alt::<_, _, nom::error::Error<&str>, _>((
            nom::combinator::map(tag("!="), |_| BinaryOp::Neq),
            nom::combinator::map(char('='), |_| BinaryOp::Eq),
        ))(rest) {
            let (rest, _) = multispace0(rest)?;
            let (rest, right) = relational_expr(rest)?;
            left = XPathExpr::BinaryOp(Box::new(left), op, Box::new(right));
            input = rest;
        } else {
            return Ok((input, left));
        }
    }
}

fn relational_expr(input: &str) -> IResult<&str, XPathExpr> {
    let (mut input, mut left) = additive_expr(input)?;
    loop {
        let (rest, _) = multispace0(input)?;
        if let Ok((rest, op)) = alt::<_, _, nom::error::Error<&str>, _>((
            nom::combinator::map(tag("<="), |_| BinaryOp::Lte),
            nom::combinator::map(tag(">="), |_| BinaryOp::Gte),
            nom::combinator::map(char('<'), |_| BinaryOp::Lt),
            nom::combinator::map(char('>'), |_| BinaryOp::Gt),
        ))(rest) {
            let (rest, _) = multispace0(rest)?;
            let (rest, right) = additive_expr(rest)?;
            left = XPathExpr::BinaryOp(Box::new(left), op, Box::new(right));
            input = rest;
        } else {
            return Ok((input, left));
        }
    }
}

fn additive_expr(input: &str) -> IResult<&str, XPathExpr> {
    let (mut input, mut left) = multiplicative_expr(input)?;
    loop {
        let (rest, _) = multispace0(input)?;
        if let Ok((rest, op)) = alt::<_, _, nom::error::Error<&str>, _>((
            nom::combinator::map(char('+'), |_| BinaryOp::Add),
            nom::combinator::map(char('-'), |_| BinaryOp::Sub),
        ))(rest) {
            let (rest, _) = multispace0(rest)?;
            let (rest, right) = multiplicative_expr(rest)?;
            left = XPathExpr::BinaryOp(Box::new(left), op, Box::new(right));
            input = rest;
        } else {
            return Ok((input, left));
        }
    }
}

fn multiplicative_expr(input: &str) -> IResult<&str, XPathExpr> {
    let (mut input, mut left) = unary_expr(input)?;
    loop {
        let (rest, _) = multispace0(input)?;
        // * is always an operator here (not wildcard — we're past the path level)
        if let Ok((rest, _)) = char::<_, nom::error::Error<&str>>('*')(rest) {
            let (rest, _) = multispace0(rest)?;
            let (rest, right) = unary_expr(rest)?;
            left = XPathExpr::BinaryOp(Box::new(left), BinaryOp::Mul, Box::new(right));
            input = rest;
        } else if rest.starts_with("div") && rest[3..].starts_with(|c: char| !c.is_alphanumeric() && c != '-' && c != '_') {
            // div must be followed by non-name char (word boundary) — reject "div2" etc.
            let rest = &rest[3..];
            let (rest, _) = multispace0(rest)?;
            let (rest, right) = unary_expr(rest)?;
            left = XPathExpr::BinaryOp(Box::new(left), BinaryOp::Div, Box::new(right));
            input = rest;
        } else if rest.starts_with("mod") && rest[3..].starts_with(|c: char| !c.is_alphanumeric() && c != '-' && c != '_') {
            let rest = &rest[3..];
            let (rest, _) = multispace0(rest)?;
            let (rest, right) = unary_expr(rest)?;
            left = XPathExpr::BinaryOp(Box::new(left), BinaryOp::Mod, Box::new(right));
            input = rest;
        } else {
            return Ok((input, left));
        }
    }
}

fn unary_expr(input: &str) -> IResult<&str, XPathExpr> {
    let (input, _) = multispace0(input)?;
    if input.starts_with('-') {
        let (input, _) = char('-')(input)?;
        let (input, expr) = unary_expr(input)?;
        Ok((input, XPathExpr::UnaryMinus(Box::new(expr))))
    } else {
        union_path_expr(input)
    }
}

/// Union path expression: path | path | ...
/// XPath 1.0: UnionExpr ::= PathExpr | UnionExpr '|' PathExpr
/// Only path expressions (not numbers/strings) can be union operands.
fn union_path_expr(input: &str) -> IResult<&str, XPathExpr> {
    let (input, first) = primary_expr_inner(input)?;

    // Only try union if we got a path-like expression (not a literal)
    let is_path = matches!(&first,
        XPathExpr::LocationPath(_)
        | XPathExpr::FunctionCall(_, _)
        | XPathExpr::FilterPath(_, _)
        | XPathExpr::GlobalFilter(_, _)
        | XPathExpr::Union(_)
    );

    if !is_path {
        return Ok((input, first));
    }

    let (input, rest) = nom::multi::many0(preceded(
        delimited(multispace0, char('|'), multispace0),
        primary_expr_inner,
    ))(input)?;

    if rest.is_empty() {
        Ok((input, first))
    } else {
        let mut all = vec![first];
        all.extend(rest);
        Ok((input, XPathExpr::Union(all)))
    }
}

fn primary_expr_inner(input: &str) -> IResult<&str, XPathExpr> {
    let (input, _) = multispace0(input)?;
    alt((
        function_call_expr,
        parenthesized_pred_expr,
        string_literal_expr,
        number_literal_expr,
        nested_path_expr,
    ))(input)
}

fn parenthesized_pred_expr(input: &str) -> IResult<&str, XPathExpr> {
    let (input, _) = char('(')(input)?;
    let (input, _) = multispace0(input)?;
    // Support full expressions including union (|) inside parens
    let (input, first) = predicate_expr(input)?;
    let (input, rest) = nom::multi::many0(preceded(
        delimited(multispace0, char('|'), multispace0),
        predicate_expr,
    ))(input)?;
    let (input, _) = multispace0(input)?;
    let (input, _) = char(')')(input)?;
    if rest.is_empty() {
        Ok((input, first))
    } else {
        let mut all = vec![first];
        all.extend(rest);
        Ok((input, XPathExpr::Union(all)))
    }
}

fn function_call_expr(input: &str) -> IResult<&str, XPathExpr> {
    // Function names must start with a letter (not digit or hyphen)
    let first = input.chars().next().ok_or_else(|| nom::Err::Error(nom::error::Error::new(input, nom::error::ErrorKind::Alpha)))?;
    if !first.is_alphabetic() {
        return Err(nom::Err::Error(nom::error::Error::new(input, nom::error::ErrorKind::Alpha)));
    }
    let (input, name) = take_while1(|c: char| c.is_alphanumeric() || c == '-')(input)?;
    // Reject node type tests (text, node, comment, processing-instruction)
    if matches!(name, "text" | "node" | "comment" | "processing-instruction") {
        return Err(nom::Err::Error(nom::error::Error::new(
            input,
            nom::error::ErrorKind::Tag,
        )));
    }
    let (input, _) = multispace0(input)?; // allow whitespace before '('
    let (input, _) = char('(')(input)?;
    let (input, _) = multispace0(input)?;

    // Parse arguments (comma-separated)
    let (input, args) = if input.starts_with(')') {
        (input, vec![])
    } else {
        let (input, first) = predicate_expr(input)?;
        let (input, rest) = nom::multi::many0(preceded(
            delimited(multispace0, char(','), multispace0),
            predicate_expr,
        ))(input)?;
        let mut args = vec![first];
        args.extend(rest);
        (input, args)
    };

    let (input, _) = multispace0(input)?;
    let (input, _) = char(')')(input)?;
    Ok((input, XPathExpr::FunctionCall(name.to_string(), args)))
}

fn string_literal_expr(input: &str) -> IResult<&str, XPathExpr> {
    alt((single_quoted_string, double_quoted_string))(input)
}

fn single_quoted_string(input: &str) -> IResult<&str, XPathExpr> {
    let (input, _) = char('\'')(input)?;
    let (input, content) = nom::bytes::complete::take_while(|c| c != '\'')(input)?;
    let (input, _) = char('\'')(input)?;
    Ok((input, XPathExpr::StringLiteral(content.to_string())))
}

fn double_quoted_string(input: &str) -> IResult<&str, XPathExpr> {
    let (input, _) = char('"')(input)?;
    let (input, content) = nom::bytes::complete::take_while(|c| c != '"')(input)?;
    let (input, _) = char('"')(input)?;
    Ok((input, XPathExpr::StringLiteral(content.to_string())))
}

fn number_literal_expr(input: &str) -> IResult<&str, XPathExpr> {
    let (input, num_str) = take_while1(|c: char| c.is_ascii_digit() || c == '.')(input)?;

    // Must contain at least one digit (reject standalone "." which is self-axis)
    if !num_str.chars().any(|c| c.is_ascii_digit()) {
        return Err(nom::Err::Error(nom::error::Error::new(
            input,
            nom::error::ErrorKind::Float,
        )));
    }
    // Don't consume if it looks like a name
    if num_str.chars().next().map_or(true, |c| !c.is_ascii_digit() && c != '.') {
        return Err(nom::Err::Error(nom::error::Error::new(
            input,
            nom::error::ErrorKind::Float,
        )));
    }

    // Check for scientific notation suffix
    let (input, num_str) = if input.starts_with('e') || input.starts_with('E') {
        let (rest, exp) = take_while1(|c: char| c.is_ascii_digit() || c == 'e' || c == 'E' || c == '+' || c == '-')(input)?;
        let full = format!("{}{}", num_str, exp);
        (rest, full)
    } else {
        (input, num_str.to_string())
    };

    let num: f64 = num_str.parse().unwrap_or(f64::NAN);
    Ok((input, XPathExpr::NumberLiteral(num)))
}

fn nested_path_expr(input: &str) -> IResult<&str, XPathExpr> {
    // A location path inside a predicate (e.g., @attr, ., ..)
    let (input, path) = location_path(input)?;
    Ok((input, XPathExpr::LocationPath(path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_path() {
        let expr = parse_xpath("/root/child").unwrap();
        match expr {
            XPathExpr::LocationPath(path) => {
                assert!(path.absolute);
                assert_eq!(path.steps.len(), 2);
                assert_eq!(path.steps[0].node_test, NodeTest::Name("root".into()));
                assert_eq!(path.steps[1].node_test, NodeTest::Name("child".into()));
            }
            _ => panic!("Expected LocationPath"),
        }
    }

    #[test]
    fn test_descendant() {
        let expr = parse_xpath("//claim").unwrap();
        match expr {
            XPathExpr::LocationPath(path) => {
                assert!(path.absolute);
                // First step is descendant-or-self::node()
                assert_eq!(path.steps[0].axis, Axis::DescendantOrSelf);
                assert_eq!(path.steps[1].node_test, NodeTest::Name("claim".into()));
            }
            _ => panic!("Expected LocationPath"),
        }
    }

    #[test]
    fn test_text_node() {
        let expr = parse_xpath("//claim/text()").unwrap();
        match expr {
            XPathExpr::LocationPath(path) => {
                let last = path.steps.last().unwrap();
                assert_eq!(last.node_test, NodeTest::Text);
            }
            _ => panic!("Expected LocationPath"),
        }
    }

    #[test]
    fn test_attribute() {
        let expr = parse_xpath("/root/@lang").unwrap();
        match expr {
            XPathExpr::LocationPath(path) => {
                let last = path.steps.last().unwrap();
                assert_eq!(last.axis, Axis::Attribute);
                assert_eq!(last.node_test, NodeTest::Name("lang".into()));
            }
            _ => panic!("Expected LocationPath"),
        }
    }

    #[test]
    fn test_wildcard() {
        let expr = parse_xpath("/root/*").unwrap();
        match expr {
            XPathExpr::LocationPath(path) => {
                let last = path.steps.last().unwrap();
                assert_eq!(last.node_test, NodeTest::Wildcard);
            }
            _ => panic!("Expected LocationPath"),
        }
    }

    #[test]
    fn test_parent_axis() {
        let expr = parse_xpath("..").unwrap();
        match expr {
            XPathExpr::LocationPath(path) => {
                assert_eq!(path.steps[0].axis, Axis::Parent);
            }
            _ => panic!("Expected LocationPath"),
        }
    }

    #[test]
    fn test_explicit_axis() {
        let expr = parse_xpath("ancestor::div").unwrap();
        match expr {
            XPathExpr::LocationPath(path) => {
                assert_eq!(path.steps[0].axis, Axis::Ancestor);
                assert_eq!(path.steps[0].node_test, NodeTest::Name("div".into()));
            }
            _ => panic!("Expected LocationPath"),
        }
    }

    #[test]
    fn test_position_predicate() {
        let expr = parse_xpath("//claim[1]").unwrap();
        match expr {
            XPathExpr::LocationPath(path) => {
                let claim_step = &path.steps[1];
                assert_eq!(claim_step.predicates.len(), 1);
                assert_eq!(claim_step.predicates[0], XPathExpr::NumberLiteral(1.0));
            }
            _ => panic!("Expected LocationPath"),
        }
    }

    #[test]
    fn test_function_predicate() {
        let expr = parse_xpath("//p[contains(., 'semiconductor')]").unwrap();
        match expr {
            XPathExpr::LocationPath(path) => {
                let p_step = &path.steps[1];
                assert_eq!(p_step.predicates.len(), 1);
                match &p_step.predicates[0] {
                    XPathExpr::FunctionCall(name, args) => {
                        assert_eq!(name, "contains");
                        assert_eq!(args.len(), 2);
                    }
                    other => panic!("Expected FunctionCall, got {:?}", other),
                }
            }
            _ => panic!("Expected LocationPath"),
        }
    }

    #[test]
    fn test_attribute_predicate() {
        let expr = parse_xpath("//claim[@type='independent']").unwrap();
        match expr {
            XPathExpr::LocationPath(path) => {
                let claim_step = &path.steps[1];
                assert_eq!(claim_step.predicates.len(), 1);
                match &claim_step.predicates[0] {
                    XPathExpr::BinaryOp(left, BinaryOp::Eq, right) => {
                        // left should be @type location path
                        // right should be 'independent' string
                        match right.as_ref() {
                            XPathExpr::StringLiteral(s) => assert_eq!(s, "independent"),
                            _ => panic!("Expected string literal"),
                        }
                    }
                    other => panic!("Expected BinaryOp, got {:?}", other),
                }
            }
            _ => panic!("Expected LocationPath"),
        }
    }

    #[test]
    fn test_position_function_predicate() {
        let expr = parse_xpath("//claim[position()=1]").unwrap();
        match expr {
            XPathExpr::LocationPath(path) => {
                let claim_step = &path.steps[1];
                assert_eq!(claim_step.predicates.len(), 1);
                match &claim_step.predicates[0] {
                    XPathExpr::BinaryOp(left, BinaryOp::Eq, right) => {
                        match left.as_ref() {
                            XPathExpr::FunctionCall(name, _) => assert_eq!(name, "position"),
                            _ => panic!("Expected FunctionCall"),
                        }
                        match right.as_ref() {
                            XPathExpr::NumberLiteral(n) => assert_eq!(*n, 1.0),
                            _ => panic!("Expected NumberLiteral"),
                        }
                    }
                    other => panic!("Expected BinaryOp, got {:?}", other),
                }
            }
            _ => panic!("Expected LocationPath"),
        }
    }

    #[test]
    fn test_multiple_predicates() {
        let expr = parse_xpath("//claim[@type='independent'][1]").unwrap();
        match expr {
            XPathExpr::LocationPath(path) => {
                let claim_step = &path.steps[1];
                assert_eq!(claim_step.predicates.len(), 2);
            }
            _ => panic!("Expected LocationPath"),
        }
    }
}
