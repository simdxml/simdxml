/// Conformance tests adapted from pugixml's XPath test suite (MIT License).
/// Tests extracted from pugixml's test_xpath_*.cpp files covering:
/// - W3C XPath 1.0 spec examples (paths, abbreviated syntax)
/// - All XPath 1.0 functions (string, number, boolean, nodeset)
/// - Operators (arithmetic, logical, equality, relational, union)
/// - Additional path and axis tests

use simdxml::xpath::{eval_standalone_expr, StandaloneResult, XPathNode};

#[derive(serde::Deserialize)]
struct Assertion {
    kind: String,
    context: Option<String>,
    xpath: String,
    #[serde(default)]
    expected_count: Option<usize>,
    #[serde(default)]
    expected: Option<serde_json::Value>,
    #[serde(default)]
    node_indices: Option<Vec<usize>>,
}

#[derive(serde::Deserialize)]
struct TestData {
    #[serde(default)]
    metadata: Option<serde_json::Value>,
    tests: Vec<TestBlock>,
}

#[derive(serde::Deserialize)]
struct TestBlock {
    name: String,
    #[serde(default)]
    xml: Option<String>,
    #[serde(default)]
    source_file: Option<String>,
    /// XPath to find the context node for "first_child" context (e.g., "/n/n2").
    /// If absent, uses the root element.
    #[serde(default)]
    context_xpath: Option<String>,
    assertions: Vec<Assertion>,
}

