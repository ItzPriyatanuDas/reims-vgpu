//! VP9's parsers and decoding process under seeded fuzzing, up to an
//! accelerator that checks what VA-API's is handed. The chunks are the
//! tests' own writers': a key frame, then inter and intra-only frames,
//! shown existing frames and superframes, with tile size markers written
//! for the tiles each frame has, and fields that stray as far as each
//! stream's noise says, then mutated.

use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::fuzz_support::{Rng, Target, long, maybe_mutate, mutate_rounds, quick};
use crate::vp9::decoder::check_tiles;

const STREAMS: Target = Target::new("vp9-streams");
const RECORDS: Target = Target::new("vp9-records");

/// Frames that reached the accelerator, which says how deep the inputs
/// get.
static FRAMES: AtomicU64 = AtomicU64::new(0);

#[test]
fn streams() {
    let report = quick(&STREAMS, 6000, stream_case);
    eprintln!(
        "vp9: {} frames reached the accelerator in {} streams",
        FRAMES.load(Ordering::Relaxed),
        report.inputs
    );
}

#[test]
#[ignore = "long: RVGV_FUZZ_CASES, RVGV_FUZZ_SECONDS, RVGV_FUZZ_SEED"]
fn streams_long() {
    let report = long(&STREAMS, stream_case);
    eprintln!(
        "vp9: {} frames reached the accelerator in {} streams",
        FRAMES.load(Ordering::Relaxed),
        report.inputs
    );
}

#[test]
fn records() {
    quick(&RECORDS, 4000, record_case);
}

#[test]
#[ignore = "long: RVGV_FUZZ_CASES, RVGV_FUZZ_SECONDS, RVGV_FUZZ_SEED"]
fn records_long() {
    long(&RECORDS, record_case);
}

/// An accelerator that checks what the decoder gives it against what
/// VA-API's takes: a supported format, a picture of the frame's size, and
/// tiles where their markers say. It fails one call in 64, as a driver
/// may.
#[derive(Default)]
struct Checking {
    next: u32,
    configured: bool,
    sizes: Vec<(u32, u32)>,
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

impl Accelerator for Checking {
    type Picture = u32;
    type Error = Error;

    fn configure(&mut self, format: &StreamFormat, width: u32, height: u32) -> Result<(), Error> {
        assert!(matches!(format.profile, 0 | 2) && matches!(format.bit_depth, 8 | 10));
        assert!((1..=65536).contains(&width) && (1..=65536).contains(&height));
        self.configured = !self.fails();
        if self.configured {
            Ok(())
        } else {
            Err(Error::Unsupported("the stand-in's configuration failure"))
        }
    }

    fn new_picture(&mut self, width: u32, height: u32) -> Result<u32, Error> {
        assert!((1..=65536).contains(&width) && (1..=65536).contains(&height));
        if self.fails() {
            return Err(Error::Unsupported("the stand-in's picture failure"));
        }
        self.next += 1;
        self.sizes.push((width, height));
        Ok(self.next)
    }

