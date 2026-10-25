//! An encoder session's properties: the ones this host's encoders support,
//! what `esup` lists of them, what `ecop` answers, what `eset` and `emul`
//! accept, and the [`EncoderSettings`] they make for the backend.
//!
//! The keys are VideoToolbox's `kVTCompressionPropertyKey_*` strings, and
//! the descriptions take the shape Apple's AVC encoder gives its own:
//! `PropertyType` (Boolean, Number or Enumeration), `ReadWriteStatus`
//! (ReadWrite or ReadOnly), and an enumeration's `SupportedValueList`
//! (measured on macOS 27, 26A434, 2026-10-24: `esup` to
//! `com.apple.videotoolbox.videoencoder.ave.avc`). Which properties a host
//! encoder supports is its own choice: these are the ones a VA-API encoder
//! can honour.

use std::collections::HashMap;

use crate::backend::Codec;
use crate::encoder_backend::{
    ColourDescription, EncoderCapabilities, EncoderSettings, EntropyMode,
};
use crate::num::low_u32;
use crate::status::{NO_ERROR, OsStatus, Status};
use crate::wire::property_list::as_i64;

/// `kVTCompressionPropertyKey_Paravirtualized`: the host's own, which it
/// sets on the encoder it makes and a guest may not set.
pub(crate) const PARAVIRTUALIZED: &str = "Paravirtualized";
const USING_HARDWARE: &str = "UsingHardwareAcceleratedVideoEncoder";
const ALLOW_FRAME_REORDERING: &str = "AllowFrameReordering";
const ALLOW_TEMPORAL_COMPRESSION: &str = "AllowTemporalCompression";
const AVERAGE_BIT_RATE: &str = "AverageBitRate";
const DATA_RATE_LIMITS: &str = "DataRateLimits";
const EXPECTED_FRAME_RATE: &str = "ExpectedFrameRate";
const MAX_KEY_FRAME_INTERVAL: &str = "MaxKeyFrameInterval";
const MAX_KEY_FRAME_INTERVAL_DURATION: &str = "MaxKeyFrameIntervalDuration";
const PROFILE_LEVEL: &str = "ProfileLevel";
const QUALITY: &str = "Quality";
const REAL_TIME: &str = "RealTime";
const ENTROPY_MODE: &str = "H264EntropyMode";
const COLOR_PRIMARIES: &str = "ColorPrimaries";
const TRANSFER_FUNCTION: &str = "TransferFunction";
const YCBCR_MATRIX: &str = "YCbCrMatrix";
const MAXIMIZE_POWER_EFFICIENCY: &str = "MaximizePowerEfficiency";
const PRIORITIZE_SPEED: &str = "PrioritizeEncodingSpeedOverQuality";
/// `kVTCompressionPropertyKey_MultiPassStorage`: the guest sends whether
/// it has one. This host's encoders do one pass, and have no such
/// property, as Apple's HEVC software encoder has none: it is not listed,
/// and copies and sets of it are -12900 (the session's own handling,
/// `encoder.rs`).
pub(crate) const MULTI_PASS_STORAGE: &str = "MultiPassStorage";

/// How a property's value is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Boolean,
    Number,
    /// One of a list of strings.
    Enumeration,
    /// An array of numbers: `DataRateLimits`.
    NumberArray,
}

/// One property this host supports.
struct Property {
    key: &'static str,
    kind: Kind,
    read_only: bool,
}

const fn property(key: &'static str, kind: Kind) -> Property {
    Property {
        key,
        kind,
        read_only: false,
    }
}

/// Every property, in the order `esup` lists them.
const PROPERTIES: &[Property] = &[
    property(ALLOW_FRAME_REORDERING, Kind::Boolean),
    property(ALLOW_TEMPORAL_COMPRESSION, Kind::Boolean),
    property(AVERAGE_BIT_RATE, Kind::Number),
    property(COLOR_PRIMARIES, Kind::Enumeration),
    property(DATA_RATE_LIMITS, Kind::NumberArray),
    property(EXPECTED_FRAME_RATE, Kind::Number),
    property(ENTROPY_MODE, Kind::Enumeration),
    property(MAX_KEY_FRAME_INTERVAL, Kind::Number),
    property(MAX_KEY_FRAME_INTERVAL_DURATION, Kind::Number),
    property(MAXIMIZE_POWER_EFFICIENCY, Kind::Boolean),
    property(PARAVIRTUALIZED, Kind::Boolean),
    property(PRIORITIZE_SPEED, Kind::Boolean),
    property(PROFILE_LEVEL, Kind::Enumeration),
    property(QUALITY, Kind::Number),
    property(REAL_TIME, Kind::Boolean),
    property(TRANSFER_FUNCTION, Kind::Enumeration),
    Property {
        key: USING_HARDWARE,
        kind: Kind::Boolean,
        read_only: true,
    },
    property(YCBCR_MATRIX, Kind::Enumeration),
];

