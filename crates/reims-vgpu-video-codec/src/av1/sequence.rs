//! The sequence header OBU (5.5, 6.4): what every frame of a coded video
//! sequence shares.

use super::Error;
use super::bits::BitReader;

/// `SELECT_SCREEN_CONTENT_TOOLS` and `SELECT_INTEGER_MV` (3): the frame
/// header chooses.
pub(crate) const SELECT: u8 = 2;

/// `timing_info()` (5.5.3), as far as the frame header needs it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TimingInfo {
    /// `num_units_in_display_tick`.
    pub(crate) num_units_in_display_tick: u32,
    /// `time_scale`.
    pub(crate) time_scale: u32,
    /// `equal_picture_interval`.
    pub(crate) equal_picture_interval: bool,
}

/// `decoder_model_info()` (5.5.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DecoderModelInfo {
    /// `buffer_delay_length_minus_1`.
    pub(crate) buffer_delay_length_minus_1: u8,
    /// `buffer_removal_time_length_minus_1`.
    pub(crate) buffer_removal_time_length_minus_1: u8,
    /// `frame_presentation_time_length_minus_1`.
    pub(crate) frame_presentation_time_length_minus_1: u8,
}

/// One operating point (5.5.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OperatingPoint {
    /// `operating_point_idc`: the temporal layers in its low 8 bits, the
    /// spatial layers above.
    pub idc: u16,
    /// `seq_level_idx`.
    pub seq_level_idx: u8,
    /// `seq_tier`.
    pub seq_tier: bool,
    /// `decoder_model_present_for_this_op`.
    pub(crate) decoder_model_present: bool,
}

/// `color_config()` (5.5.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColorConfig {
    /// `BitDepth`: 8, 10 or 12.
    pub bit_depth: u8,
    /// `mono_chrome`.
    pub mono_chrome: bool,
    /// `color_primaries`.
    pub color_primaries: u8,
    /// `transfer_characteristics`.
    pub transfer_characteristics: u8,
    /// `matrix_coefficients`.
    pub matrix_coefficients: u8,
    /// `color_range`: full range when set.
    pub color_range: bool,
    /// `subsampling_x`.
    pub subsampling_x: bool,
    /// `subsampling_y`.
    pub subsampling_y: bool,
    /// `chroma_sample_position`.
    pub chroma_sample_position: u8,
    /// `separate_uv_delta_q`.
    pub(crate) separate_uv_delta_q: bool,
}

impl ColorConfig {
    /// `NumPlanes`.
    pub(crate) fn num_planes(&self) -> u8 {
        if self.mono_chrome { 1 } else { 3 }
    }
}

/// A syntax element coded less one, as `u32`, plus one: the bits of a
/// field, or a length, named `name`.
fn plus_one(minus_1: u8, name: &'static str) -> Result<u32, Error> {
    u32::from(minus_1)
        .checked_add(1)
        .ok_or(Error::Overflow(name))
}

/// A parsed sequence header.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SequenceHeader {
    /// `seq_profile`.
    pub seq_profile: u8,
    /// `still_picture`.
    pub still_picture: bool,
    /// `reduced_still_picture_header`.
    pub reduced_still_picture_header: bool,
    /// `timing_info()`, when `timing_info_present_flag`.
    pub(crate) timing_info: Option<TimingInfo>,
    /// `decoder_model_info()`, when `decoder_model_info_present_flag`.
    pub(crate) decoder_model_info: Option<DecoderModelInfo>,
    /// The operating points, at least one.
    pub operating_points: Vec<OperatingPoint>,
    /// `frame_width_bits_minus_1`.
    pub(crate) frame_width_bits_minus_1: u8,
    /// `frame_height_bits_minus_1`.
    pub(crate) frame_height_bits_minus_1: u8,
    /// `max_frame_width_minus_1`.
    pub max_frame_width_minus_1: u32,
    /// `max_frame_height_minus_1`.
    pub max_frame_height_minus_1: u32,
    /// `max_frame_width_minus_1 + 1`, as the parse derives it.
    pub(crate) max_frame_width: u32,
    /// `max_frame_height_minus_1 + 1`, as the parse derives it.
    pub(crate) max_frame_height: u32,
    /// `frame_id_numbers_present_flag`.
    pub(crate) frame_id_numbers_present_flag: bool,
    /// `delta_frame_id_length_minus_2`.
    pub(crate) delta_frame_id_length_minus_2: u8,
    /// `additional_frame_id_length_minus_1`.
    pub(crate) additional_frame_id_length_minus_1: u8,
    /// `diffLen`, the bits of a frame id's delta, as the parse derives it
    /// where frame ids are coded.
    pub(crate) diff_len: u32,
    /// `idLen`, as the parse derives it where frame ids are coded.
    pub(crate) id_len: u32,
    /// `use_128x128_superblock`.
    pub use_128x128_superblock: bool,
    /// `enable_filter_intra`.
    pub enable_filter_intra: bool,
    /// `enable_intra_edge_filter`.
    pub enable_intra_edge_filter: bool,
    /// `enable_interintra_compound`.
    pub enable_interintra_compound: bool,
    /// `enable_masked_compound`.
    pub enable_masked_compound: bool,
    /// `enable_warped_motion`.
    pub(crate) enable_warped_motion: bool,
    /// `enable_dual_filter`.
    pub enable_dual_filter: bool,
    /// `enable_order_hint`.
    pub enable_order_hint: bool,
    /// `enable_jnt_comp`.
    pub enable_jnt_comp: bool,
    /// `enable_ref_frame_mvs`.
    pub(crate) enable_ref_frame_mvs: bool,
    /// `seq_force_screen_content_tools`: 0, 1 or [`SELECT`].
    pub(crate) seq_force_screen_content_tools: u8,
    /// `seq_force_integer_mv`: 0, 1 or [`SELECT`].
    pub(crate) seq_force_integer_mv: u8,
    /// `OrderHintBits`: 0 without order hints.
    pub order_hint_bits: u8,
    /// `enable_superres`.
    pub enable_superres: bool,
    /// `enable_cdef`.
    pub enable_cdef: bool,
    /// `enable_restoration`.
    pub enable_restoration: bool,
    /// `color_config()`.
    pub color: ColorConfig,
    /// `film_grain_params_present`.
    pub film_grain_params_present: bool,
}

