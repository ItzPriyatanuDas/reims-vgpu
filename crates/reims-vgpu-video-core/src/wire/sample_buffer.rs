//! Sample buffers as `=sbf` parameters carry them: an `sbuf` atom whose children hold the
//! timing, the data, the sizes, the format description when it changed, a cryptor recipe and
//! the attachments, as Apple's serializer writes and reads them.
//!
//! The atom headers are big-endian; the scalars inside them are native,
//! which is little-endian on both the x86_64 guest and an x86_64 host. A
//! property list leaf (`nmbv`, `bulv`, `strk`, `datv`, `dtev`) holds a
//! binary plist of one value. Dictionary keys are a known key's index
//! (`idxk`, a native u16) or a `strk` string.

use crate::fourcc::FourCc;
use crate::wire::WireError;
use crate::wire::atom::{Atom, AtomWriter, siblings};
use crate::wire::known_keys;
use crate::wire::message::MediaTime;
use crate::wire::property_list::{self, Budget};

const SAMPLE_BUFFER: FourCc = FourCc::new(*b"sbuf");
const OUTPUT_PRESENTATION_TIME: FourCc = FourCc::new(*b"opts");
const TIMING: FourCc = FourCc::new(*b"stia");
const DATA: FourCc = FourCc::new(*b"sdat");
const SAMPLE_COUNT: FourCc = FourCc::new(*b"nsmp");
const SIZES: FourCc = FourCc::new(*b"ssiz");
const FORMAT_DESCRIPTION: FourCc = FourCc::new(*b"fdsc");
const CRYPTOR: FourCc = FourCc::new(*b"crpt");
const PROPAGATED_ATTACHMENTS: FourCc = FourCc::new(*b"satt");
const OTHER_ATTACHMENTS: FourCc = FourCc::new(*b"sat2");
const SAMPLE_ATTACHMENTS: FourCc = FourCc::new(*b"sary");

const MEDIA_TYPE: FourCc = FourCc::new(*b"mdia");
const DIMENSIONS: FourCc = FourCc::new(*b"vdim");
const CODEC: FourCc = FourCc::new(*b"codc");
const EXTENSIONS: FourCc = FourCc::new(*b"extn");

const DICTIONARY: FourCc = FourCc::new(*b"dict");
const ENTRY: FourCc = FourCc::new(*b"keyv");
const KEY_INDEX: FourCc = FourCc::new(*b"idxk");
const ARRAY: FourCc = FourCc::new(*b"aray");
const NULL: FourCc = FourCc::new(*b"null");
const NUMBER: FourCc = FourCc::new(*b"nmbv");
const BOOLEAN: FourCc = FourCc::new(*b"bulv");
const STRING: FourCc = FourCc::new(*b"strk");
const BYTES: FourCc = FourCc::new(*b"datv");
const DATE: FourCc = FourCc::new(*b"dtev");
const COLOR_SPACE: FourCc = FourCc::new(*b"clrs");
const URL: FourCc = FourCc::new(*b"urlv");

const RECIPE_ID: FourCc = FourCc::new(*b"kdcr");
const RECIPE_VERSION: FourCc = FourCc::new(*b"vepm");
const RECIPE_KIND: FourCc = FourCc::new(*b"ctyp");

/// `kCMMediaType_Video`.
pub const MEDIA_TYPE_VIDEO: FourCc = FourCc::new(*b"vide");
/// `kCMMediaType_AuxiliaryPicture`.
pub const MEDIA_TYPE_AUXILIARY_PICTURE: FourCc = FourCc::new(*b"auxv");
/// The other media type the format description reader takes.
pub const MEDIA_TYPE_PICTURE: FourCc = FourCc::new(*b"pict");

/// The size of one native `CMSampleTimingInfo`: duration, presentation and
/// decode time stamps.
pub const TIMING_SIZE: usize = 72;

/// A value in a serialized dictionary or array.
#[derive(Clone, Debug, PartialEq)]
pub enum SerializedValue {
    /// `dict`: entries in the order they were written.
    Dictionary(Vec<(String, SerializedValue)>),
    /// `aray`.
    Array(Vec<SerializedValue>),
    /// A property list leaf: a number, boolean, string, data or date.
    PropertyList(plist::Value),
    /// `null`: kCFNull.
    Null,
    /// `clrs`: a CGColorSpace, as its property list.
    ColorSpace(plist::Value),
    /// `urlv`: a URL, as its absolute string.
    Url(String),
    /// `fdsc`: a format description.
    FormatDescription(Box<SerializedFormatDescription>),
}

/// A format description as the serialization carries it: its media type,
/// dimensions, codec and extensions, with no ImageDescription.
#[derive(Clone, Debug, PartialEq)]
pub struct SerializedFormatDescription {
    /// The media type, `vide` for video.
    pub media_type: FourCc,
    /// The codec type.
    pub codec: FourCc,
    /// The width.
    pub width: i32,
    /// The height.
    pub height: i32,
    /// The extensions dictionary, such as
    /// `SampleDescriptionExtensionAtoms` = {`avcC` = data}.
    pub extensions: Option<Vec<(String, SerializedValue)>>,
}