/// `ColorPrimaries` values and their ITU-T H.273 `colour_primaries`; the
/// list is Apple's AVC encoder's (measured, as above).
const PRIMARIES: &[(&str, u8)] = &[
    ("ITU_R_709_2", 1),
    ("EBU_3213", 5),
    ("SMPTE_C", 6),
    ("ITU_R_2020", 9),
    ("P3_D65", 12),
    ("DCI_P3", 11),
];

/// `TransferFunction` values and their H.273 `transfer_characteristics`.
/// `UseGamma`, which Apple's also lists, has no code point and is left
/// unspecified.
const TRANSFERS: &[(&str, u8)] = &[
    ("ITU_R_709_2", 1),
    ("SMPTE_240M_1995", 7),
    ("Linear", 8),
    ("IEC_sRGB", 13),
    ("ITU_R_2020", 14),
    ("SMPTE_ST_2084_PQ", 16),
    ("SMPTE_ST_428_1", 17),
    ("ITU_R_2100_HLG", 18),
];

/// `YCbCrMatrix` values and their H.273 `matrix_coefficients`.
const MATRICES: &[(&str, u8)] = &[
    ("Identity", 0),
    ("ITU_R_709_2", 1),
    ("ITU_R_601_4", 6),
    ("SMPTE_240M_1995", 7),
    ("ITU_R_2020", 9),
    ("ITU_R_2100_ICtCp", 14),
];

/// `H264EntropyMode` values (`kVTH264EntropyMode_CAVLC`, `_CABAC`).
const ENTROPY_MODES: &[(&str, EntropyMode)] =
    &[("CAVLC", EntropyMode::Cavlc), ("CABAC", EntropyMode::Cabac)];

/// The levels Apple's AVC encoder lists for each H.264 profile, as
/// `ProfileLevel` spells them (measured, as above): Baseline also has 1.3.
const H264_LEVELS: &[(&str, u32)] = &[
    ("3_0", 30),
    ("3_1", 31),
    ("3_2", 32),
    ("4_0", 40),
    ("4_1", 41),
    ("4_2", 42),
    ("5_0", 50),
    ("5_1", 51),
    ("5_2", 52),
];

/// A `ProfileLevel` value's profile, as `kVTProfileLevel_*` names it, and
/// the bitstream's number for it.
fn profile_names(codec: Codec) -> &'static [(&'static str, u32)] {
    match codec {
        Codec::H264 => &[("H264_Baseline", 66), ("H264_Main", 77), ("H264_High", 100)],
        Codec::Hevc => &[("HEVC_Main", 1), ("HEVC_Main10", 2)],
        // No encoder of this host's encodes AV1 or VP9.
        Codec::Av1 | Codec::Vp9 => &[],
    }
}

/// The `ProfileLevel` values an encoder with `capabilities` takes: each
/// profile it lists, at an automatic level and, for H.264, at Apple's
/// levels.
fn profile_levels(capabilities: &EncoderCapabilities) -> Vec<(String, u32, Option<u32>)> {
    let mut values = Vec::new();
    for &(name, profile) in profile_names(capabilities.codec) {
        if !capabilities.profiles.contains(&profile) {
            continue;
        }
        if capabilities.codec == Codec::H264 {
            if profile == 66 {
                values.push((format!("{name}_1_3"), profile, Some(13)));
            }
            for &(level, number) in H264_LEVELS {
                values.push((format!("{name}_{level}"), profile, Some(number)));
            }
        }
        values.push((format!("{name}_AutoLevel"), profile, None));
    }
    values
}

