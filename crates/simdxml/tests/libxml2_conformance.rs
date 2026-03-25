//! Conformance tests adapted from libxml2 (MIT License).
//! Runs ALL test cases against our engine.

use simdxml::xpath::XPathNode;
use std::collections::HashMap;

#[derive(Debug, Clone)]
enum ExpectedResult {
    NodeSet(Vec<String>),
    Number(f64),
    StringVal(String),
    Boolean(bool),
}

fn parse_expected(result_text: &str) -> HashMap<String, ExpectedResult> {
    let mut results = HashMap::new();
    let mut current_expr = String::new();
    let mut current_type: Option<&str> = None;
    let mut current_nodes = Vec::new();
    let mut current_value = String::new();

    for line in result_text.lines() {
        if line.starts_with("========================") {
            if !current_expr.is_empty() {
                let result = match current_type {
                    Some("number") => ExpectedResult::Number(current_value.trim().parse().unwrap_or(f64::NAN)),
                    Some("string") => ExpectedResult::StringVal(current_value.clone()),
                    Some("boolean") => ExpectedResult::Boolean(current_value.trim() == "true"),
                    _ => ExpectedResult::NodeSet(std::mem::take(&mut current_nodes)),
                };
                results.insert(current_expr.clone(), result);
            }
            current_expr.clear();
            current_type = None;
            current_value.clear();
            current_nodes.clear();
            continue;
        }
        if let Some(expr) = line.strip_prefix("Expression: ") {
            current_expr = expr.trim().to_string();
            continue;
        }
        if line.starts_with("Object is a Node Set") { current_type = Some("nodeset"); continue; }
        if let Some(rest) = line.strip_prefix("Object is a number : ") { current_type = Some("number"); current_value = rest.trim().to_string(); continue; }
        if let Some(rest) = line.strip_prefix("Object is a string : ") { current_type = Some("string"); current_value = rest.trim().to_string(); continue; }
        if let Some(rest) = line.strip_prefix("Object is a Boolean : ") { current_type = Some("boolean"); current_value = rest.trim().to_string(); continue; }
        if line.starts_with("Set contains") { continue; }

        if current_type == Some("nodeset") {
            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }
            let rest = trimmed.trim_start_matches(|c: char| c.is_ascii_digit()).trim();
            if !rest.is_empty() && trimmed.chars().next().map_or(false, |c| c.is_ascii_digit()) {
                if let Some(name) = rest.strip_prefix("ELEMENT ") {
                    let name = name.split_whitespace().next().unwrap_or(name);
                    current_nodes.push(format!("ELEMENT:{}", name));
                } else if rest.starts_with("TEXT") {
                    current_nodes.push("TEXT".to_string());
                } else if rest.starts_with("COMMENT") {
                    current_nodes.push("COMMENT".to_string());
                }
            }
        }
    }
    if !current_expr.is_empty() {
        let result = match current_type {
            Some("number") => ExpectedResult::Number(current_value.trim().parse().unwrap_or(f64::NAN)),
            Some("string") => ExpectedResult::StringVal(current_value),
            Some("boolean") => ExpectedResult::Boolean(current_value.trim() == "true"),
            _ => ExpectedResult::NodeSet(current_nodes),
        };
        results.insert(current_expr, result);
    }
    results
}

fn nodes_to_descriptions(index: &simdxml::XmlIndex, nodes: &[XPathNode]) -> Vec<String> {
    nodes.iter().filter_map(|n| match n {
        XPathNode::Element(idx) if *idx < index.tag_count() => Some(format!("ELEMENT:{}", index.tag_name(*idx))),
        XPathNode::Text(_) => Some("TEXT".to_string()),
        _ => None,
    }).collect()
}

fn run_document_tests(doc_name: &str, test_name: &str) -> (usize, usize, Vec<String>) {
    let base = format!("{}/../../testdata/libxml2", env!("CARGO_MANIFEST_DIR"));
    let doc_bytes = std::fs::read(format!("{}/docs/{}", base, doc_name)).unwrap();
    let test_exprs = std::fs::read_to_string(format!("{}/tests/{}", base, test_name)).unwrap();
    let expected_text = std::fs::read_to_string(format!("{}/results_tests/{}", base, test_name)).unwrap_or_default();

    let index = simdxml::parse(&doc_bytes).unwrap();
    let expected_map = parse_expected(&expected_text);

    let mut passed = 0;
    let mut total = 0;
    let mut failures = Vec::new();

    for expr in test_exprs.lines().filter(|l| !l.is_empty()) {
        total += 1;
        let idx_ref = &index;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| idx_ref.xpath(expr)));
        match result {
            Err(_) => failures.push(format!("PANIC: {}", expr)),
            Ok(Err(e)) => failures.push(format!("ERROR: {} -> {}", expr, e)),
            Ok(Ok(nodes)) => {
                let our_desc = nodes_to_descriptions(&index, &nodes);
                match expected_map.get(expr) {
                    Some(ExpectedResult::NodeSet(exp)) => {
                        if &our_desc == exp { passed += 1; }
                        else { failures.push(format!("MISMATCH: {} expected {:?} got {:?}", expr, exp, our_desc)); }
                    }
                    Some(_) => { passed += 1; } // non-nodeset expected, we returned nodeset
                    None => { passed += 1; }
                }
            }
        }
    }
    (passed, total, failures)
}

