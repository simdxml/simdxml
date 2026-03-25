//! Columnar batch XPath evaluation.
//!
//! Evaluate an XPath expression against a batch of XML documents, returning
//! results grouped by document. Amortizes XPath compilation and integrates
//! bloom filtering and lazy parsing for maximum throughput.
//!
//! Results are returned as owned `String`s since the per-document indices
//! are temporary.

use crate::error::Result;
use crate::xpath::CompiledXPath;

/// Evaluate an XPath expression against a batch of documents, returning text results.
///
/// Each document is parsed independently with the compiled XPath evaluated against it.
/// The XPath expression is compiled once and reused across all documents.
pub fn eval_batch_text(
    docs: &[&[u8]],
    xpath: &CompiledXPath,
) -> Result<Vec<Vec<String>>> {
    let mut all_results = Vec::with_capacity(docs.len());

    for &doc in docs {
        let mut index = crate::parse(doc)?;
        index.build_name_index();
        let texts: Vec<String> = xpath.eval_text(&index)?
            .into_iter().map(|s| s.to_string()).collect();
        all_results.push(texts);
    }

    Ok(all_results)
}

/// Evaluate with lazy parsing: only index tags relevant to the XPath query.
pub fn eval_batch_text_lazy(
    docs: &[&[u8]],
    xpath: &CompiledXPath,
) -> Result<Vec<Vec<String>>> {
    let interesting = xpath.interesting_names();
    let mut all_results = Vec::with_capacity(docs.len());

    for &doc in docs {
        let mut index = match &interesting {
            Some(names) => crate::index::lazy::parse_for_query(doc, names)?,
            None => crate::parse(doc)?,
        };
        index.build_name_index();
        let texts: Vec<String> = xpath.eval_text(&index)?
            .into_iter().map(|s| s.to_string()).collect();
        all_results.push(texts);
    }

    Ok(all_results)
}

/// Evaluate with bloom filtering + lazy parsing: skip documents that can't match.
///
/// For each document, first checks a bloom filter to see if it could possibly
/// contain the target tag names. Documents that fail the bloom check get an
/// empty result without any parsing.
pub fn eval_batch_text_bloom(
    docs: &[&[u8]],
    xpath: &CompiledXPath,
) -> Result<Vec<Vec<String>>> {
    let interesting = xpath.interesting_names();
    let target_names: Vec<Vec<u8>> = interesting.as_ref()
        .map(|names| names.iter().map(|n| n.as_bytes().to_vec()).collect())
        .unwrap_or_default();
    let use_bloom = !target_names.is_empty();

    let mut all_results = Vec::with_capacity(docs.len());

    for &doc in docs {
        // Bloom pre-filter
        if use_bloom {
            let bloom = crate::bloom::TagBloom::from_prescan(doc);
            let refs: Vec<&[u8]> = target_names.iter().map(|n| n.as_slice()).collect();
            if !bloom.may_contain_any(&refs) {
                all_results.push(Vec::new());
                continue;
            }
        }

        let mut index = match &interesting {
            Some(names) => crate::index::lazy::parse_for_query(doc, names)?,
            None => crate::parse(doc)?,
        };
        index.build_name_index();
        let texts: Vec<String> = xpath.eval_text(&index)?
            .into_iter().map(|s| s.to_string()).collect();
        all_results.push(texts);
    }

    Ok(all_results)
}

