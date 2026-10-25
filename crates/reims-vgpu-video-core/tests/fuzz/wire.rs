//! The wire layer's readers: property lists, messages and their
//! parameters, fragments, sample buffers, image descriptions and the
//! surface boxes. Each must refuse what does not read, never panic, and
//! read back what core's own writers wrote.

use reims_vgpu_video_core::fourcc::FourCc;
use reims_vgpu_video_core::wire::atom::siblings;
use reims_vgpu_video_core::wire::image_description::ImageDescription;
use reims_vgpu_video_core::wire::message::{
    Message, MessageFlags, MessageWriter, ValueType, find_header, read_header,
};
use reims_vgpu_video_core::wire::property_list;
use reims_vgpu_video_core::wire::sample_buffer::{SampleBuffer, SerializedValue};
use reims_vgpu_video_core::wire::surface_boxes::{
    MappingId, read_surface_release, surface_list, surface_release,
};

use crate::generate::{description, message, plist, sample_buffer};
use crate::support::{Rng, Target, long, maybe_mutate, quick};

const PROPERTY_LISTS: Target = Target::new("property-lists");
const MESSAGES: Target = Target::new("messages");
const FRAGMENTS: Target = Target::new("fragments");
const SAMPLE_BUFFERS: Target = Target::new("sample-buffers");
const IMAGE_DESCRIPTIONS: Target = Target::new("image-descriptions");
const SURFACE_BOXES: Target = Target::new("surface-boxes");

#[test]
fn property_lists() {
    quick(&PROPERTY_LISTS, 20_000, property_list_case);
}

#[test]
#[ignore = "long: RVGV_FUZZ_CASES, RVGV_FUZZ_SECONDS, RVGV_FUZZ_SEED"]
fn property_lists_long() {
    long(&PROPERTY_LISTS, property_list_case);
}

#[test]
fn messages() {
    quick(&MESSAGES, 1500, message_case);
}

#[test]
#[ignore = "long: RVGV_FUZZ_CASES, RVGV_FUZZ_SECONDS, RVGV_FUZZ_SEED"]
fn messages_long() {
    long(&MESSAGES, message_case);
}

#[test]
fn fragments() {
    quick(&FRAGMENTS, 800, fragment_case);
}

#[test]
#[ignore = "long: RVGV_FUZZ_CASES, RVGV_FUZZ_SECONDS, RVGV_FUZZ_SEED"]
fn fragments_long() {
    long(&FRAGMENTS, fragment_case);
}

#[test]
fn sample_buffers() {
    quick(&SAMPLE_BUFFERS, 12_000, sample_buffer_case);
}

#[test]
#[ignore = "long: RVGV_FUZZ_CASES, RVGV_FUZZ_SECONDS, RVGV_FUZZ_SEED"]
fn sample_buffers_long() {
    long(&SAMPLE_BUFFERS, sample_buffer_case);
}

#[test]
fn image_descriptions() {
    quick(&IMAGE_DESCRIPTIONS, 30_000, image_description_case);
}

#[test]
#[ignore = "long: RVGV_FUZZ_CASES, RVGV_FUZZ_SECONDS, RVGV_FUZZ_SEED"]
fn image_descriptions_long() {
    long(&IMAGE_DESCRIPTIONS, image_description_case);
}

#[test]
fn surface_boxes() {
    quick(&SURFACE_BOXES, 6000, surface_box_case);
}

#[test]
#[ignore = "long: RVGV_FUZZ_CASES, RVGV_FUZZ_SECONDS, RVGV_FUZZ_SEED"]
fn surface_boxes_long() {
    long(&SURFACE_BOXES, surface_box_case);
}

/// A property list decodes within its bounds or is refused, and what
/// decodes encodes and decodes again. A text plist small and shallow
/// enough for the plist crate's own builder to drop what it builds decodes
/// to what that builder makes of it, where that is within the bounds.
fn property_list_case(rng: &mut Rng) {
    let bytes = plist::bytes(rng);
    let decoded = property_list::decode(&bytes);
    if let Ok(value) = &decoded {
        assert!(
            within_bounds(value),
            "a property list decoded past its bounds"
        );
        if let Ok(again) = property_list::encode(value) {
            assert!(
                property_list::decode(&again).is_ok(),
                "a property list that decoded does not decode once encoded"
            );
        }
    }
    let nesting = bytes
        .iter()
        .filter(|&&byte| matches!(byte, b'(' | b'{' | b'<'))
        .count();
    if !bytes.starts_with(b"bplist00") && bytes.len() <= 64 << 10 && nesting <= 300 {
        let built = reims_vgpu_video_core::plist::Value::from_reader(std::io::Cursor::new(&bytes))
            .ok()
            .filter(within_bounds);
        let written = |value: Option<&reims_vgpu_video_core::plist::Value>| {
            value.map(|value| property_list::encode(value).ok())
        };
        assert_eq!(
            written(decoded.as_ref().ok()),
            written(built.as_ref()),
            "a text property list decodes otherwise than the plist crate builds it"
        );
    }
}

