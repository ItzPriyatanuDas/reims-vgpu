//! The scaler's requests as the guest lays them out: a buffer `R` of
//! 0x20 bytes for the capabilities (type 1) or 0x610 for a transform
//! (type 2), every field in the guest's byte order, little-endian on both
//! of the guest's architectures.
//!
//! The guest kext builds `R` in `submitTransformTransaction` and `start`, and a
//! transform's 0x1b0 bytes at `R+8` are the request IOSurfaceAccelerator builds for its
//! scaler. The names here are ours, for what each field holds; a field whose meaning
//! nothing states is `field_xx`, `xx` its offset in the transform.

use crate::num::{low_u16, low_u32};

/// `R+0`: the request's type.
pub const TYPE: usize = 0x0;
/// `R+4`: the status the host writes back.
pub const STATUS: usize = 0x4;

/// A capabilities request's type.
pub const TYPE_CAPABILITIES: u32 = 1;
/// A transform request's type.
pub const TYPE_TRANSFORM: u32 = 2;

/// A capabilities request's size.
pub const CAPABILITIES_SIZE: usize = 0x20;
/// Type 1, `R+8`: the guest-physical address the capabilities go to.
pub const CAPABILITIES_ADDRESS: usize = 0x8;
/// Type 1, `R+0x10`: where Apple's host writes its own pointer to that
/// memory.
pub const CAPABILITIES_HOST_POINTER: usize = 0x10;
/// Type 1, `R+0x18`: the bytes the guest has room for; the host writes
/// back the bytes it wrote.
pub const CAPABILITIES_LENGTH: usize = 0x18;

/// A transform request's size.
pub const TRANSFORM_SIZE: usize = 0x610;
/// Type 2, `R+8`: the transform, 0x1b0 bytes.
pub const TRANSFORM: usize = 0x8;
/// The transform's size.
pub const TRANSFORM_LENGTH: usize = 0x1b0;
/// Type 2, `R+0x1b8`: the source surface's mapping ID.
pub const SOURCE_MAPPING_ID: usize = 0x1b8;
/// Type 2, `R+0x1c0`: the destination surface's mapping ID.
pub const DESTINATION_MAPPING_ID: usize = 0x1c0;
/// Type 2, `R+0x1c8`: eight scatter-gather lists, 0x88 bytes each.
pub const LISTS: usize = 0x1c8;
/// A scatter-gather list's size: eight entries of 16 bytes, then a count.
pub const LIST_LENGTH: usize = 0x88;
/// The entries a list has room for.
pub const LIST_ENTRIES: usize = 8;
/// Type 2, `R+0x608`: 1 when the side buffers are used.
pub const SIDE_BUFFERS_USED: usize = 0x608;
/// Type 2, `R+0x118`: four pairs of side buffers, 0x28 bytes each (the
/// transform's `comm` slots).
pub const SIDE_BUFFERS: usize = TRANSFORM + 0x110;
/// A pair's size.
pub const SIDE_BUFFER_PAIR_LENGTH: usize = 0x28;

/// Reads a little-endian u32 at `offset`; 0 past the end.
pub fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    bytes
        .get(offset..offset + 4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u32::from_le_bytes)
}

/// Reads a little-endian u64 at `offset`; 0 past the end.
pub fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    bytes
        .get(offset..offset + 8)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u64::from_le_bytes)
}

/// Writes a little-endian u32 at `offset`, if it fits.
pub fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    if let Some(b) = bytes.get_mut(offset..offset + 4) {
        b.copy_from_slice(&value.to_le_bytes());
    }
}

/// Writes a little-endian u64 at `offset`, if it fits.
pub fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    if let Some(b) = bytes.get_mut(offset..offset + 8) {
        b.copy_from_slice(&value.to_le_bytes());
    }
}

/// A rectangle in whole pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    /// Its left edge.
    pub x: u32,
    /// Its top edge.
    pub y: u32,
    /// Its width.
    pub width: u32,
    /// Its height.
    pub height: u32,
}

/// A rectangle in 16.16 fixed point: the source window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FixedRect {
    /// Its left edge.
    pub x: u64,
    /// Its top edge.
    pub y: u64,
    /// Its width.
    pub width: u64,
    /// Its height.
    pub height: u64,
}

/// One fixed-point unit: 16.16.
pub const FIXED_ONE: u64 = 1 << 16;

/// A shared event the scaler waits on and signals (`port`, a Mach port in
/// the guest's task). The host clears both before the transform runs, as
/// Apple's does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SharedEvent {
    /// The event's port.
    pub port: u32,
    /// The value waited for.
    pub wait: u64,
    /// The value signalled.
    pub signal: u64,
}

