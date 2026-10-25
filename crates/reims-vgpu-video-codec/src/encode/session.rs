//! An encoder as the host session's backend encoder: core's
//! `BackendEncoder` over [`super::encoder::Encoder`], whatever accelerates
//! it.
//!
//! The session opens the encoder at `esta`, but the guest sets most of its
//! properties after that, and the first frame's pixel format and clock
//! decide the rest. So the stream is made provisionally at open, which
//! checks the size and profile, and made again at the first frame when the
//! settings it then derives differ. Later, settings the stream can change
//! as it runs (bitrate within a rate control mode, frame rate, key-frame
//! interval) reach it at once. Any other change makes a new stream at the
//! next frame, from an IDR picture with new parameter sets. The session
//! sends a new format description when `configuration` changes, so the
//! frames the old stream still held come back on their own, before the new
//! stream's.
//!
//! Where the guest sets nothing, the choices are ours.
//!
//! **A stream at `AverageBitRate` alone** peaks at 1.05 times the average
//! ([`PEAK_NUMERATOR`]), with an HRD buffer of a second at the peak, so
//! that it meets the average within seconds, as Apple's AVE does, and
//! makes less where the pictures need less. Through the host session, on
//! heavy noise at 352x288, `mandelbrot` at 720p and `testsrc2` at 1080p,
//! H.264 and HEVC, with and without key frames every 2 s and reordering,
//! AVE made 0.98 to 1.04 of the average over 10 s, 0.97 to 1.16 over
//! 3 s, and its first second 0.95 to 1.27 of it. Peaking at 1.5 times the
//! average, NVENC ran its first second near that peak, 1.42 to 1.65 of
//! the average, and made 1.05 to 1.17 of it over 10 s and 1.16 to 1.41
//! over 3 s, whatever its buffer; peaking at 1.05 times, 1.15 to 1.44,
//! 1.01 to 1.09 and 1.05 to 1.22. radeonsi's VBR made 0.93 to 0.99 over
//! 10 s at 1.5 times, and 0.94 to 0.99 at 1.05.
//!
//! The peak is not the average itself: NVENC runs a VBR whose peak is
//! within about 1% of its average as a constant bitrate, and lowers its
//! QP to fill pictures cheaper than their share. The encode check's
//! waves, 60 frames at 640x360 and 3 Mbit/s, came to 0.73 to 1.03 of the
//! rate so, where AVE made 0.30 to 0.94 of it, radeonsi 0.33 to 0.88
//! whether its peak was the average or 1.05 times it, and NVENC 0.44 to
//! 0.87 at 1.05 times. HEVC filled at a peak 0.75% over the average, and
//! H.264 did not at 0.3%. (Measured on an M3 Max, macOS 27.0.1, an RTX
//! 5080 Laptop GPU, driver 615.71.09, and an RX 7900 XTX and a Radeon
//! 780M, Mesa 26.2.4, which gave the same bytes, 2026-10-25;
//! `docs/video/bugs.md`, B87, for what NVENC's first second keeps over
//! AVE's.)
//!
//! **A stream held to `DataRateLimits`** peaks at two thirds of the limit,
//! with an HRD buffer of half a second at that peak
//! ([`LIMIT_PEAK_NUMERATOR`], [`LIMIT_BUFFER_MILLISECONDS`]). With a
//! buffer of a second at the limit, NVENC's first second reached 1.07
//! (H.264) and 1.09 (HEVC) of it, and with an average at the limit VA-API's
//! H.264 reached 1.25 of it; Apple's AVE made 0.66 to 0.67 of a limit set
//! alone, every second under 0.72 of it (measured by the conformance
//! suite's `-limits` cases on an RTX 5080 Laptop GPU, an RX 7900 XTX and
//! an M3 Max, 2026-10-24).

use std::collections::HashMap;
use std::sync::Arc;

use reims_vgpu_video_core::backend::{self as core_backend, BackendError, PixelFormat};
use reims_vgpu_video_core::encoder_backend::{
    self as guest, BackendEncoder, EncodedFrame, EncoderConfig, EntropyMode, SourceFrame,
};
use reims_vgpu_video_core::wire::message::MediaTime;

use super::encoder::{Accelerator, Encoder};
use super::{
    Codec, Coded, ColourDescription, EncoderSettings, Entropy, FailedSample, FrameRate,
    InputFormat, InputFrame, Plane, Profile, RateControl, SECOND_BUFFER_MILLISECONDS, Unsupported,
};
use crate::num::ToUsize;

/// Ours: the quality a stream with no bitrate and no `Quality` is made at.
const DEFAULT_QUALITY: f32 = 0.5;
/// Ours: the frame rate assumed when the guest sets none and the first
/// frame has no duration.
const DEFAULT_FRAME_RATE: FrameRate = FrameRate::new(30, 1);
/// The clock the provisional stream is made with, before the first frame
/// gives the guest's.
const PROVISIONAL_TIMESCALE: u32 = 90_000;
/// Ours: a variable bitrate's peak, where `DataRateLimits` sets none, as a
/// share of the average: 1.05 (module doc).
const PEAK_NUMERATOR: u64 = 21;
const PEAK_DENOMINATOR: u64 = 20;
/// Ours: the peak of a variable bitrate held to `DataRateLimits`, as a
/// share of the limit (two thirds), and its HRD buffer, half a second at
/// that peak. `DataRateLimits` is a hard limit over any window of its
/// duration, and without an average "an overall average bit rate target
/// will be selected at a safe margin below" it (VideoToolbox's
/// `VTCompressionProperties.h`). A stream that keeps to an HRD buffer of
/// B bits filled at R bits a second puts at most B + R bits in any
/// second, so half a second at two thirds of the limit puts at most the
/// limit in any second, and in any longer window its share (module doc).
const LIMIT_PEAK_NUMERATOR: u64 = 2;
const LIMIT_PEAK_DENOMINATOR: u64 = 3;
const LIMIT_BUFFER_MILLISECONDS: u32 = 500;

/// A failure as the session's status for it.
pub trait BackendFailure {
    /// The error core's session maps to the guest's status.
    fn into_backend(self) -> BackendError;
}

impl BackendFailure for Unsupported {
    fn into_backend(self) -> BackendError {
        BackendError::Unsupported(self.0)
    }
}

