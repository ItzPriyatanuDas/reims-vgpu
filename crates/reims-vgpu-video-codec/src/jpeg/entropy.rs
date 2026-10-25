//! A scan's entropy-coded data, walked as a decoder reads it, short of
//! dequantizing and transforming: every Huffman code, every coefficient's
//! place, and the restart markers between intervals (T.81 F.2.2, F.1.2.3,
//! B.1.1.5). A hardware decoder that takes damaged data without saying so
//! is told by this walk instead, before it is handed the data.
//!
//! What it refuses is what Apple's JPEG hardware refused, measured on
//! images written from scratch (`tests.rs`). That includes how it reads
//! past the last MCU: it takes at most 32 bytes there, and decodes two
//! blocks further, as though a next MCU began. It takes the data only if
//! it ends within the last MCU's byte, or within those two blocks, between
//! two codes or inside one with only padding's bits left.

use super::Error;
use super::syntax::{HuffmanTable, Image};
use crate::num::ToUsize;

/// The most whole bytes Apple's hardware took after the last MCU's byte.
/// Of runs of 0 to 64 zero bytes there, it refused each of 33 or more but
/// one of 35, in a grey image, and below 33 what it took depended on how
/// the bytes decode (`tests.rs`).
const TRAILING_BYTES: usize = 32;

/// The bits a code is looked up by at once. Annex K's codes are at most 9
/// bits long for DC and 16 for AC, but nearly all AC codes in an image are
/// short.
const LOOKUP_BITS: u32 = 9;

/// What a walk read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanWalk {
    /// The MCUs decoded.
    pub mcus: u32,
    /// The restart markers read between them.
    pub restarts: u32,
}

/// Walks `image`'s scan from its first MCU to its last, and the data after
/// it. It fails with [`Error::ScanEndsEarly`] where the data ends, at the
/// marker after the scan, before the last MCU, and on:
/// - a code none of its tables has ([`Error::InvalidHuffmanCode`]);
/// - a coefficient past the block's 63rd
///   ([`Error::CoefficientsPast63rd`]);
/// - a restart marker missing ([`Error::ScanEndsEarly`]), out of turn
///   ([`Error::RstOutOfTurn`]), or inside an interval
///   ([`Error::RstInsideInterval`]);
/// - data before a restart marker beyond the interval's last byte
///   ([`Error::DataBeforeRst`]);
/// - data after the last MCU, unless it ends within the last MCU's byte,
///   or is at most 32 bytes and ends within the next MCU's first two
///   blocks decoded from it, between two codes or inside one with only
///   padding's bits left ([`Error::Over32BytesAfterLastMcu`],
///   [`Error::DataAfterLastMcu`], [`Error::DataAfterLastMcuEndsInsideCode`]
///   and [`Error::DataAfterFinalRst`]).
///
/// The DC coefficients' range is not checked, as Apple's hardware takes
/// one past 2047.
pub fn walk(image: &Image<'_>) -> Result<ScanWalk, Error> {
    let frame = &image.frame;
    // The tables and blocks of each scan component, in scan order.
    let mut components = Vec::with_capacity(image.scan.components.len());
    for scan_component in &image.scan.components {
        let component = frame
            .components
            .iter()
            .find(|c| c.id == scan_component.selector)
            .ok_or(Error::SosComponentNotInFrame)?;
        let table = |tables: &[Option<HuffmanTable<'_>>; 4], id: u8| {
            tables
                .get(usize::from(id))
                .and_then(Option::as_ref)
                .ok_or(Error::UndefinedHuffmanTable)
                .and_then(Decoder::new)
        };
        components.push(ScanComponent {
            dc: table(&image.dc_tables, scan_component.dc_table)?,
            ac: table(&image.ac_tables, scan_component.ac_table)?,
            blocks: component.blocks(),
        });
    }
    // One component alone is coded block by block over its own grid
    // (A.2.2), and its MCU is one block whatever its factors; interleaved
    // ones are coded MCU by MCU, Hi x Vi blocks each (A.2.3).
    let mcus = if let [only] = &mut components[..] {
        only.blocks = 1;
        let (width, height) = frame
            .component_size(0)
            .ok_or(Error::FrameWithoutComponents)?;
        // At most 8192 x 8192 blocks.
        width
            .div_ceil(8)
            .checked_mul(height.div_ceil(8))
            .ok_or(Error::Overflow("the component's blocks"))?
    } else {
        frame.mcu_count()
    };
    // The first two blocks of an MCU, which Apple's hardware reads past
    // the last one.
    let mut order = components
        .iter()
        .flat_map(|component| std::iter::repeat_n(component, component.blocks.to_usize()));
    let (Some(first), Some(second)) = (order.next(), order.next().or(components.first())) else {
        return Err(Error::ScanWithoutComponents);
    };
    let restart_interval = u32::from(image.restart_interval);
    let mut bits = Bits::new(image.entropy_coded_data);
    let mut restarts = 0u32;
    for mcu in 0..mcus {
        // An interval of 0 has no restarts.
        if mcu != 0 && mcu.checked_rem(restart_interval) == Some(0) {
            bits.restart(restarts)?;
            // One a restart interval: fewer than the MCUs.
            restarts = restarts
                .checked_add(1)
                .ok_or(Error::Overflow("the restart markers read"))?;
        }
        for component in &components {
            for _ in 0..component.blocks {
                block(&mut bits, component).map_err(|fault| bits.inside_the_scan(fault))?;
            }
        }
    }
    bits.after_the_last_mcu(first, second)?;
    Ok(ScanWalk { mcus, restarts })
}

