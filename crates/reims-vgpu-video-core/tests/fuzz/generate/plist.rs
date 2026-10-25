//! Property lists as a guest could send them: values of every type,
//! encoded binary or as XML by the plist crate or as ASCII here, binary
//! plists written object by object here, whose references may be shared,
//! cyclic or out of range, and containers nested in text far past the
//! bound, as `wire::property_list::decode` must refuse them.

use std::fmt::Write as _;
use std::time::{Duration, UNIX_EPOCH};

use reims_vgpu_video_core::plist::{self, Value};

use crate::support::{Rng, maybe_mutate};

/// Keys the host reads, so that a dictionary often has one.
pub const KEYS: &[&str] = &[
    "VTHostBuildVersion",
    "PixelFormatType",
    "Width",
    "Height",
    "IOSurfaceProperties",
    "BytesPerRowAlignment",
    "RealTime",
    "Paravirtualized",
    "AverageBitRate",
    "DataRateLimits",
    "Quality",
    "ProfileLevel",
    "ExpectedFrameRate",
    "MaxKeyFrameInterval",
    "MaxKeyFrameIntervalDuration",
    "AllowFrameReordering",
    "H264EntropyMode",
    "ColorPrimaries",
    "TransferFunction",
    "YCbCrMatrix",
    "EncoderForceKeyframe",
    "Flags",
    "Subsampling",
    "MCUsHor",
    "MCUsVer",
    "DecodeWidth",
    "DecodeHeight",
    "CropRect",
    "X",
    "Y",
    "SampleDescriptionExtensionAtoms",
    "FormatName",
    "AV1FilmGrainMode",
    "",
];

/// A string: a key the host reads, one of its values, or anything.
pub fn string(rng: &mut Rng) -> String {
    match rng.below(4) {
        0 | 1 => (*rng.pick(KEYS)).to_owned(),
        2 => (*rng.pick(&[
            "Immediate",
            "CAVLC",
            "CABAC",
            "ITU_R_709_2",
            "ITU_R_2020",
            "SMPTE_ST_2084_PQ",
            "H264_Main_AutoLevel",
            "H264_High_5_1",
            "HEVC_Main_AutoLevel",
            "HEVC_Main10_AutoLevel",
            "com.apple.videotoolbox.videodecoder.avc",
        ]))
        .to_owned(),
        _ => (0..rng.count(40))
            .map(|_| char::from_u32(rng.range_u32(1, 0x2fff)).unwrap_or('x'))
            .collect(),
    }
}

/// An integer, often at a boundary.
pub fn integer(rng: &mut Rng) -> Value {
    match rng.below(4) {
        0 => Value::Integer(plist::Integer::from(rng.interesting_u64())),
        1 => Value::Integer(plist::Integer::from(-(rng.interesting_u32() as i64))),
        2 => Value::Integer(plist::Integer::from(i64::MIN)),
        _ => Value::Integer(plist::Integer::from(rng.below(5000) as i64)),
    }
}

/// A real, often one no setting means: NaN, an infinity, a huge one.
pub fn real(rng: &mut Rng) -> Value {
    Value::Real(*rng.pick(&[
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        -0.0,
        0.0,
        0.5,
        1.0,
        1.5,
        -1.0,
        29.97,
        1e300,
        -1e300,
        f64::MIN_POSITIVE,
        4294967296.0,
        2147483647.5,
        -9.3e18,
        1e-300,
    ]))
}

/// A scalar of any type.
pub fn scalar(rng: &mut Rng) -> Value {
    match rng.below(8) {
        0 => integer(rng),
        1 => real(rng),
        2 => Value::Boolean(rng.bool()),
        3 => Value::String(string(rng)),
        4 => Value::Data(rng.some_bytes(64)),
        5 => Value::Date(plist::Date::from(
            UNIX_EPOCH + Duration::from_secs(rng.range(0, 1 << 34)),
        )),
        6 => Value::Uid(plist::Uid::new(rng.interesting_u64())),
        _ => integer(rng),
    }
}

/// A value of any type, nested at most `depth` deep.
pub fn value(rng: &mut Rng, depth: u32) -> Value {
    if depth == 0 || rng.below(3) != 0 {
        return scalar(rng);
    }
    if rng.bool() {
        Value::Array((0..rng.count(12)).map(|_| value(rng, depth - 1)).collect())
    } else {
        Value::Dictionary(dictionary(rng, depth - 1))
    }
}

