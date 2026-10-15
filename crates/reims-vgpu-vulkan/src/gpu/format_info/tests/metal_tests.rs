//! The tests of `format_info` that compare with Metal on this host: macOS only.

use super::*;

/// Core's query of a level `width` by `height` in `format`.
fn level(format: u64, width: u64, height: u64) -> TextureLevelQuery {
    TextureLevelQuery {
        pixel_format: format,
        width,
        height,
        depth: 1,
        array_length: 1,
        options: 0,
        flags: 0,
    }
}

/// A level's layout is the Metal platform's on this host, word for word: for every value from
/// 0 to 2047 and past 2^32, as core asks it, at every pair of [`EXTENTS`] (whose block counts
/// and bytes wrap at 2^62 + 3 and `u64::MAX` as Metal's do); and for every format the runtime
/// has, at queries core never makes, of other depths and array lengths (2^32 + 5, and
/// 2^62 + 3, where the bytes of a level wrap), levels (the sixth argument, past 63 and 2^32
/// among them) and plane arguments (the seventh), which the device answers by the runtime's
/// rule where this module died before
#[test]
fn levels_are_the_metal_platforms_here() {
    use reims_vgpu_core::platform::device::GpuDevice as _;
    use reims_vgpu_metal::MetalPlatform;

    use crate::gpu::device::tests::dev;

    let d = dev();
    let bc1 = d.texture_level_info(&level(130, 17, 5));
    assert_eq!(
        (bc1.aligned_height, bc1.bytes_per_pixel, bc1.bytes_per_row),
        (8, 8, 40)
    );
    let zero = d.texture_level_info(&level(80, 0, 0));
    assert_eq!(
        (zero.field_0, zero.aligned_height, zero.bytes_per_row),
        (1, 1, 4)
    );
    let Some(metal) = MetalPlatform::system_default_device() else {
        return;
    };
    let values = (0..2048).chain([9999, u64::from(u32::MAX) + 70, (1 << 32) + 80]);
    let mut differ = Vec::new();
    let mut compare = |q: TextureLevelQuery| {
        let (ours, theirs) = (d.texture_level_info(&q), metal.texture_level_info(&q));
        if ours != theirs {
            differ.push(format!("{q:?}: ours {ours:?}, Metal's {theirs:?}"));
        }
    };
    for format in values {
        for width in EXTENTS {
            for height in EXTENTS {
                compare(level(format, width, height));
            }
        }
    }
    for pf in PixelFormat::ALL {
        for (width, height) in [(17, 9), (1000, 7)] {
            for depth in [1, 3, (1 << 32) + 5, (1 << 62) + 3] {
                for array_length in [0, 1, 6, (1 << 32) + 5, (1 << 62) + 3] {
                    for options in [0, 1, 5, 63, 64, 1000, 1 << 32] {
                        for flags in [0, 1, 2, 3, 7, 1 << 31, u64::MAX] {
                            compare(TextureLevelQuery {
                                pixel_format: u64::from(pf.as_raw()),
                                width,
                                height,
                                depth,
                                array_length,
                                options,
                                flags,
                            });
                        }
                    }
                }
            }
        }
    }
    assert!(
        differ.is_empty(),
        "{} differ:\n{}",
        differ.len(),
        differ.join("\n")
    );
}

/// Every format's description, and those of values no format is, are the Metal platform's on
/// this host, field for field, at every value from 0 to 2047 and past 2^32 (2^32 + 80 among
/// them, which Metal would answer as BGRA8Unorm if it read the value's low 32 bits alone), as
/// the device answers them; a format is named as the runtime names it
#[test]
fn descriptions_are_the_metal_platforms_here() {
    use reims_vgpu_core::platform::device::GpuDevice as _;
    use reims_vgpu_metal::MetalPlatform;

    use crate::gpu::device::tests::dev;

    let d = dev();
    let bgra = d.pixel_format_info(80);
    assert_eq!(
        (bgra.name.as_deref(), bgra.flags, bgra.block_bytes),
        (Some("MTLPixelFormatBGRA8Unorm"), 0x703_019f, 4)
    );
    let ycbcr8 = d.pixel_format_info(500);
    // macOS 27.0.1 numbers cast classes anew.
    let cast_class = match crate::platform::metal_level("MTLPixelFormatGetInfoForDevice()") {
        metal2vulkan_runtime::MetalLevel::MacOS26_7 => 43,
        metal2vulkan_runtime::MetalLevel::MacOS27_0_1 => 46,
        level => panic!("no cast class measured at {level}"),
    };
    assert_eq!(
        (ycbcr8.name.as_deref(), ycbcr8.flags, ycbcr8.cast_class),
        (Some("MTLPixelFormatYCBCR8_420_2P"), 0x783_001d, cast_class)
    );
    let invalid = d.pixel_format_info(9999);
    assert_eq!(invalid.name.as_deref(), Some("MTLPixelFormatInvalid"));
    assert_eq!((invalid.flags, invalid.block_bytes), (0, 0));
    for pf in PixelFormat::ALL {
        let info = d.pixel_format_info(u64::from(pf.as_raw()));
        if info != invalid {
            assert_eq!(info.name.as_deref(), Some(pf.name()), "{pf:?}");
        }
    }
    let Some(metal) = MetalPlatform::system_default_device() else {
        return;
    };
    let values = (0..2048).chain([9999, u64::from(u32::MAX) + 70, (1 << 32) + 80]);
    for format in values {
        assert_eq!(
            d.pixel_format_info(format),
            metal.pixel_format_info(format),
            "format {format}"
        );
    }
}
