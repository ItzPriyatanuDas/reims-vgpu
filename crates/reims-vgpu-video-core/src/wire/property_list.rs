//! Property lists as `=pls` parameters and sample buffer leaves carry them.
//!
//! CoreMedia's `FigCreateCFDataFromCFPropertyList` writes binary plists
//! (`bplist00`), and NULL as no bytes at all (measured on the M3 Max, macOS
//! 27.0.1, 26A434, 2026-10-24). Its reader, `FigCreateCFPropertyListFromData`,
//! reads XML and OpenStep plists as well, as CoreFoundation's
//! `CFPropertyListCreateWithData` does: the OpenStep `{a = 400; c = <0001>;}`
//! reads `400` as a string and `<0001>` as data (measured there,
//! 2026-10-25). So this side writes binary and reads any encoding, with the
//! plist crate, which reads OpenStep otherwise (docs/video/bugs.md, B82).
//!
//! Every property list comes from the guest, and is held to bounds of our
//! own before and while it is decoded. A binary plist names its objects by
//! index, and the plist crate reads a container again at each reference to
//! it, so a few hundred bytes that refer to one array twice at each of 64
//! levels would decode to 2^64 values. Its object graph is walked first,
//! and refused unless what it decodes to is within [`MAX_VALUES`],
//! [`MAX_DEPTH`] and [`MAX_EXTRA_BYTES`]. The value of any encoding is then
//! built here from the crate's events, and refused at the first value past
//! [`MAX_VALUES`] or [`MAX_DEPTH`]: no value deeper than the bound is ever
//! made, since dropping one recurses once a level, and the crate's own
//! builder drops what it has built when the plist fails to read further on.

use std::collections::HashMap;
use std::io::Cursor;

use crate::num::{ToU64, ToUsize};
use crate::wire::WireError;

/// The most values one property list decodes to, containers and keys
/// included. Ours: the guest's property lists are dictionaries of a few
/// dozen values.
pub const MAX_VALUES: u64 = 1 << 16;
/// The deepest nesting of containers, a scalar at the top being 1. Ours,
/// as above.
pub const MAX_DEPTH: u32 = 64;
/// The string and data bytes one property list may decode to beyond its
/// own length, which an object referred to more than once adds. Ours.
pub const MAX_EXTRA_BYTES: u64 = 16 << 20;

/// Decodes a property list of any encoding, within the bounds above.
pub fn decode(bytes: &[u8]) -> Result<plist::Value, WireError> {
    decode_within(bytes, &mut Budget::new(bytes.len()))
}

/// What property lists may still decode to: one alone, or a sample
/// buffer's between them, so that a serialization of many small plists
/// decodes to no more than one large one may.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Budget {
    /// Values, containers and keys included.
    values: u64,
    /// The bytes of strings and data a binary plist decodes to. A text
    /// plist decodes to no more of them than its own bytes, and spends
    /// none.
    bytes: u64,
}

impl Budget {
    /// The bounds of what is read from `length` bytes: [`MAX_VALUES`]
    /// values, and [`MAX_EXTRA_BYTES`] of strings and data beyond `length`.
    pub(crate) fn new(length: usize) -> Self {
        Budget {
            values: MAX_VALUES,
            bytes: MAX_EXTRA_BYTES.saturating_add(length.to_u64()),
        }
    }

    /// Spends `values` values that are not a property list's, or refuses
    /// where fewer are left.
    pub(crate) fn spend_values(&mut self, values: u64) -> Result<(), WireError> {
        self.values = self
            .values
            .checked_sub(values)
            .ok_or(WireError::Malformed("property lists too large"))?;
        Ok(())
    }
}

