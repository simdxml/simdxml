use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::fs;

const BENCH_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/bench");

fn load(name: &str) -> Vec<u8> {
    fs::read(format!("{}/{}", BENCH_DIR, name)).unwrap()
}

// ============================================================================
// Parse benchmarks: simdxml vs quick-xml vs roxmltree vs xml-rs
// ============================================================================

fn bench_parse_throughput(c: &mut Criterion) {
    let files = [
        ("patent_medium", "patent_medium.xml"),
        ("patent_large", "patent_large.xml"),
        ("patent_xlarge", "patent_xlarge.xml"),
        ("attrheavy_large", "attrheavy_large.xml"),
        ("textheavy_large", "textheavy_large.xml"),
        ("nested_large", "nested_large.xml"),
    ];

    for (label, filename) in &files {
        let data = load(filename);
        let data_str = std::str::from_utf8(&data).unwrap();

        let mut group = c.benchmark_group(format!("parse/{}", label));
        group.throughput(Throughput::Bytes(data.len() as u64));

        // simdxml (our parser)
        group.bench_function("simdxml", |b| {
            b.iter(|| {
                let _ = simdxml::parse(&data).unwrap();
            });
        });

        // quick-xml: streaming pull parser, drain all events
        group.bench_function("quick_xml", |b| {
            b.iter(|| {
                let mut reader = quick_xml::Reader::from_str(data_str);
                loop {
                    match reader.read_event() {
                        Ok(quick_xml::events::Event::Eof) => break,
                        Ok(_) => {}
                        Err(e) => panic!("{}", e),
                    }
                }
            });
        });

        // roxmltree: full DOM tree parse
        group.bench_function("roxmltree", |b| {
            b.iter(|| {
                let _ = roxmltree::Document::parse(data_str).unwrap();
            });
        });

        // xml-rs: streaming event parser (slow baseline)
        // Only bench on medium to avoid very long runs
        if data.len() < 200_000 {
            group.bench_function("xml_rs", |b| {
                b.iter(|| {
                    let parser = xml::reader::EventReader::new(data.as_slice());
                    for event in parser {
                        let _ = event;
                    }
                });
            });
        }

        group.finish();
    }
}

// ============================================================================
// Shape comparison: how different XML shapes affect parse throughput
// ============================================================================

fn bench_parse_shapes(c: &mut Criterion) {
    let shapes = [
        ("patent", "patent_large.xml"),
        ("attrheavy", "attrheavy_large.xml"),
        ("textheavy", "textheavy_large.xml"),
        ("nested", "nested_large.xml"),
    ];

    let mut group = c.benchmark_group("shape/simdxml");
    for (shape, filename) in &shapes {
        let data = load(filename);
        group.throughput(Throughput::Bytes(data.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(shape), &data, |b, data| {
            b.iter(|| {
                let _ = simdxml::parse(data).unwrap();
            });
        });
    }
    group.finish();
}

// ============================================================================
// Scaling: throughput vs document size (small → xlarge)
// ============================================================================

fn bench_parse_scaling(c: &mut Criterion) {
    let sizes = [
        ("1KB", "patent_small.xml"),
        ("100KB", "patent_medium.xml"),
        ("1MB", "patent_large.xml"),
        ("10MB", "patent_xlarge.xml"),
    ];

    let mut group = c.benchmark_group("scaling/simdxml");
    for (size_label, filename) in &sizes {
        let data = load(filename);
        group.throughput(Throughput::Bytes(data.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size_label), &data, |b, data| {
            b.iter(|| {
                let _ = simdxml::parse(data).unwrap();
            });
        });
    }
    group.finish();

    let mut group = c.benchmark_group("scaling/quick_xml");
    for (size_label, filename) in &sizes {
        let data = load(filename);
        let data_str = std::str::from_utf8(&data).unwrap().to_string();
        group.throughput(Throughput::Bytes(data.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(size_label),
            &data_str,
            |b, data_str| {
                b.iter(|| {
                    let mut reader = quick_xml::Reader::from_str(data_str);
                    loop {
                        match reader.read_event() {
                            Ok(quick_xml::events::Event::Eof) => break,
                            Ok(_) => {}
                            Err(e) => panic!("{}", e),
                        }
                    }
                });
            },
        );
    }
    group.finish();
}

// ============================================================================
// XPath evaluation benchmarks
// ============================================================================

fn bench_xpath(c: &mut Criterion) {
    let data = load("patent_large.xml");
    let index = simdxml::parse(&data).unwrap();

    let queries = [
        ("descendant", "//claim"),
        ("child_path", "/corpus/patent/claims/claim"),
        ("predicate", "//claim[@type='independent']"),
        ("text", "//title/text()"),
        ("wildcard", "//patent/*"),
    ];

    let mut group = c.benchmark_group("xpath");
    for (name, expr) in &queries {
        let compiled = simdxml::CompiledXPath::compile(expr).unwrap();
        group.bench_function(*name, |b| {
            b.iter(|| {
                let _ = compiled.eval(&index).unwrap();
            });
        });
    }
    group.finish();
}

// ============================================================================
// End-to-end: parse + xpath (the full pipeline)
// ============================================================================

fn bench_end_to_end(c: &mut Criterion) {
    let data = load("patent_large.xml");
    let data_str = std::str::from_utf8(&data).unwrap();

    let mut group = c.benchmark_group("e2e/claim_extract");
    group.throughput(Throughput::Bytes(data.len() as u64));

    // simdxml: parse + compiled xpath (avoids re-parsing expression)
    let compiled = simdxml::CompiledXPath::compile("//claim").unwrap();
    group.bench_function("simdxml", |b| {
        b.iter(|| {
            let index = simdxml::parse(&data).unwrap();
            let _ = compiled.eval_text(&index).unwrap();
        });
    });

    // quick-xml: streaming extraction (idiomatic for this use case)
    group.bench_function("quick_xml", |b| {
        b.iter(|| {
            let mut reader = quick_xml::Reader::from_str(data_str);
            let mut in_claim = false;
            let mut claims: Vec<String> = Vec::new();
            let mut buf = String::new();
            loop {
                match reader.read_event() {
                    Ok(quick_xml::events::Event::Start(e)) if e.name().as_ref() == b"claim" => {
                        in_claim = true;
                        buf.clear();
                    }
                    Ok(quick_xml::events::Event::Text(e)) if in_claim => {
                        buf.push_str(&e.unescape().unwrap());
                    }
                    Ok(quick_xml::events::Event::End(e)) if e.name().as_ref() == b"claim" => {
                        in_claim = false;
                        claims.push(std::mem::take(&mut buf));
                    }
                    Ok(quick_xml::events::Event::Eof) => break,
                    Ok(_) => {}
                    Err(e) => panic!("{}", e),
                }
            }
            claims
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_parse_throughput,
    bench_parse_shapes,
    bench_parse_scaling,
    bench_xpath,
    bench_end_to_end,
);
criterion_main!(benches);