/// Whether a value is within `property_list`'s bounds, walked without
/// recursion: its values, keys included, and its depth, a scalar at the
/// top being 1.
fn within_bounds(value: &reims_vgpu_video_core::plist::Value) -> bool {
    use reims_vgpu_video_core::plist::Value;
    let mut values = 0u64;
    let mut stack = vec![(value, 1u32)];
    while let Some((value, depth)) = stack.pop() {
        values += 1;
        if values > property_list::MAX_VALUES || depth > property_list::MAX_DEPTH {
            return false;
        }
        match value {
            Value::Array(array) => stack.extend(array.iter().map(|value| (value, depth + 1))),
            Value::Dictionary(dictionary) => {
                values += dictionary.len() as u64;
                stack.extend(dictionary.values().map(|value| (value, depth + 1)));
            }
            _ => {}
        }
    }
    true
}

/// Every reader of a message, with every key the host reads, as each
/// type.
fn message_case(rng: &mut Rng) {
    let bytes = message::any(rng);
    read_message(rng, &bytes);
}

/// Reads `bytes` every way the host reads a message.
pub fn read_message(rng: &mut Rng, bytes: &[u8]) {
    let _ = surface_list(bytes);
    let _ = read_surface_release(bytes);
    for start in [0, 1, 8, 0x30, 48, rng.below(bytes.len() + 2)] {
        let _ = find_header(bytes, start);
        let _ = read_header(bytes, start);
    }
    for atom in siblings(bytes).take(64) {
        let Ok(atom) = atom else { break };
        for child in siblings(atom.payload()).take(64) {
            if child.is_err() {
                break;
            }
        }
    }
    // The device reads the header past the `subs` box, as the host
    // session does.
    let message = match Message::parse(bytes) {
        Ok(message) => message,
        Err(_) => return,
    };
    let _ = message.header();
    for &key in message::KEYS {
        read_parameter(&message, FourCc::new(key));
    }
    read_parameter(&message, message::key(rng));
}

/// Every getter of `key`.
fn read_parameter(message: &Message<'_>, key: FourCc) {
    for value_type in message::VALUE_TYPES {
        let _ = message.contains(key, value_type);
    }
    let _ = message.u32(key);
    let _ = message.s32(key);
    let _ = message.s64(key);
    let _ = message.time(key);
    for pair in [ValueType::Dimensions, ValueType::Point, ValueType::Size] {
        let _ = message.i32_pair(key, pair);
    }
    let _ = message.uuid(key);
    let _ = message.uuid_array(key);
    let _ = message.property_list(key);
    let _ = message.dictionary(key);
    if let Ok(Some(description)) = message.image_description(key) {
        let _ = ImageDescription::parse(description);
    }
    let _ = message.data(key);
    if let Ok(Some(serialization)) = message.sample_buffer(key) {
        let _ = SampleBuffer::parse(&serialization);
    }
}

/// A sample buffer cut into fragments as the guest cuts a `dfrm`'s reads
/// back whole from the fragments joined; mutated, it reads or is refused.
fn fragment_case(rng: &mut Rng) {
    let serialization = sample_buffer::any(rng).serialize();
    let mut value = serialization.clone();
    if rng.one_in(4) {
        value.extend(rng.some_bytes(3000));
    }
    let key = FourCc::new(*b",sbf");
    let uuid = message::uuid(rng);
    let flags = MessageFlags::GUEST_EXPECTS_REPLY;
    let message_type = FourCc::new(*b"dfrm");
    let mut head = MessageWriter::new(message_type, flags, Some(uuid), 7);
    head.s64(FourCc::new(*b",frm"), rng.next_u64().cast_signed());
    head.u32(FourCc::new(*b",dfl"), rng.interesting_u32());
    // Room for a piece in the first message: the guest's appender adds
    // none to a message with fewer than 17 bytes left, and a guest whose
    // limit is 0x200000 always has them.
    let limit = rng.range(head.len() as u64 + 18, 2000) as usize;
    let pieces = message::fragments(head, (message_type, flags, uuid, 7), key, &value, limit);
    let mut whole: Vec<u8> = pieces.concat();
    if let Ok(message) = Message::parse(&whole) {
        let gathered = message.sample_buffer(key);
        assert!(
            matches!(&gathered, Ok(Some(gathered)) if *gathered == value),
            "{} fragments of {} bytes gather to {:?} bytes",
            pieces.len(),
            value.len(),
            gathered.map(|g| g.map(|g| g.len()))
        );
    }
    maybe_mutate(rng, &mut whole, b"vtpvpara,sbf=sbf", 1);
    read_message(rng, &whole);
}

