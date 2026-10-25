//! What this host's hardware encoders are, from VideoToolbox's own
//! answers, and the properties each takes:
//! - the encoder list (`VTCopyVideoEncoderList`): an H.264 or HEVC encoder
//!   flagged `IsHardwareAccelerated`, and whether it reorders frames;
//! - its supported properties (`VTCopySupportedPropertyDictionaryForEncoder`
//!   with `RequireHardwareAcceleratedVideoEncoder`): the profiles its
//!   `ProfileLevel` list names, and the keys and values a guest may set;
//! - the largest picture a session of it opens at, tried.

use reims_vgpu_video_core::backend::{Codec, PixelFormat};
use reims_vgpu_video_core::encoder_backend::EncoderCapabilities;
use reims_vgpu_video_core::plist;
use reims_vgpu_video_core::status::OsStatus;

use super::super::cf::{self, Cf, Owned, constant};
use super::super::ffi::core_foundation::{CFArrayRef, CFDictionaryRef, CFStringRef, OSStatus};
use super::super::ffi::video_toolbox::{
    VTCompressionSessionCreate, VTCompressionSessionInvalidate,
    VTCompressionSessionPrepareToEncodeFrames, VTCompressionSessionRef,
    VTCopySupportedPropertyDictionaryForEncoder, VTCopyVideoEncoderList, kVTParameterErr,
    kVTPropertyNotSupportedErr,
};
use super::super::session::Session;
use super::super::stream_format::codec_type;
use super::EncoderChoice;

/// Properties core passes that the session does not take as they are:
/// `Paravirtualized`, the host's own, and `MultiPassStorage`, which the
/// guest sends as a boolean where VideoToolbox takes a VTMultiPassStorage.
/// Core takes only false for it, one pass, which is the session's own
/// default.
pub(crate) const NOT_FOR_THE_SESSION: [&str; 2] = ["Paravirtualized", "MultiPassStorage"];

/// The `ProfileLevel` prefixes of the profiles core names, with the
/// bitstream's number for each (core's `host/encoder/properties.rs`):
/// `H264_High_` leaves out `H264_High422_` and `H264_High444Predictive_`.
const PROFILE_PREFIXES: [(&str, Codec, u32); 5] = [
    ("H264_Baseline_", Codec::H264, 66),
    ("H264_Main_", Codec::H264, 77),
    ("H264_High_", Codec::H264, 100),
    ("HEVC_Main_", Codec::Hevc, 1),
    ("HEVC_Main10_", Codec::Hevc, 2),
];

/// The sides a session is tried at for the largest picture, largest
/// first, each with the other side 64.
const SIDES: [u32; 5] = [16384, 8192, 4096, 2048, 1920];

/// The size the supported properties are asked at.
const ASKED_SIZE: (i32, i32) = (1920, 1080);

/// What a property's value must be, as its description's `PropertyType`
/// says (`VTSession.h`, `kVTPropertyTypeKey`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PropertyType {
    /// `Boolean`: a CFBoolean.
    Boolean,
    /// `Number`: a CFNumber.
    Number,
    /// `Enumeration`: a CFString.
    Enumeration,
}

impl PropertyType {
    /// The type a description names; None for none, or one not here.
    fn of(description: &plist::Value) -> Option<Self> {
        match description
            .as_dictionary()?
            .get("PropertyType")?
            .as_string()?
        {
            "Boolean" => Some(Self::Boolean),
            "Number" => Some(Self::Number),
            "Enumeration" => Some(Self::Enumeration),
            _ => None,
        }
    }

    /// Whether `value` is of this type.
    fn holds(self, value: &plist::Value) -> bool {
        match self {
            Self::Boolean => matches!(value, plist::Value::Boolean(_)),
            Self::Number => matches!(value, plist::Value::Integer(_) | plist::Value::Real(_)),
            Self::Enumeration => matches!(value, plist::Value::String(_)),
        }
    }
}

/// One key an encoder lists: its type, where it gives one, and the values
/// it lists for it.
#[derive(Clone, Debug)]
struct Supported {
    key: String,
    kind: Option<PropertyType>,
    list: Option<Vec<plist::Value>>,
}

