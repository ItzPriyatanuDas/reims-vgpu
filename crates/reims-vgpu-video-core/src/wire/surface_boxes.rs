//! The two boxes that name guest surfaces by mapping ID: `subs`, which the
//! guest kext puts before a message that lends the host surfaces, and
//! `rels`, which the host sends when it holds a surface no longer.

use crate::fourcc::FourCc;
use crate::wire::atom::{read_u32, read_u64};

/// A guest surface's mapping ID: the name the guest's IOSurface mapper
/// gives it, which the host resolves to the surface's memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MappingId(pub u64);

/// The `subs` box's type.
pub const SURFACE_LIST_ATOM: FourCc = FourCc::new(*b"subs");
/// The `rels` box's type.
pub const SURFACE_RELEASE_ATOM: FourCc = FourCc::new(*b"rels");
/// The only size a `subs` box may have: its header and five mapping IDs.
pub const SURFACE_LIST_SIZE: usize = 0x30;
/// The most surfaces one message names.
pub const MAX_SURFACES: usize = 5;

/// What the device makes of a message's leading `subs` box.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceList {
    /// The message names these surfaces: the box's big-endian IDs up to
    /// the first zero. The device resolves each one and delivers the
    /// message, box included, with them.
    Surfaces(Vec<MappingId>),
    /// The message names no surfaces: it does not start with a `subs` box,
    /// or the box's first ID is zero.
    None,
    /// The device logs these and delivers the message with no surfaces.
    Rejected(SurfaceListRejection),
}

/// Why the device delivered a message without its surfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceListRejection {
    /// Fewer than 8 bytes ("message_length %zd too small").
    TooShort,
    /// The first box is longer than the message ("first_box_size %u
    /// exceeds message_length").
    FirstBoxPastTheEnd,
    /// A `subs` box of another size ("first_box_size %u is not 48").
    WrongSize,
}

/// Reads the surfaces a guest message names, as the device does before it
/// delivers the message.
pub fn surface_list(message: &[u8]) -> SurfaceList {
    let (Some(first_size), Some(first_type)) = (read_u32(message, 0), read_u32(message, 4)) else {
        return SurfaceList::Rejected(SurfaceListRejection::TooShort);
    };
    let first_size = usize::try_from(first_size).unwrap_or(usize::MAX);
    if message.len() < first_size {
        return SurfaceList::Rejected(SurfaceListRejection::FirstBoxPastTheEnd);
    }
    if FourCc(first_type) != SURFACE_LIST_ATOM {
        return SurfaceList::None;
    }
    if first_size != SURFACE_LIST_SIZE {
        return SurfaceList::Rejected(SurfaceListRejection::WrongSize);
    }
    let ids: Vec<MappingId> = (0..MAX_SURFACES)
        .map_while(|slot| read_u64(message, 8 + 8 * slot).filter(|id| *id != 0))
        .map(MappingId)
        .collect();
    if ids.is_empty() {
        SurfaceList::None
    } else {
        SurfaceList::Surfaces(ids)
    }
}

/// The `rels` box that tells the guest the host holds these surfaces no
/// longer: a big-endian size of 8 plus 8 per ID, the type, then each ID
/// big-endian. It travels alone, not inside a `vtpv` message.
pub fn surface_release(ids: &[MappingId]) -> Vec<u8> {
    let size = u32::try_from(8 + 8 * ids.len()).unwrap_or(u32::MAX);
    let mut bytes = Vec::with_capacity(8 + 8 * ids.len());
    bytes.extend_from_slice(&size.to_be_bytes());
    bytes.extend_from_slice(&SURFACE_RELEASE_ATOM.to_be_bytes());
    for id in ids {
        bytes.extend_from_slice(&id.0.to_be_bytes());
    }
    bytes
}

/// A host message's leading `rels` box, as the guest kext reads it: its
/// IDs and the rest of the message, which the kext delivers on its own
/// when it is not empty. The kext counts the IDs from the whole message's
/// length, so a `rels` box must span the message to within 7 bytes, and it
/// must name at least one surface. The kext skips a zero ID, which this
/// gives with the others. `None` when the message does not start with a
/// box the kext accepts.
pub fn read_surface_release(message: &[u8]) -> Option<(Vec<MappingId>, &[u8])> {
    let count = message.len().checked_sub(8)? / 8;
    let size = usize::try_from(read_u32(message, 0)?).ok()?;
    if size > message.len()
        || FourCc(read_u32(message, 4)?) != SURFACE_RELEASE_ATOM
        || size != 8 + 8 * count
        || count == 0
    {
        return None;
    }
    let ids = (0..count)
        .filter_map(|index| read_u64(message, 8 + 8 * index))
        .map(MappingId)
        .collect();
    Some((ids, message.get(size..).unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface_box(ids: [u64; 5]) -> Vec<u8> {
        let mut bytes = vec![0, 0, 0, 0x30];
        bytes.extend_from_slice(b"subs");
        for id in ids {
            bytes.extend_from_slice(&id.to_be_bytes());
        }
        bytes
    }

    #[test]
    fn ids_count_up_to_the_first_zero() {
        let message = surface_box([0x101, 0x102, 0, 0x104, 0]);
        assert_eq!(
            surface_list(&message),
            SurfaceList::Surfaces(vec![MappingId(0x101), MappingId(0x102)])
        );
    }

    #[test]
    fn a_zero_first_id_names_no_surfaces() {
        assert_eq!(
            surface_list(&surface_box([0, 0x102, 0, 0, 0])),
            SurfaceList::None
        );
    }

    #[test]
    fn a_message_without_a_surface_box_names_none() {
        let mut message = vec![0, 0, 0, 48];
        message.extend_from_slice(b"vtpv");
        message.resize(48, 0);
        assert_eq!(surface_list(&message), SurfaceList::None);
    }

    #[test]
    fn the_devices_three_rejections() {
        assert_eq!(
            surface_list(&[0, 0, 0]),
            SurfaceList::Rejected(SurfaceListRejection::TooShort)
        );
        let mut long = surface_box([1, 0, 0, 0, 0]);
        long[3] = 0x40;
        assert_eq!(
            surface_list(&long),
            SurfaceList::Rejected(SurfaceListRejection::FirstBoxPastTheEnd)
        );
        let mut short = surface_box([1, 0, 0, 0, 0]);
        short[3] = 0x28;
        assert_eq!(
            surface_list(&short),
            SurfaceList::Rejected(SurfaceListRejection::WrongSize)
        );
    }

    #[test]
    fn a_release_box_is_size_type_and_ids() {
        let bytes = surface_release(&[MappingId(0x101), MappingId(0x0102_0304_0506_0708)]);
        assert_eq!(
            bytes,
            [
                0, 0, 0, 24, b'r', b'e', b'l', b's', 0, 0, 0, 0, 0, 0, 1, 1, 1, 2, 3, 4, 5, 6, 7, 8
            ]
        );
        assert_eq!(
            read_surface_release(&bytes),
            Some((
                vec![MappingId(0x101), MappingId(0x0102_0304_0506_0708)],
                &[][..]
            ))
        );
    }
}
