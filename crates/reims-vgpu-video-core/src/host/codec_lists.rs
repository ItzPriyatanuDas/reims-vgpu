//! The host session's own answers: the codec lists (`decs`, `encs`,
//! `meps`) and a decoder's capabilities (`dcap`). The JPEG engine's are
//! `host::jpeg`'s.
//!
//! The lists say what the guest may ask for. This host lists the decoders,
//! encoders and motion-estimation processor its backends report and
//! nothing else: with no motion-estimation backend, no processor, as
//! Apple's host answers when it has none.

use crate::backend::{Codec, CodecCapabilities, ProfileSupport};
use crate::encoder_backend::EncoderCapabilities;
use crate::host::names;
use crate::host::session::reply;
use crate::status::{OsStatus, Status};
use crate::wire::WireError;
use crate::wire::message::Message;
use crate::wire::property_list::{dictionary, integer};

/// The protocol version each list and create reply carries and each list
/// request is checked against: 6, for the decoders, the encoders
/// (`kVTVESHostProtocolVersion`), motion estimation and the capabilities
/// (`kVTVDCapabilitesHostProtocolVersion`) alike.
pub(crate) const PROTOCOL_VERSION: u32 = 6;

/// The rating each listed decoder carries. Apple's hardware decoders carry
/// 400 (measured on macOS 27, 26A434, 2026-10-24: `decs` lists
/// `com.apple.videotoolbox.videodecoder.avd.avc` with VTRating 400); the
/// guest prefers the higher-rated of two decoders for a codec.
pub(crate) const DECODER_RATING: i64 = 400;

/// The `CMClassImplementationID` of this host's decoder for `codec`. The
/// guest registers it as `paravirtualized:<ID>` and sends it back as
/// `,cid` when it opens one. The IDs and names are ours.
pub(crate) fn decoder_id(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "reims-vgpu-video.videodecoder.avc",
        Codec::Hevc => "reims-vgpu-video.videodecoder.hevc",
        Codec::Av1 => "reims-vgpu-video.videodecoder.av1",
        Codec::Vp9 => "reims-vgpu-video.videodecoder.vp9",
    }
}

/// The `VTMaxDecodeLevel` Apple's hardware decoders list for every profile
/// (measured on macOS 27, 26A434, 2026-10-24: 80 for `avd.avc`, above H.264's
/// highest level, 6.2; 186, HEVC's level 6.2, for `avd.hevc`; 19, AV1's
/// `seq_level_idx` for 6.3, for `av01`), which this host lists for a profile
/// whose backend knows no limit. Apple's VP9 decoder lists no level: see
/// [`lists_capabilities`].
const fn apple_max_decode_level(codec: Codec) -> Option<u32> {
    match codec {
        Codec::H264 => Some(80),
        Codec::Hevc => Some(186),
        Codec::Av1 => Some(AV1_MAX_DECODE_LEVEL),
        Codec::Vp9 => None,
    }
}

/// AV1's levels as Apple's host lists them for a hardware decoder, each a
/// `seq_level_idx`: 19 (6.3) to decode, 17 (6.1) to play back, 16 (6.0) to
/// play back HDR (measured on macOS 27, 26A434, 2026-10-24, the same for
/// each profile).
const AV1_MAX_DECODE_LEVEL: u32 = 19;
const AV1_MAX_PLAYBACK_LEVEL: u32 = 17;
const AV1_MAX_HDR_PLAYBACK_LEVEL: u32 = 16;

/// The level the host lists for `profile` of `codec`, if it lists one.
fn max_decode_level(codec: Codec, profile: &ProfileSupport) -> Option<plist::Value> {
    profile
        .max_level
        .or(apple_max_decode_level(codec))
        .map(|level| integer(i64::from(level)))
}