/// The keys an encoder lists, each with its type and the values it lists
/// for it.
#[derive(Clone, Debug, Default)]
pub(crate) struct PropertyCheck {
    supported: Vec<Supported>,
}

impl PropertyCheck {
    /// The check of a supported-properties dictionary as VideoToolbox
    /// answers it.
    pub(crate) fn new(supported: &plist::Dictionary) -> Self {
        Self {
            supported: supported
                .iter()
                .map(|(key, description)| Supported {
                    key: key.clone(),
                    kind: PropertyType::of(description),
                    list: description
                        .as_dictionary()
                        .and_then(|description| description.get("SupportedValueList"))
                        .and_then(plist::Value::as_array)
                        .cloned(),
                })
                .collect(),
        }
    }

    /// Whether the encoder takes `key` set to `value`: a key it lists, a
    /// value of the type it gives the key, and for an enumeration, a value
    /// of its list. Anything else is VideoToolbox's -12900,
    /// `kVTPropertyNotSupportedErr`.
    ///
    /// The type is checked here, not left to the session, because a value
    /// of another type can end the process: `ExpectedFrameRate` as a
    /// string threw an uncaught NSInvalidArgumentException in
    /// `VTCompressionSessionSetProperty` on both hardware encoders, and as
    /// an array, a dictionary or data on H.264's.
    pub(crate) fn check(&self, key: &str, value: &plist::Value) -> Result<(), OsStatus> {
        if NOT_FOR_THE_SESSION.contains(&key) {
            return Ok(());
        }
        let Some(supported) = self.supported.iter().find(|known| known.key == key) else {
            return Err(kVTPropertyNotSupportedErr);
        };
        if supported.kind.is_some_and(|kind| !kind.holds(value)) {
            return Err(kVTPropertyNotSupportedErr);
        }
        match &supported.list {
            Some(list) if !list.contains(value) => Err(kVTPropertyNotSupportedErr),
            _ => Ok(()),
        }
    }

    /// The profiles of `codec` its `ProfileLevel` list names, as the
    /// bitstream numbers them.
    pub(crate) fn profiles(&self, codec: Codec) -> Vec<u32> {
        let Some(levels) = self
            .supported
            .iter()
            .find(|known| known.key == "ProfileLevel")
            .and_then(|known| known.list.as_ref())
        else {
            return Vec::new();
        };
        let mut profiles: Vec<u32> = PROFILE_PREFIXES
            .iter()
            .filter(|(prefix, of, _)| {
                *of == codec
                    && levels
                        .iter()
                        .filter_map(plist::Value::as_string)
                        .any(|level| level.starts_with(prefix))
            })
            .map(|(_, _, profile)| *profile)
            .collect();
        profiles.dedup();
        profiles
    }
}

/// Sets on a session each property of `wanted` that `applied` lacks or
/// holds at another value, then clears each of `applied` that `wanted`
/// lacks (`set(key, None)`), leaving out [`NOT_FOR_THE_SESSION`].
pub(crate) fn apply_properties<E>(
    applied: &[(String, plist::Value)],
    wanted: &[(String, plist::Value)],
    mut set: impl FnMut(&str, Option<&plist::Value>) -> Result<(), E>,
) -> Result<(), E> {
    for (key, value) in wanted {
        if NOT_FOR_THE_SESSION.contains(&key.as_str()) {
            continue;
        }
        let same = applied
            .iter()
            .any(|(known, old)| known == key && old == value);
        if !same {
            set(key, Some(value))?;
        }
    }
    for (key, _) in applied {
        if NOT_FOR_THE_SESSION.contains(&key.as_str())
            || wanted.iter().any(|(known, _)| known == key)
        {
            continue;
        }
        set(key, None)?;
    }
    Ok(())
}

/// The encoder specification of `choice`:
/// `{RequireHardwareAcceleratedVideoEncoder: true}`, or for the software
/// encoder `{EnableHardwareAcceleratedVideoEncoder: false}`.
pub(crate) fn specification(choice: EncoderChoice) -> Owned {
    match choice {
        EncoderChoice::Hardware => cf::dictionary(&[(
            constant!(
                video_toolbox::kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder
            ),
            cf::boolean(true),
        )]),
        #[cfg(test)]
        EncoderChoice::Software => cf::dictionary(&[(
            constant!(
                video_toolbox::kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder
            ),
            cf::boolean(false),
        )]),
    }
}