/// Why a code could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Fault {
    /// The data is not a code, or not where one may be: the walk's error
    /// for it, [`Error::InvalidHuffmanCode`] or
    /// [`Error::CoefficientsPast63rd`].
    Bad(Error),
    /// The data stopped, at a marker or its end, before the code did.
    OutOfData(Stop),
}

/// Where in a code the data stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    /// Before its first bit.
    BetweenCodes,
    /// Inside the Huffman code, with the bits left one bit, or fewer than
    /// 8 and all ones, which Apple's hardware takes past the last MCU.
    InsideACodeAsPadding,
    /// Inside the Huffman code, with other bits left.
    InsideACode,
    /// After the code, inside its coefficient's bits.
    InsideItsBits,
}

impl Stop {
    /// Whether Apple's hardware takes data that stops so after the last
    /// MCU.
    fn taken_past_the_end(self) -> bool {
        matches!(self, Stop::BetweenCodes | Stop::InsideACodeAsPadding)
    }
}

/// A scan component's tables, and its blocks in an MCU.
struct ScanComponent {
    dc: Decoder,
    ac: Decoder,
    blocks: u32,
}

/// One block's codes: its DC difference, then its AC coefficients up to
/// an EOB or the 63rd (F.2.2.1, F.2.2.2).
fn block(bits: &mut Bits<'_>, component: &ScanComponent) -> Result<(), Fault> {
    let category = component.dc.decode(bits)?;
    bits.skip(u32::from(category))?;
    // k, the next coefficient, is below 64 at each code, so none of its
    // steps comes near 32 bits.
    let mut k = 1u32;
    while k < 64 {
        let symbol = component.ac.decode(bits)?;
        let (run, size) = (u32::from(symbol >> 4), u32::from(symbol & 0x0f));
        if size == 0 {
            if run != 15 {
                break; // EOB
            }
            // ZRL: sixteen zeros, the last of them at most the 63rd.
            k = k
                .checked_add(16)
                .filter(|&k| k <= 64)
                .ok_or(Fault::Bad(Error::CoefficientsPast63rd))?;
            continue;
        }
        // The run's zeros, then the coefficient, at most the 63rd.
        k = k
            .checked_add(run)
            .filter(|&k| k <= 63)
            .ok_or(Fault::Bad(Error::CoefficientsPast63rd))?;
        bits.skip(size)?;
        k = k
            .checked_add(1)
            .ok_or(Fault::Bad(Error::CoefficientsPast63rd))?;
    }
    Ok(())
}

