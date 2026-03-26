//! Speculative parallel chunked parsing.
//!
//! Split a large XML document into K chunks at safe boundaries (between tags),
//! parse each chunk in parallel to extract structural positions, then merge
//! and compute depth/parent relationships in a single sequential pass.
//!
//! The parallel work (memchr scanning + tag classification) is the expensive
//! part of parsing. The sequential merge (depth/parent computation) is O(n)
//! and very fast since it's just a stack-based walk over tag types.
//!
//! # Algorithm
//!
//! 1. Find K-1 safe split points (positions of `>` between tags)
//! 2. Spawn K threads, each parsing its chunk independently
//! 3. Each thread produces: tag_starts, tag_ends, tag_types, tag_names, text_ranges
//! 4. Merge chunks (concatenate — already in document order)
//! 5. Compute depths and parents in one sequential pass
//! 6. Build CSR indices

use crate::error::Result;
use crate::index::{TagType, TextRange, XmlIndex};
use memchr::memchr;

/// Minimum document size (bytes) to benefit from parallel parsing.
/// Below this, the thread overhead exceeds the parallel speedup.
const MIN_PARALLEL_SIZE: usize = 64 * 1024; // 64 KB

/// Parse XML using multiple threads.
///
/// Falls back to sequential parsing for small documents or when `num_threads <= 1`.
pub fn parse_parallel<'a>(input: &'a [u8], num_threads: usize) -> Result<XmlIndex<'a>> {
    if num_threads <= 1 || input.len() < MIN_PARALLEL_SIZE {
        return crate::index::structural::parse_scalar(input);
    }

    let num_threads = num_threads.min(input.len() / (MIN_PARALLEL_SIZE / 2));

    // Find safe split points
    let splits = find_split_points(input, num_threads);
    let num_chunks = splits.len() + 1;

    // Pre-compute chunk boundaries to avoid closure captures
    let mut boundaries: Vec<(usize, usize)> = Vec::with_capacity(num_chunks);
    for i in 0..num_chunks {
        let start = if i == 0 { 0 } else { splits[i - 1] };
        let end = if i < splits.len() { splits[i] } else { input.len() };
        boundaries.push((start, end));
    }

    // Parse chunks in parallel
    let chunk_results: Vec<ChunkResult> = std::thread::scope(|scope| {
        let handles: Vec<_> = boundaries.iter().map(|&(start, end)| {
            let chunk = &input[start..end];
            scope.spawn(move || parse_chunk(input, chunk, start))
        }).collect();

        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Merge chunk results
    merge_chunks(input, chunk_results)
}

/// A single chunk's parse results (no depth/parent — those need global state).
struct ChunkResult {
    tag_starts: Vec<u64>,
    tag_ends: Vec<u64>,
    tag_types: Vec<TagType>,
    tag_names: Vec<(u64, u16)>,
    text_ranges: Vec<TextRange>,
}

/// Find K-1 safe split points in the input.
///
/// A safe split point is a position just after a `>` that's between tags
/// (not inside a comment, CDATA, or quoted attribute). We scan backward
/// from each desired split position to find the nearest `>`.
fn find_split_points(input: &[u8], num_chunks: usize) -> Vec<usize> {
    let chunk_size = input.len() / num_chunks;
    let mut splits = Vec::with_capacity(num_chunks - 1);

    for i in 1..num_chunks {
        let target = i * chunk_size;
        if let Some(pos) = find_safe_boundary(input, target) {
            // Don't create empty chunks or duplicate splits
            let last = splits.last().copied().unwrap_or(0);
            if pos > last && pos < input.len() {
                splits.push(pos);
            }
        }
    }

    splits
}

/// Find a safe boundary near `target` — a position just after `>` that's between tags.
fn find_safe_boundary(input: &[u8], target: usize) -> Option<usize> {
    let target = target.min(input.len());

    // Search backward from target for a `>` not inside a comment/CDATA
    let search_start = target.saturating_sub(4096); // don't search too far back
    for pos in (search_start..target).rev() {
        if input[pos] == b'>' {
            // Check this isn't inside a comment (-->) or CDATA (]]>)
            // by verifying the next non-whitespace char is `<` or EOF
            let after = pos + 1;
            if after >= input.len() {
                return Some(after);
            }

            // Skip whitespace after >
            let mut check = after;
            while check < input.len() && input[check].is_ascii_whitespace() {
                check += 1;
            }

            // Next meaningful char should be < (start of next tag) or EOF
            if check >= input.len() || input[check] == b'<' {
                return Some(after);
            }
            // Otherwise this > is inside text content — keep searching
        }
    }

    // Fallback: search forward from target
    for pos in target..input.len() {
        if input[pos] == b'>' {
            return Some(pos + 1);
        }
    }

    None
}

/// Parse a single chunk, extracting structural positions.
///
/// `full_input` is the complete XML (for tag name references).
/// `chunk` is the slice being parsed.
/// `chunk_start` is the byte offset of `chunk` within `full_input`.
fn parse_chunk<'a>(
    _full_input: &'a [u8],
    chunk: &'a [u8],
    chunk_start: usize,
) -> ChunkResult {
    let est_tags = chunk.len() / 128;
    let est_text = est_tags / 2;

    let mut result = ChunkResult {
        tag_starts: Vec::with_capacity(est_tags),
        tag_ends: Vec::with_capacity(est_tags),
        tag_types: Vec::with_capacity(est_tags),
        tag_names: Vec::with_capacity(est_tags),
        text_ranges: Vec::with_capacity(est_text),
    };

    let mut pos = 0;
    let mut last_tag_end: usize = 0;

    // We use a simple open-tag counter for text range parent tracking within this chunk.
    // The real parent will be computed during merge. We store u32::MAX as placeholder.
    while let Some(offset) = memchr(b'<', &chunk[pos..]) {
        pos += offset;
        let abs_pos = chunk_start + pos;
        let tag_start = pos;

        // Text content between previous tag end and this tag start
        {
            let text_start = if last_tag_end > 0 { last_tag_end + 1 } else { 0 };
            if text_start < tag_start {
                result.text_ranges.push(TextRange {
                    start: (chunk_start + text_start) as u64,
                    end: abs_pos as u64,
                    parent_tag: u32::MAX, // placeholder — resolved during merge
                });
            }
        }

        if pos + 1 >= chunk.len() {
            break;
        }

        match chunk[pos + 1] {
            b'/' => {
                // Close tag
                pos += 2;
                let name_start = pos;
                while pos < chunk.len() && chunk[pos] != b'>' && !chunk[pos].is_ascii_whitespace() {
                    pos += 1;
                }
                let name_end = pos;

                if let Some(off) = memchr(b'>', &chunk[pos..]) {
                    pos += off;
                } else {
                    break;
                }

                result.tag_starts.push(abs_pos as u64);
                result.tag_ends.push((chunk_start + pos) as u64);
                result.tag_types.push(TagType::Close);
                result.tag_names.push(((chunk_start + name_start) as u64, (name_end - name_start) as u16));

                last_tag_end = pos;
                pos += 1;
            }
            b'!' => {
                if chunk.get(pos + 2..pos + 4) == Some(b"--") {
                    // Comment
                    result.tag_starts.push(abs_pos as u64);
                    result.tag_types.push(TagType::Comment);
                    result.tag_names.push((0, 0));

                    pos += 4;
                    loop {
                        if let Some(off) = memchr(b'-', &chunk[pos..]) {
                            pos += off;
                            if pos + 2 < chunk.len() && &chunk[pos..pos + 3] == b"-->" {
                                pos += 2;
                                break;
                            }
                            pos += 1;
                        } else {
                            pos = chunk.len();
                            break;
                        }
                    }
                    result.tag_ends.push((chunk_start + pos) as u64);
                    last_tag_end = pos;
                    pos += 1;
                } else if chunk.get(pos + 2..pos + 9) == Some(b"[CDATA[") {
                    // CDATA
                    result.tag_starts.push(abs_pos as u64);
                    result.tag_types.push(TagType::CData);
                    result.tag_names.push((0, 0));

                    pos += 9;
                    let content_start = pos;
                    loop {
                        if let Some(off) = memchr(b']', &chunk[pos..]) {
                            pos += off;
                            if pos + 2 < chunk.len() && &chunk[pos..pos + 3] == b"]]>" {
                                if pos > content_start {
                                    result.text_ranges.push(TextRange {
                                        start: (chunk_start + content_start) as u64,
                                        end: (chunk_start + pos) as u64,
                                        parent_tag: u32::MAX,
                                    });
                                }
                                pos += 2;
                                break;
                            }
                            pos += 1;
                        } else {
                            break;
                        }
                    }
                    result.tag_ends.push((chunk_start + pos) as u64);
                    last_tag_end = pos;
                    pos += 1;
                } else {
                    // DOCTYPE or other — skip
                    if let Some(off) = memchr(b'>', &chunk[pos..]) {
                        pos += off;
                    }
                    last_tag_end = pos;
                    pos += 1;
                }
            }
            b'?' => {
                // Processing instruction
                pos += 2;
                let name_start = pos;
                while pos < chunk.len()
                    && chunk[pos] != b'?'
                    && chunk[pos] != b'>'
                    && !chunk[pos].is_ascii_whitespace()
                {
                    pos += 1;
                }
                let name_end = pos;

                result.tag_starts.push(abs_pos as u64);
                result.tag_types.push(TagType::PI);
                result.tag_names.push(((chunk_start + name_start) as u64, (name_end - name_start) as u16));

                while pos + 1 < chunk.len() {
                    if chunk[pos] == b'?' && chunk[pos + 1] == b'>' {
                        pos += 1;
                        break;
                    }
                    pos += 1;
                }
                result.tag_ends.push((chunk_start + pos) as u64);
                last_tag_end = pos;
                pos += 1;
            }
            _ => {
                // Open or self-closing tag
                pos += 1;
                let name_start = pos;
                while pos < chunk.len()
                    && chunk[pos] != b'>'
                    && chunk[pos] != b'/'
                    && !chunk[pos].is_ascii_whitespace()
                {
                    pos += 1;
                }
                let name_end = pos;

                let mut self_closing = false;
                while pos < chunk.len() && chunk[pos] != b'>' {
                    if chunk[pos] == b'/' && pos + 1 < chunk.len() && chunk[pos + 1] == b'>' {
                        self_closing = true;
                        pos += 1;
                        break;
                    }
                    if chunk[pos] == b'"' {
                        pos += 1;
                        if let Some(off) = memchr(b'"', &chunk[pos..]) { pos += off; }
                    } else if chunk[pos] == b'\'' {
                        pos += 1;
                        if let Some(off) = memchr(b'\'', &chunk[pos..]) { pos += off; }
                    }
                    pos += 1;
                }

                if pos >= chunk.len() {
                    break;
                }

                let tag_type = if self_closing { TagType::SelfClose } else { TagType::Open };

                result.tag_starts.push(abs_pos as u64);
                result.tag_ends.push((chunk_start + pos) as u64);
                result.tag_types.push(tag_type);
                result.tag_names.push(((chunk_start + name_start) as u64, (name_end - name_start) as u16));

                last_tag_end = pos;
                pos += 1;
            }
        }
    }

    result
}