impl SerializedFormatDescription {
    /// The value of the extension `key`.
    pub fn extension(&self, key: &str) -> Option<&SerializedValue> {
        entry(self.extensions.as_deref()?, key)
    }

    /// The atom `kind` of the `SampleDescriptionExtensionAtoms` extension,
    /// such as `avcC`.
    pub fn sample_description_atom(&self, kind: &str) -> Option<&[u8]> {
        let SerializedValue::Dictionary(atoms) =
            self.extension("SampleDescriptionExtensionAtoms")?
        else {
            return None;
        };
        match entry(atoms, kind)? {
            SerializedValue::PropertyList(plist::Value::Data(bytes)) => Some(bytes),
            _ => None,
        }
    }
}

/// The entry `key` of a serialized dictionary.
pub fn entry<'a>(
    entries: &'a [(String, SerializedValue)],
    key: &str,
) -> Option<&'a SerializedValue> {
    entries
        .iter()
        .find(|(found, _)| found == key)
        .map(|(_, value)| value)
}

/// One sample's timing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleTiming {
    /// The duration.
    pub duration: MediaTime,
    /// The presentation time stamp.
    pub presentation: MediaTime,
    /// The decode time stamp.
    pub decode: MediaTime,
}

impl SampleTiming {
    fn from_native(bytes: &[u8]) -> Option<Self> {
        Some(Self {
            duration: MediaTime::from_native(bytes.get(0..24)?)?,
            presentation: MediaTime::from_native(bytes.get(24..48)?)?,
            decode: MediaTime::from_native(bytes.get(48..72)?)?,
        })
    }

    fn to_native(self) -> [u8; TIMING_SIZE] {
        let mut bytes = [0; TIMING_SIZE];
        bytes[0..24].copy_from_slice(&self.duration.to_native());
        bytes[24..48].copy_from_slice(&self.presentation.to_native());
        bytes[48..72].copy_from_slice(&self.decode.to_native());
        bytes
    }
}

/// A protected sample's cryptor recipe (`crpt`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CryptorRecipe {
    /// `kdcr`: the recipe's identifier.
    pub recipe_id: u64,
    /// `vepm`: its version, 3 when absent.
    pub version: u32,
    /// `ctyp`: its kind.
    pub kind: u32,
}

/// A sample buffer read from, or to be written as, a serialization.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SampleBuffer {
    /// The number of samples: `nsmp`, or 1 when there is data and no
    /// count.
    pub sample_count: u32,
    /// The timing entries.
    pub timings: Vec<SampleTiming>,
    /// The sample sizes.
    pub sizes: Vec<u32>,
    /// The sample data.
    pub data: Option<Vec<u8>>,
    /// The output presentation time stamp, when one was set.
    pub output_presentation_time: Option<MediaTime>,
    /// The format description, when this serialization carries one. The
    /// writer leaves it out when it equals the last one it wrote, so a
    /// reader keeps the last one it read.
    pub format_description: Option<SerializedFormatDescription>,
    /// The cryptor recipe of a protected sample.
    pub cryptor: Option<CryptorRecipe>,
    /// `satt`: the attachments that propagate.
    pub propagated_attachments: Option<Vec<(String, SerializedValue)>>,
    /// `sat2`: the attachments that do not.
    pub other_attachments: Option<Vec<(String, SerializedValue)>>,
    /// `sary`: each sample's attachments.
    pub sample_attachments: Option<Vec<SerializedValue>>,
}

fn native_u32(atom: &Atom<'_>) -> Result<u32, WireError> {
    atom.payload()
        .get(..4)
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
        .map(u32::from_le_bytes)
        .ok_or(WireError::Malformed("short atom"))
}

fn native_i32(bytes: &[u8]) -> Option<i32> {
    Some(i32::from_le_bytes(bytes.get(..4)?.try_into().ok()?))
}

/// The children of `atom`; an atom with no payload has none.
fn children<'a>(atom: &Atom<'a>) -> impl Iterator<Item = Result<Atom<'a>, WireError>> {
    siblings(atom.payload())
}

/// The first child of `atom`, which must be of type `kind`.
fn only_child<'a>(atom: &Atom<'a>, kind: FourCc) -> Result<Option<Atom<'a>>, WireError> {
    match children(atom).next().transpose()? {
        Some(child) if child.kind == kind => Ok(Some(child)),
        Some(_) => Err(WireError::Malformed("unexpected child atom")),
        None => Ok(None),
    }
}

fn read_property_list(atom: &Atom<'_>, budget: &mut Budget) -> Result<plist::Value, WireError> {
    property_list::decode_within(atom.payload(), budget)
}

