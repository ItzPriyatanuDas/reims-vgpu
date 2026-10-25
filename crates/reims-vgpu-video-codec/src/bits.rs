//! Reading a header's bits most significant first from bytes that need no
//! unescaping: AV1's OBU payloads and VP9's uncompressed header, whose
//! `f(n)` is the same descriptor in both specifications. Each codec reads
//! the rest of its descriptors (AV1's `uvlc()`, `su(n)` and `ns(n)`,
//! VP9's sign and magnitude) in its own `bits` module, and the reader
//! reports its refusals in that codec's error.
//!
//! H.264's and HEVC's reader is `h264::bits`, which removes the emulation
//! prevention bytes as it goes, and refuses through [`ReadError`] too.

use std::marker::PhantomData;

/// A codec's error, as the readers refuse with it: this module's and
/// `h264::bits`'.
pub(crate) trait ReadError {
    /// The data ended inside a syntax element.
    const TRUNCATED: Self;

    /// A read the descriptor does not allow, named.
    fn out_of_range(name: &'static str) -> Self;
}

/// A reader over a header's bytes, most significant bit first, that
/// refuses with `E`.
pub(crate) struct BitReader<'a, E> {
    data: &'a [u8],
    /// The number of bits read.
    position: usize,
    error: PhantomData<E>,
}

impl<'a, E: ReadError> BitReader<'a, E> {
    /// A reader at the first bit of `data`.
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            position: 0,
            error: PhantomData,
        }
    }

    /// The bits read so far (AV1's `get_position()`).
    pub(crate) fn position(&self) -> usize {
        self.position
    }

    /// The bytes the bits read so far take, the last one counted whole:
    /// where a header's trailing or alignment bits end.
    pub(crate) fn bytes_used(&self) -> usize {
        self.position.div_ceil(8)
    }

    /// One bit.
    pub(crate) fn bit(&mut self) -> Result<bool, E> {
        let byte = *self.data.get(self.position / 8).ok_or(E::TRUNCATED)?;
        let bit = (byte << (self.position % 8)) & 0x80 != 0;
        // A position past usize is past the data.
        self.position = self.position.checked_add(1).ok_or(E::TRUNCATED)?;
        Ok(bit)
    }

    /// `f(n)`, for `n` up to 32.
    pub(crate) fn bits(&mut self, n: u32) -> Result<u32, E> {
        if n > 32 {
            return Err(E::out_of_range("f(n)"));
        }
        let mut value = 0u64;
        for _ in 0..n {
            value = (value << 1) | u64::from(self.bit()?);
        }
        u32::try_from(value).map_err(|_| E::out_of_range("f(n)"))
    }

    /// `f(n)`, for `n` up to 8.
    pub(crate) fn u8(&mut self, n: u32) -> Result<u8, E> {
        u8::try_from(self.bits(n)?).map_err(|_| E::out_of_range("f(n)"))
    }

    /// `f(n)`, for `n` up to 16.
    pub(crate) fn u16(&mut self, n: u32) -> Result<u16, E> {
        u16::try_from(self.bits(n)?).map_err(|_| E::out_of_range("f(n)"))
    }

    /// Skips `n` bits, refusing when they run past the data.
    pub(crate) fn skip(&mut self, n: usize) -> Result<(), E> {
        let end = self.position.checked_add(n).ok_or(E::TRUNCATED)?;
        if end > self.data.len().saturating_mul(8) {
            return Err(E::TRUNCATED);
        }
        self.position = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    enum Refusal {
        Truncated,
        OutOfRange(&'static str),
    }

    impl ReadError for Refusal {
        const TRUNCATED: Self = Self::Truncated;

        fn out_of_range(name: &'static str) -> Self {
            Self::OutOfRange(name)
        }
    }

    #[test]
    fn fields_read_most_significant_bit_first_up_to_the_end() -> Result<(), Refusal> {
        let data = [0b1011_0011, 0xff];
        let mut r = BitReader::<Refusal>::new(&data);
        assert_eq!(r.bits(3)?, 0b101);
        assert_eq!(r.bytes_used(), 1);
        assert_eq!(r.u8(5)?, 0b1_0011);
        assert_eq!(r.u16(8)?, 0xff);
        assert_eq!((r.position(), r.bytes_used()), (16, 2));
        assert_eq!(r.bit(), Err(Refusal::Truncated));
        Ok(())
    }

    #[test]
    fn a_skip_past_the_end_moves_nothing() -> Result<(), Refusal> {
        let data = [0x80];
        let mut r = BitReader::<Refusal>::new(&data);
        assert_eq!(r.skip(9), Err(Refusal::Truncated));
        assert_eq!(r.position(), 0);
        r.skip(8)?;
        assert_eq!(r.bit(), Err(Refusal::Truncated));
        Ok(())
    }

    #[test]
    fn a_field_wider_than_its_type_is_refused() {
        let data = [0xff; 8];
        let mut r = BitReader::<Refusal>::new(&data);
        assert_eq!(r.bits(33), Err(Refusal::OutOfRange("f(n)")));
        assert_eq!(r.u8(9), Err(Refusal::OutOfRange("f(n)")));
    }
}
