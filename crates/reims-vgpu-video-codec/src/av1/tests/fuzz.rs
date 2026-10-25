//! AV1's parsers and decoding process under seeded fuzzing, up to an
//! accelerator that checks what VA-API's is handed. The temporal units are
//! the tests' own writers': a key frame, then inter, intra-only and switch
//! frames, shown existing frames and frames split over tile groups, with
//! fields that stray as far as each stream's noise says, then mutated by
//! OBU or by unit.

use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::fuzz_support::{Rng, Target, long, maybe_mutate, mutate_rounds, quick};

const STREAMS: Target = Target::new("av1-streams");
const RECORDS: Target = Target::new("av1-records");

/// Frames that reached the accelerator, which says how deep the inputs
/// get.
static FRAMES: AtomicU64 = AtomicU64::new(0);

#[test]
fn streams() {
    let report = quick(&STREAMS, 5000, stream_case);
    eprintln!(
        "av1: {} frames reached the accelerator in {} streams",
        FRAMES.load(Ordering::Relaxed),
        report.inputs
    );
}

#[test]
#[ignore = "long: RVGV_FUZZ_CASES, RVGV_FUZZ_SECONDS, RVGV_FUZZ_SEED"]
fn streams_long() {
    let report = long(&STREAMS, stream_case);
    eprintln!(
        "av1: {} frames reached the accelerator in {} streams",
        FRAMES.load(Ordering::Relaxed),
        report.inputs
    );
}

#[test]
fn records() {
    quick(&RECORDS, 3000, record_case);
}

#[test]
#[ignore = "long: RVGV_FUZZ_CASES, RVGV_FUZZ_SECONDS, RVGV_FUZZ_SEED"]
fn records_long() {
    long(&RECORDS, record_case);
}

/// An accelerator that checks what the decoder gives it against what
/// VA-API's takes: tiles inside their groups' data, every tile of the
/// frame once and no more than `MAX_TILES`, film grain points that
/// increase, a display picture where grain applies. It fails one call in
/// 64, as a driver may.
#[derive(Default)]
struct Checking {
    next: u32,
    configured: bool,
    failing: u64,
}

impl Checking {
    fn fails(&mut self) -> bool {
        self.failing = self
            .failing
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1);
        self.failing >> 58 == 0
    }
}

fn increasing(values: &[u8], count: u8) -> bool {
    values
        .get(..usize::from(count))
        .is_some_and(|values| values.windows(2).all(|w| w[0] < w[1]))
}

impl Accelerator for Checking {
    type Picture = u32;
    type Error = Error;

    fn configure(
        &mut self,
        sequence: &sequence::SequenceHeader,
        _first: &frame::FrameHeader,
    ) -> Result<(), Error> {
        let (width, height) = sequence.max_frame_size();
        assert!(width > 0 && height > 0 && width <= 65536 && height <= 65536);
        self.configured = !self.fails();
        if self.configured {
            Ok(())
        } else {
            Err(Error::Unsupported("the stand-in's configuration failure"))
        }
    }

    fn new_picture(&mut self) -> Result<u32, Error> {
        if self.fails() {
            return Err(Error::Unsupported("the stand-in's picture failure"));
        }
        self.next += 1;
        Ok(self.next)
    }

    fn new_display_picture(&mut self) -> Result<u32, Error> {
        self.new_picture()
    }