    fn decode_frame(&mut self, p: &FrameParams<'_, u32>) -> Result<(), Error> {
        assert!(self.configured, "a frame before a configuration");
        assert!(check_supported(p.header).is_ok(), "an unsupported frame");
        assert!(
            check_tiles(p.data, p.header).is_ok(),
            "tiles past their markers"
        );
        let size = self.sizes.get(*p.picture as usize - 1).copied();
        assert_eq!(
            size,
            Some((p.header.width, p.header.height)),
            "a picture of another size than its frame"
        );
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

fn size(rng: &mut Rng, noise: Noise) -> (u32, u32) {
    let side = |rng: &mut Rng| {
        if noise.strays(rng) {
            *rng.pick(&[1, 2, 8, 63, 64, 65, 4096, 8193, 65536])
        } else {
            *rng.pick(&[64, 64, 96, 128, 320, 352])
        }
    };
    (side(rng), side(rng))
}

/// The tiles the writer's `tile_info()` codes for `f`.
fn tiles_of(f: &Frame) -> usize {
    let sb64_cols = f.size.0.div_ceil(8).div_ceil(8);
    let mut min = 0;
    while (64 << min) < sb64_cols {
        min += 1;
    }
    let mut max = 1;
    while (sb64_cols >> max) >= 4 {
        max += 1;
    }
    max -= 1;
    let cols = if min >= max {
        min
    } else {
        u32::from(f.tile_cols_log2).clamp(min, max)
    };
    let rows = u32::from(f.tile_rows_log2).min(2);
    (1usize << rows) << cols
}

/// The frame, with tile data for its tiles after its compressed header:
/// each tile but the last after its 4-byte size.
fn written(rng: &mut Rng, f: &Frame, noise: Noise) -> Vec<u8> {
    let mut bytes = f.write();
    bytes.truncate(bytes.len() - 4);
    let tiles = tiles_of(f);
    for tile in 0..tiles {
        let length = if noise.strays(rng) {
            rng.below(4)
        } else {
            rng.range(1, 6) as usize
        };
        if tile + 1 < tiles {
            bytes.extend_from_slice(&(length as u32).to_be_bytes());
        }
        bytes.extend(rng.bytes(length));
    }
    bytes
}

fn frame_of(rng: &mut Rng, noise: Noise, index: u32, profile: u8, size: (u32, u32)) -> Frame {
    let mut f = if index == 0 {
        Frame::key(size.0, size.1)
    } else {
        match rng.below(10) {
            0 => Frame::key(size.0, size.1),
            1 => Frame::intra_only(size.0, size.1),
            _ => {
                let mut f = Frame::inter(size.0, size.1);
                f.refs = [0; 3].map(|_| rng.below(8) as u8);
                f.refresh = rng.next_u32() as u8;
                f.sign_bias = [rng.bool(), rng.bool(), rng.bool()];
                f.size_from_ref = if rng.one_in(4) {
                    None
                } else {
                    Some(rng.below(3))
                };
                f.filter = rng.bool().then(|| rng.below(4) as u8);
                f
            }
        }
    };
    f.profile = profile;
    f.ten_or_twelve = profile >= 2 && rng.bool();
    f.show_frame = f.kind == Kind::Key || !rng.one_in(5);
    if f.kind == Kind::IntraOnly {
        f.show_frame = false;
    }
    f.error_resilient = rng.one_in(6);
    f.refresh_context = rng.bool();
    f.parallel = rng.bool();
    f.context_idx = rng.below(4) as u8;
    f.lf_level = rng.below(64) as u8;
    f.sharpness = rng.below(8) as u8;
    f.lf_delta_enabled = rng.bool();
    if f.lf_delta_enabled && rng.bool() {
        let delta = |rng: &mut Rng| rng.bool().then(|| rng.below(127) as i32 - 63);
        f.lf_update = Some(([0; 4].map(|_| delta(rng)), [0; 2].map(|_| delta(rng))));
    }
    f.base_q = rng.below(256) as u8;
    if rng.one_in(4) {
        f.delta_q = [0; 3].map(|_| rng.below(31) as i32 - 15);
    }
    if rng.one_in(4) {
        let probability = |rng: &mut Rng| rng.bool().then(|| rng.below(256) as u8);
        let update_map = rng.bool().then(|| {
            (
                [0; 7].map(|_| probability(rng)),
                rng.bool().then(|| [0; 3].map(|_| probability(rng))),
            )
        });
        let update_data = rng.bool().then(|| {
            let features = [0; 8].map(|_| {
                [
                    rng.bool().then(|| rng.below(511) as i32 - 255),
                    rng.bool().then(|| rng.below(127) as i32 - 63),
                    rng.one_in(4).then(|| rng.below(4) as i32),
                    rng.one_in(6).then_some(0),
                ]
            });
            (rng.bool(), features)
        });
        f.seg = Some(SegWrite {
            update_map,
            update_data,
        });
    }
    f.tile_cols_log2 = rng.below(7) as u8;
    f.tile_rows_log2 = rng.below(3) as u8;
    if rng.one_in(6) {
        f.render = Some((rng.range_u32(1, 2000), rng.range_u32(1, 2000)));
    }
    if noise.strays(rng) {
        f.profile = rng.below(4) as u8;
    }
    if noise.strays(rng) {
        f.color_space = rng.below(8) as u8;
        f.full_range = rng.bool();
        f.subsampling = (rng.bool(), rng.bool());
    }
    if noise.strays(rng) {
        f.reset_frame_context = rng.below(4) as u8;
    }
    if noise.strays(rng) {
        f.header_size = if rng.bool() {
            0
        } else {
            rng.range(1, 64) as u16
        };
    }
    f
}

/// A stream of chunks: a key frame, then frames, shown existing frames and
/// superframes, mutated as its noise says.
fn stream(rng: &mut Rng, noise: Noise) -> Vec<Vec<u8>> {
    let profile = if rng.bool() { 0 } else { 2 };
    let mut size = size(rng, noise);
    let mut chunks = Vec::new();
    for index in 0..rng.range_u32(1, 12) {
        if index > 0 && rng.one_in(8) {
            size = self::size(rng, noise);
        }
        let mut frames = Vec::new();
        for _ in 0..if rng.one_in(4) { rng.range(2, 4) } else { 1 } {
            if index > 0 && rng.one_in(8) {
                frames.push(show_existing(profile, rng.below(8) as u8));
            } else {
                let f = frame_of(rng, noise, index, profile, size);
                frames.push(written(rng, &f, noise));
            }
        }
        if noise.strays(rng) {
            let at = rng.below(frames.len());
            let donor = frames[at].clone();
            maybe_mutate(rng, &mut frames[at], &donor, 1);
        }
        let mut chunk = if frames.len() > 1 || rng.one_in(8) {
            let most = frames.iter().map(Vec::len).max().unwrap_or(0);
            let size_bytes = if most > 0xff_ffff {
                4
            } else if most > 0xffff {
                3
            } else if most > 0xff {
                2
            } else {
                rng.range(1, 4) as usize
            };
            superframe(&frames, size_bytes)
        } else {
            frames.concat()
        };
        if noise.strays(rng) && rng.one_in(3) {
            let donor = chunk.clone();
            let rounds = 1 + rng.count(6);
            mutate_rounds(rng, &mut chunk, &donor, rounds);
        }
        chunks.push(chunk);
    }
    chunks
}

/// A stream through the decoder, reset now and then.
fn stream_case(rng: &mut Rng) {
    let noise = Noise::of(rng);
    let chunks = stream(rng, noise);
    let accelerator = Checking {
        failing: rng.next_u64(),
        ..Checking::default()
    };
    let mut decoder = Decoder::new(accelerator);
    for (id, chunk) in chunks.iter().enumerate() {
        let _ = decoder.decode(chunk, id as u64);
        if rng.one_in(20) {
            decoder.reset();
        }
    }
}

/// `vpcC` records, of either version, read or are refused, and so do
/// superframe indexes of any bytes.
fn record_case(rng: &mut Rng) {
    let mut record = vec![rng.below(2) as u8, 0, 0, 0];
    record.extend_from_slice(&[
        rng.below(4) as u8,
        rng.below(64) as u8,
        (rng.below(16) as u8) << 4 | (rng.below(8) as u8) << 1 | u8::from(rng.bool()),
        rng.below(256) as u8,
        rng.below(256) as u8,
        rng.below(256) as u8,
    ]);
    let initialization = rng.some_bytes(8);
    record.extend_from_slice(&(initialization.len() as u16).to_be_bytes());
    record.extend(initialization);
    let donor = record.clone();
    maybe_mutate(rng, &mut record, &donor, 2);
    let _ = vpcc::ConfigurationRecord::parse(&record);
    let frames: Vec<Vec<u8>> = (0..rng.range(1, 8)).map(|_| rng.some_bytes(40)).collect();
    let size_bytes = rng.range(1, 4) as usize;
    let mut chunk = superframe(&frames, size_bytes);
    let donor = chunk.clone();
    maybe_mutate(rng, &mut chunk, &donor, 3);
    let _ = super::superframe::frames(&chunk);
}

/// An accelerator that refuses to configure for frames wider than 64
/// and, like VA-API's, keeps nothing of its last configuration once it
/// refuses one: it has no picture to give until it is configured again.
#[derive(Default)]
struct RefusingWide {
    configured: bool,
    configurations: usize,
}

impl Accelerator for RefusingWide {
    type Picture = u32;
    type Error = Error;

    fn configure(&mut self, _format: &StreamFormat, width: u32, _height: u32) -> Result<(), Error> {
        self.configured = width <= 64;
        if !self.configured {
            return Err(Error::Unsupported("wider than 64"));
        }
        self.configurations += 1;
        Ok(())
    }

    fn new_picture(&mut self, _width: u32, _height: u32) -> Result<u32, Error> {
        if self.configured {
            Ok(0)
        } else {
            Err(Error::Unsupported("no configuration"))
        }
    }

    fn decode_frame(&mut self, _p: &FrameParams<'_, u32>) -> Result<(), Error> {
        Ok(())
    }
}

/// A key frame of the size the decoder had before a refused
/// configuration configures the accelerator again.
#[test]
fn a_refused_configuration_is_asked_for_again() -> Result<(), Error> {
    let mut d = Decoder::new(RefusingWide::default());
    d.decode(&Frame::key(64, 64).write(), 0)?;
    assert!(d.decode(&Frame::key(128, 64).write(), 1).is_err());
    assert_eq!(d.decode(&Frame::key(64, 64).write(), 2)?.len(), 1);
    assert_eq!(d.accelerator().configurations, 2);
    Ok(())
}
