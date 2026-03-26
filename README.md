# simdxml

SIMD-accelerated XML parser with full XPath 1.0 support.

Parses XML into flat arrays instead of a DOM tree using structural indexing adapted from [simdjson](https://simdjson.org/). Evaluates XPath expressions against those arrays with cache-friendly access patterns. Includes `sxq`, a fast command-line tool for XML/XPath queries.

## Performance

Benchmarked on Apple M4 Max (NEON) and AMD Ryzen 9 3950X (AVX2).

**End-to-end XPath queries (M4 Max, NEON):**

| Benchmark | sxq | pugixml | xmllint | vs pugixml |
|-----------|-----|---------|---------|-----------|
| DBLP 5.1 GB (`//article`) | 3.2s | 8.3s | — | **2.6x** |
| PubMed 195 MB (`//MeshHeading`) | 130ms | 174ms | 831ms | **1.3x** |
| Attr-heavy 10 MB (`//record`) | 7.8ms | 13.1ms | 138ms | **1.7x** |

**Attribute-heavy XML (Ryzen 9 3950X, AVX2):**

| Tool | Time | vs sxq |
|------|------|--------|
| sxq | 14.7ms | — |
| pugixml | 38.8ms | 2.6x slower |
| xmllint | 257.7ms | 17.5x slower |

**Library parse throughput vs Rust parsers** (M4 Max, criterion, parse only):

| File | simdxml | quick-xml | roxmltree | vs quick-xml |
|------|---------|-----------|-----------|-------------|
| PubMed 195 MB (1 thread) | 136ms | 153ms | — | **1.1x** |
| PubMed 195 MB (4 threads) | 92ms | — | — | — |
| Attr-heavy 1 MB | 216µs | 859µs | 3,909µs | **4.0x** |
| Patent XML 1 MB | 218µs | 289µs | 1,921µs | **1.3x** |
| Tiger SVG 69 KB | 20µs | 31µs | 153µs | **1.6x** |

Wins are largest on attribute-dense XML (where SIMD quote masking shines) and multi-gigabyte files (where flat-array memory efficiency dominates). pugixml wins on scalar aggregation queries (`count()`) and text predicates (`contains()`) where its single-pass DOM build has lower overhead. On small files (<1 MB), all native tools are within startup noise. Unlike quick-xml (streaming events) and roxmltree (read-only DOM), simdxml produces a queryable structural index with XPath support.

## sxq — CLI tool

```sh
cargo install simdxml-cli
```

```sh
sxq '//title' book.xml
sxq '//claim[@type="independent"]' patents/*.xml
sxq -c '//record' huge.xml
sxq -r '//path' drawing.svg
sxq -j '//author' papers/*.xml
sxq 'count(//article)' dblp.xml
cat feed.xml | sxq '//item/title'
sxq info large.xml
```

Files are mmap'd. Large files are parsed in parallel. 684 KB binary, zero dependencies.

## Library

```sh
cargo add simdxml
```

```rust
use simdxml::{parse, CompiledXPath};

let xml = b"<library><book><title>Rust</title></book></library>";
let index = parse(xml).unwrap();

// One-shot query
let titles = index.xpath_text("//title").unwrap();
assert_eq!(titles, vec!["Rust"]);

// Compiled query (reusable across documents)
let query = CompiledXPath::compile("//title").unwrap();
let titles = query.eval_text(&index).unwrap();

// Scalar expressions
let mut index = parse(xml).unwrap();
let count = index.eval("count(//book)").unwrap(); // Number(1)
```

## XPath 1.0

Full support for all 13 axes, predicates, functions, operators, and unions. 327/327 libxml2 conformance tests, 1015/1023 pugixml conformance tests.

All axes: `child`, `descendant`, `descendant-or-self`, `parent`, `ancestor`, `ancestor-or-self`, `following-sibling`, `preceding-sibling`, `following`, `preceding`, `self`, `attribute`, `namespace`

Functions: `string()`, `count()`, `sum()`, `boolean()`, `not()`, `contains()`, `starts-with()`, `substring()`, `concat()`, `normalize-space()`, `translate()`, `string-length()`, `number()`, `floor()`, `ceiling()`, `round()`, `id()`, `name()`, `local-name()`, `namespace-uri()`, `position()`, `last()`, `true()`, `false()`, `lang()`

## Architecture

Two-pass structural indexing (no DOM tree):

1. **Parse** — Scan XML bytes with SIMD-accelerated character classification, build flat arrays (tag offsets, types, depths, parents, text ranges). ~16 bytes/tag vs ~35 for a DOM node.
2. **Query** — Evaluate XPath against the arrays using CSR indices, pre/post-order numbering for O(1) ancestor checks, and inverted name posting lists.

Additional features: parallel parsing across cores, lazy index building, bloom filter prescan for batch skip, persistent `.sxi` index files.

## Platform Support

| Platform | SIMD Backend | Status |
|----------|-------------|--------|
| aarch64 (Apple Silicon, ARM) | NEON 128-bit | Production |
| x86_64 | AVX2 256-bit / SSE4.2 128-bit | Production |
| Other | Scalar (memchr-accelerated) | Working |

## Project Structure

```
crates/simdxml/      Rust library (crates.io: simdxml)
crates/simdxml-cli/  CLI tool (crates.io: simdxml-cli, binary: sxq)
bench/               Benchmark scripts and pugixml comparison shim
testdata/            Conformance test suites (libxml2, pugixml)
```

## License

MIT OR Apache-2.0