    fn decode_frame(&mut self, p: &FrameParams<'_, u32>) -> Result<(), Error> {
        assert!(self.configured, "a frame before a configuration");
        let info = &p.header.tile_info;
        let frame_tiles = info.cols as usize * info.rows as usize;
        assert!(
            frame_tiles <= frame::MAX_TILES,
            "{frame_tiles} tiles in a frame"
        );
        let mut seen = vec![false; frame_tiles];
        for group in p.tile_groups {
            for tile in &group.tiles {
                assert!(
                    tile.offset
                        .checked_add(tile.size)
                        .is_some_and(|end| end <= group.data.len()),
                    "a tile of {} bytes at {} in {} bytes",
                    tile.size,
                    tile.offset,
                    group.data.len()
                );
                assert!(tile.row < info.rows && tile.column < info.cols);
                let index = (tile.row * info.cols + tile.column) as usize;
                assert!(!seen[index], "tile {index} twice");
                seen[index] = true;
            }
        }
        assert!(seen.iter().all(|&s| s), "a frame with tiles missing");
        let grain = &p.header.film_grain;
        if grain.apply_grain {
            assert!(p.display.is_some(), "grain with no display picture");
            assert!(
                increasing(&grain.point_y_value, grain.num_y_points)
                    && increasing(&grain.point_cb_value, grain.num_cb_points)
                    && increasing(&grain.point_cr_value, grain.num_cr_points),
                "film grain points that do not increase"
            );
        }
        FRAMES.fetch_add(1, Ordering::Relaxed);
        if self.fails() {
            return Err(Error::Unsupported("the stand-in's decode failure"));
        }
        Ok(())
    }
}

/// How far a stream strays: 0 not at all, 1 now and then, 2 often.
#[derive(Clone, Copy)]
struct Noise(u8);

impl Noise {
    fn of(rng: &mut Rng) -> Self {
        Self(rng.below(3) as u8)
    }

    fn strays(self, rng: &mut Rng) -> bool {
        match self.0 {
            0 => false,
            1 => rng.one_in(16),
            _ => rng.one_in(4),
        }
    }
}

/// Film grain points, increasing and not: the first [`INCREASING`]
/// increase.
const INCREASING: usize = 5;
const POINTS: &[&[u8]] = &[
    &[],
    &[64],
    &[0, 128, 255],
    &[16, 32, 48, 64, 80, 96, 112, 128, 144, 160],
    &[
        0, 18, 36, 54, 72, 90, 108, 126, 144, 162, 180, 198, 216, 234,
    ],
    &[64, 64],
    &[100, 50],
    &[0, 255, 255],
];

fn sequence(rng: &mut Rng, noise: Noise) -> Sequence {
    let side = |rng: &mut Rng| {
        if noise.strays(rng) {
            *rng.pick(&[1, 2, 7, 63, 65, 1023, 1024, 4096, 8192, 65536])
        } else {
            *rng.pick(&[16, 64, 64, 96, 128, 200, 320])
        }
    };
    Sequence {
        width: side(rng),
        height: side(rng),
        film_grain: rng.bool(),
        bit_depth_10: rng.bool(),
        operating_point_idc: 0,
    }
}

/// The tiles the writer's `tile_info()` makes for `s`: as many as the
/// frame may have.
fn tiles_of(s: Sequence) -> usize {
    let (columns, rows) = (s.width.div_ceil(64).min(64), s.height.div_ceil(64).min(64));
    (columns * rows) as usize
}

/// Tile data for `tiles` tiles: a tile group's header where there are
/// several, `tile_start_and_end_present_flag` 0 and its byte alignment,
/// then one-byte sizes, all but the last's.
fn tile_data(rng: &mut Rng, tiles: usize) -> Vec<u8> {
    let mut data = Vec::new();
    if tiles > 1 {
        data.push(0);
    }
    for tile in 0..tiles {
        let length = rng.range(1, 6) as usize;
        if tile + 1 < tiles {
            data.push((length - 1) as u8);
        }
        data.extend(rng.bytes(length));
    }
    data
}