/// The border the `BorderFill` options describe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BorderFill {
    /// Its rectangle.
    pub rect: Rect,
    /// Its alpha.
    pub alpha: u32,
    /// Its red, or luma.
    pub red_y: u32,
    /// Its green, or Cb.
    pub green_cb: u32,
    /// Its blue, or Cr.
    pub blue_cr: u32,
}

/// One of the transform's four side-buffer pairs (`comm`): an inbound
/// buffer the scaler reads and an outbound one it writes, each a kind, an
/// address in the guest's task and a size. The guest kext copies each to
/// the host through scatter-gather lists; the addresses here are the
/// guest's own and mean nothing to the host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SideBufferPair {
    /// The inbound buffer's kind, 0 for none (a signed compare in Apple's
    /// host and in the guest kext).
    pub input_kind: i32,
    /// Its address in the guest.
    pub input_address: u64,
    /// Its size.
    pub input_size: u32,
    /// The outbound buffer's kind, 0 for none.
    pub output_kind: i32,
    /// Its address in the guest.
    pub output_address: u64,
    /// Its size.
    pub output_size: u32,
}

impl SideBufferPair {
    /// Pair `index` (0 to 3) of a transform request `r`.
    pub fn read(r: &[u8], index: usize) -> SideBufferPair {
        let at = SIDE_BUFFERS + SIDE_BUFFER_PAIR_LENGTH * index;
        SideBufferPair {
            input_kind: u32_at(r, at).cast_signed(),
            input_address: u64_at(r, at + 0x8),
            input_size: u32_at(r, at + 0x10),
            output_kind: u32_at(r, at + 0x14).cast_signed(),
            output_address: u64_at(r, at + 0x18),
            output_size: u32_at(r, at + 0x20),
        }
    }
}

/// One scatter-gather list: up to eight guest-physical segments and their
/// count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScatterGatherList {
    /// Each segment's guest-physical address and length.
    pub entries: [(u64, u32); LIST_ENTRIES],
    /// How many of them the guest filled; more than eight is Apple's
    /// host's abort.
    pub count: u32,
}

impl ScatterGatherList {
    /// List `index` (0 to 7) of a transform request `r`.
    pub fn read(r: &[u8], index: usize) -> ScatterGatherList {
        let at = LISTS + LIST_LENGTH * index;
        let mut entries = [(0, 0); LIST_ENTRIES];
        for (i, entry) in entries.iter_mut().enumerate() {
            *entry = (u64_at(r, at + 16 * i), u32_at(r, at + 16 * i + 8));
        }
        ScatterGatherList {
            entries,
            count: u32_at(r, at + 0x80),
        }
    }
}

/// The transform's flags word, each bit named by the option of
/// IOSurfaceAccelerator that sets it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransformFlags(pub u64);

impl TransformFlags {
    /// Bits 0 to 5, the `Transform` option: at most one bit set above
    /// bit 1. Measured on Apple's scaler: bit 0 flips vertically, bit 1
    /// horizontally, and bit 2 rotates 90 degrees clockwise, the flips
    /// after the rotation.
    pub const fn symmetric_transform(self) -> u32 {
        low_u32(self.0 & 0x3f)
    }
    /// Bit 6, `CustomFilter`; the guest kext clears it, having no custom
    /// filters.
    pub const fn custom_filter(self) -> bool {
        self.0 & 0x40 != 0
    }
    /// Bit 7, `NearestFilter`.
    pub const fn nearest_filter(self) -> bool {
        self.0 & 0x80 != 0
    }
    /// Bit 10, `SkipAsyncCallback`.
    pub const fn skip_async_callback(self) -> bool {
        self.0 & 0x400 != 0
    }
    /// Bit 12, `FixUpscaling`; also set by every `TransformSurface` call.
    pub const fn fix_upscaling(self) -> bool {
        self.0 & 0x1000 != 0
    }
    /// Bits 13 and 14, `DitherControl` (default 1).
    pub const fn dither_control(self) -> u32 {
        low_u32((self.0 >> 13) & 3)
    }
    /// Bits 15 to 17: the destination's chroma siting from its
    /// attachments, 1 left to 6 bottom, 0 for none.
    pub const fn cosite(self) -> u32 {
        low_u32((self.0 >> 15) & 7)
    }
    /// Bit 18, `AlphaOverride`: the transform's alpha override is set.
    pub const fn alpha_override(self) -> bool {
        self.0 & 0x4_0000 != 0
    }
    /// Bit 28, the `BorderFill` options.
    pub const fn border_fill(self) -> bool {
        self.0 & 0x1000_0000 != 0
    }
    /// Bit 29, `HistogramBinMode`.
    pub const fn histogram(self) -> bool {
        self.0 & 0x2000_0000 != 0
    }
    /// Bit 30, `ZeroFill`.
    pub const fn zero_fill(self) -> bool {
        self.0 & 0x4000_0000 != 0
    }
    /// Bits 36 to 38, `ColorRemapMode` (under 5).
    pub const fn color_remap_mode(self) -> u32 {
        low_u32((self.0 >> 36) & 7)
    }
    /// Bit 40, `HDREnable`: the side buffers carry HDR parameters.
    pub const fn hdr(self) -> bool {
        self.0 & (1 << 40) != 0
    }
    /// Bit 41, `EnhanceEnable`.
    pub const fn enhance(self) -> bool {
        self.0 & (1 << 41) != 0
    }
    /// Bit 48, `ITUVariantCSCEnable`.
    pub const fn itu_variant_csc(self) -> bool {
        self.0 & (1 << 48) != 0
    }
    /// Bit 49, the `WriteOnlyMode` options.
    pub const fn write_only_mode(self) -> bool {
        self.0 & (1 << 49) != 0
    }
    /// Bit 50, `FilmGrainEnable`.
    pub const fn film_grain(self) -> bool {
        self.0 & (1 << 50) != 0
    }
    /// Bit 57, `BorderFillOddSizeYCbCrEnable`.
    pub const fn border_fill_odd_size(self) -> bool {
        self.0 & (1 << 57) != 0
    }
}