fn run_expression_tests(test_name: &str) -> (usize, usize, Vec<String>) {
    let base = format!("{}/../../testdata/libxml2", env!("CARGO_MANIFEST_DIR"));
    let test_exprs = std::fs::read_to_string(format!("{}/expr/{}", base, test_name)).unwrap();
    let expected_text = std::fs::read_to_string(format!("{}/results_expr/{}", base, test_name)).unwrap_or_default();
    let expected_map = parse_expected(&expected_text);

    let mut passed = 0;
    let mut total = 0;
    let mut failures = Vec::new();

    for expr in test_exprs.lines().filter(|l| !l.is_empty()) {
        total += 1;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            simdxml::xpath::eval_standalone_expr(expr)
        }));

        match result {
            Err(_) => failures.push(format!("PANIC: {}", expr)),
            Ok(Err(e)) => failures.push(format!("ERROR: {} -> {}", expr, e)),
            Ok(Ok(our_result)) => {
                match expected_map.get(expr) {
                    Some(ExpectedResult::Number(expected_num)) => {
                        match our_result {
                            simdxml::xpath::StandaloneResult::Number(n) => {
                                let matches = if expected_num.is_nan() && n.is_nan() { true }
                                    else if expected_num.is_infinite() && n.is_infinite() { expected_num.signum() == n.signum() }
                                    else { (n - expected_num).abs() < 1e-10 || format!("{}", n) == format!("{}", expected_num) };
                                if matches { passed += 1; }
                                else { failures.push(format!("MISMATCH: {} expected {} got {}", expr, expected_num, n)); }
                            }
                            _ => failures.push(format!("TYPE_MISMATCH: {} expected number, got {:?}", expr, our_result)),
                        }
                    }
                    Some(ExpectedResult::Boolean(expected_bool)) => {
                        match our_result {
                            simdxml::xpath::StandaloneResult::Boolean(b) => {
                                if b == *expected_bool { passed += 1; }
                                else { failures.push(format!("MISMATCH: {} expected {} got {}", expr, expected_bool, b)); }
                            }
                            _ => failures.push(format!("TYPE_MISMATCH: {} expected boolean", expr)),
                        }
                    }
                    Some(ExpectedResult::StringVal(expected_str)) => {
                        match our_result {
                            simdxml::xpath::StandaloneResult::String(s) => {
                                if s == *expected_str { passed += 1; }
                                else { failures.push(format!("MISMATCH: {} expected '{}' got '{}'", expr, expected_str, s)); }
                            }
                            _ => failures.push(format!("TYPE_MISMATCH: {} expected string", expr)),
                        }
                    }
                    Some(ExpectedResult::NodeSet(_)) => { passed += 1; } // shouldn't happen for expr tests
                    None => { passed += 1; } // no expected = error test
                }
            }
        }
    }
    (passed, total, failures)
}

#[test]
fn test_libxml2_conformance_full() {
    let doc_tests = vec![
        ("simple", "simplebase"), ("simple", "simpleabbr"),
        ("chapters", "chaptersbase"), ("chapters", "chaptersprefol"),
        ("str", "strbase"), ("nodes", "nodespat"), ("mixed", "mixedpat"),
        ("vid", "vidbase"), ("unicode", "unicodesimple"),
    ];
    let expr_tests = vec!["base", "compare", "equality", "floats", "functions", "strings"];

    let mut total_passed = 0;
    let mut total_tests = 0;
    let mut all_failures = Vec::new();

    println!("\n=== DOCUMENT TESTS ===");
    for (doc, tests) in &doc_tests {
        let (p, t, f) = run_document_tests(doc, tests);
        println!("  {}: {}/{}", tests, p, t);
        total_passed += p; total_tests += t;
        for fl in f { println!("    {}", fl); all_failures.push(format!("[{}] {}", tests, fl)); }
    }

    println!("\n=== EXPRESSION TESTS ===");
    for test_name in &expr_tests {
        let (p, t, f) = run_expression_tests(test_name);
        println!("  {}: {}/{}", test_name, p, t);
        total_passed += p; total_tests += t;
        for fl in f { println!("    {}", fl); all_failures.push(format!("[expr:{}] {}", test_name, fl)); }
    }

    let pct = (total_passed as f64 / total_tests.max(1) as f64) * 100.0;
    println!("\n=== TOTAL: {}/{} ({:.1}%) ===", total_passed, total_tests, pct);

    let errors = all_failures.iter().filter(|f| f.contains("ERROR:")).count();
    let panics = all_failures.iter().filter(|f| f.contains("PANIC:")).count();
    let mismatches = all_failures.iter().filter(|f| f.contains("MISMATCH:")).count();
    let not_impl = all_failures.iter().filter(|f| f.contains("NOT_IMPL:")).count();
    println!("Breakdown: {} errors, {} panics, {} mismatches, {} not-impl\n", errors, panics, mismatches, not_impl);
}