/// The encoder `choice` picks for `codec`: its ID and its supported
/// properties as VideoToolbox lists them, or None where there is none.
pub(crate) fn supported_properties(
    codec: Codec,
    choice: EncoderChoice,
) -> Option<(String, plist::Dictionary)> {
    let specification = specification(choice);
    let mut id: CFStringRef = std::ptr::null();
    let mut supported: CFDictionaryRef = std::ptr::null();
    // SAFETY: a live specification and two live out pointers.
    let status = unsafe {
        VTCopySupportedPropertyDictionaryForEncoder(
            ASKED_SIZE.0,
            ASKED_SIZE.1,
            codec_type(codec),
            specification.as_ptr(),
            &raw mut id,
            &raw mut supported,
        )
    };
    // SAFETY: Copy results the caller owns, or NULL.
    let (id, supported) = unsafe { (Owned::take(id), Owned::take(supported)) };
    if status != 0 {
        return None;
    }
    let id = id?.cf().string()?;
    let supported = match supported?.cf().to_plist()? {
        plist::Value::Dictionary(supported) => supported,
        _ => return None,
    };
    Some((id, supported))
}

/// A session of `codec` on the hardware encoder at `width` by `height`,
/// prepared to encode, or the status of the call that failed. It encodes
/// nothing, and holds one of the Mac's hardware encoder sessions until it
/// goes.
pub(crate) fn prepared_session(codec: Codec, width: u32, height: u32) -> Result<Session, OSStatus> {
    let (Ok(width), Ok(height)) = (i32::try_from(width), i32::try_from(height)) else {
        return Err(kVTParameterErr);
    };
    let specification = specification(EncoderChoice::Hardware);
    let mut session: VTCompressionSessionRef = std::ptr::null_mut();
    // SAFETY: a live specification and out pointer; no callback, since no
    // frame is encoded.
    let status = unsafe {
        VTCompressionSessionCreate(
            std::ptr::null(),
            width,
            height,
            codec_type(codec),
            specification.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            None,
            std::ptr::null_mut(),
            &raw mut session,
        )
    };
    // SAFETY: a Create call's result, a compression session, or NULL.
    let session = unsafe { Session::take(session, VTCompressionSessionInvalidate) };
    let session = match (status, session) {
        (0, Some(session)) => session,
        (0, None) => return Err(kVTParameterErr),
        (status, _) => return Err(status),
    };
    // SAFETY: a live session.
    let prepared = unsafe { VTCompressionSessionPrepareToEncodeFrames(session.as_ptr()) };
    if prepared != 0 {
        return Err(prepared);
    }
    Ok(session)
}

/// Whether a session of `codec` opens and prepares at `width` by `height`
/// on the hardware encoder.
fn opens_at(codec: Codec, width: u32, height: u32) -> bool {
    prepared_session(codec, width, height).is_ok()
}

/// The largest side of [`SIDES`] a session of `codec` opens at, the other
/// side 64: the width with `wide`, else the height; 0 where none opens.
fn largest_side(codec: Codec, wide: bool) -> u32 {
    SIDES
        .into_iter()
        .find(|&side| {
            if wide {
                opens_at(codec, side, 64)
            } else {
                opens_at(codec, 64, side)
            }
        })
        .unwrap_or(0)
}