/// The deepest a serialization's dictionaries, arrays and format
/// descriptions nest, where the attachment atoms' own dictionary or array
/// is level one. Ours: Apple's reader recurses once a level with no limit,
/// and so would this one, past the stack of a session's thread, at 8 bytes
/// of `aray` a level. A sample's attachments nest a few levels.
pub const MAX_NESTING: u32 = 32;

/// `depth` one level deeper, or malformed past [`MAX_NESTING`].
fn deeper(depth: u32) -> Result<u32, WireError> {
    let depth = depth + 1;
    if depth > MAX_NESTING {
        return Err(WireError::Malformed("sample buffer values nest too deep"));
    }
    Ok(depth)
}

/// Reads a value by its atom type, `depth` the level of the container it is
/// in, spending it from `budget`.
fn read_value(
    atom: &Atom<'_>,
    depth: u32,
    budget: &mut Budget,
) -> Result<SerializedValue, WireError> {
    match atom.kind {
        NULL => {
            budget.spend_values(1)?;
            Ok(SerializedValue::Null)
        }
        DICTIONARY => Ok(SerializedValue::Dictionary(read_dictionary(
            atom,
            deeper(depth)?,
            budget,
        )?)),
        ARRAY => Ok(SerializedValue::Array(read_array(
            atom,
            deeper(depth)?,
            budget,
        )?)),
        NUMBER | BOOLEAN | BYTES | DATE | STRING => Ok(SerializedValue::PropertyList(
            read_property_list(atom, budget)?,
        )),
        COLOR_SPACE => Ok(SerializedValue::ColorSpace(read_property_list(
            atom, budget,
        )?)),
        URL => match read_property_list(atom, budget)? {
            plist::Value::String(url) => Ok(SerializedValue::Url(url)),
            _ => Err(WireError::Malformed("URL is not a string")),
        },
        FORMAT_DESCRIPTION => Ok(SerializedValue::FormatDescription(Box::new(
            read_format_description(atom, deeper(depth)?, budget)?,
        ))),
        _ => Err(WireError::Malformed("unknown value atom")),
    }
}

/// Reads a `dict` at nesting `depth`: `keyv` children only, each a key
/// then a value. A repeated key keeps its first value, as
/// `CFDictionaryAddValue` does; the keys seen are kept in a set, so that a
/// dictionary of many keys reads in linear time. The dictionary and each
/// key and value are spent from `budget`.
fn read_dictionary(
    atom: &Atom<'_>,
    depth: u32,
    budget: &mut Budget,
) -> Result<Vec<(String, SerializedValue)>, WireError> {
    budget.spend_values(1)?;
    let mut entries: Vec<(String, SerializedValue)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for child in children(atom) {
        let child = child?;
        if child.kind != ENTRY {
            return Err(WireError::Malformed("dictionary child is not an entry"));
        }
        let mut parts = children(&child);
        let key_atom = parts
            .next()
            .transpose()?
            .ok_or(WireError::Malformed("entry without a key"))?;
        let key = match key_atom.kind {
            KEY_INDEX => {
                budget.spend_values(1)?;
                let index = key_atom
                    .payload()
                    .try_into()
                    .map(u16::from_le_bytes)
                    .map_err(|_| WireError::Malformed("key index length"))?;
                known_keys::key_at(index)
                    .ok_or(WireError::Malformed("key index"))?
                    .to_owned()
            }
            STRING => match read_property_list(&key_atom, budget)? {
                plist::Value::String(key) => key,
                _ => return Err(WireError::Malformed("key is not a string")),
            },
            _ => return Err(WireError::Malformed("entry key atom")),
        };
        let value_atom = parts
            .next()
            .transpose()?
            .ok_or(WireError::Malformed("entry without a value"))?;
        let value = read_value(&value_atom, depth, budget)?;
        if seen.insert(key.clone()) {
            entries.push((key, value));
        }
    }
    Ok(entries)
}

/// Reads an `aray` at nesting `depth`, skipping children with no payload.
/// The array and each value are spent from `budget`.
fn read_array(
    atom: &Atom<'_>,
    depth: u32,
    budget: &mut Budget,
) -> Result<Vec<SerializedValue>, WireError> {
    budget.spend_values(1)?;
    let mut values = Vec::new();
    for child in children(atom) {
        let child = child?;
        if !child.payload().is_empty() {
            values.push(read_value(&child, depth, budget)?);
        }
    }
    Ok(values)
}

