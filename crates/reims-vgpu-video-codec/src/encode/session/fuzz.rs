//! The session encoder under seeded fuzzing: settings of every value for
//! every property the guest sets, at open and while it runs; sizes at and
//! past the driver's limits; frames of both depths with time stamps of any
//! kind; flushes. It runs on the tests' fake accelerator, which reaches no
//! driver, and which now and then fails a picture. Every frame the session
//! takes must come out once, as a sample or failed, by the last flush; a
//! frame it did not take, never.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use reims_vgpu_video_core::plist;
use reims_vgpu_video_core::surfaces::PlaneSource;

use super::*;
use crate::encode::encoder::tests::{Fake, limits};
use crate::fuzz_support::{Rng, Target, long, quick};

const SESSIONS: Target = Target::new("encode-sessions");

/// Frames that came out, which says how deep the inputs get, and those of
/// them that came out failed.
static ENCODED: AtomicU64 = AtomicU64::new(0);
static FAILED: AtomicU64 = AtomicU64::new(0);

#[test]
fn sessions() {
    let report = quick(&SESSIONS, 1200, session_case);
    eprintln!(
        "encode: {} frames out, {} of them failed, of {} sessions",
        ENCODED.load(Ordering::Relaxed),
        FAILED.load(Ordering::Relaxed),
        report.inputs
    );
}

#[test]
#[ignore = "long: RVGV_FUZZ_CASES, RVGV_FUZZ_SECONDS, RVGV_FUZZ_SEED"]
fn sessions_long() {
    let report = long(&SESSIONS, session_case);
    eprintln!(
        "encode: {} frames out, {} of them failed, of {} sessions",
        ENCODED.load(Ordering::Relaxed),
        FAILED.load(Ordering::Relaxed),
        report.inputs
    );
}

/// `value`, two times in three.
fn maybe<T>(rng: &mut Rng, value: impl FnOnce(&mut Rng) -> T) -> Option<T> {
    if rng.one_in(3) {
        None
    } else {
        Some(value(rng))
    }
}

fn real(rng: &mut Rng) -> f64 {
    *rng.pick(&[
        0.0,
        -1.0,
        1e-9,
        0.001,
        0.5,
        1.0,
        2.0,
        23.976,
        29.97,
        60.0,
        1e9,
        1e300,
        f64::MAX,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ])
}

fn rate(rng: &mut Rng) -> u32 {
    if rng.one_in(4) {
        rng.interesting_u32()
    } else {
        *rng.pick(&[0, 1, 1000, 500_000, 2_000_000, 50_000_000, u32::MAX])
    }
}

/// The settings a guest's properties make: every value of each, as core's
/// session reads them, and the ends of their types.
fn settings(rng: &mut Rng, codec: core_backend::Codec) -> guest::EncoderSettings {
    let hevc = codec == core_backend::Codec::Hevc;
    guest::EncoderSettings {
        profile: match rng.below(8) {
            0..4 => None,
            4 => Some(rng.interesting_u32()),
            _ if hevc => Some(*rng.pick(&[1, 2])),
            _ => Some(*rng.pick(&[66, 77, 100])),
        },
        level: match rng.below(8) {
            0..5 => None,
            5 => Some(rng.interesting_u32()),
            _ if hevc => Some(*rng.pick(&[30, 60, 90, 93, 120, 123, 150, 153, 156, 186])),
            _ => Some(*rng.pick(&[10, 13, 30, 31, 32, 40, 41, 42, 50, 51, 52, 62])),
        },
        average_bit_rate: maybe(rng, rate),
        max_bit_rate: maybe(rng, rate),
        quality: maybe(rng, |rng| real(rng) as f32),
        max_key_frame_interval: maybe(rng, |rng| *rng.pick(&[0, 1, 2, 30, 600, u32::MAX])),
        max_key_frame_interval_duration: maybe(rng, real),
        expected_frame_rate: maybe(rng, real),
        allow_frame_reordering: rng.bool(),
        real_time: rng.bool(),
        entropy: maybe(rng, |rng| {
            *rng.pick(&[EntropyMode::Cavlc, EntropyMode::Cabac])
        }),
        colour: guest::ColourDescription {
            primaries: maybe(rng, |rng| *rng.pick(&[0, 1, 2, 5, 6, 9, 11, 12, 22, 255])),
            transfer: maybe(rng, |rng| *rng.pick(&[0, 1, 2, 7, 8, 13, 14, 16, 18, 255])),
            matrix: maybe(rng, |rng| *rng.pick(&[0, 1, 2, 6, 7, 9, 14, 255])),
        },
        properties: (0..rng.count(3))
            .map(|_| {
                let key =
                    (*rng.pick(&["MaximizePowerEfficiency", "MultiPassStorage", "x"])).to_owned();
                let value = match rng.below(3) {
                    0 => plist::Value::Boolean(rng.bool()),
                    1 => plist::Value::Integer(i64::from(rng.interesting_i32()).into()),
                    _ => plist::Value::Real(real(rng)),
                };
                (key, value)
            })
            .collect(),
    }
}

fn side(rng: &mut Rng) -> u32 {
    if rng.one_in(8) {
        *rng.pick(&[0, 1, 2, 127, 128, 129, 4095, 4096, 4097, 16384, u32::MAX])
    } else {
        rng.range_u32(64, 200) * 2
    }
}