/// Whether the codec's `decs` entry carries VTDecoderCapabilities. Apple's
/// AV1 and VP9 entries carry none, and neither do this host's (measured on
/// macOS 27, 26A434, 2026-10-24: `avd.av1` and `avd.vp9` list their ID,
/// name, IsPreferredInternalPlugin, VTCodecType and VTRating alone; the
/// entry gets the key only from a decoder's IORegistry capabilities).
const fn lists_capabilities(codec: Codec) -> bool {
    matches!(codec, Codec::H264 | Codec::Hevc)
}

fn decoder_name(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "reims-vgpu-video Decoder - AVC",
        Codec::Hevc => "reims-vgpu-video Decoder - HEVC",
        Codec::Av1 => "reims-vgpu-video Decoder - AV1",
        Codec::Vp9 => "reims-vgpu-video Decoder - VP9",
    }
}

fn fourcc_string(codec: Codec) -> plist::Value {
    plist::Value::String(String::from_utf8_lossy(&codec.fourcc().to_be_bytes()).into_owned())
}

/// One entry of the decoder list, in the keys and shapes Apple's entries
/// have (measured shapes: VTCodecType an array of FourCC strings,
/// VTPerProfileDetails keyed by the profile as a decimal string), with
/// VTDecoderCapabilities only where Apple's has them
/// ([`lists_capabilities`]).
fn decoder_entry(capabilities: &CodecCapabilities) -> plist::Value {
    let mut entry = dictionary([
        (
            "CMClassImplementationID",
            plist::Value::String(decoder_id(capabilities.codec).to_owned()),
        ),
        (
            "CMClassImplementationName",
            plist::Value::String(decoder_name(capabilities.codec).to_owned()),
        ),
        ("IsPreferredInternalPlugin", plist::Value::Boolean(false)),
        (
            "VTCodecType",
            plist::Value::Array(vec![fourcc_string(capabilities.codec)]),
        ),
        ("VTRating", integer(DECODER_RATING)),
    ]);
    if lists_capabilities(capabilities.codec) {
        let mut details = plist::Dictionary::new();
        for profile in &capabilities.profiles {
            let mut detail = plist::Dictionary::new();
            if let Some(level) = max_decode_level(capabilities.codec, profile) {
                detail.insert("VTMaxDecodeLevel".into(), level);
            }
            details.insert(
                profile.profile.to_string(),
                plist::Value::Dictionary(detail),
            );
        }
        let profiles = capabilities
            .profiles
            .iter()
            .map(|profile| integer(i64::from(profile.profile)))
            .collect();
        entry.insert(
            "VTDecoderCapabilities".into(),
            plist::Value::Dictionary(dictionary([
                ("VTPerProfileDetails", plist::Value::Dictionary(details)),
                ("VTSupportedProfileArray", plist::Value::Array(profiles)),
            ])),
        );
    }
    plist::Value::Dictionary(entry)
}

/// `decs`: the decoders, then the version. The request's `,ver` is read but
/// not checked.
pub(crate) fn decoder_list(
    request: &Message<'_>,
    decoders: &[CodecCapabilities],
) -> Result<Vec<u8>, WireError> {
    request.u32(names::VERSION)?;
    let entries = decoders.iter().map(decoder_entry).collect();
    let mut reply = reply(request);
    reply.property_list(names::DECODERS, Some(&plist::Value::Array(entries)))?;
    reply.u32(names::VERSION, PROTOCOL_VERSION);
    Ok(reply.finish())
}

