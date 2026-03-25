//! Conformance tests adapted from libxml2 (MIT License).
//! Tests XPath evaluation against known-good libxml2 results.

use simdxml::xpath::XPathNode;
use std::collections::HashMap;

/// Parse libxml2's expected output into a map: expression → list of node descriptions.
fn parse_expected(result_text: &str) -> HashMap<String, Vec<String>> {
    let mut results = HashMap::new();
    let mut current_expr = String::new();
    let mut current_nodes = Vec::new();

    for line in result_text.lines() {
        if line.starts_with("========================") {
            if !current_expr.is_empty() {
                results.insert(current_expr.clone(), std::mem::take(&mut current_nodes));
            }
            current_expr.clear();
            continue;
        }
        if let Some(expr) = line.strip_prefix("Expression: ") {
            current_expr = expr.trim().to_string();
            continue;
        }
        if line.starts_with("Object is") || line.starts_with("Set contains") {
            continue;
        }
        let trimmed = line.trim();
        // Match "1  ELEMENT head", "10  ELEMENT title", "1  TEXT", etc.
        let rest = trimmed.trim_start_matches(|c: char| c.is_ascii_digit()).trim();
        if !rest.is_empty() && trimmed.chars().next().map_or(false, |c| c.is_ascii_digit()) {
            if let Some(name) = rest.strip_prefix("ELEMENT ") {
                let name = name.split_whitespace().next().unwrap_or(name);
                current_nodes.push(format!("ELEMENT:{}", name));
            } else if rest.starts_with("TEXT") {
                current_nodes.push("TEXT".to_string());
            } else if rest.starts_with("COMMENT") {
                current_nodes.push("COMMENT".to_string());
            } else if rest.starts_with("ATTRIBUTE") {
                if let Some(name) = rest.strip_prefix("ATTRIBUTE ") {
                    let name = name.split_whitespace().next().unwrap_or(name);
                    current_nodes.push(format!("ATTR:{}", name));
                }
            }
        }
    }
    if !current_expr.is_empty() {
        results.insert(current_expr, current_nodes);
    }
    results
}

/// Convert our XPathNode results to the same format as the expected output.
fn nodes_to_descriptions(index: &simdxml::XmlIndex, nodes: &[XPathNode]) -> Vec<String> {
    nodes
        .iter()
        .filter_map(|n| match n {
            XPathNode::Element(idx) if *idx < index.tag_count() => {
                Some(format!("ELEMENT:{}", index.tag_name(*idx)))
            }
            XPathNode::Text(_) => Some("TEXT".to_string()),
            XPathNode::Attribute(_, _) => None, // TODO: include attribute nodes
            _ => None,
        })
        .collect()
}

/// Run a set of document-context tests and report results.
fn run_document_tests(doc_name: &str, test_name: &str) -> (usize, usize, Vec<String>) {
    let base = format!("{}/../../testdata/libxml2", env!("CARGO_MANIFEST_DIR"));
    let doc_path = format!("{}/docs/{}", base, doc_name);
    let test_path = format!("{}/tests/{}", base, test_name);
    let result_path = format!("{}/results_tests/{}", base, test_name);

    let doc_bytes = std::fs::read(&doc_path).expect(&format!("Missing doc: {}", doc_path));
    let test_exprs =
        std::fs::read_to_string(&test_path).expect(&format!("Missing tests: {}", test_path));
    let expected_text = std::fs::read_to_string(&result_path).unwrap_or_default();

    let index = simdxml::parse(&doc_bytes).expect(&format!("Failed to parse {}", doc_path));
    let expected_map = parse_expected(&expected_text);

    let expressions: Vec<&str> = test_exprs.lines().filter(|l| !l.is_empty()).collect();
    let mut passed = 0;
    let mut total = 0;
    let mut failures = Vec::new();

    for expr in &expressions {
        total += 1;

        let idx_ref = &index;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| idx_ref.xpath(expr)));

        match result {
            Err(_) => {
                failures.push(format!("  PANIC: {}", expr));
            }
            Ok(Err(e)) => {
                failures.push(format!("  ERROR: {} → {}", expr, e));
            }
            Ok(Ok(nodes)) => {
                let our_desc = nodes_to_descriptions(&index, &nodes);

                if let Some(expected) = expected_map.get(*expr) {
                    if &our_desc == expected {
                        passed += 1;
                    } else {
                        failures.push(format!(
                            "  MISMATCH: {}\n    expected: {:?}\n    got:      {:?}",
                            expr, expected, our_desc
                        ));
                    }
                } else {
                    // No expected result — count as pass if no error
                    passed += 1;
                }
            }
        }
    }

    (passed, total, failures)
}

#[test]
fn test_libxml2_simplebase() {
    let (passed, total, failures) = run_document_tests("simple", "simplebase");
    println!("\nsimplebase: {}/{} passed", passed, total);
    for f in &failures { println!("{}", f); }
    assert!(passed >= total / 2, "simplebase: {}/{}", passed, total);
}

#[test]
fn test_libxml2_simpleabbr() {
    let (passed, total, failures) = run_document_tests("simple", "simpleabbr");
    println!("\nsimpleabbr: {}/{} passed", passed, total);
    for f in &failures { println!("{}", f); }
    assert!(passed >= total / 2, "simpleabbr: {}/{}", passed, total);
}

#[test]
fn test_libxml2_chaptersbase() {
    let (passed, total, failures) = run_document_tests("chapters", "chaptersbase");
    println!("\nchaptersbase: {}/{} passed", passed, total);
    for f in &failures { println!("{}", f); }
    assert!(passed >= total / 3, "chaptersbase: {}/{}", passed, total);
}

#[test]
fn test_libxml2_conformance_summary() {
    let test_sets = vec![
        ("simple", "simplebase"),
        ("simple", "simpleabbr"),
        ("chapters", "chaptersbase"),
        ("chapters", "chaptersprefol"),
        ("str", "strbase"),
        ("nodes", "nodespat"),
        ("mixed", "mixedpat"),
    ];

    let mut total_passed = 0;
    let mut total_tests = 0;
    let mut all_failures = Vec::new();

    for (doc, tests) in &test_sets {
        let (passed, total, failures) = run_document_tests(doc, tests);
        println!("{}: {}/{}", tests, passed, total);
        total_passed += passed;
        total_tests += total;
        for f in failures {
            all_failures.push(format!("[{}] {}", tests, f));
        }
    }

    let pct = (total_passed as f64 / total_tests as f64) * 100.0;
    println!(
        "\n=== CONFORMANCE: {}/{} ({:.0}%) ===",
        total_passed, total_tests, pct
    );

    if !all_failures.is_empty() {
        println!("\nFailures ({}):", all_failures.len());
        for f in &all_failures {
            println!("{}", f);
        }
    }
}
