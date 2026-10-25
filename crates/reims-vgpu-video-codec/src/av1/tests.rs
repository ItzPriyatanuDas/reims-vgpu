//! The decoding process above the tiles, on streams written here from the
//! specification's syntax and an accelerator that records what it is
//! asked to decode. The tile data is a placeholder: nothing here decodes
//! pixels. What encoders rarely write (inheritance from a primary
//! reference, a hidden key frame shown later, film grain loaded from a
//! reference, short reference signalling) is covered here; the VA-API
//! crate's `tools/av1-check.sh` covers real streams on hardware.

use super::frame::{FrameHeader, FrameType, NUM_REF_FRAMES, PRIMARY_REF_NONE, ReferenceState};
use super::sequence::SequenceHeader;
use super::*;

/// Writes syntax elements, most significant bit first.
#[derive(Default)]
struct Bits {
    bits: Vec<bool>,
}

impl Bits {
    fn f(&mut self, n: u32, value: u32) -> &mut Self {
        for i in (0..n).rev() {
            self.bits.push((value >> i) & 1 == 1);
        }
        self
    }

    fn flag(&mut self, value: bool) -> &mut Self {
        self.bits.push(value);
        self
    }

    fn su(&mut self, n: u32, value: i32) -> &mut Self {
        self.f(n, value.cast_unsigned() & ((1 << n) - 1))
    }

    /// Zero bits to the next byte, then the bytes.
    fn aligned(&mut self) -> Vec<u8> {
        while !self.bits.len().is_multiple_of(8) {
            self.bits.push(false);
        }
        self.bits
            .chunks(8)
            .map(|byte| byte.iter().fold(0u8, |v, &b| (v << 1) | u8::from(b)))
            .collect()
    }

    /// `trailing_bits()`: a one, then zero bits to the next byte.
    fn trailing(&mut self) -> Vec<u8> {
        self.bits.push(true);
        self.aligned()
    }
}

/// An OBU with its size field.
fn obu(kind: u8, payload: &[u8]) -> Vec<u8> {
    obu_with_header(&[(kind << 3) | 0x2], payload)
}

/// An OBU with an extension header naming its layers.
fn obu_in_layer(kind: u8, temporal_id: u8, spatial_id: u8, payload: &[u8]) -> Vec<u8> {
    obu_with_header(
        &[
            (kind << 3) | 0x4 | 0x2,
            (temporal_id << 5) | (spatial_id << 3),
        ],
        payload,
    )
}

