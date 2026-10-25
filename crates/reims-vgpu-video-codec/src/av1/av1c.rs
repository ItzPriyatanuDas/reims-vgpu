//! The `av1C` record: AV1CodecConfigurationRecord (AV1 Codec ISO Media
//! File Format Binding, 2.3), which carries a stream's profile, level,
//! colour format and, in `configOBUs`, its sequence header. VideoToolbox's
//! format description carries it as the `av1C` sample description
//! extension atom.

use super::Error;
use super::obu::{ObuType, obus};

/// A parsed `av1C` record, borrowing its OBUs from the record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigurationRecord<'a> {
    /// `seq_profile`.
    pub seq_profile: u8,
    /// `seq_level_idx_0`.
    pub(crate) seq_level_idx_0: u8,
    /// `seq_tier_0`.
    pub(crate) seq_tier_0: bool,
    /// `high_bitdepth`.
    pub(crate) high_bitdepth: bool,
    /// `twelve_bit`.
    pub(crate) twelve_bit: bool,
    /// `monochrome`.
    pub monochrome: bool,
    /// `chroma_subsampling_x`.
    pub chroma_subsampling_x: bool,
    /// `chroma_subsampling_y`.
    pub chroma_subsampling_y: bool,
    /// `chroma_sample_position`.
    pub(crate) chroma_sample_position: u8,
    /// `configOBUs`: zero or more OBUs, in the low overhead format.
    pub(crate) config_obus: &'a [u8],
}

impl<'a> ConfigurationRecord<'a> {
    /// Parses the record's four fixed bytes, and keeps `configOBUs`.
    pub fn parse(record: &'a [u8]) -> Result<Self, Error> {
        let [first, second, third, _, ref config_obus @ ..] = *record else {
            return Err(Error::Av1cTooShort);
        };
        if first & 0x80 == 0 {
            return Err(Error::Av1cMarker);
        }
        if first & 0x7f != 1 {
            return Err(Error::Av1cVersion);
        }
        Ok(Self {
            seq_profile: second >> 5,
            seq_level_idx_0: second & 0x1f,
            seq_tier_0: third & 0x80 != 0,
            high_bitdepth: third & 0x40 != 0,
            twelve_bit: third & 0x20 != 0,
            monochrome: third & 0x10 != 0,
            chroma_subsampling_x: third & 0x08 != 0,
            chroma_subsampling_y: third & 0x04 != 0,
            chroma_sample_position: third & 0x03,
            config_obus,
        })
    }

    /// The bit depth the record says: 8, 10 or 12.
    pub fn bit_depth(&self) -> u8 {
        match (self.high_bitdepth, self.twelve_bit) {
            (false, _) => 8,
            (true, false) => 10,
            (true, true) => 12,
        }
    }

    /// The payload of the first sequence header OBU in `configOBUs`, if
    /// there is one. A record may carry none: the samples then do.
    pub fn sequence_header_obu(&self) -> Result<Option<&'a [u8]>, Error> {
        for obu in obus(self.config_obus) {
            let obu = obu?;
            if obu.obu_type == ObuType::SequenceHeader {
                return Ok(Some(obu.payload));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixed_fields_and_config_obus_read_back() -> Result<(), Error> {
        // As libaom writes it for 10-bit 4:2:0 Main at level 5.1 (13):
        // 0x81, profile 0 | level 13, high_bitdepth with 4:2:0, no delay;
        // then a sequence header OBU of 2 bytes and a padding OBU.
        let record = [0x81, 0x0d, 0x4c, 0x00, 0x0a, 0x02, 0xab, 0xcd, 0x7a, 0x00];
        let parsed = ConfigurationRecord::parse(&record)?;
        assert_eq!(parsed.seq_profile, 0);
        assert_eq!(parsed.seq_level_idx_0, 13);
        assert!(parsed.high_bitdepth && !parsed.twelve_bit && !parsed.monochrome);
        assert!(parsed.chroma_subsampling_x && parsed.chroma_subsampling_y);
        assert_eq!(parsed.bit_depth(), 10);
        assert_eq!(parsed.sequence_header_obu()?, Some(&[0xab, 0xcd][..]));
        Ok(())
    }

    #[test]
    fn a_bad_marker_or_version_is_refused() {
        assert!(ConfigurationRecord::parse(&[0x01, 0, 0, 0]).is_err());
        assert!(ConfigurationRecord::parse(&[0x82, 0, 0, 0]).is_err());
        assert!(ConfigurationRecord::parse(&[0x81, 0, 0]).is_err());
        assert_eq!(
            ConfigurationRecord::parse(&[0x81, 0, 0x0c, 0]).map(|r| r.config_obus.len()),
            Ok(0)
        );
    }
}
