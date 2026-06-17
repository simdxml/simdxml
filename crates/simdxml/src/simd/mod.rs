//! SIMD-accelerated structural character classification.
//!
//! Stage 1 of the simdjson-for-XML architecture: classify every byte in the
//! input into structural character classes using vector instructions, producing
//! bitmasks that Stage 2 (the index builder) walks to build the structural index.
//!
//! Platform support:
//! - aarch64: NEON (128-bit, 16 bytes/vector)
//! - x86_64: AVX2 (256-bit, 32 bytes/vector) — future
//! - Fallback: scalar (current memchr-based parser)

#[doc(hidden)]
pub mod scalar;

#[cfg(target_arch = "aarch64")]
pub mod neon;

#[cfg(target_arch = "x86_64")]
#[doc(hidden)]
pub mod sse42;

#[cfg(target_arch = "x86_64")]
#[doc(hidden)]
pub mod avx2;

/// Structural positions extracted from SIMD classification.
/// Each u64 is a bitmask over a 64-byte chunk of input.
pub struct StructuralIndex {
    /// Positions of '<' characters (not inside quotes)
    pub lt_bits: Vec<u64>,
    /// Positions of '>' characters (not inside quotes)
    pub gt_bits: Vec<u64>,
    /// Total input length
    #[allow(dead_code)]
    pub len: usize,
}

impl StructuralIndex {
    /// Iterate all '<' positions in document order.
    #[inline]
    pub fn lt_positions(&self) -> BitPositionIter<'_> {
        BitPositionIter { bits: &self.lt_bits, chunk: 0, current: 0 }
    }

    /// Iterate all '>' positions in document order.
    #[inline]
    pub fn gt_positions(&self) -> BitPositionIter<'_> {
        BitPositionIter { bits: &self.gt_bits, chunk: 0, current: 0 }
    }
}

/// Iterator over set bit positions across chunks.
pub struct BitPositionIter<'a> {
    bits: &'a [u64],
    chunk: usize,
    current: u64,
}

impl<'a> Iterator for BitPositionIter<'a> {
    type Item = usize;

    #[inline]
    fn next(&mut self) -> Option<usize> {
        loop {
            if self.current != 0 {
                let pos = self.current.trailing_zeros() as usize;
                self.current &= self.current - 1; // clear lowest set bit
                return Some((self.chunk - 1) * 64 + pos);
            }
            if self.chunk >= self.bits.len() {
                return None;
            }
            self.current = self.bits[self.chunk];
            self.chunk += 1;
        }
    }
}

/// Resolve `<`/`>` positions from the raw per-chunk character masks, honoring
/// XML quoting rules.
///
/// In well-formed XML a literal `<` may never appear in text content or inside
/// an attribute value (it must be escaped as `&lt;`), so every `<` reliably
/// starts markup. We therefore (a) always emit `<`, and (b) reset the quote
/// state at each `<`, so quotes only suppress `>` *within* the current tag's
/// attribute values — never across a tag boundary. This avoids the failure mode
/// of a global quote toggle, where a single `"` or `'` in text content would
/// flip the parity and drop every subsequent tag from the index.
///
/// `quote` carries the active quote byte (`0`, `b'"'`, or `b'\''`) across chunks.
/// Only the set structural bits are walked, so this stays cheap (a handful of
/// iterations per 64-byte chunk) while the SIMD backends do the bulk scanning.
#[inline]
pub(crate) fn resolve_quotes(
    lt_mask: u64,
    gt_mask: u64,
    dq_mask: u64,
    sq_mask: u64,
    quote: &mut u8,
) -> (u64, u64) {
    // Fast path: no quote characters in this chunk and not currently inside a
    // quoted value → every `<` and `>` passes through unchanged.
    if dq_mask == 0 && sq_mask == 0 && *quote == 0 {
        return (lt_mask, gt_mask);
    }

    let mut lt_out: u64 = 0;
    let mut gt_out: u64 = 0;
    let mut events = lt_mask | gt_mask | dq_mask | sq_mask;
    while events != 0 {
        let pos = events.trailing_zeros();
        events &= events - 1;
        let bit = 1u64 << pos;

        if lt_mask & bit != 0 {
            // `<` always begins markup; emit it and clear any (malformed) open quote.
            lt_out |= bit;
            *quote = 0;
        } else if *quote != 0 {
            // Inside an attribute value: only the matching quote byte closes it.
            // A `>` here is literal and must not terminate the tag.
            let q = *quote;
            if (q == b'"' && dq_mask & bit != 0) || (q == b'\'' && sq_mask & bit != 0) {
                *quote = 0;
            }
        } else if gt_mask & bit != 0 {
            gt_out |= bit;
        } else if dq_mask & bit != 0 {
            *quote = b'"';
        } else {
            // sq_mask & bit != 0
            *quote = b'\'';
        }
    }
    (lt_out, gt_out)
}