fn obu_with_header(header: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = header.to_vec();
    let mut size = payload.len();
    loop {
        let byte = (size & 0x7f) as u8;
        size >>= 7;
        if size == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
    out.extend_from_slice(payload);
    out
}

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;
const OBU_FRAME_HEADER: u8 = 3;
const OBU_TILE_GROUP: u8 = 4;
const OBU_FRAME: u8 = 6;
const OBU_TILE_LIST: u8 = 8;

/// What the test sequence has.
#[derive(Clone, Copy)]
struct Sequence {
    width: u32,
    height: u32,
    film_grain: bool,
    bit_depth_10: bool,
    /// Operating point 0's `operating_point_idc`.
    operating_point_idc: u16,
}

impl Default for Sequence {
    fn default() -> Self {
        Self {
            width: 64,
            height: 48,
            film_grain: false,
            bit_depth_10: false,
            operating_point_idc: 0,
        }
    }
}

/// Order hints of 7 bits, no superres, CDEF, restoration or warped
/// motion, screen content tools left to each frame.
fn sequence_header(s: Sequence) -> Vec<u8> {
    let mut b = Bits::default();
    b.f(3, 0) // seq_profile
        .flag(false) // still_picture
        .flag(false) // reduced_still_picture_header
        .flag(false) // timing_info_present_flag
        .flag(false) // initial_display_delay_present_flag
        .f(5, 0) // operating_points_cnt_minus_1
        .f(12, u32::from(s.operating_point_idc)) // operating_point_idc[0]
        .f(5, 8) // seq_level_idx[0]: 4.0
        .flag(false) // seq_tier[0]
        .f(4, 15)
        .f(4, 15)
        .f(16, s.width - 1)
        .f(16, s.height - 1)
        .flag(false) // frame_id_numbers_present_flag
        .flag(false) // use_128x128_superblock
        .flag(true) // enable_filter_intra
        .flag(true) // enable_intra_edge_filter
        .flag(true) // enable_interintra_compound
        .flag(true) // enable_masked_compound
        .flag(false) // enable_warped_motion
        .flag(true) // enable_dual_filter
        .flag(true) // enable_order_hint
        .flag(true) // enable_jnt_comp
        .flag(true) // enable_ref_frame_mvs
        .flag(true) // seq_choose_screen_content_tools
        .flag(true) // seq_choose_integer_mv
        .f(3, 6) // order_hint_bits_minus_1
        .flag(false) // enable_superres
        .flag(false) // enable_cdef
        .flag(false) // enable_restoration
        .flag(s.bit_depth_10) // high_bitdepth
        .flag(false) // mono_chrome
        .flag(false) // color_description_present_flag
        .flag(false) // color_range
        .f(2, 0) // chroma_sample_position
        .flag(false) // separate_uv_delta_q
        .flag(s.film_grain);
    obu(OBU_SEQUENCE_HEADER, &b.trailing())
}

/// Film grain as a frame codes it.
#[derive(Clone, Copy)]
enum Grain {
    /// `apply_grain` 0.
    Off,
    /// New parameters: one luma point, this seed.
    New(u16),
    /// `update_grain` 0: the parameters of this slot, this seed.
    FromSlot(u8, u16),
    /// New parameters with these point values for Y, Cb and Cr.
    Points(&'static [u8], &'static [u8], &'static [u8]),
}

/// Segmentation as a frame codes it.
#[derive(Clone, Copy)]
enum Segments {
    Off,
    /// Enabled, with segment 1's alternative quantizer.
    AltQ(i32),
    /// Enabled, the features kept (`segmentation_update_data` 0).
    Keep,
}

/// One frame header.
#[derive(Clone, Copy)]
struct Frame {
    frame_type: FrameType,
    show_frame: bool,
    showable_frame: bool,
    error_resilient: bool,
    order_hint: u32,
    primary_ref_frame: u8,
    refresh: u8,
    refs: [u8; 7],
    short_signaling: Option<(u8, u8)>,
    base_q_idx: u32,
    segments: Segments,
    /// `loop_filter_ref_deltas` to code, by reference frame.
    ref_deltas: Option<[Option<i32>; 8]>,
    grain: Grain,
    /// Translation global motion for LAST_FRAME, in coded units.
    gm_last: Option<(i32, i32)>,
    /// A size of its own (`frame_size_override_flag`), found from no
    /// reference.
    size: Option<(u32, u32)>,
}

impl Frame {
    fn key() -> Self {
        Self {
            frame_type: FrameType::Key,
            show_frame: true,
            showable_frame: false,
            error_resilient: true,
            order_hint: 0,
            primary_ref_frame: PRIMARY_REF_NONE,
            refresh: 0xff,
            refs: [0; 7],
            short_signaling: None,
            base_q_idx: 100,
            segments: Segments::Off,
            ref_deltas: None,
            grain: Grain::Off,
            gm_last: None,
            size: None,
        }
    }

    fn inter(order_hint: u32, refresh: u8, refs: [u8; 7]) -> Self {
        Self {
            frame_type: FrameType::Inter,
            showable_frame: true,
            error_resilient: false,
            order_hint,
            refresh,
            refs,
            primary_ref_frame: 0,
            ..Self::key()
        }
    }
}

/// `uncompressed_header()` for [`Frame`], with the test sequence's
/// choices, and nothing after it.
fn uncompressed_header(b: &mut Bits, f: &Frame, s: Sequence) {
    let intra = f.frame_type.is_intra();
    b.flag(false) // show_existing_frame
        .f(2, f.frame_type as u32)
        .flag(f.show_frame);
    if !f.show_frame {
        b.flag(f.showable_frame);
    }
    let shown_key = f.frame_type == FrameType::Key && f.show_frame;
    if f.frame_type != FrameType::Switch && !shown_key {
        b.flag(f.error_resilient);
    }
    b.flag(false) // disable_cdf_update
        .flag(false); // allow_screen_content_tools
    if f.frame_type != FrameType::Switch {
        b.flag(f.size.is_some()); // frame_size_override_flag
    }
    // frame_size(): the size, with no superres in the sequence. A switch
    // frame's frame_size_override_flag is 1 (5.9.2), so it codes its size,
    // the sequence's where the test gives none.
    let frame_size = |b: &mut Bits| {
        let size = match f.size {
            None if f.frame_type == FrameType::Switch => Some((s.width, s.height)),
            size => size,
        };
        if let Some((width, height)) = size {
            b.f(16, width - 1).f(16, height - 1);
        }
    };
    b.f(7, f.order_hint);
    if !intra && !f.error_resilient {
        b.f(3, u32::from(f.primary_ref_frame));
    }
    if !(f.frame_type == FrameType::Switch || shown_key) {
        b.f(8, u32::from(f.refresh));
    }
    if (!intra || f.refresh != 0xff) && f.error_resilient {
        // ref_order_hint[]: none of the tests changes them.
        for _ in 0..8 {
            b.f(7, 0);
        }
    }
    if intra {
        frame_size(b);
        b.flag(false); // render_and_frame_size_different
    } else {
        b.flag(f.short_signaling.is_some());
        if let Some((last, gold)) = f.short_signaling {
            b.f(3, u32::from(last)).f(3, u32::from(gold));
        } else {
            for r in f.refs {
                b.f(3, u32::from(r));
            }
        }
        if f.size.is_some() && !f.error_resilient {
            // frame_size_with_refs(): found_ref 0 for each.
            for _ in 0..7 {
                b.flag(false);
            }
        }
        frame_size(b);
        b.flag(false) // render_and_frame_size_different
            .flag(true) // allow_high_precision_mv
            .flag(true) // is_filter_switchable
            .flag(false); // is_motion_mode_switchable
        if !f.error_resilient {
            b.flag(false); // use_ref_frame_mvs
        }
    }
    b.flag(false); // disable_frame_end_update_cdf
    // tile_info(): uniform spacing. A frame of one superblock has one
    // tile and codes only the flag; a larger one takes as many tiles as
    // it may, then context_update_tile_id 0 and tile_size_bytes_minus_1 0
    // (one-byte sizes).
    b.flag(true);
    // tile_log2(block, target) (5.9.15).
    let log2 = |block: u32, target: u32| (0u32..).find(|&k| (block << k) >= target).unwrap_or(0);
    let (width, height) = f.size.unwrap_or((s.width, s.height));
    let (sb_cols, sb_rows) = (width.div_ceil(64), height.div_ceil(64));
    let (cols_log2, rows_log2) = (log2(1, sb_cols.min(64)), log2(1, sb_rows.min(64)));
    let min_log2_tiles = log2(2304, sb_cols * sb_rows);
    for _ in 0..cols_log2 {
        b.flag(true);
    }
    for _ in min_log2_tiles.saturating_sub(cols_log2)..rows_log2 {
        b.flag(true);
    }
    if cols_log2 + rows_log2 > 0 {
        b.f(cols_log2 + rows_log2, 0).f(2, 0);
    }
    // quantization_params()
    b.f(8, f.base_q_idx)
        .flag(false) // DeltaQYDc
        .flag(false) // DeltaQUDc
        .flag(false) // DeltaQUAc
        .flag(false); // using_qmatrix
    // segmentation_params()
    let primary_none = intra || f.error_resilient || f.primary_ref_frame == PRIMARY_REF_NONE;
    match f.segments {
        Segments::Off => {
            b.flag(false);
        }
        Segments::AltQ(q) => {
            b.flag(true);
            if !primary_none {
                b.flag(false).flag(true); // update_map 0, update_data 1
            }
            for segment in 0..8 {
                for feature in 0..8 {
                    let on = segment == 1 && feature == 0;
                    b.flag(on);
                    if on {
                        b.su(9, q);
                    }
                }
            }
        }
        Segments::Keep => {
            assert!(!primary_none);
            b.flag(true).flag(false).flag(false); // update_map 0, update_data 0
        }
    }
    if f.base_q_idx > 0 {
        b.flag(false); // delta_q_present
    }
    // loop_filter_params()
    b.f(6, 10).f(6, 10).f(6, 5).f(6, 5).f(3, 2);
    match f.ref_deltas {
        None => {
            b.flag(true).flag(false); // delta enabled, no update
        }
        Some(deltas) => {
            b.flag(true).flag(true);
            for delta in deltas {
                b.flag(delta.is_some());
                if let Some(d) = delta {
                    b.su(7, d);
                }
            }
            b.flag(false).flag(false); // mode deltas kept
        }
    }
    b.flag(true); // tx_mode_select
    if !intra {
        b.flag(false); // reference_select: no skip mode either
    }
    b.flag(false); // reduced_tx_set
    if !intra {
        for reference in 1..8 {
            match f.gm_last {
                Some((x, y)) if reference == 1 => {
                    b.flag(true).flag(false).flag(true); // TRANSLATION
                    // Against the default reference 0, with high precision
                    // mv: absBits 9, so mx 512; decode_signed_subexp_with_ref
                    // codes v = value + 512 recentred on r = 512.
                    for value in [x, y] {
                        write_subexp_recentred(b, value + 512, 512, 1025);
                    }
                }
                _ => {
                    b.flag(false);
                }
            }
        }
    }
    if s.film_grain && (f.show_frame || f.showable_frame) {
        match f.grain {
            Grain::Off => {
                b.flag(false);
            }
            Grain::New(seed) => {
                b.flag(true).f(16, u32::from(seed));
                if f.frame_type == FrameType::Inter {
                    b.flag(true); // update_grain
                }
                b.f(4, 1)
                    .f(8, 64)
                    .f(8, 100) // one luma point
                    .flag(false) // chroma_scaling_from_luma
                    .f(4, 0)
                    .f(4, 0) // no chroma points
                    .f(2, 3) // grain_scaling_minus_8
                    .f(2, 0) // ar_coeff_lag: no luma coefficients
                    .f(2, 1) // ar_coeff_shift_minus_6
                    .f(2, 0) // grain_scale_shift
                    .flag(true) // overlap_flag
                    .flag(false); // clip_to_restricted_range
            }
            Grain::Points(y, cb, cr) => {
                b.flag(true).f(16, 7);
                if f.frame_type == FrameType::Inter {
                    b.flag(true); // update_grain
                }
                let points = |b: &mut Bits, values: &[u8]| {
                    b.f(4, u32::try_from(values.len()).unwrap_or(0));
                    for &value in values {
                        b.f(8, u32::from(value)).f(8, 100);
                    }
                };
                points(b, y);
                b.flag(false); // chroma_scaling_from_luma
                points(b, cb);
                points(b, cr);
                b.f(2, 3).f(2, 0); // grain_scaling_minus_8, ar_coeff_lag
                if !cb.is_empty() {
                    b.f(8, 128); // ar_coeffs_cb_plus_128[0]
                }
                if !cr.is_empty() {
                    b.f(8, 128);
                }
                b.f(2, 1).f(2, 0); // ar_coeff_shift_minus_6, grain_scale_shift
                if !cb.is_empty() {
                    b.f(8, 128).f(8, 128).f(9, 256); // cb_mult, cb_luma_mult, cb_offset
                }
                if !cr.is_empty() {
                    b.f(8, 128).f(8, 128).f(9, 256);
                }
                b.flag(true).flag(false); // overlap_flag, clip_to_restricted_range
            }
            Grain::FromSlot(slot, seed) => {
                b.flag(true)
                    .f(16, u32::from(seed))
                    .flag(false)
                    .f(3, u32::from(slot));
            }
        }
    }
}

/// Writes `v` (0 <= v < n) as decode_unsigned_subexp_with_ref() reads it
/// for a reference `r` in the lower half, where inverse_recenter is
/// its own inverse's simple case: the test values stay within `2 * r` of
/// it.
fn write_subexp_recentred(b: &mut Bits, v: i32, r: i32, n: i32) {
    // recenter_nonneg: the inverse of inverse_recenter(r, x).
    let x = if v > 2 * r {
        v
    } else if v >= r {
        (v - r) << 1
    } else {
        ((r - v) << 1) - 1
    };
    // decode_subexp(n) with k = 3: find the bucket.
    let mut i = 0u32;
    let mut mk = 0i32;
    loop {
        let b2 = if i > 0 { 3 + i - 1 } else { 3 };
        let a = 1i32 << b2;
        if n <= mk + 3 * a {
            write_ns(b, (n - mk) as u32, (x - mk) as u32);
            return;
        }
        if x >= mk + a {
            b.flag(true);
            i += 1;
            mk += a;
        } else {
            b.flag(false).f(b2, (x - mk) as u32);
            return;
        }
    }
}

/// `ns(n)` written.
fn write_ns(b: &mut Bits, n: u32, v: u32) {
    if n <= 1 {
        return;
    }
    let w = 32 - n.leading_zeros();
    let m = (1u32 << w) - n;
    if v < m {
        b.f(w - 1, v);
    } else {
        let extra = v + m;
        b.f(w - 1, extra >> 1).f(1, extra & 1);
    }
}

/// A frame OBU: the header, its byte alignment, and a one-tile group of
/// `tile` bytes.
fn frame_obu(f: &Frame, s: Sequence, tile: &[u8]) -> Vec<u8> {
    let mut b = Bits::default();
    uncompressed_header(&mut b, f, s);
    let mut payload = b.aligned();
    payload.extend_from_slice(tile);
    obu(OBU_FRAME, &payload)
}

/// A frame OBU as [`frame_obu`] writes it, in spatial layer `spatial_id`
/// of temporal layer 0.
fn frame_obu_in_layer(f: &Frame, s: Sequence, tile: &[u8], spatial_id: u8) -> Vec<u8> {
    let mut b = Bits::default();
    uncompressed_header(&mut b, f, s);
    let mut payload = b.aligned();
    payload.extend_from_slice(tile);
    obu_in_layer(OBU_FRAME, 0, spatial_id, &payload)
}

/// A frame header OBU that shows the frame in `slot`.
fn show_existing(slot: u8) -> Vec<u8> {
    let mut b = Bits::default();
    b.flag(true).f(3, u32::from(slot));
    obu(OBU_FRAME_HEADER, &b.trailing())
}

fn temporal_unit(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut out = obu(OBU_TEMPORAL_DELIMITER, &[]);
    for part in parts {
        out.extend_from_slice(part);
    }
    out
}

/// What the recording accelerator was asked to decode.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Decoded {
    picture: u32,
    display: Option<u32>,
    references: [Option<u32>; NUM_REF_FRAMES],
    header: FrameHeader,
    tiles: Vec<Vec<u8>>,
}

#[derive(Default)]
struct Recorder {
    next: u32,
    configured: Vec<(u32, u32, u8, bool)>,
    decoded: Vec<Decoded>,
}

impl Accelerator for Recorder {
    type Picture = u32;
    type Error = Error;

    fn configure(&mut self, sequence: &SequenceHeader, _first: &FrameHeader) -> Result<(), Error> {
        let (w, h) = sequence.max_frame_size();
        self.configured.push((
            w,
            h,
            sequence.color.bit_depth,
            sequence.film_grain_params_present,
        ));
        Ok(())
    }

    fn new_picture(&mut self) -> Result<u32, Error> {
        self.next += 1;
        Ok(self.next)
    }

    fn new_display_picture(&mut self) -> Result<u32, Error> {
        self.next += 1;
        Ok(1000 + self.next)
    }

    fn decode_frame(&mut self, p: &FrameParams<'_, u32>) -> Result<(), Error> {
        let tiles = p
            .tile_groups
            .iter()
            .flat_map(|g| {
                g.tiles
                    .iter()
                    .map(|t| g.data[t.offset..t.offset + t.size].to_vec())
            })
            .collect();
        self.decoded.push(Decoded {
            picture: *p.picture,
            display: p.display.copied(),
            references: p.references.map(|r| r.copied()),
            header: p.header.clone(),
            tiles,
        });
        Ok(())
    }
}

fn shown(outputs: &[Output<u32>]) -> Vec<(u32, u64, bool)> {
    outputs
        .iter()
        .map(|o| (*o.picture, o.id, o.show_existing_frame))
        .collect()
}

#[test]
fn a_sequence_header_reads_back() -> Result<(), Error> {
    let s = Sequence {
        width: 1920,
        height: 1080,
        film_grain: true,
        bit_depth_10: true,
        ..Sequence::default()
    };
    let bytes = sequence_header(s);
    let parsed = SequenceHeader::parse(&bytes[2..])?;
    assert_eq!(parsed.max_frame_size(), (1920, 1080));
    assert_eq!(parsed.color.bit_depth, 10);
    assert!(parsed.color.subsampling_x && parsed.color.subsampling_y);
    assert_eq!(parsed.order_hint_bits, 7);
    assert!(parsed.film_grain_params_present && parsed.enable_order_hint);
    assert_eq!(parsed.operating_point().seq_level_idx, 8);
    Ok(())
}

#[test]
fn key_and_inter_frames_fill_and_use_the_slots() -> Result<(), Error> {
    let s = Sequence::default();
    let mut decoder = Decoder::new(Recorder::default());
    let key = temporal_unit(&[sequence_header(s), frame_obu(&Frame::key(), s, &[0xaa])]);
    let out = decoder.decode(&key, 10)?;
    assert_eq!(shown(&out), [(1, 10, false)]);
    assert_eq!(out[0].width, 64);
    assert_eq!(out[0].height, 48);
    // An inter frame refreshing slot 1, predicting from the key frame.
    let inter = Frame::inter(1, 0b10, [0; 7]);
    let out = decoder.decode(&temporal_unit(&[frame_obu(&inter, s, &[0xbb, 0xcc])]), 11)?;
    assert_eq!(shown(&out), [(2, 11, false)]);
    let r = &decoder.accelerator().decoded;
    assert_eq!(r[0].references, [None; 8]);
    assert_eq!(r[1].references, [Some(1); 8]);
    assert_eq!(r[1].tiles, [vec![0xbb, 0xcc]]);
    assert_eq!(r[1].header.ref_frame_idx, [0; 7]);
    // The next sees the inter frame in slot 1.
    let next = Frame::inter(2, 0b100, [1, 0, 0, 0, 0, 0, 0]);
    decoder.decode(&temporal_unit(&[frame_obu(&next, s, &[0xdd])]), 12)?;
    let r = &decoder.accelerator().decoded;
    assert_eq!(r[2].references[..3], [Some(1), Some(2), Some(1)]);
    assert_eq!(decoder.accelerator().configured, [(64, 48, 8, false)]);
    Ok(())
}

#[test]
fn a_hidden_frame_is_shown_later_from_its_slot() -> Result<(), Error> {
    let s = Sequence::default();
    let mut decoder = Decoder::new(Recorder::default());
    decoder.decode(
        &temporal_unit(&[sequence_header(s), frame_obu(&Frame::key(), s, &[1])]),
        0,
    )?;
    // A temporal unit with a hidden altref (slot 6), then a shown frame.
    let altref = Frame {
        show_frame: false,
        ..Frame::inter(4, 0b100_0000, [0; 7])
    };
    let shown_frame = Frame::inter(1, 0b10, [0, 0, 0, 0, 0, 0, 6]);
    let out = decoder.decode(
        &temporal_unit(&[
            frame_obu(&altref, s, &[2]),
            frame_obu(&shown_frame, s, &[3]),
        ]),
        1,
    )?;
    assert_eq!(shown(&out), [(3, 1, false)]);
    // show_existing_frame of the altref: no decode.
    let out = decoder.decode(&temporal_unit(&[show_existing(6)]), 4)?;
    assert_eq!(shown(&out), [(2, 4, true)]);
    assert_eq!(out[0].order_hint, 4);
    assert_eq!(decoder.accelerator().decoded.len(), 3);
    Ok(())
}

#[test]
fn showing_a_hidden_key_frame_refreshes_every_slot() -> Result<(), Error> {
    let s = Sequence::default();
    let mut decoder = Decoder::new(Recorder::default());
    decoder.decode(
        &temporal_unit(&[sequence_header(s), frame_obu(&Frame::key(), s, &[1])]),
        0,
    )?;
    // A forward key frame: hidden, showable, in slot 3 only.
    let hidden_key = Frame {
        show_frame: false,
        showable_frame: true,
        error_resilient: false,
        order_hint: 8,
        refresh: 0b1000,
        ..Frame::key()
    };
    decoder.decode(&temporal_unit(&[frame_obu(&hidden_key, s, &[2])]), 1)?;
    let out = decoder.decode(&temporal_unit(&[show_existing(3)]), 2)?;
    assert_eq!(shown(&out), [(2, 2, true)]);
    assert_eq!(out[0].frame_type, FrameType::Key);
    // Every slot now holds it.
    let inter = Frame::inter(9, 0b1, [0, 1, 2, 3, 4, 5, 7]);
    decoder.decode(&temporal_unit(&[frame_obu(&inter, s, &[3])]), 3)?;
    let last = decoder.accelerator().decoded.last().cloned();
    assert_eq!(last.map(|d| d.references), Some([Some(2); 8]));
    Ok(())
}

#[test]
fn segmentation_and_loop_filter_deltas_come_from_the_primary_reference() -> Result<(), Error> {
    let s = Sequence::default();
    let mut decoder = Decoder::new(Recorder::default());
    let key = Frame {
        segments: Segments::AltQ(-20),
        ref_deltas: Some([None, Some(5), None, None, Some(-7), None, None, None]),
        ..Frame::key()
    };
    decoder.decode(
        &temporal_unit(&[sequence_header(s), frame_obu(&key, s, &[1])]),
        0,
    )?;
    let first = decoder.accelerator().decoded[0].header.clone();
    assert_eq!(first.segmentation.feature_mask[1], 1);
    assert_eq!(first.segmentation.feature_data[1][0], -20);
    // Coded against the defaults [1, 0, 0, 0, -1, 0, -1, -1].
    assert_eq!(first.loop_filter.ref_deltas, [1, 5, 0, 0, -7, 0, -1, -1]);
    // An inter frame keeping the features, primary reference LAST (slot
    // 0): it inherits both.
    let inter = Frame {
        segments: Segments::Keep,
        ..Frame::inter(1, 0b10, [0; 7])
    };
    decoder.decode(&temporal_unit(&[frame_obu(&inter, s, &[2])]), 1)?;
    let second = decoder.accelerator().decoded[1].header.clone();
    assert!(second.segmentation.enabled && !second.segmentation.update_data);
    assert_eq!(second.segmentation.feature_data[1][0], -20);
    assert_eq!(second.loop_filter.ref_deltas, [1, 5, 0, 0, -7, 0, -1, -1]);
    // With no primary reference, the defaults again.
    let independent = Frame {
        primary_ref_frame: PRIMARY_REF_NONE,
        ..Frame::inter(2, 0b100, [1; 7])
    };
    decoder.decode(&temporal_unit(&[frame_obu(&independent, s, &[3])]), 2)?;
    let third = decoder.accelerator().decoded[2].header.clone();
    assert!(!third.segmentation.enabled);
    assert_eq!(third.loop_filter.ref_deltas, frame::DEFAULT_REF_DELTAS);
    Ok(())
}

#[test]
fn film_grain_points_that_do_not_increase_are_refused() {
    let s = Sequence {
        film_grain: true,
        ..Sequence::default()
    };
    let decode = |grain: Grain| {
        let key = Frame {
            grain,
            ..Frame::key()
        };
        let mut decoder = Decoder::new(Recorder::default());
        let result = decoder.decode(
            &temporal_unit(&[sequence_header(s), frame_obu(&key, s, &[1])]),
            0,
        );
        (result.err(), decoder.accelerator().decoded.len())
    };
    let refused = (Some(Error::FilmGrainPointsUnordered), 0);
    assert_eq!(decode(Grain::Points(&[64, 64], &[], &[])), refused);
    assert_eq!(decode(Grain::Points(&[64, 32], &[], &[])), refused);
    assert_eq!(decode(Grain::Points(&[16, 64], &[8, 8], &[8, 9])), refused);
    // 4:2:0 has points for both chroma components or neither.
    assert_eq!(
        decode(Grain::Points(&[16, 64], &[8, 9], &[])),
        (Some(Error::OutOfRange("num_cr_points")), 0)
    );
    assert_eq!(
        decode(Grain::Points(&[16, 64], &[8, 9], &[8, 9])),
        (None, 1)
    );
    assert_eq!(decode(Grain::Points(&[16, 64], &[], &[])), (None, 1));
}

#[test]
fn film_grain_goes_to_a_display_picture_and_loads_from_a_slot() -> Result<(), Error> {
    let s = Sequence {
        film_grain: true,
        ..Sequence::default()
    };
    let mut decoder = Decoder::new(Recorder::default());
    let key = Frame {
        grain: Grain::New(1234),
        ..Frame::key()
    };
    let out = decoder.decode(
        &temporal_unit(&[sequence_header(s), frame_obu(&key, s, &[1])]),
        0,
    )?;
    // The display picture is shown; the decoded one is the reference.
    assert_eq!(shown(&out), [(1002, 0, false)]);
    let decoded = decoder.accelerator().decoded[0].clone();
    assert_eq!((decoded.picture, decoded.display), (1, Some(1002)));
    let g = decoded.header.film_grain;
    assert!(g.apply_grain && g.update_grain);
    assert_eq!(
        (
            g.grain_seed,
            g.num_y_points,
            g.point_y_value[0],
            g.point_y_scaling[0]
        ),
        (1234, 1, 64, 100)
    );
    assert_eq!((g.grain_scaling_minus_8, g.ar_coeff_shift_minus_6), (3, 1));
    // An inter frame taking slot 0's parameters with a new seed.
    let inter = Frame {
        grain: Grain::FromSlot(0, 99),
        ..Frame::inter(1, 0b10, [0; 7])
    };
    decoder.decode(&temporal_unit(&[frame_obu(&inter, s, &[2])]), 1)?;
    let decoded = decoder.accelerator().decoded[1].clone();
    assert_eq!(decoded.references[0], Some(1));
    let loaded = decoded.header.film_grain;
    assert_eq!(loaded.grain_seed, 99);
    assert_eq!(loaded.point_y_scaling[0], 100);
    assert!(loaded.apply_grain);
    // A frame without grain has no display picture.
    let plain = Frame::inter(2, 0b100, [1; 7]);
    let out = decoder.decode(&temporal_unit(&[frame_obu(&plain, s, &[3])]), 2)?;
    let decoded = &decoder.accelerator().decoded[2];
    assert_eq!(decoded.display, None);
    assert_eq!(*out[0].picture, decoded.picture);
    Ok(())
}

#[test]
fn short_signaling_picks_the_references_by_order_hint() -> Result<(), Error> {
    let s = Sequence::default();
    let mut decoder = Decoder::new(Recorder::default());
    decoder.decode(
        &temporal_unit(&[sequence_header(s), frame_obu(&Frame::key(), s, &[1])]),
        0,
    )?;
    // Slots 1 to 4 get order hints 1, 2, 3 and 16 (a future altref).
    for (hint, slot) in [(1u32, 1u8), (2, 2), (3, 3), (16, 4)] {
        let frame = Frame {
            show_frame: hint != 16,
            ..Frame::inter(hint, 1 << slot, [0; 7])
        };
        decoder.decode(
            &temporal_unit(&[frame_obu(&frame, s, &[2])]),
            u64::from(hint),
        )?;
    }
    // At order hint 4, LAST is slot 3 and GOLDEN slot 0 (hint 0).
    let frame = Frame {
        short_signaling: Some((3, 0)),
        ..Frame::inter(4, 0b10_0000, [0; 7])
    };
    decoder.decode(&temporal_unit(&[frame_obu(&frame, s, &[3])]), 4)?;
    let header = decoder
        .accelerator()
        .decoded
        .last()
        .map(|d| d.header.clone());
    // ALTREF is the latest backward, slot 4 (hint 16); no backward slot
    // is left for BWDREF and ALTREF2. LAST2 and LAST3 take the latest
    // forward ones left, 2 then 1; BWDREF and ALTREF2 then take the latest
    // of slots 5 to 7 (hint 0, as slot 0), the last of equals winning: 7,
    // then 6.
    assert_eq!(header.map(|h| h.ref_frame_idx), Some([3, 2, 1, 0, 7, 6, 4]));
    Ok(())
}

#[test]
fn global_motion_translation_reads_its_deltas() -> Result<(), Error> {
    let s = Sequence::default();
    let mut decoder = Decoder::new(Recorder::default());
    decoder.decode(
        &temporal_unit(&[sequence_header(s), frame_obu(&Frame::key(), s, &[1])]),
        0,
    )?;
    let frame = Frame {
        gm_last: Some((3, -5)),
        ..Frame::inter(1, 0b10, [0; 7])
    };
    decoder.decode(&temporal_unit(&[frame_obu(&frame, s, &[2])]), 1)?;
    let header = decoder.accelerator().decoded[1].header.clone();
    let gm = header.global_motion[1];
    assert_eq!(gm.kind, 1);
    // TRANSLATION with high precision: precBits 3, so a coded unit is
    // 1 << 13 of WARPEDMODEL_PREC_BITS.
    assert_eq!(gm.params[..2], [3 << 13, -5 << 13]);
    assert_eq!(gm.params[2..], [1 << 16, 0, 0, 1 << 16]);
    assert!(gm.shear_is_valid());
    assert_eq!(header.global_motion[2].kind, 0);
    Ok(())
}

#[test]
fn a_frame_split_over_tile_groups_is_decoded_once_whole() -> Result<(), Error> {
    let s = Sequence::default();
    let mut decoder = Decoder::new(Recorder::default());
    // A frame header OBU, its copy, then one tile group of the one tile.
    let mut b = Bits::default();
    uncompressed_header(&mut b, &Frame::key(), s);
    let header = obu(OBU_FRAME_HEADER, &b.trailing());
    let out = decoder.decode(
        &temporal_unit(&[
            sequence_header(s),
            header.clone(),
            header,
            obu(OBU_TILE_GROUP, &[7, 8]),
        ]),
        0,
    )?;
    assert_eq!(shown(&out), [(1, 0, false)]);
    assert_eq!(decoder.accelerator().decoded[0].tiles, [vec![7, 8]]);
    Ok(())
}

#[test]
fn a_frame_obus_tiles_start_at_tile_0() -> Result<(), Error> {
    let s = Sequence {
        width: 128,
        ..Sequence::default()
    };
    let mut decoder = Decoder::new(Recorder::default());
    // Two tiles: the first's size, a byte, then each tile's byte.
    let both = frame_obu(&Frame::key(), s, &[0x00, 0x00, 7, 8]);
    decoder.decode(&temporal_unit(&[sequence_header(s), both]), 0)?;
    assert_eq!(decoder.accelerator().decoded[0].tiles, [vec![7], vec![8]]);
    // tile_start_and_end_present_flag, tg_start 1, tg_end 1: tile 1 only.
    let second = frame_obu(&Frame::key(), s, &[0b1110_0000, 8]);
    assert_eq!(
        decoder.decode(&temporal_unit(&[second]), 1).err(),
        Some(Error::OutOfRange("tg_start"))
    );
    assert_eq!(decoder.accelerator().decoded.len(), 1);
    Ok(())
}

#[test]
fn frames_of_more_tiles_than_the_hardware_holds_are_refused() -> Result<(), Error> {
    let decode = |width: u32, height: u32, tiles: usize| {
        let s = Sequence {
            width,
            height,
            ..Sequence::default()
        };
        // Each tile a byte, all but the last after its size.
        let mut data = vec![0x00];
        for _ in 1..tiles {
            data.extend_from_slice(&[0x00, 0x11]);
        }
        data.push(0x22);
        let mut decoder = Decoder::new(Recorder::default());
        let result = decoder.decode(
            &temporal_unit(&[sequence_header(s), frame_obu(&Frame::key(), s, &data)]),
            0,
        );
        result.map(|_| decoder.accelerator().decoded[0].tiles.len())
    };
    // 64 by 64 superblocks: 4,096 tiles.
    assert_eq!(
        decode(4096, 4096, 4096),
        Err(Error::Unsupported("more than 256 tiles"))
    );
    // 16 by 16: 256.
    assert_eq!(decode(1024, 1024, 256)?, 256);
    Ok(())
}

#[test]
fn what_cannot_be_decoded_is_refused() {
    let s = Sequence::default();
    let mut decoder = Decoder::new(Recorder::default());
    // No sequence header yet.
    assert_eq!(
        decoder
            .decode(&temporal_unit(&[frame_obu(&Frame::key(), s, &[1])]), 0)
            .err(),
        Some(Error::MissingSequenceHeader)
    );
    // An inter frame before any key frame.
    let inter = Frame::inter(1, 1, [0; 7]);
    assert_eq!(
        decoder
            .decode(
                &temporal_unit(&[sequence_header(s), frame_obu(&inter, s, &[1])]),
                0
            )
            .err(),
        Some(Error::MissingReference)
    );
    // After a key frame it decodes; after reset() it is refused again.
    let key = temporal_unit(&[frame_obu(&Frame::key(), s, &[1])]);
    assert!(decoder.decode(&key, 1).is_ok());
    assert!(
        decoder
            .decode(&temporal_unit(&[frame_obu(&inter, s, &[1])]), 2)
            .is_ok()
    );
    decoder.reset();
    assert_eq!(
        decoder
            .decode(&temporal_unit(&[frame_obu(&inter, s, &[1])]), 3)
            .err(),
        Some(Error::MissingReference)
    );
    assert_eq!(
        decoder.decode(&temporal_unit(&[show_existing(2)]), 4).err(),
        Some(Error::MissingReference)
    );
    // A tile list: large scale tile.
    assert_eq!(
        decoder.decode(&obu(OBU_TILE_LIST, &[0]), 5).err(),
        Some(Error::Unsupported("large scale tile decoding"))
    );
    // A frame header whose tiles never come.
    let mut b = Bits::default();
    uncompressed_header(&mut b, &Frame::key(), s);
    assert_eq!(
        decoder
            .decode(&obu(OBU_FRAME_HEADER, &b.trailing()), 6)
            .err(),
        Some(Error::Truncated)
    );
}

#[test]
fn a_new_sequence_starts_at_its_key_frame() -> Result<(), Error> {
    let small = Sequence::default();
    let large = Sequence {
        width: 32,
        height: 32,
        bit_depth_10: true,
        ..Sequence::default()
    };
    let mut decoder = Decoder::new(Recorder::default());
    decoder.decode(
        &temporal_unit(&[
            sequence_header(small),
            frame_obu(&Frame::key(), small, &[1]),
        ]),
        0,
    )?;
    // The same header again changes nothing.
    decoder.decode(
        &temporal_unit(&[
            sequence_header(small),
            frame_obu(&Frame::inter(1, 1, [0; 7]), small, &[2]),
        ]),
        1,
    )?;
    assert_eq!(decoder.accelerator().configured.len(), 1);
    // Another, then an inter frame: refused.
    let refused = decoder.decode(
        &temporal_unit(&[
            sequence_header(large),
            frame_obu(&Frame::inter(2, 1, [0; 7]), large, &[3]),
        ]),
        2,
    );
    assert!(refused.is_err());
    // Its key frame starts it.
    let out = decoder.decode(
        &temporal_unit(&[
            sequence_header(large),
            frame_obu(&Frame::key(), large, &[4]),
        ]),
        3,
    )?;
    assert_eq!(out[0].bit_depth, 10);
    assert_eq!(
        decoder.accelerator().configured,
        [(64, 48, 8, false), (32, 32, 10, false)]
    );
    Ok(())
}

#[test]
fn the_reference_state_holds_what_later_headers_read() {
    let header = FrameHeader {
        upscaled_width: 640,
        frame_height: 360,
        order_hint: 9,
        frame_type: FrameType::IntraOnly,
        ..FrameHeader::default()
    };
    let state: ReferenceState = header.reference_state();
    assert!(state.valid);
    assert_eq!(
        (state.upscaled_width, state.frame_height, state.order_hint),
        (640, 360, 9)
    );
    assert_eq!(state.frame_type, FrameType::IntraOnly);
}

/// What decoding each temporal unit in turn says: the first refusal, and
/// the frames that reached the accelerator.
fn refusal(units: &[Vec<u8>]) -> (Option<String>, usize) {
    let mut decoder = Decoder::new(Recorder::default());
    let error = units
        .iter()
        .enumerate()
        .find_map(|(id, unit)| decoder.decode(unit, id as u64).err());
    (
        error.map(|e| e.to_string()),
        decoder.accelerator().decoded.len(),
    )
}

#[test]
fn references_past_2_to_1_down_or_16_to_1_up_are_refused() {
    let past = Some("a reference out of scale (7.11.3.3)".to_owned());
    // A 64x48 key frame in every slot, then an inter frame of `size`
    // predicting from them all.
    let s = Sequence::default();
    let key = temporal_unit(&[sequence_header(s), frame_obu(&Frame::key(), s, &[1])]);
    for (size, refused) in [((31, 24), true), ((32, 23), true), ((32, 24), false)] {
        let inter = Frame {
            size: Some(size),
            ..Frame::inter(1, 0b10, [0; 7])
        };
        let unit = temporal_unit(&[frame_obu(&inter, s, &[2])]);
        assert_eq!(
            refusal(&[key.clone(), unit]),
            if refused {
                (past.clone(), 1)
            } else {
                (None, 2)
            },
            "{size:?}"
        );
    }
    // A key frame of 4x3, then a frame of the sequence's 64x48, 16 times
    // as large; of 3x3 or 4x2, too large.
    for (key_size, refused) in [((4, 3), false), ((3, 3), true), ((4, 2), true)] {
        let small_key = Frame {
            size: Some(key_size),
            ..Frame::key()
        };
        let key = temporal_unit(&[sequence_header(s), frame_obu(&small_key, s, &[1])]);
        let inter = temporal_unit(&[frame_obu(&Frame::inter(1, 0b10, [0; 7]), s, &[2])]);
        assert_eq!(
            refusal(&[key, inter]),
            if refused {
                (past.clone(), 1)
            } else {
                (None, 2)
            },
            "{key_size:?}"
        );
    }
    // Every reference counts, not only the first: slot 1 holds a 32x24
    // frame, slot 0 the 64x48 key frame, and a 16x16 frame within 2:1 of
    // the first is past it of the key frame, its seventh.
    let s = Sequence::default();
    let key = temporal_unit(&[sequence_header(s), frame_obu(&Frame::key(), s, &[1])]);
    let half = Frame {
        size: Some((32, 24)),
        ..Frame::inter(1, 0b10, [0; 7])
    };
    let half = temporal_unit(&[frame_obu(&half, s, &[2])]);
    for (refs, refused) in [([1, 1, 1, 1, 1, 1, 0], true), ([1; 7], false)] {
        let quarter = Frame {
            size: Some((16, 16)),
            ..Frame::inter(2, 0b100, refs)
        };
        let quarter = temporal_unit(&[frame_obu(&quarter, s, &[3])]);
        assert_eq!(
            refusal(&[key.clone(), half.clone(), quarter]),
            if refused {
                (past.clone(), 2)
            } else {
                (None, 3)
            },
            "{refs:?}"
        );
    }
}

/// 6.8.2 has a frame shown again coded showable, but Apple's host shows
/// one coded `showable_frame` 0, as libaom writes its filtered forward key
/// frames, and so does the decoder.
#[test]
fn a_frame_coded_not_showable_is_shown_as_apples_host_shows_it() -> Result<(), Error> {
    let s = Sequence::default();
    let key = temporal_unit(&[sequence_header(s), frame_obu(&Frame::key(), s, &[1])]);
    for showable_frame in [false, true] {
        let hidden = Frame {
            show_frame: false,
            showable_frame,
            ..Frame::inter(4, 0b100_0000, [0; 7])
        };
        let hidden = temporal_unit(&[frame_obu(&hidden, s, &[2])]);
        let mut decoder = Decoder::new(Recorder::default());
        decoder.decode(&key, 0)?;
        assert!(decoder.decode(&hidden, 1)?.is_empty());
        let shown_again = decoder.decode(&temporal_unit(&[show_existing(6)]), 2)?;
        assert_eq!(shown(&shown_again), [(2, 2, true)], "{showable_frame}");
    }
    // A hidden key frame coded not showable, as libaom's are, shown later.
    let hidden_key = Frame {
        show_frame: false,
        showable_frame: false,
        error_resilient: false,
        order_hint: 8,
        refresh: 0b1000,
        ..Frame::key()
    };
    let hidden_key = temporal_unit(&[frame_obu(&hidden_key, s, &[2])]);
    assert_eq!(
        refusal(&[key.clone(), hidden_key, temporal_unit(&[show_existing(3)])]),
        (None, 2)
    );
    // A key frame shown from its slot is shown so once.
    let hidden_key = Frame {
        show_frame: false,
        showable_frame: true,
        error_resilient: false,
        order_hint: 8,
        refresh: 0b1000,
        ..Frame::key()
    };
    let hidden_key = temporal_unit(&[frame_obu(&hidden_key, s, &[2])]);
    let show = temporal_unit(&[show_existing(3)]);
    assert_eq!(
        refusal(&[key, hidden_key, show.clone(), show]),
        (Some("a key frame shown again".to_owned()), 2)
    );
    Ok(())
}

#[test]
fn a_temporal_unit_shows_one_frame_of_its_layer() {
    let s = Sequence::default();
    let key = temporal_unit(&[sequence_header(s), frame_obu(&Frame::key(), s, &[1])]);
    let second = Some("a second shown frame of a spatial layer in a temporal unit".to_owned());
    let shown_frame = frame_obu(&Frame::inter(1, 0b10, [0; 7]), s, &[2]);
    let first = temporal_unit(std::slice::from_ref(&shown_frame));
    // A frame shown twice; a frame, then a show; a thousand shows. Slot 1
    // holds a shown inter frame, which may be shown again.
    for (units, decoded) in [
        (
            vec![
                key.clone(),
                first.clone(),
                temporal_unit(&[show_existing(1), show_existing(1)]),
            ],
            2,
        ),
        (
            vec![
                key.clone(),
                first.clone(),
                temporal_unit(&[shown_frame.clone(), show_existing(1)]),
            ],
            3,
        ),
        (
            vec![
                key.clone(),
                first.clone(),
                temporal_unit(&vec![show_existing(1); 1000]),
            ],
            2,
        ),
    ] {
        assert_eq!(refusal(&units), (second.clone(), decoded));
    }
    // A hidden frame and a shown one in a unit, and a show in the next.
    let hidden = Frame {
        show_frame: false,
        ..Frame::inter(4, 0b100_0000, [0; 7])
    };
    let units = [
        key,
        temporal_unit(&[frame_obu(&hidden, s, &[3]), shown_frame]),
        temporal_unit(&[show_existing(6)]),
    ];
    assert_eq!(refusal(&units), (None, 3));
}

/// Two spatial layers, the lower half the upper's size on a side, as
/// libaom's scalable coding makes them (the VA-API crate's
/// `tools/av1-svc.c`): each temporal
/// unit shows a frame of each, and the upper one alone is output.
#[test]
fn a_unit_of_two_spatial_layers_outputs_the_upper() -> Result<(), Error> {
    let s = Sequence {
        operating_point_idc: 0x301,
        ..Sequence::default()
    };
    let base = |f: Frame| Frame {
        size: Some((32, 24)),
        ..f
    };
    let units = [
        temporal_unit(&[
            sequence_header(s),
            frame_obu_in_layer(&base(Frame::key()), s, &[1], 0),
            // The upper layer predicts from the lower in its unit.
            frame_obu_in_layer(&Frame::inter(0, 0b10, [0; 7]), s, &[2], 1),
        ]),
        temporal_unit(&[
            frame_obu_in_layer(&base(Frame::inter(1, 0b1, [0; 7])), s, &[3], 0),
            frame_obu_in_layer(&Frame::inter(1, 0b10, [1, 1, 1, 0, 1, 1, 1]), s, &[4], 1),
        ]),
        // A unit with the lower layer alone shows it.
        temporal_unit(&[frame_obu_in_layer(
            &base(Frame::inter(2, 0b1, [0; 7])),
            s,
            &[5],
            0,
        )]),
    ];
    let mut decoder = Decoder::new(Recorder::default());
    let mut out = Vec::new();
    for (id, unit) in units.iter().enumerate() {
        let outputs = decoder.decode(unit, id as u64)?;
        out.extend(
            outputs
                .iter()
                .map(|o| (*o.picture, o.id, o.spatial_id, o.width, o.height)),
        );
    }
    assert_eq!(decoder.accelerator().decoded.len(), 5);
    assert_eq!(
        out,
        [(2, 0, 1, 64, 48), (4, 1, 1, 64, 48), (5, 2, 0, 32, 24)]
    );
    // The upper layer's frames predict from the lower layer's.
    let decoded = &decoder.accelerator().decoded;
    assert_eq!(decoded[1].references[0], Some(1));
    assert_eq!(decoded[3].references[0], Some(3));
    assert_eq!(decoded[3].references[1], Some(2));
    Ok(())
}

/// An operating point of the lower spatial layer decodes that layer
/// alone: the upper layer's OBUs are dropped before they are parsed.
#[test]
fn an_operating_point_of_the_lower_layer_drops_the_upper() -> Result<(), Error> {
    let s = Sequence {
        operating_point_idc: 0x101,
        ..Sequence::default()
    };
    let unit = temporal_unit(&[
        sequence_header(s),
        frame_obu_in_layer(&Frame::key(), s, &[1], 0),
        // Not even a valid frame: never read.
        obu_in_layer(OBU_FRAME, 0, 1, &[0xff; 4]),
    ]);
    let mut decoder = Decoder::new(Recorder::default());
    let outputs = decoder.decode(&unit, 7)?;
    assert_eq!(shown(&outputs), [(1, 7, false)]);
    assert_eq!(decoder.accelerator().decoded.len(), 1);
    Ok(())
}

/// Each frame a unit decodes or shows again is a step, numbered by the
/// frame, with the slots after it and the frame its state loads from.
#[test]
fn each_frame_a_unit_carries_is_a_step() -> Result<(), Error> {
    let s = Sequence::default();
    let mut decoder = Decoder::new(Recorder::default());
    decoder.decode(
        &temporal_unit(&[sequence_header(s), frame_obu(&Frame::key(), s, &[1])]),
        0,
    )?;
    let steps = decoder.steps().to_vec();
    assert_eq!(steps.len(), 1);
    let key = steps[0].picture_id;
    assert_eq!(steps[0].references, [Some(key); 8]);
    assert_eq!(steps[0].primary_reference, None);
    assert!(steps[0].output && !steps[0].shown_again);
    assert_eq!((steps[0].width, steps[0].height), (64, 48));
    assert_eq!(steps[0].largest, (64, 48));
    // A hidden altref into slot 6 loading its state from slot 0, then a
    // shown frame loading from slot 6's altref.
    let altref = Frame {
        show_frame: false,
        ..Frame::inter(4, 0b100_0000, [0; 7])
    };
    let shown_frame = Frame {
        primary_ref_frame: 6,
        ..Frame::inter(1, 0b10, [0, 0, 0, 0, 0, 0, 6])
    };
    decoder.decode(
        &temporal_unit(&[
            frame_obu(&altref, s, &[2]),
            frame_obu(&shown_frame, s, &[3]),
        ]),
        1,
    )?;
    let steps = decoder.steps().to_vec();
    assert_eq!(steps.len(), 2);
    let (hidden, inter) = (steps[0].picture_id, steps[1].picture_id);
    assert!(hidden > key && inter > hidden);
    assert_eq!(steps[0].primary_reference, Some(key));
    assert_eq!(steps[1].primary_reference, Some(hidden));
    assert_eq!(steps[0].references[6], Some(hidden));
    assert_eq!(steps[1].references[1], Some(inter));
    assert_eq!((steps[0].output, steps[1].output), (false, true));
    // Showing the altref again is a step of the altref's number.
    let out = decoder.decode(&temporal_unit(&[show_existing(6)]), 4)?;
    assert_eq!(out[0].picture_id, hidden);
    let steps = decoder.steps();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].picture_id, hidden);
    assert!(steps[0].shown_again && steps[0].output);
    assert_eq!(steps[0].primary_reference, None);
    Ok(())
}

/// A unit of two spatial layers is two steps, the lower not output.
#[test]
fn both_layers_of_a_unit_are_steps() -> Result<(), Error> {
    let s = Sequence {
        operating_point_idc: 0x301,
        ..Sequence::default()
    };
    let base = Frame {
        size: Some((32, 24)),
        ..Frame::key()
    };
    let unit = temporal_unit(&[
        sequence_header(s),
        frame_obu_in_layer(&base, s, &[1], 0),
        frame_obu_in_layer(&Frame::inter(0, 0b10, [0; 7]), s, &[2], 1),
    ]);
    let mut decoder = Decoder::new(Recorder::default());
    decoder.decode(&unit, 0)?;
    let steps = decoder.steps();
    let sizes: Vec<_> = steps
        .iter()
        .map(|s| (s.spatial_id, s.width, s.height, s.output))
        .collect();
    assert_eq!(sizes, [(0, 32, 24, false), (1, 64, 48, true)]);
    assert_eq!(steps[1].primary_reference, Some(steps[0].picture_id));
    Ok(())
}

mod fuzz;