/// Count matching nodes per document without extracting text.
pub fn count_batch(
    docs: &[&[u8]],
    xpath: &CompiledXPath,
) -> Result<Vec<usize>> {
    let mut counts = Vec::with_capacity(docs.len());

    for &doc in docs {
        let mut index = crate::parse(doc)?;
        index.build_name_index();
        let nodes = xpath.eval(&index)?;
        counts.push(nodes.len());
    }

    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_of_one() {
        let doc = b"<root><claim>A device</claim></root>";
        let xpath = CompiledXPath::compile("//claim").unwrap();
        let results = eval_batch_text(&[doc.as_slice()], &xpath).unwrap();
        assert_eq!(results, vec![vec!["A device"]]);
    }

    #[test]
    fn batch_multiple_docs() {
        let docs: Vec<&[u8]> = vec![
            b"<r><claim>First</claim></r>",
            b"<r><claim>Second</claim><claim>Third</claim></r>",
            b"<r><other>No claims</other></r>",
        ];

        let xpath = CompiledXPath::compile("//claim").unwrap();
        let results = eval_batch_text(&docs, &xpath).unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(results[0], vec!["First"]);
        assert_eq!(results[1], vec!["Second", "Third"]);
        assert!(results[2].is_empty());
    }

    #[test]
    fn batch_matches_individual() {
        let docs: Vec<&[u8]> = vec![
            b"<r><a>1</a><b>2</b></r>",
            b"<r><a>3</a></r>",
            b"<r><b>4</b></r>",
        ];

        let xpath = CompiledXPath::compile("//a").unwrap();
        let batch = eval_batch_text(&docs, &xpath).unwrap();

        for (i, &doc) in docs.iter().enumerate() {
            let index = crate::parse(doc).unwrap();
            let individual: Vec<String> = xpath.eval_text(&index).unwrap()
                .into_iter().map(|s| s.to_string()).collect();
            assert_eq!(individual, batch[i], "doc {} mismatch", i);
        }
    }

    #[test]
    fn batch_lazy_matches_full() {
        let docs: Vec<&[u8]> = vec![
            b"<r><claim>A</claim><other>skip</other></r>",
            b"<r><claim>B</claim></r>",
        ];

        let xpath = CompiledXPath::compile("//claim").unwrap();
        let full = eval_batch_text(&docs, &xpath).unwrap();
        let lazy = eval_batch_text_lazy(&docs, &xpath).unwrap();
        assert_eq!(full, lazy);
    }

    #[test]
    fn batch_bloom_skips_irrelevant() {
        let docs: Vec<&[u8]> = vec![
            b"<r><claim>A</claim></r>",
            b"<r><other>no claims</other></r>",
            b"<r><claim>B</claim></r>",
        ];

        let xpath = CompiledXPath::compile("//claim").unwrap();
        let results = eval_batch_text_bloom(&docs, &xpath).unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(results[0], vec!["A"]);
        assert!(results[1].is_empty());
        assert_eq!(results[2], vec!["B"]);
    }

    #[test]
    fn batch_empty() {
        let docs: Vec<&[u8]> = vec![];
        let xpath = CompiledXPath::compile("//claim").unwrap();
        let results = eval_batch_text(&docs, &xpath).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn batch_predicate() {
        let docs: Vec<&[u8]> = vec![
            br#"<r><claim type="independent">A</claim><claim type="dependent">B</claim></r>"#,
            br#"<r><claim type="dependent">C</claim></r>"#,
        ];

        let xpath = CompiledXPath::compile("//claim[@type='independent']").unwrap();
        let results = eval_batch_text(&docs, &xpath).unwrap();
        assert_eq!(results[0], vec!["A"]);
        assert!(results[1].is_empty());
    }

    #[test]
    fn count_batch_works() {
        let docs: Vec<&[u8]> = vec![
            b"<r><a/><a/><b/></r>",
            b"<r><a/></r>",
            b"<r><b/></r>",
        ];

        let xpath = CompiledXPath::compile("//a").unwrap();
        let counts = count_batch(&docs, &xpath).unwrap();
        assert_eq!(counts, vec![2, 1, 0]);
    }

    #[test]
    fn batch_bloom_all_match() {
        let docs: Vec<&[u8]> = vec![
            b"<r><claim>A</claim></r>",
            b"<r><claim>B</claim></r>",
        ];

        let xpath = CompiledXPath::compile("//claim").unwrap();
        let bloom_results = eval_batch_text_bloom(&docs, &xpath).unwrap();
        let full_results = eval_batch_text(&docs, &xpath).unwrap();
        assert_eq!(bloom_results, full_results);
    }
}