const PROFILES: [Profile; 5] = [
    Profile::H264ConstrainedBaseline,
    Profile::H264Main,
    Profile::H264High,
    Profile::HevcMain,
    Profile::HevcMain10,
];

fn time(rng: &mut Rng, n: i64) -> MediaTime {
    if rng.one_in(8) {
        MediaTime {
            value: rng.interesting_u64().cast_signed(),
            timescale: *rng.pick(&[0, 1, -1, 600, 90_000, i32::MAX, i32::MIN]),
            flags: *rng.pick(&[0, MediaTime::FLAG_VALID, u32::MAX]),
            epoch: rng.interesting_u64().cast_signed(),
        }
    } else {
        MediaTime {
            value: n * 20,
            timescale: 600,
            flags: MediaTime::FLAG_VALID,
            epoch: 0,
        }
    }
}

/// One session: opened on any settings and size, then frames, new
/// settings and flushes in any order.
fn session_case(rng: &mut Rng) {
    let codec = match rng.below(16) {
        0 => core_backend::Codec::Av1,
        1 => core_backend::Codec::Vp9,
        2..9 => core_backend::Codec::H264,
        _ => core_backend::Codec::Hevc,
    };
    let config = EncoderConfig {
        codec,
        width: side(rng),
        height: side(rng),
        settings: settings(rng, codec),
    };
    let offered: Vec<Profile> = PROFILES
        .iter()
        .copied()
        .filter(|_| !rng.one_in(8))
        .collect();
    // Each stream's picture of this number fails, in one session of three.
    let failing = (!rng.one_in(3)).then(|| rng.below(8));
    let session = SessionEncoder::new(
        &config,
        offered,
        Box::new(move |settings: &EncoderSettings| {
            let fake = failing.map_or_else(Fake::default, Fake::failing_at);
            Encoder::new(fake, settings.clone(), &limits()).map_err(BackendFailure::into_backend)
        }),
    );
    let mut session = match session {
        Ok(session) => session,
        Err(error) => {
            if std::env::var_os("RVGV_FUZZ_TRACE").is_some() {
                eprintln!("refused: {error}");
            }
            return;
        }
    };
    let (width, height) = (config.width as usize, config.height as usize);
    // Frames past a megapixel are opened and set, but not encoded.
    let frames = width * height <= 1 << 20;
    // The frames taken and not yet out, by their IDs.
    let mut owed: HashMap<i64, usize> = HashMap::new();
    let mut n = 0i64;
    for _ in 0..rng.range(1, 16) {
        let out = match rng.below(10) {
            0..6 if frames => {
                n += 1;
                let frame_id = if rng.one_in(8) {
                    rng.interesting_u64().cast_signed()
                } else {
                    n
                };
                let format = *rng.pick(&[
                    PixelFormat::Nv12VideoRange,
                    PixelFormat::Nv12VideoRange,
                    PixelFormat::Nv12FullRange,
                    PixelFormat::P010VideoRange,
                    PixelFormat::P010FullRange,
                ]);
                let depth = format.bytes_per_sample();
                let pad = if rng.one_in(4) { 64 } else { 0 };
                let (luma_row, chroma_row) =
                    (width * depth + pad, width.div_ceil(2) * 2 * depth + pad);
                let luma = vec![0x40; luma_row * height];
                let chroma = vec![0x80; chroma_row * height.div_ceil(2)];
                let planes = [
                    PlaneSource {
                        bytes: &luma,
                        bytes_per_row: luma_row,
                        rows: config.height,
                    },
                    PlaneSource {
                        bytes: &chroma,
                        bytes_per_row: chroma_row,
                        rows: config.height.div_ceil(2),
                    },
                ];
                let presentation_time = time(rng, n);
                let duration = time(rng, 1);
                let out = session.encode(&SourceFrame {
                    frame_id,
                    presentation_time,
                    duration,
                    force_key_frame: rng.one_in(6),
                    pixel_format: format,
                    width: config.width,
                    height: config.height,
                    planes: &planes,
                });
                if out.is_ok() {
                    *owed.entry(frame_id).or_default() += 1;
                }
                out
            }
            6 | 7 => {
                let settings = settings(rng, codec);
                session.reconfigure(&settings).map(|()| Vec::new())
            }
            8 => session.flush(),
            _ => {
                let _ = session.configuration();
                Ok(Vec::new())
            }
        };
        came_out(&mut owed, out.unwrap_or_default());
    }
    came_out(&mut owed, session.flush().unwrap_or_default());
    owed.retain(|_, count| *count > 0);
    assert!(
        owed.is_empty(),
        "frames taken and never out: {:?}",
        owed.keys().collect::<Vec<_>>()
    );
}

/// Counts out each frame, which must be one taken and owed.
fn came_out(owed: &mut HashMap<i64, usize>, frames: Vec<EncodedFrame>) {
    for frame in frames {
        let count = owed.get_mut(&frame.frame_id);
        assert!(
            count.as_ref().is_some_and(|count| **count > 0),
            "frame {} came out, which was not taken or came out already",
            frame.frame_id
        );
        if let Some(count) = count {
            *count -= 1;
        }
        assert!(
            frame.error.is_none() || frame.data.is_empty(),
            "frame {} has a sample and an error",
            frame.frame_id
        );
        ENCODED.fetch_add(1, Ordering::Relaxed);
        if frame.error.is_some() {
            FAILED.fetch_add(1, Ordering::Relaxed);
        }
    }
}