/// A decoder's capabilities as `dcap` answers them: per profile, whether
/// it is hardware-accelerated and its highest decode and playback level,
/// then the profiles (measured shape on macOS 27 for `avc1`:
/// VTIsHDRAllowedOnDevice, VTPerProfileSupport, VTSupportedProfiles).
/// VTIsHDRAllowedOnDevice is true for H.264 and HEVC, as Apple's host
/// answers on a device that allows HDR (measured on macOS 27, 26A434,
/// 2026-10-24, for both): a backend decodes an HDR stream as it decodes any
/// other, and the profiles say which it takes. What the guest then displays
/// is its own business. AV1's are in a shape of their own, and VP9 has
/// none: Apple's host answers NULL for `vp09`, since
/// `vtCreateDecoderCapabilitiesDictionary` knows no VP9 (measured on macOS
/// 27, 26A434, 2026-10-24: `,dca` empty, then `,ver`).
fn capabilities_dictionary(capabilities: &CodecCapabilities) -> Option<plist::Value> {
    match capabilities.codec {
        Codec::Av1 => return Some(av1_capabilities_dictionary(capabilities)),
        Codec::Vp9 => return None,
        Codec::H264 | Codec::Hevc => {}
    }
    let mut per_profile = plist::Dictionary::new();
    for profile in &capabilities.profiles {
        let mut support = dictionary([("VTIsHardwareAccelerated", plist::Value::Boolean(true))]);
        if let Some(level) = max_decode_level(capabilities.codec, profile) {
            support.insert("VTMaxDecodeLevel".into(), level.clone());
            support.insert("VTMaxPlaybackLevel".into(), level);
        }
        per_profile.insert(
            profile.profile.to_string(),
            plist::Value::Dictionary(support),
        );
    }
    let profiles = capabilities
        .profiles
        .iter()
        .map(|profile| integer(i64::from(profile.profile)))
        .collect();
    Some(plist::Value::Dictionary(dictionary([
        ("VTIsHDRAllowedOnDevice", plist::Value::Boolean(true)),
        ("VTPerProfileSupport", plist::Value::Dictionary(per_profile)),
        ("VTSupportedProfiles", plist::Value::Array(profiles)),
    ])))
}

/// AV1's capabilities as Apple's host writes them for a hardware decoder
/// (measured on macOS 27, 26A434, 2026-10-24): per profile,
/// VTChromaSubsampling "110" and "111", VTColorDepth "08" and "10" ("12"
/// too for profile 2), VTIsHardwareAccelerated, and the decode, playback
/// and HDR playback levels; then VTSupportedProfiles, with no
/// VTIsHDRAllowedOnDevice. Apple lists profiles 0 to 2 whatever its decoder
/// does; this host lists the profiles its backend decodes, and a level the
/// backend reports caps the playback levels too.
fn av1_capabilities_dictionary(capabilities: &CodecCapabilities) -> plist::Value {
    let strings = |values: &[&str]| {
        plist::Value::Array(
            values
                .iter()
                .map(|&v| plist::Value::String(v.to_owned()))
                .collect(),
        )
    };
    let mut per_profile = plist::Dictionary::new();
    for profile in &capabilities.profiles {
        let decode = profile.max_level.unwrap_or(AV1_MAX_DECODE_LEVEL);
        let depths: &[&str] = if profile.profile == 2 {
            &["08", "10", "12"]
        } else {
            &["08", "10"]
        };
        per_profile.insert(
            profile.profile.to_string(),
            plist::Value::Dictionary(dictionary([
                ("VTChromaSubsampling", strings(&["110", "111"])),
                ("VTColorDepth", strings(depths)),
                ("VTIsHardwareAccelerated", plist::Value::Boolean(true)),
                ("VTMaxDecodeLevel", integer(i64::from(decode))),
                (
                    "VTMaxHDRPlaybackLevel",
                    integer(i64::from(decode.min(AV1_MAX_HDR_PLAYBACK_LEVEL))),
                ),
                (
                    "VTMaxPlaybackLevel",
                    integer(i64::from(decode.min(AV1_MAX_PLAYBACK_LEVEL))),
                ),
            ])),
        );
    }
    let profiles = capabilities
        .profiles
        .iter()
        .map(|profile| integer(i64::from(profile.profile)))
        .collect();
    plist::Value::Dictionary(dictionary([
        ("VTPerProfileSupport", plist::Value::Dictionary(per_profile)),
        ("VTSupportedProfiles", plist::Value::Array(profiles)),
    ]))
}