/// A Huffman table made ready to decode with (C.2, F.2.2.3).
struct Decoder {
    /// By the next [`LOOKUP_BITS`] bits: the length and value of the code
    /// they start with, where it is that short.
    lookup: Box<[Option<(u8, u8)>; 1 << LOOKUP_BITS]>,
    /// The codes of each length, 1 to 16 (F.2.2.3's MINCODE, MAXCODE and
    /// VALPTR), where it has any.
    lengths: [Option<Codes>; 16],
    /// The longest code's length.
    longest: u32,
    values: Vec<u8>,
}

/// The codes of one length: from `min_code` to `max_code`, the first one's
/// value at `first_value` (F.2.2.3).
#[derive(Clone, Copy)]
struct Codes {
    min_code: u32,
    max_code: u32,
    first_value: usize,
}

impl Decoder {
    fn new(table: &HuffmanTable<'_>) -> Result<Decoder, Error> {
        let mut lookup = Box::new([None; 1 << LOOKUP_BITS]);
        let mut lengths = [None; 16];
        let mut longest = 0;
        // The codes are made as C.2 makes them, HUFFCODE, each length's
        // first value at VALPTR. They stay under 2^16, and the values under
        // 256, as the parser's check_codes holds them.
        let huffcode = || Error::Overflow("HUFFCODE");
        let mut code = 0u32;
        let mut first_value = 0usize;
        for ((length, &count), codes) in (1..=16u8).zip(&table.counts).zip(&mut lengths) {
            if count != 0 {
                let count_codes = u32::from(count);
                // The length's codes, the first to the one before `end`.
                let end = code.checked_add(count_codes).ok_or_else(huffcode)?;
                *codes = Some(Codes {
                    min_code: code,
                    max_code: end.checked_sub(1).ok_or_else(huffcode)?,
                    first_value,
                });
                if let Some(shift) = LOOKUP_BITS.checked_sub(u32::from(length)) {
                    let values = table.values.iter().skip(first_value);
                    for (code, &value) in (code..end).zip(values) {
                        // Every lookup entry the code's bits start.
                        let first = code.to_usize() << shift;
                        let last = first | !(usize::MAX << shift);
                        if let Some(entries) = lookup.get_mut(first..=last) {
                            entries.fill(Some((length, value)));
                        }
                    }
                }
                code = end;
                first_value = (first_value.checked_add(usize::from(count)))
                    .ok_or(Error::Overflow("VALPTR"))?;
                longest = u32::from(length);
            }
            code <<= 1;
        }
        Ok(Decoder {
            lookup,
            lengths,
            longest,
            values: table.values.to_vec(),
        })
    }

    /// The value of the code at the reader.
    fn decode(&self, bits: &mut Bits<'_>) -> Result<u8, Fault> {
        bits.fill();
        if bits.real() == 0 {
            return Err(Fault::OutOfData(Stop::BetweenCodes));
        }
        let entry = self.lookup.get(bits.peek(LOOKUP_BITS).to_usize());
        if let Some(&Some((length, value))) = entry {
            bits.consume_code(u32::from(length))?;
            return Ok(value);
        }
        let longer = (1..=16u32).zip(&self.lengths).skip(LOOKUP_BITS.to_usize());
        for (length, codes) in longer {
            let code = bits.peek(length);
            let Some(codes) = codes.filter(|codes| code <= codes.max_code) else {
                continue;
            };
            bits.consume_code(length)?;
            return (code.checked_sub(codes.min_code))
                .and_then(|index| codes.first_value.checked_add(index.to_usize()))
                .and_then(|at| self.values.get(at))
                .copied()
                .ok_or(Fault::Bad(Error::InvalidHuffmanCode));
        }
        // No code matched. Where the data stopped short of the longest
        // code, the ones read past it may be what failed to match.
        if bits.real() < self.longest {
            return Err(Fault::OutOfData(bits.inside_a_code()));
        }
        Err(Fault::Bad(Error::InvalidHuffmanCode))
    }
}