/// Decodes a property list of any encoding, and spends what it decodes to
/// from `budget`, refusing it where that is more than is left.
pub(crate) fn decode_within(bytes: &[u8], budget: &mut Budget) -> Result<plist::Value, WireError> {
    let bytes_spent = if bytes.starts_with(BINARY_MAGIC) {
        check_binary(bytes, budget)?
    } else {
        0
    };
    let (value, values) = build(
        plist::stream::Reader::new(Cursor::new(bytes)),
        budget.values,
    )?;
    budget.values = budget.values.saturating_sub(values);
    budget.bytes = budget.bytes.saturating_sub(bytes_spent);
    Ok(value)
}

/// What the plist crate takes for a binary plist (its `Reader::is_binary`).
const BINARY_MAGIC: &[u8] = b"bplist00";

/// A container [`build`] has open, as the plist crate's builder keeps one:
/// a dictionary with the key its next value goes under, once read.
enum Open {
    Array(Vec<plist::Value>),
    Dictionary(plist::Dictionary, Option<String>),
}

/// The value of a property list's events, built as plist 1.10's
/// `Value::from_reader` builds it (`value.rs`, `Builder`): a dictionary's
/// keys are strings, one value is the whole plist, and an event past it is
/// refused. Each value, container or key, is counted as it comes, and
/// refused past `max_values`, or past [`MAX_DEPTH`] where it would lie
/// deeper, a scalar at the top being 1. What is open is never deeper than
/// the bound, so it drops, on any refusal, by a recursion of 64 levels at
/// most. Gives the value and how many values it counted.
fn build(
    mut events: impl Iterator<Item = Result<plist::stream::OwnedEvent, plist::Error>>,
    max_values: u64,
) -> Result<(plist::Value, u64), WireError> {
    use plist::stream::Event;
    let unread = |error: plist::Error| WireError::PropertyList(error.to_string());
    // What the crate's builder refuses, as it refuses it.
    let malformed = |why: &str| Err(WireError::PropertyList(why.to_owned()));
    let mut open: Vec<Open> = Vec::new();
    let mut values = 0u64;
    let root = loop {
        let Some(event) = events.next() else {
            return malformed("property list ends before its value is whole");
        };
        let event = event.map_err(unread)?;
        if !matches!(event, Event::EndCollection) {
            // A value, a container or a key, one level below what is open.
            values += 1;
            if values > max_values || open.len() >= MAX_DEPTH.to_usize() {
                return Err(WireError::Malformed("property list too large or too deep"));
            }
        }
        let value = match event {
            Event::StartArray(_) => {
                open.push(Open::Array(Vec::new()));
                continue;
            }
            Event::StartDictionary(_) => {
                open.push(Open::Dictionary(plist::Dictionary::new(), None));
                continue;
            }
            Event::EndCollection => match open.pop() {
                Some(Open::Array(array)) => plist::Value::Array(array),
                Some(Open::Dictionary(dictionary, None)) => plist::Value::Dictionary(dictionary),
                Some(Open::Dictionary(_, Some(_))) => {
                    return malformed("property list key without a value");
                }
                None => return malformed("property list ends a container it did not open"),
            },
            Event::Boolean(flag) => plist::Value::Boolean(flag),
            Event::Data(data) => plist::Value::Data(data.into_owned()),
            Event::Date(date) => plist::Value::Date(date),
            Event::Integer(integer) => plist::Value::Integer(integer),
            Event::Real(real) => plist::Value::Real(real),
            Event::String(string) => plist::Value::String(string.into_owned()),
            Event::Uid(uid) => plist::Value::Uid(uid),
            _ => return malformed("property list event of an unknown kind"),
        };
        match open.last_mut() {
            None => break value,
            Some(Open::Array(array)) => array.push(value),
            Some(Open::Dictionary(dictionary, key)) => match (key.take(), value) {
                (None, plist::Value::String(name)) => *key = Some(name),
                (None, _) => return malformed("property list key that is not a string"),
                (Some(name), value) => {
                    dictionary.insert(name, value);
                }
            },
        }
    };
    match events.next() {
        None => Ok((root, values)),
        Some(Err(error)) => Err(unread(error)),
        Some(Ok(_)) => malformed("property list goes on past its value"),
    }
}

