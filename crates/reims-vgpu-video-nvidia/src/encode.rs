//! NVENC behind the codec crate's encoder session: a
//! [`reims_vgpu_video_codec::encode::session::Stream`] over
//! [`NvencEncoder`], and core's encoder backend made of them.
//!
//! The session derives the settings from the guest's properties, as it
//! does for every backend; this module turns them into NVENC's, and NVENC's
//! output into the session's samples. What it decides itself, where NVENC
//! would otherwise:
//! - the key frames: an IDR picture at the first frame, where the guest
//!   forces one, where the key frame interval (frames or time) runs out,
//!   and 2^30 frames after an IDR picture at the latest, by the codec
//!   crate's [`KeyFrames`], which places VA-API's too. A frame's place in
//!   display order from its IDR picture is so held to
//!   [`reims_vgpu_video_codec::encode::gop::MAX_ORDER`]; NVENC writes the
//!   order counts, and how it makes them from that place is not read;
//! - the B pictures between references: the codec crate's default and
//!   limit, within what NVENC's caps allow, never as references;
//! - each sample's decode time stamp, from the codec crate's
//!   [`DecodeTimes`];
//! - the configuration record, from the parameter sets NVENC answers
//!   (`nvEncGetSequenceParams`), which every IDR access unit repeats and
//!   the samples leave out.

use std::collections::VecDeque;
use std::sync::Arc;

use reims_vgpu_video_codec::encode::encoder::{DEFAULT_B_FRAMES, MAX_B_FRAMES};
use reims_vgpu_video_codec::encode::gop::{DecodeTimes, KeyFrames};
use reims_vgpu_video_codec::encode::session::{BackendFailure, SessionEncoder, Stream};
use reims_vgpu_video_codec::encode::{
    self as codec, Coded, EncodedSample, EncoderSettings, Entropy, FailedSample, FrameRate,
    FrameType, InputFrame, Profile, RateControl,
};
use reims_vgpu_video_core::backend::{BackendError, Codec, PixelFormat};
use reims_vgpu_video_core::encoder_backend::{
    BackendEncoder, EncoderBackend, EncoderCapabilities, EncoderConfig,
};

use crate::cuda::Gpu;
use crate::error::Error;
use crate::nvenc::encoder::{
    Bitstream, EncodeParams, EncodeProfile, EncodeProfileSupport, EncodeRateControl, NvencEncoder,
    PictureType, RawFrame, Tuning, encode_profiles,
};

impl BackendFailure for Error {
    /// What NVENC lacks is unsupported, planes that do not hold the
    /// picture are bad data, and the rest is the driver's malfunction.
    fn into_backend(self) -> BackendError {
        let text = self.to_string();
        match self {
            Self::Unsupported(_) => BackendError::Unsupported(text),
            Self::PlaneTooSmall(_) | Self::Bitstream(_) => BackendError::BadData(text),
            _ => BackendError::Malfunction(text),
        }
    }
}

/// NVENC's profile for the codec crate's.
fn nvenc_profile(profile: Profile) -> EncodeProfile {
    match profile {
        Profile::H264ConstrainedBaseline => EncodeProfile::H264Baseline,
        Profile::H264Main => EncodeProfile::H264Main,
        Profile::H264High => EncodeProfile::H264High,
        Profile::HevcMain => EncodeProfile::HevcMain,
        Profile::HevcMain10 => EncodeProfile::HevcMain10,
    }
}

/// The codec crate's profile for NVENC's.
fn codec_profile(profile: EncodeProfile) -> Profile {
    match profile {
        EncodeProfile::H264Baseline => Profile::H264ConstrainedBaseline,
        EncodeProfile::H264Main => Profile::H264Main,
        EncodeProfile::H264High => Profile::H264High,
        EncodeProfile::HevcMain => Profile::HevcMain,
        EncodeProfile::HevcMain10 => Profile::HevcMain10,
    }
}

/// Ours: the least bitrate NVENC is given, and so the least VBV buffer, a
/// second of it. It is the VA-API backend's floor (`MIN_BITS_PER_SECOND`
/// in its `va/encoder/params.rs`), so that a guest's `AverageBitRate` of
/// 1 gets the stream the encoder can make on either backend, and NVENC
/// never gets a buffer of a bit. NVENC's own output stops falling above
/// it.
pub(crate) const MIN_BITS_PER_SECOND: u32 = 32_000;

