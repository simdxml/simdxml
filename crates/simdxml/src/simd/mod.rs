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

#[cfg(target_arch = "aarch64")]
pub mod neon;

/// Structural positions extracted from SIMD classification.
/// Each u64 is a bitmask over a 64-byte chunk of input.
pub struct StructuralIndex {
    /// Positions of '<' characters (not inside quotes)
    pub lt_bits: Vec<u64>,
    /// Positions of '>' characters (not inside quotes)
    pub gt_bits: Vec<u64>,
    /// Total input length
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

    #[cfg(not(target_arch = "aarch64"))]
    {
        classify_scalar(input)
    }
}

/// Scalar fallback: byte-at-a-time classification.
#[cfg(not(target_arch = "aarch64"))]
fn classify_scalar(input: &[u8]) -> StructuralIndex {
    let num_chunks = (input.len() + 63) / 64;
    let mut lt_bits = vec![0u64; num_chunks];
    let mut gt_bits = vec![0u64; num_chunks];
    let mut in_quote: u8 = 0; // 0 = not in quote, b'"' or b'\'' = in that quote

    for (i, &byte) in input.iter().enumerate() {
        let chunk = i / 64;
        let bit = i % 64;
        if in_quote != 0 {
            if byte == in_quote {
                in_quote = 0;
            }
            continue;
        }
        match byte {
            b'<' => lt_bits[chunk] |= 1u64 << bit,
            b'>' => gt_bits[chunk] |= 1u64 << bit,
            b'"' | b'\'' => in_quote = byte,
            _ => {}
        }
    }

    StructuralIndex { lt_bits, gt_bits, len: input.len() }
}
