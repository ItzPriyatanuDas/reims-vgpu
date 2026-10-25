//! `vtpv` messages as a guest could send them: a header of any type and
//! flags, then parameters with the keys the host reads, each of a type
//! that may or may not be the one it is read as, written raw so that a
//! payload need not fit its type; a `subs` box before them; and the
//! fragments a large value is cut into.

use reims_vgpu_video_core::fourcc::FourCc;
use reims_vgpu_video_core::wire::message::{GuestUuid, MessageFlags, MessageWriter, ValueType};

use crate::generate::{description, plist, sample_buffer};
use crate::support::{Rng, maybe_mutate};

/// Every parameter key the host reads or writes (`host/names.rs`).
pub const KEYS: &[[u8; 4]] = &[
    *b",ver", *b",err", *b",cod", *b",dec", *b",enc", *b",mep", *b",dca", *b",cid", *b",gin",
    *b",hin", *b",joo", *b",jca", *b",fds", *b",blu", *b",frm", *b",sbf", *b",dfl", *b",fop",
    *b",inf", *b",uui", *b",ui+", *b",pba", *b",key", *b",val", *b",sup", *b",pro", *b",can",
    *b",vdi", *b",pts", *b",dur", *b",fpr", *b",pat", *b",efl", *b",bfl", *b",esp", *b",twh",
    *b",ept", *b",til", *b",tio", *b",tia", *b",tip", *b",wdt", *b",hgt", *b",ext", *b"=fds",
];

/// The host session's own messages (`host/names.rs`, `SessionMessage`).
pub const SESSION_TYPES: &[[u8; 4]] = &[
    *b"decs", *b"+dec", *b"+enc", *b"+mep", *b"-all", *b"-dec", *b"-enc", *b"-mep", *b"-uui",
    *b"dcap", *b"meps", *b"encs", *b"jcap", *b"jdec", *b"jenc",
];

/// A decoder session's messages.
pub const DECODER_TYPES: &[[u8; 4]] = &[
    *b"dcop", *b"dset", *b"dsta", *b"dfrm", *b"dsup", *b"dsts", *b"dtil", *b"dtfi", *b"dfin",
    *b"dcan", *b"dinv", *b"dmul", *b"dsav",
];

/// An encoder session's messages.
pub const ENCODER_TYPES: &[[u8; 4]] = &[
    *b"ecop", *b"eset", *b"esta", *b"epfr", *b"efrm", *b"emif", *b"ecfr", *b"ebps", *b"eeps",
    *b"esup", *b"ests", *b"epti", *b"etil", *b"ecti", *b"einv", *b"emul", *b"esav",
];

/// What the host asks of the guest, whose replies the guest sends.
pub const HOST_REQUEST_TYPES: &[[u8; 4]] = &[*b"Dbuf", *b"Demt", *b"Efds", *b"Eemt", *b"Epba"];

/// Every value type the wire has.
pub const VALUE_TYPES: [ValueType; 14] = [
    ValueType::U32,
    ValueType::S32,
    ValueType::S64,
    ValueType::Time,
    ValueType::Dimensions,
    ValueType::Point,
    ValueType::Size,
    ValueType::Data,
    ValueType::PropertyList,
    ValueType::Uuid,
    ValueType::UuidArray,
    ValueType::TagCollections,
    ValueType::FormatDescription,
    ValueType::SampleBuffer,
];

pub fn key(rng: &mut Rng) -> FourCc {
    if rng.one_in(16) {
        FourCc(rng.next_u32())
    } else {
        FourCc::new(*rng.pick(KEYS))
    }
}

/// A message type of any of the lists, or anything.
pub fn message_type(rng: &mut Rng) -> FourCc {
    let list = match rng.below(9) {
        0 | 1 => SESSION_TYPES,
        2 | 3 => DECODER_TYPES,
        4 | 5 => ENCODER_TYPES,
        6 => HOST_REQUEST_TYPES,
        7 => &[*b"Bye!", *b"vtpv", *b"para", *b"subs", *b"rels"],
        _ => return FourCc(rng.next_u32()),
    };
    FourCc::new(*rng.pick(list))
}

/// Flags: a guest's request, a reply to the host, a fragment, or any.
pub fn flags(rng: &mut Rng) -> MessageFlags {
    match rng.below(6) {
        0 | 1 => MessageFlags::GUEST_EXPECTS_REPLY,
        2 => MessageFlags::REPLY_TO_HOST,
        3 => MessageFlags(MessageFlags::GUEST_EXPECTS_REPLY.0 | MessageFlags::MORE_FRAGMENTS.0),
        4 => MessageFlags::NONE,
        _ => MessageFlags(rng.interesting_u32()),
    }
}

/// A UUID from a few, so that messages meet the same session.
pub fn uuid(rng: &mut Rng) -> GuestUuid {
    if rng.one_in(12) {
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&rng.bytes(16));
        GuestUuid(bytes)
    } else {
        GuestUuid([rng.below(4) as u8 + 0x51; 16])
    }
}

/// Appends a parameter atom of `key` and `value_type` with `payload`, as
/// the guest's appenders write one, whatever the payload.
pub fn raw_parameter(bytes: &mut Vec<u8>, key: FourCc, value_type: FourCc, payload: &[u8]) {
    let size = u32::try_from(16 + payload.len()).unwrap_or(u32::MAX);
    bytes.extend_from_slice(&size.to_be_bytes());
    bytes.extend_from_slice(b"para");
    bytes.extend_from_slice(&key.to_be_bytes());
    bytes.extend_from_slice(&value_type.to_be_bytes());
    bytes.extend_from_slice(payload);
}