/// The properties a session's guest set, and what makes them settings.
pub(crate) struct EncoderProperties {
    capabilities: EncoderCapabilities,
    values: HashMap<&'static str, plist::Value>,
}

/// What a set asks of the session.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SetOutcome {
    /// The value was taken; the settings may have changed.
    Taken,
    /// The status to answer with: the property is not one this host's
    /// encoder supports, the value is not of its type, or it is read-only.
    Refused(OsStatus),
}

impl EncoderProperties {
    pub(crate) fn new(capabilities: EncoderCapabilities) -> Self {
        Self {
            capabilities,
            values: HashMap::new(),
        }
    }

    fn find(&self, key: &str) -> Option<&'static Property> {
        PROPERTIES.iter().find(|property| {
            property.key == key && (property.key != ENTROPY_MODE || self.codec() == Codec::H264)
        })
    }

    fn codec(&self) -> Codec {
        self.capabilities.codec
    }

    /// The values an enumeration takes.
    fn enumeration_values(&self, key: &str) -> Vec<String> {
        let names =
            |table: &[(&str, u8)]| table.iter().map(|(name, _)| (*name).to_owned()).collect();
        match key {
            COLOR_PRIMARIES => names(PRIMARIES),
            TRANSFER_FUNCTION => names(TRANSFERS),
            YCBCR_MATRIX => names(MATRICES),
            ENTROPY_MODE => ENTROPY_MODES
                .iter()
                .map(|(name, _)| (*name).to_owned())
                .collect(),
            PROFILE_LEVEL => profile_levels(&self.capabilities)
                .into_iter()
                .map(|(name, _, _)| name)
                .collect(),
            _ => Vec::new(),
        }
    }

    /// `esup`'s `,sup`: each property's description, by its key.
    pub(crate) fn supported(&self) -> plist::Dictionary {
        let mut supported = plist::Dictionary::new();
        for property in PROPERTIES {
            if self.find(property.key).is_none() {
                continue;
            }
            let mut description = plist::Dictionary::new();
            let kind = match property.kind {
                Kind::Boolean => Some("Boolean"),
                Kind::Number => Some("Number"),
                Kind::Enumeration => Some("Enumeration"),
                // Apple's `DataRateLimits` has no type, only its status.
                Kind::NumberArray => None,
            };
            if let Some(kind) = kind {
                description.insert(
                    "PropertyType".to_owned(),
                    plist::Value::String(kind.to_owned()),
                );
            }
            let status = if property.read_only {
                "ReadOnly"
            } else {
                "ReadWrite"
            };
            description.insert(
                "ReadWriteStatus".to_owned(),
                plist::Value::String(status.to_owned()),
            );
            let values = self.enumeration_values(property.key);
            if !values.is_empty() {
                description.insert(
                    "SupportedValueList".to_owned(),
                    plist::Value::Array(values.into_iter().map(plist::Value::String).collect()),
                );
            }
            supported.insert(
                property.key.to_owned(),
                plist::Value::Dictionary(description),
            );
        }
        supported
    }

    /// `ecop`: the status, and the value when there is one. A property
    /// the guest has not set answers its default, as Apple's AVC encoder
    /// answers: false, 0, an automatic level.
    pub(crate) fn copy(&self, key: &str) -> (OsStatus, Option<plist::Value>) {
        let Some(property) = self.find(key) else {
            return (Status::PropertyNotSupported.code(), None);
        };
        let value = match property.key {
            USING_HARDWARE | PARAVIRTUALIZED => plist::Value::Boolean(true),
            _ => match self.values.get(property.key) {
                Some(value) => value.clone(),
                None => match property.kind {
                    Kind::Boolean => {
                        plist::Value::Boolean(property.key == ALLOW_TEMPORAL_COMPRESSION)
                    }
                    Kind::Number => plist::Value::Integer(0.into()),
                    Kind::NumberArray => plist::Value::Array(Vec::new()),
                    Kind::Enumeration if property.key == PROFILE_LEVEL => {
                        match profile_levels(&self.capabilities).last() {
                            Some((name, _, _)) => plist::Value::String(name.clone()),
                            None => return (Status::PropertyNotSupported.code(), None),
                        }
                    }
                    Kind::Enumeration => return (NO_ERROR, None),
                },
            },
        };
        (NO_ERROR, Some(value))
    }

    /// `eset`, and each of `emul`'s: takes a value, or refuses it. A NULL
    /// value clears the property. A value of the wrong type is refused
    /// with -12900, as Apple's AVC encoder refuses one (measured, as
    /// above: `AllowFrameReordering` as a number), and so is a number out
    /// of the range Apple's AVE takes ([`in_range`]). `MultiPassStorage`,
    /// which no encoder here has, is the session's to answer
    /// ([`MULTI_PASS_STORAGE`]), and is refused here too.
    pub(crate) fn set(&mut self, key: &str, value: Option<plist::Value>) -> SetOutcome {
        let Some(property) = self.find(key) else {
            return SetOutcome::Refused(Status::PropertyNotSupported.code());
        };
        if property.read_only {
            return SetOutcome::Refused(Status::PropertyReadOnly.code());
        }
        let Some(value) = value else {
            self.values.remove(property.key);
            return SetOutcome::Taken;
        };
        let fits = match property.kind {
            Kind::Boolean => value.as_boolean().is_some(),
            Kind::Number => number(&value).is_some(),
            Kind::NumberArray => value
                .as_array()
                .is_some_and(|values| values.iter().all(|value| number(value).is_some())),
            Kind::Enumeration => value.as_string().is_some_and(|name| {
                self.enumeration_values(property.key)
                    .iter()
                    .any(|known| known == name)
            }),
        };
        if !fits || !in_range(property.key, &value) {
            return SetOutcome::Refused(Status::PropertyNotSupported.code());
        }
        self.values.insert(property.key, value);
        SetOutcome::Taken
    }

    fn boolean(&self, key: &str) -> Option<bool> {
        self.values.get(key).and_then(plist::Value::as_boolean)
    }

    fn number(&self, key: &str) -> Option<f64> {
        self.values.get(key).and_then(number)
    }

    fn code(&self, key: &str, table: &[(&str, u8)]) -> Option<u8> {
        let name = self.values.get(key)?.as_string()?;
        table
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, code)| *code)
    }

    /// The settings the properties make.
    pub(crate) fn settings(&self) -> EncoderSettings {
        let (profile, level) = self
            .values
            .get(PROFILE_LEVEL)
            .and_then(plist::Value::as_string)
            .and_then(|name| {
                profile_levels(&self.capabilities)
                    .into_iter()
                    .find(|(known, _, _)| known == name)
            })
            .map_or((None, None), |(_, profile, level)| (Some(profile), level));
        let max_key_frame_interval = if self.boolean(ALLOW_TEMPORAL_COMPRESSION) == Some(false) {
            Some(1)
        } else {
            self.number(MAX_KEY_FRAME_INTERVAL)
                .and_then(whole_number)
                .and_then(|frames| u32::try_from(frames).ok())
                .filter(|&frames| frames > 0)
        };
        EncoderSettings {
            profile,
            level,
            average_bit_rate: self
                .number(AVERAGE_BIT_RATE)
                .and_then(whole_number)
                .and_then(|bits| u32::try_from(bits).ok())
                .filter(|&bits| bits > 0),
            max_bit_rate: self.max_bit_rate(),
            // A NaN passes a clamp; it is no quality, as if unset.
            quality: self
                .number(QUALITY)
                .filter(|quality| !quality.is_nan())
                .map(|quality| narrow(quality.clamp(0.0, 1.0))),
            max_key_frame_interval,
            max_key_frame_interval_duration: self
                .number(MAX_KEY_FRAME_INTERVAL_DURATION)
                .filter(|&seconds| seconds.is_finite() && seconds > 0.0),
            expected_frame_rate: self
                .number(EXPECTED_FRAME_RATE)
                .filter(|&rate| rate.is_finite() && rate > 0.0),
            allow_frame_reordering: self.boolean(ALLOW_FRAME_REORDERING).unwrap_or(false),
            real_time: self.boolean(REAL_TIME).unwrap_or(false),
            entropy: self
                .values
                .get(ENTROPY_MODE)
                .and_then(plist::Value::as_string)
                .and_then(|name| {
                    ENTROPY_MODES
                        .iter()
                        .find(|(known, _)| *known == name)
                        .map(|(_, mode)| *mode)
                }),
            colour: ColourDescription {
                primaries: self.code(COLOR_PRIMARIES, PRIMARIES),
                transfer: self.code(TRANSFER_FUNCTION, TRANSFERS),
                matrix: self.code(YCBCR_MATRIX, MATRICES),
            },
            properties: {
                let mut properties: Vec<(String, plist::Value)> = (self.values.iter())
                    .map(|(key, value)| ((*key).to_owned(), value.clone()))
                    .collect();
                properties.sort_by(|a, b| a.0.cmp(&b.0));
                properties
            },
        }
    }

    /// The value the guest set for `key`, if any: what [`Self::restore`]
    /// puts back when the backend refuses a new one.
    pub(crate) fn value(&self, key: &str) -> Option<plist::Value> {
        self.values.get(key).cloned()
    }

    /// Puts back `key`'s earlier value, or none.
    pub(crate) fn restore(&mut self, key: &str, value: Option<plist::Value>) {
        let Some(property) = self.find(key) else {
            return;
        };
        match value {
            Some(value) => self.values.insert(property.key, value),
            None => self.values.remove(property.key),
        };
    }

    /// The tightest of `DataRateLimits`' pairs, each a byte count then a
    /// duration in seconds, in bits per second.
    fn max_bit_rate(&self) -> Option<u32> {
        let limits = self.values.get(DATA_RATE_LIMITS)?.as_array()?;
        limits
            .as_chunks::<2>()
            .0
            .iter()
            .filter_map(|[bytes, seconds]| {
                let bytes = number(bytes)?;
                let seconds = number(seconds)?;
                let rate = bytes * 8.0 / seconds;
                (bytes > 0.0 && seconds > 0.0 && rate.is_finite()).then_some(rate)
            })
            .fold(None, |tightest: Option<f64>, rate| {
                Some(tightest.map_or(rate, |tightest| tightest.min(rate)))
            })
            .and_then(whole_number)
            .and_then(|rate| u32::try_from(rate).ok())
            // A limit that rounds to nothing is none, as an AverageBitRate
            // of 0 is.
            .filter(|&bits| bits > 0)
    }

    /// The colour properties the guest set, as format description
    /// extensions (`CVImageBufferColorPrimaries` and the others).
    pub(crate) fn colour_extensions(&self) -> Vec<(&'static str, String)> {
        [
            (COLOR_PRIMARIES, "CVImageBufferColorPrimaries"),
            (TRANSFER_FUNCTION, "CVImageBufferTransferFunction"),
            (YCBCR_MATRIX, "CVImageBufferYCbCrMatrix"),
        ]
        .into_iter()
        .filter_map(|(key, extension)| {
            let value = self.values.get(key)?.as_string()?;
            Some((extension, value.to_owned()))
        })
        .collect()
    }
}

