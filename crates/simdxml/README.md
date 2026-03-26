# simdxml

SIMD-accelerated XML parser with full XPath 1.0 support.

Uses a two-pass structural indexing architecture (adapted from [simdjson](https://simdjson.org/)) to parse XML into flat arrays instead of a DOM tree, then evaluates XPath expressions against those arrays with cache-friendly access patterns.

## Performance

Benchmarked on Apple M4 Max (NEON) and AMD Ryzen 9 3950X (AVX2) against pugixml (C++), libxml2 (C), and others.

**Where simdxml wins** — large files and attribute-dense XML:

| Benchmark | simdxml | pugixml | Speedup |
|-----------|---------|---------|---------|
| DBLP 5.1 GB | 3.2s | 8.3s | **2.6x** |
| Attr-heavy 10 MB (ARM) | 7.8ms | 13.1ms | **1.7x** |
| Attr-heavy 10 MB (x86) | 14.7ms | 38.8ms | **2.6x** |

**Where it's close** — text-heavy XML, simple queries:

| Benchmark | simdxml | pugixml | |
|-----------|---------|---------|---|
| PubMed descendant query | 130ms | 174ms | 1.3x faster |
| PubMed `count()` | 133ms | 117ms | pugixml wins |
| PubMed `contains()` | 162ms | 148ms | pugixml wins |

pugixml's single-pass DOM build has lower overhead for queries where parse time dominates. simdxml's advantage grows with file size and attribute density.

## Quick Start

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
assert_eq!(titles, vec!["Rust"]);
```

## Features

- **SIMD structural indexing** — NEON (aarch64), SSE4.2/AVX2 (x86_64), scalar fallback
- **Full XPath 1.0** — all 13 axes, predicates, functions, operators. 327/327 libxml2 conformance, 1015/1023 pugixml conformance
- **Flat-array index** — ~16 bytes/tag vs ~35 for a DOM tree. Cache-friendly for repeated queries
- **Parallel parsing** — split large files across cores with `parallel::parse_parallel`
- **Lazy indexing** — only build structures needed by the query
- **Bloom filter prescan** — skip files that can't match before parsing
- **Persistent indices** — serialize to `.sxi` files, mmap for instant reload
- **Zero-copy** — borrows input bytes, no allocation for tag names or text content

## XPath Support

All XPath 1.0 expressions work, including:

```rust
// Location paths
index.xpath("//book/title")?;
index.xpath("//book[@lang='en']/title")?;
index.xpath("//book[position() < 3]")?;

// All 13 axes
index.xpath("//title/ancestor::library")?;
index.xpath("//book/following-sibling::book")?;

// Functions
index.eval("count(//book)")?;          // -> Number(42)
index.eval("string(//title)")?;        // -> String("Rust")
index.eval("contains(//title, 'Ru')")?; // -> Boolean(true)

// Unions
index.xpath("//title | //author")?;
```

## Batch Processing

Process many documents with compiled queries and automatic parallelism:

```rust
use simdxml::{parse, batch, CompiledXPath};

let docs: Vec<&[u8]> = vec![
    b"<r><claim>First</claim></r>",
    b"<r><claim>Second</claim></r>",
];
let query = CompiledXPath::compile("//claim").unwrap();

// Parallel batch with bloom filter prescan
let results = batch::eval_batch_text_bloom(&docs, &query).unwrap();
```

## CLI

The `sxq` command-line tool provides fast XML/XPath queries from the terminal:

```sh
sxq '//title' book.xml
sxq -c '//claim' patents/*.xml       # count matches
sxq -r '//path' drawing.svg          # raw XML output
sxq -j '//author' papers/*.xml       # JSON output
cat data.xml | sxq '//record/@id'    # stdin
sxq 'count(//article)' dblp.xml      # scalar expressions
sxq info large.xml                   # structural statistics
```

Install: `cargo install simdxml-cli`

## Platform Support

| Platform | SIMD | Status |
|----------|------|--------|
| aarch64 (Apple Silicon, ARM) | NEON 128-bit | Production |
| x86_64 | AVX2 256-bit / SSE4.2 128-bit | Production |
| Other | Scalar fallback | Working |

## License

MIT OR Apache-2.0
