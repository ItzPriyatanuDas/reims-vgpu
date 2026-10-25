//! The markers and marker segments of one image, in the interchange format
//! (T.81 annex B), up to its scan's entropy-coded data.

use super::Error;
use super::tables::{
    DEFAULT_AC_CHROMINANCE, DEFAULT_AC_LUMINANCE, DEFAULT_DC_CHROMINANCE, DEFAULT_DC_LUMINANCE,
};

/// The coding process a frame header names, of those taken (T.81 Table
/// B.1). The others are refused while parsing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Process {
    /// SOF0: baseline sequential DCT, Huffman coding, 8-bit samples.
    Baseline,
    /// SOF1: extended sequential DCT, Huffman coding; 8-bit samples here.
    ExtendedSequential,
}

/// One component of a frame (B.2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Component {
    /// Ci, which scans name it by.
    pub id: u8,
    /// Hi, 1 to 4.
    pub horizontal: u8,
    /// Vi, 1 to 4.
    pub vertical: u8,
    /// Tqi, 0 to 3.
    pub quantization_table: u8,
}

impl Component {
    /// Its blocks in an interleaved scan's MCU, Hi x Vi (A.2.3): 1 to 16,
    /// so the product never saturates.
    pub(crate) fn blocks(self) -> u32 {
        u32::from(self.horizontal).saturating_mul(u32::from(self.vertical))
    }
}

/// How an image's chroma is sampled, read from its components' factors.
/// The names are ours.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Subsampling {
    /// Three components, the first 2x2, the others 1x1.
    Yuv420,
    /// Three components, the first 2x1, the others 1x1.
    Yuv422,
    /// Three components, the first 1x2, the others 1x1.
    Yuv440,
    /// Three components, each 1x1.
    Yuv444,
    /// Three components, the first 4x1, the others 1x1.
    Yuv411,
    /// One component.
    Grey,
}

/// A frame header (B.2.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    /// The process its marker names.
    pub(crate) process: Process,
    /// P, the sample precision: 8.
    pub(crate) precision: u8,
    /// Y, the number of lines.
    pub height: u16,
    /// X, the samples per line.
    pub width: u16,
    /// The components, in the header's order.
    pub components: Vec<Component>,
}

impl FrameHeader {
    /// Hmax and Vmax: 1 at least, as for a frame of no components.
    pub(crate) fn max_sampling(&self) -> (u8, u8) {
        self.components
            .iter()
            .fold((1, 1), |(h, v), c| (h.max(c.horizontal), v.max(c.vertical)))
    }

    /// How the chroma is sampled, where it is one of [`Subsampling`]'s.
    pub fn subsampling(&self) -> Option<Subsampling> {
        match self.components[..] {
            [_] => Some(Subsampling::Grey),
            [luma, cb, cr] => {
                let factors = |c: Component| (c.horizontal, c.vertical);
                if factors(cb) != (1, 1) || factors(cr) != (1, 1) {
                    return None;
                }
                match factors(luma) {
                    (2, 2) => Some(Subsampling::Yuv420),
                    (2, 1) => Some(Subsampling::Yuv422),
                    (1, 2) => Some(Subsampling::Yuv440),
                    (1, 1) => Some(Subsampling::Yuv444),
                    (4, 1) => Some(Subsampling::Yuv411),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// The MCU's size in samples: 8Hmax by 8Vmax for an interleaved scan,
    /// one 8x8 block for one component (A.2).
    pub fn mcu_size(&self) -> (u32, u32) {
        if self.components.len() == 1 {
            return (8, 8);
        }
        let (h, v) = self.max_sampling();
        (u32::from(h) << 3, u32::from(v) << 3)
    }

    /// The MCUs across and down the image.
    pub(crate) fn mcus(&self) -> (u32, u32) {
        let (width, height) = self.mcu_size();
        (
            u32::from(self.width).div_ceil(width),
            u32::from(self.height).div_ceil(height),
        )
    }

    /// The MCUs in a scan of every component: at most 8192 by 8192, for
    /// an image 65535 samples square.
    pub fn mcu_count(&self) -> u32 {
        let (across, down) = self.mcus();
        across.saturating_mul(down)
    }

    /// The size of component `index`: X·Hi/Hmax by Y·Vi/Vmax, rounded up
    /// (A.1.1).
    pub(crate) fn component_size(&self, index: usize) -> Option<(u32, u32)> {
        let component = self.components.get(index)?;
        let (h, v) = self.max_sampling();
        // A factor is at most 4, so the product never saturates, and the
        // largest at least 1.
        let scale = |size: u16, factor: u8, max: u8| {
            u32::from(size)
                .saturating_mul(u32::from(factor))
                .div_ceil(u32::from(max))
        };
        Some((
            scale(self.width, component.horizontal, h),
            scale(self.height, component.vertical, v),
        ))
    }
}

/// A quantization table (B.2.4.1), its values in zig-zag order as the
/// stream gives them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuantizationTable {
    /// Whether the stream wrote it in 16-bit values (Pq 1).
    pub(crate) sixteen_bit: bool,
    /// Q0 to Q63, none 0.
    pub values: [u16; 64],
}

/// A Huffman table (B.2.4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HuffmanTable<'a> {
    /// L1 to L16: the number of codes of each length.
    pub counts: [u8; 16],
    /// The values, in order of their codes: as many as the counts' sum.
    pub values: &'a [u8],
}

/// One component of a scan (B.2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanComponent {
    /// Csj: the frame component's Ci.
    pub selector: u8,
    /// Tdj, the DC table.
    pub dc_table: u8,
    /// Taj, the AC table.
    pub ac_table: u8,
}

/// A scan header (B.2.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanHeader {
    /// The components, in the frame's order.
    pub components: Vec<ScanComponent>,
    /// Ss: 0 for a sequential scan.
    pub(crate) spectral_start: u8,
    /// Se: 63 for a sequential scan.
    pub(crate) spectral_end: u8,
    /// Ah: 0 for a sequential scan.
    pub(crate) approximation_high: u8,
    /// Al: 0 for a sequential scan.
    pub(crate) approximation_low: u8,
}

