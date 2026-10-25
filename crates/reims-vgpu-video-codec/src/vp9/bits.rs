//! Reading VP9's uncompressed header (section 4's descriptors): `f(n)`
//! and the bit position the header's size needs (`trailing_bits`, 9.1)
//! are the shared reader's ([`crate::bits`]); `s(n)`, a magnitude then its
//! sign, is VP9's own.

use super::Error;
use crate::bits::ReadError;

impl ReadError for Error {
    const TRUNCATED: Self = Self::Truncated;

    fn out_of_range(name: &'static str) -> Self {
        Self::OutOfRange(name)
    }
}

/// A reader over a frame's bytes, most significant bit first.
pub(crate) type BitReader<'a> = crate::bits::BitReader<'a, Error>;

impl BitReader<'_> {
    /// `s(n)`: `n` bits of magnitude, then a sign bit, set for negative
    /// (VP9's form, not AV1's two's complement), for `n` up to 15.
    pub(crate) fn signed(&mut self, n: u32) -> Result<i16, Error> {
        let magnitude = i16::try_from(self.bits(n)?).map_err(|_| Error::OutOfRange("s(n)"))?;
        if self.bit()? {
            magnitude.checked_neg().ok_or(Error::OutOfRange("s(n)"))
        } else {
            Ok(magnitude)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_fields_are_a_magnitude_then_a_sign() -> Result<(), Error> {
        // s(4): 0101 0 -> 5, 0101 1 -> -5; s(6): 000000 1 -> 0.
        let data = [0b0101_0010, 0b1100_0000, 0b1000_0000];
        let mut r = BitReader::new(&data);
        assert_eq!([r.signed(4)?, r.signed(4)?, r.signed(6)?], [5, -5, 0]);
        assert_eq!(r.bytes_used(), 3);
        Ok(())
    }
}