/// A sample buffer reads or is refused, and one that reads is written
/// again without a panic. What is written need not read back: an empty
/// array is written as nothing, so a dictionary entry of one has no value
/// and an array of only empty ones loses a level, as in Apple's
/// serialization. The host writes neither.
fn sample_buffer_case(rng: &mut Rng) {
    let mut bytes = sample_buffer::any(rng).serialize();
    maybe_mutate(
        rng,
        &mut bytes,
        b"sbufsdatssizstiansmpfdscmdiavdimcodcextndictkeyvidxkaraynullnmbvbulvstrkdatvdtevclrsurlvcrptsattsat2sary",
        6,
    );
    if let Ok(sample) = SampleBuffer::parse(&bytes) {
        assert!(
            sample_values(&sample) <= property_list::MAX_VALUES,
            "a sample buffer read past its bound of values"
        );
        let _ = SampleBuffer::parse(&sample.serialize());
    }
}

/// The values a sample buffer that read holds, as its reader counts them:
/// its dictionaries, arrays, keys, nulls and format descriptions, and its
/// property lists' values. Recursive: a sample buffer nests 32 deep at
/// most, and a property list 64.
fn sample_values(sample: &SampleBuffer) -> u64 {
    use reims_vgpu_video_core::plist::Value;
    fn plist(value: &Value) -> u64 {
        match value {
            Value::Array(array) => 1 + array.iter().map(plist).sum::<u64>(),
            Value::Dictionary(dictionary) => {
                1 + dictionary
                    .values()
                    .map(|value| 1 + plist(value))
                    .sum::<u64>()
            }
            _ => 1,
        }
    }
    fn entries(entries: &[(String, SerializedValue)]) -> u64 {
        1 + entries
            .iter()
            .map(|(_, value)| 1 + serialized(value))
            .sum::<u64>()
    }
    fn serialized(value: &SerializedValue) -> u64 {
        match value {
            SerializedValue::Dictionary(dictionary) => entries(dictionary),
            SerializedValue::Array(array) => 1 + array.iter().map(serialized).sum::<u64>(),
            SerializedValue::PropertyList(value) | SerializedValue::ColorSpace(value) => {
                plist(value)
            }
            SerializedValue::Null | SerializedValue::Url(_) => 1,
            SerializedValue::FormatDescription(description) => {
                1 + description.extensions.as_deref().map_or(0, entries)
            }
        }
    }
    let description = sample.format_description.as_ref().map_or(0, |description| {
        1 + description.extensions.as_deref().map_or(0, entries)
    });
    let attachments = [&sample.propagated_attachments, &sample.other_attachments]
        .into_iter()
        .flatten()
        .map(|dictionary| entries(dictionary))
        .sum::<u64>();
    let per_sample = sample
        .sample_attachments
        .as_ref()
        .map_or(0, |array| 1 + array.iter().map(serialized).sum::<u64>());
    description + attachments + per_sample
}

/// An image description reads or is refused, and one that reads is
/// written back as it read.
fn image_description_case(rng: &mut Rng) {
    let bytes = description::bytes(rng);
    if let Ok(parsed) = ImageDescription::parse(&bytes) {
        let written = parsed.to_bytes();
        let again = ImageDescription::parse(&written);
        assert!(
            again.as_ref() == Ok(&parsed),
            "an image description that read does not read back: {again:?}"
        );
    }
}

/// The `subs` and `rels` boxes: what the host writes reads back, and
/// any bytes read or are refused.
fn surface_box_case(rng: &mut Rng) {
    let ids: Vec<MappingId> = (0..rng.count(3000))
        .map(|_| MappingId(rng.interesting_u64()))
        .collect();
    let mut bytes = surface_release(&ids);
    if !ids.is_empty() {
        assert_eq!(
            read_surface_release(&bytes).map(|(read, rest)| (read, rest.len())),
            Some((ids.clone(), 0)),
            "a rels box of {} IDs does not read back",
            ids.len()
        );
    }
    if rng.bool() {
        bytes.extend(message::any(rng));
    }
    maybe_mutate(rng, &mut bytes, b"relssubs", 4);
    let _ = read_surface_release(&bytes);
    let _ = surface_list(&bytes);
    let ids = message::surface_ids(rng);
    let mut bytes = message::with_surfaces(&ids, message::any(rng));
    maybe_mutate(rng, &mut bytes, b"subs", 3);
    let _ = surface_list(&bytes);
}
