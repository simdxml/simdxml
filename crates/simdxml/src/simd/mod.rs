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
