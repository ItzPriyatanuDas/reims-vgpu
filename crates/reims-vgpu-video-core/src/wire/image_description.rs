//! Format descriptions as `=fds` parameters carry them: a big-endian
//! QuickTime ImageDescription, the 86-byte header and then extension atoms
//! such as `avcC`, which Apple's host writes with
//! `CMVideoFormatDescriptionCopyAsBigEndianImageDescriptionBlockBuffer` and
//! reads back with the matching create.
//!
//! The layout below is QuickTime's, and the values the writer fills in are
//! the ones CoreMedia wrote for a 64x48 H.264 description on the M3 Max
//! (macOS 27, 26A434, 2026-10-24): data reference index 0xffff, qualities 0x200,
//! 72 dpi, one frame, depth 24, no colour table, and a 4-byte zero after the
//! extensions.

use crate::fourcc::FourCc;
use crate::wire::WireError;
use crate::wire::atom::{read_u16, read_u32};

/// The ImageDescription header's size.
pub const HEADER_SIZE: usize = 86;

/// The `avcC` extension: an AVCDecoderConfigurationRecord.
pub const AVC_CONFIGURATION: FourCc = FourCc::new(*b"avcC");
/// The `hvcC` extension: an HEVCDecoderConfigurationRecord.
pub const HEVC_CONFIGURATION: FourCc = FourCc::new(*b"hvcC");
/// The `av1C` extension: an AV1CodecConfigurationRecord. CoreMedia keeps it
/// among a description's sample description extension atoms, as it keeps
/// `avcC` (measured on the M3 Max, macOS 27, 2026-10-24: an AV1 MP4's format
/// description carries `av1C` and `btrt` there).
pub const AV1_CONFIGURATION: FourCc = FourCc::new(*b"av1C");
/// The `vpcC` extension: the box's version and flags, then a
/// VPCodecConfigurationRecord. CoreMedia keeps it among a description's
/// sample description extension atoms, as it keeps `avcC` (measured on the
/// M3 Max, macOS 27, 2026-10-24: a VP9 MP4's format description carries
/// `vpcC`, 12 bytes from version 1, and `btrt`).
pub const VP9_CONFIGURATION: FourCc = FourCc::new(*b"vpcC");

/// A video format description, as far as this side reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageDescription {
    /// The codec type, such as `avc1`.
    pub codec: FourCc,
    /// The coded width.
    pub width: u16,
    /// The coded height.
    pub height: u16,
    /// The pixel depth (24 for colour video).
    pub depth: u16,
    /// The compressor name, a Pascal string in the header.
    pub compressor_name: Vec<u8>,
    /// The extension atoms in order, each as its type and payload.
    pub extensions: Vec<(FourCc, Vec<u8>)>,
}

impl ImageDescription {
    /// Reads an ImageDescription. Its own size field bounds it; the
    /// extensions end at that size, at a zero size field, or at a tail too
    /// short for an atom.
    pub fn parse(bytes: &[u8]) -> Result<Self, WireError> {
        let size = read_u32(bytes, 0)
            .and_then(|size| usize::try_from(size).ok())
            .ok_or(WireError::Malformed("image description size"))?;
        if size < HEADER_SIZE || size > bytes.len() {
            return Err(WireError::Malformed("image description size"));
        }
        let description = bytes
            .get(..size)
            .ok_or(WireError::Malformed("image description size"))?;
        let field16 =
            |offset| read_u16(description, offset).ok_or(WireError::Malformed("image description"));
        let name = description
            .get(50..82)
            .ok_or(WireError::Malformed("image description"))?;
        let name_length = usize::from(name.first().copied().unwrap_or(0)).min(31);
        let mut extensions = Vec::new();
        let mut offset = HEADER_SIZE;
        while offset + 8 <= description.len() {
            let atom_size = read_u32(description, offset)
                .and_then(|size| usize::try_from(size).ok())
                .ok_or(WireError::Malformed("extension size"))?;
            if atom_size == 0 {
                break;
            }
            if atom_size < 8 || atom_size > description.len() - offset {
                return Err(WireError::Malformed("extension size"));
            }
            let kind =
                read_u32(description, offset + 4).ok_or(WireError::Malformed("extension type"))?;
            let payload = description
                .get(offset + 8..offset + atom_size)
                .ok_or(WireError::Malformed("extension size"))?;
            extensions.push((FourCc(kind), payload.to_vec()));
            offset += atom_size;
        }
        Ok(Self {
            codec: FourCc(read_u32(description, 4).ok_or(WireError::Malformed("codec type"))?),
            width: field16(32)?,
            height: field16(34)?,
            depth: field16(82)?,
            compressor_name: name.get(1..1 + name_length).unwrap_or_default().to_vec(),
            extensions,
        })
    }

