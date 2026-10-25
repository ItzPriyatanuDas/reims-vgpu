//! The `vpcC` box's contents: VPCodecConfigurationRecord (VP Codec ISO
//! Media File Format Binding, v1.0, 2.2), as an MP4's `vp09` sample entry
//! carries it and a format description keeps it among its sample
//! description extension atoms.
//!
//! VP9 frames carry their own profile and colour configuration, so the
//! record only says ahead of the first frame what the stream is: its
//! profile, bit depth and subsampling, for a decoder to refuse or set up
//! its output before any frame comes.

use super::Error;

/// `chromaSubsampling` (2.3): 4:2:0 with chroma between the luma samples
/// vertically, 4:2:0 with chroma on the first luma sample, 4:2:2, 4:4:4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChromaSubsampling {
    /// 0: 4:2:0, vertically between.
    Yuv420Vertical,
    /// 1: 4:2:0, colocated with luma (0, 0).
    Yuv420Colocated,
    /// 2: 4:2:2.
    Yuv422,
    /// 3: 4:4:4.
    Yuv444,
    /// Another value, reserved.
    Reserved(u8),
}

impl ChromaSubsampling {
    /// Whether it is either kind of 4:2:0.
    pub fn is_420(self) -> bool {
        matches!(self, Self::Yuv420Vertical | Self::Yuv420Colocated)
    }
}

/// A VPCodecConfigurationRecord.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigurationRecord {
    /// The box's version: 1 is the binding's; 0 the draft's older layout.
    pub version: u8,
    /// `profile`, 0 to 3.
    pub profile: u8,
    /// `level`, as the binding numbers it: 10 for level 1, 41 for 4.1.
    pub level: u8,
    /// `bitDepth`: 8, 10 or 12.
    pub bit_depth: u8,
    /// `chromaSubsampling`.
    pub chroma_subsampling: ChromaSubsampling,
    /// `videoFullRangeFlag`.
    pub full_range: bool,
    /// `colourPrimaries`, `transferCharacteristics` and
    /// `matrixCoefficients` (ISO/IEC 23001-8), for version 1. Version 0
    /// codes a colour space and transfer function instead, kept here as
    /// coded.
    pub colour: (u8, u8, u8),
}

impl ConfigurationRecord {
    /// Parses the box's contents: its version and flags, then the record.
    pub fn parse(data: &[u8]) -> Result<Self, Error> {
        let [version, _, _, _, ref record @ ..] = *data else {
            return Err(Error::VpccTooShort);
        };
        match version {
            1 => {
                let [
                    profile,
                    level,
                    depth_subsampling_range,
                    primaries,
                    transfer,
                    matrix,
                    size_hi,
                    size_lo,
                    ..,
                ] = *record
                else {
                    return Err(Error::VpccVersion1TooShort);
                };
                let init_size = usize::from(u16::from_be_bytes([size_hi, size_lo]));
                if record
                    .get(8..)
                    .is_none_or(|initialization| initialization.len() < init_size)
                {
                    return Err(Error::VpccInitializationDataPastEnd);
                }
                Ok(Self {
                    version,
                    profile,
                    level,
                    bit_depth: depth_subsampling_range >> 4,
                    chroma_subsampling: subsampling((depth_subsampling_range >> 1) & 0x7),
                    full_range: depth_subsampling_range & 1 == 1,
                    colour: (primaries, transfer, matrix),
                })
            }
            0 => {
                let [profile, level, depth_space, subsampling_transfer, range, ..] = *record else {
                    return Err(Error::VpccVersion0TooShort);
                };
                Ok(Self {
                    version,
                    profile,
                    level,
                    bit_depth: depth_space >> 4,
                    chroma_subsampling: subsampling(subsampling_transfer >> 4),
                    full_range: range >> 7 == 1,
                    colour: (depth_space & 0xf, subsampling_transfer & 0xf, 0),
                })
            }
            _ => Err(Error::VpccUnknownVersion),
        }
    }
}

fn subsampling(value: u8) -> ChromaSubsampling {
    match value {
        0 => ChromaSubsampling::Yuv420Vertical,
        1 => ChromaSubsampling::Yuv420Colocated,
        2 => ChromaSubsampling::Yuv422,
        3 => ChromaSubsampling::Yuv444,
        other => ChromaSubsampling::Reserved(other),
    }
}
