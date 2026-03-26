//! NEON (AArch64) structural character classifier.
//!
//! Processes 64 bytes at a time (4x 16-byte NEON registers) to produce
//! bitmasks for '<' and '>' positions, with quote masking to ignore
//! structural characters inside attribute values.
//!
//! Key insight from simdjson: instead of branching per-byte, classify ALL
//! bytes in one vectorized pass, then walk the bitmasks with bit manipulation.

#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::*;

use super::StructuralIndex;

/// Classify structural characters using NEON vector instructions.
/// Processes the entire input in one pass, producing bitmasks for Stage 2.
#[cfg(target_arch = "aarch64")]
pub fn classify_neon(input: &[u8]) -> StructuralIndex {
    let len = input.len();
    let num_chunks = (len + 63) / 64;
    let mut lt_bits = vec![0u64; num_chunks];
    let mut gt_bits = vec![0u64; num_chunks];

    // Track quote state across chunks.
    // 0 = not in quotes, 1 = in double quotes, 2 = in single quotes
    let mut in_dquote = false;
    let mut in_squote = false;

    let full_chunks = len / 64;

    unsafe {
        // Splat comparison targets
        let v_lt = vdupq_n_u8(b'<');
        let v_gt = vdupq_n_u8(b'>');
        let v_dquote = vdupq_n_u8(b'"');
        let v_squote = vdupq_n_u8(b'\'');

        for chunk in 0..full_chunks {
            let base = chunk * 64;
            let ptr = input.as_ptr().add(base);

            // Load 4x16 bytes
            let v0 = vld1q_u8(ptr);
            let v1 = vld1q_u8(ptr.add(16));
            let v2 = vld1q_u8(ptr.add(32));
            let v3 = vld1q_u8(ptr.add(48));

            // Compare for each structural character (produces 0xFF or 0x00 per byte)
            let lt0 = vceqq_u8(v0, v_lt);
            let lt1 = vceqq_u8(v1, v_lt);
            let lt2 = vceqq_u8(v2, v_lt);
            let lt3 = vceqq_u8(v3, v_lt);

            let gt0 = vceqq_u8(v0, v_gt);
            let gt1 = vceqq_u8(v1, v_gt);
            let gt2 = vceqq_u8(v2, v_gt);
            let gt3 = vceqq_u8(v3, v_gt);

            let dq0 = vceqq_u8(v0, v_dquote);
            let dq1 = vceqq_u8(v1, v_dquote);
            let dq2 = vceqq_u8(v2, v_dquote);
            let dq3 = vceqq_u8(v3, v_dquote);

            let sq0 = vceqq_u8(v0, v_squote);
            let sq1 = vceqq_u8(v1, v_squote);
            let sq2 = vceqq_u8(v2, v_squote);
            let sq3 = vceqq_u8(v3, v_squote);

            // Convert NEON masks to u64 bitmasks
            let lt_mask = neon_to_bitmask_64(lt0, lt1, lt2, lt3);
            let gt_mask = neon_to_bitmask_64(gt0, gt1, gt2, gt3);
            let dq_mask = neon_to_bitmask_64(dq0, dq1, dq2, dq3);
            let sq_mask = neon_to_bitmask_64(sq0, sq1, sq2, sq3);

            // Apply quote masking: walk quote characters to determine which
            // structural characters are inside quoted regions.
            let (masked_lt, masked_gt) = apply_quote_mask(
                lt_mask, gt_mask, dq_mask, sq_mask,
                &mut in_dquote, &mut in_squote,
            );

            lt_bits[chunk] = masked_lt;
            gt_bits[chunk] = masked_gt;
        }
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

            if in_dquote {
                if byte == b'"' { in_dquote = false; }
                continue;
            }
            if in_squote {
                if byte == b'\'' { in_squote = false; }
                continue;
            }
            match byte {
                b'<' => lt |= 1u64 << bit,
                b'>' => gt |= 1u64 << bit,
                b'"' => in_dquote = true,
                b'\'' => in_squote = true,
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

/// Convert four 16-byte NEON comparison results into a single u64 bitmask.
/// Each byte in the NEON result is either 0xFF (match) or 0x00 (no match).
/// We extract one bit per byte, producing a 64-bit mask.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn neon_to_bitmask_64(
    v0: uint8x16_t, v1: uint8x16_t, v2: uint8x16_t, v3: uint8x16_t,
) -> u64 {
    // Use NEON narrowing shift to extract high bits:
    // Each 0xFF byte → 1 bit via vshrn (shift right and narrow).
    // Method: AND with a power-of-2 mask, then add pairwise to collapse.
    //
    // Faster approach: use the NEON SHRN + ZIP trick.
    // For each 16-byte vector, extract a 16-bit mask.
    let m0 = neon_movemask(v0) as u64;
    let m1 = neon_movemask(v1) as u64;
    let m2 = neon_movemask(v2) as u64;
    let m3 = neon_movemask(v3) as u64;

    m0 | (m1 << 16) | (m2 << 32) | (m3 << 48)
}

/// Extract a 16-bit bitmask from a NEON comparison result (0xFF/0x00 per byte).
/// Equivalent to x86 _mm_movemask_epi8.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn neon_movemask(v: uint8x16_t) -> u16 {
    // Bit extraction using shift+narrow+addv approach.
    // Multiply each byte by its bit position weight, then sum.
    const MASK: [u8; 16] = [
        1, 2, 4, 8, 16, 32, 64, 128,
        1, 2, 4, 8, 16, 32, 64, 128,
    ];
    let mask = vld1q_u8(MASK.as_ptr());
    let masked = vandq_u8(v, mask);
    // Sum the low 8 bytes and high 8 bytes separately
    let lo = vget_low_u8(masked);
    let hi = vget_high_u8(masked);
    // Pairwise add to collapse: 8 bytes → 4 → 2 → 1
    let lo_sum = vaddv_u8(lo);
    let hi_sum = vaddv_u8(hi);
    (lo_sum as u16) | ((hi_sum as u16) << 8)
}

/// Create a bitmask with bits 0..=pos set. Safe for pos 0..=63.
#[inline(always)]
fn mask_up_to(pos: u32) -> u64 {
    if pos >= 63 { u64::MAX } else { (1u64 << (pos + 1)) - 1 }
}

/// Create a bitmask with bits pos..=63 set. Safe for pos 0..=63.
#[inline(always)]
fn mask_from(pos: u32) -> u64 {
    if pos >= 64 { 0 } else { !((1u64 << pos) - 1) }
}

/// Compute prefix-XOR using ARM PMULL (polynomial multiply).
/// prefix_xor(x) at bit i = XOR of all bits 0..=i in x.
/// This is equivalent to: x[0], x[0]^x[1], x[0]^x[1]^x[2], ...
/// Computed in 1-2 instructions via carryless multiply by ALL_ONES.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn prefix_xor(mask: u64) -> u64 {
    // PMULL: polynomial multiply long (carryless multiply)
    // clmul(mask, 0xFFFF_FFFF_FFFF_FFFF) produces the prefix XOR.
    // We only need the low 64 bits of the 128-bit result.
    use std::arch::aarch64::*;
    let result = vmull_p64(mask, u64::MAX);
    // Extract low 64 bits from the poly128_t result
    vgetq_lane_u64(vreinterpretq_u64_p128(result), 0)
}