/// NVENC's rate control for the session's: no rate under
/// [`MIN_BITS_PER_SECOND`], and a peak at least the average.
fn rate_control(rate_control: RateControl) -> EncodeRateControl {
    let floor = |rate: u32| rate.max(MIN_BITS_PER_SECOND);
    match rate_control {
        RateControl::ConstantQp {
            intra,
            predicted,
            bidirectional,
        } => EncodeRateControl::ConstantQp {
            intra: intra.into(),
            predicted: predicted.into(),
            bidirectional: bidirectional.into(),
        },
        RateControl::ConstantBitRate { bits_per_second } => EncodeRateControl::ConstantBitRate {
            bits_per_second: floor(bits_per_second),
        },
        RateControl::VariableBitRate {
            average_bits_per_second,
            peak_bits_per_second,
        } => {
            let average = floor(average_bits_per_second);
            EncodeRateControl::VariableBitRate {
                average_bits_per_second: average,
                peak_bits_per_second: peak_bits_per_second.max(average),
                target_quality: None,
            }
        }
        // NVENC's target quality runs 1 (best) to 51, as the quality
        // factor does.
        RateControl::QualityVariableBitRate {
            average_bits_per_second,
            peak_bits_per_second,
            quality,
        } => {
            let average = floor(average_bits_per_second);
            EncodeRateControl::VariableBitRate {
                average_bits_per_second: average,
                peak_bits_per_second: peak_bits_per_second.max(average),
                target_quality: Some(quality.max(1)),
            }
        }
    }
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// The display aspect ratio NVENC derives the sample aspect ratio from.
fn display_aspect(settings: &EncoderSettings) -> Option<(u32, u32)> {
    let (horizontal, vertical) = settings.sample_aspect_ratio?;
    let width = settings.width.saturating_mul(u32::from(horizontal));
    let height = settings.height.saturating_mul(u32::from(vertical));
    let divisor = gcd(width, height).max(1);
    Some((width / divisor, height / divisor))
}

/// The B pictures a stream of `settings` runs, where NVENC allows
/// `max_b_frames`: the codec crate's default and limit, as VA-API's
/// encoder takes them, held to the guest's frame delay. HEVC has B
/// pictures here, as NVENC encodes them.
fn b_frames(settings: &EncoderSettings, max_b_frames: u32) -> u32 {
    let profile = nvenc_profile(settings.profile);
    if !settings.allow_frame_reordering || !profile.has_b_slices() || max_b_frames == 0 {
        return 0;
    }
    settings
        .b_frames
        .unwrap_or(DEFAULT_B_FRAMES)
        .min(settings.max_frame_delay.unwrap_or(u32::MAX))
        .min(MAX_B_FRAMES)
        .min(max_b_frames)
}

/// NVENC's parameters for `settings`.
fn encode_params(settings: &EncoderSettings, b_frames: u32) -> EncodeParams {
    EncodeParams {
        profile: nvenc_profile(settings.profile),
        width: settings.width,
        height: settings.height,
        frame_rate: (
            settings.frame_rate.numerator,
            settings.frame_rate.denominator,
        ),
        rate_control: rate_control(settings.rate_control),
        buffer_milliseconds: settings.buffer_milliseconds,
        // The key frames are ours: NVENC places none of its own.
        gop_length: None,
        b_frames,
        level: settings.level.map(u32::from),
        cabac: Some(settings.h264_entropy() == Entropy::Cabac),
        full_range: settings.full_range,
        colour: settings.colour.map(|c| (c.primaries, c.transfer, c.matrix)),
        display_aspect: display_aspect(settings),
        // Ours: VA-API's quality level runs 1 (best) to 7, as NVENC's
        // presets run P7 (best) to P1; none is the middle, P4.
        preset: settings
            .quality_level
            .map_or(4, |level| 8 - u8::try_from(level.clamp(1, 7)).unwrap_or(4)),
        tuning: Tuning::HighQuality,
    }
}

/// The parameter sets' NAL unit types, which samples leave out.
fn is_parameter_set(codec: codec::Codec, unit: &[u8]) -> bool {
    let Some(&header) = unit.first() else {
        return false;
    };
    match codec {
        codec::Codec::H264 => matches!(header & 0x1f, 7 | 8),
        codec::Codec::Hevc => matches!((header >> 1) & 0x3f, 32..=34),
    }
}

/// The configuration record for NVENC's parameter sets, Annex B.
fn record(codec: codec::Codec, parameter_sets: &[u8]) -> Result<Arc<[u8]>, Error> {
    let units = codec::nal::split_annex_b(parameter_sets);
    let record = match (codec, units.as_slice()) {
        (codec::Codec::H264, [sps, pps]) => Some(codec::h264::avcc(sps, pps)),
        (codec::Codec::Hevc, [vps, sps, pps]) => codec::hevc::hvcc(vps, sps, pps),
        _ => None,
    };
    record.map(Into::into).ok_or_else(|| {
        Error::Unsupported(format!(
            "NVENC's parameter sets are {} NAL units that make no {codec:?} record",
            units.len()
        ))
    })
}

/// A frame NVENC holds, by the index it was sent with.
struct Pending {
    index: u64,
    id: u64,
    pts: i64,
    duration: i64,
}

/// One NVENC session as the codec crate's session drives a stream.
pub(crate) struct NvencStream {
    encoder: NvencEncoder,
    settings: EncoderSettings,
    record: Arc<[u8]>,
    record_due: bool,
    times: DecodeTimes,
    pending: VecDeque<Pending>,
    next_index: u64,
    keys: KeyFrames,
}

impl NvencStream {
    /// A stream for `settings` on `gpu`, where NVENC encodes `support`.
    pub(crate) fn new(
        gpu: Arc<Gpu>,
        settings: EncoderSettings,
        support: &EncodeProfileSupport,
    ) -> Result<Self, Error> {
        if settings.width == 0
            || settings.height == 0
            || !settings.width.is_multiple_of(2)
            || !settings.height.is_multiple_of(2)
        {
            return Err(Error::Unsupported(format!(
                "{}x{}: 4:2:0 needs an even size",
                settings.width, settings.height
            )));
        }
        if settings.frame_rate.numerator == 0
            || settings.frame_rate.denominator == 0
            || settings.timescale == 0
        {
            return Err(Error::Unsupported(
                "a frame rate or timescale of 0".to_owned(),
            ));
        }
        let b_frames = b_frames(&settings, support.max_b_frames);
        let keys = KeyFrames::new(
            settings.key_frame_interval,
            settings.key_frame_interval_duration,
        );
        let encoder = NvencEncoder::new(gpu, &encode_params(&settings, b_frames))?;
        let record = record(settings.profile.codec(), encoder.sequence_parameters())?;
        Ok(Self {
            encoder,
            settings,
            record,
            record_due: true,
            times: DecodeTimes::new(),
            pending: VecDeque::new(),
            next_index: 0,
            keys,
        })
    }

    /// Each frame NVENC wrote, with its own result, in decode order. A
    /// packet is its frame's sample. A bitstream that does not read names
    /// no frame, but every frame NVENC was to write is in the drain that
    /// gave it, so the frames the packets leave pending are the failed
    /// ones: each gets one of the drain's errors, oldest first, and its
    /// decode time stamp is taken so the next samples' stay in step. A
    /// packet of a frame not pending, one NVENC writes after its stream
    /// ended, is left out.
    fn samples(&mut self, bitstreams: Vec<Bitstream>) -> Vec<Coded<BackendError>> {
        let codec = self.settings.profile.codec();
        let mut coded = Vec::with_capacity(bitstreams.len());
        let mut errors = Vec::new();
        for bitstream in bitstreams {
            let packet = match bitstream {
                Ok(packet) => packet,
                Err(e) => {
                    log::warn!("an NVENC bitstream does not read: {e}");
                    errors.push(e);
                    continue;
                }
            };
            let Some(position) = self.pending.iter().position(|p| p.index == packet.index) else {
                log::warn!(
                    "NVENC wrote frame {}, which is not pending; left out",
                    packet.index
                );
                continue;
            };
            let Some(frame) = self.pending.remove(position) else {
                continue;
            };
            let mut data = Vec::with_capacity(packet.data.len());
            for unit in codec::nal::split_annex_b(&packet.data) {
                if !is_parameter_set(codec, unit) {
                    codec::nal::push_length_prefixed(&mut data, unit);
                }
            }
            let (frame_type, sync) = match packet.picture_type {
                PictureType::Idr => (FrameType::Idr, true),
                PictureType::Bidirectional => (FrameType::B, false),
                // An I picture that is not IDR: NVENC writes none with
                // its own GOP off, and it is a P picture's kind here.
                PictureType::Intra
                | PictureType::Predicted
                | PictureType::NonReferencePredicted => (FrameType::P, false),
            };
            coded.push(Ok(EncodedSample {
                id: frame.id,
                data,
                pts: frame.pts,
                dts: self.times.next_dts(),
                duration: frame.duration,
                frame_type,
                is_sync: sync,
                depends_on_others: !matches!(
                    packet.picture_type,
                    PictureType::Idr | PictureType::Intra
                ),
                is_depended_on: packet.picture_type.is_reference(),
                configuration: std::mem::take(&mut self.record_due).then(|| self.record.clone()),
            }));
        }
        if let Some(last) = errors.last().cloned() {
            for (n, frame) in self.pending.drain(..).enumerate() {
                self.times.next_dts();
                let error = errors.get(n).cloned().unwrap_or_else(|| last.clone());
                coded.push(Err(FailedSample {
                    id: frame.id,
                    error: Some(error.into_backend()),
                }));
            }
        }
        coded
    }

    /// Takes NVENC's parameter sets again after a change, and sends a new
    /// record with the next sample where they differ.
    fn refresh_record(&mut self) -> Result<(), Error> {
        let record = record(
            self.settings.profile.codec(),
            self.encoder.sequence_parameters(),
        )?;
        if record != self.record {
            self.record = record;
            self.record_due = true;
        }
        Ok(())
    }
}

impl Stream for NvencStream {
    fn encode(&mut self, frame: &InputFrame<'_>) -> Result<Vec<Coded<BackendError>>, BackendError> {
        let key = self.keys.place(frame.pts, frame.force_key_frame) == 0;
        let index = self.next_index;
        let [luma, chroma] = frame.planes;
        let raw = RawFrame {
            luma: (luma.data, luma.bytes_per_row),
            chroma: (chroma.data, chroma.bytes_per_row),
            index,
            force_idr: key,
        };
        self.pending.push_back(Pending {
            index,
            id: frame.id,
            pts: frame.pts,
            duration: frame.duration,
        });
        let bitstreams = match self.encoder.encode(&raw) {
            Ok(bitstreams) => bitstreams,
            Err(e) => {
                self.pending.pop_back();
                return Err(e.into_backend());
            }
        };
        self.next_index += 1;
        self.times.push(frame.pts);
        Ok(self.samples(bitstreams))
    }

    fn flush(&mut self) -> Result<Vec<Coded<BackendError>>, BackendError> {
        match self.encoder.flush() {
            Ok(bitstreams) => Ok(self.samples(bitstreams)),
            Err(e) => {
                // The frames NVENC held are lost with the end it did not
                // take: the session fails each, and a packet NVENC writes
                // of one later is left out.
                for _ in self.pending.drain(..) {
                    self.times.next_dts();
                }
                Err(e.into_backend())
            }
        }
    }

    fn configuration(&self) -> Arc<[u8]> {
        self.record.clone()
    }

    fn settings(&self) -> &EncoderSettings {
        &self.settings
    }

    fn set_rate_control(&mut self, rate_control: RateControl) -> Result<(), BackendError> {
        let frame_rate = self.encoder.params().frame_rate;
        self.encoder
            .reconfigure(self::rate_control(rate_control), frame_rate)
            .and_then(|()| self.refresh_record())
            .map_err(BackendFailure::into_backend)?;
        self.settings.rate_control = rate_control;
        Ok(())
    }

    fn set_frame_rate(&mut self, frame_rate: FrameRate) -> Result<(), BackendError> {
        let rate_control = self.encoder.params().rate_control;
        self.encoder
            .reconfigure(rate_control, (frame_rate.numerator, frame_rate.denominator))
            .and_then(|()| self.refresh_record())
            .map_err(BackendFailure::into_backend)?;
        self.settings.frame_rate = frame_rate;
        Ok(())
    }

    fn set_key_frame_interval(&mut self, frames: Option<u32>, duration: Option<i64>) {
        self.settings.key_frame_interval = frames;
        self.settings.key_frame_interval_duration = duration;
        self.keys.set_interval(frames, duration);
    }
}

/// The encoders of NVENC on one GPU.
pub struct NvidiaEncoders {
    gpu: Arc<Gpu>,
    support: Vec<EncodeProfileSupport>,
    listed: Vec<EncoderCapabilities>,
}

impl NvidiaEncoders {
    /// The encoders on `gpu`. What NVENC encodes is asked now, once.
    pub fn new(gpu: Arc<Gpu>) -> Result<Self, Error> {
        let support = encode_profiles(Arc::clone(&gpu))?;
        Ok(Self {
            listed: listed(&support),
            support,
            gpu,
        })
    }
}

/// The encoder list: per codec, the profiles NVENC encodes, the largest
/// picture any of them takes, the source formats (8-bit first, as Apple's
/// AVC encoder lists `420v` and `420f` first; 10-bit too where HEVC Main
/// 10 is there), and whether B pictures are.
fn listed(support: &[EncodeProfileSupport]) -> Vec<EncoderCapabilities> {
    [
        (Codec::H264, codec::Codec::H264),
        (Codec::Hevc, codec::Codec::Hevc),
    ]
    .into_iter()
    .filter_map(|(codec, ours)| {
        let of: Vec<&EncodeProfileSupport> = support
            .iter()
            .filter(|s| codec_profile(s.profile).codec() == ours)
            .collect();
        if of.is_empty() {
            return None;
        }
        let mut source_formats = vec![PixelFormat::Nv12VideoRange, PixelFormat::Nv12FullRange];
        if of.iter().any(|s| s.profile.bit_depth() > 8) {
            source_formats.extend([PixelFormat::P010VideoRange, PixelFormat::P010FullRange]);
        }
        Some(EncoderCapabilities {
            codec,
            profiles: of
                .iter()
                .map(|s| u32::from(codec_profile(s.profile).profile_idc()))
                .collect(),
            max_width: of.iter().map(|s| s.max_width).max().unwrap_or(0),
            max_height: of.iter().map(|s| s.max_height).max().unwrap_or(0),
            source_formats,
            frame_reordering: of.iter().any(|s| s.max_b_frames > 0),
        })
    })
    .collect()
}

impl EncoderBackend for NvidiaEncoders {
    fn encoders(&self) -> Vec<EncoderCapabilities> {
        self.listed.clone()
    }

    fn open(&self, config: &EncoderConfig) -> Result<Box<dyn BackendEncoder>, BackendError> {
        if !self
            .listed
            .iter()
            .any(|listed| listed.codec == config.codec)
        {
            return Err(BackendError::Unsupported(format!(
                "NVENC encodes no {:?}",
                config.codec
            )));
        }
        let gpu = Arc::clone(&self.gpu);
        let support = self.support.clone();
        let encoder: SessionEncoder<NvencStream> = SessionEncoder::new(
            config,
            support.iter().map(|s| codec_profile(s.profile)).collect(),
            Box::new(move |settings| {
                let profile = nvenc_profile(settings.profile);
                let Some(of) = support.iter().find(|s| s.profile == profile) else {
                    return Err(BackendError::Unsupported(format!(
                        "NVENC does not encode {profile:?}"
                    )));
                };
                if settings.width < of.min_width
                    || settings.height < of.min_height
                    || settings.width > of.max_width
                    || settings.height > of.max_height
                {
                    return Err(BackendError::Unsupported(format!(
                        "NVENC encodes {profile:?} from {}x{} to {}x{}, not {}x{}",
                        of.min_width,
                        of.min_height,
                        of.max_width,
                        of.max_height,
                        settings.width,
                        settings.height
                    )));
                }
                NvencStream::new(Arc::clone(&gpu), settings.clone(), of)
                    .map_err(BackendFailure::into_backend)
            }),
        )?;
        Ok(Box::new(encoder))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b_pictures_follow_the_settings_within_nvencs_caps() {
        let mut settings = EncoderSettings::new(Profile::H264High, 640, 480);
        assert_eq!(b_frames(&settings, 4), DEFAULT_B_FRAMES);
        settings.b_frames = Some(9);
        assert_eq!(b_frames(&settings, 7), MAX_B_FRAMES);
        assert_eq!(b_frames(&settings, 2), 2);
        settings.max_frame_delay = Some(0);
        assert_eq!(b_frames(&settings, 7), 0);
        settings.max_frame_delay = None;
        settings.allow_frame_reordering = false;
        assert_eq!(b_frames(&settings, 7), 0);
        let mut baseline = EncoderSettings::new(Profile::H264ConstrainedBaseline, 640, 480);
        baseline.b_frames = Some(2);
        assert_eq!(b_frames(&baseline, 7), 0);
        let hevc = EncoderSettings::new(Profile::HevcMain, 640, 480);
        assert_eq!(b_frames(&hevc, 5), DEFAULT_B_FRAMES);
    }

    #[test]
    fn settings_become_nvencs_parameters() {
        let mut settings = EncoderSettings::new(Profile::H264Main, 1280, 720);
        settings.sample_aspect_ratio = Some((4, 3));
        settings.rate_control = RateControl::QualityVariableBitRate {
            average_bits_per_second: 4_000_000,
            peak_bits_per_second: 6_000_000,
            quality: 0,
        };
        settings.quality_level = Some(1);
        let params = encode_params(&settings, 1);
        assert_eq!(params.profile, EncodeProfile::H264Main);
        // 1280x720 of 4:3 samples shows as 5120:2160.
        assert_eq!(params.display_aspect, Some((64, 27)));
        assert_eq!(
            params.rate_control,
            EncodeRateControl::VariableBitRate {
                average_bits_per_second: 4_000_000,
                peak_bits_per_second: 6_000_000,
                target_quality: Some(1),
            }
        );
        assert_eq!(
            (params.preset, params.gop_length, params.b_frames),
            (7, None, 1)
        );
        assert_eq!(params.cabac, Some(true));
        settings.profile = Profile::H264ConstrainedBaseline;
        settings.quality_level = None;
        let params = encode_params(&settings, 0);
        assert_eq!((params.cabac, params.preset), (Some(false), 4));
    }

    /// The configuration NVENC would get for `params`, from zero.
    fn ffi_config(params: &EncodeParams) -> crate::nvenc::ffi::NV_ENC_CONFIG {
        use crate::nvenc::ffi::Zeroed;
        let mut config = crate::nvenc::ffi::NV_ENC_CONFIG::zeroed();
        crate::nvenc::encoder::configure(&mut config, params);
        config
    }

    fn vbr(average_bits_per_second: u32, peak_bits_per_second: u32) -> EncodeRateControl {
        EncodeRateControl::VariableBitRate {
            average_bits_per_second,
            peak_bits_per_second,
            target_quality: None,
        }
    }

    #[test]
    fn rates_under_the_floor_reach_nvenc_at_it() {
        // A guest's AverageBitRate of 1, which the session peaks at 1.05
        // times the average.
        let one = RateControl::VariableBitRate {
            average_bits_per_second: 1,
            peak_bits_per_second: 1,
        };
        assert_eq!(rate_control(one), vbr(32_000, 32_000));
        let under = RateControl::VariableBitRate {
            average_bits_per_second: MIN_BITS_PER_SECOND - 1,
            peak_bits_per_second: (MIN_BITS_PER_SECOND - 1) * 3 / 2,
        };
        assert_eq!(rate_control(under), vbr(32_000, 47_998));
        let cbr = RateControl::ConstantBitRate { bits_per_second: 0 };
        assert_eq!(
            rate_control(cbr),
            EncodeRateControl::ConstantBitRate {
                bits_per_second: 32_000
            }
        );
        let qvbr = RateControl::QualityVariableBitRate {
            average_bits_per_second: 8,
            peak_bits_per_second: 12,
            quality: 25,
        };
        assert_eq!(
            rate_control(qvbr),
            EncodeRateControl::VariableBitRate {
                average_bits_per_second: 32_000,
                peak_bits_per_second: 32_000,
                target_quality: Some(25),
            }
        );
    }

    #[test]
    fn rates_at_the_floor_and_over_it_reach_nvenc_as_asked() {
        let at = RateControl::VariableBitRate {
            average_bits_per_second: MIN_BITS_PER_SECOND,
            peak_bits_per_second: MIN_BITS_PER_SECOND * 3 / 2,
        };
        assert_eq!(rate_control(at), vbr(32_000, 48_000));
        let over = RateControl::VariableBitRate {
            average_bits_per_second: 2_000_000,
            peak_bits_per_second: 3_000_000,
        };
        assert_eq!(rate_control(over), vbr(2_000_000, 3_000_000));
        // A peak under the average is raised to it.
        let low_peak = RateControl::VariableBitRate {
            average_bits_per_second: 2_000_000,
            peak_bits_per_second: 1_000_000,
        };
        assert_eq!(rate_control(low_peak), vbr(2_000_000, 2_000_000));
        let cqp = RateControl::ConstantQp {
            intra: 24,
            predicted: 26,
            bidirectional: 28,
        };
        assert_eq!(
            rate_control(cqp),
            EncodeRateControl::ConstantQp {
                intra: 24,
                predicted: 26,
                bidirectional: 28
            }
        );
    }

    #[test]
    fn the_vbv_buffer_is_a_second_at_the_floored_peak() {
        let mut settings = EncoderSettings::new(Profile::HevcMain, 640, 480);
        settings.rate_control = RateControl::VariableBitRate {
            average_bits_per_second: 1,
            peak_bits_per_second: 1,
        };
        let mut config = ffi_config(&encode_params(&settings, 0));
        assert_eq!(
            (
                config.rcParams.averageBitRate,
                config.rcParams.maxBitRate,
                config.rcParams.vbvBufferSize,
                config.rcParams.vbvInitialDelay
            ),
            (32_000, 32_000, 32_000, 24_000)
        );
        settings.rate_control = RateControl::ConstantBitRate {
            bits_per_second: 5_000_000,
        };
        config = ffi_config(&encode_params(&settings, 0));
        assert_eq!(
            (
                config.rcParams.vbvBufferSize,
                config.rcParams.vbvInitialDelay
            ),
            (5_000_000, 3_750_000)
        );
        // Held to a limit, half a second at the peak.
        settings.rate_control = RateControl::VariableBitRate {
            average_bits_per_second: 2_000_000,
            peak_bits_per_second: 2_000_000,
        };
        settings.buffer_milliseconds = 500;
        config = ffi_config(&encode_params(&settings, 0));
        assert_eq!(
            (
                config.rcParams.maxBitRate,
                config.rcParams.vbvBufferSize,
                config.rcParams.vbvInitialDelay
            ),
            (2_000_000, 1_000_000, 750_000)
        );
    }

    #[test]
    fn parameter_sets_are_told_apart_and_make_records() {
        assert!(is_parameter_set(codec::Codec::H264, &[0x67, 0x64]));
        assert!(is_parameter_set(codec::Codec::H264, &[0x68]));
        assert!(!is_parameter_set(codec::Codec::H264, &[0x65]));
        assert!(is_parameter_set(codec::Codec::Hevc, &[0x40, 0x01]));
        assert!(is_parameter_set(codec::Codec::Hevc, &[0x44, 0x01]));
        assert!(!is_parameter_set(codec::Codec::Hevc, &[0x26, 0x01]));
        let sets = [
            0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f, 0xac, 0, 0, 1, 0x68, 0xee,
        ];
        let avcc = record(codec::Codec::H264, &sets).unwrap_or_else(|_| Arc::from([]));
        assert_eq!(avcc.get(..4), Some(&[1, 0x64, 0x00, 0x1f][..]));
        assert!(record(codec::Codec::H264, &sets[..9]).is_err());
    }
}