/// The scan's bits, most significant first, with stuffed bytes taken out
/// (F.1.2.3). Past the data, or at a marker, it reads ones, which it
/// counts so that reading them is [`Fault::OutOfData`].
struct Bits<'a> {
    /// The data not read yet.
    rest: &'a [u8],
    /// The bits read ahead, in the top `count` bits: at most 64.
    buffer: u64,
    count: u32,
    /// How many of the buffer's bits are past the data or a marker: the
    /// last `beyond` of the `count`, so never more.
    beyond: u32,
    /// Whether the reader stands at a restart marker: `rest` starts with
    /// its first 0xFF.
    at_marker: bool,
}

/// `data` after its leading fill bytes, 0xFF.
fn past_fill_bytes(mut data: &[u8]) -> &[u8] {
    while let Some((&0xff, after)) = data.split_first() {
        data = after;
    }
    data
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits {
            rest: data,
            buffer: 0,
            count: 0,
            beyond: 0,
            at_marker: false,
        }
    }

    /// Reads bytes until the buffer holds 57 bits or more.
    fn fill(&mut self) {
        // While a byte fits, at the bits after the `count` held.
        while let Some(shift) = 56u32.checked_sub(self.count) {
            let next = self.rest.split_first().filter(|_| !self.at_marker);
            let byte = match next {
                Some((&0xff, after)) => {
                    // Fill bytes, then a stuffed zero or a restart marker:
                    // the parser ends the data before any other marker.
                    match past_fill_bytes(after).split_first() {
                        Some((&0x00, past)) => self.rest = past,
                        _ => {
                            self.at_marker = true;
                            continue;
                        }
                    }
                    0xff
                }
                Some((&byte, after)) => {
                    self.rest = after;
                    byte
                }
                None => {
                    // `count` is at most 56 here, and `beyond` no more, so
                    // 8 more saturates neither.
                    self.beyond = self.beyond.saturating_add(8);
                    0xff
                }
            };
            self.buffer |= u64::from(byte) << shift;
            self.count = self.count.saturating_add(8);
        }
    }

    /// The bits read ahead that are the data's.
    fn real(&self) -> u32 {
        // `beyond` is never more than `count`, so this never saturates.
        self.count.saturating_sub(self.beyond)
    }

    /// The whole bytes of the data from the reader to the next marker or
    /// the end, its stuffed bytes counted once, and the read-ahead's;
    /// `None` past a `usize`.
    fn bytes_to_the_end(&self) -> Option<usize> {
        let mut bytes = 0usize;
        if !self.at_marker {
            let mut rest = self.rest;
            while let Some((&byte, after)) = rest.split_first() {
                if byte == 0xff {
                    match past_fill_bytes(after).split_first() {
                        Some((&0x00, past)) => rest = past,
                        _ => break,
                    }
                } else {
                    rest = after;
                }
                bytes = bytes.checked_add(1)?;
            }
        }
        bytes.checked_add((self.real() / 8).to_usize())
    }

    /// The next `count` bits, without reading them (`count` 1 to 16).
    fn peek(&self, count: u32) -> u32 {
        let [high, low, ..] = self.buffer.to_be_bytes();
        // 16 - count.
        u32::from(u16::from_be_bytes([high, low])) >> count.abs_diff(16)
    }

    /// Takes `count` bits that are the data's from the buffer, or nothing
    /// where it holds fewer.
    fn take(&mut self, count: u32) -> Option<()> {
        // The data's bits are those above `beyond`.
        self.count = self
            .count
            .checked_sub(count)
            .filter(|&left| left >= self.beyond)?;
        self.buffer <<= count;
        Some(())
    }

    /// Reads a Huffman code of `count` bits, which must be the data's.
    fn consume_code(&mut self, count: u32) -> Result<(), Fault> {
        self.take(count)
            .ok_or_else(|| Fault::OutOfData(self.inside_a_code()))
    }

    /// How a code the data stops inside ends: the bits left of it.
    fn inside_a_code(&self) -> Stop {
        let left = self.real();
        // All ones: the `left` low bits of a mask.
        if left == 1 || (left < 8 && self.peek(left) == !(u32::MAX << left)) {
            Stop::InsideACodeAsPadding
        } else {
            Stop::InsideACode
        }
    }

    /// Reads a coefficient's `size` bits (RECEIVE, F.2.2.1), whose value
    /// the walk has no use for.
    fn skip(&mut self, size: u32) -> Result<(), Fault> {
        if size == 0 {
            return Ok(());
        }
        self.fill();
        self.take(size).ok_or(Fault::OutOfData(Stop::InsideItsBits))
    }

    /// What a fault inside the scan's MCUs is: the data stopping at a
    /// restart marker that is not due, at the marker after the scan, or a
    /// bad code.
    fn inside_the_scan(&self, fault: Fault) -> Error {
        match fault {
            Fault::Bad(error) => error,
            Fault::OutOfData(_) if self.at_marker => Error::RstInsideInterval,
            Fault::OutOfData(_) => Error::ScanEndsEarly,
        }
    }

    /// The `index`th restart marker since the scan began, RSTn with n its
    /// index modulo 8 (B.2.1), after at most the interval's last byte.
    fn restart(&mut self, index: u32) -> Result<(), Error> {
        self.fill();
        if self.real() >= 8 {
            return Err(Error::DataBeforeRst);
        }
        if !self.at_marker {
            return Err(Error::ScanEndsEarly);
        }
        // 0xD0 + index % 8, in the low three bits.
        if u32::from(self.marker()) != 0xd0 | (index % 8) {
            return Err(Error::RstOutOfTurn);
        }
        self.past_marker();
        Ok(())
    }

    /// The restart marker the reader stands at.
    fn marker(&self) -> u8 {
        past_fill_bytes(self.rest).first().copied().unwrap_or(0xff)
    }

    /// Steps over the marker the reader stands at, and starts afresh.
    fn past_marker(&mut self) {
        self.rest = past_fill_bytes(self.rest).get(1..).unwrap_or_default();
        self.buffer = 0;
        self.count = 0;
        self.beyond = 0;
        self.at_marker = false;
    }

    /// The data after the last MCU, as Apple's hardware takes it: what is
    /// left of the last MCU's byte, or at most [`TRAILING_BYTES`] more that
    /// the next MCU's first two blocks are cut short in; then the end, or a
    /// restart marker with nothing after it. Neither block may hold a bad
    /// code, the second may not end whole, and where the data stops in
    /// one, it stops between two codes, or inside one with one bit, or
    /// fewer than 8 all ones, left ([`Stop::taken_past_the_end`]).
    fn after_the_last_mcu(
        &mut self,
        first: &ScanComponent,
        second: &ScanComponent,
    ) -> Result<(), Error> {
        self.fill();
        if self
            .bytes_to_the_end()
            .is_none_or(|bytes| bytes > TRAILING_BYTES)
        {
            return Err(Error::Over32BytesAfterLastMcu);
        }
        if self.real() >= 8 {
            let stopped = match block(self, first) {
                Ok(()) => match block(self, second) {
                    Ok(()) => return Err(Error::DataAfterLastMcu),
                    stopped => stopped,
                },
                stopped => stopped,
            };
            match stopped {
                Err(Fault::OutOfData(stop)) if stop.taken_past_the_end() => {}
                Err(Fault::OutOfData(_)) => return Err(Error::DataAfterLastMcuEndsInsideCode),
                Err(Fault::Bad(error)) => return Err(error),
                Ok(()) => {}
            }
        }
        if self.at_marker {
            self.past_marker();
            if !self.rest.is_empty() {
                return Err(Error::DataAfterFinalRst);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(super) mod tests;
