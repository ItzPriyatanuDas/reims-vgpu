//! libva's VP9 decoding interface (`va_dec_vp9.h`, libva 2.24.1), written
//! by hand as `super::super::ffi` is. The test at the end checks each
//! struct's size and offsets, each bit-field's position and the two
//! profiles' values against the installed header with a C compiler.

#![expect(
    non_upper_case_globals,
    reason = "libva's names, kept for grepping the headers"
)]

use super::super::ffi::{BitField, VA_PADDING_LOW, VA_PADDING_MEDIUM, VAProfile, VASurfaceID};

/// `VAProfileVP9Profile0`: 8 bits, 4:2:0.
pub(crate) const VAProfileVP9Profile0: VAProfile = 19;
/// `VAProfileVP9Profile2`: 10 and 12 bits, 4:2:0.
pub(crate) const VAProfileVP9Profile2: VAProfile = 21;

const fn bits(shift: u32, width: u32) -> BitField {
    BitField { shift, width }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct VADecPictureParameterBufferVP9 {
    pub(crate) frame_width: u16,
    pub(crate) frame_height: u16,
    pub(crate) reference_frames: [VASurfaceID; 8],
    /// `pic_fields.value`: see [`pic_fields`].
    pub(crate) pic_fields: u32,
    pub(crate) filter_level: u8,
    pub(crate) sharpness_level: u8,
    pub(crate) log2_tile_rows: u8,
    pub(crate) log2_tile_columns: u8,
    pub(crate) frame_header_length_in_bytes: u8,
    pub(crate) first_partition_size: u16,
    pub(crate) mb_segment_tree_probs: [u8; 7],
    pub(crate) segment_pred_probs: [u8; 3],
    pub(crate) profile: u8,
    pub(crate) bit_depth: u8,
    pub(crate) va_reserved: [u32; VA_PADDING_MEDIUM],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct VASegmentParameterVP9 {
    /// `segment_flags.value`: see [`segment_flags`].
    pub(crate) segment_flags: u16,
    pub(crate) filter_level: [[u8; 2]; 4],
    pub(crate) luma_ac_quant_scale: i16,
    pub(crate) luma_dc_quant_scale: i16,
    pub(crate) chroma_ac_quant_scale: i16,
    pub(crate) chroma_dc_quant_scale: i16,
    pub(crate) va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct VASliceParameterBufferVP9 {
    pub(crate) slice_data_size: u32,
    pub(crate) slice_data_offset: u32,
    pub(crate) slice_data_flag: u32,
    pub(crate) seg_param: [VASegmentParameterVP9; 8],
    pub(crate) va_reserved: [u32; VA_PADDING_LOW],
}

/// `VADecPictureParameterBufferVP9::pic_fields.bits`.
pub(crate) mod pic_fields {
    use super::{BitField, bits};
    pub(crate) const SUBSAMPLING_X: BitField = bits(0, 1);
    pub(crate) const SUBSAMPLING_Y: BitField = bits(1, 1);
    pub(crate) const FRAME_TYPE: BitField = bits(2, 1);
    pub(crate) const SHOW_FRAME: BitField = bits(3, 1);
    pub(crate) const ERROR_RESILIENT_MODE: BitField = bits(4, 1);
    pub(crate) const INTRA_ONLY: BitField = bits(5, 1);
    pub(crate) const ALLOW_HIGH_PRECISION_MV: BitField = bits(6, 1);
    pub(crate) const MCOMP_FILTER_TYPE: BitField = bits(7, 3);
    pub(crate) const FRAME_PARALLEL_DECODING_MODE: BitField = bits(10, 1);
    pub(crate) const RESET_FRAME_CONTEXT: BitField = bits(11, 2);
    pub(crate) const REFRESH_FRAME_CONTEXT: BitField = bits(13, 1);
    pub(crate) const FRAME_CONTEXT_IDX: BitField = bits(14, 2);
    pub(crate) const SEGMENTATION_ENABLED: BitField = bits(16, 1);
    pub(crate) const SEGMENTATION_TEMPORAL_UPDATE: BitField = bits(17, 1);
    pub(crate) const SEGMENTATION_UPDATE_MAP: BitField = bits(18, 1);
    pub(crate) const LAST_REF_FRAME: BitField = bits(19, 3);
    pub(crate) const LAST_REF_FRAME_SIGN_BIAS: BitField = bits(22, 1);
    pub(crate) const GOLDEN_REF_FRAME: BitField = bits(23, 3);
    pub(crate) const GOLDEN_REF_FRAME_SIGN_BIAS: BitField = bits(26, 1);
    pub(crate) const ALT_REF_FRAME: BitField = bits(27, 3);
    pub(crate) const ALT_REF_FRAME_SIGN_BIAS: BitField = bits(30, 1);
    pub(crate) const LOSSLESS_FLAG: BitField = bits(31, 1);
}

/// `VASegmentParameterVP9::segment_flags.fields`, a 16-bit union.
pub(crate) mod segment_flags {
    use super::{BitField, bits};
    pub(crate) const SEGMENT_REFERENCE_ENABLED: BitField = bits(0, 1);
    pub(crate) const SEGMENT_REFERENCE: BitField = bits(1, 2);
    pub(crate) const SEGMENT_REFERENCE_SKIPPED: BitField = bits(3, 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// What the C side prints, line for line.
    const LAYOUT_C: &str = include_str!("ffi_layout.c");

    macro_rules! layout {
        ($out:ident, $ty:ty, $c:literal, { $($field:ident),* $(,)? }) => {
            $out.push(format!("{} size {}", $c, size_of::<$ty>()));
            $( $out.push(format!("{}.{} {}", $c, stringify!($field), offset_of!($ty, $field))); )*
        };
    }

    fn rust_layout() -> Vec<String> {
        let mut out = Vec::new();
        layout!(out, VADecPictureParameterBufferVP9, "VADecPictureParameterBufferVP9", {
            frame_width, frame_height, reference_frames, pic_fields, filter_level,
            sharpness_level, log2_tile_rows, log2_tile_columns, frame_header_length_in_bytes,
            first_partition_size, mb_segment_tree_probs, segment_pred_probs, profile, bit_depth,
            va_reserved,
        });
        layout!(out, VASegmentParameterVP9, "VASegmentParameterVP9", {
            segment_flags, filter_level, luma_ac_quant_scale, luma_dc_quant_scale,
            chroma_ac_quant_scale, chroma_dc_quant_scale, va_reserved,
        });
        layout!(out, VASliceParameterBufferVP9, "VASliceParameterBufferVP9", {
            slice_data_size, slice_data_offset, slice_data_flag, seg_param, va_reserved,
        });
        use pic_fields as p;
        let pic = [
            ("subsampling_x", p::SUBSAMPLING_X),
            ("subsampling_y", p::SUBSAMPLING_Y),
            ("frame_type", p::FRAME_TYPE),
            ("show_frame", p::SHOW_FRAME),
            ("error_resilient_mode", p::ERROR_RESILIENT_MODE),
            ("intra_only", p::INTRA_ONLY),
            ("allow_high_precision_mv", p::ALLOW_HIGH_PRECISION_MV),
            ("mcomp_filter_type", p::MCOMP_FILTER_TYPE),
            (
                "frame_parallel_decoding_mode",
                p::FRAME_PARALLEL_DECODING_MODE,
            ),
            ("reset_frame_context", p::RESET_FRAME_CONTEXT),
            ("refresh_frame_context", p::REFRESH_FRAME_CONTEXT),
            ("frame_context_idx", p::FRAME_CONTEXT_IDX),
            ("segmentation_enabled", p::SEGMENTATION_ENABLED),
            (
                "segmentation_temporal_update",
                p::SEGMENTATION_TEMPORAL_UPDATE,
            ),
            ("segmentation_update_map", p::SEGMENTATION_UPDATE_MAP),
            ("last_ref_frame", p::LAST_REF_FRAME),
            ("last_ref_frame_sign_bias", p::LAST_REF_FRAME_SIGN_BIAS),
            ("golden_ref_frame", p::GOLDEN_REF_FRAME),
            ("golden_ref_frame_sign_bias", p::GOLDEN_REF_FRAME_SIGN_BIAS),
            ("alt_ref_frame", p::ALT_REF_FRAME),
            ("alt_ref_frame_sign_bias", p::ALT_REF_FRAME_SIGN_BIAS),
            ("lossless_flag", p::LOSSLESS_FLAG),
        ];
        for (name, field) in pic {
            out.push(format!("pic_fields.{name} {:#x}", field.put(u32::MAX)));
        }
        use segment_flags as s;
        let seg = [
            ("segment_reference_enabled", s::SEGMENT_REFERENCE_ENABLED),
            ("segment_reference", s::SEGMENT_REFERENCE),
            ("segment_reference_skipped", s::SEGMENT_REFERENCE_SKIPPED),
        ];
        for (name, field) in seg {
            out.push(format!("segment_flags.{name} {:#x}", field.put(u32::MAX)));
        }
        out.push(format!("VAProfileVP9Profile0 {VAProfileVP9Profile0}"));
        out.push(format!("VAProfileVP9Profile2 {VAProfileVP9Profile2}"));
        out
    }

    /// Compiles `ffi_layout.c` against the installed libva headers and
    /// compares its output with this module's layout. `CC` picks the
    /// compiler (`cc` by default).
    #[test]
    #[ignore = "needs libva's headers and a C compiler: run on the test host"]
    fn vp9_layouts_match_libva_headers() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("rvgv-va-vp9-layout-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let binary = dir.join("layout");
        let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
        let mut compile = Command::new(cc)
            .args(["-Wno-overflow", "-x", "c", "-", "-o"])
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
