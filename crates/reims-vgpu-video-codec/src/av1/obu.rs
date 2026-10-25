//! Open bitstream units (5.3): their headers, and splitting a temporal
//! unit in the low overhead bitstream format (5.2), as MP4 samples and
//! IVF frames carry it, into its OBUs.

use super::Error;
use super::bits::leb128;
use crate::num::ToUsize;

/// An OBU's type (`obu_type`, 6.2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObuType {
    /// `OBU_SEQUENCE_HEADER`.
    SequenceHeader,
    /// `OBU_TEMPORAL_DELIMITER`.
    TemporalDelimiter,
    /// `OBU_FRAME_HEADER`.
    FrameHeader,
    /// `OBU_TILE_GROUP`.
    TileGroup,
    /// `OBU_METADATA`.
    Metadata,
    /// `OBU_FRAME`: a frame header and its first tile group.
    Frame,
    /// `OBU_REDUNDANT_FRAME_HEADER`.
    RedundantFrameHeader,
    /// `OBU_TILE_LIST`, which large scale tile decoding alone uses.
    TileList,
    /// `OBU_PADDING`.
    Padding,
    /// A reserved type, which decoders ignore.
    Reserved(u8),
}

impl ObuType {
    fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::SequenceHeader,
            2 => Self::TemporalDelimiter,
            3 => Self::FrameHeader,
            4 => Self::TileGroup,
            5 => Self::Metadata,
            6 => Self::Frame,
            7 => Self::RedundantFrameHeader,
            8 => Self::TileList,
            15 => Self::Padding,
            other => Self::Reserved(other),
        }
    }
}

/// One OBU: its header's fields and its payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Obu<'a> {
    /// `obu_type`.
    pub obu_type: ObuType,
    /// `temporal_id` and `spatial_id`, when the OBU has an extension
    /// header.
    pub extension: Option<(u8, u8)>,
    /// The payload, `obu_size` bytes.
    pub payload: &'a [u8],
}

impl Obu<'_> {
    /// `temporal_id`, 0 without an extension header.
    pub(crate) fn temporal_id(&self) -> u8 {
        self.extension.map_or(0, |(t, _)| t)
    }

    /// `spatial_id`, 0 without an extension header.
    pub(crate) fn spatial_id(&self) -> u8 {
        self.extension.map_or(0, |(_, s)| s)
    }

    /// Whether the OBU belongs to operating point `idc`
    /// (`OperatingPointIdc`): the drop rule of 7.5, which keeps every OBU
    /// without an extension, and every OBU of an operating point of 0.
    pub(crate) fn in_operating_point(&self, idc: u16) -> bool {
        self.extension
            .is_none_or(|(temporal, spatial)| in_layers(idc, temporal, spatial))
    }
}

/// Whether operating point `idc` holds the layer of `temporal_id` and
/// `spatial_id`: every layer for an `idc` of 0, else the layer whose
/// temporal bit (the low 8) and spatial bit (the 4 above) are both set, as
/// 7.5's drop rule and the frame header's `buffer_removal_time` (5.9.2)
/// read them.
pub(crate) fn in_layers(idc: u16, temporal_id: u8, spatial_id: u8) -> bool {
    let bit = |shift: u32| idc.checked_shr(shift).is_some_and(|bits| bits & 1 == 1);
    idc == 0 || (bit(u32::from(temporal_id)) && bit(u32::from(spatial_id) + 8))
}

/// The OBUs of `data`, a temporal unit (or an `av1C` record's
/// `configOBUs`) in the low overhead format. An OBU without
/// `obu_has_size_field` runs to the end of the data.
pub fn obus(data: &[u8]) -> impl Iterator<Item = Result<Obu<'_>, Error>> {
    let mut rest = data;
    core::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        match parse_one(rest) {
            Ok((obu, after)) => {
                rest = after;
                Some(Ok(obu))
            }
            Err(error) => {
                rest = &[];
                Some(Err(error))
            }
        }
    })
}

/// One OBU at the start of `data`, and the data after it.
fn parse_one(data: &[u8]) -> Result<(Obu<'_>, &[u8]), Error> {
    let (&header, mut rest) = data.split_first().ok_or(Error::Truncated)?;
    if header & 0x80 != 0 {
        return Err(Error::OutOfRange("obu_forbidden_bit"));
    }
    let obu_type = ObuType::from_raw((header >> 3) & 0xf);
    let has_extension = header & 0x4 != 0;
    let has_size = header & 0x2 != 0;
    let extension = if has_extension {
        let (&byte, after) = rest.split_first().ok_or(Error::Truncated)?;
        rest = after;
        Some((byte >> 5, (byte >> 3) & 3))
    } else {
        None
    };
    let (payload, rest) = if has_size {
        let (size, used) = leb128(rest)?;
        rest.get(used..)
            .and_then(|sized| sized.split_at_checked(size.to_usize()))
            .ok_or(Error::Truncated)?
    } else {
        (rest, &[][..])
    };
    Ok((
        Obu {
            obu_type,
            extension,
            payload,
        },
        rest,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_temporal_unit_splits_into_its_obus() -> Result<(), Error> {
        // A temporal delimiter (size 0), a padding OBU with an extension
        // header (temporal 2, spatial 1) and 2 bytes, then a frame OBU with
        // no size field, running to the end.
        let data = [
            0x12,
            0x00, //
            0x7e,
            0b0100_1000,
            0x02,
            0xaa,
            0xbb, //
            0x30,
            0x01,
            0x02,
            0x03,
        ];
        let all: Vec<Obu<'_>> = obus(&data).collect::<Result<_, _>>()?;
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].obu_type, ObuType::TemporalDelimiter);
        assert!(all[0].payload.is_empty());
        assert_eq!(all[1].obu_type, ObuType::Padding);
        assert_eq!(all[1].extension, Some((2, 1)));
        assert_eq!(all[1].payload, [0xaa, 0xbb]);
        assert_eq!(all[2].obu_type, ObuType::Frame);
        assert_eq!(all[2].payload, [0x01, 0x02, 0x03]);
        Ok(())
    }

    #[test]
    fn operating_points_drop_other_layers() {
        let obu = Obu {
            obu_type: ObuType::Frame,
            extension: Some((1, 0)),
            payload: &[],
        };
        // Temporal layers 0 and 1 of spatial layer 0.
        assert!(obu.in_operating_point(0x103));
        // Temporal layer 0 alone.
        assert!(!obu.in_operating_point(0x101));
        assert!(obu.in_operating_point(0));
    }

    #[test]
    fn a_size_past_the_end_is_truncated() {
        let data = [0x0a, 0x05, 0x00];
        assert_eq!(obus(&data).next(), Some(Err(Error::Truncated)));
        let data = [0x8a, 0x00];
        assert_eq!(
            obus(&data).next(),
            Some(Err(Error::OutOfRange("obu_forbidden_bit")))
        );
        // An extension flag with no extension byte, and a size field cut
        // short.
        assert_eq!(obus(&[0x34]).next(), Some(Err(Error::Truncated)));
        assert_eq!(obus(&[0x32, 0x80]).next(), Some(Err(Error::Truncated)));
        // After an error, nothing more.
        let data = [0x0a, 0x05, 0x12, 0x00];
        assert_eq!(obus(&data).count(), 1);
    }
}
