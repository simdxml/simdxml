# sxq

A fast XML/XPath query tool, powered by SIMD.

`sxq` is to XML what `jq` is to JSON — a small, sharp command-line tool for extracting data from structured documents. It parses XML using SIMD-accelerated structural indexing and evaluates XPath 1.0 expressions against the result.

## Install

```sh
cargo install simdxml-cli
```

## Usage

```sh
sxq XPATH [FILE...]
```

Files are read via mmap for zero-copy access. Large files are parsed in parallel across available cores. If no files are given, reads from stdin.

## Examples

```sh
# Extract text content
sxq '//title' book.xml

# Multiple files with filename headers
sxq '//claim' patents/*.xml

# Raw XML fragments
sxq -r '//svg:path' drawing.svg

# Count matches
sxq -c '//record' huge.xml

# JSON output
sxq -j '//author' papers/*.xml

# Scalar expressions
sxq 'count(//article)' dblp.xml
sxq 'string(//title)' book.xml
sxq 'contains(//abstract, "neural")' paper.xml

# Stdin
curl -s https://example.com/feed.xml | sxq '//item/title'

# Structural statistics
sxq info large.xml

# Control parallelism
sxq -t 4 -c '//record' huge.xml
```

## Options

```
XPATH       XPath 1.0 expression
FILE...     XML files (reads stdin if omitted, - for explicit stdin)

-r          Raw XML fragments instead of text content
-c          Count matching nodes only
-j          JSON array output
-l          Print only filenames with matches
-0          NUL-separated output (for xargs -0)
-W          Include whitespace-only results
-t N        Number of threads for parallel processing
-H          Suppress filename headers in multi-file output
```

## Exit Codes

- **0** — matches found
- **1** — no matches
- **2** — error (parse failure, invalid XPath, I/O error)

## Performance

On a 195 MB PubMed XML file, `sxq` processes in 107ms — 1.6x faster than pugixml and 8x faster than xmllint. On DBLP (5.1 GB), it finishes in 3.2 seconds, 2.6x faster than pugixml.

## License

MIT OR Apache-2.0