/// `dcap`: the codec's capabilities, NULL for a codec the host does not
/// decode or lists none of, then the version. The codec is `,cod` as
/// `=s32`.
pub(crate) fn decoder_capabilities(
    request: &Message<'_>,
    decoders: &[CodecCapabilities],
) -> Result<Vec<u8>, WireError> {
    let codec = request.s32(names::CODEC)?.unwrap_or(0);
    let found = Codec::from_fourcc(crate::fourcc::FourCc(codec.cast_unsigned()))
        .and_then(|codec| decoders.iter().find(|entry| entry.codec == codec));
    let capabilities = found.and_then(capabilities_dictionary);
    let mut reply = reply(request);
    reply.property_list(names::DECODER_CAPABILITIES, capabilities.as_ref())?;
    reply.u32(names::VERSION, PROTOCOL_VERSION);
    Ok(reply.finish())
}

/// The rating each listed encoder carries: Apple's AVE encoders' (measured
/// on macOS 27, 26A434, 2026-10-24: `encs` lists
/// `com.apple.videotoolbox.videoencoder.ave.avc` and `.hevc` with VTRating
/// 400). The guest registers each with its rating.
pub(crate) const ENCODER_RATING: i64 = 400;

/// The `CMClassImplementationID` of this host's encoder for `codec`. The
/// guest registers it as `paravirtualized:<ID>` and sends it back as
/// `,cid` when it opens one. The IDs and names are ours.
pub(crate) fn encoder_id(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "reims-vgpu-video.videoencoder.avc",
        Codec::Hevc => "reims-vgpu-video.videoencoder.hevc",
        Codec::Av1 => "reims-vgpu-video.videoencoder.av1",
        Codec::Vp9 => "reims-vgpu-video.videoencoder.vp9",
    }
}

/// The `CMClassImplementationID` of Apple's AVE encoder for `codec`, which
/// the guest's two fixed paravirtualized encoders send as `,cid`: they put
/// it in their specification under `VTHostEncoderID`, the key the guest's
/// encoder reads it from: the one the factories use is "VTHostEncoderID"
/// (measured on macOS 27, 26A434, 2026-10-24, by `dlsym`), and the one the
/// guest encoder reads holds the same string. The guest has no fixed AV1
/// or VP9 encoder, so none for either.
pub(crate) fn apple_encoder_id(codec: Codec) -> Option<&'static str> {
    match codec {
        Codec::H264 => Some("com.apple.videotoolbox.videoencoder.ave.avc"),
        Codec::Hevc => Some("com.apple.videotoolbox.videoencoder.ave.hevc"),
        Codec::Av1 | Codec::Vp9 => None,
    }
}

fn encoder_name(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "reims-vgpu-video Encoder - AVC",
        Codec::Hevc => "reims-vgpu-video Encoder - HEVC",
        Codec::Av1 => "reims-vgpu-video Encoder - AV1",
        Codec::Vp9 => "reims-vgpu-video Encoder - VP9",
    }
}

/// The codec's name as Apple's encoders give it in `VTCodecName` (measured
/// on macOS 27: "H.264" for `ave.avc`, "HEVC" for `ave.hevc`). Apple has no
/// AV1 or VP9 encoder to measure; "AV1" and "VP9" are ours, and no encoder
/// of this host's encodes either.
pub(crate) fn codec_name(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "H.264",
        Codec::Hevc => "HEVC",
        Codec::Av1 => "AV1",
        Codec::Vp9 => "VP9",
    }
}