impl SequenceHeader {
    /// Parses a sequence header OBU's payload.
    pub fn parse(payload: &[u8]) -> Result<Self, Error> {
        let mut r = BitReader::new(payload);
        let mut s = Self {
            seq_profile: r.u8(3)?,
            still_picture: r.bit()?,
            reduced_still_picture_header: r.bit()?,
            ..Self::default()
        };
        if s.seq_profile > 2 {
            return Err(Error::OutOfRange("seq_profile"));
        }
        if s.reduced_still_picture_header {
            s.operating_points.push(OperatingPoint {
                seq_level_idx: r.u8(5)?,
                ..OperatingPoint::default()
            });
        } else {
            if r.bit()? {
                let timing = TimingInfo {
                    num_units_in_display_tick: r.bits(32)?,
                    time_scale: r.bits(32)?,
                    equal_picture_interval: r.bit()?,
                };
                if timing.equal_picture_interval {
                    // num_ticks_per_picture_minus_1
                    r.uvlc()?;
                }
                s.timing_info = Some(timing);
                if r.bit()? {
                    let buffer_delay_length_minus_1 = r.u8(5)?;
                    // num_units_in_decoding_tick
                    r.bits(32)?;
                    s.decoder_model_info = Some(DecoderModelInfo {
                        buffer_delay_length_minus_1,
                        buffer_removal_time_length_minus_1: r.u8(5)?,
                        frame_presentation_time_length_minus_1: r.u8(5)?,
                    });
                }
            }
            let initial_display_delay_present = r.bit()?;
            // operating_points_cnt_minus_1 + 1.
            for _ in 0..=r.u8(5)? {
                let mut point = OperatingPoint {
                    idc: r.u16(12)?,
                    seq_level_idx: r.u8(5)?,
                    ..OperatingPoint::default()
                };
                if point.seq_level_idx > 7 {
                    point.seq_tier = r.bit()?;
                }
                if let Some(model) = s.decoder_model_info {
                    point.decoder_model_present = r.bit()?;
                    if point.decoder_model_present {
                        // operating_parameters_info(): the two delays and
                        // low_delay_mode_flag.
                        // decoder_buffer_delay and encoder_buffer_delay,
                        // of n = buffer_delay_length_minus_1 + 1 bits each,
                        // and low_delay_mode_flag.
                        let n = usize::from(model.buffer_delay_length_minus_1)
                            .checked_add(1)
                            .ok_or(Error::Overflow("n"))?;
                        r.skip(n)?;
                        r.skip(n)?;
                        r.skip(1)?;
                    }
                }
                if initial_display_delay_present && r.bit()? {
                    // initial_display_delay_minus_1
                    r.skip(4)?;
                }
                s.operating_points.push(point);
            }
        }
        s.frame_width_bits_minus_1 = r.u8(4)?;
        s.frame_height_bits_minus_1 = r.u8(4)?;
        s.max_frame_width_minus_1 = r.bits(plus_one(
            s.frame_width_bits_minus_1,
            "frame_width_bits_minus_1 + 1",
        )?)?;
        s.max_frame_height_minus_1 = r.bits(plus_one(
            s.frame_height_bits_minus_1,
            "frame_height_bits_minus_1 + 1",
        )?)?;
        s.max_frame_width = (s.max_frame_width_minus_1.checked_add(1))
            .ok_or(Error::Overflow("max_frame_width_minus_1 + 1"))?;
        s.max_frame_height = (s.max_frame_height_minus_1.checked_add(1))
            .ok_or(Error::Overflow("max_frame_height_minus_1 + 1"))?;
        if !s.reduced_still_picture_header {
            s.frame_id_numbers_present_flag = r.bit()?;
        }
        if s.frame_id_numbers_present_flag {
            s.delta_frame_id_length_minus_2 = r.u8(4)?;
            s.additional_frame_id_length_minus_1 = r.u8(3)?;
            // diffLen, delta_frame_id_length_minus_2 + 2, and idLen,
            // additional_frame_id_length_minus_1 + diffLen + 1.
            s.diff_len = (u32::from(s.delta_frame_id_length_minus_2).checked_add(2))
                .ok_or(Error::Overflow("diffLen"))?;
            s.id_len = plus_one(s.additional_frame_id_length_minus_1, "idLen")?
                .checked_add(s.diff_len)
                .ok_or(Error::Overflow("idLen"))?;
        }
        s.use_128x128_superblock = r.bit()?;
        s.enable_filter_intra = r.bit()?;
        s.enable_intra_edge_filter = r.bit()?;
        if s.reduced_still_picture_header {
            s.seq_force_screen_content_tools = SELECT;
            s.seq_force_integer_mv = SELECT;
        } else {
            s.enable_interintra_compound = r.bit()?;
            s.enable_masked_compound = r.bit()?;
            s.enable_warped_motion = r.bit()?;
            s.enable_dual_filter = r.bit()?;
            s.enable_order_hint = r.bit()?;
            if s.enable_order_hint {
                s.enable_jnt_comp = r.bit()?;
                s.enable_ref_frame_mvs = r.bit()?;
            }
            s.seq_force_screen_content_tools = if r.bit()? { SELECT } else { r.u8(1)? };
            s.seq_force_integer_mv = if s.seq_force_screen_content_tools > 0 {
                if r.bit()? { SELECT } else { r.u8(1)? }
            } else {
                SELECT
            };
            if s.enable_order_hint {
                s.order_hint_bits =
                    (r.u8(3)?.checked_add(1)).ok_or(Error::Overflow("OrderHintBits"))?;
            }
        }
        s.enable_superres = r.bit()?;
        s.enable_cdef = r.bit()?;
        s.enable_restoration = r.bit()?;
        s.color = color_config(&mut r, s.seq_profile)?;
        s.film_grain_params_present = r.bit()?;
        Ok(s)
    }

