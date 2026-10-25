//! Reading AV1's syntax elements (4.10): `uvlc()`, `le(n)`, `leb128()`,
//! `su(n)` and `ns(n)`, and the byte alignment of the frame and tile group
//! headers (5.3.5). `f(n)` and the bit position are the shared reader's
//! ([`crate::bits`]).

use super::Error;
use crate::bits::ReadError;

impl ReadError for Error {
    const TRUNCATED: Self = Self::Truncated;

    fn out_of_range(name: &'static str) -> Self {
        Self::OutOfRange(name)
    }
}

/// A reader over one OBU's payload, most significant bit first.
pub(crate) type BitReader<'a> = crate::bits::BitReader<'a, Error>;

impl BitReader<'_> {
    /// `uvlc()` (4.10.3). A value that does not fit 32 bits is refused,
    /// where the specification saturates it: no field it codes needs more.
    pub(crate) fn uvlc(&mut self) -> Result<u32, Error> {
        for leading_zeros in 0..32 {
            if self.bit()? {
                // The suffix and the leading one, less one: 2^leadingZeros
                // - 1 + f(leadingZeros), below 2^33.
                let code = (1u64 << leading_zeros) | u64::from(self.bits(leading_zeros)?);
                return code
                    .checked_sub(1)
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or(Error::OutOfRange("uvlc()"));
            }
        }
        Err(Error::OutOfRange("uvlc()"))
    }

    /// `su(n)` (4.10.6): `n` bits, two's complement.
    pub(crate) fn su(&mut self, n: u32) -> Result<i32, Error> {
        let value = i64::from(self.bits(n)?);
        // `n` is 1 to 32 here: f(n) has refused a wider one.
        let sign_bit = n.checked_sub(1).ok_or(Error::OutOfRange("su(n)"))?;
        let sign_mask = 1i64 << sign_bit;
        let value = if value & sign_mask != 0 {
            // value - 2 * signMask, within 33 bits.
            value
                .checked_sub(sign_mask << 1)
                .ok_or(Error::OutOfRange("su(n)"))?
        } else {
            value
        };
        i32::try_from(value).map_err(|_| Error::OutOfRange("su(n)"))
    }

    /// `ns(n)` (4.10.7): a value below `n`, in as few bits as it takes.
    pub(crate) fn ns(&mut self, n: u32) -> Result<u32, Error> {
        // w - 1, FloorLog2(n); `n` of 0 or 1 codes nothing.
        let Some(floor_log2) = n.checked_ilog2().filter(|&log2| log2 > 0) else {
            return Ok(0);
        };
        // In 64 bits: m = (1 << w) - n, which 2^w past n makes positive.
        let out_of_range = || Error::OutOfRange("ns(n)");
        let m = (2u64 << floor_log2)
            .checked_sub(u64::from(n))
            .ok_or_else(out_of_range)?;
        let v = u64::from(self.bits(floor_log2)?);
        if v < m {
            return u32::try_from(v).map_err(|_| out_of_range());
        }
        let extra_bit = u64::from(self.bit()?);
        // (v << 1) - m + extra_bit: below n, as v is at least m.
        (v << 1)
            .checked_sub(m)
            .and_then(|value| value.checked_add(extra_bit))
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(out_of_range)
    }

    /// `f(n)` of a syntax element coded less one, plus one: the
    /// `frame_width_minus_1` and the like a header codes for `FrameWidth`,
    /// named `name`.
    pub(crate) fn plus_one(&mut self, n: u32, name: &'static str) -> Result<u32, Error> {
        self.bits(n)?.checked_add(1).ok_or(Error::Overflow(name))
    }

    /// `byte_alignment()` (5.3.5): the zero bits up to the next byte.
    pub(crate) fn byte_alignment(&mut self) -> Result<(), Error> {
        while !self.position().is_multiple_of(8) {
            if self.bit()? {
                return Err(Error::OutOfRange("zero_bit"));
            }
        }
        Ok(())
    }
}

/// `leb128()` (4.10.5) at the start of `data`: the value and the bytes it
/// took. Eight bytes at most, and the value must fit 32 bits, as every
/// size in a conformant stream does.
pub(crate) fn leb128(data: &[u8]) -> Result<(u64, usize), Error> {
    // Byte i's seven bits go 7 * i up.
    const SHIFTS: [u32; 8] = [0, 7, 14, 21, 28, 35, 42, 49];
    let mut value = 0u64;
    for ((&byte, shift), taken) in data.iter().zip(SHIFTS).zip(1usize..) {
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            if value > u64::from(u32::MAX) {
                return Err(Error::OutOfRange("leb128()"));
            }
            return Ok((value, taken));
        }
    }
    if data.len() < SHIFTS.len() {
        Err(Error::Truncated)
    } else {
        Err(Error::OutOfRange("leb128()"))
    }
}