/// Whether `value`, of `key`'s type, is in the range Apple's AVE takes:
/// it answers -12900 for any other (VTSessionSetProperty in process,
/// H.264 and HEVC alike, M3 Max, macOS 27.0.1, 2026-10-24).
/// - `Quality` from 0 to 1: -0.0001, 1.0001, NaN and infinity refused.
/// - `ExpectedFrameRate` from 1 to under 100000: 0.99, 100000, NaN and
///   infinity refused, 1 and 99999.9 taken.
/// - `MaxKeyFrameIntervalDuration` 0 or more: -0.5 and NaN refused,
///   infinity taken.
/// - `AverageBitRate` and `MaxKeyFrameInterval` as a signed 32-bit
///   integer, 0 or more ([`as_int32`]): 2147483647 and 4294967296 taken,
///   -1, 2147483648, 4294967295 and 10^12 refused; -0.5 and NaN taken,
///   -1.5 and minus infinity refused.
///
/// Every other number and every array of numbers is taken: `DataRateLimits`
/// of one, three or six numbers, zeros and negatives among them.
fn in_range(key: &str, value: &plist::Value) -> bool {
    let real = number(value).unwrap_or(f64::NAN);
    match key {
        QUALITY => (0.0..=1.0).contains(&real),
        EXPECTED_FRAME_RATE => (1.0..100_000.0).contains(&real),
        MAX_KEY_FRAME_INTERVAL_DURATION => real >= 0.0,
        AVERAGE_BIT_RATE | MAX_KEY_FRAME_INTERVAL => as_int32(value).is_some_and(|n| n >= 0),
        _ => true,
    }
}

