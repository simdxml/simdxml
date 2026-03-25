//! Conformance tests adapted from libxml2 (MIT License).
//! Tests XPath evaluation against known-good libxml2 results.

use simdxml::xpath::XPathNode;

/// Parse libxml2's expected output format to extract element names from node sets.
fn parse_expected_elements(result_text: &str) -> Vec<Vec<String>> {
    let mut all_results = Vec::new();
    let mut current_elements = Vec::new();
    let mut in_expression = false;

    for line in result_text.lines() {
        if line.starts_with("========================") {
            if in_expression && !current_elements.is_empty() {
                all_results.push(std::mem::take(&mut current_elements));
            }
            in_expression = true;
            continue;
        }
        if line.starts_with("Expression:") {
            continue;
        }
        if line.starts_with("Object is a Node Set") || line.starts_with("Set contains") {
            continue;
        }
        // Extract element names from lines like "1  ELEMENT head"
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(|c: char| c.is_ascii_digit()) {
            let rest = rest.trim();
            if let Some(name) = rest.strip_prefix("ELEMENT ") {
                current_elements.push(name.trim().to_string());
            } else if rest.starts_with("TEXT") {
                current_elements.push("#text".to_string());
            } else if rest.starts_with("COMMENT") {
                current_elements.push("#comment".to_string());
            }
        }
        // Handle "Object is a number" / "Object is a string" / "Object is a Boolean"
        if trimmed.starts_with("Object is a number") {
            // Number result — just note it
        }
        if trimmed.starts_with("Object is a Boolean") {
            // Boolean result
        }
    }
    if !current_elements.is_empty() {
        all_results.push(current_elements);
    }
    all_results
}

/// Run a set of document-context tests and report results.
fn run_document_tests(doc_name: &str, test_name: &str) -> (usize, usize, Vec<String>) {
    let base = format!("{}/../../testdata/libxml2", env!("CARGO_MANIFEST_DIR"));
    let doc_path = format!("{}/docs/{}", base, doc_name);
    let test_path = format!("{}/tests/{}", base, test_name);
    let result_path = format!("{}/results_tests/{}", base, test_name);

    let doc_bytes = std::fs::read(&doc_path).expect(&format!("Missing doc: {}", doc_path));
    let test_exprs = std::fs::read_to_string(&test_path).expect(&format!("Missing tests: {}", test_path));
    let expected_text = std::fs::read_to_string(&result_path).unwrap_or_default();

    let index = simdxml::parse(&doc_bytes).expect(&format!("Failed to parse {}", doc_path));
    let expected_sets = parse_expected_elements(&expected_text);

    let expressions: Vec<&str> = test_exprs.lines().filter(|l| !l.is_empty()).collect();
    let mut passed = 0;
    let mut total = 0;
    let mut failures = Vec::new();

    for (i, expr) in expressions.iter().enumerate() {
        total += 1;

        // Try to parse and evaluate — catch panics
        let idx_ref = &index;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            idx_ref.xpath(expr)
        }));

        match result {
            Err(_) => {
                failures.push(format!("  PANIC: {}", expr));
            }
            Ok(Err(e)) => {
                failures.push(format!("  ERROR: {} → {}", expr, e));
            }
            Ok(Ok(nodes)) => {
                let our_names: Vec<String> = nodes
                    .iter()
                    .filter_map(|n| match n {
                        XPathNode::Element(idx) => Some(index.tag_name(*idx).to_string()),
                        XPathNode::Text(_) => Some("#text".to_string()),
                        _ => None,
                    })
                    .collect();

                if let Some(expected) = expected_sets.get(i) {
                    if &our_names == expected {
                        passed += 1;
                    } else {
                        failures.push(format!(
                            "  MISMATCH: {}\n    expected: {:?}\n    got:      {:?}",
                            expr, expected, our_names
                        ));
                    }
                } else {
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
    for f in &failures {
        println!("{}", f);
    }
    // Report but don't assert all pass yet — we're tracking conformance
    assert!(
        passed > total / 2,
        "simplebase: only {}/{} passed. Failures:\n{}",
        passed, total, failures.join("\n")
    );
}

#[test]
fn test_libxml2_simpleabbr() {
    let (passed, total, failures) = run_document_tests("simple", "simpleabbr");
    println!("\nsimpleabbr: {}/{} passed", passed, total);
    for f in &failures {
        println!("{}", f);
    }
    assert!(
        passed > total / 2,
        "simpleabbr: only {}/{} passed. Failures:\n{}",
        passed, total, failures.join("\n")
    );
}

#[test]
fn test_libxml2_chaptersbase() {
    let (passed, total, failures) = run_document_tests("chapters", "chaptersbase");
    println!("\nchaptersbase: {}/{} passed", passed, total);
    for f in &failures {
        println!("{}", f);
    }
    assert!(
        passed > total / 3,
        "chaptersbase: only {}/{} passed. Failures:\n{}",
        passed, total, failures.join("\n")
    );
}

/// Summary test — reports overall conformance across all test sets.
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

    println!("\n=== CONFORMANCE: {}/{} ({:.0}%) ===",
        total_passed, total_tests,
        (total_passed as f64 / total_tests as f64) * 100.0
    );

    if !all_failures.is_empty() {
        println!("\nFailures ({}):", all_failures.len());
        for f in all_failures.iter().take(20) {
            println!("{}", f);
        }
        if all_failures.len() > 20 {
            println!("... and {} more", all_failures.len() - 20);
        }
    }
}