/// Merge chunk results into a single XmlIndex.
///
/// Concatenates structural arrays (already in document order), then computes
/// depth and parent relationships in a single sequential pass.
fn merge_chunks<'a>(input: &'a [u8], chunks: Vec<ChunkResult>) -> Result<XmlIndex<'a>> {
    // Count totals for pre-allocation
    let total_tags: usize = chunks.iter().map(|c| c.tag_starts.len()).sum();
    let total_text: usize = chunks.iter().map(|c| c.text_ranges.len()).sum();

    let mut tag_starts = Vec::with_capacity(total_tags);
    let mut tag_ends = Vec::with_capacity(total_tags);
    let mut tag_types = Vec::with_capacity(total_tags);
    let mut tag_names = Vec::with_capacity(total_tags);
    let mut text_ranges = Vec::with_capacity(total_text);

    for chunk in chunks {
        tag_starts.extend_from_slice(&chunk.tag_starts);
        tag_ends.extend_from_slice(&chunk.tag_ends);
        tag_types.extend_from_slice(&chunk.tag_types);
        tag_names.extend_from_slice(&chunk.tag_names);
        text_ranges.extend_from_slice(&chunk.text_ranges);
    }

    // === Fused pass: depth, parents, close_map, post_order, text parents ===
    // Pre-allocated arrays with direct indexing (no Vec::push bounds checks).
    // Text range parents assigned via interleaved linear scan (O(n+t), cache-friendly).
    let _n = tag_types.len();

    // === Fused pass: depth, parents, close_map, post_order, text parents ===
    // Uses fixed-size array stack (no heap, no bounds checks).
    // Speculative parallel depth was benchmarked but thread spawn overhead
    // exceeds savings at typical tag counts (<500K). Sequential is optimal here.
    let n = tag_types.len();

    let mut depths = vec![0u16; n];
    let mut parents = vec![u32::MAX; n];
    let mut close_map = vec![u32::MAX; n];
    let mut post_order = vec![0u32; n];
    let mut depth: u16 = 0;
    let mut post_counter: u32 = 0;
    let mut text_idx = 0;

    const MAX_DEPTH: usize = 4096;
    let mut stack = [0u32; MAX_DEPTH];
    let mut stack_top: usize = 0;

    for i in 0..n {
        let tag_pos = tag_starts[i];
        let current_parent = if stack_top == 0 { u32::MAX } else { stack[stack_top - 1] };

        while text_idx < text_ranges.len() && text_ranges[text_idx].start < tag_pos {
            text_ranges[text_idx].parent_tag = current_parent;
            text_idx += 1;
        }

        match tag_types[i] {
            TagType::Close => {
                if depth > 0 { depth -= 1; }
                if stack_top > 0 {
                    stack_top -= 1;
                    let open_idx = stack[stack_top] as usize;
                    close_map[open_idx] = i as u32;
                    post_order[open_idx] = post_counter;
                }
                post_order[i] = post_counter;
                post_counter += 1;
                depths[i] = depth;
                parents[i] = if stack_top == 0 { u32::MAX } else { stack[stack_top - 1] };
            }
            TagType::Open => {
                depths[i] = depth;
                parents[i] = current_parent;
                stack[stack_top] = i as u32;
                stack_top += 1;
                depth += 1;
            }
            _ => {
                if tag_types[i] == TagType::SelfClose {
                    close_map[i] = i as u32;
                }
                post_order[i] = post_counter;
                post_counter += 1;
                depths[i] = depth;
                parents[i] = current_parent;
            }
        }
    }

    let final_parent = if stack_top == 0 { u32::MAX } else { stack[stack_top - 1] };
    while text_idx < text_ranges.len() {
        text_ranges[text_idx].parent_tag = final_parent;
        text_idx += 1;
    }

    let index = XmlIndex {
        input,
        tag_starts,
        tag_ends,
        tag_types,
        tag_names,
        depths,
        parents,
        text_ranges,
        child_offsets: Vec::new(),
        child_data: Vec::new(),
        text_child_offsets: Vec::new(),
        text_child_data: Vec::new(),
        close_map,      // pre-computed in fused pass
        post_order,     // pre-computed in fused pass
        name_ids: Vec::new(),
        name_table: Vec::new(),
        name_posting: Vec::new(),
    };

    // NOTE: Indices not built here — use ensure_indices() or build_indices()
    // when needed. parse_parallel_indexed() does this automatically.

    Ok(index)
}