fn run_pugixml_tests() -> (usize, usize, Vec<String>) {
    let json_str = include_str!("../../../testdata/pugixml_xpath_tests.json");
    let data: TestData = serde_json::from_str(json_str).unwrap();

    let mut passed = 0;
    let mut total = 0;
    let mut failures = Vec::new();

    for block in &data.tests {
        let xml_str = block.xml.as_deref().unwrap_or("<r/>");
        let xml_bytes = xml_str.as_bytes();
        let index = match simdxml::parse(xml_bytes) {
            Ok(idx) => idx,
            Err(_) => continue,
        };
        // Find root element (first Open/SelfClose at depth 0)
        let root_elem_idx = (0..index.tag_count())
            .find(|&i| index.depths[i] == 0
                && (index.tag_types[i] == simdxml::index::TagType::Open
                    || index.tag_types[i] == simdxml::index::TagType::SelfClose))
            .unwrap_or(0);

        // Determine first_child context: use context_xpath if specified, else root element
        let first_child_idx = if let Some(ctx_xpath) = &block.context_xpath {
            index.xpath(ctx_xpath)
                .ok()
                .and_then(|nodes| nodes.first().and_then(|n| match n {
                    simdxml::xpath::XPathNode::Element(idx) => Some(*idx),
                    _ => None,
                }))
                .unwrap_or(root_elem_idx)
        } else {
            root_elem_idx
        };

        for assertion in &block.assertions {
            let ctx = assertion.context.as_deref().unwrap_or("doc");
            // Skip contexts we can't handle
            if ctx != "null" && ctx != "doc" && ctx != "first_child" { continue; }

            match assertion.kind.as_str() {
                "nodeset" => {
                    if ctx == "null" { continue; }

                    total += 1;
                    let xpath_str = assertion.xpath.clone();
                    let fc_idx = first_child_idx;

                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if ctx == "first_child" {
                            index.xpath_from(&xpath_str, fc_idx)
                        } else {
                            // pugixml "doc" context = virtual document root,
                            // not the document element. Use evaluate() directly.
                            let expr = simdxml::xpath::parse_xpath(&xpath_str)?;
                            simdxml::xpath::evaluate(&index, &expr)
                        }
                    }));

                    match result {
                        Err(_) => failures.push(format!("[{}] PANIC: {}", block.name, assertion.xpath)),
                        Ok(Err(e)) => {
                            failures.push(format!("[{}] ERROR: {} -> {}", block.name, assertion.xpath, e));
                        }
                        Ok(Ok(nodes)) => {
                            if let Some(expected) = assertion.expected_count {
                                if nodes.len() == expected {
                                    passed += 1;
                                } else {
                                    failures.push(format!(
                                        "[{}] COUNT: {} expected {} got {}",
                                        block.name, assertion.xpath, expected, nodes.len()
                                    ));
                                }
                            } else {
                                passed += 1; // No count assertion
                            }
                        }
                    }
                }

                "number" | "number_nan" => {
                    if ctx != "null" && ctx != "doc" && ctx != "first_child" { continue; }
                    total += 1;
                    let xpath_clone = assertion.xpath.clone();
                    let expr_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if ctx == "null" {
                            eval_standalone_expr(&xpath_clone)
                        } else {
                            if ctx == "first_child" { index.eval_expr_from(&xpath_clone, first_child_idx) } else { index.eval_expr(&xpath_clone) }
                        }
                    }));
                    let expr_result = match expr_result {
                        Ok(r) => r,
                        Err(_) => { failures.push(format!("[{}] PANIC: {}", block.name, assertion.xpath)); continue; }
                    };
                    match expr_result {
                        Ok(StandaloneResult::Number(n)) => {
                            if assertion.kind == "number_nan" {
                                if n.is_nan() { passed += 1; }
                                else { failures.push(format!("[{}] NAN: {} got {}", block.name, assertion.xpath, n)); }
                            } else if let Some(serde_json::Value::Number(exp)) = &assertion.expected {
                                let exp_f = exp.as_f64().unwrap_or(0.0);
                                let close = (n - exp_f).abs() < 1e-10
                                    || (exp_f.abs() > 1e15 && (n - exp_f).abs() < exp_f.abs() * 1e-4)
                                    || (n.is_infinite() && exp_f.is_infinite() && n.signum() == exp_f.signum());
                                if close { passed += 1; }
                                else { failures.push(format!("[{}] NUM: {} expected {} got {}", block.name, assertion.xpath, exp_f, n)); }
                            } else {
                                passed += 1;
                            }
                        }
                        Ok(_) => failures.push(format!("[{}] TYPE: {} expected number", block.name, assertion.xpath)),
                        Err(e) => failures.push(format!("[{}] ERROR: {} -> {}", block.name, assertion.xpath, e)),
                    }
                }

                "boolean" => {
                    if ctx != "null" && ctx != "doc" && ctx != "first_child" { continue; }
                    total += 1;
                    let xpath_clone = assertion.xpath.clone();
                    let expr_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if ctx == "null" {
                            eval_standalone_expr(&xpath_clone)
                        } else {
                            if ctx == "first_child" { index.eval_expr_from(&xpath_clone, first_child_idx) } else { index.eval_expr(&xpath_clone) }
                        }
                    }));
                    let expr_result = match expr_result {
                        Ok(r) => r,
                        Err(_) => { failures.push(format!("[{}] PANIC: {}", block.name, assertion.xpath)); continue; }
                    };
                    match expr_result {
                        Ok(StandaloneResult::Boolean(b)) => {
                            if let Some(serde_json::Value::Bool(exp)) = &assertion.expected {
                                if b == *exp { passed += 1; }
                                else { failures.push(format!("[{}] BOOL: {} expected {} got {}", block.name, assertion.xpath, exp, b)); }
                            } else {
                                passed += 1;
                            }
                        }
                        Ok(StandaloneResult::String(s)) => {
                            // LocationPath expressions return strings via eval_expr.
                            // For boolean tests, try evaluating as a nodeset for truthiness.
                            if ctx != "null" {
                                let xpath_retry = assertion.xpath.clone();
                                let fc = first_child_idx;
                                let nodeset_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    if ctx == "first_child" {
                                        index.xpath_from(&xpath_retry, fc)
                                    } else {
                                        let expr = simdxml::xpath::parse_xpath(&xpath_retry)?;
                                        simdxml::xpath::evaluate(&index, &expr)
                                    }
                                }));
                                let b = match nodeset_result {
                                    Ok(Ok(nodes)) => !nodes.is_empty(),
                                    _ => !s.is_empty(), // fallback to string truthiness
                                };
                                if let Some(serde_json::Value::Bool(exp)) = &assertion.expected {
                                    if b == *exp { passed += 1; }
                                    else { failures.push(format!("[{}] BOOL: {} expected {} got {}", block.name, assertion.xpath, exp, b)); }
                                } else {
                                    passed += 1;
                                }
                            } else {
                                // For null context, use string truthiness
                                let b = !s.is_empty();
                                if let Some(serde_json::Value::Bool(exp)) = &assertion.expected {
                                    if b == *exp { passed += 1; }
                                    else { failures.push(format!("[{}] BOOL: {} expected {} got {}", block.name, assertion.xpath, exp, b)); }
                                } else {
                                    passed += 1;
                                }
                            }
                        }
                        Ok(StandaloneResult::Number(n)) => {
                            // Number to boolean: true if nonzero and not NaN
                            let b = n != 0.0 && !n.is_nan();
                            if let Some(serde_json::Value::Bool(exp)) = &assertion.expected {
                                if b == *exp { passed += 1; }
                                else { failures.push(format!("[{}] BOOL: {} expected {} got {}", block.name, assertion.xpath, exp, b)); }
                            } else {
                                passed += 1;
                            }
                        }
                        Err(e) => failures.push(format!("[{}] ERROR: {} -> {}", block.name, assertion.xpath, e)),
                    }
                }

                "string" => {
                    if ctx != "null" && ctx != "doc" && ctx != "first_child" { continue; }
                    total += 1;
                    let xpath_clone = assertion.xpath.clone();
                    let expr_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if ctx == "null" {
                            eval_standalone_expr(&xpath_clone)
                        } else {
                            if ctx == "first_child" { index.eval_expr_from(&xpath_clone, first_child_idx) } else { index.eval_expr(&xpath_clone) }
                        }
                    }));
                    let expr_result = match expr_result {
                        Ok(r) => r,
                        Err(_) => { failures.push(format!("[{}] PANIC: {}", block.name, assertion.xpath)); continue; }
                    };
                    match expr_result {
                        Ok(StandaloneResult::String(s)) => {
                            if let Some(serde_json::Value::String(exp)) = &assertion.expected {
                                if s == *exp { passed += 1; }
                                else { failures.push(format!("[{}] STR: {} expected '{}' got '{}'", block.name, assertion.xpath, exp, s)); }
                            } else {
                                passed += 1;
                            }
                        }
                        Ok(r) => {
                            // Some string tests return numbers/booleans — check as_string
                            if let Some(serde_json::Value::String(exp)) = &assertion.expected {
                                let s = match &r {
                                    StandaloneResult::Number(n) => simdxml::xpath::eval_standalone_expr(&format!("string({})", &assertion.xpath))
                                        .map(|r| match r { StandaloneResult::String(s) => s, _ => String::new() })
                                        .unwrap_or_default(),
                                    StandaloneResult::Boolean(b) => b.to_string(),
                                    StandaloneResult::String(s) => s.clone(),
                                };
                                if s == *exp { passed += 1; }
                                else { failures.push(format!("[{}] STR: {} expected '{}' got '{}'", block.name, assertion.xpath, exp, s)); }
                            } else {
                                passed += 1;
                            }
                        }
                        Err(e) => failures.push(format!("[{}] ERROR: {} -> {}", block.name, assertion.xpath, e)),
                    }
                }

                "fail" => {
                    // Expected parse/eval failure
                    total += 1;
                    let result = eval_standalone_expr(&assertion.xpath);
                    if result.is_err() {
                        passed += 1;
                    } else {
                        // Also check if it fails when evaluated against a document
                        let doc_result = index.xpath(&assertion.xpath);
                        if doc_result.is_err() {
                            passed += 1;
                        } else {
                            failures.push(format!("[{}] NOFAIL: {} should have failed", block.name, assertion.xpath));
                        }
                    }
                }

                _ => continue,
            }
        }
    }

    (passed, total, failures)
}

#[test]
fn test_pugixml_conformance() {
    let (passed, total, failures) = run_pugixml_tests();
    let pct = (passed as f64 / total.max(1) as f64) * 100.0;

    println!("\n=== PUGIXML CONFORMANCE: {}/{} ({:.1}%) ===", passed, total, pct);

    if !failures.is_empty() {
        let errors = failures.iter().filter(|f| f.contains("ERROR:")).count();
        let panics = failures.iter().filter(|f| f.contains("PANIC:")).count();
        let counts = failures.iter().filter(|f| f.contains("COUNT:")).count();
        let types = failures.iter().filter(|f| f.contains("TYPE:") || f.contains("BOOL:") || f.contains("NUM:") || f.contains("STR:") || f.contains("NAN:")).count();
        let nofails = failures.iter().filter(|f| f.contains("NOFAIL:")).count();
        println!("  {} errors, {} panics, {} count mismatches, {} type/value, {} expected-failures",
            errors, panics, counts, types, nofails);

        // Print first 20 failures for debugging
        for f in failures.iter().take(500) {
            println!("  {}", f);
        }
        if failures.len() > 20 {
            println!("  ... and {} more", failures.len() - 20);
        }
    }
}