/// Scalar fallback for prefix_xor (used on non-aarch64 or in tests).
#[cfg(not(target_arch = "aarch64"))]
#[inline(always)]
fn prefix_xor(mask: u64) -> u64 {
    let mut x = mask;
    x ^= x << 1;
    x ^= x << 2;
    x ^= x << 4;
    x ^= x << 8;
    x ^= x << 16;
    x ^= x << 32;
    x
}

/// Apply quote masking to structural character bitmasks.
///
/// Uses PMULL (carryless multiply) for the fast path when only one quote type
/// is present — computes the "inside quotes" bitmask in 1-2 instructions.
/// Falls back to sequential bit-walk when both " and ' appear in the same chunk.
#[inline]
fn apply_quote_mask(
    lt_mask: u64,
    gt_mask: u64,
    dq_mask: u64,
    sq_mask: u64,
    in_dquote: &mut bool,
    in_squote: &mut bool,
) -> (u64, u64) {
    // Fast path: no quotes in this chunk and not inside quotes → no masking
    if dq_mask == 0 && sq_mask == 0 && !*in_dquote && !*in_squote {
        return (lt_mask, gt_mask);
    }

    // PMULL fast path: only one quote type in this chunk
    // This covers 99%+ of real XML chunks (attributes typically use " only).
    if sq_mask == 0 && !*in_squote {
        // Only double quotes — use PMULL prefix-XOR
        let quoted = unsafe { prefix_xor(dq_mask) };
        // If we carried in a dquote state, flip the mask
        let quoted = if *in_dquote { !quoted } else { quoted };
        // Update carry-out: odd number of dquotes toggles state
        *in_dquote = (dq_mask.count_ones() & 1 == 1) ^ *in_dquote;
        return (lt_mask & !quoted, gt_mask & !quoted);
    }

    if dq_mask == 0 && !*in_dquote {
        // Only single quotes — use PMULL prefix-XOR
        let quoted = unsafe { prefix_xor(sq_mask) };
        let quoted = if *in_squote { !quoted } else { quoted };
        *in_squote = (sq_mask.count_ones() & 1 == 1) ^ *in_squote;
        return (lt_mask & !quoted, gt_mask & !quoted);
    }

    // Slow path: both quote types present — sequential bit-walk.
    // Rare in practice (< 1% of chunks in typical XML).
    apply_quote_mask_slow(lt_mask, gt_mask, dq_mask, sq_mask, in_dquote, in_squote)
}

