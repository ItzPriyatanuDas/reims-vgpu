//! Metadata OBUs (5.8, 6.7): the HDR ones, the content light level and
//! the mastering display's colour volume. Nothing here is used to decode:
//! they say what a stream carries (the info examples print them).
//!
//! Bounded by the OBU: each field is read from the payload, and nothing
//! is kept of a metadata type not read here.

use super::Error;
use super::bits::leb128;

/// `METADATA_TYPE_HDR_CLL` (6.7.1).
pub(crate) const HDR_CLL: u64 = 1;
/// `METADATA_TYPE_HDR_MDCV`.
pub(crate) const HDR_MDCV: u64 = 2;

/// `metadata_hdr_cll()` (5.8.3, 6.7.3): CTA-861.3's light levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentLightLevel {
    /// `max_cll`, in cd/m2.
    pub max_cll: u16,
    /// `max_fall`, in cd/m2.
    pub max_fall: u16,
}

/// `metadata_hdr_mdcv()` (5.8.4, 6.7.4): SMPTE ST 2086's colour volume of
/// the mastering display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MasteringDisplay {
    /// `primary_chromaticity_x[i]` and `_y[i]`, 0.16 fixed point, for red,
    /// green and blue in that order.
    pub primaries: [(u16, u16); 3],
    /// `white_point_chromaticity_x` and `_y`, 0.16 fixed point.
    pub white_point: (u16, u16),
    /// `luminance_max`, 24.8 fixed point, in cd/m2.
    pub luminance_max: u32,
    /// `luminance_min`, 18.14 fixed point, in cd/m2.
    pub luminance_min: u32,
}

/// A metadata OBU, by its type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metadata {
    /// `METADATA_TYPE_HDR_CLL`.
    ContentLightLevel(ContentLightLevel),
    /// `METADATA_TYPE_HDR_MDCV`.
    MasteringDisplay(MasteringDisplay),
    /// Any other `metadata_type`, its payload not read.
    Other(u64),
}

/// The big-endian `N` bytes at `at`.
fn big_endian<const N: usize>(data: &[u8], at: usize) -> Result<[u8; N], Error> {
    data.get(at..)
        .and_then(|from| from.first_chunk::<N>())
        .copied()
        .ok_or(Error::Truncated)
}

/// `metadata_obu()` (5.8.1) from an `OBU_METADATA`'s payload.
pub fn parse(payload: &[u8]) -> Result<Metadata, Error> {
    let (metadata_type, used) = leb128(payload)?;
    let body = payload.get(used..).ok_or(Error::Truncated)?;
    let be16 = |at| big_endian(body, at).map(u16::from_be_bytes);
    let be32 = |at| big_endian(body, at).map(u32::from_be_bytes);
    Ok(match metadata_type {
        HDR_CLL => Metadata::ContentLightLevel(ContentLightLevel {
            max_cll: be16(0)?,
            max_fall: be16(2)?,
        }),
        HDR_MDCV => Metadata::MasteringDisplay(MasteringDisplay {
            primaries: [
                (be16(0)?, be16(2)?),
                (be16(4)?, be16(6)?),
                (be16(8)?, be16(10)?),
            ],
            white_point: (be16(12)?, be16(14)?),
            luminance_max: be32(16)?,
            luminance_min: be32(20)?,
        }),
        other => Metadata::Other(other),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_light_level_reads_as_coded() -> Result<(), Error> {
        // max_cll 1000, max_fall 400, then the trailing bits (5.3.4), which
        // the payload's sizes leave aside.
        assert_eq!(
            parse(&[0x01, 0x03, 0xe8, 0x01, 0x90, 0x80])?,
            Metadata::ContentLightLevel(ContentLightLevel {
                max_cll: 1000,
                max_fall: 400,
            })
        );
        Ok(())
    }

    #[test]
    fn a_mastering_display_reads_red_first_in_fixed_point() -> Result<(), Error> {
        // BT.2020's primaries and D65 in 0.16: R 0.708, 0.292; G 0.170,
        // 0.797; B 0.131, 0.046; white 0.3127, 0.3290. 1000 cd/m2 in 24.8,
        // 0.005 in 18.14 (82).
        let fixed = |v: f64| (v * 65536.0).round() as u16;
        let mut payload = vec![0x02];
        for v in [0.708, 0.292, 0.170, 0.797, 0.131, 0.046, 0.3127, 0.3290] {
            payload.extend_from_slice(&fixed(v).to_be_bytes());
        }
        payload.extend_from_slice(&(1000u32 << 8).to_be_bytes());
        payload.extend_from_slice(&82u32.to_be_bytes());
        payload.push(0x80);
        assert_eq!(
            parse(&payload)?,
            Metadata::MasteringDisplay(MasteringDisplay {
                primaries: [(46399, 19137), (11141, 52232), (8585, 3015)],
                white_point: (20493, 21561),
                luminance_max: 256_000,
                luminance_min: 82,
            })
        );
        Ok(())
    }

    #[test]
    fn other_types_are_named_and_short_payloads_refused() {
        // ITU-T T.35 (4), with a two-byte leb128 type of 200 too.
        assert_eq!(parse(&[0x04, 0xb5, 0x00]), Ok(Metadata::Other(4)));
        assert_eq!(parse(&[0xc8, 0x01]), Ok(Metadata::Other(200)));
        assert_eq!(parse(&[0x01, 0x03, 0xe8, 0x01]), Err(Error::Truncated));
        assert_eq!(parse(&[0x02; 24]), Err(Error::Truncated));
        assert_eq!(parse(&[]), Err(Error::Truncated));
        assert_eq!(parse(&[0x80]), Err(Error::Truncated));
    }
}
