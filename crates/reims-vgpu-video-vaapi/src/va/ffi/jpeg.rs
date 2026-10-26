//! libva's baseline JPEG decode interface, hand-written from libva
//! 2.24.1's headers (`va_dec_jpeg.h`, and `va.h` for the Huffman table
//! buffer and the formats; VA-API 1.24.0). The test at the end checks each
//! struct's size and offsets, and each constant, against those headers
//! with a C compiler, as the parent module's test does for the other
//! decoders.
//!
//! Names keep libva's spelling; struct fields are snake-cased.

#![allow(
    dead_code,
    reason = "the binding declares what the headers have beside what is called"
)]
#![expect(
    non_upper_case_globals,
    reason = "libva's names, kept for grepping the headers"
)]

use core::ffi::c_uint;

// What video processing's binding has too.
pub(crate) use super::super::vpp_ffi::{
    VA_FOURCC_YUY2, VA_ROTATION_NONE, VA_RT_FORMAT_YUV422, VARectangle,
};
use super::{VA_PADDING_LOW, VA_PADDING_MEDIUM, VABufferType, VAConfigAttribType, VAProfile};

pub(crate) const VAProfileJPEGBaseline: VAProfile = 12;
pub(crate) const VAHuffmanTableBufferType: VABufferType = 12;
pub(crate) const VAConfigAttribDecJPEG: VAConfigAttribType = 7;

pub(crate) const VA_RT_FORMAT_YUV444: c_uint = 0x0000_0004;
pub(crate) const VA_RT_FORMAT_YUV411: c_uint = 0x0000_0008;
pub(crate) const VA_RT_FORMAT_YUV400: c_uint = 0x0000_0010;

pub(crate) const VA_FOURCC_Y800: u32 = 0x3030_3859;
pub(crate) const VA_FOURCC_411P: u32 = 0x5031_3134;
pub(crate) const VA_FOURCC_422H: u32 = 0x4832_3234;
pub(crate) const VA_FOURCC_422V: u32 = 0x5632_3234;
pub(crate) const VA_FOURCC_444P: u32 = 0x5034_3434;

/// One frame component of `VAPictureParameterBufferJPEGBaseline`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VAPictureParameterBufferJPEGBaselineComponent {
    pub(crate) component_id: u8,
    pub(crate) h_sampling_factor: u8,
    pub(crate) v_sampling_factor: u8,
    pub(crate) quantiser_table_selector: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct VAPictureParameterBufferJPEGBaseline {
    pub(crate) picture_width: u16,
    pub(crate) picture_height: u16,
    pub(crate) components: [VAPictureParameterBufferJPEGBaselineComponent; 255],
    pub(crate) num_components: u8,
    pub(crate) color_space: u8,
    pub(crate) rotation: u32,
    pub(crate) crop_rectangle: VARectangle,
    pub(crate) va_reserved: [u32; VA_PADDING_MEDIUM - 3],
}