/// What the session drives of a stream: [`Encoder`], here, whatever its
/// accelerator.
pub trait Stream: Send {
    /// Encodes one frame, and gives back each frame it completes, with its
    /// own result. `Err` is a frame it did not take, which never comes
    /// back.
    fn encode(&mut self, frame: &InputFrame<'_>) -> Result<Vec<Coded<BackendError>>, BackendError>;
    /// Encodes every frame held back, and gives each back with its own
    /// result. `Err` loses every frame it held, which the session then
    /// fails with it.
    fn flush(&mut self) -> Result<Vec<Coded<BackendError>>, BackendError>;
    /// The configuration record (avcC or hvcC) of what it emits.
    fn configuration(&self) -> Arc<[u8]>;
    /// The settings it encodes with now.
    fn settings(&self) -> &EncoderSettings;
    /// Changes the rate control, from the next frame on.
    fn set_rate_control(&mut self, rate_control: RateControl) -> Result<(), BackendError>;
    /// Changes the frame rate, from the next frame on.
    fn set_frame_rate(&mut self, frame_rate: FrameRate) -> Result<(), BackendError>;
    /// Changes the key frame interval, in frames and in time, from the next frame on.
    fn set_key_frame_interval(&mut self, frames: Option<u32>, duration: Option<i64>);
}

impl<A> Stream for Encoder<A>
where
    A: Accelerator + Send,
    A::Input: Send,
    A::Error: BackendFailure,
{
    fn encode(&mut self, frame: &InputFrame<'_>) -> Result<Vec<Coded<BackendError>>, BackendError> {
        Encoder::encode(self, frame)
            .map(into_backend)
            .map_err(BackendFailure::into_backend)
    }

    fn flush(&mut self) -> Result<Vec<Coded<BackendError>>, BackendError> {
        Ok(into_backend(Encoder::flush(self)))
    }

    fn configuration(&self) -> Arc<[u8]> {
        Encoder::configuration(self)
    }

    fn settings(&self) -> &EncoderSettings {
        Encoder::settings(self)
    }

    fn set_rate_control(&mut self, rate_control: RateControl) -> Result<(), BackendError> {
        Encoder::set_rate_control(self, rate_control).map_err(BackendFailure::into_backend)
    }

    fn set_frame_rate(&mut self, frame_rate: FrameRate) -> Result<(), BackendError> {
        Encoder::set_frame_rate(self, frame_rate).map_err(BackendFailure::into_backend)
    }

    fn set_key_frame_interval(&mut self, frames: Option<u32>, duration: Option<i64>) {
        Encoder::set_key_frame_interval(self, frames, duration);
    }
}

/// Each frame's result with its failure as the session's status for it.
fn into_backend<E: BackendFailure>(coded: Vec<Coded<E>>) -> Vec<Coded<BackendError>> {
    coded
        .into_iter()
        .map(|coded| {
            coded.map_err(|failed| FailedSample {
                id: failed.id,
                error: failed.error.map(BackendFailure::into_backend),
            })
        })
        .collect()
}

/// Makes a stream for settings.
pub(crate) type Build<S> = Box<dyn Fn(&EncoderSettings) -> Result<S, BackendError> + Send>;

/// What the source frames say of the stream: its bit depth and range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Facts {
    ten_bit: bool,
    full_range: bool,
}

impl Facts {
    fn of(format: PixelFormat) -> Self {
        Self {
            ten_bit: matches!(
                format,
                PixelFormat::P010VideoRange | PixelFormat::P010FullRange
            ),
            full_range: matches!(
                format,
                PixelFormat::Nv12FullRange | PixelFormat::P010FullRange
            ),
        }
    }
}

/// The stream's clock, from its first frame: its time stamps' timescale
/// and epoch, and the frame rate its duration says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Clock {
    timescale: u32,
    epoch: i64,
    frame_rate: FrameRate,
}

impl Clock {
    const PROVISIONAL: Self = Self {
        timescale: PROVISIONAL_TIMESCALE,
        epoch: 0,
        frame_rate: DEFAULT_FRAME_RATE,
    };

    fn of(frame: &SourceFrame<'_>) -> Result<Self, BackendError> {
        let pts = frame.presentation_time;
        let timescale = u32::try_from(pts.timescale)
            .ok()
            .filter(|&timescale| pts.is_valid() && timescale > 0)
            .ok_or_else(|| {
                BackendError::BadData("a frame without a valid presentation time".into())
            })?;
        let duration = frame.duration;
        let frame_rate = (duration.is_valid() && duration.timescale > 0 && duration.value > 0)
            .then(|| frame_rate_of(f64::from(duration.timescale) / seconds(duration.value)))
            .flatten()
            .unwrap_or(DEFAULT_FRAME_RATE);
        Ok(Self {
            timescale,
            epoch: pts.epoch,
            frame_rate,
        })
    }

    /// `time` in this clock's ticks, or `None` when it is invalid or does
    /// not fit.
    fn ticks(self, time: MediaTime) -> Option<i64> {
        if !time.is_valid() || time.timescale <= 0 {
            return None;
        }
        if u32::try_from(time.timescale).ok() == Some(self.timescale) {
            return Some(time.value);
        }
        let ticks = i128::from(time.value)
            .checked_mul(i128::from(self.timescale))?
            .checked_div(i128::from(time.timescale))?;
        i64::try_from(ticks).ok()
    }

    fn time(self, ticks: i64) -> MediaTime {
        MediaTime {
            value: ticks,
            timescale: i32::try_from(self.timescale).unwrap_or(i32::MAX),
            flags: MediaTime::FLAG_VALID,
            epoch: self.epoch,
        }
    }
}

/// A count of ticks as a float.
#[expect(
    clippy::as_conversions,
    reason = "a time's ticks as a float, for the rate it gives"
)]
fn seconds(ticks: i64) -> f64 {
    ticks as f64
}

