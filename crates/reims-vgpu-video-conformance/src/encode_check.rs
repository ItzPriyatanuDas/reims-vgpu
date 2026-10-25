//! The encode suite: frames drawn from scratch, encoded through the host
//! session on a backend's encoders ([`crate::encode`]), then checked as a
//! guest's VideoToolbox would take them, and decoded back on the same
//! backend's decoders where it has them.
//!
//! The cases ([`cases`]) are made from what the backend lists, so that
//! every backend is held to the same ones where it lists the same: for
//! each codec, each source format family with reordering and without, an
//! odd size, the full-range formats, 1920x1080, 3840x2160 (or its refusal
//! where the backend lists less), a size past what it lists, forced key
//! frames, each rate control the session offers (`AverageBitRate`,
//! `DataRateLimits`, both, `Quality`), and a bit rate and a key frame
//! interval changed by `eset` mid-stream.
//!
//! For each case it checks:
//! - every `eset` was answered 0, but where the case expects a refusal;
//! - every `efrm` was answered 0, and every frame came back once, by its
//!   ID, with `Eemt` status 0, encode info flags 0 and a sample of its own
//!   PTS, as Apple's host forwards each of its encoder's emits with its own
//!   status;
//! - every sample is in the configuration record's NAL unit framing;
//! - the first record parses, and its SPS gives the case's size, its even
//!   size below where it is odd ([`encoded_size`]), a profile the backend
//!   lists, and the source's range in `video_full_range_flag`;
//! - the times: each sample's PTS is its frame's, and each sample's DTS
//!   the PTS of the frame sent in its place, as Apple's encoders give them,
//!   so a B picture's is after its own PTS. With reordering off every
//!   sample comes out in presentation order, and so with its DTS its PTS;
//! - how far the samples are reordered, against what the SPS says a
//!   decoder must allow for (`max_num_reorder_frames`,
//!   `sps_max_num_reorder_pics`);
//! - the sync flags: the first sample is sync, a sample is sync exactly
//!   when it holds an IDR (H.264) or IRAP (HEVC) picture, and no two sync
//!   samples are further apart than the case's key frame interval;
//! - decoded back, every frame's luma and chroma PSNR against its source,
//!   the source's top left where the size is odd, is at least the case's
//!   floor; for a full-range source, the decoded luma reaches below 16
//!   and above 235, as the source does;
//! - what the case's [`Expect`] adds.
//!
//! One line a case, `PASS encode NAME` or `FAIL encode NAME`, then its
//! numbers, then the first samples' times in frames, `PTS/DTS` in decode
//! order, to compare backends by.

use std::collections::HashMap;
use std::sync::OnceLock;

use reims_vgpu_video_codec::encode::nal::length_prefixed_units;
use reims_vgpu_video_codec::{h264, hevc};
use reims_vgpu_video_core::backend::{Backend, Codec, PixelFormat};
use reims_vgpu_video_core::plist;
use reims_vgpu_video_core::status::Status;
use reims_vgpu_video_core::sync::Mutex;

use crate::decode::decode_file_keeping_pixels;
use crate::encode::{EncodeRun, Options, attachment, encode_frames};
use crate::mp4_write::{self, Sample};

/// What a case's frames show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Picture {
    /// Smooth waves that move, and a bright square that moves across them:
    /// cheap to code, so a wrong plane, stride or order shows.
    Waves,
    /// Waves twice as tall, and a white and a black square, so that the
    /// luma reaches 0 and 255 (1020 at 10 bits): what a range clamped to
    /// video's 16 to 235 would lose.
    FullSwing,
    /// Two textures that pan across each other in different directions:
    /// moving and compressible, as the rate checks need, and costlier than
    /// their targets, since a rate check on a still source checks the
    /// source.
    Texture,
    /// vtencode's frames: a diagonal ramp and a box that move, bars, and a
    /// quarter of noise that scrolls by whole pixels. The noise is dear to
    /// code within a picture and free to predict, so the key frames cost
    /// most of the bits.
    ScrollingNoise,
}

/// What a case checks beyond the common checks.
#[derive(Clone, Debug, PartialEq)]
pub enum Expect {
    /// The common checks alone.
    Plain,
    /// The bitrate over the whole case against `AverageBitRate`, and
    /// every second's against `DataRateLimits` (given as bytes a second):
    /// the case sets the ones it names, each a share of what the content
    /// costs with no target ([`cost_with_no_target`]), so that each is a
    /// target the content can reach.
    Rate {
        /// `AverageBitRate`, as a share of the cost.
        average: Option<f64>,
        /// `DataRateLimits`, as bits in any second, as a share of the cost.
        limit: Option<f64>,
    },
    /// Key frames forced on these frames (`EncoderForceKeyframe` in
    /// `efrm`'s `,fpr`): each holds an IDR or IRAP picture.
    Forced(Vec<u32>),
    /// `AverageBitRate` `before`, then `after` by `eset` before frame
    /// `at`, each a share of what the content costs with no target.
    RateChange {
        /// The frame the `eset` goes before.
        at: u32,
        /// The first bitrate, as a share of the cost.
        before: f64,
        /// The second.
        after: f64,
    },
    /// `MaxKeyFrameInterval` `before`, then `after` by `eset` before frame
    /// `at`.
    KeyIntervalChange {
        /// The frame the `eset` goes before.
        at: u32,
        /// The first interval, in frames.
        before: u32,
        /// The second.
        after: u32,
    },
    /// `esta` answers `status`, with no `Epba` before it, as Apple's host
    /// refuses a size its encoder does not take.
    Refused {
        /// The status.
        status: i32,
    },
    /// Each `eset` of the case, in order, answered with its status: the
    /// values Apple's AVE refuses with -12900 and some it takes, as it
    /// answers them.
    Answers(Vec<(String, plist::Value, i32)>),
    /// The same frames at `Quality` `low`, then `high`: the higher gives
    /// more bits and a higher PSNR, or `Quality` is refused with -12900 as
    /// VideoToolbox refuses a property its encoder lacks.
    QualityOrder {
        /// The lower quality.
        low: f64,
        /// The higher.
        high: f64,
    },
}

/// One encode the suite runs.
#[derive(Clone, Debug)]
pub struct Case {
    /// Its name, as its line prints it.
    pub name: String,
    /// The codec, profile and settings.
    pub options: Options,
    /// How many frames.
    pub frames: u64,
    /// The `MaxKeyFrameInterval` the case sets, in frames.
    pub key_frame_interval: u32,
    /// Whether the case allows frame reordering.
    pub reordering: bool,
    /// The lowest PSNR, in dB, a frame decoded back may have.
    pub min_psnr: f64,
    /// What the frames show.
    pub picture: Picture,
    /// What else it checks.
    pub expect: Expect,
}

/// The width of the suite's frames.
const WIDTH: u32 = 640;
/// Their height.
const HEIGHT: u32 = 360;
/// Frames a case encodes.
const FRAMES: u64 = 60;
/// The odd size each codec is encoded at once: a column and a row past
/// [`WIDTH`] by [`HEIGHT`].
const ODD: (u32, u32) = (WIDTH + 1, HEIGHT + 1);
/// The large sizes: 1080p for every codec, 2160p where the backend lists
/// it, with fewer frames.
const LARGE: (u32, u32) = (1920, 1080);
const UHD: (u32, u32) = (3840, 2160);
const UHD_FRAMES: u64 = 30;
/// Frames from one key frame to the next, at most.
const KEY_FRAME_INTERVAL: u32 = 30;
/// The bitrate asked for: about 0.43 bits a pixel, which these frames
/// take with room to spare.
const BIT_RATE: i64 = 3_000_000;
/// The PSNR floor for frames decoded back. A synthetic picture at this
/// rate measured well above it; a wrong plane, stride or frame order
/// falls far below.
const MIN_PSNR: f64 = 30.0;
/// The frames forced to be key frames: one in each of the first two
/// intervals, both odd, so that with reordering each would have been a B
/// picture.
const FORCED: [u32; 2] = [17, 44];
/// The rate cases: ten seconds of 720p, over which Apple's AVE meets its
/// average within 4% (docs/video/devices.md, "The encoder session"), with
/// a key frame every two seconds.
const RATE_SIZE: (u32, u32) = (1280, 720);
const RATE_FRAMES: u64 = 300;
const RATE_KEY_FRAME_INTERVAL: u32 = 60;
/// `AverageBitRate` alone, as a share of what the content costs with no
/// target. The shares stay above 0.35, since AVE's H.264 encoder came no
/// lower than 0.26 of its own choice on the texture (M3 Max, macOS 27.0.1,
/// 2026-10-24).
const AVERAGE: f64 = 0.5;
/// `DataRateLimits` alone, as bits a second.
const LIMIT: f64 = 0.5;
/// Both: the average under the limit.
const AVERAGE_UNDER_LIMIT: (f64, f64) = (0.4, 0.65);
/// The limit case on [`Picture::ScrollingNoise`]: a key frame a second, as
/// vtencode sets, so that every second holds one.
const KEY_LIMIT_INTERVAL: u32 = 30;
/// The bitrate change: from the first to the second at the frame.
const RATE_CHANGE: (u32, f64, f64) = (150, 0.35, 0.75);
/// The key frame interval change: at the frame, from the first to the
/// second, over these frames.
const KEY_INTERVAL_CHANGE: (u32, u32, u32) = (45, 60, 10);
const KEY_INTERVAL_FRAMES: u64 = 150;
/// The `Quality` pair.
const QUALITIES: (f64, f64) = (0.25, 0.75);
/// The PSNR floor of the texture's frames, which cost more than the rate
/// cases' targets on purpose.
const MIN_PSNR_TEXTURE: f64 = 25.0;
/// The PSNR floor of [`Picture::ScrollingNoise`] under its limit. Its noise
/// is a sixth of the luma, a ninth of a frame's samples, and costs more
/// than the limit allows; flattened to its mean, it alone leaves a frame
/// 21.7 dB, and the worst frames came to 21.5 dB on NVENC (RTX 5080 Laptop
/// GPU) and 21.6 dB on VA-API (radeonsi), 2026-10-24. A wrong plane, stride or
/// frame order falls far below.
const MIN_PSNR_NOISE: f64 = 20.0;
/// How far a bitrate may stray from its target: a quarter either way.
const RATE_TOLERANCE: f64 = 0.25;
/// How far one second's bits may go past `DataRateLimits`' bits: a hard
/// limit on "any contiguous segment of that duration (in decode time)", as
/// VideoToolbox's header defines it (`VTCompressionProperties.h`), with 5%
/// for the rounding of a limit to whole bytes and of a window to whole
/// frames.
const LIMIT_TOLERANCE: f64 = 0.05;
/// The least share of `DataRateLimits` a case with the limit alone makes:
/// AVE made 0.66 to 0.67 of it (M3 Max, macOS 27.0.1, 2026-10-24).
const LIMIT_FLOOR: f64 = 0.5;