/// One image in one scan: its frame and scan headers, the tables in force
/// when its scan began, and its entropy-coded data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image<'a> {
    /// The frame header.
    pub frame: FrameHeader,
    /// The quantization tables, by Tq.
    pub quantization_tables: [Option<QuantizationTable>; 4],
    /// The DC Huffman tables, by Th.
    pub dc_tables: [Option<HuffmanTable<'a>>; 4],
    /// The AC Huffman tables, by Th.
    pub ac_tables: [Option<HuffmanTable<'a>>; 4],
    /// Whether the image defined no Huffman table, so that the scan takes
    /// Annex K's, as tables 0 and 1 (Motion JPEG leaves them out).
    pub default_huffman_tables: bool,
    /// Ri, the MCUs between restart markers; 0 for none.
    pub restart_interval: u16,
    /// The scan header.
    pub scan: ScanHeader,
    /// The scan's entropy-coded segments, from the first byte after its
    /// header up to the marker after it, restart markers and stuffed
    /// bytes included, as a decoder reads them.
    pub entropy_coded_data: &'a [u8],
    /// The transform an Adobe APP14 segment names, where there is one: 0
    /// for none (RGB or CMYK components), 1 for YCbCr, 2 for YCCK.
    pub adobe_transform: Option<u8>,
}

/// The tables and settings in force at a point of the stream.
#[derive(Default)]
struct Tables<'a> {
    quantization: [Option<QuantizationTable>; 4],
    dc: [Option<HuffmanTable<'a>>; 4],
    ac: [Option<HuffmanTable<'a>>; 4],
    defined_huffman: bool,
    restart_interval: u16,
}

/// Reads one image, from its SOI to its EOI.
pub fn parse(data: &[u8]) -> Result<Image<'_>, Error> {
    if data.get(..2) != Some(&[0xff, 0xd8][..]) {
        return Err(Error::NoSoi);
    }
    let mut position = 2;
    let mut tables = Tables::default();
    let mut frame: Option<FrameHeader> = None;
    let mut adobe_transform = None;
    let mut image: Option<Image<'_>> = None;
    loop {
        let (marker, after) = next_marker(data, position)?;
        position = after;
        match marker {
            0xd9 => return image.ok_or(Error::EoiBeforeScan),
            0xd8 => return Err(Error::SecondSoi),
            0xd0..=0xd7 => return Err(Error::RstOutsideScan),
            // TEM has no segment.
            0x01 => continue,
            _ => {}
        }
        let (segment, after) = segment(data, position)?;
        position = after;
        if image.is_some() {
            // Past the one scan, only another scan or a DNL matters: the
            // tables it would take are already in the image.
            match marker {
                0xda => return Err(Error::Unsupported("an image in more than one scan")),
                0xdc => return Err(Error::Unsupported("a height set after the scan (DNL)")),
                _ => continue,
            }
        }
        match marker {
            0xc0 | 0xc1 => {
                if frame.is_some() {
                    return Err(Error::SecondSof);
                }
                let process = if marker == 0xc0 {
                    Process::Baseline
                } else {
                    Process::ExtendedSequential
                };
                frame = Some(frame_header(process, segment)?);
            }
            0xc2 => return Err(Error::Unsupported("progressive coding")),
            0xc3 => return Err(Error::Unsupported("lossless coding")),
            0xc5..=0xc7 | 0xde | 0xdf => return Err(Error::Unsupported("hierarchical coding")),
            0xc8 => return Err(Error::Unsupported("a JPEG extension's frame (JPG)")),
            0xc9..=0xcf => return Err(Error::Unsupported("arithmetic coding")),
            0xf7 | 0xf8 => return Err(Error::Unsupported("JPEG-LS")),
            0xc4 => huffman_tables(segment, &mut tables)?,
            0xdb => quantization_tables(segment, &mut tables)?,
            0xdd => {
                let [high, low] = segment[..] else {
                    return Err(Error::DriLength);
                };
                tables.restart_interval = u16::from_be_bytes([high, low]);
            }
            0xdc => return Err(Error::DnlBeforeScan),
            0xda => {
                let frame = frame.as_ref().ok_or(Error::SosBeforeSof)?;
                let scan = scan_header(frame, segment, &mut tables)?;
                let (entropy_coded_data, after) = entropy_coded_data(data, position)?;
                position = after;
                image = Some(Image {
                    frame: frame.clone(),
                    quantization_tables: tables.quantization,
                    dc_tables: tables.dc,
                    ac_tables: tables.ac,
                    default_huffman_tables: !tables.defined_huffman,
                    restart_interval: tables.restart_interval,
                    scan,
                    entropy_coded_data,
                    adobe_transform,
                });
            }
            // "Adobe", version, flags0, flags1, transform.
            0xee if segment.starts_with(b"Adobe") => {
                if let Some(&transform) = segment.get(11) {
                    adobe_transform = Some(transform);
                }
            }
            // APPn, COM and the reserved markers: their segments are
            // skipped.
            _ => {}
        }
    }
}

/// The marker at `position`, after any fill bytes, and where its segment
/// starts.
fn next_marker(data: &[u8], position: usize) -> Result<(u8, usize), Error> {
    let rest = data.get(position..).ok_or(Error::Truncated)?;
    let (&first, rest) = rest.split_first().ok_or(Error::Truncated)?;
    if first != 0xff {
        return Err(Error::NotAMarker);
    }
    // B.1.1.2: any marker may be preceded by fill bytes of 0xFF.
    let fill = rest.iter().take_while(|&&byte| byte == 0xff).count();
    match rest.get(fill) {
        None => Err(Error::Truncated),
        Some(0x00) => Err(Error::StuffedZeroAsMarker),
        // The marker's byte is in `data`, so the position after it is too.
        Some(&marker) => position
            .checked_add(fill)
            .and_then(|position| position.checked_add(2))
            .map(|after| (marker, after))
            .ok_or(Error::Truncated),
    }
}

/// The segment at `position`, after its two length bytes, and where the
/// next marker is (B.1.1.4).
fn segment(data: &[u8], position: usize) -> Result<(&[u8], usize), Error> {
    let rest = data.get(position..).ok_or(Error::Truncated)?;
    let (length, rest) = rest.split_first_chunk::<2>().ok_or(Error::Truncated)?;
    let length = usize::from(u16::from_be_bytes(*length));
    let size = length.checked_sub(2).ok_or(Error::SegmentLength)?;
    let segment = rest.get(..size).ok_or(Error::Truncated)?;
    // The segment is in `data`, so the position after it is too.
    let after = position.checked_add(length).ok_or(Error::Truncated)?;
    Ok((segment, after))
}

/// A frame header's segment (B.2.2).
fn frame_header(process: Process, segment: &[u8]) -> Result<FrameHeader, Error> {
    let [precision, y_high, y_low, x_high, x_low, count, rest @ ..] = segment else {
        return Err(Error::SofTooShort);
    };
    match (process, precision) {
        (_, 8) => {}
        (Process::ExtendedSequential, 12) => return Err(Error::Unsupported("12-bit samples")),
        _ => {
            return Err(Error::SofPrecision);
        }
    }
    let height = u16::from_be_bytes([*y_high, *y_low]);
    let width = u16::from_be_bytes([*x_high, *x_low]);
    if height == 0 {
        return Err(Error::Unsupported("a height set after the scan (DNL)"));
    }
    if width == 0 {
        return Err(Error::SofZeroWidth);
    }
    if *count == 0 || rest.len() != 3 * usize::from(*count) {
        return Err(Error::SofLength);
    }
    let mut components: Vec<Component> = Vec::with_capacity(usize::from(*count));
    for &[id, factors, quantization_table] in rest.as_chunks::<3>().0 {
        let component = Component {
            id,
            horizontal: factors >> 4,
            vertical: factors & 0xf,
            quantization_table,
        };
        if !(1..=4).contains(&component.horizontal) || !(1..=4).contains(&component.vertical) {
            return Err(Error::SofSamplingFactor);
        }
        if component.quantization_table > 3 {
            return Err(Error::SofQuantizationTable);
        }
        if components.iter().any(|c| c.id == component.id) {
            return Err(Error::SofDuplicateComponent);
        }
        components.push(component);
    }
    Ok(FrameHeader {
        process,
        precision: *precision,
        height,
        width,
        components,
    })
}

/// A DQT segment's tables (B.2.4.1).
fn quantization_tables(mut segment: &[u8], tables: &mut Tables<'_>) -> Result<(), Error> {
    while let [spec, rest @ ..] = segment {
        let (precision, index) = (spec >> 4, usize::from(spec & 0xf));
        if precision > 1 || index > 3 {
            return Err(Error::DqtPrecisionOrDestination);
        }
        let size = if precision == 1 { 128 } else { 64 };
        let (bytes, after) = rest.split_at_checked(size).ok_or(Error::DqtPastSegment)?;
        let mut values = [0u16; 64];
        if precision == 1 {
            for (value, pair) in values.iter_mut().zip(bytes.as_chunks::<2>().0) {
                *value = u16::from_be_bytes(*pair);
            }
        } else {
            for (value, &byte) in values.iter_mut().zip(bytes) {
                *value = u16::from(byte);
            }
        }
        if values.contains(&0) {
            return Err(Error::DqtZeroValue);
        }
        *tables
            .quantization
            .get_mut(index)
            .ok_or(Error::DqtPrecisionOrDestination)? = Some(QuantizationTable {
            sixteen_bit: precision == 1,
            values,
        });
        segment = after;
    }
    Ok(())
}

/// A DHT segment's tables (B.2.4.2).
fn huffman_tables<'a>(mut segment: &'a [u8], tables: &mut Tables<'a>) -> Result<(), Error> {
    while let [spec, rest @ ..] = segment {
        let (class, index) = (spec >> 4, usize::from(spec & 0xf));
        if class > 1 || index > 3 {
            return Err(Error::DhtClassOrDestination);
        }
        let (&counts, rest) = rest
            .split_first_chunk::<16>()
            .ok_or(Error::DhtPastSegment)?;
        // At most 16 x 255 values.
        let total = counts.iter().map(|&count| usize::from(count)).sum();
        let (values, after) = rest.split_at_checked(total).ok_or(Error::DhtPastSegment)?;
        let table = HuffmanTable { counts, values };
        check_codes(&table)?;
        let class = if class == 0 {
            &mut tables.dc
        } else {
            &mut tables.ac
        };
        *class.get_mut(index).ok_or(Error::DhtClassOrDestination)? = Some(table);
        tables.defined_huffman = true;
        segment = after;
    }
    Ok(())
}

