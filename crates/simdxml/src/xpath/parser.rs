use nom::{
    branch::alt,
    bytes::complete::{tag, take_while1},
    character::complete::{char, multispace0},
    combinator::opt,
    multi::separated_list1,
    sequence::{delimited, pair, preceded, tuple},
    IResult,
};

use super::ast::*;
use crate::error::{Result, SimdXmlError};

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
    // For now, parse location paths and simple expressions
    // Full XPath 1.0 expression grammar will be expanded
    alt((union_expr, location_path_expr))(input)
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
        let (input, _) = char('/')(input)?;
        let (input, mut steps) = separated_list1(char('/'), step)(input)?;
        // Prepend descendant-or-self::node() for //
        steps.insert(
            0,
            Step {
                axis: Axis::DescendantOrSelf,
                node_test: NodeTest::Node,
                predicates: vec![],
            },
        );
        Ok((
            input,
            LocationPath {
                absolute: true,
                steps,
            },
        ))
    } else if input.is_empty() || input.starts_with('|') || input.starts_with(')') {
        // Bare / — select root
        Ok((
            input,
            LocationPath {
                absolute: true,
                steps: vec![],
            },
        ))
    } else {
        let (input, steps) = separated_list1(
            alt((
                // Handle // within path
                nom::combinator::map(tag("//"), |_| true),
                nom::combinator::map(char('/'), |_| false),
            )),
            step,
        )(input)?;
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
    let (input, steps) = separated_list1(char('/'), step)(input)?;
    let mut all_steps = vec![Step {
        axis: Axis::DescendantOrSelf,
        node_test: NodeTest::Node,
        predicates: vec![],
    }];
    all_steps.extend(steps);
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
    let (input, steps) = separated_list1(char('/'), step)(input)?;
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
    let (input, _) = tag("()")(input)?;
    let test = match name {
        "text" => NodeTest::Text,
        "node" => NodeTest::Node,
        "comment" => NodeTest::Comment,
        "processing-instruction" => NodeTest::PI,
        _ => {
            return Err(nom::Err::Error(nom::error::Error::new(
                input,
                nom::error::ErrorKind::Tag,
            )));
        }
    };
    Ok((input, test))
}

fn wildcard_test(input: &str) -> IResult<&str, NodeTest> {
    let (input, _) = char('*')(input)?;
    Ok((input, NodeTest::Wildcard))
}

fn name_test(input: &str) -> IResult<&str, NodeTest> {
    let (input, name) = take_while1(|c: char| c.is_alphanumeric() || c == '-' || c == '_' || c == ':')(input)?;
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

/// Expression inside a predicate — supports comparisons, function calls, literals, numbers
fn predicate_expr(input: &str) -> IResult<&str, XPathExpr> {
    alt((comparison_expr, primary_expr))(input)
}

fn comparison_expr(input: &str) -> IResult<&str, XPathExpr> {
    let (input, left) = primary_expr(input)?;
    let (input, _) = multispace0(input)?;

    if let Ok((rest, op)) = comparison_op(input) {
        let (rest, _) = multispace0(rest)?;
        let (rest, right) = primary_expr(rest)?;
        Ok((
            rest,
            XPathExpr::BinaryOp(Box::new(left), op, Box::new(right)),
        ))
    } else {
        Ok((input, left))
    }
}

fn comparison_op(input: &str) -> IResult<&str, BinaryOp> {
    alt((
        nom::combinator::map(tag("!="), |_| BinaryOp::Neq),
        nom::combinator::map(tag("<="), |_| BinaryOp::Lte),
        nom::combinator::map(tag(">="), |_| BinaryOp::Gte),
        nom::combinator::map(char('='), |_| BinaryOp::Eq),
        nom::combinator::map(char('<'), |_| BinaryOp::Lt),
        nom::combinator::map(char('>'), |_| BinaryOp::Gt),
    ))(input)
}

fn primary_expr(input: &str) -> IResult<&str, XPathExpr> {
    let (input, _) = multispace0(input)?;
    alt((
        function_call_expr,
        string_literal_expr,
        number_literal_expr,
        nested_path_expr,
    ))(input)
}

fn function_call_expr(input: &str) -> IResult<&str, XPathExpr> {
    let (input, name) = take_while1(|c: char| c.is_alphanumeric() || c == '-')(input)?;
    // Reject node type tests (text, node, comment, processing-instruction)
    if matches!(name, "text" | "node" | "comment" | "processing-instruction") {
        return Err(nom::Err::Error(nom::error::Error::new(
            input,
            nom::error::ErrorKind::Tag,
        )));
    }
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
    let (input, content) = take_while1(|c| c != '\'')(input)?;
    let (input, _) = char('\'')(input)?;
    Ok((input, XPathExpr::StringLiteral(content.to_string())))
}

fn double_quoted_string(input: &str) -> IResult<&str, XPathExpr> {
    let (input, _) = char('"')(input)?;
    let (input, content) = take_while1(|c| c != '"')(input)?;
    let (input, _) = char('"')(input)?;
    Ok((input, XPathExpr::StringLiteral(content.to_string())))
}

fn number_literal_expr(input: &str) -> IResult<&str, XPathExpr> {
    let (input, num_str) = take_while1(|c: char| c.is_ascii_digit() || c == '.')(input)?;
    let num: f64 = num_str
        .parse()
        .map_err(|_| nom::Err::Error(nom::error::Error::new(input, nom::error::ErrorKind::Float)))?;
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