/// One entry of the encoder list, in the keys Apple's entries have
/// (measured on macOS 27: VTCodecType the codec type as a number,
/// VTCodecName, CMClassImplementationName, CMClassImplementationID,
/// VTRating, and Hide only when true).
fn encoder_entry(capabilities: &EncoderCapabilities) -> plist::Value {
    let codec = capabilities.codec;
    plist::Value::Dictionary(dictionary([
        ("VTCodecType", integer(i64::from(codec.fourcc().0))),
        ("VTRating", integer(ENCODER_RATING)),
        (
            "VTCodecName",
            plist::Value::String(codec_name(codec).to_owned()),
        ),
        (
            "CMClassImplementationName",
            plist::Value::String(encoder_name(codec).to_owned()),
        ),
        (
            "CMClassImplementationID",
            plist::Value::String(encoder_id(codec).to_owned()),
        ),
    ]))
}

/// `encs`: the encoders, then the version. The request's `,ver` is read but
/// not checked.
pub(crate) fn encoder_list(
    request: &Message<'_>,
    encoders: &[EncoderCapabilities],
) -> Result<Vec<u8>, WireError> {
    request.u32(names::VERSION)?;
    let entries = encoders.iter().map(encoder_entry).collect();
    let mut reply = reply(request);
    reply.property_list(names::ENCODERS, Some(&plist::Value::Array(entries)))?;
    reply.u32(names::VERSION, PROTOCOL_VERSION);
    Ok(reply.finish())
}

/// What `meps` answers: a reply, or a status and no reply.
pub(crate) enum MotionEstimationAnswer {
    /// The reply to send.
    Reply(Vec<u8>),
    /// The status to return, with nothing sent.
    Refused(OsStatus),
}

/// The `CMClassImplementationID` of this host's motion-estimation
/// processor, which the guest registers as `paravirtualized:<ID>` and
/// sends back as the specification's `HostProcessorID`. The ID and the name
/// are ours.
pub(crate) const MOTION_PROCESSOR_ID: &str = "reims-vgpu-video.motionestimationprocessor";
/// Its `CMClassImplementationName`.
pub(crate) const MOTION_PROCESSOR_NAME: &str = "reims-vgpu-video Motion Estimation";

/// This host's processor as `meps` lists it, in the shape of Apple's entry
/// (measured on macOS 27, 26A434, 2026-10-24): the name, the ID, and
/// matching info with `VTIsParavirtualizationAware` and
/// `VTIsHardwareAccelerated` true, both of which Apple's host requires of
/// a processor it lists and the guest's create requires of the processor it
/// takes.
fn motion_processor_entry() -> plist::Value {
    plist::Value::Dictionary(dictionary([
        (
            "CMClassImplementationName",
            plist::Value::String(MOTION_PROCESSOR_NAME.to_owned()),
        ),
        (
            "CMClassImplementationID",
            plist::Value::String(MOTION_PROCESSOR_ID.to_owned()),
        ),
        (
            "CMMatchingInfo",
            plist::Value::Dictionary(dictionary([
                ("VTIsParavirtualizationAware", plist::Value::Boolean(true)),
                ("VTIsHardwareAccelerated", plist::Value::Boolean(true)),
            ])),
        ),
    ]))
}

/// `meps`: a guest older than protocol 6 is refused with no reply;
/// otherwise this host's processor when it has motion estimation, or none,
/// then the version.
pub(crate) fn motion_estimation_list(
    request: &Message<'_>,
    has_processor: bool,
) -> Result<MotionEstimationAnswer, WireError> {
    let version = request.u32(names::VERSION)?.unwrap_or(0);
    if version < PROTOCOL_VERSION {
        return Ok(MotionEstimationAnswer::Refused(
            Status::UnsupportedGuest.code(),
        ));
    }
    let processors = if has_processor {
        vec![motion_processor_entry()]
    } else {
        Vec::new()
    };
    let mut reply = reply(request);
    reply.property_list(
        names::MOTION_ESTIMATORS,
        Some(&plist::Value::Array(processors)),
    )?;
    reply.u32(names::VERSION, PROTOCOL_VERSION);
    Ok(MotionEstimationAnswer::Reply(reply.finish()))
}