/// A binary plist's trailer: its last 32 bytes.
const TRAILER_SIZE: usize = 32;

/// Refuses a binary plist whose objects decode to more than `budget` has
/// left, by walking its object graph as plist 1.10's binary reader reads
/// it: the 32-byte trailer, the offset table, each object's marker, and a
/// container's references, which a dictionary has for its keys and then
/// its values. What does not read here is refused, as the reader would
/// refuse it; a reference cycle is refused, as the reader refuses one it
/// meets.
///
/// Each object is visited once and what it decodes to is remembered, and
/// the walk refuses as soon as what it has added up, or the containers it
/// has open, pass a bound, so it reads no more references than the
/// values left, [`MAX_VALUES`] at most. Every reference it reads is a value the plist
/// decodes to at least once (the plist's tree has a node for each), so a
/// plist it stops reading would decode past the bound too. A container's
/// references are read one at a time as the walk reaches them: many
/// objects may share one offset, or overlap, and none is read whole
/// before the walk knows it is within the bounds.
///
/// Gives the bytes of strings and data the plist decodes to.
fn check_binary(bytes: &[u8], budget: &Budget) -> Result<u64, WireError> {
    let refuse = |why| Err(WireError::Malformed(why));
    let Some(end) = bytes.len().checked_sub(TRAILER_SIZE) else {
        return refuse("binary property list without a trailer");
    };
    let Some(&[_, _, _, _, _, _, offset_size, ref_size, ref rest @ ..]) = bytes.get(end..) else {
        return refuse("binary property list without a trailer");
    };
    if !matches!(offset_size, 1 | 2 | 3 | 4 | 8) || !matches!(ref_size, 1 | 2 | 3 | 4 | 8) {
        return refuse("binary property list integer size");
    }
    let field = |at: usize| {
        rest.get(at..at + 8)
            .and_then(|field| <[u8; 8]>::try_from(field).ok())
            .map_or(0, u64::from_be_bytes)
    };
    let (objects, root, table) = (field(0), field(8), field(16));
    let plist = Objects { bytes, end };
    let table_end = objects
        .checked_mul(u64::from(offset_size))
        .and_then(|length| length.checked_add(table));
    if table_end.is_none_or(|table_end| table_end > plist.end_u64()) {
        return refuse("binary property list offset table");
    }
    // The plist crate's reader keeps 9 bytes for each object in the table.
    // CoreFoundation's writer writes each object the root reaches once, and
    // no other, so a plist of more objects than values left decodes past
    // the bound too.
    if objects > budget.values {
        return refuse("property list too large or too deep");
    }
    let index = |reference: u64| {
        usize::try_from(reference)
            .ok()
            .filter(|_| reference < objects)
            .ok_or(WireError::Malformed("binary property list reference"))
    };
    // Every offset in the table lies before the trailer, as checked above.
    let offset = |object: usize| {
        u64::try_from(object)
            .ok()
            .and_then(|object| object.checked_mul(u64::from(offset_size)))
            .and_then(|at| plist.int(table.checked_add(at)?, offset_size))
            .ok_or(WireError::Malformed("binary property list offset"))
    };

    let mut visits: HashMap<usize, Visit> = HashMap::new();
    let mut open: Vec<OpenContainer> = Vec::new();
    let mut references_left = budget.values;
    let mut root_totals = None;
    let mut next = Some(index(root)?);
    loop {
        if let Some(object) = next.take() {
            match plist.contents(offset(object)?, ref_size)? {
                Contents::Container { first, count } if count > 0 => {
                    references_left = references_left
                        .checked_sub(count)
                        .ok_or(WireError::Malformed("property list too large or too deep"))?;
                    if open.len() >= MAX_DEPTH.to_usize() {
                        return refuse("property list too large or too deep");
                    }
                    visits.insert(object, Visit::Open);
                    open.push(OpenContainer {
                        object,
                        first,
                        count,
                        next: 0,
                        totals: Totals::ONE,
                    });
                }
                Contents::Container { .. } => {
                    finish(
                        &mut visits,
                        &mut open,
                        &mut root_totals,
                        object,
                        Totals::ONE,
                    );
                }
                Contents::Scalar(own_bytes) => {
                    let totals = Totals {
                        extra: own_bytes,
                        ..Totals::ONE
                    };
                    finish(&mut visits, &mut open, &mut root_totals, object, totals);
                }
            }
        }
        let Some(container) = open.last_mut() else {
            break;
        };
        if container.next < container.count {
            let at = container
                .next
                .checked_mul(u64::from(ref_size))
                .and_then(|at| at.checked_add(container.first));
            container.next += 1;
            let child = index(
                at.and_then(|at| plist.int(at, ref_size))
                    .ok_or(WireError::Malformed("binary property list reference"))?,
            )?;
            match visits.get(&child) {
                None => next = Some(child),
                Some(Visit::Open) => return refuse("binary property list refers to itself"),
                Some(&Visit::Done(totals)) => container.totals.add(totals),
            }
        } else if let Some(done) = open.pop() {
            finish(
                &mut visits,
                &mut open,
                &mut root_totals,
                done.object,
                done.totals,
            );
        }
        // What is open decodes to at least what it has added up, and the
        // root to at least what any of them does.
        if open.last().is_some_and(|open| !open.totals.within(budget)) {
            return refuse("property list too large or too deep");
        }
    }
    match root_totals {
        Some(totals) if totals.within(budget) => Ok(totals.extra),
        _ => refuse("property list too large or too deep"),
    }
}

