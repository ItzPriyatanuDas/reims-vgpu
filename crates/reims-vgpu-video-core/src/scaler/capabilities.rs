//! The scaler's capabilities: the 0x288 bytes a type 1 request asks the
//! host to write into guest memory, laid out as the guest kext reads them.
//!
//! The guest keeps them at driver + 0xc8 and publishes them under
//! `IOSurfaceAcceleratorCapabilitiesDict` in its registry entry, one key a
//! field, where IOSurfaceAccelerator's clients read them. It also reads a
//! few itself before a transform's side buffers. Each offset below is the
//! guest's, named by the key it publishes the field under. Apple's own
//! scaler answers the same layout. Bytes this host does not set stay 0,
//! which turns each feature off.

/// The bytes the guest asks for and reads.
pub const LENGTH: usize = 0x288;

/// The format classes, one byte each from +0x8 to +0x23; the guest
/// publishes each as both an input and an output format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum FormatClass {
    /// `...FormatIn2Planes420`: 8-bit 4:2:0 in two planes.
    TwoPlanes420 = 0x8,
    /// `...2Planes422`.
    TwoPlanes422 = 0x9,
    /// `...2Planes444`.
    TwoPlanes444 = 0xa,
    /// `...RGB565`.
    Rgb565 = 0xb,
    /// `...ARGB1555`.
    Argb1555 = 0xc,
    /// `...ARGB8888`.
    Argb8888 = 0xd,
    /// `...RGBA8888`.
    Rgba8888 = 0xe,
    /// `...PMARGB8888`.
    PremultipliedArgb8888 = 0xf,
    /// `...Interleaved422`: 8-bit 4:2:2 in one plane.
    Interleaved422 = 0x10,
    /// `...ARGB2101010`.
    Argb2101010 = 0x11,
    /// `...ARGB8101010`.
    Argb8101010 = 0x12,
    /// `...ARGBV16`.
    ArgbV16 = 0x14,
    /// `...PARGBV16`.
    PremultipliedArgbV16 = 0x15,
    /// `...1PlaneYCBCR10444`.
    OnePlaneYCbCr10444 = 0x16,
    /// `...LumaOnlyL008` and `...1PlaneYonly8`.
    LumaOnly8 = 0x17,
    /// `...LumaOnlyL010` and `...1PlaneYonly10`.
    LumaOnly10 = 0x18,
    /// `...1PlanePackedYonly10`.
    PackedLumaOnly10 = 0x19,
    /// `...LumaOnlyL012` and `...1PlaneYonly12`.
    LumaOnly12 = 0x1a,
    /// `...1PlanePackedYonly12`.
    PackedLumaOnly12 = 0x1b,
    /// `...LumaOnlyL016`.
    LumaOnly16 = 0x1c,
    /// `...1PlaneYCBCR10422`.
    OnePlaneYCbCr10422 = 0x1d,
    /// `...2Planes42010`: 10-bit 4:2:0 in two planes.
    TwoPlanes42010 = 0x1e,
    /// `...2Planes42210`.
    TwoPlanes42210 = 0x1f,
    /// `...2Planes44410`.
    TwoPlanes44410 = 0x20,
    /// `...2PlanesPacked42010`.
    TwoPlanesPacked42010 = 0x21,
    /// `...2PlanesPacked42210`.
    TwoPlanesPacked42210 = 0x22,
    /// `...2PlanesPacked44410`.
    TwoPlanesPacked44410 = 0x23,
}

impl FormatClass {
    /// Its byte's offset.
    #[expect(
        clippy::as_conversions,
        reason = "a class's discriminant is its byte's offset"
    )]
    const fn offset(self) -> usize {
        self as usize
    }
}