/// `le(n)` (4.10.4): `n` bytes, little-endian.
pub(crate) fn le(data: &[u8], n: usize) -> Result<u32, Error> {
    let bytes = data.get(..n).ok_or(Error::Truncated)?;
    Ok(bytes
        .iter()
        .rev()
        .fold(0u32, |value, &byte| (value << 8) | u32::from(byte)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_and_signed_fields_read_most_significant_bit_first() -> Result<(), Error> {
        // 101 | 1001 | 111 | 00: f(3) 5, su(4) -7, su(3) -1, f(2) 0.
        let data = [0b1011_0011, 0b1100_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.bits(3)?, 0b101);
        assert_eq!(r.su(4)?, -7);
        assert_eq!(r.su(3)?, -1);
        assert_eq!(r.bits(2)?, 0);
        assert_eq!(r.position(), 12);
        Ok(())
    }

    #[test]
    fn su_is_twos_complement() -> Result<(), Error> {
        // 0111 1000 0001 1111: su(4) 7, su(4) -8, su(4) 1, su(4) -1.
        let data = [0x78, 0x1f];
        let mut r = BitReader::new(&data);
        assert_eq!([r.su(4)?, r.su(4)?, r.su(4)?, r.su(4)?], [7, -8, 1, -1]);
        Ok(())
    }

    #[test]
    fn ns_takes_one_bit_more_only_above_the_short_codes() -> Result<(), Error> {
        // ns(5): w = 3, m = 3; values 0..2 in two bits, 3 and 4 in three.
        // 00 | 10 | 110 | 111 -> 0, 2, 3, 4.
        let data = [0b0010_1101, 0b1100_0000];
        let mut r = BitReader::new(&data);
        assert_eq!([r.ns(5)?, r.ns(5)?, r.ns(5)?, r.ns(5)?], [0, 2, 3, 4]);
        assert_eq!(r.position(), 10);
        assert_eq!(r.ns(1)?, 0);
        assert_eq!(r.position(), 10);
        // ns(2^32 - 1): w = 32, m = 1; 31 one bits and an extra bit of 1
        // code the largest value, n - 1.
        let data = [0xff; 4];
        let mut r = BitReader::new(&data);
        assert_eq!(r.ns(u32::MAX)?, u32::MAX - 1);
        Ok(())
    }

    #[test]
    fn uvlc_counts_leading_zeros() -> Result<(), Error> {
        // 1 | 010 | 00111 -> 0, 1, 6.
        let data = [0b1010_0011, 0b1000_0000];
        let mut r = BitReader::new(&data);
        assert_eq!([r.uvlc()?, r.uvlc()?, r.uvlc()?], [0, 1, 6]);
        Ok(())
    }

    #[test]
    fn leb128_and_le_read_their_bytes() -> Result<(), Error> {
        assert_eq!(leb128(&[0x05])?, (5, 1));
        assert_eq!(leb128(&[0x80, 0x01])?, (128, 2));
        assert_eq!(leb128(&[0xe5, 0x8e, 0x26])?, (624_485, 3));
        assert_eq!(leb128(&[0x80]), Err(Error::Truncated));
        // Eight bytes at most, and a value of 32 bits.
        assert_eq!(leb128(&[0x80; 9]), Err(Error::OutOfRange("leb128()")));
        assert_eq!(
            leb128(&[0x80, 0x80, 0x80, 0x80, 0x10]),
            Err(Error::OutOfRange("leb128()"))
        );
        assert_eq!(
            leb128(&[0xff, 0xff, 0xff, 0xff, 0x0f])?,
            (u64::from(u32::MAX), 5)
        );
        assert_eq!(le(&[0x34, 0x12, 0xff], 2)?, 0x1234);
        Ok(())
    }

    #[test]
    fn byte_alignment_needs_zero_bits() -> Result<(), Error> {
        let data = [0b1000_0000, 0b1000_0001];
        let mut r = BitReader::new(&data);
        r.bit()?;
        r.byte_alignment()?;
        assert_eq!(r.position(), 8);
        r.bit()?;
        assert_eq!(r.byte_alignment(), Err(Error::OutOfRange("zero_bit")));
        Ok(())
    }
}