/// A dictionary whose keys are often the host's.
pub fn dictionary(rng: &mut Rng, depth: u32) -> plist::Dictionary {
    let mut dictionary = plist::Dictionary::new();
    for _ in 0..rng.count(12) {
        dictionary.insert(string(rng), value(rng, depth));
    }
    dictionary
}

/// A value encoded by the plist crate, binary or XML.
pub fn encode(value: &Value, xml: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    let written = if xml {
        value.to_writer_xml(&mut bytes)
    } else {
        value.to_writer_binary(&mut bytes)
    };
    if written.is_err() {
        bytes.clear();
    }
    bytes
}

/// A value written as an OpenStep plist, the ASCII form the plist crate
/// reads too: scalars as quoted strings, data as hex.
pub fn ascii(value: &Value, out: &mut String) {
    let quoted = |text: &str, out: &mut String| {
        out.push('"');
        for c in text.chars() {
            if c == '"' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
    };
    match value {
        Value::Array(array) => {
            out.push('(');
            for (i, value) in array.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                ascii(value, out);
            }
            out.push(')');
        }
        Value::Dictionary(dictionary) => {
            out.push('{');
            for (key, value) in dictionary {
                quoted(key, out);
                out.push('=');
                ascii(value, out);
                out.push(';');
            }
            out.push('}');
        }
        Value::Data(data) => {
            out.push('<');
            for byte in data {
                let _ = write!(out, "{byte:02x}");
            }
            out.push('>');
        }
        Value::String(text) => quoted(text, out),
        Value::Boolean(flag) => quoted(if *flag { "YES" } else { "NO" }, out),
        other => quoted(&format!("{other:?}"), out),
    }
}