/// Reads an `fdsc` at nesting `depth`: the last `mdia`, `vdim` and `codc`
/// win, unknown children are skipped, and only video-like media types make
/// a description. It and its extensions are spent from `budget`.
fn read_format_description(
    atom: &Atom<'_>,
    depth: u32,
    budget: &mut Budget,
) -> Result<SerializedFormatDescription, WireError> {
    budget.spend_values(1)?;
    let mut media_type = FourCc(0);
    let mut dimensions = (0, 0);
    let mut codec = FourCc(0);
    let mut extensions = None;
    for child in children(atom) {
        let child = child?;
        match child.kind {
            MEDIA_TYPE => media_type = FourCc(native_u32(&child)?),
            DIMENSIONS => {
                let payload = child.payload();
                dimensions = (
                    native_i32(payload).ok_or(WireError::Malformed("short atom"))?,
                    payload
                        .get(4..)
                        .and_then(native_i32)
                        .ok_or(WireError::Malformed("short atom"))?,
                );
            }
            CODEC => codec = FourCc(native_u32(&child)?),
            EXTENSIONS => {
                extensions = match only_child(&child, DICTIONARY)? {
                    Some(dictionary) => Some(read_dictionary(&dictionary, deeper(depth)?, budget)?),
                    None => None,
                };
            }
            _ => {}
        }
    }
    if ![
        MEDIA_TYPE_VIDEO,
        MEDIA_TYPE_AUXILIARY_PICTURE,
        MEDIA_TYPE_PICTURE,
    ]
    .contains(&media_type)
    {
        return Err(WireError::Malformed("format description media type"));
    }
    Ok(SerializedFormatDescription {
        media_type,
        codec,
        width: dimensions.0,
        height: dimensions.1,
        extensions,
    })
}

/// Reads a `crpt`: its recipe identifier is required.
fn read_cryptor(atom: &Atom<'_>) -> Result<CryptorRecipe, WireError> {
    let mut recipe_id = None;
    let mut version = 3;
    let mut kind = 0;
    for child in children(atom) {
        let child = child?;
        match child.kind {
            RECIPE_ID => {
                recipe_id = Some(
                    child
                        .payload()
                        .get(..8)
                        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
                        .map(u64::from_le_bytes)
                        .ok_or(WireError::Malformed("short atom"))?,
                );
            }
            RECIPE_VERSION => version = native_u32(&child)?,
            RECIPE_KIND => kind = native_u32(&child)?,
            _ => {}
        }
    }
    Ok(CryptorRecipe {
        recipe_id: recipe_id.ok_or(WireError::Malformed("cryptor without a recipe"))?,
        version,
        kind,
    })
}