/// The list's entry for each hardware encoder: its codec type, and whether
/// it reorders frames (true where the list says nothing,
/// `VTVideoEncoderList.h`).
fn listed_hardware_encoders() -> Vec<(u32, bool)> {
    let mut list: CFArrayRef = std::ptr::null();
    // SAFETY: no options and a live out pointer.
    let status = unsafe { VTCopyVideoEncoderList(std::ptr::null(), &raw mut list) };
    // SAFETY: a Copy result the caller owns, or NULL.
    let Some(list) = (unsafe { Owned::take(list) }) else {
        return Vec::new();
    };
    if status != 0 {
        return Vec::new();
    }
    let flag = |entry: Cf<'_>, key: Cf<'_>| entry.get(key).and_then(Cf::bool);
    let hardware = constant!(video_toolbox::kVTVideoEncoderList_IsHardwareAccelerated);
    let codec_type = constant!(video_toolbox::kVTVideoEncoderList_CodecType);
    let reorders = constant!(video_toolbox::kVTVideoEncoderList_SupportsFrameReordering);
    (list.cf().values().into_iter())
        .filter(|&entry| flag(entry, hardware) == Some(true))
        .filter_map(|entry| {
            let codec = u32::try_from(entry.get(codec_type)?.i64()?).ok()?;
            Some((codec, flag(entry, reorders).unwrap_or(true)))
        })
        .collect()
}

/// The hardware encoders of the codecs core encodes, H.264 and HEVC, as
/// VideoToolbox's encoder list names them: each codec once, and whether
/// any of its encoders reorders frames. The list is VideoToolbox's
/// registry, which answers whatever the encoders' state: it named both
/// while every hardware encoder session of the Mac was taken (the M3
/// Max, macOS 27.0.1, 2026-10-24).
pub(crate) fn listed_encoders() -> Vec<(Codec, bool)> {
    let listed = listed_hardware_encoders();
    [Codec::H264, Codec::Hevc]
        .into_iter()
        .filter_map(|codec| {
            let reorders = listed
                .iter()
                .filter(|(of, _)| *of == codec_type(codec))
                .map(|(_, reorders)| *reorders)
                .reduce(|a, b| a || b)?;
            Some((codec, reorders))
        })
        .collect()
}

/// The check of the properties `codec`'s hardware encoder takes, or None
/// where VideoToolbox lists none: as while every hardware encoder session
/// of the Mac is taken, when `VTCopySupportedPropertyDictionaryForEncoder`
/// fails.
pub(crate) fn property_check(codec: Codec) -> Option<PropertyCheck> {
    let (_, supported) = supported_properties(codec, EncoderChoice::Hardware)?;
    Some(PropertyCheck::new(&supported))
}