/// The cases for `backend`, from what its encoders list (module doc).
pub fn cases(backend: &Backend) -> Vec<Case> {
    let Some(encoders) = &backend.encoders else {
        return Vec::new();
    };
    let mut cases = Vec::new();
    for listed in encoders.encoders() {
        let codec = match listed.codec {
            Codec::H264 => *b"avc1",
            Codec::Hevc => *b"hvc1",
            _ => continue,
        };
        let first = |ten: bool, full: bool| {
            (listed.source_formats.iter())
                .find(|f| (f.bytes_per_sample() == 2) == ten && is_full_range(**f) == full)
                .or_else(|| {
                    (!full)
                        .then(|| {
                            (listed.source_formats.iter())
                                .find(|f| (f.bytes_per_sample() == 2) == ten)
                        })
                        .flatten()
                })
                .copied()
        };
        let (eight, ten) = (first(false, false), first(true, false));
        let reorders = listed.frame_reordering;
        // Every format family and reordering at the even size, then the
        // first 8-bit format at the odd one, without reordering.
        for format in [eight, ten].into_iter().flatten() {
            if reorders {
                cases.push(case(codec, format, true, (WIDTH, HEIGHT), ""));
            }
            cases.push(case(codec, format, false, (WIDTH, HEIGHT), ""));
        }
        if let Some(format) = eight {
            cases.push(case(codec, format, false, ODD, ""));
        }
        // The full-range formats the encoder takes, of each depth.
        for format in [first(false, true), first(true, true)]
            .into_iter()
            .flatten()
        {
            if is_full_range(format) {
                let mut full = case(codec, format, false, (WIDTH, HEIGHT), "");
                full.picture = Picture::FullSwing;
                cases.push(full);
            }
        }
        let Some(eight) = eight else {
            continue;
        };
        cases.push(case(codec, eight, reorders, LARGE, ""));
        if UHD.0 <= listed.max_width && UHD.1 <= listed.max_height {
            let mut uhd = case(codec, eight, reorders, UHD, "");
            uhd.frames = UHD_FRAMES;
            cases.push(uhd);
        } else {
            cases.push(refused(codec, eight, UHD));
        }
        // A size past the largest the backend lists, as Apple's host
        // refuses one past its encoder's.
        cases.push(refused(
            codec,
            eight,
            (listed.max_width.saturating_add(2), 64),
        ));

        let mut forced = case(codec, eight, reorders, (WIDTH, HEIGHT), "-force");
        forced.options.force = FORCED.to_vec();
        forced.expect = Expect::Forced(FORCED.to_vec());
        cases.push(forced);

        for (suffix, average, limit) in [
            ("-abr", Some(AVERAGE), None),
            ("-limits", None, Some(LIMIT)),
            (
                "-abr-limits",
                Some(AVERAGE_UNDER_LIMIT.0),
                Some(AVERAGE_UNDER_LIMIT.1),
            ),
        ] {
            let mut rate = rate_case(codec, eight, suffix);
            rate.expect = Expect::Rate { average, limit };
            cases.push(rate);
        }
        // The limit alone where the key frames cost most: every second
        // holds one.
        let mut keys = rate_case(codec, eight, "-limits-keys");
        keys.picture = Picture::ScrollingNoise;
        keys.min_psnr = MIN_PSNR_NOISE;
        keys.key_frame_interval = KEY_LIMIT_INTERVAL;
        set_value(
            &mut keys.options.set,
            "MaxKeyFrameInterval",
            plist::Value::Integer(i64::from(KEY_LIMIT_INTERVAL).into()),
        );
        keys.expect = Expect::Rate {
            average: None,
            limit: Some(LIMIT),
        };
        cases.push(keys);

        let (at, before, after) = RATE_CHANGE;
        let mut change = rate_case(codec, eight, "-rate-change");
        change.expect = Expect::RateChange { at, before, after };
        cases.push(change);

        let (at, before, after) = KEY_INTERVAL_CHANGE;
        let mut keys = case(codec, eight, false, (WIDTH, HEIGHT), "-keyint-change");
        keys.frames = KEY_INTERVAL_FRAMES;
        keys.key_frame_interval = before;
        set_value(
            &mut keys.options.set,
            "MaxKeyFrameInterval",
            plist::Value::Integer(i64::from(before).into()),
        );
        keys.options.set_at.push((
            at,
            "MaxKeyFrameInterval".to_owned(),
            plist::Value::Integer(i64::from(after).into()),
        ));
        keys.expect = Expect::KeyIntervalChange { at, before, after };
        cases.push(keys);

        let mut answers = case(codec, eight, false, (WIDTH, HEIGHT), "-property-answers");
        answers.frames = 2;
        let expected = property_answers();
        answers
            .options
            .set
            .extend(expected.iter().map(|(k, v, _)| (k.clone(), v.clone())));
        answers.expect = Expect::Answers(expected);
        cases.push(answers);

        let mut quality = case(codec, eight, false, (WIDTH, HEIGHT), "-quality");
        quality
            .options
            .set
            .retain(|(key, _)| key != "AverageBitRate");
        quality.picture = Picture::Texture;
        quality.min_psnr = MIN_PSNR_TEXTURE;
        quality.expect = Expect::QualityOrder {
            low: QUALITIES.0,
            high: QUALITIES.1,
        };
        cases.push(quality);
    }
    cases
}

/// The `eset`s of the property-answers case and Apple's AVE's answer to
/// each, H.264 and HEVC alike (measured in process, M3 Max, macOS 27.0.1,
/// 2026-10-24). The last ones put the frame rate and bitrate back for the
/// frames.
fn property_answers() -> Vec<(String, plist::Value, i32)> {
    let refused = Status::PropertyNotSupported.code();
    let real = plist::Value::Real;
    let integer = |n: i64| plist::Value::Integer(n.into());
    [
        ("Quality", real(1.0001), refused),
        ("Quality", real(-0.0001), refused),
        ("Quality", real(1.0), 0),
        ("ExpectedFrameRate", real(0.99), refused),
        ("ExpectedFrameRate", integer(100_000), refused),
        ("ExpectedFrameRate", real(1000.0), 0),
        ("MaxKeyFrameIntervalDuration", real(-0.5), refused),
        ("MaxKeyFrameIntervalDuration", real(0.0), 0),
        ("AverageBitRate", integer(-1), refused),
        ("AverageBitRate", integer(2_147_483_648), refused),
        ("AverageBitRate", integer(4_294_967_295), refused),
        ("AverageBitRate", integer(4_294_967_296), 0),
        ("MaxKeyFrameInterval", integer(-1), refused),
        ("MaxKeyFrameInterval", integer(2_147_483_648), refused),
        ("MaxKeyFrameInterval", real(-0.5), 0),
        (
            "DataRateLimits",
            plist::Value::Array(vec![integer(400_000), real(1.0), integer(800_000)]),
            0,
        ),
        ("ExpectedFrameRate", integer(30), 0),
        ("AverageBitRate", integer(BIT_RATE), 0),
        (
            "MaxKeyFrameInterval",
            integer(i64::from(KEY_FRAME_INTERVAL)),
            0,
        ),
    ]
    .into_iter()
    .map(|(key, value, status)| (key.to_owned(), value, status))
    .collect()
}

/// Whether `format` is a full-range one.
fn is_full_range(format: PixelFormat) -> bool {
    matches!(
        format,
        PixelFormat::Nv12FullRange | PixelFormat::P010FullRange
    )
}

/// Whether the format named by its four bytes is a full-range one.
fn full_range_fourcc(format: [u8; 4]) -> bool {
    matches!(&format, b"420f" | b"xf20")
}

/// `DataRateLimits` for `bits` a second: bytes, then seconds.
fn limits(bits: u32) -> plist::Value {
    plist::Value::Array(vec![
        plist::Value::Integer(i64::from(bits / 8).into()),
        plist::Value::Real(1.0),
    ])
}

/// Sets `key` in `set` to `value`, in place of any value it had.
fn set_value(set: &mut Vec<(String, plist::Value)>, key: &str, value: plist::Value) {
    set.retain(|(k, _)| k != key);
    set.push((key.to_owned(), value));
}

/// A case of `codec` from `format` at `size`, at the suite's bitrate and
/// key frame interval, named by them and `suffix`.
fn case(
    codec: [u8; 4],
    format: PixelFormat,
    reordering: bool,
    (width, height): (u32, u32),
    suffix: &str,
) -> Case {
    let mut options = Options::new(codec, format.fourcc().to_be_bytes(), width, height);
    options.set = vec![
        (
            "AverageBitRate".to_owned(),
            plist::Value::Integer(BIT_RATE.into()),
        ),
        (
            "MaxKeyFrameInterval".to_owned(),
            plist::Value::Integer(i64::from(KEY_FRAME_INTERVAL).into()),
        ),
        (
            "AllowFrameReordering".to_owned(),
            plist::Value::Boolean(reordering),
        ),
    ];
    let sized = (width, height) != (WIDTH, HEIGHT);
    Case {
        name: format!(
            "{}-{}{}{}{suffix}",
            String::from_utf8_lossy(&codec),
            format.fourcc(),
            if reordering { "-reordered" } else { "" },
            if sized {
                format!("-{width}x{height}")
            } else {
                String::new()
            }
        ),
        options,
        frames: FRAMES,
        key_frame_interval: KEY_FRAME_INTERVAL,
        reordering,
        min_psnr: MIN_PSNR,
        picture: Picture::Waves,
        expect: Expect::Plain,
    }
}