/// What a scaler can do, as the guest reads it. Everything not here is
/// written 0: no HDR, ASE or comm API (so the guest never sends side
/// buffers), no histogram, no compression, no directional or VRA scaling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// +0x4, `IOSurfaceAcceleratorScalerVersion`.
    pub scaler_version: u32,
    /// The format classes it reads and writes.
    pub formats: Vec<FormatClass>,
    /// +0x28, `...MinSourceWidth` and `...MinSourceHeight`.
    pub min_source: u32,
    /// +0x30 and +0x34, `...MaxSourceWidth`, `...MaxSourceHeight`.
    pub max_source: (u32, u32),
    /// +0x40, `...MinDestWidth` and `...MinDestHeight`.
    pub min_destination: u32,
    /// +0x44 and +0x48, `...MaxDestWidth`, `...MaxDestHeight`.
    pub max_destination: (u32, u32),
    /// +0x128, +0x12c, +0x130 and +0x134: `...MaxHUpscale`,
    /// `...MaxHDownscale`, `...MaxVUpscale`, `...MaxVDownscale`.
    pub max_scale: [u32; 4],
    /// +0x10c and +0x10d, `...SourceXYOffsetsWithTiling` and
    /// `...DestXYOffsetsWithTiling`.
    pub xy_offsets_with_tiling: (bool, bool),
}

impl Capabilities {
    /// The 0x288 bytes the guest reads.
    pub fn to_bytes(&self) -> [u8; LENGTH] {
        let mut b = [0u8; LENGTH];
        // Every offset is a constant within the 0x288 bytes.
        let mut put = |offset: usize, value: &[u8]| {
            if let Some(field) = b.get_mut(offset..offset + value.len()) {
                field.copy_from_slice(value);
            }
        };
        put(0x4, &self.scaler_version.to_le_bytes());
        put(0x28, &self.min_source.to_le_bytes());
        put(0x30, &self.max_source.0.to_le_bytes());
        put(0x34, &self.max_source.1.to_le_bytes());
        put(0x40, &self.min_destination.to_le_bytes());
        put(0x44, &self.max_destination.0.to_le_bytes());
        put(0x48, &self.max_destination.1.to_le_bytes());
        for (i, ratio) in self.max_scale.iter().enumerate() {
            put(0x128 + 4 * i, &ratio.to_le_bytes());
        }
        for class in &self.formats {
            put(class.offset(), &[1]);
        }
        put(0x10c, &[u8::from(self.xy_offsets_with_tiling.0)]);
        put(0x10d, &[u8::from(self.xy_offsets_with_tiling.1)]);
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// +0x0, which `mapCommParams` reads as the side buffers' version.
    const COMM_VERSION: usize = 0x0;
    /// +0x183, the ASE (legacy and HCU) side buffers' switch.
    const ASE: usize = 0x183;
    /// +0x184, `...CapabilitiesHDR` and `HDREnable`: the side buffers'
    /// switch.
    const HDR: usize = 0x184;
    /// +0x1b2, `IOSurfaceAcceleratorCommApi`.
    const COMM_API: usize = 0x1b2;

    #[test]
    fn each_field_lands_where_the_guest_reads_it() {
        let caps = Capabilities {
            scaler_version: 0x0102_0304,
            formats: vec![FormatClass::TwoPlanes420, FormatClass::TwoPlanes42010],
            min_source: 4,
            max_source: (8192, 4352),
            min_destination: 2,
            max_destination: (16384, 16384),
            max_scale: [8, 8, 8, 4],
            xy_offsets_with_tiling: (true, false),
        };
        let b = caps.to_bytes();
        assert_eq!(&b[4..8], &[4, 3, 2, 1]);
        assert_eq!((b[0x8], b[0x9], b[0x1e], b[0x1f]), (1, 0, 1, 0));
        assert_eq!(
            u32::from_le_bytes([b[0x34], b[0x35], b[0x36], b[0x37]]),
            4352
        );
        assert_eq!(b[0x134], 4);
        assert_eq!((b[0x10c], b[0x10d]), (1, 0));
        assert_eq!((b[HDR], b[ASE], b[COMM_API], b[COMM_VERSION]), (0, 0, 0, 0));
    }
}