impl SampleBuffer {
    /// Reads a serialization.
    ///
    /// A second `stia`, `ssiz` or `fdsc` is malformed, as is an `ssiz`
    /// whose length is not a multiple of 4, and atoms the reader does not
    /// know are skipped. The counts must make a sample buffer: one timing
    /// entry or one per sample, one size or one per sample, and as many
    /// sample attachment dictionaries as samples.
    ///
    /// Its values, its own dictionaries, arrays, keys and format
    /// descriptions and its property lists' values, are held between them
    /// to what one property list may decode to (`property_list::Budget`):
    /// each leaf is a property list of its own, and a serialization of
    /// many leaves, each within the bounds, would otherwise decode to far
    /// more than its bytes.
    pub fn parse(serialization: &[u8]) -> Result<Self, WireError> {
        let mut budget = Budget::new(serialization.len());
        let root = siblings(serialization)
            .next()
            .transpose()?
            .filter(|root| root.kind == SAMPLE_BUFFER)
            .ok_or(WireError::Malformed("not a sample buffer"))?;
        let mut buffer = Self::default();
        let mut have_timings = false;
        let mut have_sizes = false;
        for child in children(&root) {
            let child = child?;
            let payload = child.payload();
            match child.kind {
                TIMING => {
                    if have_timings {
                        return Err(WireError::Malformed("second timing atom"));
                    }
                    have_timings = true;
                    buffer.timings = payload
                        .as_chunks::<TIMING_SIZE>()
                        .0
                        .iter()
                        .filter_map(|timing| SampleTiming::from_native(timing))
                        .collect();
                }
                SIZES => {
                    if have_sizes {
                        return Err(WireError::Malformed("second sizes atom"));
                    }
                    if payload.len() % 4 != 0 {
                        return Err(WireError::Malformed("sizes length"));
                    }
                    have_sizes = true;
                    buffer.sizes = payload
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|size| u32::from_le_bytes(*size))
                        .collect();
                }
                DATA => {
                    buffer.data = Some(payload.to_vec());
                    if buffer.sample_count == 0 {
                        buffer.sample_count = 1;
                    }
                }
                SAMPLE_COUNT => buffer.sample_count = native_u32(&child)?,
                OUTPUT_PRESENTATION_TIME => {
                    buffer.output_presentation_time = Some(
                        MediaTime::from_native(payload)
                            .ok_or(WireError::Malformed("short atom"))?,
                    );
                }
                FORMAT_DESCRIPTION => {
                    if buffer.format_description.is_some() {
                        return Err(WireError::Malformed("second format description"));
                    }
                    buffer.format_description =
                        Some(read_format_description(&child, 1, &mut budget)?);
                }
                CRYPTOR => buffer.cryptor = Some(read_cryptor(&child)?),
                PROPAGATED_ATTACHMENTS => {
                    if let Some(dictionary) = only_child(&child, DICTIONARY)? {
                        buffer.propagated_attachments =
                            Some(read_dictionary(&dictionary, 1, &mut budget)?);
                    }
                }
                OTHER_ATTACHMENTS => {
                    if let Some(dictionary) = only_child(&child, DICTIONARY)? {
                        buffer.other_attachments =
                            Some(read_dictionary(&dictionary, 1, &mut budget)?);
                    }
                }
                SAMPLE_ATTACHMENTS => {
                    if let Some(array) = only_child(&child, ARRAY)? {
                        // Apple's reader makes no array when no element has
                        // a payload, and the sample buffer then gets no
                        // sample attachments. The guest writes a sample's
                        // empty dictionary as an empty `dict`:
                        // AVAssetReader's first AV1 sample, `[{}]`, arrives
                        // so.
                        let attachments = read_array(&array, 1, &mut budget)?;
                        buffer.sample_attachments =
                            Some(attachments).filter(|attachments| !attachments.is_empty());
                    }
                }
                _ => {}
            }
        }
        buffer.check_counts()?;
        Ok(buffer)
    }

    /// The counts `CMSampleBufferCreateReady` and the attachment copy
    /// accept.
    fn check_counts(&self) -> Result<(), WireError> {
        let samples = usize::try_from(self.sample_count).unwrap_or(usize::MAX);
        let fits = |entries: usize| entries == 0 || entries == 1 || entries == samples;
        if !fits(self.timings.len()) {
            return Err(WireError::Malformed("timing entries"));
        }
        if samples != 0 && !fits(self.sizes.len()) {
            return Err(WireError::Malformed("size entries"));
        }
        if let Some(attachments) = &self.sample_attachments
            && attachments.len() != samples
        {
            return Err(WireError::Malformed("sample attachment entries"));
        }
        Ok(())
    }

    /// Writes the serialization in the writer's order: the output time
    /// stamp when valid, the timings, the data with the count when nonzero
    /// and the sizes, the format description, the cryptor and the three
    /// attachment atoms. Empty dictionaries and arrays are left out, as the
    /// writer leaves them out.
    pub fn serialize(&self) -> Vec<u8> {
        let mut writer = AtomWriter::new();
        writer.begin(SAMPLE_BUFFER);
        if let Some(time) = self.output_presentation_time.filter(|time| time.is_valid()) {
            writer.atom(OUTPUT_PRESENTATION_TIME, &time.to_native());
        }
        if !self.timings.is_empty() {
            let timings: Vec<u8> = self.timings.iter().flat_map(|t| t.to_native()).collect();
            writer.atom(TIMING, &timings);
        }
        if let Some(data) = &self.data {
            writer.atom(DATA, data);
        }
        if self.sample_count != 0 {
            writer.atom(SAMPLE_COUNT, &self.sample_count.to_le_bytes());
        }
        if !self.sizes.is_empty() {
            let sizes: Vec<u8> = self
                .sizes
                .iter()
                .flat_map(|size| size.to_le_bytes())
                .collect();
            writer.atom(SIZES, &sizes);
        }
        if let Some(description) = &self.format_description {
            write_format_description(&mut writer, description);
        }
        if let Some(cryptor) = &self.cryptor {
            writer.begin(CRYPTOR);
            writer.atom(RECIPE_ID, &cryptor.recipe_id.to_le_bytes());
            writer.atom(RECIPE_VERSION, &cryptor.version.to_le_bytes());
            writer.atom(RECIPE_KIND, &cryptor.kind.to_le_bytes());
            writer.end();
        }
        for (kind, attachments) in [
            (PROPAGATED_ATTACHMENTS, &self.propagated_attachments),
            (OTHER_ATTACHMENTS, &self.other_attachments),
        ] {
            if let Some(attachments) = attachments {
                writer.begin(kind);
                write_dictionary(&mut writer, attachments, false);
                writer.end();
            }
        }
        if let Some(attachments) = &self.sample_attachments {
            writer.begin(SAMPLE_ATTACHMENTS);
            write_array(&mut writer, attachments);
            writer.end();
        }
        writer.finish()
    }
}

fn write_property_list(writer: &mut AtomWriter, kind: FourCc, value: &plist::Value) {
    // A leaf that does not encode is left out, as the writer's error would
    // end the serialization; every leaf this side writes encodes.
    if let Ok(bytes) = property_list::encode(value) {
        writer.atom(kind, &bytes);
    }
}

/// Writes a value by its type.
fn write_value(writer: &mut AtomWriter, value: &SerializedValue, keep_empty: bool) {
    match value {
        SerializedValue::Dictionary(entries) => write_dictionary(writer, entries, keep_empty),
        SerializedValue::Array(values) => write_array(writer, values),
        SerializedValue::PropertyList(leaf) => {
            let kind = match leaf {
                plist::Value::Boolean(_) => BOOLEAN,
                plist::Value::String(_) => STRING,
                plist::Value::Data(_) => BYTES,
                plist::Value::Date(_) => DATE,
                _ => NUMBER,
            };
            write_property_list(writer, kind, leaf);
        }
        SerializedValue::Null => {
            writer.atom(NULL, &[]);
        }
        SerializedValue::ColorSpace(plist) => write_property_list(writer, COLOR_SPACE, plist),
        SerializedValue::Url(url) => {
            write_property_list(writer, URL, &plist::Value::String(url.clone()));
        }
        SerializedValue::FormatDescription(description) => {
            write_format_description(writer, description);
        }
    }
}