/// A rate in frames a second as a fraction: whole, NTSC's thousand over
/// 1001, or to a thousandth.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the rate is 1000 at most and positive, so each rounding fits a u32"
)]
fn frame_rate_of(rate: f64) -> Option<FrameRate> {
    if !(rate.is_finite() && rate > 0.0 && rate <= 1000.0) {
        return None;
    }
    let whole = rate.round();
    if (rate - whole).abs() < 1e-3 && whole >= 1.0 {
        return Some(FrameRate::new(whole as u32, 1));
    }
    let ntsc = (rate * 1.001).round();
    if ntsc >= 1.0 && (rate - ntsc / 1.001).abs() < 1e-3 {
        return Some(FrameRate::new((ntsc * 1000.0) as u32, 1001));
    }
    Some(FrameRate::new(
        (rate * 1000.0).round().max(1.0) as u32,
        1000,
    ))
}

/// The codec the encoders write for one core names; none for AV1 or VP9,
/// which they do not.
fn codec_of(codec: core_backend::Codec) -> Result<Codec, BackendError> {
    match codec {
        core_backend::Codec::H264 => Ok(Codec::H264),
        core_backend::Codec::Hevc => Ok(Codec::Hevc),
        core_backend::Codec::Av1 => Err(BackendError::Unsupported(
            "the encoders write no AV1".into(),
        )),
        core_backend::Codec::Vp9 => Err(BackendError::Unsupported(
            "the encoders write no VP9".into(),
        )),
    }
}

/// The profile the guest's `ProfileLevel` names, or ours where it names
/// none: the most capable the driver has, and for HEVC the source's bit
/// depth.
fn choose_profile(
    codec: Codec,
    asked: Option<u32>,
    ten_bit: bool,
    offered: &[Profile],
) -> Result<Profile, BackendError> {
    if let Some(number) = asked {
        return offered
            .iter()
            .copied()
            .find(|profile| profile.codec() == codec && u32::from(profile.profile_idc()) == number)
            .ok_or_else(|| BackendError::Unsupported(format!("no {codec:?} profile {number}")));
    }
    let preference: &[Profile] = match codec {
        Codec::H264 => &[
            Profile::H264High,
            Profile::H264Main,
            Profile::H264ConstrainedBaseline,
        ],
        Codec::Hevc if ten_bit => &[Profile::HevcMain10, Profile::HevcMain],
        Codec::Hevc => &[Profile::HevcMain, Profile::HevcMain10],
    };
    preference
        .iter()
        .copied()
        .find(|profile| offered.contains(profile))
        .ok_or_else(|| BackendError::Unsupported(format!("the driver encodes no {codec:?}")))
}

/// Ours: `Quality` as a constant QP for P pictures, from 40 at 0 to 18 at
/// 1, I pictures 2 below and B pictures 2 above.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the clamped quality makes a QP of 18 to 40"
)]
fn constant_qp(quality: f32) -> RateControl {
    // A NaN passes a clamp, and would make QP 0 and its I pictures' -2.
    let quality = if quality.is_nan() {
        0.0
    } else {
        quality.clamp(0.0, 1.0)
    };
    let qp = (40.0 - 22.0 * quality).round() as u8;
    // The QP is 18 to 40, so neither saturates.
    RateControl::ConstantQp {
        intra: qp.saturating_sub(2),
        predicted: qp,
        bidirectional: qp.saturating_add(2),
    }
}

/// The rate control the guest's settings ask for: a variable bitrate when
/// it sets `AverageBitRate` or `DataRateLimits`, else a constant QP from
/// `Quality` (both ours). At an average alone, it peaks at 1.05 times the
/// average (module doc). Held to a limit, it peaks at two thirds of it,
/// and aims at the average, if set, under that peak, else at the peak: the
/// limit is a hard one, the average a target.
fn rate_control(settings: &guest::EncoderSettings) -> RateControl {
    match (settings.average_bit_rate, settings.max_bit_rate) {
        (Some(average), None) => RateControl::VariableBitRate {
            average_bits_per_second: average,
            // A peak past a u32, as a product past a u64 would be, is a
            // u32's largest.
            peak_bits_per_second: (u64::from(average).checked_mul(PEAK_NUMERATOR))
                .and_then(|bits| u32::try_from(bits / PEAK_DENOMINATOR).ok())
                .unwrap_or(u32::MAX),
        },
        (average, Some(limit)) => {
            let peak = (u64::from(limit).checked_mul(LIMIT_PEAK_NUMERATOR))
                .and_then(|bits| u32::try_from(bits / LIMIT_PEAK_DENOMINATOR).ok())
                .unwrap_or(limit)
                .max(1);
            RateControl::VariableBitRate {
                average_bits_per_second: average.map_or(peak, |average| average.min(peak)),
                peak_bits_per_second: peak,
            }
        }
        (None, None) => constant_qp(settings.quality.unwrap_or(DEFAULT_QUALITY)),
    }
}

/// How long the HRD buffer holds at the peak: half a second where
/// `DataRateLimits` holds the stream, else a second.
fn buffer_milliseconds(settings: &guest::EncoderSettings) -> u32 {
    if settings.max_bit_rate.is_some() {
        LIMIT_BUFFER_MILLISECONDS
    } else {
        SECOND_BUFFER_MILLISECONDS
    }
}

/// The settings a stream is made with, from the guest's.
fn derive(
    config: &EncoderConfig,
    settings: &guest::EncoderSettings,
    facts: Facts,
    clock: Clock,
    offered: &[Profile],
) -> Result<EncoderSettings, BackendError> {
    let profile = choose_profile(
        codec_of(config.codec)?,
        settings.profile,
        facts.ten_bit,
        offered,
    )?;
    let level = settings
        .level
        .map(|level| {
            u8::try_from(level)
                .map_err(|_| BackendError::Unsupported(format!("level number {level}")))
        })
        .transpose()?;
    let colour = settings.colour;
    let mut out = EncoderSettings::new(profile, config.width, config.height);
    out.full_range = facts.full_range;
    out.colour = (colour.primaries.is_some()
        || colour.transfer.is_some()
        || colour.matrix.is_some())
    .then(|| ColourDescription {
        // 2 is unspecified (H.273).
        primaries: colour.primaries.unwrap_or(2),
        transfer: colour.transfer.unwrap_or(2),
        matrix: colour.matrix.unwrap_or(2),
    });
    out.frame_rate = settings
        .expected_frame_rate
        .and_then(frame_rate_of)
        .unwrap_or(clock.frame_rate);
    out.timescale = clock.timescale;
    out.rate_control = rate_control(settings);
    out.buffer_milliseconds = buffer_milliseconds(settings);
    out.hard_limit = settings.max_bit_rate.is_some();
    out.key_frame_interval = settings.max_key_frame_interval;
    out.key_frame_interval_duration = settings
        .max_key_frame_interval_duration
        .map(|seconds| ticks_of(seconds, clock.timescale))
        .filter(|&ticks| ticks > 0);
    out.allow_frame_reordering = settings.allow_frame_reordering;
    out.level = level;
    out.entropy = settings.entropy.map(|entropy| match entropy {
        EntropyMode::Cavlc => Entropy::Cavlc,
        EntropyMode::Cabac => Entropy::Cabac,
    });
    Ok(out)
}

