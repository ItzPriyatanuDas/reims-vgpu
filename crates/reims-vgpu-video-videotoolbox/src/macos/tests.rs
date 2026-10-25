//! The backend's encoders and decoders against each other, on the host's
//! VideoToolbox: pictures drawn here are encoded on the hardware encoders,
//! decoded back on the hardware and software decoders, and compared with
//! what was drawn. Nothing here is read from a file.

use std::time::Duration;

use reims_vgpu_video_codec::{h264, hevc};
use reims_vgpu_video_core::backend::{
    BackendDecoder, BackendError, Codec, CompressedFrame, DecoderBackend, DecoderConfig,
    PixelFormat,
};
use reims_vgpu_video_core::encoder_backend::{
    BackendEncoder, EncodedFrame, EncoderBackend, EncoderConfig, EncoderSettings, SourceFrame,
};
use reims_vgpu_video_core::plist::Value;
use reims_vgpu_video_core::runtime::{Runtime, StdRuntime};
use reims_vgpu_video_core::surfaces::{PlaneDestination, PlaneSource};
use reims_vgpu_video_core::wire::message::MediaTime;

use super::cf::{Cf, constant};
use super::decoder::{Decoder, DecoderChoice, VideoToolboxBackend};
use super::encoder::{Encoder, EncoderChoice, VideoToolboxEncoderBackend, properties};
use super::session::Session;

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;
const FRAMES: i64 = 30;
const TIMESCALE: i32 = 30;

/// The text of one of VideoToolbox's constant strings, as the guest sends
/// it: a key or a value.
fn name(constant: Cf<'_>) -> String {
    constant.string().unwrap_or_default()
}

fn time(value: i64) -> MediaTime {
    MediaTime {
        value,
        timescale: TIMESCALE,
        flags: MediaTime::FLAG_VALID,
        epoch: 0,
    }
}

/// One drawn picture, 4:2:0 in two planes, with no padding.
struct Picture {
    format: PixelFormat,
    luma: Vec<u8>,
    chroma: Vec<u8>,
}

impl Picture {
    /// Frame `index` of a moving picture: a diagonal ramp, a square that
    /// slides, and chroma that turns, in the range `format` holds.
    fn draw(format: PixelFormat, index: i64) -> Self {
        let (w, h) = (WIDTH as usize, HEIGHT as usize);
        let ten = format.bytes_per_sample() == 2;
        let shift = index as usize * 4;
        let sample = |value: f64| -> u16 {
            // Video range: 16..235 at 8 bits, 64..940 at 10.
            let (low, span) = if ten { (64.0, 876.0) } else { (16.0, 219.0) };
            (low + value.clamp(0.0, 1.0) * span).round() as u16
        };
        let chroma_sample = |value: f64| -> u16 {
            let (low, span) = if ten { (64.0, 896.0) } else { (16.0, 224.0) };
            (low + value.clamp(0.0, 1.0) * span).round() as u16
        };
        let mut luma = Vec::with_capacity(w * h * 2);
        let put = |plane: &mut Vec<u8>, value: u16| {
            if ten {
                plane.extend_from_slice(&(value << 6).to_le_bytes());
            } else {
                plane.push(value as u8);
            }
        };
        for y in 0..h {
            for x in 0..w {
                let ramp = ((x + y + shift) % 512) as f64 / 511.0;
                let inside = (x + w - shift % w) % w < 120 && (y + shift / 2) % h < 90;
                put(&mut luma, sample(if inside { 0.9 } else { ramp * 0.7 }));
            }
        }
        let mut chroma = Vec::with_capacity(w * h);
        for y in 0..h / 2 {
            for x in 0..w / 2 {
                let angle = (x as f64 / 40.0) + (index as f64 / 10.0);
                put(&mut chroma, chroma_sample(0.5 + 0.3 * angle.sin()));
                put(
                    &mut chroma,
                    chroma_sample(0.5 + 0.3 * (y as f64 / 30.0).cos()),
                );
            }
        }
        Self {
            format,
            luma,
            chroma,
        }
    }

    fn planes(&self) -> [PlaneSource<'_>; 2] {
        let row = WIDTH as usize * self.format.bytes_per_sample();
        [
            PlaneSource {
                bytes: &self.luma,
                bytes_per_row: row,
                rows: HEIGHT,
            },
            PlaneSource {
                bytes: &self.chroma,
                bytes_per_row: row,
                rows: HEIGHT / 2,
            },
        ]
    }
}

/// The settings of a session with `properties`, sorted as core sorts them.
fn settings(mut properties: Vec<(String, Value)>) -> EncoderSettings {
    properties.sort_by(|a, b| a.0.cmp(&b.0));
    EncoderSettings {
        properties,
        ..EncoderSettings::default()
    }
}