/// Containers nested in text, XML or ASCII, most often about as deep as
/// `property_list::MAX_DEPTH` and now and then far deeper than a thread's
/// stack would survive dropping: closed, left open or broken off partway,
/// and with what follows the last.
pub fn nested(rng: &mut Rng) -> Vec<u8> {
    let depth = if rng.one_in(8) {
        rng.range(1, 30_000)
    } else {
        rng.range(1, 80)
    } as usize;
    let closed = match rng.below(4) {
        0 => depth - 1,
        1 => rng.below(depth + 1),
        _ => depth,
    };
    let xml = rng.bool();
    let mut text = String::new();
    if xml {
        text.push_str(r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0">"#);
    }
    let dictionaries: Vec<bool> = (0..depth).map(|_| rng.one_in(3)).collect();
    for &dictionary in &dictionaries {
        text.push_str(match (xml, dictionary) {
            (true, false) => "<array>",
            (true, true) => "<dict><key>k</key>",
            (false, false) => "(",
            (false, true) => "{k=",
        });
    }
    if rng.bool() {
        text.push_str(if xml { "<integer>1</integer>" } else { "1" });
    }
    for &dictionary in dictionaries.iter().rev().take(closed) {
        text.push_str(match (xml, dictionary) {
            (true, false) => "</array>",
            (true, true) => "</dict>",
            (false, false) => ")",
            (false, true) => ";}",
        });
    }
    if xml && closed == depth && rng.bool() {
        text.push_str("</plist>");
    }
    match rng.below(4) {
        0 => text.push_str(if xml { "<true/>" } else { "(" }),
        1 => text.push_str("\0}"),
        _ => {}
    }
    text.into_bytes()
}

/// Property list bytes as a guest could send them: a value encoded, a
/// binary plist written here, containers nested in text, or bytes, each
/// mutated now and then.
pub fn bytes(rng: &mut Rng) -> Vec<u8> {
    let mut bytes = match rng.below(12) {
        0..=4 => {
            let v = value(rng, 4);
            encode(&v, false)
        }
        5 | 6 => {
            let v = value(rng, 4);
            encode(&v, true)
        }
        7 | 8 => crafted(rng),
        9 => {
            let mut text = String::new();
            ascii(&value(rng, 4), &mut text);
            text.into_bytes()
        }
        10 => nested(rng),
        _ => {
            let mut bytes = b"bplist00".to_vec();
            bytes.extend(rng.some_bytes(80));
            bytes
        }
    };
    maybe_mutate(
        rng,
        &mut bytes,
        b"bplist00\x08\x09\xa2\x01\x02\xd1\x01\x01",
        3,
    );
    bytes
}

/// A binary plist written object by object (the format plist 1.10's
/// binary reader reads): markers of every kind, and containers whose
/// references may name any object, themselves and ones past the table
/// included, so that objects are shared, nest deeply and cycle.
pub fn crafted(rng: &mut Rng) -> Vec<u8> {
    let objects = 1 + rng.count(48);
    let ref_size: u8 = if objects > 255 || rng.one_in(6) {
        *rng.pick(&[2, 3, 4, 8])
    } else {
        1
    };
    let reference = |rng: &mut Rng, at: usize| -> u64 {
        match rng.below(10) {
            // The next object, which makes chains as deep as the table.
            0..=3 => (at + 1).min(objects - 1) as u64,
            4 => at as u64,
            5 if rng.one_in(4) => rng.interesting_u64(),
            _ => rng.below(objects) as u64,
        }
    };
    let mut body = b"bplist00".to_vec();
    let mut offsets = Vec::with_capacity(objects);
    for at in 0..objects {
        offsets.push(body.len() as u64);
        let length = |rng: &mut Rng| -> usize {
            if rng.one_in(10) {
                rng.below(300)
            } else {
                rng.below(6)
            }
        };
        match rng.below(12) {
            0 => body.push(*rng.pick(&[0x00, 0x08, 0x09, 0x0f])),
            1 => {
                let power = rng.below(5);
                body.push(0x10 | power as u8);
                body.extend(rng.bytes(1 << power));
            }
            2 => {
                let power = *rng.pick(&[2usize, 3]);
                body.push(0x20 | power as u8);
                body.extend(rng.bytes(1 << power));
            }
            3 => {
                body.push(0x33);
                body.extend(rng.bytes(8));
            }
            4 | 5 => {
                let marker = *rng.pick(&[0x40u8, 0x50, 0x60]);
                let n = length(rng);
                push_length(&mut body, marker, n);
                let unit = if marker == 0x60 { 2 } else { 1 };
                body.extend(rng.bytes(n * unit));
            }
            6 => {
                let n = rng.below(8);
                body.push(0x80 | n as u8);
                body.extend(rng.bytes(n + 1));
            }
            _ => {
                let marker = *rng.pick(&[0xa0u8, 0xa0, 0xc0, 0xd0, 0xd0]);
                let n = length(rng).min(40);
                push_length(&mut body, marker, n);
                let references = if marker == 0xd0 { 2 * n } else { n };
                for _ in 0..references {
                    let r = reference(rng, at);
                    push_sized(&mut body, r, ref_size);
                }
            }
        }
    }
    let table = body.len() as u64;
    let offset_size: u8 = if table > 0xffff {
        4
    } else if table > 0xff {
        2
    } else {
        1
    };
    let offset_size = if rng.one_in(10) {
        *rng.pick(&[0, 1, 2, 3, 4, 5, 8])
    } else {
        offset_size
    };
    for offset in offsets {
        push_sized(&mut body, offset, offset_size);
    }
    body.extend_from_slice(&[0; 6]);
    body.push(offset_size);
    body.push(ref_size);
    let count = if rng.one_in(12) {
        rng.interesting_u64()
    } else {
        objects as u64
    };
    let root = if rng.one_in(12) {
        rng.interesting_u64()
    } else {
        0
    };
    let table = if rng.one_in(12) {
        rng.interesting_u64()
    } else {
        table
    };
    body.extend_from_slice(&count.to_be_bytes());
    body.extend_from_slice(&root.to_be_bytes());
    body.extend_from_slice(&table.to_be_bytes());
    body
}

/// A marker with its length in its low nibble, or 0xf and an integer.
fn push_length(body: &mut Vec<u8>, marker: u8, length: usize) {
    if length < 15 {
        body.push(marker | length as u8);
    } else if length < 256 {
        body.extend_from_slice(&[marker | 0xf, 0x10, length as u8]);
    } else {
        body.extend_from_slice(&[marker | 0xf, 0x11]);
        body.extend_from_slice(&(length as u16).to_be_bytes());
    }
}

/// `value`'s low `size` bytes, big-endian.
fn push_sized(body: &mut Vec<u8>, value: u64, size: u8) {
    let bytes = value.to_be_bytes();
    let size = usize::from(size).min(8);
    body.extend_from_slice(&bytes[8 - size..]);
}