/// A frame header for the frame at `index` in a stream whose slots are
/// filled by its key frame.
fn frame_of(rng: &mut Rng, noise: Noise, index: u32) -> Frame {
    let mut f = if index == 0 {
        Frame::key()
    } else {
        let refresh = if rng.bool() { 1 << rng.below(8) } else { 0 };
        let refs =
            [0, 1, 2, 3, 4, 5, 6].map(|r: u8| if rng.one_in(3) { rng.below(8) as u8 } else { r });
        let mut inter = Frame::inter(index % 128, refresh, refs);
        if rng.one_in(8) {
            inter.frame_type = FrameType::IntraOnly;
            inter.refresh = rng.range(1, 254) as u8;
        }
        if rng.one_in(12) {
            inter.frame_type = FrameType::Switch;
            inter.error_resilient = true;
            inter.refresh = 0xff;
        }
        if rng.one_in(4) {
            inter.primary_ref_frame = PRIMARY_REF_NONE;
        }
        if rng.one_in(6) {
            inter.show_frame = false;
            inter.showable_frame = rng.bool();
        }
        if rng.one_in(6) {
            inter.short_signaling = Some((rng.below(8) as u8, rng.below(8) as u8));
        }
        if rng.one_in(4) {
            inter.gm_last = Some((rng.below(1025) as i32 - 512, rng.below(1025) as i32 - 512));
        }
        inter
    };
    f.base_q_idx = rng.range_u32(0, 255);
    if noise.strays(rng) {
        f.primary_ref_frame = rng.below(8) as u8;
    }
    let primary_none =
        f.frame_type.is_intra() || f.error_resilient || f.primary_ref_frame == PRIMARY_REF_NONE;
    f.segments = match rng.below(4) {
        0 => Segments::AltQ(rng.below(511) as i32 - 255),
        1 if !primary_none => Segments::Keep,
        _ => Segments::Off,
    };
    if rng.one_in(4) {
        f.ref_deltas = Some([0; 8].map(|_: i32| rng.bool().then(|| rng.below(127) as i32 - 63)));
    }
    f.grain = match rng.below(5) {
        0 => Grain::New(rng.next_u32() as u16),
        1 if f.frame_type == FrameType::Inter => {
            // One of the frame's references (7.20), unless it strays.
            let slot = if noise.strays(rng) || f.short_signaling.is_some() {
                rng.below(8) as u8
            } else {
                *rng.pick(&f.refs)
            };
            Grain::FromSlot(slot, rng.next_u32() as u16)
        }
        2 => {
            // Points that increase, and 4:2:0's chroma points for both
            // components or neither, unless it strays.
            let strays = noise.strays(rng);
            let pick = |rng: &mut Rng, most: usize| loop {
                let points = if strays {
                    *rng.pick(POINTS)
                } else {
                    *rng.pick(&POINTS[..INCREASING])
                };
                if points.len() <= most {
                    break points;
                }
            };
            let y = pick(rng, 14);
            let cb = pick(rng, 10);
            let cr = if strays { pick(rng, 10) } else { cb };
            Grain::Points(y, cb, cr)
        }
        _ => Grain::Off,
    };
    if noise.strays(rng) {
        f.order_hint = rng.range_u32(0, 127);
    }
    if noise.strays(rng) {
        f.refresh = rng.next_u32() as u8;
    }
    f
}