/// `make`'s first value that `ready` takes, made again every 50 ms for up
/// to a minute while the host's hardware encoders are all taken; after
/// that, the last one made. The Mac's 32 hardware encoder sessions are
/// shared by every process: while all are taken, VideoToolbox answers
/// -12903 to a new session and lists no encoder's properties, so a backend
/// made then lists its encoders by their codecs alone until they read. In
/// 40 runs of these tests beside two more copies of them, five failed so:
/// one at a session's open, four at a backend's listing (the M3 Max,
/// 2026-10-24).
pub(super) fn when_encoders_are_free<T>(
    mut make: impl FnMut() -> T,
    ready: impl Fn(&T) -> bool,
) -> T {
    let deadline = StdRuntime.deadline_in(Duration::from_secs(60));
    loop {
        let made = make();
        if ready(&made) || StdRuntime.now() >= deadline {
            return made;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// An encoder session, once the Mac has one to give.
fn open_encoder(config: &EncoderConfig, choice: EncoderChoice) -> Result<Encoder, BackendError> {
    when_encoders_are_free(
        || Encoder::open(config, choice, StdRuntime::shared()),
        |opened| !matches!(opened, Err(BackendError::Native { status: -12903, .. })),
    )
}

/// The encoder backend, once both of its encoders have read.
fn encoder_backend() -> VideoToolboxEncoderBackend {
    let made = || VideoToolboxEncoderBackend::new(StdRuntime::shared());
    when_encoders_are_free(made, |backend| {
        let encoders = backend.encoders();
        encoders.len() == 2 && encoders.iter().all(|e| !e.profiles.is_empty())
    })
}

/// Every hardware encoder session of the Mac this process can take, each
/// held until it goes: H.264 sessions at 1920x1080, opened until one
/// fails, and that one's status.
fn every_encoder_held() -> (Vec<Session>, i32) {
    let mut held = Vec::new();
    let status = loop {
        match properties::prepared_session(Codec::H264, 1920, 1080) {
            Ok(session) => held.push(session),
            Err(status) => break status,
        }
        if held.len() > 64 {
            break 0;
        }
    };
    (held, status)
}

/// On the host's hardware: a backend made while every hardware encoder
/// session is taken lists both encoders by their codecs and refuses `+enc`
/// with -12912, as Apple's host does then; once the sessions are let go,
/// `+enc` reads each encoder, and the listing has their profiles and
/// sizes. Ignored: it takes all 32 of the Mac's sessions, which every
/// process shares, for about a second. Run it alone, while nothing else
/// encodes on the host: `cargo test -p reims-vgpu-video-videotoolbox --lib
/// -- --ignored every_encoder_session`.
#[test]
#[ignore]
fn a_backend_made_while_every_encoder_session_is_taken_offers_them_once_one_is_free() {
    let (held, status) = every_encoder_held();
    assert_eq!(status, -12903, "{} sessions held", held.len());
    assert_eq!(
        properties::listed_encoders(),
        [(Codec::H264, true), (Codec::Hevc, true)],
        "the encoder list while every session is taken"
    );
    let backend = VideoToolboxEncoderBackend::new(StdRuntime::shared());
    let listed: Vec<(Codec, usize)> = (backend.encoders().into_iter())
        .map(|e| (e.codec, e.profiles.len()))
        .collect();
    assert_eq!(listed, [(Codec::H264, 0), (Codec::Hevc, 0)]);
    for codec in [Codec::H264, Codec::Hevc] {
        assert_eq!(
            backend.session_capabilities(codec),
            Err(-12912),
            "{codec:?}"
        );
    }
    let sessions = held.len();
    drop(held);
    for codec in [Codec::H264, Codec::Hevc] {
        let started = StdRuntime.now();
        let read = backend.session_capabilities(codec);
        let first = StdRuntime.elapsed_since(started);
        let started = StdRuntime.now();
        let again = backend.session_capabilities(codec);
        let second = StdRuntime.elapsed_since(started);
        let capabilities = read.clone().ok().flatten();
        assert!(
            capabilities
                .as_ref()
                .is_some_and(|c| !c.profiles.is_empty() && c.max_width >= 1920),
            "{codec:?}: {read:?} once {sessions} sessions were let go"
        );
        assert_eq!(again, read);
        println!("{codec:?}: read in {first:?}, asked again in {second:?}");
    }
    assert!(backend.encoders().iter().all(|e| !e.profiles.is_empty()));
}

/// Encodes `FRAMES` pictures of `format` on `choice`, forcing a key frame
/// at each of `keys`: the samples in the order they came, and the record.
pub(super) fn encode(
    codec: Codec,
    format: PixelFormat,
    choice: EncoderChoice,
    properties: Vec<(String, Value)>,
    keys: &[i64],
) -> Result<(Vec<EncodedFrame>, Vec<u8>), BackendError> {
    let config = EncoderConfig {
        codec,
        width: WIDTH,
        height: HEIGHT,
        settings: settings(properties),
    };
    let mut encoder = open_encoder(&config, choice)?;
    let mut samples = Vec::new();
    for index in 0..FRAMES {
        let picture = Picture::draw(format, index);
        let planes = picture.planes();
        samples.extend(encoder.encode(&SourceFrame {
            frame_id: index,
            presentation_time: time(index),
            duration: time(1),
            force_key_frame: keys.contains(&index),
            pixel_format: format,
            width: WIDTH,
            height: HEIGHT,
            planes: &planes,
        })?);
    }
    samples.extend(encoder.flush()?);
    Ok((samples, encoder.configuration()))
}

/// One decoded picture's planes, packed, by the frame ID it carried.
struct Decoded {
    frame_id: i64,
    format: PixelFormat,
    luma: Vec<u8>,
    chroma: Vec<u8>,
}

/// Decodes `samples` with a decoder of `choice`, in the order given.
fn decode(
    codec: Codec,
    record: &[u8],
    samples: &[EncodedFrame],
    choice: DecoderChoice,
) -> Result<Vec<Decoded>, BackendError> {
    let config = DecoderConfig {
        codec,
        width: WIDTH,
        height: HEIGHT,
        configuration: record.to_vec(),
    };
    let mut decoder = Decoder::open(&config, choice)?;
    let mut frames = Vec::new();
    for sample in samples {
        frames.extend(decoder.decode(&CompressedFrame {
            frame_id: sample.frame_id,
            data: &sample.data,
            sample_sizes: &[],
            presentation_time: sample.presentation_time,
            decode_time: sample.decode_time,
            duration: sample.duration,
            new_config: None,
        })?);
    }
    frames.extend(decoder.flush()?);
    frames
        .iter()
        .map(|frame| {
            let format = frame.pixel_format();
            let row = WIDTH as usize * format.bytes_per_sample();
            let mut luma = vec![0; row * HEIGHT as usize];
            let mut chroma = vec![0; row * HEIGHT as usize / 2];
            frame.read_back()?.copy_to(&mut [
                PlaneDestination {
                    bytes: &mut luma,
                    bytes_per_row: row,
                    rows: HEIGHT,
                },
                PlaneDestination {
                    bytes: &mut chroma,
                    bytes_per_row: row,
                    rows: HEIGHT / 2,
                },
            ])?;
            Ok(Decoded {
                frame_id: frame.frame_id(),
                format,
                luma,
                chroma,
            })
        })
        .collect()
}

/// The PSNR of `got` against `want`, samples of `bytes` bytes, little
/// endian with the value in the top bits when two.
fn psnr(want: &[u8], got: &[u8], bytes: usize) -> f64 {
    let value = |plane: &[u8], at: usize| -> f64 {
        if bytes == 2 {
            f64::from(u16::from_le_bytes([plane[at], plane[at + 1]]) >> 6)
        } else {
            f64::from(plane[at])
        }
    };
    let peak = if bytes == 2 { 1023.0 } else { 255.0 };
    let count = want.len() / bytes;
    let error: f64 = (0..count)
        .map(|i| (value(want, i * bytes) - value(got, i * bytes)).powi(2))
        .sum::<f64>()
        / count as f64;
    if error == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (peak * peak / error).log10()
    }
}

/// The NAL units of a sample of 4-byte lengths, or None where the lengths
/// do not add up to the sample.
fn nal_units(sample: &[u8]) -> Option<Vec<&[u8]>> {
    let mut units = Vec::new();
    let mut rest = sample;
    while !rest.is_empty() {
        let length = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        units.push(rest.get(4..4 + length)?);
        rest = &rest[4 + length..];
    }
    Some(units)
}

/// What every stream an encoder gives must be: one sample a source frame,
/// a sync first, samples of 4-byte NAL units and no parameter sets, a
/// record that parses with 4-byte lengths, and decoded pictures near what
/// was drawn.
fn check_stream(
    codec: Codec,
    format: PixelFormat,
    samples: &[EncodedFrame],
    record: &[u8],
) -> Result<(), BackendError> {
    assert_eq!(samples.len(), FRAMES as usize, "{codec:?}");
    assert!(
        samples[0].sync && !samples[0].depends_on_others,
        "{codec:?}"
    );
    let mut ids: Vec<i64> = samples.iter().map(|s| s.frame_id).collect();
    ids.sort_unstable();
    assert_eq!(ids, (0..FRAMES).collect::<Vec<_>>());
    for sample in samples {
        assert_eq!(sample.presentation_time, time(sample.frame_id));
        let units = nal_units(&sample.data);
        assert!(
            units.is_some(),
            "{codec:?}: lengths that are not 4-byte NAL units'"
        );
        for unit in units.unwrap_or_default() {
            let parameter_set = match codec {
                // SPS 7, PPS 8.
                Codec::H264 => matches!(unit[0] & 0x1f, 7 | 8),
                // VPS 32, SPS 33, PPS 34.
                _ => matches!((unit[0] >> 1) & 0x3f, 32..=34),
            };
            assert!(!parameter_set, "{codec:?}: a parameter set in a sample");
        }
    }
    match codec {
        Codec::H264 => {
            let avcc = h264::avcc::DecoderConfiguration::parse(record)
                .map_err(|e| BackendError::BadData(format!("the avcC: {e}")))?;
            assert_eq!(avcc.length_size, 4);
            assert_eq!(avcc.sequence_parameter_sets.len(), 1);
            assert_eq!(avcc.picture_parameter_sets.len(), 1);
        }
        _ => {
            let hvcc = hevc::hvcc::DecoderConfiguration::parse(record)
                .map_err(|e| BackendError::BadData(format!("the hvcC: {e}")))?;
            assert_eq!(hvcc.length_size, 4);
            assert_eq!(
                u32::from(hvcc.bit_depth_luma_minus8) + 8,
                if format.bytes_per_sample() == 2 {
                    10
                } else {
                    8
                }
            );
        }
    }
    let decoded = decode(codec, record, samples, DecoderChoice::Hardware)?;
    assert_eq!(decoded.len(), FRAMES as usize);
    let bytes = format.bytes_per_sample();
    for frame in &decoded {
        assert_eq!(frame.format, format);
        let drawn = Picture::draw(format, frame.frame_id);
        let (luma, chroma) = (
            psnr(&drawn.luma, &frame.luma, bytes),
            psnr(&drawn.chroma, &frame.chroma, bytes),
        );
        assert!(
            luma > 30.0 && chroma > 30.0,
            "{codec:?} frame {}: luma {luma:.1} dB, chroma {chroma:.1} dB",
            frame.frame_id
        );
    }
    Ok(())
}

#[test]
fn the_keys_are_the_names_the_guest_sends() {
    assert_eq!(
        name(constant!(
            video_toolbox::kVTCompressionPropertyKey_AllowFrameReordering
        )),
        "AllowFrameReordering"
    );
    assert_eq!(
        name(constant!(
            video_toolbox::kVTProfileLevel_HEVC_Main10_AutoLevel
        )),
        "HEVC_Main10_AutoLevel"
    );
}

#[test]
fn h264_and_hevc_encode_on_the_hardware_and_decode_back_near_their_source()
-> Result<(), BackendError> {
    let cases = [
        (
            Codec::H264,
            PixelFormat::Nv12VideoRange,
            constant!(video_toolbox::kVTProfileLevel_H264_High_AutoLevel),
        ),
        (
            Codec::Hevc,
            PixelFormat::Nv12VideoRange,
            constant!(video_toolbox::kVTProfileLevel_HEVC_Main_AutoLevel),
        ),
        (
            Codec::Hevc,
            PixelFormat::P010VideoRange,
            constant!(video_toolbox::kVTProfileLevel_HEVC_Main10_AutoLevel),
        ),
    ];
    for (codec, format, profile) in cases {
        let key = name(constant!(
            video_toolbox::kVTCompressionPropertyKey_ProfileLevel
        ));
        let properties = vec![(key, Value::String(name(profile)))];
        let (samples, record) = encode(codec, format, EncoderChoice::Hardware, properties, &[])?;
        check_stream(codec, format, &samples, &record)?;
    }
    Ok(())
}

#[test]
fn a_ten_bit_source_encodes_on_a_main_profile_session() -> Result<(), BackendError> {
    let properties = vec![(
        name(constant!(
            video_toolbox::kVTCompressionPropertyKey_ProfileLevel
        )),
        Value::String(name(constant!(
            video_toolbox::kVTProfileLevel_HEVC_Main_AutoLevel
        ))),
    )];
    let (samples, record) = encode(
        Codec::Hevc,
        PixelFormat::P010VideoRange,
        EncoderChoice::Hardware,
        properties,
        &[],
    )?;
    // Converted to 8 bits by VideoToolbox, as the profile says.
    check_stream(Codec::Hevc, PixelFormat::Nv12VideoRange, &samples, &record)
}

#[test]
fn the_hardware_and_software_decoders_agree_bit_for_bit() -> Result<(), BackendError> {
    for (codec, format) in [
        (Codec::H264, PixelFormat::Nv12VideoRange),
        (Codec::Hevc, PixelFormat::Nv12VideoRange),
    ] {
        let (samples, record) = encode(codec, format, EncoderChoice::Hardware, Vec::new(), &[])?;
        let hardware = decode(codec, &record, &samples, DecoderChoice::Hardware)?;
        let software = decode(codec, &record, &samples, DecoderChoice::Software)?;
        assert_eq!(hardware.len(), software.len());
        for (a, b) in hardware.iter().zip(&software) {
            assert_eq!(a.frame_id, b.frame_id, "{codec:?}: decode order");
            assert!(
                a.luma == b.luma && a.chroma == b.chroma,
                "{codec:?} frame {} differs",
                a.frame_id
            );
        }
    }
    Ok(())
}

#[test]
fn a_forced_key_frame_is_a_sync_sample() -> Result<(), BackendError> {
    for codec in [Codec::H264, Codec::Hevc] {
        let (interval, reordering) = (
            name(constant!(
                video_toolbox::kVTCompressionPropertyKey_MaxKeyFrameInterval
            )),
            name(constant!(
                video_toolbox::kVTCompressionPropertyKey_AllowFrameReordering
            )),
        );
        let properties = vec![
            (interval, Value::Integer(1000.into())),
            (reordering, Value::Boolean(false)),
        ];
        let (samples, _) = encode(
            codec,
            PixelFormat::Nv12VideoRange,
            EncoderChoice::Hardware,
            properties,
            &[12],
        )?;
        // The first and the forced one are sync samples. The HEVC encoder
        // adds key frames of its own when other sessions share it (IDR
        // pictures at frames 1 to 4 in three of four parallel runs), so
        // the rest are checked against the bitstream: a sample is flagged
        // sync exactly when its picture is an IDR (H.264) or IRAP (HEVC).
        for sample in &samples {
            let random_access = nal_units(&sample.data)
                .unwrap_or_default()
                .iter()
                .any(|unit| match codec {
                    Codec::H264 => unit[0] & 0x1f == 5,
                    _ => (16..=23).contains(&((unit[0] >> 1) & 0x3f)),
                });
            assert_eq!(
                sample.sync, random_access,
                "{codec:?} frame {}",
                sample.frame_id
            );
            assert_eq!(sample.depends_on_others, !sample.sync);
        }
        let synced = |id: i64| samples.iter().any(|s| s.frame_id == id && s.sync);
        assert!(synced(0) && synced(12), "{codec:?}");
        // Without reordering, samples come in source order with no decode
        // time of their own, and each after the first depends on another.
        let ids: Vec<i64> = samples.iter().map(|s| s.frame_id).collect();
        assert_eq!(ids, (0..FRAMES).collect::<Vec<_>>(), "{codec:?}");
        assert!(samples.iter().all(|s| !s.decode_time.is_valid()));
    }
    Ok(())
}

/// The bits of an encode of the drawn picture at `target` bit/s, or at the
/// encoder's own choice where `target` is None, per second.
fn bit_rate(codec: Codec, target: Option<i64>) -> Result<f64, BackendError> {
    let (rate, frame_rate, real_time) = (
        name(constant!(
            video_toolbox::kVTCompressionPropertyKey_AverageBitRate
        )),
        name(constant!(
            video_toolbox::kVTCompressionPropertyKey_ExpectedFrameRate
        )),
        name(constant!(video_toolbox::kVTCompressionPropertyKey_RealTime)),
    );
    let mut properties = vec![
        (frame_rate, Value::Integer(i64::from(TIMESCALE).into())),
        (real_time, Value::Boolean(false)),
    ];
    if let Some(target) = target {
        properties.push((rate, Value::Integer(target.into())));
    }
    let (samples, _) = encode(
        codec,
        PixelFormat::Nv12VideoRange,
        EncoderChoice::Hardware,
        properties,
        &[],
    )?;
    let bits: usize = samples.iter().map(|s| s.data.len() * 8).sum();
    Ok(bits as f64 / (FRAMES as f64 / f64::from(TIMESCALE)))
}

/// An average bit rate the picture would exceed is kept. The encoders do
/// not stuff, so a target above what the picture costs checks the picture,
/// not the rate: at 1 Mbit/s, the HEVC encoder's own 0.32 Mbit/s became
/// 0.39 to 0.43, and so failed a bound of 0.4 of the target now and then
/// as other sessions shared the encoder. At 200 kbit/s, which both
/// encoders' own choices exceed by 1.6 times or more, they kept to 1.05 to
/// 1.12 of it, shared or alone (the M3 Max, 2026-10-24).
#[test]
fn the_average_bit_rate_is_kept_near_its_target() -> Result<(), BackendError> {
    let target = 200_000;
    for codec in [Codec::H264, Codec::Hevc] {
        let own = bit_rate(codec, None)?;
        assert!(
            own > 1.4 * target as f64,
            "{codec:?}: the picture costs {own:.0} bit/s, too near {target} to check the rate"
        );
        let achieved = bit_rate(codec, Some(target))?;
        assert!(
            achieved > 0.75 * target as f64 && achieved < 1.3 * target as f64,
            "{codec:?}: {achieved:.0} bit/s for a target of {target}, {own:.0} without one"
        );
    }
    Ok(())
}

/// The bits of one second's encode of the drawn picture, at the encoder's
/// own rate, with a `DataRateLimits` of `limit` bytes a second set after
/// the session opened: after the guest's `epfr` prepared it where
/// `prepared` is set, else before the first frame.
fn rate_with_a_late_limit(codec: Codec, limit: i64, prepared: bool) -> Result<f64, BackendError> {
    let (frame_rate, real_time, limits) = (
        name(constant!(
            video_toolbox::kVTCompressionPropertyKey_ExpectedFrameRate
        )),
        name(constant!(video_toolbox::kVTCompressionPropertyKey_RealTime)),
        name(constant!(
            video_toolbox::kVTCompressionPropertyKey_DataRateLimits
        )),
    );
    let properties = vec![
        (frame_rate, Value::Integer(i64::from(TIMESCALE).into())),
        (real_time, Value::Boolean(false)),
    ];
    let config = EncoderConfig {
        codec,
        width: WIDTH,
        height: HEIGHT,
        settings: settings(properties.clone()),
    };
    let mut encoder = open_encoder(&config, EncoderChoice::Hardware)?;
    if prepared {
        encoder.prepare()?;
    }
    let mut limited = properties;
    limited.push((
        limits,
        Value::Array(vec![Value::Integer(limit.into()), Value::Integer(1.into())]),
    ));
    encoder.reconfigure(&settings(limited))?;
    let mut bytes = 0;
    for index in 0..FRAMES {
        let picture = Picture::draw(PixelFormat::Nv12VideoRange, index);
        let planes = picture.planes();
        let out = encoder.encode(&SourceFrame {
            frame_id: index,
            presentation_time: time(index),
            duration: time(1),
            force_key_frame: false,
            pixel_format: PixelFormat::Nv12VideoRange,
            width: WIDTH,
            height: HEIGHT,
            planes: &planes,
        })?;
        bytes += out.iter().map(|s| s.data.len()).sum::<usize>();
    }
    bytes += encoder.flush()?.iter().map(|s| s.data.len()).sum::<usize>();
    Ok(bytes as f64 * 8.0 / (FRAMES as f64 / f64::from(TIMESCALE)))
}

/// A `DataRateLimits` set after the guest's `epfr` is lost, and one set
/// before it, or with no `epfr` at all, is kept, as behind Apple's host,
/// which prepares AVE at `epfr` alone. AVE takes the limit only before it
/// is prepared: in process, vtencode's limit of 1.5 Mbit/s set after an
/// explicit prepare left 720p at AVE's own 5.4 Mbit/s, and set before it,
/// or with no prepare, kept every second at 0.74 of it or less (the M3
/// Max, macOS 27.0.1, 2026-10-24). The limit here is a third of what
/// the picture costs.
#[test]
fn a_limit_set_after_the_guests_prepare_is_lost_as_behind_apples_host() -> Result<(), BackendError>
{
    for codec in [Codec::H264, Codec::Hevc] {
        let own = bit_rate(codec, None)?;
        let limit = (own / 8.0 / 3.0) as i64;
        let kept = rate_with_a_late_limit(codec, limit, false)?;
        let lost = rate_with_a_late_limit(codec, limit, true)?;
        let bits = limit as f64 * 8.0;
        println!("{codec:?}: own {own:.0}, limit {bits:.0}, kept {kept:.0}, lost {lost:.0} bit/s");
        assert!(
            kept <= 1.05 * bits,
            "{codec:?}: {kept:.0} bit/s for a limit of {bits:.0} set before the prepare"
        );
        assert!(
            lost > 2.0 * bits,
            "{codec:?}: {lost:.0} bit/s for a limit of {bits:.0} set after the prepare, \
             {own:.0} without one"
        );
    }
    Ok(())
}

#[test]
fn a_property_the_encoder_does_not_list_is_refused() {
    let backend = encoder_backend();
    let (rate, profile) = (
        name(constant!(
            video_toolbox::kVTCompressionPropertyKey_AverageBitRate
        )),
        name(constant!(
            video_toolbox::kVTCompressionPropertyKey_ProfileLevel
        )),
    );
    for codec in [Codec::H264, Codec::Hevc] {
        assert_eq!(
            backend.check_property(codec, &rate, &Value::Integer(2_000_000.into())),
            Ok(())
        );
        assert_eq!(
            backend.check_property(codec, "NoSuchProperty", &Value::Boolean(true)),
            Err(-12900)
        );
        assert_eq!(
            backend.check_property(codec, &profile, &Value::String("Not_A_Profile".into())),
            Err(-12900)
        );
    }
}

#[test]
fn corrupt_and_truncated_samples_fail_without_harm() -> Result<(), BackendError> {
    for codec in [Codec::H264, Codec::Hevc] {
        let (samples, record) = encode(
            codec,
            PixelFormat::Nv12VideoRange,
            EncoderChoice::Hardware,
            Vec::new(),
            &[],
        )?;
        let config = DecoderConfig {
            codec,
            width: WIDTH,
            height: HEIGHT,
            configuration: record.clone(),
        };
        let mut decoder = Decoder::open(&config, DecoderChoice::Hardware)?;
        let first = &samples[0];
        let mut damaged: Vec<Vec<u8>> = Vec::new();
        // Cut short, past its length.
        damaged.push(first.data[..first.data.len() / 2].to_vec());
        // A length longer than the sample.
        let mut long = first.data.clone();
        long[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        damaged.push(long);
        // Its slice data overwritten.
        let mut noise = first.data.clone();
        for (i, byte) in noise.iter_mut().enumerate().skip(16) {
            *byte = (i as u8).wrapping_mul(151).wrapping_add(7);
        }
        damaged.push(noise);
        // Nothing at all, and a single byte.
        damaged.push(Vec::new());
        damaged.push(vec![0]);
        for (index, data) in damaged.iter().enumerate() {
            let result = decoder.decode(&CompressedFrame {
                frame_id: 100 + index as i64,
                data,
                sample_sizes: &[],
                presentation_time: time(0),
                decode_time: MediaTime::INVALID,
                duration: time(1),
                new_config: None,
            });
            // Bad data, or a frame emitted that the session sends as it
            // came; never a panic, a hang or a malfunction.
            let malfunction = match &result {
                Err(BackendError::Malfunction(why)) => Some(why.clone()),
                _ => None,
            };
            assert_eq!(malfunction, None, "{codec:?} damage {index}");
        }
        // The decoder goes on: the stream from its key frame decodes whole.
        let mut decoded = 0;
        for sample in &samples {
            decoded += decoder
                .decode(&CompressedFrame {
                    frame_id: sample.frame_id,
                    data: &sample.data,
                    sample_sizes: &[],
                    presentation_time: sample.presentation_time,
                    decode_time: sample.decode_time,
                    duration: sample.duration,
                    new_config: None,
                })?
                .len();
        }
        decoded += decoder.flush()?.len();
        assert_eq!(decoded, FRAMES as usize, "{codec:?}");
    }
    Ok(())
}

#[test]
fn the_decoders_list_what_the_hardware_decodes() {
    let backend = VideoToolboxBackend::new();
    let listed = backend.capabilities();
    for codec in [Codec::H264, Codec::Hevc] {
        let found: Vec<_> = listed.iter().filter(|c| c.codec == codec).collect();
        assert_eq!(
            found.len(),
            1,
            "{codec:?} is listed on every Apple silicon Mac"
        );
        for listed in found {
            assert!(listed.profiles.iter().all(|p| p.max_level.is_some()));
        }
    }
}

/// A guest's sample buffer of several samples decodes as one sample, to
/// one frame at most. VideoToolbox decodes each sample of a sample buffer
/// as a frame of its own, so three copies of a key frame came back as
/// three frames, and a `dfrm` of 128 MiB of copies of a small one would be
/// hundreds of thousands, held at once: 1,000 copies of a 1080p key frame
/// held 3.1 GB.
#[test]
fn a_sample_buffer_of_several_samples_decodes_to_one_frame_at_most() -> Result<(), BackendError> {
    for codec in [Codec::H264, Codec::Hevc] {
        let (samples, record) = encode(
            codec,
            PixelFormat::Nv12VideoRange,
            EncoderChoice::Hardware,
            Vec::new(),
            &[],
        )?;
        let key = &samples[0];
        let copies = 3;
        let data = key.data.repeat(copies);
        let sizes = vec![u32::try_from(key.data.len()).unwrap_or(u32::MAX); copies];
        let config = DecoderConfig {
            codec,
            width: WIDTH,
            height: HEIGHT,
            configuration: record,
        };
        let mut decoder = Decoder::open(&config, DecoderChoice::Hardware)?;
        let frame = |data, sample_sizes| CompressedFrame {
            frame_id: key.frame_id,
            data,
            sample_sizes,
            presentation_time: key.presentation_time,
            decode_time: key.decode_time,
            duration: key.duration,
            new_config: None,
        };
        let frames = decoder
            .decode(&frame(&data, &sizes))
            .map_or(0, |frames| frames.len());
        assert!(
            frames <= 1,
            "{codec:?}: {frames} frames from one sample buffer"
        );
        // The decoder goes on, and the key frame alone decodes.
        assert_eq!(
            decoder.decode(&frame(&key.data, &[]))?.len(),
            1,
            "{codec:?}"
        );
    }
    Ok(())
}

/// A value of another type than the hardware encoder gives its key is
/// refused before it reaches the session: `ExpectedFrameRate` as a string
/// ends the process in `VTCompressionSessionSetProperty`, with an uncaught
/// NSInvalidArgumentException, on both encoders. Numbers still go through.
#[test]
fn a_value_of_another_type_than_the_encoder_lists_is_refused() {
    let backend = encoder_backend();
    let (frame_rate, real_time) = (
        name(constant!(
            video_toolbox::kVTCompressionPropertyKey_ExpectedFrameRate
        )),
        name(constant!(video_toolbox::kVTCompressionPropertyKey_RealTime)),
    );
    for codec in [Codec::H264, Codec::Hevc] {
        for value in [
            Value::String("30".into()),
            Value::Array(vec![Value::Integer(30.into())]),
            Value::Data(vec![30]),
        ] {
            assert_eq!(
                backend.check_property(codec, &frame_rate, &value),
                Err(-12900),
                "{codec:?} {value:?}"
            );
        }
        assert_eq!(
            backend.check_property(codec, &frame_rate, &Value::Real(29.97)),
            Ok(())
        );
        assert_eq!(
            backend.check_property(codec, &real_time, &Value::Integer(1.into())),
            Err(-12900)
        );
    }
}

/// A frame the encoder drops comes back as a frame with no data, which
/// core emits to the guest as dropped. VideoToolbox drops every frame of a
/// session one pixel wide or tall (status 0, `kVTEncodeInfo_FrameDropped`,
/// no sample, on both hardware encoders), and those frames came back as
/// nothing, so core held each, with its surface, until the guest completed
/// its frames.
#[test]
fn frames_the_encoder_drops_come_back_as_dropped() -> Result<(), BackendError> {
    for codec in [Codec::H264, Codec::Hevc] {
        let (width, height) = (1, 64);
        let config = EncoderConfig {
            codec,
            width,
            height,
            settings: EncoderSettings::default(),
        };
        let mut encoder = open_encoder(&config, EncoderChoice::Hardware)?;
        let luma = vec![128u8; 64];
        let chroma = vec![128u8; 64];
        let mut frames = Vec::new();
        for index in 0..3 {
            let planes = [
                PlaneSource {
                    bytes: &luma,
                    bytes_per_row: 1,
                    rows: height,
                },
                PlaneSource {
                    bytes: &chroma,
                    bytes_per_row: 2,
                    rows: height / 2,
                },
            ];
            frames.extend(encoder.encode(&SourceFrame {
                frame_id: index,
                presentation_time: time(index),
                duration: time(1),
                force_key_frame: false,
                pixel_format: PixelFormat::Nv12VideoRange,
                width,
                height,
                planes: &planes,
            })?);
        }
        frames.extend(encoder.flush()?);
        let mut ids: Vec<i64> = frames.iter().map(|frame| frame.frame_id).collect();
        ids.sort_unstable();
        assert_eq!(ids, [0, 1, 2], "{codec:?}");
        assert!(
            frames.iter().all(|frame| frame.data.is_empty()),
            "{codec:?}"
        );
    }
    Ok(())
}

/// A format description of another size than its stream's gives frames of
/// the stream's picture, cut to the description's size where that is
/// smaller, as Apple's host writes the picture alone into the guest's
/// buffer: VideoToolbox's own frames were the description's size.
#[test]
fn a_description_of_another_size_gives_the_streams_picture() -> Result<(), BackendError> {
    for codec in [Codec::H264, Codec::Hevc] {
        let (samples, record) = encode(
            codec,
            PixelFormat::Nv12VideoRange,
            EncoderChoice::Hardware,
            Vec::new(),
            &[],
        )?;
        // The first frame's luma, at the stream's size and stride, and the
        // frame's size.
        let first = |width: u32, height: u32| -> Result<((u32, u32), Vec<u8>), BackendError> {
            let config = DecoderConfig {
                codec,
                width,
                height,
                configuration: record.clone(),
            };
            let mut decoder = Decoder::open(&config, DecoderChoice::Hardware)?;
            let key = &samples[0];
            let mut frames = decoder.decode(&CompressedFrame {
                frame_id: key.frame_id,
                data: &key.data,
                sample_sizes: &[],
                presentation_time: key.presentation_time,
                decode_time: key.decode_time,
                duration: key.duration,
                new_config: None,
            })?;
            frames.extend(decoder.flush()?);
            let frame = frames
                .first()
                .ok_or_else(|| BackendError::Malfunction("no frame".into()))?;
            let row = WIDTH as usize;
            let mut luma = vec![0; row * HEIGHT as usize];
            let mut chroma = vec![0; row * HEIGHT as usize / 2];
            frame.read_back()?.copy_to(&mut [
                PlaneDestination {
                    bytes: &mut luma,
                    bytes_per_row: row,
                    rows: HEIGHT,
                },
                PlaneDestination {
                    bytes: &mut chroma,
                    bytes_per_row: row,
                    rows: HEIGHT / 2,
                },
            ])?;
            Ok(((frame.width(), frame.height()), luma))
        };
        let (size, picture) = first(WIDTH, HEIGHT)?;
        assert_eq!(size, (WIDTH, HEIGHT), "{codec:?}");
        let (size, larger) = first(WIDTH * 2, HEIGHT * 2)?;
        assert_eq!(size, (WIDTH, HEIGHT), "{codec:?}: a larger description");
        assert!(
            larger == picture,
            "{codec:?}: a larger description's picture"
        );
        for (width, height) in [(32, 16), (1, 1)] {
            let (size, smaller) = first(width, height)?;
            assert_eq!(size, (width, height), "{codec:?}: {width}x{height}");
            let row = WIDTH as usize;
            for y in 0..height as usize {
                let cut = y * row..y * row + width as usize;
                assert!(
                    smaller[cut.clone()] == picture[cut],
                    "{codec:?}: {width}x{height}, row {y}"
                );
            }
        }
    }
    Ok(())
}