/// A transform, the 0x1b0 bytes at `R+8`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Transform {
    /// The source surface's ID in the guest (Apple's host writes its own
    /// surface's ID here).
    pub source_id: u32,
    /// The destination's.
    pub destination_id: u32,
    /// The guest's completion callback, nonzero for an asynchronous
    /// transform, and its two words.
    pub completion: [u64; 3],
    /// The flags.
    pub flags: TransformFlags,
    /// The source window, 16.16; all zero for the whole source.
    pub source_rect: FixedRect,
    /// The source surface's width and height as the guest's client saw
    /// them.
    pub source_size: (u32, u32),
    /// +0x50, zeroed by the framework.
    pub field_50: u64,
    /// +0x58, zeroed by the framework.
    pub field_58: u64,
    /// The destination window; all zero for the whole destination.
    pub destination_rect: Rect,
    /// The destination's width and height as the guest's client saw them.
    pub destination_size: (u32, u32),
    /// The event to wait on before the transform.
    pub source_event: SharedEvent,
    /// The event to signal after it.
    pub destination_event: SharedEvent,
    /// The alpha the destination takes, with [`TransformFlags::alpha_override`].
    pub alpha_override: u32,
    /// The border, with [`TransformFlags::border_fill`].
    pub border_fill: BorderFill,
    /// The write-only mode's alpha, red or luma, green or Cb, blue or Cr.
    pub write_only_mode: [u16; 4],
    /// +0xd4.
    pub field_d4: u32,
    /// +0xd8.
    pub field_d8: u32,
    /// The histogram's bin mode, window and programmable bins, with
    /// [`TransformFlags::histogram`].
    pub histogram: [u32; 13],
    /// The four side-buffer pairs.
    pub side_buffers: [SideBufferPair; 4],
}

impl Transform {
    /// The transform of a transform request `r` (at `R+8`).
    pub fn read(r: &[u8]) -> Transform {
        let t = |offset: usize| TRANSFORM + offset;
        let event = |offset: usize| SharedEvent {
            port: u32_at(r, t(offset)),
            wait: u64_at(r, t(offset + 8)),
            signal: u64_at(r, t(offset + 0x10)),
        };
        let mut histogram = [0u32; 13];
        for (i, word) in histogram.iter_mut().enumerate() {
            *word = u32_at(r, t(0xdc + 4 * i));
        }
        let mut write_only_mode = [0u16; 4];
        for (i, word) in write_only_mode.iter_mut().enumerate() {
            *word = low_u16(u32_at(r, t(0xcc + 2 * i)));
        }
        Transform {
            source_id: u32_at(r, t(0)),
            destination_id: u32_at(r, t(4)),
            completion: [u64_at(r, t(8)), u64_at(r, t(0x10)), u64_at(r, t(0x18))],
            flags: TransformFlags(u64_at(r, t(0x20))),
            source_rect: FixedRect {
                x: u64_at(r, t(0x28)),
                y: u64_at(r, t(0x30)),
                width: u64_at(r, t(0x38)),
                height: u64_at(r, t(0x40)),
            },
            source_size: (u32_at(r, t(0x48)), u32_at(r, t(0x4c))),
            field_50: u64_at(r, t(0x50)),
            field_58: u64_at(r, t(0x58)),
            destination_rect: Rect {
                x: u32_at(r, t(0x60)),
                y: u32_at(r, t(0x64)),
                width: u32_at(r, t(0x68)),
                height: u32_at(r, t(0x6c)),
            },
            destination_size: (u32_at(r, t(0x70)), u32_at(r, t(0x74))),
            source_event: event(0x78),
            destination_event: event(0x90),
            alpha_override: u32_at(r, t(0xa8)),
            border_fill: BorderFill {
                rect: Rect {
                    x: u32_at(r, t(0xac)),
                    y: u32_at(r, t(0xb0)),
                    width: u32_at(r, t(0xb4)),
                    height: u32_at(r, t(0xb8)),
                },
                alpha: u32_at(r, t(0xbc)),
                red_y: u32_at(r, t(0xc0)),
                green_cb: u32_at(r, t(0xc4)),
                blue_cr: u32_at(r, t(0xc8)),
            },
            write_only_mode,
            field_d4: u32_at(r, t(0xd4)),
            field_d8: u32_at(r, t(0xd8)),
            histogram,
            side_buffers: [0, 1, 2, 3].map(|i| SideBufferPair::read(r, i)),
        }
    }
}