    /// The first extension of type `kind`.
    pub fn extension(&self, kind: FourCc) -> Option<&[u8]> {
        self.extensions
            .iter()
            .find(|(found, _)| *found == kind)
            .map(|(_, payload)| payload.as_slice())
    }

    /// Writes the description as CoreMedia lays it out.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER_SIZE + 64);
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&self.codec.to_be_bytes());
        bytes.extend_from_slice(&[0; 6]);
        bytes.extend_from_slice(&0xffff_u16.to_be_bytes());
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&0x200_u32.to_be_bytes());
        bytes.extend_from_slice(&0x200_u32.to_be_bytes());
        bytes.extend_from_slice(&self.width.to_be_bytes());
        bytes.extend_from_slice(&self.height.to_be_bytes());
        bytes.extend_from_slice(&0x0048_0000_u32.to_be_bytes());
        bytes.extend_from_slice(&0x0048_0000_u32.to_be_bytes());
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&1_u16.to_be_bytes());
        // A Pascal string in 32 bytes: its length, then up to 31 bytes.
        let name = (self.compressor_name.get(..31)).unwrap_or(&self.compressor_name);
        bytes.push(u8::try_from(name.len()).unwrap_or(31));
        bytes.extend_from_slice(name);
        bytes.resize(bytes.len() + (31 - name.len()), 0);
        bytes.extend_from_slice(&self.depth.to_be_bytes());
        bytes.extend_from_slice(&0xffff_u16.to_be_bytes());
        for (kind, payload) in &self.extensions {
            let size = u32::try_from(payload.len() + 8).unwrap_or(u32::MAX);
            bytes.extend_from_slice(&size.to_be_bytes());
            bytes.extend_from_slice(&kind.to_be_bytes());
            bytes.extend_from_slice(payload);
        }
        bytes.extend_from_slice(&[0; 4]);
        let size = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        if let Some(header) = bytes.get_mut(0..4) {
            header.copy_from_slice(&size.to_be_bytes());
        }
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64x48 `avc1` description with a 7-byte `avcC`, written byte by
    /// byte from the QuickTime layout.
    fn hand_written() -> Vec<u8> {
        let mut bytes = vec![0, 0, 0, 105];
        bytes.extend_from_slice(b"avc1");
        bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0xff, 0xff]);
        bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 2, 0, 0, 0, 2, 0]);
        bytes.extend_from_slice(&[0, 64, 0, 48]);
        bytes.extend_from_slice(&[0, 0x48, 0, 0, 0, 0x48, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 0, 0, 0, 1]);
        let mut name = [0; 32];
        name[..6].copy_from_slice(b"\x05H.264");
        bytes.extend_from_slice(&name);
        bytes.extend_from_slice(&[0, 24, 0xff, 0xff]);
        bytes.extend_from_slice(&[0, 0, 0, 15]);
        bytes.extend_from_slice(b"avcC");
        bytes.extend_from_slice(&[1, 0x64, 0, 0x0b, 0xff, 0xe0, 0]);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes
    }

    fn expected() -> ImageDescription {
        ImageDescription {
            codec: FourCc::new(*b"avc1"),
            width: 64,
            height: 48,
            depth: 24,
            compressor_name: b"H.264".to_vec(),
            extensions: vec![(AVC_CONFIGURATION, vec![1, 0x64, 0, 0x0b, 0xff, 0xe0, 0])],
        }
    }

    #[test]
    fn reads_a_hand_written_description() -> Result<(), WireError> {
        let bytes = hand_written();
        assert_eq!(bytes.len(), 105);
        let description = ImageDescription::parse(&bytes)?;
        assert_eq!(description, expected());
        assert_eq!(
            description.extension(AVC_CONFIGURATION),
            Some(&[1, 0x64, 0, 0x0b, 0xff, 0xe0, 0][..])
        );
        assert_eq!(description.extension(HEVC_CONFIGURATION), None);
        Ok(())
    }

    #[test]
    fn writes_what_coremedia_writes() {
        assert_eq!(expected().to_bytes(), hand_written());
    }

    #[test]
    fn a_description_shorter_than_its_header_is_malformed() {
        let mut bytes = hand_written();
        bytes.truncate(80);
        assert_eq!(
            ImageDescription::parse(&bytes),
            Err(WireError::Malformed("image description size"))
        );
    }

    #[test]
    fn an_extension_past_the_end_is_malformed() {
        let mut bytes = hand_written();
        bytes[HEADER_SIZE + 3] = 40;
        assert_eq!(
            ImageDescription::parse(&bytes),
            Err(WireError::Malformed("extension size"))
        );
    }
}
