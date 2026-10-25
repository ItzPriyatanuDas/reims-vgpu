//! What core reads of an H.264 parameter set: the raw byte sequence
//! payload under a NAL unit's emulation prevention (7.4.1), and its
//! fixed-length fields and Exp-Golomb codes (9.1). Every read answers `None` past the end, so a malformed
//! parameter set from the guest says nothing rather than panics.

/// The RBSP of escaped NAL unit bytes: each 0x03 after two zero bytes,
/// `emulation_prevention_three_byte`, removed.
pub(super) fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &byte in data {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// A reader of an RBSP's bits, most significant first.
pub(super) struct Bits<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Bits<'a> {
    pub(super) fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    pub(super) fn bit(&mut self) -> Option<bool> {
        let byte = self.data.get(self.position / 8)?;
        let bit = (byte >> (7 - self.position % 8)) & 1 == 1;
        self.position += 1;
        Some(bit)
    }

    /// `u(n)` for `n` up to 32: the tests' reads of what the writer wrote.
    #[cfg(test)]
    pub(super) fn bits(&mut self, n: u32) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..n.min(32) {
            value = (value << 1) | u32::from(self.bit()?);
        }
        Some(value)
    }

    pub(super) fn skip(&mut self, bits: usize) -> Option<()> {
        self.position = self.position.checked_add(bits)?;
        (self.position <= self.data.len() * 8).then_some(())
    }

    /// `ue(v)`, up to 2^32 - 2.
    pub(super) fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let mut value = 1u64;
        for _ in 0..zeros {
            value = (value << 1) | u64::from(self.bit()?);
        }
        u32::try_from(value - 1).ok()
    }

    /// `se(v)`.
    pub(super) fn se(&mut self) -> Option<i32> {
        let code = self.ue()?;
        let magnitude = i32::try_from(code.div_ceil(2)).ok()?;
        Some(if code % 2 == 1 { magnitude } else { -magnitude })
    }
}

/// The tests' writer of parameter sets from scratch: fixed-length fields
/// and Exp-Golomb codes, the stop bit, and emulation prevention.
#[cfg(test)]
pub(super) mod writer {
    /// A field to write.
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum Field {
        /// `u(n)`: the value in that many bits.
        U(u32, u32),
        /// `ue(v)`.
        Ue(u32),
        /// `se(v)`.
        Se(i32),
    }

    /// The RBSP of `fields`, then `rbsp_trailing_bits()`.
    pub(crate) fn rbsp(fields: &[Field]) -> Vec<u8> {
        let mut bits: Vec<bool> = Vec::new();
        let mut put = |n: u32, value: u64| {
            for i in (0..n).rev() {
                bits.push((value >> i) & 1 == 1);
            }
        };
        for &field in fields {
            match field {
                Field::U(n, value) => put(n, u64::from(value)),
                Field::Ue(value) => {
                    let code = u64::from(value) + 1;
                    let len = 64 - code.leading_zeros();
                    put(len - 1, 0);
                    put(len, code);
                }
                Field::Se(value) => {
                    let code = if value > 0 {
                        2 * value.unsigned_abs() - 1
                    } else {
                        2 * value.unsigned_abs()
                    };
                    let code = u64::from(code) + 1;
                    let len = 64 - code.leading_zeros();
                    put(len - 1, 0);
                    put(len, code);
                }
            }
        }
        put(1, 1);
        while !bits.len().is_multiple_of(8) {
            bits.push(false);
        }
        bits.chunks(8)
            .map(|byte| byte.iter().fold(0u8, |acc, &b| (acc << 1) | u8::from(b)))
            .collect()
    }

    /// `rbsp` with an `emulation_prevention_three_byte` after each two
    /// zero bytes that a byte of 3 or less follows.
    pub(crate) fn escape(rbsp: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(rbsp.len() + 4);
        let mut zeros = 0;
        for &byte in rbsp {
            if zeros >= 2 && byte <= 3 {
                out.push(3);
                zeros = 0;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            out.push(byte);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::writer::{Field, escape, rbsp};
    use super::*;

    #[test]
    fn an_emulation_prevention_byte_is_not_read_as_data() {
        assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3, 0]), vec![0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn what_the_writer_escapes_reads_back() {
        let data = rbsp(&[
            Field::U(32, 0),
            Field::U(16, 0),
            Field::Ue(0),
            Field::Ue(1_000_000),
            Field::Se(-7),
            Field::Se(7),
            Field::U(3, 5),
        ]);
        let escaped = escape(&data);
        assert!(escaped.len() > data.len());
        let plain = unescape(&escaped);
        assert_eq!(plain, data);
        let mut bits = Bits::new(&plain);
        assert_eq!(bits.bits(32), Some(0));
        assert_eq!(bits.bits(16), Some(0));
        assert_eq!(bits.ue(), Some(0));
        assert_eq!(bits.ue(), Some(1_000_000));
        assert_eq!(bits.se(), Some(-7));
        assert_eq!(bits.se(), Some(7));
        assert_eq!(bits.bits(3), Some(5));
        assert_eq!(bits.bit(), Some(true));
    }

    #[test]
    fn reads_past_the_end_say_nothing() {
        let mut bits = Bits::new(&[0x00, 0x00]);
        assert_eq!(bits.ue(), None);
        let mut bits = Bits::new(&[0xff]);
        assert_eq!(bits.bits(8), Some(0xff));
        assert_eq!(bits.bit(), None);
        assert_eq!(bits.skip(1), None);
        let mut bits = Bits::new(&[]);
        assert_eq!(bits.skip(usize::MAX), None);
    }
}