/// Writes a `dict`, or nothing when it is empty and `keep_empty` is false.
/// Entries pass `keep_empty` on to their values.
fn write_dictionary(
    writer: &mut AtomWriter,
    entries: &[(String, SerializedValue)],
    keep_empty: bool,
) {
    if entries.is_empty() && !keep_empty {
        return;
    }
    writer.begin(DICTIONARY);
    for (key, value) in entries {
        writer.begin(ENTRY);
        match known_keys::index_of(key) {
            Some(index) => {
                writer.atom(KEY_INDEX, &index.to_le_bytes());
            }
            None => write_property_list(writer, STRING, &plist::Value::String(key.clone())),
        }
        write_value(writer, value, keep_empty);
        writer.end();
    }
    writer.end();
}

/// Writes an `aray`, or nothing when it is empty; its elements keep their
/// empty dictionaries.
fn write_array(writer: &mut AtomWriter, values: &[SerializedValue]) {
    if values.is_empty() {
        return;
    }
    writer.begin(ARRAY);
    for value in values {
        write_value(writer, value, true);
    }
    writer.end();
}

/// Writes an `fdsc`.
fn write_format_description(writer: &mut AtomWriter, description: &SerializedFormatDescription) {
    writer.begin(FORMAT_DESCRIPTION);
    writer.atom(MEDIA_TYPE, &description.media_type.0.to_le_bytes());
    let mut dimensions = [0; 8];
    dimensions[..4].copy_from_slice(&description.width.to_le_bytes());
    dimensions[4..].copy_from_slice(&description.height.to_le_bytes());
    writer.atom(DIMENSIONS, &dimensions);
    writer.atom(CODEC, &description.codec.0.to_le_bytes());
    if let Some(extensions) = &description.extensions {
        writer.begin(EXTENSIONS);
        write_dictionary(writer, extensions, false);
        writer.end();
    }
    writer.end();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::property_list::MAX_VALUES;
    use crate::wire::property_list::tests::{array, binary};

    fn time(value: i64) -> MediaTime {
        MediaTime {
            value,
            timescale: 30,
            flags: MediaTime::FLAG_VALID,
            epoch: 0,
        }
    }

    fn avc_format() -> SerializedFormatDescription {
        SerializedFormatDescription {
            media_type: MEDIA_TYPE_VIDEO,
            codec: FourCc::new(*b"avc1"),
            width: 64,
            height: 48,
            extensions: Some(vec![
                (
                    "SampleDescriptionExtensionAtoms".to_owned(),
                    SerializedValue::Dictionary(vec![(
                        "avcC".to_owned(),
                        SerializedValue::PropertyList(plist::Value::Data(vec![1, 2, 3])),
                    )]),
                ),
                (
                    "FormatName".to_owned(),
                    SerializedValue::PropertyList(plist::Value::String("H.264".to_owned())),
                ),
            ]),
        }
    }

    /// A one-sample buffer written atom by atom from the serializer's
    /// layout: `sbuf` { `stia` (72 native bytes), `sdat`, `nsmp` 1,
    /// `ssiz` [3] }.
    fn hand_written() -> Vec<u8> {
        let mut timing = Vec::new();
        for value in [1_i64, 2, 2] {
            timing.extend_from_slice(&value.to_le_bytes());
            timing.extend_from_slice(&30_i32.to_le_bytes());
            timing.extend_from_slice(&1_u32.to_le_bytes());
            timing.extend_from_slice(&0_i64.to_le_bytes());
        }
        let mut bytes = vec![0, 0, 0, 8 + 80 + 11 + 12 + 12];
        bytes.extend_from_slice(b"sbuf");
        bytes.extend_from_slice(&[0, 0, 0, 80]);
        bytes.extend_from_slice(b"stia");
        bytes.extend_from_slice(&timing);
        bytes.extend_from_slice(&[0, 0, 0, 11]);
        bytes.extend_from_slice(b"sdat");
        bytes.extend_from_slice(&[0xaa, 0xbb, 0xcc]);
        bytes.extend_from_slice(&[0, 0, 0, 12]);
        bytes.extend_from_slice(b"nsmp");
        bytes.extend_from_slice(&[1, 0, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 0, 12]);
        bytes.extend_from_slice(b"ssiz");
        bytes.extend_from_slice(&[3, 0, 0, 0]);
        bytes
    }

    fn one_sample() -> SampleBuffer {
        SampleBuffer {
            sample_count: 1,
            timings: vec![SampleTiming {
                duration: time(1),
                presentation: time(2),
                decode: time(2),
            }],
            sizes: vec![3],
            data: Some(vec![0xaa, 0xbb, 0xcc]),
            ..SampleBuffer::default()
        }
    }

    #[test]
    fn reads_a_hand_written_serialization() -> Result<(), WireError> {
        assert_eq!(SampleBuffer::parse(&hand_written())?, one_sample());
        Ok(())
    }

    #[test]
    fn writes_the_hand_written_serialization() {
        assert_eq!(one_sample().serialize(), hand_written());
    }

    #[test]
    fn data_without_a_count_is_one_sample() -> Result<(), WireError> {
        let buffer = SampleBuffer {
            sample_count: 0,
            ..one_sample()
        };
        assert_eq!(SampleBuffer::parse(&buffer.serialize())?.sample_count, 1);
        Ok(())
    }

    #[test]
    fn a_format_description_and_attachments_round_trip() -> Result<(), WireError> {
        let buffer = SampleBuffer {
            format_description: Some(avc_format()),
            propagated_attachments: Some(vec![(
                "CVImageBufferColorPrimaries".to_owned(),
                SerializedValue::PropertyList(plist::Value::String("ITU_R_709_2".to_owned())),
            )]),
            sample_attachments: Some(vec![SerializedValue::Dictionary(vec![(
                "NotSync".to_owned(),
                SerializedValue::PropertyList(plist::Value::Boolean(true)),
            )])]),
            output_presentation_time: Some(time(5)),
            ..one_sample()
        };
        let read = SampleBuffer::parse(&buffer.serialize())?;
        assert_eq!(read, buffer);
        let Some(format) = &read.format_description else {
            panic!("written above");
        };
        assert_eq!(format.sample_description_atom("avcC"), Some(&[1, 2, 3][..]));
        Ok(())
    }

    #[test]
    fn a_sample_whose_attachments_are_empty_has_none() -> Result<(), WireError> {
        // `[{}]` is written as an `aray` holding an empty `dict`, which
        // reads as no array, and not as no entries for the one sample.
        let buffer = SampleBuffer {
            sample_attachments: Some(vec![SerializedValue::Dictionary(Vec::new())]),
            ..one_sample()
        };
        let bytes = buffer.serialize();
        let empty_dictionary = [0, 0, 0, 8, b'd', b'i', b'c', b't'];
        assert!(
            bytes
                .windows(empty_dictionary.len())
                .any(|window| window == empty_dictionary)
        );
        assert_eq!(SampleBuffer::parse(&bytes)?, one_sample());
        Ok(())
    }

    #[test]
    fn known_keys_are_written_as_indexes() {
        let buffer = SampleBuffer {
            sample_attachments: Some(vec![SerializedValue::Dictionary(vec![(
                "NotSync".to_owned(),
                SerializedValue::Null,
            )])]),
            ..one_sample()
        };
        let bytes = buffer.serialize();
        let index = [0, 0, 0, 10, b'i', b'd', b'x', b'k', 0, 0];
        assert!(bytes.windows(index.len()).any(|window| window == index));
    }

    #[test]
    fn the_root_must_be_a_sample_buffer() {
        let mut bytes = hand_written();
        bytes[4..8].copy_from_slice(b"sbuX");
        assert_eq!(
            SampleBuffer::parse(&bytes),
            Err(WireError::Malformed("not a sample buffer"))
        );
    }

    #[test]
    fn a_second_timing_atom_is_malformed() {
        let mut bytes = hand_written();
        let timing = bytes[8..88].to_vec();
        bytes.extend_from_slice(&timing);
        let size = u32::try_from(bytes.len()).unwrap_or(0);
        bytes[0..4].copy_from_slice(&size.to_be_bytes());
        assert_eq!(
            SampleBuffer::parse(&bytes),
            Err(WireError::Malformed("second timing atom"))
        );
    }

    #[test]
    fn sizes_must_be_whole_u32s() {
        let mut bytes = hand_written();
        let at = bytes.len() - 12;
        bytes[at + 3] = 11;
        bytes.pop();
        let size = u32::try_from(bytes.len()).unwrap_or(0);
        bytes[0..4].copy_from_slice(&size.to_be_bytes());
        assert_eq!(
            SampleBuffer::parse(&bytes),
            Err(WireError::Malformed("sizes length"))
        );
    }

    #[test]
    fn unknown_atoms_are_skipped() -> Result<(), WireError> {
        let mut bytes = hand_written();
        bytes.extend_from_slice(&[0, 0, 0, 9, b'z', b'z', b'z', b'z', 7]);
        let size = u32::try_from(bytes.len()).unwrap_or(0);
        bytes[0..4].copy_from_slice(&size.to_be_bytes());
        assert_eq!(SampleBuffer::parse(&bytes)?, one_sample());
        Ok(())
    }

    #[test]
    fn a_format_description_that_is_not_video_is_malformed() {
        let buffer = SampleBuffer {
            format_description: Some(SerializedFormatDescription {
                media_type: FourCc::new(*b"soun"),
                ..avc_format()
            }),
            ..one_sample()
        };
        assert_eq!(
            SampleBuffer::parse(&buffer.serialize()),
            Err(WireError::Malformed("format description media type"))
        );
    }

    #[test]
    fn a_cryptor_needs_its_recipe() -> Result<(), WireError> {
        let buffer = SampleBuffer {
            cryptor: Some(CryptorRecipe {
                recipe_id: 9,
                version: 4,
                kind: 2,
            }),
            ..one_sample()
        };
        assert_eq!(SampleBuffer::parse(&buffer.serialize())?, buffer);
        Ok(())
    }

    /// One sample whose attachments are `levels` arrays, each inside the
    /// last, around a `null`.
    fn nested_attachments(levels: usize) -> Vec<u8> {
        let mut writer = AtomWriter::new();
        writer.begin(SAMPLE_BUFFER);
        writer.atom(DATA, &[1, 2, 3]);
        writer.begin(SAMPLE_ATTACHMENTS);
        for _ in 0..levels {
            writer.begin(ARRAY);
        }
        writer.atom(NULL, &[]);
        writer.finish()
    }

    #[test]
    fn values_nest_as_deep_as_the_limit_and_no_deeper() -> Result<(), WireError> {
        let deepest = usize::try_from(MAX_NESTING).unwrap_or(0);
        SampleBuffer::parse(&nested_attachments(deepest))?;
        assert_eq!(
            SampleBuffer::parse(&nested_attachments(deepest + 1)),
            Err(WireError::Malformed("sample buffer values nest too deep"))
        );
        // 8 bytes a level: deep enough to overflow a thread's stack before.
        assert_eq!(
            SampleBuffer::parse(&nested_attachments(300_000)),
            Err(WireError::Malformed("sample buffer values nest too deep"))
        );
        Ok(())
    }

    /// A sample buffer whose propagated attachments are `entries`, each
    /// under the first known key, with `value` its value atom.
    fn attachments_of(entries: usize, kind: FourCc, value: &[u8]) -> Vec<u8> {
        let mut writer = AtomWriter::new();
        writer.begin(SAMPLE_BUFFER);
        writer.atom(DATA, &[1, 2, 3]);
        writer.begin(PROPAGATED_ATTACHMENTS);
        writer.begin(DICTIONARY);
        for _ in 0..entries {
            writer.begin(ENTRY);
            writer.atom(KEY_INDEX, &0u16.to_le_bytes());
            writer.atom(kind, value);
            writer.end();
        }
        writer.finish()
    }

    #[test]
    fn its_values_and_its_leaves_share_one_bound() -> Result<(), WireError> {
        let refused = Err(WireError::Malformed("property lists too large"));
        // The dictionary, then a key and a value an entry.
        let entries = usize::try_from((MAX_VALUES - 1) / 2).unwrap_or(0);
        SampleBuffer::parse(&attachments_of(entries, NULL, &[0]))?;
        assert_eq!(
            SampleBuffer::parse(&attachments_of(entries + 1, NULL, &[0])),
            refused
        );
        // Leaves of 30,001 values each, from 30,000 references to one
        // integer: each alone is within the bound, three are not.
        let leaf = binary(&[array(&[1; 30_000]), vec![0x10, 7]]);
        SampleBuffer::parse(&attachments_of(2, NUMBER, &leaf))?;
        assert!(matches!(
            SampleBuffer::parse(&attachments_of(3, NUMBER, &leaf)),
            Err(WireError::Malformed(_))
        ));
        Ok(())
    }

    #[test]
    fn its_leaves_share_one_bound_of_bytes() -> Result<(), WireError> {
        // 1,024 references to one 4 KiB data: 4 MiB a leaf, so four leaves
        // decode to the bound beyond the serialization's length, and five
        // past it.
        let mut data = vec![0x4f, 0x11, 0x10, 0x00];
        data.extend_from_slice(&[7; 4096]);
        let leaf = binary(&[array(&[1; 1024]), data]);
        SampleBuffer::parse(&attachments_of(4, BYTES, &leaf))?;
        assert!(matches!(
            SampleBuffer::parse(&attachments_of(5, BYTES, &leaf)),
            Err(WireError::Malformed(_))
        ));
        Ok(())
    }

    #[test]
    fn a_repeated_key_keeps_its_first_value() -> Result<(), WireError> {
        let leaf = |n: i64| SerializedValue::PropertyList(plist::Value::Integer(n.into()));
        let buffer = SampleBuffer {
            propagated_attachments: Some(vec![
                ("ours".to_owned(), leaf(1)),
                ("ours".to_owned(), leaf(2)),
                ("theirs".to_owned(), leaf(3)),
            ]),
            ..one_sample()
        };
        let read = SampleBuffer::parse(&buffer.serialize())?;
        assert_eq!(
            read.propagated_attachments,
            Some(vec![
                ("ours".to_owned(), leaf(1)),
                ("theirs".to_owned(), leaf(3))
            ])
        );
        Ok(())
    }
}