/// A number as AVE reads one into a signed 32-bit integer: an integer's
/// low 32 bits, and a real truncated toward zero and held to the range
/// (NaN to 0), which is what its answers above fit.
fn as_int32(value: &plist::Value) -> Option<i32> {
    match value {
        plist::Value::Real(real) => Some(truncate_to_i32(*real)),
        plist::Value::Boolean(_) => None,
        // Truncated to the low 32 bits, as the measured answers show.
        other => as_i64(other).map(|integer| low_u32(integer.cast_unsigned()).cast_signed()),
    }
}

/// A number value, integer or real.
fn number(value: &plist::Value) -> Option<f64> {
    match value {
        plist::Value::Real(real) => Some(*real),
        plist::Value::Boolean(_) => None,
        other => as_i64(other).map(nearest_f64),
    }
}

/// `value` rounded to the nearest whole number, half away from zero, where
/// it is finite and not negative: a count of frames or bits. One past
/// `u64::MAX` is `u64::MAX`.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a float to an integer: `as` saturates, and the value is finite and not negative"
)]
fn whole_number(value: f64) -> Option<u64> {
    (value.is_finite() && value >= 0.0).then(|| value.round() as u64)
}

/// `quality`, from 0 to 1, as the backend's `f32`.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "a float to a narrower float: within 0 to 1 it only rounds"
)]
fn narrow(quality: f64) -> f32 {
    quality as f32
}