/// A stream of temporal units: the sequence header and a key frame, then
/// frames, shown existing frames and new sequence headers, mutated as its
/// noise says.
fn stream(rng: &mut Rng, noise: Noise) -> (Sequence, Vec<Vec<u8>>) {
    let mut s = sequence(rng, noise);
    let first = s;
    let mut units = Vec::new();
    // Which slots hold a frame `show_existing_frame` may show (6.8.2), as
    // the stream's frames leave them.
    let mut showable = [false; 8];
    for index in 0..rng.range_u32(1, 10) {
        let mut parts = Vec::new();
        if index == 0 || rng.one_in(16) {
            if index > 0 && noise.strays(rng) {
                s = sequence(rng, noise);
            }
            parts.push(sequence_header(s));
        }
        // One shown frame a temporal unit, the last, with the frames before
        // it not shown (7.5), unless it strays; the key frame alone.
        let frames = if index == 0 && !noise.strays(rng) {
            1
        } else {
            1 + rng.count(2)
        };
        for at in 0..frames {
            let last = at + 1 == frames;
            let strays = noise.strays(rng);
            if index > 0 && (last || strays) && rng.one_in(8) {
                let shown: Vec<u8> = (0..8).filter(|&k| showable[usize::from(k)]).collect();
                if strays || !shown.is_empty() {
                    let slot = if strays || shown.is_empty() {
                        rng.below(8) as u8
                    } else {
                        *rng.pick(&shown)
                    };
                    parts.push(show_existing(slot));
                    continue;
                }
            }
            let mut f = frame_of(rng, noise, index);
            if !last && !strays && f.show_frame {
                f.show_frame = false;
                f.showable_frame = rng.bool();
            }
            let can_show = if f.show_frame {
                f.frame_type != FrameType::Key
            } else {
                f.showable_frame
            };
            for (k, slot) in showable.iter_mut().enumerate() {
                if f.refresh & (1 << k) != 0 {
                    *slot = can_show;
                }
            }
            let tiles = if noise.strays(rng) {
                rng.range(1, 300) as usize
            } else {
                tiles_of(s)
            };
            let data = tile_data(rng, tiles);
            if tiles == 1 && rng.one_in(4) {
                // The header in an OBU of its own, maybe twice, then the
                // tile group.
                let mut b = Bits::default();
                uncompressed_header(&mut b, &f, s);
                let header = obu(OBU_FRAME_HEADER, &b.trailing());
                parts.push(header.clone());
                if rng.bool() {
                    parts.push(header);
                }
                parts.push(obu(OBU_TILE_GROUP, &data));
            } else {
                parts.push(frame_obu(&f, s, &data));
            }
        }
        if rng.one_in(10) {
            // Metadata, padding, a tile list, a reserved type.
            let kind = *rng.pick(&[5u8, 15, 8, 0, 9]);
            let payload = rng.some_bytes(16);
            parts.push(obu(kind, &payload));
        }
        if noise.strays(rng) {
            let at = rng.below(parts.len());
            let donor = parts[at].clone();
            maybe_mutate(rng, &mut parts[at], &donor, 1);
        }
        let mut unit = temporal_unit(&parts);
        if noise.strays(rng) && rng.one_in(3) {
            let donor = unit.clone();
            let rounds = 1 + rng.count(6);
            mutate_rounds(rng, &mut unit, &donor, rounds);
        }
        units.push(unit);
    }
    (first, units)
}

/// A stream through the decoder, from an `av1C` record now and then,
/// reset now and then.
fn stream_case(rng: &mut Rng) {
    let noise = Noise::of(rng);
    let (s, units) = stream(rng, noise);
    let accelerator = Checking {
        failing: rng.next_u64(),
        ..Checking::default()
    };
    let mut decoder = if rng.one_in(4) {
        let record = record(rng, s);
        match Decoder::from_av1c(accelerator, &record) {
            Ok(decoder) => decoder,
            Err(_) => return,
        }
    } else {
        Decoder::new(accelerator)
    };
    for (id, unit) in units.iter().enumerate() {
        let _ = decoder.decode(unit, id as u64);
        if rng.one_in(20) {
            decoder.reset();
        }
    }
}

/// An `av1C` record with the sequence header among its config OBUs.
fn record(rng: &mut Rng, s: Sequence) -> Vec<u8> {
    let mut record = vec![0x81, 0x08, if s.bit_depth_10 { 0x4c } else { 0x0c }, 0];
    if rng.below(4) != 0 {
        record.extend(sequence_header(s));
    }
    if rng.one_in(3) {
        let donor = record.clone();
        maybe_mutate(rng, &mut record, &donor, 1);
    }
    record
}

/// `av1C` records read or are refused, and so does their sequence header.
fn record_case(rng: &mut Rng) {
    let noise = Noise::of(rng);
    let s = sequence(rng, noise);
    let bytes = record(rng, s);
    if let Ok(parsed) = av1c::ConfigurationRecord::parse(&bytes) {
        let _ = parsed.bit_depth();
        let _ = parsed.sequence_header_obu();
    }
    let _ = Decoder::from_av1c(Checking::default(), &bytes);
}
