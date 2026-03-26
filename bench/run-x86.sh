#!/usr/bin/env bash
set -euo pipefail

# x86 benchmark script for porthos (Ryzen 9 3950X, 16 cores, AVX2)
#
# Prerequisites:
#   sudo apt install -y libxml2-utils libpugixml-dev hyperfine build-essential
#   cargo build --release -p simdxml-cli
#   c++ -O2 -std=c++17 -I/usr/include bench/pugixml-xpath.cpp -lpugixml -o bench/pugixml-xpath
#
# Data should be at ~/simdxml-bench/testdata/ (SCP'd from Mac)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SXQ="$ROOT/target/release/sxq"
PUGIXML="$SCRIPT_DIR/pugixml-xpath"
DATA="${SIMDXML_DATA:-$HOME/simdxml-bench/testdata}"

# Build if needed
if [ ! -f "$SXQ" ]; then
    echo "Building sxq..."
    cargo build --release -p simdxml-cli --manifest-path="$ROOT/Cargo.toml"
fi

# Build pugixml shim if needed
if [ ! -f "$PUGIXML" ]; then
    echo "Building pugixml shim..."
    c++ -O2 -std=c++17 bench/pugixml-xpath.cpp -lpugixml -o "$PUGIXML" 2>/dev/null || \
    c++ -O2 -std=c++17 -I/usr/include bench/pugixml-xpath.cpp -lpugixml -o "$PUGIXML"
fi

echo "============================================"
echo "  sxq x86 benchmark suite ($(uname -m))"
echo "  $(grep 'model name' /proc/cpuinfo 2>/dev/null | head -1 | cut -d: -f2 | xargs)"
echo "  $(nproc) threads"
echo "============================================"
echo ""

# Check data
PUBMED="$DATA/pubmed26n0001.xml"
ATTR="$DATA/attrheavy_xlarge.xml"
PATENT_US="$DATA/patent-us.xml"

for f in "$PUBMED" "$ATTR" "$PATENT_US"; do
    if [ -f "$f" ]; then
        echo "  $(basename "$f"): $(ls -lh "$f" | awk '{print $5}')"
    else
        echo "  $(basename "$f"): MISSING"
    fi
done
echo ""

W=5; R=20

# ================================================================
# 1. PubMed per-query workload (the paper table)
# ================================================================
if [ -f "$PUBMED" ]; then
    echo ">>> 1. PubMed per-query workload (195 MB)"
    echo ""

    for label_query in \
        "Q1-descendant-330K://MeshHeading" \
        "Q2-child-path-30K://Article/ArticleTitle" \
        "Q3-attr-pred-22K://DescriptorName[@MajorTopicYN='Y']" \
        "Q4-scalar:count(//PubmedArticle)" \
        "Q5-deep-path-76K://PubmedArticle/MedlineCitation/Article/AuthorList/Author" \
        "Q6-contains-87://ArticleTitle[contains(.,'cancer')]"; do
        label="${label_query%%:*}"
        query="${label_query#*:}"

        echo "--- $label ---"
        hyperfine --warmup $W --runs $R -i \
            -n "sxq" "$SXQ -c '$query' $PUBMED" \
            -n "pugixml" "$PUGIXML '$query' $PUBMED > /dev/null" \
            -n "xmllint" "xmllint --xpath '$query' $PUBMED > /dev/null" \
            2>&1 || true
        echo ""
    done
fi

# ================================================================
# 2. Attribute-heavy 10MB (SIMD sweet spot)
# ================================================================
if [ -f "$ATTR" ]; then
    echo ">>> 2. Attribute-heavy 10MB — //record"
    echo ""
    hyperfine --warmup $W --runs $R -i \
        -n "sxq" "$SXQ -r '//record' $ATTR | wc -c" \
        -n "pugixml" "$PUGIXML '//record' $ATTR | wc -c" \
        -n "xmllint" "xmllint --xpath '//record' $ATTR | wc -c" \
        2>&1 || true
    echo ""
fi

# ================================================================
# 3. Parse throughput scaling (PubMed)
# ================================================================
if [ -f "$PUBMED" ]; then
    echo ">>> 3. Parse throughput scaling (PubMed 195MB)"
    echo ""
    for t in 1 2 4 8 16; do
        echo -n "  $t thread(s): "
        hyperfine -N --warmup 3 --runs 10 -i \
            "$SXQ -t $t -c '//*' $PUBMED" 2>&1 | grep "Time" || true
    done
    echo ""
fi

# ================================================================
# 4. Criterion parse benchmarks (in-process, no CLI overhead)
# ================================================================
echo ">>> 4. Criterion parse benchmarks"
echo "   Run separately: cargo bench --bench parse_bench -- 'large/'"
echo ""

# ================================================================
# 5. Small file baseline (86KB patent)
# ================================================================
if [ -f "$PATENT_US" ]; then
    echo ">>> 5. Small file baseline (patent-us.xml, 86KB)"
    echo ""
    hyperfine --warmup $W --runs $R -i \
        -n "sxq" "$SXQ '//claim-text' $PATENT_US | wc -c" \
        -n "pugixml" "$PUGIXML '//claim-text' $PATENT_US | wc -c" \
        -n "xmllint" "xmllint --xpath '//claim-text' $PATENT_US | wc -c" \
        2>&1 || true
fi

echo ""
echo "============================================"
echo "  Done."
echo "============================================"