impl Default for VAPictureParameterBufferJPEGBaseline {
    fn default() -> Self {
        Self {
            picture_width: 0,
            picture_height: 0,
            components: [VAPictureParameterBufferJPEGBaselineComponent::default(); 255],
            num_components: 0,
            color_space: 0,
            rotation: VA_ROTATION_NONE,
            crop_rectangle: VARectangle::default(),
            va_reserved: [0; VA_PADDING_MEDIUM - 3],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct VAIQMatrixBufferJPEGBaseline {
    pub(crate) load_quantiser_table: [u8; 4],
    /// Each in zig-zag order.
    pub(crate) quantiser_table: [[u8; 64]; 4],
    pub(crate) va_reserved: [u32; VA_PADDING_LOW],
}

impl Default for VAIQMatrixBufferJPEGBaseline {
    fn default() -> Self {
        Self {
            load_quantiser_table: [0; 4],
            quantiser_table: [[0; 64]; 4],
            va_reserved: [0; VA_PADDING_LOW],
        }
    }
}

/// One table of `VAHuffmanTableBufferJPEGBaseline`: a DC and an AC table
/// of one index.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct VAHuffmanTableBufferJPEGBaselineTable {
    pub(crate) num_dc_codes: [u8; 16],
    pub(crate) dc_values: [u8; 12],
    pub(crate) num_ac_codes: [u8; 16],
    pub(crate) ac_values: [u8; 162],
    pub(crate) pad: [u8; 2],
}

impl Default for VAHuffmanTableBufferJPEGBaselineTable {
    fn default() -> Self {
        Self {
            num_dc_codes: [0; 16],
            dc_values: [0; 12],
            num_ac_codes: [0; 16],
            ac_values: [0; 162],
            pad: [0; 2],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct VAHuffmanTableBufferJPEGBaseline {
    pub(crate) load_huffman_table: [u8; 2],
    pub(crate) huffman_table: [VAHuffmanTableBufferJPEGBaselineTable; 2],
    pub(crate) va_reserved: [u32; VA_PADDING_LOW],
}

/// One scan component of `VASliceParameterBufferJPEGBaseline`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VASliceParameterBufferJPEGBaselineComponent {
    pub(crate) component_selector: u8,
    pub(crate) dc_table_selector: u8,
    pub(crate) ac_table_selector: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct VASliceParameterBufferJPEGBaseline {
    pub(crate) slice_data_size: u32,
    pub(crate) slice_data_offset: u32,
    pub(crate) slice_data_flag: u32,
    pub(crate) slice_horizontal_position: u32,
    pub(crate) slice_vertical_position: u32,
    pub(crate) components: [VASliceParameterBufferJPEGBaselineComponent; 4],
    pub(crate) num_components: u8,
    pub(crate) restart_interval: u16,
    pub(crate) num_mcus: u32,
    pub(crate) va_reserved: [u32; VA_PADDING_LOW],
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// What the C side prints, line for line.
    const LAYOUT_C: &str = include_str!("jpeg_layout.c");

    macro_rules! layout {
        ($out:ident, $ty:ty, $c:literal, { $($field:ident => $cfield:literal),* $(,)? }) => {
            $out.push(format!("{} size {}", $c, size_of::<$ty>()));
            $( $out.push(format!("{}.{} {}", $c, $cfield, offset_of!($ty, $field))); )*
        };
    }

    fn rust_layout() -> Vec<String> {
        type Picture = VAPictureParameterBufferJPEGBaseline;
        type PictureComponent = VAPictureParameterBufferJPEGBaselineComponent;
        type Huffman = VAHuffmanTableBufferJPEGBaseline;
        type HuffmanTable = VAHuffmanTableBufferJPEGBaselineTable;
        type Slice = VASliceParameterBufferJPEGBaseline;
        type SliceComponent = VASliceParameterBufferJPEGBaselineComponent;
        let mut out = Vec::new();
        layout!(out, Picture, "VAPictureParameterBufferJPEGBaseline", {
            picture_width => "picture_width", picture_height => "picture_height",
            components => "components", num_components => "num_components",
            color_space => "color_space", rotation => "rotation",
            crop_rectangle => "crop_rectangle", va_reserved => "va_reserved",
        });
        layout!(out, PictureComponent, "picture.components", {
            component_id => "component_id", h_sampling_factor => "h_sampling_factor",
            v_sampling_factor => "v_sampling_factor",
            quantiser_table_selector => "quantiser_table_selector",
        });
        layout!(out, VAIQMatrixBufferJPEGBaseline, "VAIQMatrixBufferJPEGBaseline", {
            load_quantiser_table => "load_quantiser_table",
            quantiser_table => "quantiser_table", va_reserved => "va_reserved",
        });
        layout!(out, Huffman, "VAHuffmanTableBufferJPEGBaseline", {
            load_huffman_table => "load_huffman_table", huffman_table => "huffman_table",
            va_reserved => "va_reserved",
        });
        layout!(out, HuffmanTable, "huffman.huffman_table", {
            num_dc_codes => "num_dc_codes", dc_values => "dc_values",
            num_ac_codes => "num_ac_codes", ac_values => "ac_values", pad => "pad",
        });
        layout!(out, Slice, "VASliceParameterBufferJPEGBaseline", {
            slice_data_size => "slice_data_size", slice_data_offset => "slice_data_offset",
            slice_data_flag => "slice_data_flag",
            slice_horizontal_position => "slice_horizontal_position",
            slice_vertical_position => "slice_vertical_position", components => "components",
            num_components => "num_components", restart_interval => "restart_interval",
            num_mcus => "num_mcus", va_reserved => "va_reserved",
        });
        layout!(out, SliceComponent, "slice.components", {
            component_selector => "component_selector", dc_table_selector => "dc_table_selector",
            ac_table_selector => "ac_table_selector",
        });
        let constants: [(&str, i64); 15] = [
            ("VAProfileJPEGBaseline", VAProfileJPEGBaseline.into()),
            ("VAHuffmanTableBufferType", VAHuffmanTableBufferType.into()),
            ("VAConfigAttribDecJPEG", VAConfigAttribDecJPEG.into()),
            ("VA_RT_FORMAT_YUV422", VA_RT_FORMAT_YUV422.into()),
            ("VA_RT_FORMAT_YUV444", VA_RT_FORMAT_YUV444.into()),
            ("VA_RT_FORMAT_YUV411", VA_RT_FORMAT_YUV411.into()),
            ("VA_RT_FORMAT_YUV400", VA_RT_FORMAT_YUV400.into()),
            ("VA_FOURCC_YUY2", VA_FOURCC_YUY2.into()),
            ("VA_FOURCC_Y800", VA_FOURCC_Y800.into()),
            ("VA_FOURCC_411P", VA_FOURCC_411P.into()),
            ("VA_FOURCC_422H", VA_FOURCC_422H.into()),
            ("VA_FOURCC_422V", VA_FOURCC_422V.into()),
            ("VA_FOURCC_444P", VA_FOURCC_444P.into()),
            ("VA_ROTATION_NONE", VA_ROTATION_NONE.into()),
            (
                "VA_SLICE_DATA_FLAG_ALL",
                super::super::VA_SLICE_DATA_FLAG_ALL.into(),
            ),
        ];
        for (name, value) in constants {
            out.push(format!("{name} {value}"));
        }
        out
    }

    /// Compiles `jpeg_layout.c` against the installed libva headers and
    /// compares its output with this module's layout. `CC` picks the
    /// compiler (`cc` by default).
    #[test]
    #[ignore = "needs libva's headers and a C compiler: run on the test host"]
    fn jpeg_layouts_match_libva_headers() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("rvgv-va-jpeg-layout-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let binary = dir.join("layout");
        let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
        let mut compile = Command::new(cc)
            .args(["-x", "c", "-", "-o"])
            .arg(&binary)
            .stdin(Stdio::piped())
            .spawn()?;
        compile
            .stdin
            .take()
            .ok_or("no stdin")?
            .write_all(LAYOUT_C.as_bytes())?;
        if !compile.wait()?.success() {
            return Err("the C compiler failed".into());
        }
        let output = Command::new(&binary).output()?;
        std::fs::remove_dir_all(&dir)?;
        let c_lines: Vec<String> = String::from_utf8(output.stdout)?
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(rust_layout(), c_lines);
        Ok(())
    }
}