/// A rate case: ten seconds of the texture at 720p, a key frame every two
/// seconds, without reordering.
fn rate_case(codec: [u8; 4], format: PixelFormat, suffix: &str) -> Case {
    let mut rate = case(codec, format, false, RATE_SIZE, suffix);
    rate.name = format!(
        "{}-{}{suffix}",
        String::from_utf8_lossy(&codec),
        format.fourcc()
    );
    rate.frames = RATE_FRAMES;
    rate.key_frame_interval = RATE_KEY_FRAME_INTERVAL;
    // The targets are set at the run, from what the content costs.
    rate.options.set.retain(|(key, _)| key != "AverageBitRate");
    set_value(
        &mut rate.options.set,
        "MaxKeyFrameInterval",
        plist::Value::Integer(i64::from(RATE_KEY_FRAME_INTERVAL).into()),
    );
    rate.picture = Picture::Texture;
    rate.min_psnr = MIN_PSNR_TEXTURE;
    rate
}

/// A case whose `esta` at `size` is to be refused with -19354 and no
/// `Epba`, as Apple's host refuses a size its encoder does not take.
fn refused(codec: [u8; 4], format: PixelFormat, size: (u32, u32)) -> Case {
    let mut refused = case(codec, format, false, size, "-refused");
    refused.frames = 1;
    refused.expect = Expect::Refused {
        status: Status::EncoderSizeUnsupported.code(),
    };
    refused
}

/// The size a stream of `width` by `height` frames comes out at: each side
/// rounded down to even, as Apple's AVE encodes an odd size and core's
/// encoder session does on every backend.
pub fn encoded_size(width: u32, height: u32) -> (u32, u32) {
    (width & !1, height & !1)
}

/// The top left `width` by `height` of `frame`, packed as
/// [`source_frame`] packs a frame of `from_width` by `from_height`, in the
/// same packing: luma, then chroma rows of a Cb/Cr pair for every two luma
/// samples.
pub fn top_left(
    frame: &[u8],
    (from_width, from_height): (u32, u32),
    (width, height): (u32, u32),
    bytes_per_sample: usize,
) -> Vec<u8> {
    let size = |v: u32| usize::try_from(v).unwrap_or(0);
    let (fw, fh, w, h) = (
        size(from_width),
        size(from_height),
        size(width),
        size(height),
    );
    let mut out = Vec::new();
    for y in 0..h {
        let start = y * fw * bytes_per_sample;
        out.extend_from_slice(
            frame
                .get(start..start + w * bytes_per_sample)
                .unwrap_or_default(),
        );
    }
    let chroma = fw * fh * bytes_per_sample;
    let (from_row, row) = (fw.div_ceil(2) * 2, w.div_ceil(2) * 2);
    for y in 0..h.div_ceil(2) {
        let start = chroma + y * from_row * bytes_per_sample;
        out.extend_from_slice(
            frame
                .get(start..start + row * bytes_per_sample)
                .unwrap_or_default(),
        );
    }
    out
}

/// Frame `index` of the suite's waves ([`Picture::Waves`]) in `format`'s
/// family, `width` by `height`: luma and interleaved chroma, smooth waves
/// that move, and a bright square that moves across them. 10-bit frames
/// are the same picture at four times the values, in the high bits of
/// little-endian words.
pub fn source_frame(format: [u8; 4], width: u32, height: u32, index: u64, frame: &mut [u8]) {
    picture_frame(Picture::Waves, format, width, height, index, frame);
}

/// Frame `index` of `picture` in `format`'s family, packed as
/// [`source_frame`] packs it.
pub fn picture_frame(
    picture: Picture,
    format: [u8; 4],
    width: u32,
    height: u32,
    index: u64,
    frame: &mut [u8],
) {
    let ten_bit = matches!(&format, b"x420" | b"xf20");
    let (w, h) = (
        usize::try_from(width).unwrap_or(0),
        usize::try_from(height).unwrap_or(0),
    );
    let n = index as f64;
    let side = 64usize;
    let moving = |dx: u64, dy: u64| {
        (
            usize::try_from(index * dx).unwrap_or(0) % w.saturating_sub(side).max(1),
            usize::try_from(index * dy).unwrap_or(0) % h.saturating_sub(side).max(1),
        )
    };
    let (box_x, box_y) = moving(5, 3);
    let (dark_x, dark_y) = moving(3, 4);
    let inside = |(bx, by): (usize, usize), x: usize, y: usize| {
        (bx..bx + side).contains(&x) && (by..by + side).contains(&y)
    };
    let mut put = |at: usize, value: f64| {
        let value = value.clamp(0.0, 255.0);
        if ten_bit {
            let word = ((value * 4.0).round() as u16) << 6;
            if let Some(bytes) = frame.get_mut(at * 2..at * 2 + 2) {
                bytes.copy_from_slice(&word.to_le_bytes());
            }
        } else if let Some(byte) = frame.get_mut(at) {
            *byte = value.round() as u8;
        }
    };
    if picture == Picture::ScrollingNoise {
        for y in 0..h {
            for x in 0..w {
                put(y * w + x, f64::from(vtencode_luma(index, x, y, w, h)));
            }
        }
        let chroma_width = w.div_ceil(2);
        for y in 0..h.div_ceil(2) {
            for x in 0..chroma_width {
                let (cb, cr) = vtencode_chroma(index, x, y, w, h);
                let at = w * h + y * chroma_width * 2 + x * 2;
                put(at, f64::from(cb));
                put(at + 1, f64::from(cr));
            }
        }
        return;
    }
    // The waves are sums of a row's and a column's terms, so each frame
    // takes a sine a column and a row, not a pixel.
    let swing = if picture == Picture::FullSwing {
        2.0
    } else {
        1.0
    };
    let columns: Vec<f64> = (0..w)
        .map(|x| 50.0 * swing * ((x as f64 + 3.0 * n) / 23.0).sin())
        .collect();
    let rows: Vec<f64> = (0..h)
        .map(|y| 30.0 * swing * ((y as f64 - 2.0 * n) / 17.0).cos())
        .collect();
    let texture = (picture == Picture::Texture).then(textures);
    let shift = |by: i64| (index as i64).wrapping_mul(by);
    for (y, row) in rows.iter().enumerate() {
        for (x, column) in columns.iter().enumerate() {
            let luma = match (picture, texture) {
                (Picture::Texture, Some([near, far])) => {
                    let a = near.at(x as i64 + shift(2), y as i64 + shift(1));
                    let b = far.at(x as i64 - shift(3), y as i64 + shift(2));
                    40.0 + 0.42 * f64::from(a) + 0.18 * f64::from(b)
                }
                (Picture::FullSwing, _) if inside((dark_x, dark_y), x, y) => 0.0,
                _ if inside((box_x, box_y), x, y) => {
                    if picture == Picture::FullSwing {
                        255.0
                    } else {
                        220.0
                    }
                }
                _ => 128.0 + column + row,
            };
            put(y * w + x, luma);
        }
    }
    let chroma_width = w.div_ceil(2);
    for y in 0..h.div_ceil(2) {
        for x in 0..chroma_width {
            let (cx, cy) = (x as f64 * 2.0, y as f64 * 2.0);
            let at = w * h + y * chroma_width * 2 + x * 2;
            let (cb, cr) = match texture {
                Some([near, far]) => (
                    96.0 + f64::from(near.at(2 * x as i64 + shift(2), 2 * y as i64 + shift(1)))
                        / 4.0,
                    96.0 + f64::from(far.at(2 * x as i64 - shift(3), 2 * y as i64 + shift(2)))
                        / 4.0,
                ),
                None => (
                    128.0 + 40.0 * swing * ((cx + cy + n) / 31.0).sin(),
                    128.0 + 40.0 * swing * ((cx - cy) / 29.0).cos(),
                ),
            };
            put(at, cb);
            put(at + 1, cr);
        }
    }
}

/// vtencode's fixed noise at (`x`, `y`), in its 32-bit arithmetic.
fn vtencode_noise(x: u32, y: u32) -> u8 {
    let mut v = x.wrapping_mul(0x9e37_79b1) ^ y.wrapping_mul(0x85eb_ca77);
    v ^= v >> 15;
    v = v.wrapping_mul(0x2c1b_3c6d);
    v ^= v >> 12;
    (v & 0xff) as u8
}

/// Luma of vtencode's frame `index` at (`x`, `y`) of `w` by `h`
/// (`lumaAt`): eight bars in the lower third, the noise scrolling 3 pixels
/// a frame in the top right, a box of 235 crossing at a quarter of the
/// height, and a diagonal ramp that moves 4 a frame.
fn vtencode_luma(index: u64, x: usize, y: usize, w: usize, h: usize) -> u8 {
    // vtencode's ints: its frame counts and sizes stay far below 2^31.
    let (i, x, y, w, h) = (index as i64, x as i64, y as i64, w as i64, h as i64);
    if y > h * 2 / 3 {
        return (16 + 219 * ((x * 8 / w) % 8) / 7) as u8;
    }
    if x >= w / 2 && y < h / 3 {
        let noise = i64::from(vtencode_noise((x + 3 * i) as u32, y as u32));
        return (16 + noise * 219 / 255) as u8;
    }
    let box_x = (i * 7) % if w > 64 { w - 64 } else { 1 };
    let box_y = h / 4;
    if (box_x..box_x + 64).contains(&x) && (box_y..box_y + 64).contains(&y) {
        return 235;
    }
    (16 + (x + y + i * 4) % 220) as u8
}

/// Cb and Cr of vtencode's frame `index` at chroma sample (`x`, `y`)
/// (`chromaAt`): a slow gradient that drifts with the frame.
fn vtencode_chroma(index: u64, x: usize, y: usize, w: usize, h: usize) -> (u8, u8) {
    let (i, x, y, w, h) = (index as i64, x as i64, y as i64, w as i64, h as i64);
    let half = |side: i64| if side / 2 > 0 { side / 2 } else { 1 };
    (
        (64 + (128 * x) / half(w) / 2 + i % 16) as u8,
        (64 + (128 * y) / half(h) / 2 + (i / 2) % 16) as u8,
    )
}

/// A square of value noise that tiles: random values on a lattice,
/// smoothly interpolated.
struct Tile {
    values: Vec<u8>,
}

/// A tile's side, a power of two.
const TILE: usize = 256;