/// What an object decodes to, as far as [`check_binary`] has added it up:
/// its values, the string and data bytes it decodes to, and its depth, a
/// scalar being 1.
#[derive(Clone, Copy, Debug)]
struct Totals {
    values: u64,
    extra: u64,
    depth: u32,
}

impl Totals {
    /// One value and nothing inside it.
    const ONE: Totals = Totals {
        values: 1,
        extra: 0,
        depth: 1,
    };

    /// Adds a child it refers to once more.
    fn add(&mut self, child: Totals) {
        self.values = self.values.saturating_add(child.values);
        self.extra = self.extra.saturating_add(child.extra);
        self.depth = self.depth.max(child.depth.saturating_add(1));
    }

    /// Whether it is within [`MAX_DEPTH`] and what `budget` has left.
    fn within(self, budget: &Budget) -> bool {
        self.values <= budget.values && self.depth <= MAX_DEPTH && self.extra <= budget.bytes
    }
}

/// An object [`check_binary`] has met: open, its references being walked,
/// or done, with what it decodes to.
enum Visit {
    Open,
    Done(Totals),
}

/// A container [`check_binary`] has open: `count` references from byte
/// `first`, of which `next` is the next to read, and what it has added up.
struct OpenContainer {
    object: usize,
    first: u64,
    count: u64,
    next: u64,
    totals: Totals,
}

/// Marks `object` done with `totals`, and adds it to the container that
/// referred to it, or makes it the root's.
fn finish(
    visits: &mut HashMap<usize, Visit>,
    open: &mut [OpenContainer],
    root: &mut Option<Totals>,
    object: usize,
    totals: Totals,
) {
    visits.insert(object, Visit::Done(totals));
    match open.last_mut() {
        Some(container) => container.totals.add(totals),
        None => *root = Some(totals),
    }
}

/// What [`Objects::contents`] finds at an object's offset.
enum Contents {
    /// A scalar, with the string or data bytes it decodes to, 0 for one
    /// that is neither.
    Scalar(u64),
    /// An array's references, or a dictionary's keys' and then values':
    /// `count` of them from byte `first`.
    Container { first: u64, count: u64 },
}

