//! Scalar fallback structural character classifier.
//!
//! Byte-at-a-time classification used as the universal fallback on platforms
//! without a dedicated SIMD backend, and as the placeholder implementation
//! for SIMD backends under development.

use super::StructuralIndex;

/// Scalar fallback: byte-at-a-time classification.
#[allow(dead_code)] // Used by x86 placeholders and WASM; not called on aarch64
pub fn classify_scalar(input: &[u8]) -> StructuralIndex {
    let num_chunks = (input.len() + 63) / 64;
    let mut lt_bits = vec![0u64; num_chunks];
    let mut gt_bits = vec![0u64; num_chunks];
    let mut in_quote: u8 = 0; // 0 = not in quote, b'"' or b'\'' = in that quote

    for (i, &byte) in input.iter().enumerate() {
        let chunk = i / 64;
        let bit = i % 64;
        // `<` always begins markup (it is illegal raw in text or attribute
        // values), so emit it unconditionally and reset quote state. This keeps
        // a stray `"`/`'` in text content from masking out later tags.
        if byte == b'<' {
            lt_bits[chunk] |= 1u64 << bit;
            in_quote = 0;
            continue;
        }
        if in_quote != 0 {
            if byte == in_quote {
                in_quote = 0;
            }
            continue;
        }
        match byte {
            b'>' => gt_bits[chunk] |= 1u64 << bit,
            b'"' | b'\'' => in_quote = byte,
            _ => {}
        }
    }

    StructuralIndex { lt_bits, gt_bits, len: input.len() }
}
