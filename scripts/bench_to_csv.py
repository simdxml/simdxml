#!/usr/bin/env python3
"""Parse criterion benchmark output into CSV.

Usage:
    cargo bench -p simdxml 2>&1 | python3 scripts/bench_to_csv.py > benchmarks.csv
    cargo bench -p simdxml -- "realworld/" 2>&1 | python3 scripts/bench_to_csv.py
"""

import re
import sys
import csv

def parse_criterion_output(lines):
    """Parse criterion stdout into (name, low, median, high, unit) tuples."""
    results = []
    name = None
    for line in lines:
        line = line.rstrip()
        # Match: "Benchmarking group/bench_name"
        m = re.match(r'^Benchmarking (\S+)\s*$', line)
        if m:
            name = m.group(1)
            continue
        # Match timing line: "time:   [123.45 µs 234.56 µs 345.67 µs]"
        if name and 'time:' in line and '[' in line:
            # Skip change% lines
            if '%' in line:
                continue
            m2 = re.search(r'\[([\d.]+)\s+(\S+)\s+([\d.]+)\s+(\S+)\s+([\d.]+)\s+(\S+)\]', line)
            if m2:
                low = float(m2.group(1))
                unit_low = m2.group(2)
                median = float(m2.group(3))
                unit_med = m2.group(4)
                high = float(m2.group(5))
                unit_high = m2.group(6)
                results.append((name, low, median, high, unit_med))
                name = None
    return results

def main():
    lines = sys.stdin.readlines()
    results = parse_criterion_output(lines)

    writer = csv.writer(sys.stdout)
    writer.writerow(['benchmark', 'low', 'median', 'high', 'unit'])
    for name, low, median, high, unit in results:
        writer.writerow([name, f'{low:.4f}', f'{median:.4f}', f'{high:.4f}', unit])

if __name__ == '__main__':
    main()