/// The guest's `seconds` in ticks of `timescale`, rounded: saturated where
/// they do not fit an `i64`, and 0 for a NaN.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "a float's conversion saturates, and takes a NaN to 0"
)]
fn ticks_of(seconds: f64, timescale: u32) -> i64 {
    (seconds * f64::from(timescale)).round() as i64
}

/// Whether a stream made with `built` cannot become `wanted` as it runs:
/// anything but the bitrate within its mode, the frame rate and the
/// key-frame interval differs.
fn needs_new_stream(built: &EncoderSettings, wanted: &EncoderSettings) -> bool {
    let mut built = built.clone();
    let mut wanted = wanted.clone();
    for settings in [&mut built, &mut wanted] {
        settings.frame_rate = DEFAULT_FRAME_RATE;
        settings.key_frame_interval = None;
        settings.key_frame_interval_duration = None;
        // A stream keeps the buffer it was made with: AVE takes no
        // DataRateLimits once it is prepared.
        settings.buffer_milliseconds = SECOND_BUFFER_MILLISECONDS;
        settings.hard_limit = false;
    }
    if core::mem::discriminant(&built.rate_control) == core::mem::discriminant(&wanted.rate_control)
    {
        wanted.rate_control = built.rate_control;
    }
    built != wanted
}

/// Gives a running stream what of `wanted` it lacks, of what it can change.
fn apply<S: Stream>(stream: &mut S, wanted: &EncoderSettings) -> Result<(), BackendError> {
    let current = stream.settings().clone();
    if current.rate_control != wanted.rate_control {
        stream.set_rate_control(wanted.rate_control)?;
    }
    if current.frame_rate != wanted.frame_rate {
        stream.set_frame_rate(wanted.frame_rate)?;
    }
    if (
        current.key_frame_interval,
        current.key_frame_interval_duration,
    ) != (
        wanted.key_frame_interval,
        wanted.key_frame_interval_duration,
    ) {
        stream.set_key_frame_interval(
            wanted.key_frame_interval,
            wanted.key_frame_interval_duration,
        );
    }
    Ok(())
}

/// A stream, with what it was made from.
struct Built<S> {
    stream: S,
    settings: EncoderSettings,
    facts: Facts,
}

/// One encoder of the host session, over streams `build` makes.
pub struct SessionEncoder<S> {
    build: Build<S>,
    config: EncoderConfig,
    offered: Vec<Profile>,
    /// The guest's settings, as the session last gave them.
    settings: guest::EncoderSettings,
    built: Option<Built<S>>,
    /// From the first frame on.
    clock: Option<Clock>,
    /// Each frame's time stamps as the guest sent them, by its ID, until
    /// its sample comes out.
    times: HashMap<u64, (MediaTime, MediaTime)>,
    /// The new stream's frames, held back while the old one's go out
    /// under its configuration record.
    deferred: Vec<EncodedFrame>,
    /// The configuration record of the frames last returned.
    published: Arc<[u8]>,
    /// Planes converted to the stream's bit depth.
    scratch: [Vec<u8>; 2],
}

impl<S: Stream> SessionEncoder<S> {
    /// An encoder for `config`, with the profiles the driver encodes. The
    /// provisional stream is made now, which refuses a size or profile
    /// the driver cannot encode.
    pub fn new(
        config: &EncoderConfig,
        offered: Vec<Profile>,
        build: Build<S>,
    ) -> Result<Self, BackendError> {
        let facts = Facts {
            ten_bit: false,
            full_range: false,
        };
        let settings = derive(
            config,
            &config.settings,
            facts,
            Clock::PROVISIONAL,
            &offered,
        )?;
        let stream = build(&settings)?;
        let published = stream.configuration();
        Ok(Self {
            build,
            config: config.clone(),
            offered,
            settings: config.settings.clone(),
            built: Some(Built {
                stream,
                settings,
                facts,
            }),
            clock: None,
            times: HashMap::new(),
            deferred: Vec::new(),
            published,
            scratch: [Vec::new(), Vec::new()],
        })
    }

    fn derive(&self, facts: Facts) -> Result<EncoderSettings, BackendError> {
        derive(
            &self.config,
            &self.settings,
            facts,
            self.clock.unwrap_or(Clock::PROVISIONAL),
            &self.offered,
        )
    }

    /// The stream's frames as the session's, with the time stamps the
    /// guest sent, each failed one with its error, or a malfunction where
    /// it was lost with another.
    fn frames_of(&mut self, coded: Vec<Coded<BackendError>>) -> Vec<EncodedFrame> {
        let clock = self.clock.unwrap_or(Clock::PROVISIONAL);
        coded
            .into_iter()
            .map(|coded| {
                let sample = match coded {
                    Ok(sample) => sample,
                    Err(failed) => {
                        self.times.remove(&failed.id);
                        let error = failed.error.unwrap_or_else(|| {
                            BackendError::Malfunction(
                                "lost with a picture of its run, which failed".into(),
                            )
                        });
                        return EncodedFrame::failed(failed.id.cast_signed(), error);
                    }
                };
                let (presentation_time, duration) =
                    self.times.remove(&sample.id).unwrap_or_else(|| {
                        let duration = if sample.duration > 0 {
                            clock.time(sample.duration)
                        } else {
                            MediaTime::INVALID
                        };
                        (clock.time(sample.pts), duration)
                    });
                let decode_time = if sample.dts == sample.pts {
                    MediaTime::INVALID
                } else {
                    MediaTime {
                        epoch: presentation_time.epoch,
                        ..clock.time(sample.dts)
                    }
                };
                EncodedFrame {
                    frame_id: sample.id.cast_signed(),
                    data: sample.data,
                    presentation_time,
                    decode_time,
                    duration,
                    sync: sample.is_sync,
                    depends_on_others: sample.depends_on_others,
                    depended_on_by_others: sample.is_depended_on,
                    error: None,
                }
            })
            .collect()
    }

