//! SSE4.2 (x86_64) structural character classifier.
//!
//! Processes 64 bytes at a time (4x 16-byte SSE registers) to produce
//! bitmasks for '<' and '>' positions, with quote masking to ignore
//! structural characters inside attribute values.

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

use super::StructuralIndex;

/// Classify structural characters using SSE4.2 vector instructions.
/// Processes the entire input in one pass, producing bitmasks for Stage 2.
///
/// # Safety
/// Caller must ensure SSE4.2 is available (checked by dispatcher in mod.rs).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
pub unsafe fn classify_sse42(input: &[u8]) -> StructuralIndex {
    let len = input.len();
    let num_chunks = (len + 63) / 64;
    let mut lt_bits = vec![0u64; num_chunks];
    let mut gt_bits = vec![0u64; num_chunks];

    // Active quote byte carried across chunks: 0, b'"', or b'\''.
    let mut quote: u8 = 0;

    let full_chunks = len / 64;

    // Splat comparison targets
    let v_lt = _mm_set1_epi8(b'<' as i8);
    let v_gt = _mm_set1_epi8(b'>' as i8);
    let v_dquote = _mm_set1_epi8(b'"' as i8);
    let v_squote = _mm_set1_epi8(b'\'' as i8);

    for chunk in 0..full_chunks {
        let base = chunk * 64;
        let ptr = input.as_ptr().add(base) as *const __m128i;

        // Load 4x16 bytes
        let v0 = _mm_loadu_si128(ptr);
        let v1 = _mm_loadu_si128(ptr.add(1));
        let v2 = _mm_loadu_si128(ptr.add(2));
        let v3 = _mm_loadu_si128(ptr.add(3));

        // Compare for each structural character (0xFF or 0x00 per byte)
        let lt0 = _mm_cmpeq_epi8(v0, v_lt);
        let lt1 = _mm_cmpeq_epi8(v1, v_lt);
        let lt2 = _mm_cmpeq_epi8(v2, v_lt);
        let lt3 = _mm_cmpeq_epi8(v3, v_lt);

        let gt0 = _mm_cmpeq_epi8(v0, v_gt);
        let gt1 = _mm_cmpeq_epi8(v1, v_gt);
        let gt2 = _mm_cmpeq_epi8(v2, v_gt);
        let gt3 = _mm_cmpeq_epi8(v3, v_gt);

        let dq0 = _mm_cmpeq_epi8(v0, v_dquote);
        let dq1 = _mm_cmpeq_epi8(v1, v_dquote);
        let dq2 = _mm_cmpeq_epi8(v2, v_dquote);
        let dq3 = _mm_cmpeq_epi8(v3, v_dquote);

        let sq0 = _mm_cmpeq_epi8(v0, v_squote);
        let sq1 = _mm_cmpeq_epi8(v1, v_squote);
        let sq2 = _mm_cmpeq_epi8(v2, v_squote);
        let sq3 = _mm_cmpeq_epi8(v3, v_squote);

        // Convert SSE masks to u64 bitmasks using native movemask
        let lt_mask = movemask_64(lt0, lt1, lt2, lt3);
        let gt_mask = movemask_64(gt0, gt1, gt2, gt3);
        let dq_mask = movemask_64(dq0, dq1, dq2, dq3);
        let sq_mask = movemask_64(sq0, sq1, sq2, sq3);

        let (masked_lt, masked_gt) =
            super::resolve_quotes(lt_mask, gt_mask, dq_mask, sq_mask, &mut quote);

        lt_bits[chunk] = masked_lt;
        gt_bits[chunk] = masked_gt;
    }

    // Handle remaining bytes (< 64) with scalar
    let remaining_start = full_chunks * 64;
    if remaining_start < len {
        let chunk_idx = full_chunks;
        let mut lt: u64 = 0;
        let mut gt: u64 = 0;

        for i in remaining_start..len {
            let byte = input[i];
            let bit = (i - remaining_start) as u32;

            if byte == b'<' {
                lt |= 1u64 << bit;
                quote = 0;
                continue;
            }
            if quote != 0 {
                if byte == quote { quote = 0; }
                continue;
            }
            match byte {
                b'>' => gt |= 1u64 << bit,
                b'"' => quote = b'"',
                b'\'' => quote = b'\'',
                _ => {}
            }
        }

        if chunk_idx < lt_bits.len() {
            lt_bits[chunk_idx] = lt;
            gt_bits[chunk_idx] = gt;
        }
    }

    StructuralIndex { lt_bits, gt_bits, len }
}

/// Combine four 16-byte SSE movemask results into a single u64 bitmask.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
#[inline]
unsafe fn movemask_64(v0: __m128i, v1: __m128i, v2: __m128i, v3: __m128i) -> u64 {
    let m0 = _mm_movemask_epi8(v0) as u16 as u64;
    let m1 = _mm_movemask_epi8(v1) as u16 as u64;
    let m2 = _mm_movemask_epi8(v2) as u16 as u64;
    let m3 = _mm_movemask_epi8(v3) as u16 as u64;
    m0 | (m1 << 16) | (m2 << 32) | (m3 << 48)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(input: &[u8]) -> StructuralIndex {
        assert!(is_x86_feature_detected!("sse4.2"));
        unsafe { classify_sse42(input) }
    }

    #[test]
    fn test_simple_tags() {
        let input = b"<root><child>text</child></root>";
        let idx = classify(input);
        let lt_pos: Vec<usize> = idx.lt_positions().collect();
        let gt_pos: Vec<usize> = idx.gt_positions().collect();
        assert_eq!(lt_pos, vec![0, 6, 17, 25]);
        assert_eq!(gt_pos, vec![5, 12, 24, 31]);
    }

    #[test]
    fn test_quoted_gt() {
        let input = b"<root attr=\"a>b\">text</root>";
        let idx = classify(input);
        let gt_pos: Vec<usize> = idx.gt_positions().collect();
        assert!(!gt_pos.contains(&13));
        assert!(gt_pos.contains(&16));
    }

    #[test]
    fn test_no_quotes() {
        let input = b"<a><b>hello</b></a>";
        let idx = classify(input);
        let lt_pos: Vec<usize> = idx.lt_positions().collect();
        assert_eq!(lt_pos, vec![0, 3, 11, 15]);
    }

    #[test]
    fn test_large_input() {
        let mut input = Vec::new();
        for i in 0..100 {
            input.extend_from_slice(format!("<t{}>x</t{}>", i, i).as_bytes());
        }
        let idx = classify(&input);
        let lt_count = idx.lt_positions().count();
        let gt_count = idx.gt_positions().count();
        assert_eq!(lt_count, 200);
        assert_eq!(gt_count, 200);
    }
}