/// Checks that a table's codes fit their lengths, as C.2 makes them, with
/// the code of all ones left unused, and that its values are values of its
/// class.
fn check_codes(table: &HuffmanTable<'_>) -> Result<(), Error> {
    if table.values.len() > 256 {
        return Err(Error::DhtOver256Codes);
    }
    // The next code of each length: below 2^16, as each is checked to be.
    let mut code: u32 = 0;
    for (length, &count) in (1..=16u32).zip(&table.counts) {
        code = code
            .checked_add(u32::from(count))
            .filter(|&code| code < 1 << length)
            .ok_or(Error::DhtTooManyCodes)?;
        code <<= 1;
    }
    Ok(())
}

/// A scan header's segment (B.2.3), checked against the frame and the
/// tables in force, which it may complete with Annex K's.
fn scan_header(
    frame: &FrameHeader,
    segment: &[u8],
    tables: &mut Tables<'_>,
) -> Result<ScanHeader, Error> {
    let [count, rest @ ..] = segment else {
        return Err(Error::SosTooShort);
    };
    let count = usize::from(*count);
    let (specs, &[spectral_start, spectral_end, approximation]) =
        rest.split_last_chunk::<3>().ok_or(Error::SosLength)?;
    let (specs, []) = specs.as_chunks::<2>() else {
        return Err(Error::SosLength);
    };
    if !(1..=4).contains(&count) || specs.len() != count {
        return Err(Error::SosLength);
    }
    let mut components: Vec<ScanComponent> = Vec::with_capacity(count);
    // B.2.3: in the frame header's order, each once. Each search goes on
    // from the component after the one the last found.
    let mut in_frame = frame.components.iter();
    for &[selector, tables_spec] in specs {
        let component = ScanComponent {
            selector,
            dc_table: tables_spec >> 4,
            ac_table: tables_spec & 0xf,
        };
        if !in_frame.any(|c| c.id == component.selector) {
            return Err(Error::SosComponentNotInFrame);
        }
        let limit = if frame.process == Process::Baseline {
            1
        } else {
            3
        };
        if component.dc_table > limit || component.ac_table > limit {
            return Err(Error::SosTableSelector);
        }
        components.push(component);
    }
    let scan = ScanHeader {
        components,
        spectral_start,
        spectral_end,
        approximation_high: approximation >> 4,
        approximation_low: approximation & 0xf,
    };
    if (
        scan.spectral_start,
        scan.spectral_end,
        scan.approximation_high,
        scan.approximation_low,
    ) != (0, 63, 0, 0)
    {
        return Err(Error::SosNotSequential);
    }
    if scan.components.len() != frame.components.len() {
        return Err(Error::Unsupported("an image in more than one scan"));
    }
    if scan.components.len() > 1 {
        let blocks: u32 = frame.components.iter().map(|c| c.blocks()).sum();
        if blocks > 10 {
            return Err(Error::McuOver10Blocks);
        }
    }
    for component in &frame.components {
        let table = tables
            .quantization
            .get(usize::from(component.quantization_table));
        if table.is_none_or(Option::is_none) {
            return Err(Error::UndefinedQuantizationTable);
        }
    }
    if !tables.defined_huffman {
        tables.dc[0] = Some(DEFAULT_DC_LUMINANCE);
        tables.dc[1] = Some(DEFAULT_DC_CHROMINANCE);
        tables.ac[0] = Some(DEFAULT_AC_LUMINANCE);
        tables.ac[1] = Some(DEFAULT_AC_CHROMINANCE);
    }
    for component in &scan.components {
        let (dc, ac) = (
            tables.dc.get(usize::from(component.dc_table)),
            tables.ac.get(usize::from(component.ac_table)),
        );
        let (Some(Some(dc)), Some(Some(ac))) = (dc, ac) else {
            return Err(Error::UndefinedHuffmanTable);
        };
        if dc.values.is_empty() || ac.values.is_empty() {
            return Err(Error::EmptyHuffmanTable);
        }
        // F.1.2.1: DC differences of 8-bit samples are of categories 0 to
        // 11.
        if dc.values.iter().any(|&category| category > 11) {
            return Err(Error::DcCategoryOver11);
        }
        // F.1.2.2: an AC value is a run and a size of 1 to 10, or EOB or
        // ZRL. A table of more codes than the 12 categories, or the 162 AC
        // values, repeats one, and is more than VA-API's baseline tables
        // hold (ours: the counts follow from T.81's values).
        let ac_value = |&value: &u8| match value & 0x0f {
            0 => value == 0x00 || value == 0xf0,
            size => size <= 10,
        };
        if dc.values.len() > 12 {
            return Err(Error::DcTableOver12Codes);
        }
        if ac.values.len() > 162 || !ac.values.iter().all(ac_value) {
            return Err(Error::AcTableValues);
        }
    }
    Ok(scan)
}