/// What Apple's host does to the transform before its kernel sees it
/// (`IOSurfaceAcceleratorParavirtProcessGuestData`): an asynchronous
/// transform's completion is cleared, and both shared events always are.
/// The reply carries `R` so changed.
pub fn clear_completion_and_events(r: &mut [u8]) {
    let t = |offset: usize| TRANSFORM + offset;
    if u64_at(r, t(8)) != 0 {
        for offset in [8, 0x10, 0x18] {
            put_u64(r, t(offset), 0);
        }
    }
    for event in [0x78, 0x90] {
        put_u32(r, t(event), 0);
        put_u64(r, t(event + 8), 0);
        put_u64(r, t(event + 0x10), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layout_ends_where_the_guest_kext_puts_the_flag() {
        assert_eq!(TRANSFORM + TRANSFORM_LENGTH, SOURCE_MAPPING_ID);
        assert_eq!(
            SIDE_BUFFERS + 4 * SIDE_BUFFER_PAIR_LENGTH,
            SOURCE_MAPPING_ID
        );
        assert_eq!(LISTS + 8 * LIST_LENGTH, SIDE_BUFFERS_USED);
        assert_eq!(SIDE_BUFFERS_USED + 8, TRANSFORM_SIZE);
    }

    #[test]
    fn a_transform_reads_its_fields_and_pairs() {
        let mut r = vec![0u8; TRANSFORM_SIZE];
        put_u32(&mut r, TRANSFORM, 7);
        put_u64(&mut r, TRANSFORM + 0x20, 0x4 | (1 << 13));
        put_u64(&mut r, TRANSFORM + 0x38, 640 << 16);
        put_u32(&mut r, TRANSFORM + 0x68, 320);
        put_u32(&mut r, SIDE_BUFFERS + 0x28 * 2, 3);
        put_u32(&mut r, SIDE_BUFFERS + 0x28 * 2 + 0x10, 0x200);
        put_u32(&mut r, LISTS + LIST_LENGTH + 0x80, 9);
        let t = Transform::read(&r);
        assert_eq!(t.source_id, 7);
        assert_eq!(t.flags.symmetric_transform(), 4);
        assert_eq!(t.flags.dither_control(), 1);
        assert_eq!(t.source_rect.width, 640 << 16);
        assert_eq!(t.destination_rect.width, 320);
        assert_eq!(t.side_buffers[2].input_kind, 3);
        assert_eq!(t.side_buffers[2].input_size, 0x200);
        assert_eq!(ScatterGatherList::read(&r, 1).count, 9);
    }

    #[test]
    fn the_events_are_cleared_and_a_completion_only_when_set() {
        let mut r = vec![0xffu8; TRANSFORM_SIZE];
        clear_completion_and_events(&mut r);
        assert_eq!(u64_at(&r, TRANSFORM + 8), 0);
        assert_eq!(u32_at(&r, TRANSFORM + 0x78), 0);
        assert_eq!(
            u32_at(&r, TRANSFORM + 0x7c),
            0xffff_ffff,
            "the padding stays"
        );
        assert_eq!(u64_at(&r, TRANSFORM + 0xa0), 0);
        let mut r = vec![0u8; TRANSFORM_SIZE];
        put_u64(&mut r, TRANSFORM + 0x10, 5);
        clear_completion_and_events(&mut r);
        assert_eq!(u64_at(&r, TRANSFORM + 0x10), 5, "no callback, no clearing");
    }
}