    /// Every frame `stream` holds, each with its own result. Those it took
    /// and does not give back, which a flush that fails loses, are failed
    /// with its error, or as lost where it gave none, in presentation
    /// order: the frames whose time stamps are still kept.
    fn flush_stream(&mut self, stream: &mut S) -> Vec<EncodedFrame> {
        let (mut frames, error) = match stream.flush() {
            Ok(coded) => (self.frames_of(coded), None),
            Err(error) => (Vec::new(), Some(error)),
        };
        let mut lost: Vec<(i64, u64)> = self
            .times
            .drain()
            .map(|(id, (presentation_time, _))| (presentation_time.value, id))
            .collect();
        lost.sort_unstable();
        let error = error
            .unwrap_or_else(|| BackendError::Malfunction("the stream did not give it back".into()));
        frames.extend(
            lost.into_iter()
                .map(|(_, id)| EncodedFrame::failed(id.cast_signed(), error.clone())),
        );
        frames
    }
}

impl<S: Stream> SessionEncoder<S> {
    /// Encodes `frame` into the stream `wanted` asks for, appending to
    /// `batch` what comes out. When a new stream had to be made and the old
    /// one's frames are in `batch`, this frame's are returned instead, to
    /// go out after them under the new configuration record.
    fn encode_into(
        &mut self,
        frame: &SourceFrame<'_>,
        pts: i64,
        duration: i64,
        facts: Facts,
        wanted: &EncoderSettings,
        batch: &mut Vec<EncodedFrame>,
    ) -> Result<Option<Vec<EncodedFrame>>, BackendError> {
        let mut old_stream_frames = false;
        match self.built.as_mut() {
            Some(built) if built.facts == facts && !needs_new_stream(&built.settings, wanted) => {
                apply(&mut built.stream, wanted)?;
            }
            _ => {
                if let Some(mut old) = self.built.take() {
                    let flushed = self.flush_stream(&mut old.stream);
                    batch.extend(flushed);
                    if !batch.is_empty() {
                        self.published = old.stream.configuration();
                        old_stream_frames = true;
                    }
                }
                self.built = Some(Built {
                    stream: (self.build)(wanted)?,
                    settings: wanted.clone(),
                    facts,
                });
            }
        }
        let Some(built) = self.built.as_mut() else {
            return Err(BackendError::Malfunction("no stream".into()));
        };
        let id = frame.frame_id.cast_unsigned();
        let input = InputFrame {
            planes: planes(frame, wanted.input_format(), &mut self.scratch)?,
            pts,
            duration,
            force_key_frame: frame.force_key_frame,
            id,
        };
        self.times
            .insert(id, (frame.presentation_time, frame.duration));
        let samples = built.stream.encode(&input).inspect_err(|_| {
            self.times.remove(&id);
        })?;
        let configuration = built.stream.configuration();
        let frames = self.frames_of(samples);
        if old_stream_frames {
            Ok(Some(frames))
        } else {
            batch.extend(frames);
            self.published = configuration;
            Ok(None)
        }
    }
}

/// `frame`'s planes in `format`: its own where it is in that depth, else
/// converted into `scratch`, 8-bit samples to the high byte of 16 and 16
/// to their high byte.
fn planes<'a>(
    frame: &'a SourceFrame<'_>,
    format: InputFormat,
    scratch: &'a mut [Vec<u8>; 2],
) -> Result<[Plane<'a>; 2], BackendError> {
    let [luma, chroma, ..] = frame.planes else {
        return Err(BackendError::BadData(format!(
            "a 4:2:0 frame needs two planes, not {}",
            frame.planes.len()
        )));
    };
    let from = if Facts::of(frame.pixel_format).ten_bit {
        InputFormat::P010
    } else {
        InputFormat::Nv12
    };
    if from == format {
        return Ok([luma, chroma].map(|plane| Plane {
            data: plane.bytes,
            bytes_per_row: plane.bytes_per_row,
        }));
    }
    // A row of either plane holds the width's samples: luma's, or Cb and
    // Cr for each pair of columns.
    let samples = frame.width.next_multiple_of(2).to_usize();
    let rows = [frame.height, frame.height.div_ceil(2)];
    let (from_bytes, to_bytes) = (from.bytes_per_sample(), format.bytes_per_sample());
    let too_large = || BackendError::BadData("a frame too large to convert".into());
    let (from_row, to_row) = (samples.checked_mul(from_bytes))
        .zip(samples.checked_mul(to_bytes))
        .ok_or_else(too_large)?;
    for ((plane, rows), out) in [luma, chroma].iter().zip(rows).zip(scratch.iter_mut()) {
        let rows = rows.to_usize();
        out.clear();
        out.reserve(to_row.checked_mul(rows).ok_or_else(too_large)?);
        for row in 0..rows {
            // An offset past a usize is past the plane's bytes.
            let source = (row.checked_mul(plane.bytes_per_row))
                .and_then(|start| plane.bytes.get(start..start.checked_add(from_row)?))
                .ok_or_else(|| BackendError::BadData("a source plane too small".into()))?;
            if from_bytes == 1 {
                for &sample in source {
                    out.extend_from_slice(&(u16::from(sample) << 8).to_le_bytes());
                }
            } else {
                out.extend(source.as_chunks::<2>().0.iter().map(|[_, high]| *high));
            }
        }
    }
    let [luma, chroma] = scratch;
    Ok([
        Plane {
            data: luma,
            bytes_per_row: to_row,
        },
        Plane {
            data: chroma,
            bytes_per_row: to_row,
        },
    ])
}

impl<S: Stream> BackendEncoder for SessionEncoder<S> {
    fn configuration(&self) -> Vec<u8> {
        self.published.to_vec()
    }