/// A payload for `value_type`: one that fits it, most often.
pub fn payload(rng: &mut Rng, value_type: ValueType) -> Vec<u8> {
    if rng.one_in(10) {
        return rng.some_bytes(40);
    }
    match value_type {
        ValueType::U32 => rng.interesting_u32().to_be_bytes().to_vec(),
        ValueType::S32 => rng.interesting_i32().to_be_bytes().to_vec(),
        ValueType::S64 => rng.interesting_u64().to_be_bytes().to_vec(),
        ValueType::Time => {
            let time = sample_buffer::time(rng);
            let mut payload = Vec::with_capacity(24);
            payload.extend_from_slice(&time.value.to_be_bytes());
            payload.extend_from_slice(&time.timescale.to_be_bytes());
            payload.extend_from_slice(&time.flags.to_be_bytes());
            payload.extend_from_slice(&time.epoch.to_be_bytes());
            payload
        }
        ValueType::Dimensions | ValueType::Point | ValueType::Size => {
            let mut payload = rng.interesting_i32().to_be_bytes().to_vec();
            payload.extend_from_slice(&rng.interesting_i32().to_be_bytes());
            payload
        }
        ValueType::Uuid => uuid(rng).0.to_vec(),
        ValueType::UuidArray => (0..rng.count(6)).flat_map(|_| uuid(rng).0).collect(),
        ValueType::PropertyList => {
            if rng.one_in(6) {
                Vec::new()
            } else {
                plist::bytes(rng)
            }
        }
        ValueType::FormatDescription => description::bytes(rng),
        ValueType::SampleBuffer => {
            let mut bytes = sample_buffer::any(rng).serialize();
            maybe_mutate(rng, &mut bytes, b"sbufsdatssizstiafdscdictarayaraykeyv", 3);
            bytes
        }
        ValueType::Data | ValueType::TagCollections => rng.some_bytes(64),
    }
}

/// A header and any parameters.
pub fn any(rng: &mut Rng) -> Vec<u8> {
    let mut bytes = MessageWriter::new(
        message_type(rng),
        flags(rng),
        Some(uuid(rng)),
        if rng.bool() {
            rng.below(200) as u64
        } else {
            rng.interesting_u64()
        },
    )
    .finish();
    for _ in 0..rng.count(10) {
        let key = key(rng);
        let value_type = *rng.pick(&VALUE_TYPES);
        let written_type = if rng.one_in(10) {
            FourCc(rng.next_u32())
        } else {
            value_type.fourcc()
        };
        let payload = payload(rng, value_type);
        raw_parameter(&mut bytes, key, written_type, &payload);
    }
    if rng.one_in(4) {
        bytes = with_surfaces(&surface_ids(rng), bytes);
    }
    if rng.one_in(8) {
        // A second header, as the fragments of a value have.
        let second = any(rng);
        bytes.extend(second);
    }
    maybe_mutate(rng, &mut bytes, b"vtpvpara=u32=s32=pls=sbf=fdssubs", 3);
    bytes
}

/// Mapping IDs for a `subs` box: from a few, so that surfaces repeat.
pub fn surface_ids(rng: &mut Rng) -> Vec<u64> {
    (0..rng.range(1, 5))
        .map(|_| {
            if rng.one_in(10) {
                rng.interesting_u64()
            } else {
                rng.range(1, 12)
            }
        })
        .collect()
}

/// `message` behind the `subs` box the guest kext puts before a message
/// that lends surfaces: five big-endian IDs, zero after the last.
pub fn with_surfaces(ids: &[u64], message: Vec<u8>) -> Vec<u8> {
    let mut bytes = vec![0, 0, 0, 0x30];
    bytes.extend_from_slice(b"subs");
    for slot in 0..5 {
        bytes.extend_from_slice(&ids.get(slot).copied().unwrap_or(0).to_be_bytes());
    }
    bytes.extend_from_slice(&message);
    bytes
}

/// A message whose `key` value is cut into fragments of at most `limit`
/// bytes a message, as the guest cuts a `dfrm`'s sample buffer: the first
/// message with `head`'s parameters and the first piece, each later one a
/// header of its own and the next piece, every one but the last flagged
/// 0x10. Each message is returned on its own, as the guest delivers them.
pub fn fragments(
    head: MessageWriter,
    header: (FourCc, MessageFlags, GuestUuid, u64),
    key: FourCc,
    value: &[u8],
    limit: usize,
) -> Vec<Vec<u8>> {
    let (message_type, flags, uuid, reply_id) = header;
    let mut messages = Vec::new();
    let mut message = head;
    let mut rest = value;
    loop {
        rest = message.sample_buffer_with_limit(key, limit, rest);
        if rest.is_empty() {
            messages.push(message.finish());
            return messages;
        }
        message.change_flags(MessageFlags::MORE_FRAGMENTS, MessageFlags::MORE_FRAGMENTS);
        messages.push(message.finish());
        message = MessageWriter::new(message_type, flags, Some(uuid), reply_id);
        if messages.len() > 4096 {
            return messages;
        }
    }
}