/// Parse parallel and immediately build indices (for callers that need them).
pub fn parse_parallel_indexed<'a>(input: &'a [u8], num_threads: usize) -> Result<XmlIndex<'a>> {
    let mut index = parse_parallel(input, num_threads)?;
    if index.tag_count() >= 64 {
        index.build_indices();
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_matches_sequential_small() {
        let xml = b"<root><a>1</a><b>2</b><c>3</c></root>";
        let mut seq = crate::parse(xml).unwrap();
        let mut par = parse_parallel(xml, 2).unwrap(); // falls back to sequential (too small)

        assert_eq!(seq.tag_count(), par.tag_count());
        assert_eq!(seq.tag_types, par.tag_types);
    }

    #[test]
    fn find_safe_boundary_basic() {
        let xml = b"<root><a>text</a><b>more</b></root>";
        let boundary = find_safe_boundary(xml, 15).unwrap();
        // Should find a > followed by <
        assert!(boundary > 0 && boundary <= xml.len());
        assert!(xml[boundary - 1] == b'>');
    }

    #[test]
    fn split_points_reasonable() {
        // Create a ~128KB document
        let mut xml = String::from("<root>");
        for i in 0..2000 {
            xml.push_str(&format!("<item id=\"{}\">content {}</item>", i, i));
        }
        xml.push_str("</root>");
        let bytes = xml.as_bytes();

        let splits = find_split_points(bytes, 4);
        // Should have up to 3 split points for 4 chunks
        assert!(!splits.is_empty());
        assert!(splits.len() <= 3);

        // Each split should be at a > boundary
        for &s in &splits {
            assert!(s > 0);
            assert_eq!(bytes[s - 1], b'>');
        }
    }

    #[test]
    fn parallel_parse_large_doc() {
        // Build a document large enough to trigger parallel parsing
        let mut xml = String::from("<corpus>");
        for i in 0..2000 {
            xml.push_str(&format!(
                "<patent id=\"{}\"><title>Patent {}</title><claims><claim>Claim text {}</claim></claims></patent>",
                i, i, i
            ));
        }
        xml.push_str("</corpus>");
        let bytes = xml.as_bytes();
        assert!(bytes.len() > MIN_PARALLEL_SIZE);

        let mut seq = crate::parse(bytes).unwrap();
        let mut par = parse_parallel(bytes, 4).unwrap();

        // Same number of tags
        assert_eq!(seq.tag_count(), par.tag_count(),
            "tag count: seq={} par={}", seq.tag_count(), par.tag_count());

        // Same tag types
        assert_eq!(seq.tag_types, par.tag_types);

        // Same tag positions
        assert_eq!(seq.tag_starts, par.tag_starts);
        assert_eq!(seq.tag_ends, par.tag_ends);

        // Same depths
        assert_eq!(seq.depths, par.depths);

        // Same parents
        assert_eq!(seq.parents, par.parents);
    }

    #[test]
    fn parallel_xpath_equivalence() {
        let mut xml = String::from("<corpus>");
        for i in 0..2000 {
            xml.push_str(&format!(
                "<patent><title>Title {}</title><claim>Claim {}</claim></patent>",
                i, i
            ));
        }
        xml.push_str("</corpus>");
        let bytes = xml.as_bytes();

        let mut seq = crate::parse(bytes).unwrap();
        let mut par = parse_parallel(bytes, 4).unwrap();
        par.ensure_indices();

        let queries = ["//title", "//claim", "//patent", "/corpus/patent/title"];
        for q in &queries {
            let seq_results = seq.xpath_text(q).unwrap();
            let par_results = par.xpath_text(q).unwrap();
            assert_eq!(seq_results.len(), par_results.len(),
                "count mismatch for {}: seq={} par={}", q, seq_results.len(), par_results.len());
            assert_eq!(seq_results, par_results, "text mismatch for {}", q);
        }
    }

    #[test]
    fn parallel_thread_counts() {
        let mut xml = String::from("<r>");
        for i in 0..3000 {
            xml.push_str(&format!("<item>{}</item>", i));
        }
        xml.push_str("</r>");
        let bytes = xml.as_bytes();

        let mut seq = crate::parse(bytes).unwrap();

        for threads in [1, 2, 4, 8] {
            let mut par = parse_parallel(bytes, threads).unwrap();
            assert_eq!(seq.tag_count(), par.tag_count(),
                "tag count mismatch with {} threads", threads);
            assert_eq!(seq.tag_types, par.tag_types,
                "tag types mismatch with {} threads", threads);
        }
    }

    #[test]
    fn timing_breakdown() {
        // Diagnostic: where does parallel time go?
        let mut xml = String::from("<corpus>");
        for i in 0..5000 {
            xml.push_str(&format!(
                "<patent id=\"{}\"><title>Patent {}</title><claims><claim>Claim text {} with more words</claim></claims></patent>",
                i, i, i
            ));
        }
        xml.push_str("</corpus>");
        let bytes = xml.as_bytes();
        let size_mb = bytes.len() as f64 / 1_048_576.0;

        // Warm up
        let _ = crate::parse(bytes).unwrap();
        let _ = parse_parallel(bytes, 4).unwrap();

        let iters = 20;

        // Sequential baseline
        let start = std::time::Instant::now();
        for _ in 0..iters {
            let _ = crate::parse(bytes).unwrap();
        }
        let seq_total = start.elapsed() / iters;

        // Parallel: time split finding
        let start = std::time::Instant::now();
        for _ in 0..iters {
            let _ = find_split_points(bytes, 4);
        }
        let split_time = start.elapsed() / iters;

        // Parallel: time chunk parsing (4 threads)
        let splits = find_split_points(bytes, 4);
        let num_chunks = splits.len() + 1;
        let mut boundaries: Vec<(usize, usize)> = Vec::with_capacity(num_chunks);
        for i in 0..num_chunks {
            let s = if i == 0 { 0 } else { splits[i - 1] };
            let e = if i < splits.len() { splits[i] } else { bytes.len() };
            boundaries.push((s, e));
        }

        let start = std::time::Instant::now();
        for _ in 0..iters {
            let _: Vec<ChunkResult> = std::thread::scope(|scope| {
                let handles: Vec<_> = boundaries.iter().map(|&(s, e)| {
                    let chunk = &bytes[s..e];
                    scope.spawn(move || parse_chunk(bytes, chunk, s))
                }).collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
        }
        let chunk_time = start.elapsed() / iters;

        // Parallel: time merge
        let chunk_results: Vec<ChunkResult> = std::thread::scope(|scope| {
            let handles: Vec<_> = boundaries.iter().map(|&(s, e)| {
                let chunk = &bytes[s..e];
                scope.spawn(move || parse_chunk(bytes, chunk, s))
            }).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        // Clone-ish: we need to run merge multiple times
        // Just measure one merge
        let start = std::time::Instant::now();
        let _ = merge_chunks(bytes, chunk_results).unwrap();
        let merge_time = start.elapsed();

        // Full parallel
        let start = std::time::Instant::now();
        for _ in 0..iters {
            let _ = parse_parallel(bytes, 4).unwrap();
        }
        let par_total = start.elapsed() / iters;

        let speedup = seq_total.as_secs_f64() / par_total.as_secs_f64();

        println!("\n=== PARALLEL TIMING BREAKDOWN ({:.1} MB, {} chunks) ===", size_mb, num_chunks);
        println!("sequential total:  {:>8.1?}", seq_total);
        println!("parallel total:    {:>8.1?}  ({:.2}x)", par_total, speedup);
        println!("  split finding:   {:>8.1?}  ({:.1}%)", split_time, split_time.as_secs_f64() / par_total.as_secs_f64() * 100.0);
        println!("  chunk parsing:   {:>8.1?}  ({:.1}%)", chunk_time, chunk_time.as_secs_f64() / par_total.as_secs_f64() * 100.0);
        println!("  merge:           {:>8.1?}  ({:.1}%)", merge_time, merge_time.as_secs_f64() / par_total.as_secs_f64() * 100.0);
        println!("  overhead:        {:>8.1?}", par_total.saturating_sub(split_time + chunk_time + merge_time));
    }

    #[test]
    fn parallel_with_attributes() {
        let mut xml = String::from("<root>");
        for i in 0..2000 {
            xml.push_str(&format!(
                r#"<item id="{}" class="c{}" data-value="{}">content</item>"#,
                i, i % 10, i * 100
            ));
        }
        xml.push_str("</root>");
        let bytes = xml.as_bytes();

        let mut seq = crate::parse(bytes).unwrap();
        let mut par = parse_parallel(bytes, 4).unwrap();
        par.ensure_indices();

        assert_eq!(seq.tag_count(), par.tag_count());
        assert_eq!(seq.tag_starts, par.tag_starts);

        // Attribute access should work
        let seq_text = seq.xpath_text("//item").unwrap();
        let par_text = par.xpath_text("//item").unwrap();
        assert_eq!(seq_text, par_text);
    }
}