/// What `codec`'s hardware encoder encodes, as the host lists it to the
/// guest, from the properties it takes, and the largest pictures a session
/// of it opens at, tried now; None where it lists no profile core encodes.
pub(crate) fn capabilities(
    codec: Codec,
    reorders: bool,
    check: &PropertyCheck,
) -> Option<EncoderCapabilities> {
    let profiles = check.profiles(codec);
    if profiles.is_empty() {
        log::warn!("videotoolbox: the {codec:?} hardware encoder lists no profile core encodes");
        return None;
    }
    let mut source_formats = vec![PixelFormat::Nv12VideoRange, PixelFormat::Nv12FullRange];
    if codec == Codec::Hevc && profiles.contains(&2) {
        source_formats.extend([PixelFormat::P010VideoRange, PixelFormat::P010FullRange]);
    }
    Some(EncoderCapabilities {
        codec,
        profiles,
        max_width: largest_side(codec, true),
        max_height: largest_side(codec, false),
        source_formats,
        frame_reordering: reorders,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use reims_vgpu_video_core::wire::property_list::dictionary;

    fn enumeration(values: &[&str]) -> plist::Value {
        plist::Value::Dictionary(dictionary([
            ("PropertyType", plist::Value::String("Enumeration".into())),
            (
                "SupportedValueList",
                plist::Value::Array(
                    values
                        .iter()
                        .map(|v| plist::Value::String((*v).into()))
                        .collect(),
                ),
            ),
        ]))
    }

    fn avc() -> PropertyCheck {
        let mut supported = plist::Dictionary::new();
        supported.insert(
            "ProfileLevel".into(),
            enumeration(&[
                "H264_Baseline_3_0",
                "H264_High_AutoLevel",
                "H264_High422_AutoLevel",
                "H264_ConstrainedBaseline_AutoLevel",
            ]),
        );
        supported.insert(
            "AverageBitRate".into(),
            plist::Value::Dictionary(dictionary([(
                "PropertyType",
                plist::Value::String("Number".into()),
            )])),
        );
        PropertyCheck::new(&supported)
    }

    #[test]
    fn a_key_or_value_the_encoder_does_not_list_is_refused() {
        let check = avc();
        let level = |name: &str| plist::Value::String(name.into());
        assert_eq!(
            check.check("ProfileLevel", &level("H264_High_AutoLevel")),
            Ok(())
        );
        assert_eq!(
            check.check("ProfileLevel", &level("H264_Main_4_1")),
            Err(-12900)
        );
        assert_eq!(
            check.check("AverageBitRate", &plist::Value::Integer(1_000_000.into())),
            Ok(())
        );
        assert_eq!(check.check("H264EntropyMode", &level("CABAC")), Err(-12900));
        assert_eq!(
            check.check("MultiPassStorage", &plist::Value::Boolean(false)),
            Ok(())
        );
    }

    #[test]
    fn a_value_of_another_type_than_the_encoder_gives_is_refused() {
        let mut supported = plist::Dictionary::new();
        let typed = |kind: &str| {
            plist::Value::Dictionary(dictionary([(
                "PropertyType",
                plist::Value::String(kind.into()),
            )]))
        };
        supported.insert("ExpectedFrameRate".into(), typed("Number"));
        supported.insert("RealTime".into(), typed("Boolean"));
        supported.insert("H264EntropyMode".into(), typed("Enumeration"));
        // Listed with no type, as VideoToolbox lists it.
        supported.insert(
            "DataRateLimits".into(),
            plist::Value::Dictionary(plist::Dictionary::new()),
        );
        let check = PropertyCheck::new(&supported);
        let text = |text: &str| plist::Value::String(text.into());
        for (key, taken, refused) in [
            (
                "ExpectedFrameRate",
                vec![plist::Value::Integer(30.into()), plist::Value::Real(29.97)],
                vec![
                    text("30"),
                    plist::Value::Boolean(true),
                    plist::Value::Array(vec![plist::Value::Integer(30.into())]),
                    plist::Value::Dictionary(plist::Dictionary::new()),
                    plist::Value::Data(vec![30]),
                ],
            ),
            (
                "RealTime",
                vec![plist::Value::Boolean(false)],
                vec![plist::Value::Integer(1.into()), text("true")],
            ),
            (
                "H264EntropyMode",
                vec![text("CABAC")],
                vec![plist::Value::Integer(1.into())],
            ),
            (
                "DataRateLimits",
                vec![plist::Value::Array(Vec::new()), text("any")],
                vec![],
            ),
        ] {
            for value in taken {
                assert_eq!(check.check(key, &value), Ok(()), "{key} {value:?}");
            }
            for value in refused {
                assert_eq!(check.check(key, &value), Err(-12900), "{key} {value:?}");
            }
        }
    }

    #[test]
    fn profiles_come_from_the_profile_level_list() {
        assert_eq!(avc().profiles(Codec::H264), vec![66, 100]);
        assert!(avc().profiles(Codec::Hevc).is_empty());
    }

    #[test]
    fn only_changes_reach_the_session() {
        let int = |n: i64| plist::Value::Integer(n.into());
        let applied = vec![
            ("AverageBitRate".to_owned(), int(1)),
            ("Quality".to_owned(), plist::Value::Real(0.5)),
        ];
        let wanted = vec![
            ("AverageBitRate".to_owned(), int(2)),
            ("MultiPassStorage".to_owned(), plist::Value::Boolean(false)),
            ("RealTime".to_owned(), plist::Value::Boolean(true)),
        ];
        let mut calls = Vec::new();
        let done: Result<(), ()> = apply_properties(&applied, &wanted, |key, value| {
            calls.push((key.to_owned(), value.cloned()));
            Ok(())
        });
        assert_eq!(done, Ok(()));
        assert_eq!(
            calls,
            vec![
                ("AverageBitRate".to_owned(), Some(int(2))),
                ("RealTime".to_owned(), Some(plist::Value::Boolean(true))),
                ("Quality".to_owned(), None),
            ]
        );
    }
}
