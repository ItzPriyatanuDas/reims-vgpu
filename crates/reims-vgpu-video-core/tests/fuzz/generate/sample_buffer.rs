//! Sample buffer serializations as a guest's `dfrm` carries them: written
//! by core's own serializer from values of every kind, nested past
//! `MAX_NESTING` now and then, then mutated.

use reims_vgpu_video_core::fourcc::FourCc;
use reims_vgpu_video_core::wire::message::MediaTime;
use reims_vgpu_video_core::wire::sample_buffer::{
    CryptorRecipe, MAX_NESTING, MEDIA_TYPE_AUXILIARY_PICTURE, MEDIA_TYPE_PICTURE, MEDIA_TYPE_VIDEO,
    SampleBuffer, SampleTiming, SerializedFormatDescription, SerializedValue,
};

use crate::generate::plist;
use crate::support::Rng;

/// A time, valid most often.
pub fn time(rng: &mut Rng) -> MediaTime {
    if rng.one_in(6) {
        return MediaTime {
            value: rng.interesting_u64().cast_signed(),
            timescale: rng.interesting_i32(),
            flags: rng.interesting_u32(),
            epoch: rng.interesting_u64().cast_signed(),
        };
    }
    MediaTime {
        value: rng.below(100_000) as i64,
        timescale: *rng.pick(&[600, 90_000, 1_000_000_000, 30]),
        flags: MediaTime::FLAG_VALID,
        epoch: 0,
    }
}

/// A codec type: one the host decodes, most often.
pub fn codec(rng: &mut Rng) -> FourCc {
    if rng.one_in(8) {
        FourCc(rng.next_u32())
    } else {
        FourCc::new(*rng.pick(&[*b"avc1", *b"hvc1", *b"av01", *b"vp09", *b"jpeg", *b"mp4v"]))
    }
}

/// The configuration record's atom name for `codec`.
pub fn record_name(codec: FourCc) -> &'static str {
    match &codec.to_be_bytes() {
        b"avc1" => "avcC",
        b"hvc1" => "hvcC",
        b"av01" => "av1C",
        b"vp09" => "vpcC",
        _ => "avcC",
    }
}

/// A format description, with `record` as its configuration record when
/// one is given.
pub fn format_description(
    rng: &mut Rng,
    codec: FourCc,
    size: (i32, i32),
    record: Option<Vec<u8>>,
) -> SerializedFormatDescription {
    let mut extensions = Vec::new();
    if let Some(record) = record {
        extensions.push((
            "SampleDescriptionExtensionAtoms".to_owned(),
            SerializedValue::Dictionary(vec![(
                record_name(codec).to_owned(),
                SerializedValue::PropertyList(reims_vgpu_video_core::plist::Value::Data(record)),
            )]),
        ));
    }
    if rng.one_in(3) {
        extensions.push((plist::string(rng), value(rng, 2)));
    }
    SerializedFormatDescription {
        media_type: if rng.one_in(8) {
            *rng.pick(&[
                MEDIA_TYPE_AUXILIARY_PICTURE,
                MEDIA_TYPE_PICTURE,
                FourCc::new(*b"soun"),
            ])
        } else {
            MEDIA_TYPE_VIDEO
        },
        codec,
        width: size.0,
        height: size.1,
        extensions: (!extensions.is_empty() || rng.bool()).then_some(extensions),
    }
}

/// A serialized value of any kind, nested at most `depth` deep.
pub fn value(rng: &mut Rng, depth: u32) -> SerializedValue {
    if depth == 0 || rng.below(3) != 0 {
        return match rng.below(8) {
            0 => SerializedValue::Null,
            1 => SerializedValue::Url(plist::string(rng)),
            2 => SerializedValue::ColorSpace(plist::scalar(rng)),
            _ => SerializedValue::PropertyList(plist::scalar(rng)),
        };
    }
    match rng.below(5) {
        0 | 1 => SerializedValue::Array((0..rng.count(8)).map(|_| value(rng, depth - 1)).collect()),
        2 | 3 => SerializedValue::Dictionary(entries(rng, depth - 1)),
        _ => {
            let codec = codec(rng);
            let size = (rng.interesting_i32(), rng.interesting_i32());
            let record = rng.bool().then(|| rng.some_bytes(40));
            SerializedValue::FormatDescription(Box::new(format_description(
                rng, codec, size, record,
            )))
        }
    }
}

/// Dictionary entries, keyed by known keys most often.
pub fn entries(rng: &mut Rng, depth: u32) -> Vec<(String, SerializedValue)> {
    (0..rng.count(8))
        .map(|_| {
            let key = if rng.bool() {
                (*rng.pick(&[
                    "DependsOnOthers",
                    "NotSync",
                    "DisplayImmediately",
                    "DoNotDisplay",
                    "EarlierDisplayTimesAllowed",
                    "IsDependedOnByOthers",
                    "HasRedundantCoding",
                    "FillDiscontinuitiesWithSilence",
                ]))
                .to_owned()
            } else {
                plist::string(rng)
            };
            (key, value(rng, depth))
        })
        .collect()
}

/// Arrays inside arrays, `levels` deep.
pub fn nested(levels: u32) -> SerializedValue {
    let mut value = SerializedValue::Null;
    for _ in 0..levels {
        value = SerializedValue::Array(vec![value]);
    }
    value
}

/// A sample buffer for `data`, as a guest's `dfrm` carries one, with a
/// format description when one is given.
pub fn for_sample(
    rng: &mut Rng,
    data: Vec<u8>,
    sizes: Vec<u32>,
    format: Option<SerializedFormatDescription>,
) -> SampleBuffer {
    let timing = SampleTiming {
        duration: time(rng),
        presentation: time(rng),
        decode: time(rng),
    };
    SampleBuffer {
        sample_count: if rng.one_in(8) {
            rng.interesting_u32()
        } else {
            1
        },
        timings: (0..if rng.one_in(8) { rng.count(4) } else { 1 })
            .map(|_| timing)
            .collect(),
        sizes,
        data: Some(data),
        output_presentation_time: rng.one_in(4).then(|| time(rng)),
        format_description: format,
        cryptor: rng.one_in(16).then(|| CryptorRecipe {
            recipe_id: rng.next_u64(),
            version: rng.interesting_u32(),
            kind: rng.interesting_u32(),
        }),
        propagated_attachments: rng.one_in(4).then(|| entries(rng, 3)),
        other_attachments: rng.one_in(4).then(|| entries(rng, 3)),
        sample_attachments: rng.one_in(4).then(|| {
            (0..rng.count(3))
                .map(|_| {
                    if rng.one_in(16) {
                        nested(MAX_NESTING + rng.range_u32(0, 2) - 1)
                    } else {
                        SerializedValue::Dictionary(entries(rng, 3))
                    }
                })
                .collect()
        }),
    }
}

/// A sample buffer of anything.
pub fn any(rng: &mut Rng) -> SampleBuffer {
    let data = rng.some_bytes(200);
    let sizes = (0..rng.count(4)).map(|_| rng.interesting_u32()).collect();
    let format = rng.bool().then(|| {
        let codec = codec(rng);
        let record = Some(rng.some_bytes(40));
        let size = (rng.interesting_i32(), rng.interesting_i32());
        format_description(rng, codec, size, record)
    });
    let mut sample = for_sample(rng, data, sizes, format);
    if rng.one_in(6) {
        sample.data = None;
    }
    sample
}