/// A binary plist's bytes, and where its trailer starts: every read of an
/// object ends before it.
struct Objects<'a> {
    bytes: &'a [u8],
    end: usize,
}

impl Objects<'_> {
    fn end_u64(&self) -> u64 {
        self.end.to_u64()
    }

    /// The big-endian integer of `size` bytes at `at`, where it ends before
    /// the trailer.
    fn int(&self, at: u64, size: u8) -> Option<u64> {
        let at = usize::try_from(at).ok()?;
        let end = at.checked_add(usize::from(size))?;
        let field = self.bytes.get(at..end).filter(|_| end <= self.end)?;
        Some(
            field
                .iter()
                .fold(0u64, |value, &byte| (value << 8) | u64::from(byte)),
        )
    }

    /// What the object at `offset` is, as plist 1.10's `read_next` reads
    /// it (`binary_reader.rs`): a container's references, `ref_size` bytes
    /// each, all before the trailer, or the bytes a string or data decodes
    /// to of its own.
    fn contents(&self, offset: u64, ref_size: u8) -> Result<Contents, WireError> {
        let malformed = || WireError::Malformed("binary property list object");
        let at = usize::try_from(offset)
            .ok()
            .filter(|&at| at < self.end)
            .ok_or_else(malformed)?;
        let marker = *self.bytes.get(at).ok_or_else(malformed)?;
        let (kind, low) = (marker >> 4, marker & 0x0f);
        // `read_object_len`: the low nibble, or after 0xf an integer whose
        // size is 2 to the power of the next byte's low two bits, ending
        // before the trailer.
        let length = || -> Result<(u64, u64), WireError> {
            let after_marker = offset.saturating_add(1);
            if low != 0x0f {
                return Ok((u64::from(low), after_marker));
            }
            let power = self.bytes.get(at.saturating_add(1)).ok_or_else(malformed)? & 3;
            let size = 1u8 << power;
            let start = offset.saturating_add(2);
            let length = self.int(start, size).ok_or_else(malformed)?;
            Ok((length, start.saturating_add(u64::from(size))))
        };
        match kind {
            0x4 | 0x5 => Ok(Contents::Scalar(length()?.0)),
            0x6 => Ok(Contents::Scalar(length()?.0.saturating_mul(2))),
            0xa | 0xd => {
                let (count, first) = length()?;
                let count = if kind == 0xd {
                    count.checked_mul(2).ok_or_else(malformed)?
                } else {
                    count
                };
                let references_end = count
                    .checked_mul(u64::from(ref_size))
                    .and_then(|length| length.checked_add(first))
                    .ok_or_else(malformed)?;
                if references_end > self.end_u64() {
                    return Err(malformed());
                }
                Ok(Contents::Container { first, count })
            }
            _ => Ok(Contents::Scalar(0)),
        }
    }
}

/// Encodes a property list as a binary plist.
pub fn encode(value: &plist::Value) -> Result<Vec<u8>, WireError> {
    let mut bytes = Vec::new();
    value
        .to_writer_binary(&mut bytes)
        .map_err(|error| WireError::Unwritable(error.to_string()))?;
    Ok(bytes)
}

/// A dictionary built from string keys and values, in that order.
pub fn dictionary<const N: usize>(entries: [(&str, plist::Value); N]) -> plist::Dictionary {
    let mut dictionary = plist::Dictionary::new();
    for (key, value) in entries {
        dictionary.insert(key.to_owned(), value);
    }
    dictionary
}

/// A number property list value.
pub fn integer(value: i64) -> plist::Value {
    plist::Value::Integer(value.into())
}

