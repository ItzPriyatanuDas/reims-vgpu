//! The integer conversions std has no lossless spelling for on the targets
//! the library builds for, whose `usize` is 64 bits wide: `u32` and `u64`
//! to `usize`, and `usize` to `u64`. Sizes and counts that come from the
//! guest or a format are `u32` or `u64` where the wire carries them, and
//! index slices as `usize`; each conversion is reached by name rather than
//! as an `as` at each site. So are the truncations to a field's low bits
//! and the nearest `f64` to an integer.

#![expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "the conversions every other module reaches by name"
)]

const _: () = assert!(usize::BITS == u64::BITS);

/// Lossless conversion to `usize`.
pub(crate) trait ToUsize: Copy {
    /// `self` as `usize`.
    fn to_usize(self) -> usize;
}

impl ToUsize for u32 {
    #[inline]
    fn to_usize(self) -> usize {
        self as usize
    }
}

impl ToUsize for u64 {
    #[inline]
    fn to_usize(self) -> usize {
        self as usize
    }
}

/// Lossless conversion of a `usize` to `u64`.
pub(crate) trait ToU64: Copy {
    /// `self` as `u64`.
    fn to_u64(self) -> u64;
}

impl ToU64 for usize {
    #[inline]
    fn to_u64(self) -> u64 {
        self as u64
    }
}

/// The low 32 bits of `value`: a truncation, named.
#[inline]
pub(crate) const fn low_u32(value: u64) -> u32 {
    value as u32
}

/// The low 16 bits of `value`: a truncation, named.
#[inline]
pub(crate) const fn low_u16(value: u32) -> u16 {
    value as u16
}

/// The nearest `f64` to an integer: exact up to 2^53.
pub(crate) trait ToF64: Copy {
    /// `self` as the nearest `f64`.
    fn to_f64(self) -> f64;
}

impl ToF64 for u64 {
    #[inline]
    fn to_f64(self) -> f64 {
        self as f64
    }
}