/// Sequential bit-walk fallback for mixed-quote chunks.
fn apply_quote_mask_slow(
    lt_mask: u64,
    gt_mask: u64,
    dq_mask: u64,
    sq_mask: u64,
    in_dquote: &mut bool,
    in_squote: &mut bool,
) -> (u64, u64) {
    let mut quoted_mask: u64 = 0;
    let mut remaining = dq_mask | sq_mask;

    if *in_dquote {
        if dq_mask != 0 {
            let close_pos = dq_mask.trailing_zeros();
            quoted_mask |= mask_up_to(close_pos);
            *in_dquote = false;
            remaining &= !mask_up_to(close_pos);
        } else {
            return (0, 0);
        }
    } else if *in_squote {
        if sq_mask != 0 {
            let close_pos = sq_mask.trailing_zeros();
            quoted_mask |= mask_up_to(close_pos);
            *in_squote = false;
            remaining &= !mask_up_to(close_pos);
        } else {
            return (0, 0);
        }
    }

    while remaining != 0 {
        let pos = remaining.trailing_zeros();
        remaining &= remaining - 1;
        let byte_is_dquote = (dq_mask >> pos) & 1 == 1;

        let after_mask = if pos < 63 { !((1u64 << (pos + 1)) - 1) } else { 0 };
        let close_mask = if byte_is_dquote {
            dq_mask & after_mask
        } else {
            sq_mask & after_mask
        };

        if close_mask != 0 {
            let close_pos = close_mask.trailing_zeros();
            let range = mask_up_to(close_pos) & mask_from(pos);
            quoted_mask |= range;
            remaining &= !range;
        } else {
            quoted_mask |= mask_from(pos);
            if byte_is_dquote { *in_dquote = true; } else { *in_squote = true; }
            break;
        }
    }

    (lt_mask & !quoted_mask, gt_mask & !quoted_mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_tags() {
        let input = b"<root><child>text</child></root>";
        let idx = classify_neon(input);
        let lt_pos: Vec<usize> = idx.lt_positions().collect();
        let gt_pos: Vec<usize> = idx.gt_positions().collect();
        // <root> at 0-5, <child> at 6-12, </child> at 17-24, </root> at 25-31
        assert_eq!(lt_pos, vec![0, 6, 17, 25]);
        assert_eq!(gt_pos, vec![5, 12, 24, 31]);
    }

    #[test]
    fn test_quoted_gt() {
        // '>' inside attribute value should be masked out
        let input = b"<root attr=\"a>b\">text</root>";
        let idx = classify_neon(input);
        let gt_pos: Vec<usize> = idx.gt_positions().collect();
        // Only the '>' at position 16 (closing tag start) and 27 (closing tag end)
        assert!(!gt_pos.contains(&13)); // the '>' inside quotes
        assert!(gt_pos.contains(&16));  // closing '>' of open tag
    }

    #[test]
    fn test_no_quotes() {
        let input = b"<a><b>hello</b></a>";
        let idx = classify_neon(input);
        let lt_pos: Vec<usize> = idx.lt_positions().collect();
        assert_eq!(lt_pos, vec![0, 3, 11, 15]);
    }

    #[test]
    fn test_large_input() {
        // Test with >64 bytes to exercise full NEON path
        let mut input = Vec::new();
        for i in 0..100 {
            input.extend_from_slice(format!("<t{}>x</t{}>", i, i).as_bytes());
        }
        let idx = classify_neon(&input);
        let lt_count = idx.lt_positions().count();
        let gt_count = idx.gt_positions().count();
        assert_eq!(lt_count, 200); // 100 open + 100 close
        assert_eq!(gt_count, 200);
    }
}