/// The integer a number value holds, if it holds one.
pub fn as_i64(value: &plist::Value) -> Option<i64> {
    match value {
        plist::Value::Integer(integer) => integer.as_signed(),
        plist::Value::Boolean(flag) => Some(i64::from(*flag)),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::runtime::{Runtime, StdRuntime};

    #[test]
    fn writes_binary_and_reads_it_back() -> Result<(), WireError> {
        let value = plist::Value::Dictionary(dictionary([
            ("VTHostBuildVersion", plist::Value::String("x".into())),
            ("VTRating", integer(400)),
        ]));
        let bytes = encode(&value)?;
        assert_eq!(bytes.get(..8), Some(&b"bplist00"[..]));
        assert_eq!(decode(&bytes)?, value);
        Ok(())
    }

    #[test]
    fn reads_xml_too() -> Result<(), WireError> {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>a</key><integer>6</integer></dict></plist>"#;
        assert_eq!(
            decode(xml)?,
            plist::Value::Dictionary(dictionary([("a", integer(6))]))
        );
        Ok(())
    }

    #[test]
    fn garbage_is_a_property_list_error() {
        assert!(matches!(
            decode(b"not a plist"),
            Err(WireError::PropertyList(_))
        ));
    }

    /// A binary plist of these objects, written from the format: each
    /// object's bytes, 2-byte references, 4-byte offsets, object 0 the
    /// root.
    pub(crate) fn binary(objects: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = b"bplist00".to_vec();
        let mut offsets = Vec::new();
        for object in objects {
            offsets.push(u32::try_from(bytes.len()).unwrap_or(0));
            bytes.extend_from_slice(object);
        }
        let table = bytes.len() as u64;
        for offset in offsets {
            bytes.extend_from_slice(&offset.to_be_bytes());
        }
        bytes.extend_from_slice(&[0; 6]);
        bytes.extend_from_slice(&[4, 2]);
        bytes.extend_from_slice(&(objects.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&0u64.to_be_bytes());
        bytes.extend_from_slice(&table.to_be_bytes());
        bytes
    }

    /// An array object of 2-byte references.
    pub(crate) fn array(references: &[u16]) -> Vec<u8> {
        let mut object = vec![0xaf, 0x11];
        object.extend_from_slice(&u16::try_from(references.len()).unwrap_or(0).to_be_bytes());
        for reference in references {
            object.extend_from_slice(&reference.to_be_bytes());
        }
        object
    }

    #[test]
    fn the_binary_writers_own_plists_pass_the_walk() -> Result<(), WireError> {
        let value = plist::Value::Dictionary(dictionary([
            ("a", plist::Value::Array(vec![integer(1), integer(2)])),
            ("b", plist::Value::Data(vec![7; 300])),
            (
                "c",
                plist::Value::Dictionary(dictionary([("d", plist::Value::Boolean(true))])),
            ),
        ]));
        assert_eq!(decode(&encode(&value)?)?, value);
        // Hand-written: an array of three references to one integer.
        let three = binary(&[array(&[1, 1, 1]), vec![0x10, 7]]);
        assert_eq!(
            decode(&three)?,
            plist::Value::Array(vec![integer(7), integer(7), integer(7)])
        );
        Ok(())
    }

    #[test]
    fn an_array_that_refers_to_one_array_twice_at_each_level_is_refused() {
        // 2^40 values from 41 objects: refused by the walk, before the
        // decoder tries to make them.
        let levels = 40u16;
        let mut objects: Vec<Vec<u8>> = (0..levels).map(|i| array(&[i + 1, i + 1])).collect();
        objects.push(vec![0x10, 1]);
        let started = StdRuntime.now();
        assert!(matches!(
            decode(&binary(&objects)),
            Err(WireError::Malformed(_))
        ));
        assert!(StdRuntime.elapsed_since(started) < std::time::Duration::from_secs(1));
    }

    #[test]
    fn a_string_referred_to_too_often_is_refused() {
        // 30000 references to one 1 KiB string decode to 30 MB.
        let mut string = vec![0x5f, 0x11, 0x04, 0x00];
        string.extend_from_slice(&[b'x'; 1024]);
        let objects = [array(&[1; 30_000]), string];
        assert!(matches!(
            decode(&binary(&objects)),
            Err(WireError::Malformed(_))
        ));
    }

    #[test]
    fn a_binary_chain_deeper_than_the_limit_is_refused() -> Result<(), WireError> {
        let chain = |depth: u16| {
            let mut objects: Vec<Vec<u8>> = (1..depth).map(|i| array(&[i])).collect();
            objects.push(vec![0x09]);
            binary(&objects)
        };
        let deepest = u16::try_from(MAX_DEPTH).unwrap_or(0);
        decode(&chain(deepest))?;
        assert!(matches!(
            decode(&chain(deepest + 1)),
            Err(WireError::Malformed(_))
        ));
        // Far deeper than a thread's stack would survive dropping.
        assert!(matches!(
            decode(&chain(20_000)),
            Err(WireError::Malformed(_))
        ));
        Ok(())
    }

    /// A root array of `objects` references, each to another object at
    /// one offset, an array of `references` references to one integer:
    /// written from the format with 4-byte references and offsets.
    fn sharing_one_offset(objects: u32, references: u32) -> Vec<u8> {
        let mut bytes = b"bplist00".to_vec();
        let array = |bytes: &mut Vec<u8>, count: u32, reference: &dyn Fn(u32) -> u32| {
            bytes.extend_from_slice(&[0xaf, 0x12]);
            bytes.extend_from_slice(&count.to_be_bytes());
            for i in 0..count {
                bytes.extend_from_slice(&reference(i).to_be_bytes());
            }
        };
        let offset = |bytes: &Vec<u8>| u32::try_from(bytes.len()).unwrap_or(0);
        let root = offset(&bytes);
        array(&mut bytes, objects, &|i| i + 1);
        let shared = offset(&bytes);
        array(&mut bytes, references, &|_| objects + 1);
        let integer = offset(&bytes);
        bytes.extend_from_slice(&[0x10, 1]);
        let table = bytes.len() as u64;
        bytes.extend_from_slice(&root.to_be_bytes());
        for _ in 0..objects {
            bytes.extend_from_slice(&shared.to_be_bytes());
        }
        bytes.extend_from_slice(&integer.to_be_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0, 4, 4]);
        bytes.extend_from_slice(&(u64::from(objects) + 2).to_be_bytes());
        bytes.extend_from_slice(&0u64.to_be_bytes());
        bytes.extend_from_slice(&table.to_be_bytes());
        bytes
    }

    #[test]
    fn objects_as_many_as_the_values_allow_decode() -> Result<(), WireError> {
        // The root, an array of one reference, then integers that nothing
        // refers to past the first.
        let objects = |count: u64| {
            let mut objects = vec![array(&[1])];
            objects.resize(usize::try_from(count).unwrap_or(0), vec![0x10, 7]);
            binary(&objects)
        };
        assert_eq!(
            decode(&objects(MAX_VALUES))?,
            plist::Value::Array(vec![integer(7)])
        );
        assert_eq!(
            decode(&objects(MAX_VALUES + 1)),
            Err(WireError::Malformed("property list too large or too deep"))
        );
        Ok(())
    }

    #[test]
    fn objects_that_share_one_offset_are_refused_in_linear_time() {
        // 240 KB: walked object by object, each reading its 20,000
        // references, it took 7.5 s in a release build.
        let started = StdRuntime.now();
        assert!(matches!(
            decode(&sharing_one_offset(20_000, 20_000)),
            Err(WireError::Malformed(_))
        ));
        assert!(StdRuntime.elapsed_since(started) < std::time::Duration::from_secs(1));
    }

    #[test]
    fn references_as_many_as_the_values_allow_decode() -> Result<(), WireError> {
        // The root, one array and its references: one value under the
        // bound, at it, and past it.
        let last = u32::try_from(MAX_VALUES).unwrap_or(0) - 2;
        decode(&sharing_one_offset(1, last - 1))?;
        decode(&sharing_one_offset(1, last))?;
        assert!(matches!(
            decode(&sharing_one_offset(1, last + 1)),
            Err(WireError::Malformed(_))
        ));
        Ok(())
    }

    #[test]
    fn a_binary_cycle_is_refused() {
        let objects = [array(&[1]), array(&[0])];
        assert!(decode(&binary(&objects)).is_err());
    }

    #[test]
    fn deep_xml_is_refused_and_dropped_without_recursion() {
        let depth = 200_000;
        let mut xml =
            String::from(r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0">"#);
        xml.push_str(&"<array>".repeat(depth));
        xml.push_str(&"</array>".repeat(depth));
        xml.push_str("</plist>");
        assert!(matches!(
            decode(xml.as_bytes()),
            Err(WireError::Malformed(_))
        ));
    }

    /// Containers `depth` deep as XML, of which the innermost `closed` are
    /// closed.
    fn xml_chain(depth: usize, closed: usize) -> String {
        let mut xml =
            String::from(r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0">"#);
        xml.push_str(&"<array>".repeat(depth));
        xml.push_str(&"</array>".repeat(closed));
        if closed == depth {
            xml.push_str("</plist>");
        }
        xml
    }

    /// The same as an OpenStep (ASCII) plist.
    fn ascii_chain(depth: usize, closed: usize) -> String {
        let mut ascii = "(".repeat(depth);
        ascii.push_str(&")".repeat(closed));
        ascii
    }

    #[test]
    fn deep_text_that_breaks_off_is_refused_without_recursion() {
        // Each closes all but its outermost container, then ends: the
        // crate's builder would drop the value inside, 199,999 deep, once
        // a level, past a thread's stack. Followed by a second value, it
        // would fail after the whole value too.
        let depth = 200_000;
        for text in [
            xml_chain(depth, depth - 1),
            ascii_chain(depth, depth - 1),
            ascii_chain(depth, depth) + "(",
        ] {
            assert!(matches!(
                decode(text.as_bytes()),
                Err(WireError::Malformed(_))
            ));
        }
    }

    #[test]
    fn text_nests_as_deep_as_the_limit_and_no_deeper() -> Result<(), WireError> {
        let deepest = MAX_DEPTH as usize;
        for chain in [xml_chain, ascii_chain] {
            decode(chain(deepest, deepest).as_bytes())?;
            assert!(matches!(
                decode(chain(deepest + 1, deepest + 1).as_bytes()),
                Err(WireError::Malformed(_))
            ));
        }
        Ok(())
    }

    #[test]
    fn text_of_more_values_than_the_limit_is_refused() -> Result<(), WireError> {
        // An array and its elements: MAX_VALUES values, then one more.
        let array = |elements: u64| {
            let mut ascii = "(".to_owned();
            for _ in 0..elements {
                ascii.push_str("1,");
            }
            ascii.push(')');
            ascii
        };
        decode(array(MAX_VALUES - 1).as_bytes())?;
        assert!(matches!(
            decode(array(MAX_VALUES).as_bytes()),
            Err(WireError::Malformed(_))
        ));
        Ok(())
    }

    #[test]
    fn text_is_built_as_the_crates_builder_builds_it() {
        for text in [
            r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>a</key><array><integer>1</integer><real>0.5</real><true/></array><key>b</key><data>AAE=</data></dict></plist>"#,
            "{a = (1, 2); b = {c = d;}; a = 3;}",
            "{(1) = 2;}",
            "{a = 1; b}",
            "(1, 2) (3)",
            "(1, 2",
            "((1), 2))",
            "",
        ] {
            let ours = decode(text.as_bytes()).ok();
            let crates = plist::Value::from_reader(Cursor::new(text.as_bytes())).ok();
            assert_eq!(ours, crates, "{text}");
        }
    }
}