/// `real` truncated toward zero into an `i32`, held to its range, NaN 0:
/// what `as` does, and what AVE's answers fit ([`as_int32`]).
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "a float to an integer: `as` truncates toward zero and saturates, NaN to 0"
)]
fn truncate_to_i32(real: f64) -> i32 {
    real as i32
}

/// The nearest `f64` to `integer`: exact up to 2^53.
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "an integer to a float, the nearest one"
)]
fn nearest_f64(integer: i64) -> f64 {
    integer as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::PixelFormat;
    use crate::wire::property_list::dictionary;

    /// A read-write property's description.
    fn description(kind: &str) -> plist::Value {
        plist::Value::Dictionary(dictionary([
            ("PropertyType", plist::Value::String(kind.to_owned())),
            (
                "ReadWriteStatus",
                plist::Value::String("ReadWrite".to_owned()),
            ),
        ]))
    }

    fn h264() -> EncoderProperties {
        EncoderProperties::new(EncoderCapabilities {
            codec: Codec::H264,
            profiles: vec![66, 77, 100],
            max_width: 4096,
            max_height: 4096,
            source_formats: vec![PixelFormat::Nv12VideoRange],
            frame_reordering: false,
        })
    }

    #[test]
    fn the_list_takes_apples_shape() {
        let supported = h264().supported();
        assert_eq!(
            supported.get(AVERAGE_BIT_RATE),
            Some(&description("Number"))
        );
        assert_eq!(
            supported
                .get(USING_HARDWARE)
                .and_then(plist::Value::as_dictionary)
                .and_then(|entry| entry.get("ReadWriteStatus")),
            Some(&plist::Value::String("ReadOnly".into()))
        );
        let Some(plist::Value::Array(levels)) = supported
            .get(PROFILE_LEVEL)
            .and_then(plist::Value::as_dictionary)
            .and_then(|entry| entry.get("SupportedValueList"))
        else {
            panic!("ProfileLevel is an enumeration");
        };
        assert!(levels.contains(&plist::Value::String("H264_Baseline_1_3".into())));
        assert!(levels.contains(&plist::Value::String("H264_High_AutoLevel".into())));
        // Nine levels and AutoLevel for each profile, and Baseline 1.3.
        assert_eq!(levels.len(), 3 * 10 + 1);
    }

    #[test]
    fn values_become_settings() {
        let mut properties = h264();
        for (key, value) in [
            (PROFILE_LEVEL, plist::Value::String("H264_Main_4_1".into())),
            (AVERAGE_BIT_RATE, plist::Value::Integer(2_000_000.into())),
            (ALLOW_FRAME_REORDERING, plist::Value::Boolean(true)),
            (MAX_KEY_FRAME_INTERVAL, plist::Value::Real(30.0)),
            (COLOR_PRIMARIES, plist::Value::String("ITU_R_709_2".into())),
            (ENTROPY_MODE, plist::Value::String("CABAC".into())),
            (
                DATA_RATE_LIMITS,
                plist::Value::Array(vec![
                    plist::Value::Integer(500_000.into()),
                    plist::Value::Real(1.0),
                    plist::Value::Integer(100_000.into()),
                    plist::Value::Real(0.5),
                ]),
            ),
        ] {
            assert_eq!(properties.set(key, Some(value)), SetOutcome::Taken, "{key}");
        }
        let settings = properties.settings();
        assert_eq!((settings.profile, settings.level), (Some(77), Some(41)));
        assert_eq!(settings.average_bit_rate, Some(2_000_000));
        assert_eq!(settings.max_bit_rate, Some(1_600_000));
        assert_eq!(settings.max_key_frame_interval, Some(30));
        assert!(settings.allow_frame_reordering);
        assert_eq!(settings.entropy, Some(EntropyMode::Cabac));
        assert_eq!(settings.colour.primaries, Some(1));
        assert_eq!(settings.colour.matrix, None);
    }

    #[test]
    fn wrong_types_unknown_keys_and_read_only_ones_are_refused() {
        let mut properties = h264();
        let refused = |status: Status| SetOutcome::Refused(status.code());
        assert_eq!(
            properties.set(
                ALLOW_FRAME_REORDERING,
                Some(plist::Value::Integer(0.into()))
            ),
            refused(Status::PropertyNotSupported)
        );
        assert_eq!(
            properties.set("NoSuchProperty", Some(plist::Value::Boolean(true))),
            refused(Status::PropertyNotSupported)
        );
        assert_eq!(
            properties.set(USING_HARDWARE, Some(plist::Value::Boolean(false))),
            refused(Status::PropertyReadOnly)
        );
        assert_eq!(
            properties.set(
                PROFILE_LEVEL,
                Some(plist::Value::String("HEVC_Main_AutoLevel".into()))
            ),
            refused(Status::PropertyNotSupported)
        );
        assert_eq!(
            properties.set(MULTI_PASS_STORAGE, Some(plist::Value::Boolean(true))),
            refused(Status::PropertyNotSupported)
        );
        assert_eq!(
            properties.copy(USING_HARDWARE),
            (NO_ERROR, Some(plist::Value::Boolean(true)))
        );
        assert_eq!(
            properties.copy("NoSuchProperty"),
            (Status::PropertyNotSupported.code(), None)
        );
    }

    /// The values Apple's AVE refuses with -12900 (`in_range`), and those
    /// it takes.
    #[test]
    fn numbers_out_of_aves_ranges_are_refused() {
        let real = |value: f64| Some(plist::Value::Real(value));
        let integer = |value: i64| Some(plist::Value::Integer(value.into()));
        let mut properties = h264();
        for (key, value, taken) in [
            (QUALITY, real(0.0), true),
            (QUALITY, real(1.0), true),
            (QUALITY, real(-0.0001), false),
            (QUALITY, real(1.0001), false),
            (QUALITY, real(f64::NAN), false),
            (QUALITY, real(f64::INFINITY), false),
            (EXPECTED_FRAME_RATE, real(1.0), true),
            (EXPECTED_FRAME_RATE, real(99_999.9), true),
            (EXPECTED_FRAME_RATE, real(0.99), false),
            (EXPECTED_FRAME_RATE, integer(0), false),
            (EXPECTED_FRAME_RATE, integer(100_000), false),
            (EXPECTED_FRAME_RATE, real(f64::INFINITY), false),
            (EXPECTED_FRAME_RATE, real(f64::NAN), false),
            (MAX_KEY_FRAME_INTERVAL_DURATION, real(0.0), true),
            (MAX_KEY_FRAME_INTERVAL_DURATION, real(f64::INFINITY), true),
            (MAX_KEY_FRAME_INTERVAL_DURATION, real(-0.5), false),
            (MAX_KEY_FRAME_INTERVAL_DURATION, real(f64::NAN), false),
            (AVERAGE_BIT_RATE, integer(0), true),
            (AVERAGE_BIT_RATE, integer(2_147_483_647), true),
            (AVERAGE_BIT_RATE, integer(4_294_967_296), true),
            (AVERAGE_BIT_RATE, integer(-1), false),
            (AVERAGE_BIT_RATE, integer(2_147_483_648), false),
            (AVERAGE_BIT_RATE, integer(4_294_967_295), false),
            (AVERAGE_BIT_RATE, integer(1_000_000_000_000), false),
            (AVERAGE_BIT_RATE, real(1.5), true),
            (AVERAGE_BIT_RATE, real(f64::NAN), true),
            (AVERAGE_BIT_RATE, real(f64::INFINITY), true),
            (AVERAGE_BIT_RATE, real(-1.5), false),
            (AVERAGE_BIT_RATE, real(f64::NEG_INFINITY), false),
            (MAX_KEY_FRAME_INTERVAL, integer(2_147_483_647), true),
            (MAX_KEY_FRAME_INTERVAL, integer(2_147_483_648), false),
            (MAX_KEY_FRAME_INTERVAL, integer(-1), false),
            (MAX_KEY_FRAME_INTERVAL, real(-0.5), true),
        ] {
            let want = if taken {
                SetOutcome::Taken
            } else {
                SetOutcome::Refused(Status::PropertyNotSupported.code())
            };
            assert_eq!(properties.set(key, value.clone()), want, "{key} {value:?}");
        }
        let limits = |values: &[f64]| {
            Some(plist::Value::Array(
                values.iter().map(|&v| plist::Value::Real(v)).collect(),
            ))
        };
        for values in [
            &[1000.0][..],
            &[0.0, 1.0],
            &[-1.0, 1.0],
            &[1000.0, 1.0, 2000.0],
        ] {
            assert_eq!(
                properties.set(DATA_RATE_LIMITS, limits(values)),
                SetOutcome::Taken
            );
        }
    }

    #[test]
    fn infinite_rates_and_limits_that_round_to_nothing_are_unset() {
        let mut properties = h264();
        let real = |value: f64| Some(plist::Value::Real(value));
        properties.set(MAX_KEY_FRAME_INTERVAL_DURATION, real(f64::INFINITY));
        // Refused, as AVE refuses it (`in_range`): the rate stays unset.
        properties.set(EXPECTED_FRAME_RATE, real(f64::INFINITY));
        // 8 bits over 100 s: 0.08 bit/s.
        let limits = vec![plist::Value::Integer(1.into()), plist::Value::Real(100.0)];
        properties.set(DATA_RATE_LIMITS, Some(plist::Value::Array(limits)));
        let settings = properties.settings();
        assert_eq!(settings.max_key_frame_interval_duration, None);
        assert_eq!(settings.expected_frame_rate, None);
        assert_eq!(settings.max_bit_rate, None);
        let limits = vec![plist::Value::Real(f64::INFINITY), plist::Value::Real(1.0)];
        properties.set(DATA_RATE_LIMITS, Some(plist::Value::Array(limits)));
        assert_eq!(properties.settings().max_bit_rate, None);
    }

    #[test]
    fn no_temporal_compression_is_all_key_frames() {
        let mut properties = h264();
        properties.set(
            MAX_KEY_FRAME_INTERVAL,
            Some(plist::Value::Integer(60.into())),
        );
        properties.set(
            ALLOW_TEMPORAL_COMPRESSION,
            Some(plist::Value::Boolean(false)),
        );
        assert_eq!(properties.settings().max_key_frame_interval, Some(1));
    }

    #[test]
    fn hevc_has_no_entropy_mode_and_its_own_profiles() {
        let properties = EncoderProperties::new(EncoderCapabilities {
            codec: Codec::Hevc,
            profiles: vec![1],
            max_width: 4096,
            max_height: 4096,
            source_formats: vec![PixelFormat::Nv12VideoRange],
            frame_reordering: false,
        });
        assert!(properties.supported().get(ENTROPY_MODE).is_none());
        assert_eq!(
            properties.copy(PROFILE_LEVEL),
            (
                NO_ERROR,
                Some(plist::Value::String("HEVC_Main_AutoLevel".into()))
            )
        );
    }
}