#[cfg(test)]
mod resolve_tests {
    use super::resolve_quotes;

    /// Build the four per-chunk character masks for up to 64 bytes, then resolve.
    fn resolve(s: &[u8], quote: &mut u8) -> (Vec<usize>, Vec<usize>) {
        assert!(s.len() <= 64);
        let (mut lt, mut gt, mut dq, mut sq) = (0u64, 0u64, 0u64, 0u64);
        for (i, &b) in s.iter().enumerate() {
            let bit = 1u64 << i;
            match b {
                b'<' => lt |= bit,
                b'>' => gt |= bit,
                b'"' => dq |= bit,
                b'\'' => sq |= bit,
                _ => {}
            }
        }
        let (lt_out, gt_out) = resolve_quotes(lt, gt, dq, sq, quote);
        let bits = |m: u64| (0..64).filter(|i| m & (1 << i) != 0).collect::<Vec<_>>();
        (bits(lt_out), bits(gt_out))
    }

    #[test]
    fn quote_in_text_does_not_mask_following_tags() {
        // `<a>"</a>` — the bug: the `"` would mask the `</a>`'s `<` and `>`.
        let mut q = 0;
        let (lt, gt) = resolve(br#"<a>"</a>"#, &mut q);
        assert_eq!(lt, vec![0, 4]); // both '<' survive
        assert_eq!(gt, vec![2, 7]); // both '>' survive
        assert_eq!(q, 0);
    }

    #[test]
    fn gt_inside_attribute_is_suppressed() {
        let mut q = 0;
        let (lt, gt) = resolve(br#"<a b="x>y">"#, &mut q);
        assert_eq!(lt, vec![0]);
        assert_eq!(gt, vec![10]); // the '>' at index 7 (inside quotes) is gone
        assert_eq!(q, 0);
    }

    #[test]
    fn single_quote_inside_double_is_literal() {
        let mut q = 0;
        let (_, gt) = resolve(br#"<a b="it's > ok">"#, &mut q);
        // Only the final tag-closing '>' survives; the one inside the value does not.
        assert_eq!(gt, vec![16]);
        assert_eq!(q, 0);
    }

    #[test]
    fn quote_state_carries_across_chunks() {
        // An attribute value left open at a chunk boundary stays open.
        let mut q = 0;
        let (_, gt1) = resolve(br#"<a b="x>"#, &mut q);
        assert_eq!(gt1, Vec::<usize>::new()); // '>' is inside the open quote
        assert_eq!(q, b'"');
        // Next chunk: the value closes, then the real tag end.
        let (_, gt2) = resolve(br#"y">"#, &mut q);
        assert_eq!(gt2, vec![2]);
        assert_eq!(q, 0);
    }
}

/// Build structural index using the best available SIMD on this platform.
pub fn classify_structural(input: &[u8]) -> StructuralIndex {
    #[cfg(target_arch = "aarch64")]
    {
        return neon::classify_neon(input);
    }

    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            // Safety: AVX2 availability checked above
            return unsafe { avx2::classify_avx2(input) };
        }
        if is_x86_feature_detected!("sse4.2") {
            // Safety: SSE4.2 availability checked above
            return unsafe { sse42::classify_sse42(input) };
        }
        return scalar::classify_scalar(input);
    }

    // Universal scalar fallback for other architectures
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        scalar::classify_scalar(input)
    }
}