    /// The operating point decoded: the first, as 7.1's
    /// `choose_operating_point()` does by default.
    pub fn operating_point(&self) -> OperatingPoint {
        self.operating_points.first().copied().unwrap_or_default()
    }

    /// The largest frame the sequence codes: `max_frame_width_minus_1 + 1`
    /// by `max_frame_height_minus_1 + 1`.
    pub fn max_frame_size(&self) -> (u32, u32) {
        (self.max_frame_width, self.max_frame_height)
    }

    /// `idLen`: the bits of a frame id, when the stream codes them.
    pub(crate) fn frame_id_length(&self) -> u32 {
        self.id_len
    }

    /// Whether the sequence's frames read `temporal_point_info()`.
    pub(crate) fn has_frame_presentation_time(&self) -> bool {
        self.decoder_model_info.is_some()
            && !self.timing_info.is_some_and(|t| t.equal_picture_interval)
    }
}

/// `color_config()` (5.5.2).
fn color_config(r: &mut BitReader<'_>, seq_profile: u8) -> Result<ColorConfig, Error> {
    let high_bitdepth = r.bit()?;
    let bit_depth = if seq_profile == 2 && high_bitdepth {
        if r.bit()? { 12 } else { 10 }
    } else if high_bitdepth {
        10
    } else {
        8
    };
    let mono_chrome = if seq_profile == 1 { false } else { r.bit()? };
    let mut c = ColorConfig {
        bit_depth,
        mono_chrome,
        // CP_UNSPECIFIED, TC_UNSPECIFIED, MC_UNSPECIFIED.
        color_primaries: 2,
        transfer_characteristics: 2,
        matrix_coefficients: 2,
        ..ColorConfig::default()
    };
    if r.bit()? {
        c.color_primaries = r.u8(8)?;
        c.transfer_characteristics = r.u8(8)?;
        c.matrix_coefficients = r.u8(8)?;
    }
    if mono_chrome {
        c.color_range = r.bit()?;
        c.subsampling_x = true;
        c.subsampling_y = true;
        return Ok(c);
    }
    // CP_BT_709, TC_SRGB, MC_IDENTITY: 4:4:4 RGB, full range.
    if c.color_primaries == 1 && c.transfer_characteristics == 13 && c.matrix_coefficients == 0 {
        c.color_range = true;
    } else {
        c.color_range = r.bit()?;
        match seq_profile {
            0 => (c.subsampling_x, c.subsampling_y) = (true, true),
            1 => {}
            _ if bit_depth == 12 => {
                c.subsampling_x = r.bit()?;
                c.subsampling_y = c.subsampling_x && r.bit()?;
            }
            _ => c.subsampling_x = true,
        }
        if c.subsampling_x && c.subsampling_y {
            c.chroma_sample_position = r.u8(2)?;
        }
    }
    c.separate_uv_delta_q = r.bit()?;
    Ok(c)
}