impl Tile {
    /// Noise whose lattice is `step` pixels apart (a divisor of [`TILE`]),
    /// from `seed`.
    fn new(step: usize, seed: u64) -> Self {
        let cells = TILE / step;
        let lattice = |cx: usize, cy: usize| -> f64 {
            // SplitMix64 of the cell and the seed: the same tile everywhere.
            let mut z = seed
                .wrapping_add(((cy % cells) * cells + cx % cells) as u64)
                .wrapping_mul(0x9e37_79b9_7f4a_7c15);
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            ((z ^ (z >> 31)) >> 56) as f64
        };
        let smooth = |t: f64| t * t * (3.0 - 2.0 * t);
        let mut values = vec![0; TILE * TILE];
        for y in 0..TILE {
            for x in 0..TILE {
                let (cx, cy) = (x / step, y / step);
                let fx = smooth((x % step) as f64 / step as f64);
                let fy = smooth((y % step) as f64 / step as f64);
                let top = lattice(cx, cy) * (1.0 - fx) + lattice(cx + 1, cy) * fx;
                let bottom = lattice(cx, cy + 1) * (1.0 - fx) + lattice(cx + 1, cy + 1) * fx;
                values[y * TILE + x] = (top * (1.0 - fy) + bottom * fy).round() as u8;
            }
        }
        Self { values }
    }

    /// The value at (`x`, `y`), the tile repeated across the plane.
    fn at(&self, x: i64, y: i64) -> u8 {
        let wrap = |v: i64| (v.rem_euclid(TILE as i64)) as usize;
        self.values[wrap(y) * TILE + wrap(x)]
    }
}

/// The texture's two tiles, made once.
fn textures() -> [&'static Tile; 2] {
    static TILES: OnceLock<[Tile; 2]> = OnceLock::new();
    let [near, far] = TILES.get_or_init(|| [Tile::new(32, 1), Tile::new(16, 2)]);
    [near, far]
}

/// Whether a sample of `codec` holds a picture a decoder can start at: an
/// IDR slice (H.264 `nal_unit_type` 5) or an IRAP one (HEVC 16 to 23).
pub fn starts_a_stream(codec: [u8; 4], sample: &[u8]) -> bool {
    let Some(units) = length_prefixed_units(sample) else {
        return false;
    };
    units.iter().any(|unit| match (&codec, unit.first()) {
        (b"avc1", Some(&header)) => header & 0x1f == 5,
        (b"hvc1", Some(&header)) => (16..=23).contains(&((header >> 1) & 0x3f)),
        _ => false,
    })
}

/// The PSNR in dB of `got` against `want`, two planes' samples packed alike,
/// of `bytes_per_sample` each: 8-bit samples against 255, and P010's
/// 16-bit words, little-endian, by their high 10 bits against 1023. Equal
/// planes are infinite.
pub fn psnr(got: &[u8], want: &[u8], bytes_per_sample: usize) -> f64 {
    let (mut squared, mut count) = (0f64, 0usize);
    let peak = if bytes_per_sample == 2 { 1023.0 } else { 255.0 };
    if bytes_per_sample == 2 {
        for (a, b) in got.as_chunks::<2>().0.iter().zip(want.as_chunks::<2>().0) {
            let a = f64::from(u16::from_le_bytes(*a) >> 6);
            let b = f64::from(u16::from_le_bytes(*b) >> 6);
            squared += (a - b) * (a - b);
            count += 1;
        }
    } else {
        for (&a, &b) in got.iter().zip(want) {
            let d = f64::from(a) - f64::from(b);
            squared += d * d;
            count += 1;
        }
    }
    if count == 0 || got.len() != want.len() {
        return 0.0;
    }
    if squared == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (peak * peak / (squared / count as f64)).log10()
}

/// One sample as the checks read it.
struct Got {
    pts: i64,
    dts: i64,
    sync: bool,
    data: Vec<u8>,
}

/// What the first configuration record's SPS says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StreamFacts {
    profile: u32,
    size: (u32, u32),
    full_range: bool,
    /// The most frames a decoder must hold back to put them in order, as
    /// the SPS says it, if it says.
    max_reorder: Option<u32>,
}

/// What one case's encode gave, checked.
struct Outcome {
    problems: Vec<String>,
    numbers: String,
    times: String,
    bytes: usize,
    mean_psnr: Option<f64>,
    /// `Quality`'s `eset` status, for the quality pair.
    quality_status: Option<i32>,
}

/// Runs `case` on `backend`: a line that starts `PASS` or `FAIL`, and
/// whether it passed.
pub fn check(backend: &Backend, case: &Case) -> (bool, String) {
    match &case.expect {
        Expect::QualityOrder { low, high } => check_quality_order(backend, case, *low, *high),
        _ => {
            let outcome = run(backend, case);
            line(case, &outcome)
        }
    }
}

/// The case's line from its outcome.
fn line(case: &Case, outcome: &Outcome) -> (bool, String) {
    let Outcome {
        problems,
        numbers,
        times,
        ..
    } = outcome;
    if problems.is_empty() {
        (
            true,
            format!("PASS encode {}: {numbers}\n     {times}", case.name),
        )
    } else {
        (
            false,
            format!(
                "FAIL encode {}: {}\n     {numbers}\n     {times}",
                case.name,
                problems.join("; ")
            ),
        )
    }
}

/// The quality pair: the same case at each quality, then their order.
fn check_quality_order(backend: &Backend, case: &Case, low: f64, high: f64) -> (bool, String) {
    let at = |quality: f64| {
        let mut case = case.clone();
        set_value(
            &mut case.options.set,
            "Quality",
            plist::Value::Real(quality),
        );
        run(backend, &case)
    };
    let (lower, higher) = (at(low), at(high));
    let mut problems: Vec<String> = (lower.problems.iter())
        .map(|p| format!("at {low}: {p}"))
        .chain(higher.problems.iter().map(|p| format!("at {high}: {p}")))
        .collect();
    let refused = Status::PropertyNotSupported.code();
    let order = match (lower.quality_status, higher.quality_status) {
        (Some(0), Some(0)) => {
            if higher.bytes <= lower.bytes {
                problems.push(format!(
                    "Quality {high} gave {} bytes, no more than {low}'s {}",
                    higher.bytes, lower.bytes
                ));
            }
            match (lower.mean_psnr, higher.mean_psnr) {
                (Some(l), Some(h)) if h <= l => problems.push(format!(
                    "Quality {high} decoded at {h:.1} dB, no better than {low}'s {l:.1}"
                )),
                _ => {}
            }
            format!(
                "Quality {low}: {} bytes, Quality {high}: {} bytes",
                lower.bytes, higher.bytes
            )
        }
        (Some(a), Some(b)) if a == refused && b == refused => {
            format!("Quality refused with {refused}, as the encoder lacks it")
        }
        other => {
            problems.push(format!("Quality's eset answered {other:?}"));
            String::new()
        }
    };
    let outcome = Outcome {
        problems,
        numbers: format!("{order}; {}", higher.numbers),
        times: higher.times,
        bytes: higher.bytes,
        mean_psnr: higher.mean_psnr,
        quality_status: higher.quality_status,
    };
    line(case, &outcome)
}

/// A rate case's targets, in bits a second, from what its content costs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Targets {
    /// What the content costs with no target.
    cost: f64,
    average: Option<u32>,
    limit: Option<u32>,
    /// The bitrates before and after the change.
    change: Option<(u32, u32)>,
}

/// `case` with its rate targets set, from what its content costs on
/// `backend` with none, and the targets; other cases as they are.
fn with_targets(backend: &Backend, case: &Case) -> Result<(Case, Option<Targets>), String> {
    let shares = match case.expect {
        Expect::Rate { average, limit } => (average, limit, None),
        Expect::RateChange { at, before, after } => (Some(before), None, Some((at, after))),
        _ => return Ok((case.clone(), None)),
    };
    let cost = cost_with_no_target(backend, case)?;
    let bits = |share: f64| (cost * share).round().clamp(1.0, f64::from(u32::MAX)) as u32;
    let mut case = case.clone();
    let mut targets = Targets {
        cost,
        ..Targets::default()
    };
    let (average, limit, change) = shares;
    if let Some(average) = average {
        let average = bits(average);
        targets.average = Some(average);
        set_value(
            &mut case.options.set,
            "AverageBitRate",
            plist::Value::Integer(i64::from(average).into()),
        );
    }
    if let Some(limit) = limit {
        let limit = bits(limit);
        targets.limit = Some(limit);
        set_value(&mut case.options.set, "DataRateLimits", limits(limit));
    }
    if let Some((at, after)) = change {
        let after = bits(after);
        targets.change = Some((targets.average.unwrap_or(0), after));
        targets.average = None;
        case.options.set_at.push((
            at,
            "AverageBitRate".to_owned(),
            plist::Value::Integer(i64::from(after).into()),
        ));
    }
    Ok((case, Some(targets)))
}