    fn encode(&mut self, frame: &SourceFrame<'_>) -> Result<Vec<EncodedFrame>, BackendError> {
        let clock = match self.clock {
            Some(clock) => clock,
            None => *self.clock.insert(Clock::of(frame)?),
        };
        let pts = clock.ticks(frame.presentation_time).ok_or_else(|| {
            BackendError::BadData("a frame without a valid presentation time".into())
        })?;
        let duration = clock
            .ticks(frame.duration)
            .filter(|&ticks| ticks > 0)
            .unwrap_or(0);
        let facts = Facts::of(frame.pixel_format);
        let wanted = self.derive(facts)?;
        let mut batch = core::mem::take(&mut self.deferred);
        match self.encode_into(frame, pts, duration, facts, &wanted, &mut batch) {
            Ok(Some(frames)) => {
                // The old stream's frames go out now, this one's next.
                self.deferred = frames;
                Ok(batch)
            }
            Ok(None) => Ok(batch),
            Err(error) => {
                // What came out before the failure goes at the next call.
                self.deferred = batch;
                Err(error)
            }
        }
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>, BackendError> {
        let mut batch = core::mem::take(&mut self.deferred);
        let mut built = self.built.take();
        if let Some(built) = built.as_mut() {
            batch.extend(self.flush_stream(&mut built.stream));
            self.published = built.stream.configuration();
        }
        self.built = built;
        Ok(batch)
    }

    fn reconfigure(&mut self, settings: &guest::EncoderSettings) -> Result<(), BackendError> {
        let previous = core::mem::replace(&mut self.settings, settings.clone());
        let Some(facts) = self.built.as_ref().map(|built| built.facts) else {
            return Ok(());
        };
        let result = self
            .derive(facts)
            .and_then(|wanted| match self.built.as_mut() {
                // A new stream is made at the next frame.
                Some(built) if needs_new_stream(&built.settings, &wanted) => Ok(()),
                Some(built) => apply(&mut built.stream, &wanted),
                None => Ok(()),
            });
        if result.is_err() {
            self.settings = previous;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::encoder::tests::{Fake, limits};
    use reims_vgpu_video_core::surfaces::PlaneSource;

    const OFFERED: [Profile; 4] = [
        Profile::H264ConstrainedBaseline,
        Profile::H264Main,
        Profile::H264High,
        Profile::HevcMain,
    ];

    fn config(codec: core_backend::Codec) -> EncoderConfig {
        EncoderConfig {
            codec,
            width: 640,
            height: 480,
            settings: guest::EncoderSettings::default(),
        }
    }

    /// Streams on the fake accelerator, counted.
    fn session(
        codec: core_backend::Codec,
        made: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Result<SessionEncoder<Encoder<Fake>>, BackendError> {
        SessionEncoder::new(
            &config(codec),
            OFFERED.to_vec(),
            Box::new(move |settings| {
                made.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Encoder::new(Fake::default(), settings.clone(), &limits())
                    .map_err(BackendFailure::into_backend)
            }),
        )
    }

    fn time(value: i64) -> MediaTime {
        MediaTime {
            value,
            timescale: 600,
            flags: MediaTime::FLAG_VALID,
            epoch: 0,
        }
    }

    fn encode<S: Stream>(
        encoder: &mut SessionEncoder<S>,
        n: i64,
        format: PixelFormat,
    ) -> Result<Vec<EncodedFrame>, BackendError> {
        let depth = if Facts::of(format).ten_bit { 2 } else { 1 };
        let luma = vec![0x40; 640 * depth * 480];
        let chroma = vec![0x80; 640 * depth * 240];
        let planes = [
            PlaneSource {
                bytes: &luma,
                bytes_per_row: 640 * depth,
                rows: 480,
            },
            PlaneSource {
                bytes: &chroma,
                bytes_per_row: 640 * depth,
                rows: 240,
            },
        ];
        encoder.encode(&SourceFrame {
            frame_id: n,
            presentation_time: time(n * 20),
            duration: time(20),
            force_key_frame: false,
            pixel_format: format,
            width: 640,
            height: 480,
            planes: &planes,
        })
    }

    #[test]
    fn any_quality_makes_qps_within_51() {
        for quality in [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            -1.0,
            0.0,
            1.0,
            2.0,
        ] {
            let RateControl::ConstantQp {
                intra,
                predicted,
                bidirectional,
            } = constant_qp(quality)
            else {
                panic!("{quality} gives a rate control other than a constant QP");
            };
            assert!(
                (16..=42).contains(&intra) && intra < predicted && predicted < bidirectional,
                "{quality}"
            );
            assert!(bidirectional <= 51, "{quality}");
        }
    }

    #[test]
    fn guest_settings_become_the_streams() -> Result<(), BackendError> {
        let config = config(core_backend::Codec::H264);
        let facts = Facts {
            ten_bit: false,
            full_range: true,
        };
        let clock = Clock {
            timescale: 600,
            epoch: 0,
            frame_rate: FrameRate::new(30, 1),
        };
        let mut settings = guest::EncoderSettings::default();
        let derived = derive(&config, &settings, facts, clock, &OFFERED)?;
        assert_eq!(derived.profile, Profile::H264High);
        assert!(derived.full_range);
        assert!(!derived.allow_frame_reordering);
        assert_eq!(derived.key_frame_interval, None);
        assert_eq!(derived.rate_control, constant_qp(DEFAULT_QUALITY));
        settings.profile = Some(77);
        settings.average_bit_rate = Some(2_000_000);
        settings.expected_frame_rate = Some(29.97);
        settings.max_key_frame_interval_duration = Some(2.0);
        settings.colour.primaries = Some(1);
        let derived = derive(&config, &settings, facts, clock, &OFFERED)?;
        assert_eq!(derived.profile, Profile::H264Main);
        assert_eq!(
            derived.rate_control,
            RateControl::VariableBitRate {
                average_bits_per_second: 2_000_000,
                peak_bits_per_second: 2_100_000,
            }
        );
        assert_eq!(derived.frame_rate, FrameRate::new(30_000, 1001));
        assert_eq!(derived.key_frame_interval_duration, Some(1200));
        assert_eq!(
            derived.colour,
            Some(ColourDescription {
                primaries: 1,
                transfer: 2,
                matrix: 2
            })
        );
        settings.profile = Some(2);
        assert!(derive(&config, &settings, facts, clock, &OFFERED).is_err());
        Ok(())
    }

    #[test]
    fn the_first_frame_remakes_the_stream_and_its_times_come_back() -> Result<(), BackendError> {
        let made = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut encoder = session(core_backend::Codec::H264, made.clone())?;
        let settings = guest::EncoderSettings {
            allow_frame_reordering: true,
            ..Default::default()
        };
        encoder.reconfigure(&settings)?;
        let mut frames = Vec::new();
        for n in 0..5 {
            frames.extend(encode(&mut encoder, n, PixelFormat::Nv12VideoRange)?);
        }
        frames.extend(encoder.flush()?);
        // The provisional stream, then the first frame's, on the guest's
        // 600 Hz clock.
        assert_eq!(made.load(std::sync::atomic::Ordering::SeqCst), 2);
        let ids: Vec<i64> = frames.iter().map(|frame| frame.frame_id).collect();
        assert_eq!(ids, [0, 2, 1, 4, 3]);
        assert!(frames[0].sync && !frames[1].sync);
        for frame in &frames {
            assert_eq!(frame.presentation_time, time(frame.frame_id * 20));
            assert_eq!(frame.duration, time(20));
        }
        // With B pictures each DTS is the PTS of the frame sent in its
        // place, as Apple's encoders give it, so B picture 1's is after its
        // PTS. Where it equals the PTS it is left invalid, as the session
        // reads it.
        let dts: Vec<Option<i64>> = frames
            .iter()
            .map(|frame| {
                frame
                    .decode_time
                    .is_valid()
                    .then_some(frame.decode_time.value)
            })
            .collect();
        assert_eq!(dts, [None, Some(20), Some(40), Some(60), Some(80)]);
        assert!(!encoder.configuration().is_empty());
        Ok(())
    }

    #[test]
    fn live_changes_reach_the_stream_and_others_make_a_new_one() -> Result<(), BackendError> {
        let made = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut encoder = session(core_backend::Codec::H264, made.clone())?;
        let mut settings = guest::EncoderSettings {
            average_bit_rate: Some(1_000_000),
            allow_frame_reordering: true,
            ..Default::default()
        };
        encoder.reconfigure(&settings)?;
        assert_eq!(
            encode(&mut encoder, 0, PixelFormat::Nv12VideoRange)?.len(),
            1
        );
        assert!(encode(&mut encoder, 1, PixelFormat::Nv12VideoRange)?.is_empty());
        settings.average_bit_rate = Some(2_000_000);
        encoder.reconfigure(&settings)?;
        assert_eq!(made.load(std::sync::atomic::Ordering::SeqCst), 2);
        let record = encoder.configuration();
        // A new profile: frame 1, held as a B picture, goes out under the
        // old record, and frame 2's IDR waits for the next call.
        settings.profile = Some(66);
        encoder.reconfigure(&settings)?;
        let old = encode(&mut encoder, 2, PixelFormat::Nv12VideoRange)?;
        assert_eq!(made.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(old.iter().map(|f| f.frame_id).collect::<Vec<_>>(), [1]);
        assert_eq!(encoder.configuration(), record);
        let new = encode(&mut encoder, 3, PixelFormat::Nv12VideoRange)?;
        assert_eq!(new.iter().map(|f| f.frame_id).collect::<Vec<_>>(), [2, 3]);
        assert!(new[0].sync);
        assert_ne!(encoder.configuration(), record);
        Ok(())
    }

    /// Each frame's ID, and whether it came with a sample or an error.
    fn outcomes(frames: &[EncodedFrame]) -> Vec<(i64, bool)> {
        frames
            .iter()
            .map(|frame| {
                assert!(frame.error.is_none() || frame.data.is_empty());
                (frame.frame_id, frame.error.is_none())
            })
            .collect()
    }

    /// A picture that fails comes out failed in its place, with the
    /// session's status for its error, and the samples of its run coded
    /// before it come out with it.
    #[test]
    fn a_failed_picture_comes_out_failed_among_its_runs_samples() -> Result<(), BackendError> {
        let mut encoder = SessionEncoder::new(
            &config(core_backend::Codec::H264),
            OFFERED.to_vec(),
            // Each stream's third picture: the first frame's stream codes
            // frame 0, then P picture 2, then B picture 1, which fails.
            Box::new(|settings| {
                Encoder::new(Fake::failing_at(2), settings.clone(), &limits())
                    .map_err(BackendFailure::into_backend)
            }),
        )?;
        encoder.reconfigure(&guest::EncoderSettings {
            allow_frame_reordering: true,
            ..Default::default()
        })?;
        let mut frames = Vec::new();
        for n in 0..5 {
            frames.extend(encode(&mut encoder, n, PixelFormat::Nv12VideoRange)?);
        }
        frames.extend(encoder.flush()?);
        assert_eq!(
            outcomes(&frames),
            [(0, true), (2, true), (1, false), (3, true), (4, true)]
        );
        assert!(matches!(
            frames[2].error,
            Some(BackendError::Unsupported(_))
        ));
        // The frame after the failure starts the stream again.
        assert!(frames[3].sync);
        Ok(())
    }

    /// A stream whose flush fails, losing what it held.
    struct FlushFails(Encoder<Fake>);

    impl Stream for FlushFails {
        fn encode(
            &mut self,
            frame: &InputFrame<'_>,
        ) -> Result<Vec<Coded<BackendError>>, BackendError> {
            Stream::encode(&mut self.0, frame)
        }
        fn flush(&mut self) -> Result<Vec<Coded<BackendError>>, BackendError> {
            Err(BackendError::Malfunction("the flush fails".into()))
        }
        fn configuration(&self) -> Arc<[u8]> {
            Stream::configuration(&self.0)
        }
        fn settings(&self) -> &EncoderSettings {
            Stream::settings(&self.0)
        }
        fn set_rate_control(&mut self, rate_control: RateControl) -> Result<(), BackendError> {
            Stream::set_rate_control(&mut self.0, rate_control)
        }
        fn set_frame_rate(&mut self, frame_rate: FrameRate) -> Result<(), BackendError> {
            Stream::set_frame_rate(&mut self.0, frame_rate)
        }
        fn set_key_frame_interval(&mut self, frames: Option<u32>, duration: Option<i64>) {
            Stream::set_key_frame_interval(&mut self.0, frames, duration);
        }
    }

    /// A flush that fails loses the frames the stream held: each comes out
    /// failed with its error, and the frames deferred before it keep their
    /// samples, where the failure lost them all.
    #[test]
    fn a_failed_flush_fails_each_frame_it_held_and_keeps_the_rest() -> Result<(), BackendError> {
        let mut encoder = SessionEncoder::new(
            &config(core_backend::Codec::H264),
            OFFERED.to_vec(),
            Box::new(|settings| {
                Encoder::new(Fake::default(), settings.clone(), &limits())
                    .map(FlushFails)
                    .map_err(BackendFailure::into_backend)
            }),
        )?;
        let mut settings = guest::EncoderSettings {
            allow_frame_reordering: true,
            ..Default::default()
        };
        encoder.reconfigure(&settings)?;
        let mut frames = Vec::new();
        for n in 0..4 {
            frames.extend(encode(&mut encoder, n, PixelFormat::Nv12VideoRange)?);
        }
        // Frame 3 waits as a B picture.
        assert_eq!(outcomes(&frames), [(0, true), (2, true), (1, true)]);
        // A new profile: the old stream's flush fails, and frame 3 with it;
        // frame 4 goes out after it under the new stream.
        settings.profile = Some(66);
        encoder.reconfigure(&settings)?;
        let old = encode(&mut encoder, 4, PixelFormat::Nv12VideoRange)?;
        assert_eq!(outcomes(&old), [(3, false)]);
        assert!(matches!(old[0].error, Some(BackendError::Malfunction(_))));
        let new = encode(&mut encoder, 5, PixelFormat::Nv12VideoRange)?;
        assert_eq!(outcomes(&new), [(4, true), (5, true)]);
        // Baseline holds nothing back, so this flush loses nothing.
        assert!(encoder.flush()?.is_empty());
        Ok(())
    }

    #[test]
    fn ten_bit_sources_choose_main_10_or_are_narrowed() -> Result<(), BackendError> {
        let source = [0x12, 0x34, 0x56, 0x78];
        let planes_in = [
            PlaneSource {
                bytes: &source,
                bytes_per_row: 4,
                rows: 1,
            },
            PlaneSource {
                bytes: &source,
                bytes_per_row: 4,
                rows: 1,
            },
        ];
        let frame = SourceFrame {
            frame_id: 0,
            presentation_time: time(0),
            duration: time(20),
            force_key_frame: false,
            pixel_format: PixelFormat::P010VideoRange,
            width: 2,
            height: 1,
            planes: &planes_in,
        };
        let mut scratch = [Vec::new(), Vec::new()];
        let [luma, _] = planes(&frame, InputFormat::Nv12, &mut scratch)?;
        assert_eq!(luma.data, [0x34, 0x78]);
        let nv12 = SourceFrame {
            pixel_format: PixelFormat::Nv12FullRange,
            ..frame
        };
        let [luma, _] = planes(&nv12, InputFormat::P010, &mut scratch)?;
        assert_eq!(luma.data, [0, 0x12, 0, 0x34]);
        let mut offered = OFFERED.to_vec();
        assert_eq!(
            choose_profile(Codec::Hevc, None, true, &offered)?,
            Profile::HevcMain
        );
        offered.push(Profile::HevcMain10);
        assert_eq!(
            choose_profile(Codec::Hevc, None, true, &offered)?,
            Profile::HevcMain10
        );
        Ok(())
    }

    #[test]
    fn a_limit_holds_every_second_under_itself() {
        let mut settings = guest::EncoderSettings {
            max_bit_rate: Some(3_000_000),
            ..guest::EncoderSettings::default()
        };
        let limited = RateControl::VariableBitRate {
            average_bits_per_second: 2_000_000,
            peak_bits_per_second: 2_000_000,
        };
        assert_eq!(rate_control(&settings), limited);
        assert_eq!(buffer_milliseconds(&settings), 500);
        // The buffer and a second at the peak are the limit.
        let mut out = EncoderSettings::new(Profile::H264High, 640, 360);
        out.buffer_milliseconds = buffer_milliseconds(&settings);
        let (buffer, initial) = out.buffer_bits(2_000_000);
        assert_eq!((buffer + 2_000_000, initial), (3_000_000, 750_000));
        // A stream keeps the buffer and the limit it was made with.
        let mut limited_out = out.clone();
        limited_out.hard_limit = true;
        assert!(!needs_new_stream(&limited_out, &out));
        // An average under the peak is the target; one over it is held to it.
        settings.average_bit_rate = Some(1_500_000);
        assert_eq!(
            rate_control(&settings),
            RateControl::VariableBitRate {
                average_bits_per_second: 1_500_000,
                peak_bits_per_second: 2_000_000,
            }
        );
        settings.average_bit_rate = Some(2_500_000);
        assert_eq!(rate_control(&settings), limited);
        settings.max_bit_rate = Some(1);
        settings.average_bit_rate = None;
        assert_eq!(
            rate_control(&settings),
            RateControl::VariableBitRate {
                average_bits_per_second: 1,
                peak_bits_per_second: 1,
            }
        );
        // With no limit, the buffer is a second, and the peak 1.05 times
        // the average.
        settings.max_bit_rate = None;
        settings.average_bit_rate = Some(2_000_000);
        assert_eq!(buffer_milliseconds(&settings), 1000);
        assert_eq!(
            rate_control(&settings),
            RateControl::VariableBitRate {
                average_bits_per_second: 2_000_000,
                peak_bits_per_second: 2_100_000,
            }
        );
        settings.average_bit_rate = Some(u32::MAX);
        assert_eq!(
            rate_control(&settings),
            RateControl::VariableBitRate {
                average_bits_per_second: u32::MAX,
                peak_bits_per_second: u32::MAX,
            }
        );
    }

    #[test]
    fn frame_rates_read_as_fractions() {
        assert_eq!(frame_rate_of(30.0), Some(FrameRate::new(30, 1)));
        assert_eq!(frame_rate_of(59.94), Some(FrameRate::new(60_000, 1001)));
        assert_eq!(frame_rate_of(23.976), Some(FrameRate::new(24_000, 1001)));
        assert_eq!(frame_rate_of(12.5), Some(FrameRate::new(12_500, 1000)));
        assert_eq!(frame_rate_of(0.0), None);
    }
}

#[cfg(test)]
mod fuzz;