/// The entropy-coded data from `start` up to the first marker that is not
/// a restart marker, and where that marker starts (B.1.1.5, F.1.2.3).
fn entropy_coded_data(data: &[u8], start: usize) -> Result<(&[u8], usize), Error> {
    let scan = data.get(start..).ok_or(Error::Truncated)?;
    // Each position below is within `scan`, and `scan` within `data`, so
    // none of the sums passes a usize: one that did would be past the data.
    let add = |a: usize, b: usize| a.checked_add(b).ok_or(Error::Truncated);
    let mut at = 0usize;
    loop {
        let rest = scan.get(at..).ok_or(Error::Truncated)?;
        let marker = rest
            .iter()
            .position(|&b| b == 0xff)
            .ok_or(Error::Truncated)?;
        let fill = rest.iter().skip(marker).take_while(|&&b| b == 0xff).count();
        let after = add(marker, fill)?;
        match rest.get(after) {
            None => return Err(Error::Truncated),
            // A stuffed zero, or a restart marker: still the scan.
            Some(0x00 | 0xd0..=0xd7) => at = add(add(at, after)?, 1)?,
            Some(_) => {
                let length = add(at, marker)?;
                let (data, _) = scan.split_at_checked(length).ok_or(Error::Truncated)?;
                return Ok((data, add(start, length)?));
            }
        }
    }
}