/// The frames of `case`, as the backend's encoders gave them back.
fn run(backend: &Backend, case: &Case) -> Outcome {
    let (case, targets) = match with_targets(backend, case) {
        Ok(found) => found,
        Err(why) => {
            return Outcome {
                problems: vec![format!("with no target: {why}")],
                numbers: String::new(),
                times: String::new(),
                bytes: 0,
                mean_psnr: None,
                quality_status: None,
            };
        }
    };
    let case = &case;
    let options = &case.options;
    let run = encode_frames(backend, options, case.frames, &mut |index, frame| {
        picture_frame(
            case.picture,
            options.format,
            options.width,
            options.height,
            index,
            frame,
        );
        Ok(())
    });
    let mut outcome = Outcome {
        problems: Vec::new(),
        numbers: String::new(),
        times: String::new(),
        bytes: 0,
        mean_psnr: None,
        quality_status: None,
    };
    let run = match run {
        Ok(run) => run,
        Err(error) => {
            outcome.problems.push(error);
            return outcome;
        }
    };
    outcome.quality_status = (run.sets.iter())
        .find(|set| set.key == "Quality")
        .map(|set| set.status);
    if let Expect::Refused { status } = case.expect {
        let asked = run.counts.get("Epba").copied().unwrap_or(0);
        if run.start_status != status {
            outcome
                .problems
                .push(format!("esta answered {}, not {status}", run.start_status));
        }
        if asked > 0 {
            outcome
                .problems
                .push(format!("{asked} Epba before the refusal"));
        }
        outcome.numbers = format!(
            "esta {}x{}: {}, {asked} Epba",
            options.width, options.height, run.start_status
        );
        return outcome;
    }
    if run.start_status != 0 {
        outcome
            .problems
            .push(format!("esta answered {}", run.start_status));
        return outcome;
    }
    let problems = &mut outcome.problems;
    if let Expect::Answers(expected) = &case.expect {
        // The case's own sets come after the ones every case makes, which
        // must be answered 0, as in every other case.
        let (common, ours) = run
            .sets
            .split_at(run.sets.len().saturating_sub(expected.len()));
        problems.extend(
            (common.iter())
                .filter(|set| set.status != 0)
                .map(|set| format!("eset {} answered {}", set.key, set.status)),
        );
        let wrong: Vec<String> = (expected.iter().zip(ours))
            .filter(|((_, _, want), got)| got.status != *want)
            .map(|((key, value, want), got)| {
                format!("eset {key} {value:?} answered {}, not {want}", got.status)
            })
            .collect();
        if ours.len() != expected.len() {
            problems.push(format!(
                "{} of {} esets answered",
                ours.len(),
                expected.len()
            ));
        }
        outcome.numbers = format!(
            "{} of {} esets answered as AVE answers them",
            expected.len() - wrong.len(),
            expected.len()
        );
        problems.extend(wrong);
        return outcome;
    }
    for set in &run.sets {
        let allowed = set.key == "Quality" && set.status == Status::PropertyNotSupported.code();
        if set.status != 0 && !(allowed && matches!(case.expect, Expect::QualityOrder { .. })) {
            problems.push(format!(
                "eset {}{} answered {}",
                set.key,
                set.frame
                    .map(|f| format!(" at frame {f}"))
                    .unwrap_or_default(),
                set.status
            ));
        }
    }
    problems.extend(check_frames(case, &run));
    let samples: Vec<Got> = (run.emitted.iter())
        .filter_map(|emitted| {
            let sample = emitted.sample.as_ref().filter(|_| emitted.status == 0)?;
            let timing = sample.timings.first()?;
            Some(Got {
                pts: timing.presentation.value,
                dts: timing.decode.value,
                sync: attachment(sample, "NotSync") != Some(true),
                data: sample.data.clone().unwrap_or_default(),
            })
        })
        .collect();
    if samples.len() as u64 != case.frames {
        problems.push(format!(
            "{} samples with an image of {} frames ({} emitted)",
            samples.len(),
            case.frames,
            run.emitted.len()
        ));
    }
    if samples
        .iter()
        .any(|s| length_prefixed_units(&s.data).is_none())
    {
        problems.push("a sample's NAL unit lengths run past its end".to_owned());
    }
    let listed: Vec<u32> = (backend.encoders.iter())
        .flat_map(|e| e.encoders())
        .filter(|e| (e.codec == Codec::H264) == (&options.codec == b"avc1"))
        .flat_map(|e| e.profiles)
        .collect();
    let facts = match run.records.first() {
        None => {
            problems.push("no configuration record".to_owned());
            None
        }
        Some(record) => match stream_facts(options.codec, record) {
            Ok(facts) => {
                problems.extend(check_record(options, &facts, &listed));
                Some(facts)
            }
            Err(why) => {
                problems.push(why);
                None
            }
        },
    };
    problems.extend(check_times(case, &samples));
    let depth = reorder_depth(&samples);
    if let Some(allowed) = facts.and_then(|f| f.max_reorder)
        && depth > allowed
    {
        problems.push(format!(
            "samples reordered by {depth}, past the SPS's {allowed}"
        ));
    }
    let syncs = sync_frames(case, &samples);
    problems.extend(check_sync(case, &samples, &syncs));
    let mut rates = String::new();
    match (&case.expect, targets) {
        (Expect::Forced(frames), _) => problems.extend(check_forced(case, &samples, frames)),
        (Expect::Rate { .. }, Some(targets)) => {
            let (found, line) = check_rate(case, &samples, &targets);
            problems.extend(found);
            rates = line;
        }
        (Expect::RateChange { at, .. }, Some(targets)) => {
            let (found, line) = check_rate_change(case, &samples, *at, &targets);
            problems.extend(found);
            rates = line;
        }
        (Expect::KeyIntervalChange { at, before, after }, _) => {
            let (found, line) = check_interval_change(&syncs, case, *at, *before, *after);
            problems.extend(found);
            rates = line;
        }
        _ => {}
    }
    let mut psnr_line = String::from("not decoded back");
    if !decodes(backend, options.codec) {
        // The pictures are checked by decoding them back: a backend with
        // no decoder of its own codec leaves them unchecked, which is no
        // pass.
        problems.push(format!(
            "no decoder of {} on the backend to check the pictures with",
            String::from_utf8_lossy(&options.codec)
        ));
    } else {
        match decode_back(backend, case, &run.records, &samples) {
            Ok(back) => {
                psnr_line = format!("PSNR worst {:.1} dB, mean {:.1} dB", back.worst, back.mean);
                outcome.mean_psnr = Some(back.mean);
                if back.worst < case.min_psnr {
                    problems.push(format!(
                        "a frame decoded back at {:.1} dB, under {:.0}",
                        back.worst, case.min_psnr
                    ));
                }
                if case.picture == Picture::FullSwing {
                    psnr_line.push_str(&format!(", luma {} to {}", back.luma.0, back.luma.1));
                    if back.luma.0 >= 16 || back.luma.1 <= 235 {
                        problems.push(format!(
                            "a full-range source decoded back with its luma from {} to {}, \
                             within video range",
                            back.luma.0, back.luma.1
                        ));
                    }
                }
            }
            Err(why) => problems.push(format!("decoded back: {why}")),
        }
    }
    let bytes: usize = samples.iter().map(|s| s.data.len()).sum();
    outcome.bytes = bytes;
    let seconds = case.frames as f64 / f64::from(options.fps.max(1));
    let shown_syncs = if syncs.len() > 12 {
        format!("{} sync", syncs.len())
    } else {
        format!(
            "sync at {}",
            syncs
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    outcome.numbers = format!(
        "{} samples, {shown_syncs}, {} records, {:.0} kbit/s{rates}, reordered by {depth}{}, {}{psnr_line}, {:.3} s",
        samples.len(),
        run.records.len(),
        bytes as f64 * 8.0 / seconds / 1000.0,
        match facts.and_then(|f| f.max_reorder) {
            Some(allowed) => format!(" (SPS {allowed})"),
            None => String::new(),
        },
        if facts.is_some_and(|f| f.full_range) {
            "full range, "
        } else {
            ""
        },
        run.elapsed
    );
    outcome.times = times_line(case, &samples);
    outcome
}

/// Every `efrm` answered 0, and every frame emitted once by its ID with
/// status 0, encode info flags 0 and a sample of its own PTS.
fn check_frames(case: &Case, run: &EncodeRun) -> Vec<String> {
    let mut problems = Vec::new();
    if run.failed > 0 {
        problems.push(format!("{} frames refused", run.failed));
    }
    let flagged = (run.replies.iter())
        .filter(|reply| reply.info_flags.unwrap_or(0) != 0)
        .count();
    if flagged > 0 {
        problems.push(format!("{flagged} efrm replies with info flags"));
    }
    let tick = case.options.tick();
    let mut seen: HashMap<i64, usize> = HashMap::new();
    let (mut failed, mut flags, mut misplaced) = (0, 0, 0);
    for emitted in &run.emitted {
        *seen.entry(emitted.frame_id).or_default() += 1;
        if emitted.status != 0 {
            failed += 1;
        }
        if emitted.info_flags.unwrap_or(0) != 0 {
            flags += 1;
        }
        let pts = (emitted.sample.as_ref())
            .and_then(|s| s.timings.first())
            .map(|t| t.presentation.value);
        if pts.is_some_and(|pts| pts != emitted.frame_id * tick) {
            misplaced += 1;
        }
    }
    let frames = i64::try_from(case.frames).unwrap_or(0);
    let missing = (0..frames).filter(|id| !seen.contains_key(id)).count();
    let twice = seen.values().filter(|&&count| count > 1).count();
    let strangers = seen
        .keys()
        .filter(|&&id| !(0..frames).contains(&id))
        .count();
    for (count, what) in [
        (failed, "emitted with an error"),
        (flags, "emitted with encode info flags"),
        (misplaced, "emitted with another frame's PTS"),
        (missing, "never emitted"),
        (twice, "emitted more than once"),
        (strangers, "emitted that were never sent"),
    ] {
        if count > 0 {
            problems.push(format!("{count} frames {what}"));
        }
    }
    problems
}

/// Whether `backend` decodes `codec`.
fn decodes(backend: &Backend, codec: [u8; 4]) -> bool {
    let want = if &codec == b"avc1" {
        Codec::H264
    } else {
        Codec::Hevc
    };
    (backend.decoders.iter())
        .flat_map(|d| d.capabilities())
        .any(|c| c.codec == want)
}

/// The facts of the first SPS of `record`, an `avcC` or `hvcC`.
fn stream_facts(codec: [u8; 4], record: &[u8]) -> Result<StreamFacts, String> {
    if &codec == b"avc1" {
        let parsed =
            h264::avcc::DecoderConfiguration::parse(record).map_err(|e| format!("avcC: {e}"))?;
        let sps = parsed
            .sequence_parameter_sets
            .first()
            .ok_or("the avcC has no SPS")?;
        let sps = h264::sps::Sps::parse(sps).map_err(|e| format!("SPS: {e}"))?;
        let (_, _, w, h) = sps.crop_rect();
        Ok(StreamFacts {
            profile: u32::from(parsed.profile_indication),
            size: (w, h),
            full_range: sps.vui.as_ref().is_some_and(|v| v.video_full_range_flag),
            max_reorder: sps.vui.as_ref().and_then(|v| v.max_num_reorder_frames),
        })
    } else {
        let parsed =
            hevc::hvcc::DecoderConfiguration::parse(record).map_err(|e| format!("hvcC: {e}"))?;
        let sps = (parsed.nal_units.iter())
            .find(|unit| {
                hevc::nal::NalUnit::new(unit)
                    .is_ok_and(|nal| nal.unit_type() == hevc::nal::nal_type::SPS)
            })
            .ok_or("the hvcC has no SPS")?;
        let sps = hevc::sps::Sps::parse(sps).map_err(|e| format!("SPS: {e}"))?;
        let (_, _, w, h) = sps.crop_rect();
        Ok(StreamFacts {
            profile: u32::from(parsed.general_profile_idc),
            size: (w, h),
            full_range: sps.vui.as_ref().is_some_and(|v| v.video_full_range_flag),
            max_reorder: (sps.sub_layer_ordering.last()).map(|o| u32::from(o.max_num_reorder_pics)),
        })
    }
}

/// The SPS gives the case's size, a listed profile and the source's range.
fn check_record(options: &Options, facts: &StreamFacts, listed: &[u32]) -> Vec<String> {
    let mut problems = Vec::new();
    let want = encoded_size(options.width, options.height);
    if facts.size != want {
        problems.push(format!(
            "the SPS is {}x{}, not {}x{}",
            facts.size.0, facts.size.1, want.0, want.1
        ));
    }
    if !listed.contains(&facts.profile) {
        problems.push(format!("profile {} is not among {listed:?}", facts.profile));
    }
    let full = full_range_fourcc(options.format);
    if facts.full_range != full {
        problems.push(format!(
            "the SPS's video_full_range_flag is {} for a {} source",
            u8::from(facts.full_range),
            String::from_utf8_lossy(&options.format)
        ));
    }
    problems
}

/// The samples' times, against the frames'.
fn check_times(case: &Case, samples: &[Got]) -> Vec<String> {
    let tick = case.options.tick();
    let mut problems = Vec::new();
    let mut presentation: Vec<i64> = samples.iter().map(|s| s.pts).collect();
    presentation.sort_unstable();
    let frames = i64::try_from(case.frames).unwrap_or(0);
    let sent: Vec<i64> = (0..frames).map(|n| n * tick).collect();
    if presentation != sent {
        problems.push("the samples' PTSes are not the frames'".to_owned());
    }
    // Apple's encoders take each sample's DTS from the session's queue of
    // the PTSes sent, so a B picture's is after its own PTS.
    if let Some(at) = (samples.iter().zip(&sent)).position(|(s, &dts)| s.dts != dts) {
        problems.push(format!(
            "sample {at}'s DTS is not the PTS of the frame sent in its place"
        ));
    }
    if !case.reordering && samples.windows(2).any(|w| w[1].pts <= w[0].pts) {
        problems.push("reordered with reordering off".to_owned());
    }
    problems
}

/// How far the samples are reordered: the most samples that come before
/// one in decode order and after it in presentation order, which is what
/// H.264's `max_num_reorder_frames` and HEVC's `sps_max_num_reorder_pics`
/// bound.
fn reorder_depth(samples: &[Got]) -> u32 {
    (samples.iter().enumerate())
        .map(|(at, sample)| {
            samples[..at]
                .iter()
                .filter(|earlier| earlier.pts > sample.pts)
                .count()
        })
        .max()
        .map_or(0, |depth| u32::try_from(depth).unwrap_or(u32::MAX))
}

/// The first samples' times in frames, `PTS/DTS` in decode order.
fn times_line(case: &Case, samples: &[Got]) -> String {
    const SHOWN: usize = 12;
    let tick = case.options.tick().max(1);
    let frames = |t: i64| {
        if t % tick == 0 {
            (t / tick).to_string()
        } else {
            format!("{:.2}", t as f64 / tick as f64)
        }
    };
    let shown: Vec<String> = (samples.iter().take(SHOWN))
        .map(|s| format!("{}/{}", frames(s.pts), frames(s.dts)))
        .collect();
    format!(
        "times (frames, PTS/DTS): {}{}",
        shown.join(" "),
        if samples.len() > SHOWN { " ..." } else { "" }
    )
}

/// The frames, by PTS, whose samples are sync, in order.
fn sync_frames(case: &Case, samples: &[Got]) -> Vec<i64> {
    let tick = case.options.tick().max(1);
    let mut syncs: Vec<i64> = (samples.iter())
        .filter(|s| s.sync)
        .map(|s| s.pts / tick)
        .collect();
    syncs.sort_unstable();
    syncs
}

/// The sync flags, against the pictures and the key frame interval. A
/// case that changes its interval has its gaps checked by
/// [`check_interval_change`] instead.
fn check_sync(case: &Case, samples: &[Got], syncs: &[i64]) -> Vec<String> {
    let mut problems = Vec::new();
    if samples.first().is_some_and(|s| !s.sync) {
        problems.push("the first sample is not sync".to_owned());
    }
    let codec = case.options.codec;
    let wrong = (samples.iter())
        .filter(|s| s.sync != starts_a_stream(codec, &s.data))
        .count();
    if wrong > 0 {
        problems.push(format!(
            "{wrong} samples' sync flags disagree with their pictures"
        ));
    }
    if matches!(case.expect, Expect::KeyIntervalChange { .. }) {
        return problems;
    }
    let frames = i64::try_from(case.frames).unwrap_or(0);
    let mut ends = syncs.to_vec();
    ends.push(frames);
    let gap = ends.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(0);
    if gap > i64::from(case.key_frame_interval) {
        problems.push(format!(
            "{gap} frames between sync samples, over {}",
            case.key_frame_interval
        ));
    }
    problems
}

/// Each forced frame's sample holds an IDR or IRAP picture, by its NAL
/// unit types.
fn check_forced(case: &Case, samples: &[Got], forced: &[u32]) -> Vec<String> {
    let tick = case.options.tick();
    let codec = case.options.codec;
    (forced.iter())
        .filter_map(|&frame| {
            let pts = i64::from(frame) * tick;
            match samples.iter().find(|s| s.pts == pts) {
                None => Some(format!("no sample of forced frame {frame}")),
                Some(s) if !starts_a_stream(codec, &s.data) => Some(format!(
                    "forced frame {frame} holds no {} picture",
                    if &codec == b"avc1" { "IDR" } else { "IRAP" }
                )),
                Some(_) => None,
            }
        })
        .collect()
}

/// The bits of the samples whose frames are in `frames`, a second's worth
/// of them a second: their bitrate.
fn rate_of(case: &Case, samples: &[Got], frames: std::ops::Range<i64>) -> f64 {
    let tick = case.options.tick().max(1);
    let bytes: usize = (samples.iter())
        .filter(|s| frames.contains(&(s.pts / tick)))
        .map(|s| s.data.len())
        .sum();
    let seconds = (frames.end - frames.start) as f64 / f64::from(case.options.fps.max(1));
    bytes as f64 * 8.0 / seconds.max(f64::MIN_POSITIVE)
}

/// The most bits in any second's run of samples, in decode order, and
/// the decode index of its first sample.
fn busiest_second(case: &Case, samples: &[Got]) -> (f64, usize) {
    let window = usize::try_from(case.options.fps.max(1)).unwrap_or(1);
    let sizes: Vec<usize> = samples.iter().map(|s| s.data.len()).collect();
    let (start, bytes) = (sizes.windows(window.min(sizes.len()).max(1)))
        .map(|w| w.iter().sum::<usize>())
        .enumerate()
        .max_by_key(|&(at, bytes)| (bytes, std::cmp::Reverse(at)))
        .unwrap_or((0, 0));
    (bytes as f64 * 8.0, start)
}

/// What the case's content costs on `backend` with no target: the same
/// frames with no bitrate and no limit, the encoder's own choice. Each
/// codec, format, picture and key frame interval's is encoded once, and
/// kept.
fn cost_with_no_target(backend: &Backend, case: &Case) -> Result<f64, String> {
    static COSTS: Mutex<Vec<(String, String, f64)>> = Mutex::new(Vec::new());
    let key = format!(
        "{}-{}-{}x{}-{}-{:?}-{}",
        String::from_utf8_lossy(&case.options.codec),
        String::from_utf8_lossy(&case.options.format),
        case.options.width,
        case.options.height,
        case.frames,
        case.picture,
        case.key_frame_interval
    );
    let backend_name = backend.name.clone();
    let known = (COSTS.lock().iter())
        .find(|(name, k, _)| *name == backend_name && *k == key)
        .map(|(_, _, cost)| *cost);
    if let Some(cost) = known {
        return Ok(cost);
    }
    let mut free = case.clone();
    free.options.set.retain(|(key, _)| {
        !matches!(
            key.as_str(),
            "AverageBitRate" | "DataRateLimits" | "Quality"
        )
    });
    free.options.set_at.clear();
    free.expect = Expect::Plain;
    let run = encode_frames(backend, &free.options, free.frames, &mut |index, frame| {
        picture_frame(
            free.picture,
            free.options.format,
            free.options.width,
            free.options.height,
            index,
            frame,
        );
        Ok(())
    })?;
    let bytes: usize = (run.emitted.iter())
        .filter_map(|e| e.sample.as_ref()?.data.as_ref().map(Vec::len))
        .sum();
    let cost = bytes as f64 * 8.0 * f64::from(free.options.fps.max(1)) / free.frames.max(1) as f64;
    COSTS.lock().push((backend_name, key, cost));
    Ok(cost)
}

/// `AverageBitRate` over the whole case, and `DataRateLimits` over every
/// second of it.
fn check_rate(case: &Case, samples: &[Got], targets: &Targets) -> (Vec<String>, String) {
    let mut problems = Vec::new();
    let frames = i64::try_from(case.frames).unwrap_or(0);
    let rate = rate_of(case, samples, 0..frames);
    let mut line = format!(", {:.0} kbit/s with no target", targets.cost / 1000.0);
    if let Some(average) = targets.average {
        let share = rate / f64::from(average);
        line.push_str(&format!(
            ", {share:.2} of AverageBitRate {} kbit/s",
            average / 1000
        ));
        if (share - 1.0).abs() > RATE_TOLERANCE {
            problems.push(format!(
                "{:.0} kbit/s, {share:.2} of AverageBitRate's {} kbit/s",
                rate / 1000.0,
                average / 1000
            ));
        }
    }
    if let Some(limit) = targets.limit {
        let (busiest, from) = busiest_second(case, samples);
        let share = busiest / f64::from(limit);
        let largest_key = (samples.iter())
            .filter(|s| s.sync)
            .map(|s| s.data.len())
            .max()
            .unwrap_or(0);
        line.push_str(&format!(
            ", busiest second {share:.2} of DataRateLimits {} kbit (from sample {from}), \
             key frames up to {} kbit",
            limit / 1000,
            largest_key * 8 / 1000
        ));
        if share > 1.0 + LIMIT_TOLERANCE {
            problems.push(format!(
                "{:.0} kbit in one second, {share:.2} of DataRateLimits' {} kbit",
                busiest / 1000.0,
                limit / 1000
            ));
        }
        // A limit alone on content that costs twice as much is a target
        // too: a stream far under it has lost the rate control.
        let used = rate / f64::from(limit);
        if targets.average.is_none() && used < LIMIT_FLOOR {
            problems.push(format!(
                "{:.0} kbit/s, {used:.2} of DataRateLimits' {} kbit/s on content that costs \
                 {:.0}",
                rate / 1000.0,
                limit / 1000,
                targets.cost / 1000.0
            ));
        }
    }
    (problems, line)
}

/// The bitrate before and after an `eset` of `AverageBitRate`, each from a
/// second after its start, so that the rate control has settled. The
/// `eset` is answered 0 by every encoder; the stream is to follow the new
/// rate from then on, or keep the first, as Apple's AVE keeps it when
/// frames come faster than real time. The line says which.
fn check_rate_change(
    case: &Case,
    samples: &[Got],
    at: u32,
    targets: &Targets,
) -> (Vec<String>, String) {
    let mut problems = Vec::new();
    let Some((before, after)) = targets.change else {
        return (problems, String::new());
    };
    let settle = i64::from(case.options.fps);
    let (at, frames) = (i64::from(at), i64::try_from(case.frames).unwrap_or(0));
    let first = rate_of(case, samples, settle..at);
    let second = rate_of(case, samples, at + settle..frames);
    let within = |rate: f64, target: u32| (rate / f64::from(target) - 1.0).abs() <= RATE_TOLERANCE;
    let mut line = format!(
        ", {:.0} kbit/s with no target, {:.2} of {} kbit/s before, {:.2} of {} kbit/s after",
        targets.cost / 1000.0,
        first / f64::from(before),
        before / 1000,
        second / f64::from(after),
        after / 1000
    );
    if !within(first, before) {
        problems.push(format!(
            "{:.0} kbit/s before the change, {:.2} of {} kbit/s",
            first / 1000.0,
            first / f64::from(before),
            before / 1000
        ));
    } else if within(second, after) {
        line.push_str(", the change taken");
    } else if within(second, before) {
        line.push_str(", the first rate kept");
    } else {
        problems.push(format!(
            "{:.0} kbit/s after the change, {:.2} of {} kbit/s and {:.2} of the first {} kbit/s",
            second / 1000.0,
            second / f64::from(after),
            after / 1000,
            second / f64::from(before),
            before / 1000
        ));
    }
    (problems, line)
}

/// After an `eset` of `MaxKeyFrameInterval` at `at`, the sync samples
/// keep to the new interval from the first at or past `at` on (the change
/// taken: when the encoder takes it up, at the change or at its next key
/// frame, is its own), or to the first interval throughout (the first
/// kept, as Apple's AVE keeps it when frames come faster than real time).
/// The line says which.
fn check_interval_change(
    syncs: &[i64],
    case: &Case,
    at: u32,
    before: u32,
    after: u32,
) -> (Vec<String>, String) {
    let frames = i64::try_from(case.frames).unwrap_or(0);
    let mut ends = syncs.to_vec();
    ends.push(frames);
    let at = i64::from(at);
    let over = |allowed: &dyn Fn(i64, i64) -> u32| -> Vec<String> {
        (ends.windows(2))
            .filter_map(|gap| {
                let (start, end) = (gap[0], gap[1]);
                let allowed = allowed(start, end);
                (end - start > i64::from(allowed)).then(|| {
                    format!(
                        "{} frames from the sync sample at {start}, over {allowed}",
                        end - start
                    )
                })
            })
            .collect()
    };
    let taken = over(&|start, end| {
        if start >= at {
            after
        } else if end >= at {
            before.max(after)
        } else {
            before
        }
    });
    let taken_somewhere = syncs
        .iter()
        .any(|&sync| sync >= at && sync < frames - i64::from(after));
    if taken.is_empty() && taken_somewhere {
        return (Vec::new(), ", the change taken".to_owned());
    }
    let kept = over(&|_, _| before);
    if kept.is_empty() {
        return (Vec::new(), ", the first interval kept".to_owned());
    }
    (taken, String::new())
}

/// What decoding back found.
struct DecodedBack {
    worst: f64,
    mean: f64,
    /// The lowest and highest decoded luma, in 8-bit steps.
    luma: (u16, u16),
}

/// Decodes the samples back on `backend`, as an MP4 of the first record,
/// and measures each frame against its source: the worst and the mean
/// PSNR over both planes, and the decoded luma's extremes.
fn decode_back(
    backend: &Backend,
    case: &Case,
    records: &[Vec<u8>],
    samples: &[Got],
) -> Result<DecodedBack, String> {
    let options = &case.options;
    if records.len() != 1 {
        return Err(format!(
            "{} records: one MP4 track takes one",
            records.len()
        ));
    }
    let (entry, record_box) = if &options.codec == b"avc1" {
        (b"avc1", b"avcC")
    } else {
        (b"hvc1", b"hvcC")
    };
    let track: Vec<Sample> = (samples.iter())
        .map(|s| Sample {
            data: s.data.clone(),
            pts: s.pts,
            dts: s.dts,
            duration: options.tick(),
            sync: s.sync,
        })
        .collect();
    let (width, height) = encoded_size(options.width, options.height);
    let file = mp4_write::write(
        entry,
        record_box,
        &records[0],
        width,
        height,
        options.timescale.cast_unsigned(),
        &track,
    );
    let run = decode_file_keeping_pixels(backend, &file, false)?;
    if run.frames.len() != samples.len() {
        return Err(format!(
            "{} frames of {} samples",
            run.frames.len(),
            samples.len()
        ));
    }
    let tick = options.tick().max(1);
    let bytes_per_sample = options.sample_bytes();
    let mut source = vec![0; options.frame_size()];
    let (mut worst, mut total) = (f64::INFINITY, 0.0);
    let mut luma = (u16::MAX, 0);
    let luma_samples = usize::try_from(width * height).unwrap_or(0);
    for frame in &run.frames {
        let sample = usize::try_from(frame.frame_id)
            .ok()
            .and_then(|i| samples.get(i))
            .ok_or("a frame of no sample")?;
        let index = u64::try_from(sample.pts / tick).map_err(|_| "a sample of no frame")?;
        if index >= case.frames {
            return Err("a sample of no frame".into());
        }
        picture_frame(
            case.picture,
            options.format,
            options.width,
            options.height,
            index,
            &mut source,
        );
        let pixels = frame.pixels.as_deref().ok_or("no pixels kept")?;
        let want = top_left(
            &source,
            (options.width, options.height),
            (width, height),
            bytes_per_sample,
        );
        let value = psnr(pixels, &want, bytes_per_sample);
        worst = worst.min(value);
        total += value.min(99.0);
        if case.picture == Picture::FullSwing {
            let eight = |at: usize| -> u16 {
                if bytes_per_sample == 2 {
                    pixels
                        .get(at * 2..at * 2 + 2)
                        .map_or(0, |b| u16::from_le_bytes([b[0], b[1]]) >> 8)
                } else {
                    pixels.get(at).copied().map_or(0, u16::from)
                }
            };
            for at in 0..luma_samples {
                let value = eight(at);
                luma = (luma.0.min(value), luma.1.max(value));
            }
        }
    }
    Ok(DecodedBack {
        worst,
        mean: total / run.frames.len().max(1) as f64,
        luma,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn got(times: &[(i64, i64)], tick: i64) -> Vec<Got> {
        (times.iter())
            .map(|&(pts, dts)| Got {
                pts: pts * tick,
                dts: dts * tick,
                sync: pts == 0,
                data: Vec::new(),
            })
            .collect()
    }

    fn test_case(frames: u64, reordering: bool) -> Case {
        let mut case = case(
            *b"avc1",
            PixelFormat::Nv12VideoRange,
            reordering,
            (64, 64),
            "",
        );
        case.frames = frames;
        case
    }

    #[test]
    fn sync_samples_are_found_by_their_nal_unit_types() {
        let sample = |header: &[u8]| {
            let mut s = (header.len() as u32).to_be_bytes().to_vec();
            s.extend_from_slice(header);
            s
        };
        assert!(starts_a_stream(*b"avc1", &sample(&[0x65, 0x88])));
        assert!(!starts_a_stream(*b"avc1", &sample(&[0x41, 0x9a])));
        // HEVC IDR_W_RADL (19) and CRA (21) start a stream; TRAIL_R (1) does not.
        assert!(starts_a_stream(*b"hvc1", &sample(&[19 << 1, 1])));
        assert!(starts_a_stream(*b"hvc1", &sample(&[21 << 1, 1])));
        assert!(!starts_a_stream(*b"hvc1", &sample(&[1 << 1, 1])));
        assert!(!starts_a_stream(*b"avc1", &[0, 0, 0, 9, 0x65]));
    }

    /// The decode times of a reordered H.264 encode at 30 fps: Apple's
    /// encoders' (each the PTS of the frame sent in its place, measured on
    /// the M3 Max's), and others that are not.
    #[test]
    fn decode_times_are_apples() {
        let case = test_case(5, true);
        let tick = case.options.tick();
        // Display order 0 4 2 1 3, as the M3 Max's AVE encoders emit it, and
        // 0 2 1 4 3, as the codec crate's encoders emit it.
        let apples = got(&[(0, 0), (4, 1), (2, 2), (1, 3), (3, 4)], tick);
        assert!(apples[3].dts > apples[3].pts);
        assert_eq!(check_times(&case, &apples), Vec::<String>::new());
        let ours = got(&[(0, 0), (2, 1), (1, 2), (4, 3), (3, 4)], tick);
        assert_eq!(check_times(&case, &ours), Vec::<String>::new());
        let not_apples = |at: usize| {
            vec![format!(
                "sample {at}'s DTS is not the PTS of the frame sent in its place"
            )]
        };
        // A frame behind the PTSes sent, never after a sample's own PTS.
        let trailing = got(&[(0, -1), (2, 0), (1, 1), (4, 2), (3, 3)], tick);
        assert_eq!(check_times(&case, &trailing), not_apples(0));
        let late = got(&[(0, 0), (4, 1), (2, 3), (1, 4), (3, 5)], tick);
        assert_eq!(check_times(&case, &late), not_apples(2));
        let in_order = Case {
            reordering: false,
            ..case.clone()
        };
        assert_eq!(
            check_times(&in_order, &apples),
            vec!["reordered with reordering off".to_owned()]
        );
        let none = got(&[(0, 0), (1, 1), (2, 2), (3, 3), (4, 4)], tick);
        assert_eq!(check_times(&in_order, &none), Vec::<String>::new());
        assert_eq!(
            times_line(&case, &apples),
            "times (frames, PTS/DTS): 0/0 4/1 2/2 1/3 3/4"
        );
    }

    /// AVE's pyramid reorders by two, one B picture between references by
    /// one, and frames in order by none.
    #[test]
    fn reordering_is_counted_as_the_sps_bounds_it() {
        let depth = |order: &[i64]| {
            let times: Vec<(i64, i64)> = (order.iter().enumerate())
                .map(|(at, &pts)| (pts, at as i64))
                .collect();
            reorder_depth(&got(&times, 20))
        };
        assert_eq!(depth(&[0, 4, 2, 1, 3, 8, 6, 5, 7]), 2);
        assert_eq!(depth(&[0, 2, 1, 4, 3]), 1);
        assert_eq!(depth(&[0, 1, 2, 3]), 0);
        assert_eq!(depth(&[0, 3, 1, 2]), 1);
        assert_eq!(depth(&[0, 3, 2, 1]), 2);
    }

    #[test]
    fn an_interval_change_is_taken_or_the_first_kept() {
        let case = test_case(150, false);
        let check = |syncs: &[i64]| check_interval_change(syncs, &case, 45, 60, 10);
        let taken = (Vec::new(), ", the change taken".to_owned());
        // 60 then 10 at frame 45: a key frame at the change, or at the old
        // interval's next, then every 10.
        assert_eq!(
            check(&[0, 45, 55, 65, 75, 85, 95, 105, 115, 125, 135, 145]),
            taken
        );
        assert_eq!(check(&[0, 60, 70, 80, 90, 100, 110, 120, 130, 140]), taken);
        // The first kept, as AVE keeps it.
        assert_eq!(
            check(&[0, 60, 120]),
            (Vec::new(), ", the first interval kept".to_owned())
        );
        // Neither: a gap past both.
        assert_eq!(
            check(&[0, 60, 70, 140]).0,
            vec!["70 frames from the sync sample at 70, over 10".to_owned()]
        );
        assert!(!check(&[0, 61, 70]).0.is_empty());
    }

    #[test]
    fn rates_are_counted_by_presentation_and_seconds_by_decode_order() {
        let mut case = test_case(60, false);
        case.options.fps = 30;
        let tick = case.options.tick();
        let mut samples = got(&(0..60).map(|n| (n, n)).collect::<Vec<_>>(), tick);
        for (n, sample) in samples.iter_mut().enumerate() {
            // 1000 bytes a frame, 4000 in the first.
            sample.data = vec![0; if n == 0 { 4000 } else { 1000 }];
        }
        assert_eq!(rate_of(&case, &samples, 30..60), 240_000.0);
        assert_eq!(rate_of(&case, &samples, 0..30), (4000.0 + 29_000.0) * 8.0);
        assert_eq!(busiest_second(&case, &samples), (33_000.0 * 8.0, 0));
        samples[45].data = vec![0; 9000];
        assert_eq!(busiest_second(&case, &samples), (38_000.0 * 8.0, 16));
    }

    #[test]
    fn forced_frames_are_checked_by_their_pictures() {
        let mut case = test_case(3, false);
        case.options.force = vec![1];
        let tick = case.options.tick();
        let mut samples = got(&[(0, 0), (1, 1), (2, 2)], tick);
        let idr = vec![0, 0, 0, 2, 0x65, 0x88];
        let p = vec![0, 0, 0, 2, 0x41, 0x9a];
        samples[0].data = idr.clone();
        samples[1].data = p.clone();
        samples[2].data = p;
        assert_eq!(
            check_forced(&case, &samples, &[1]),
            vec!["forced frame 1 holds no IDR picture".to_owned()]
        );
        samples[1].data = idr;
        assert!(check_forced(&case, &samples, &[1]).is_empty());
        assert_eq!(
            check_forced(&case, &samples, &[5]),
            vec!["no sample of forced frame 5".to_owned()]
        );
    }

    #[test]
    fn psnr_is_infinite_when_equal_and_falls_with_the_error() {
        let a = vec![100u8; 64];
        assert_eq!(psnr(&a, &a, 1), f64::INFINITY);
        let b = vec![101u8; 64];
        assert!((psnr(&a, &b, 1) - 48.13).abs() < 0.01);
        let words = |v: u16| (v << 6).to_le_bytes().repeat(32);
        assert!((psnr(&words(400), &words(401), 2) - 60.20).abs() < 0.01);
        assert_eq!(psnr(&a, &b[..63], 1), 0.0);
    }

    #[test]
    fn the_source_moves_and_ten_bits_are_four_times_eight() {
        let mut first = vec![0; 64 * 32 * 3 / 2];
        let mut second = first.clone();
        source_frame(*b"420v", 64, 32, 0, &mut first);
        source_frame(*b"420v", 64, 32, 1, &mut second);
        assert_ne!(first, second);
        let mut ten = vec![0; 64 * 32 * 3];
        source_frame(*b"x420", 64, 32, 0, &mut ten);
        let word = u16::from_le_bytes([ten[0], ten[1]]) >> 6;
        assert!((i32::from(word) - i32::from(first[0]) * 4).abs() <= 2);
    }

    /// The full swing reaches both ends; the waves and the texture stay
    /// well inside video range's.
    #[test]
    fn the_full_swing_reaches_black_and_white() {
        let (w, h) = (256u32, 144u32);
        let luma = |picture: Picture| {
            let mut frame = vec![0; (w * h * 3 / 2) as usize];
            picture_frame(picture, *b"420f", w, h, 3, &mut frame);
            let plane = &frame[..(w * h) as usize];
            (
                plane.iter().copied().min().unwrap_or(0),
                plane.iter().copied().max().unwrap_or(0),
            )
        };
        assert_eq!(luma(Picture::FullSwing), (0, 255));
        let (low, high) = luma(Picture::Waves);
        assert!(low >= 16 && high <= 235, "{low} {high}");
        let (low, high) = luma(Picture::Texture);
        assert!(low >= 16 && high <= 235, "{low} {high}");
    }

    #[test]
    fn the_texture_tiles_and_pans() {
        let [near, far] = textures();
        assert_eq!(near.at(3, 5), near.at(3 + TILE as i64, 5 - TILE as i64));
        assert_ne!(near.values, far.values);
        let (w, h) = (128u32, 64u32);
        let mut first = vec![0; (w * h * 3 / 2) as usize];
        let mut second = first.clone();
        picture_frame(Picture::Texture, *b"420v", w, h, 0, &mut first);
        picture_frame(Picture::Texture, *b"420v", w, h, 1, &mut second);
        assert_ne!(first, second);
    }

    /// vtencode's frames: their values at a few points, and the noise
    /// scrolled by 3 pixels a frame.
    #[test]
    fn the_scrolling_noise_is_vtencodes() {
        let (w, h) = (1280usize, 720usize);
        let noise: Vec<u8> = [(0, 0), (1, 0), (0, 1), (1000, 77), (4000, 239)]
            .iter()
            .map(|&(x, y)| vtencode_noise(x, y))
            .collect();
        assert_eq!(noise, [0, 227, 127, 87, 104]);
        let luma: Vec<u8> = [
            (0, 0, 0),
            (5, 100, 100),
            (5, 700, 10),
            (5, 40, 200),
            (9, 639, 481),
            (9, 640, 481),
            (0, 1279, 719),
        ]
        .iter()
        .map(|&(i, x, y)| vtencode_luma(i, x, y, w, h))
        .collect();
        assert_eq!(luma, [16, 16, 127, 235, 109, 141, 235]);
        assert_eq!(vtencode_chroma(0, 0, 0, w, h), (64, 64));
        assert_eq!(vtencode_chroma(5, 100, 100, w, h), (79, 83));
        assert_eq!(vtencode_chroma(17, 639, 359, w, h), (128, 135));
        let (fw, fh) = (256u32, 144u32);
        let mut first = vec![0; (fw * fh * 3 / 2) as usize];
        let mut second = first.clone();
        picture_frame(Picture::ScrollingNoise, *b"420v", fw, fh, 0, &mut first);
        picture_frame(Picture::ScrollingNoise, *b"420v", fw, fh, 1, &mut second);
        let row = 10 * fw as usize;
        assert_eq!(second[row + 128..row + 250], first[row + 131..row + 253]);
        assert_eq!(first[row + 5], vtencode_luma(0, 5, 10, 256, 144));
    }

    #[test]
    fn an_odd_frames_top_left_is_its_even_frame() {
        assert_eq!(encoded_size(641, 361), (640, 360));
        assert_eq!(encoded_size(640, 360), (640, 360));
        // A 5x3 frame: luma 15 samples, then two chroma rows of three pairs.
        let mut odd = vec![0; 15 + 12];
        source_frame(*b"420v", 5, 3, 0, &mut odd);
        let cut = top_left(&odd, (5, 3), (4, 2), 1);
        let want: Vec<u8> = [&odd[0..4], &odd[5..9], &odd[15..19]].concat();
        assert_eq!(cut, want);
        // The suite's own odd frame: each row's last sample is left out.
        let options = Options::new(*b"avc1", *b"420v", ODD.0, ODD.1);
        let mut frame = vec![0; options.frame_size()];
        source_frame(*b"420v", ODD.0, ODD.1, 0, &mut frame);
        let cut = top_left(&frame, ODD, encoded_size(ODD.0, ODD.1), 1);
        assert_eq!(cut.len(), 640 * 360 * 3 / 2);
        assert_eq!(cut[640], frame[641]);
    }
}
