//! JPEG's interchange format, as far as a baseline hardware decoder takes
//! it: the markers and segments of one image up to its scan's
//! entropy-coded data, the tables they define and the frame and scan
//! headers (ITU-T T.81, 09/92, by annex and clause).
//!
//! [`parse`] reads a whole image, from its SOI to its EOI, and refuses
//! cleanly what a baseline decoder does not take: progressive, lossless,
//! hierarchical and arithmetic coding, 12-bit samples, and an image in
//! more than one scan. [`walk`] then reads its scan's entropy-coded data
//! as a decoder would, short of the samples, and refuses it where Apple's
//! JPEG hardware does. What decodes the samples is a backend's.

mod entropy;
mod syntax;
mod tables;
#[cfg(test)]
mod tests;

pub use entropy::{ScanWalk, walk};
pub use syntax::{
    Component, FrameHeader, HuffmanTable, Image, Process, QuantizationTable, ScanComponent,
    ScanHeader, Subsampling, parse,
};
pub use tables::{
    DEFAULT_AC_CHROMINANCE, DEFAULT_AC_LUMINANCE, DEFAULT_DC_CHROMINANCE, DEFAULT_DC_LUMINANCE,
};

/// Why an image could not be read or decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The data ended inside a segment, or before the EOI.
    Truncated,
    /// The data does not start with an SOI marker (B.2.1).
    NoSoi,
    /// An EOI marker before any scan: an image of no scan (B.2.1).
    EoiBeforeScan,
    /// A second SOI marker before the EOI (B.2.1).
    SecondSoi,
    /// An RSTm marker outside a scan's entropy-coded data.
    RstOutsideScan,
    /// A byte other than 0xFF where a marker should start (B.1.1.2).
    NotAMarker,
    /// A stuffed zero, 0xFF then 0x00, where a marker should be: the
    /// stuffing of entropy-coded data, outside it (B.1.1.5).
    StuffedZeroAsMarker,
    /// A marker segment's length field below 2, the length field's own
    /// bytes (B.1.1.4).
    SegmentLength,
    /// A second frame header (B.2.1).
    SecondSof,
    /// A frame header shorter than its fixed fields, up to Nf (B.2.2).
    SofTooShort,
    /// A frame header's sample precision, P, that its process does not
    /// have: other than 8 for baseline, 8 or 12 for extended sequential
    /// (B.2.2).
    SofPrecision,
    /// A frame header's number of samples per line, X, of 0 (B.2.2).
    SofZeroWidth,
    /// A frame header's number of components, Nf, of 0, or a length, Lf,
    /// other than its Nf components' (B.2.2).
    SofLength,
    /// A frame component's sampling factor, Hi or Vi, outside 1 to 4
    /// (B.2.2).
    SofSamplingFactor,
    /// A frame component's quantization table selector, Tqi, outside 0 to
    /// 3 (B.2.2).
    SofQuantizationTable,
    /// Two frame components of one identifier, Ci (B.2.2).
    SofDuplicateComponent,
    /// A quantization table's precision, Pq, past 1, or its destination,
    /// Tq, past 3 (B.2.4.1).
    DqtPrecisionOrDestination,
    /// A quantization table that runs past its DQT segment (B.2.4.1).
    DqtPastSegment,
    /// A quantization table element, Qk, of 0 (B.2.4.1).
    DqtZeroValue,
    /// A Huffman table's class, Tc, past 1, or its destination, Th, past 3
    /// (B.2.4.2).
    DhtClassOrDestination,
    /// A Huffman table whose counts or values run past its DHT segment
    /// (B.2.4.2).
    DhtPastSegment,
    /// A Huffman table of over 256 values (B.2.4.2).
    DhtOver256Codes,
    /// A Huffman table whose codes do not fit their lengths, as C.2 makes
    /// them, with the code of all ones left unused (C.2).
    DhtTooManyCodes,
    /// A DRI segment's length, Lr, other than 4 (B.2.4.4).
    DriLength,
    /// A DNL segment before the scan (B.2.5).
    DnlBeforeScan,
    /// A scan header before the frame header (B.2.1).
    SosBeforeSof,
    /// A scan header shorter than its number of components, Ns (B.2.3).
    SosTooShort,
    /// A scan header's Ns outside 1 to 4, or a length, Ls, other than its
    /// Ns components' (B.2.3).
    SosLength,
    /// A scan component selector, Csj, naming no frame component after
    /// the one before it: not in the frame, or out of the frame's order
    /// (B.2.3).
    SosComponentNotInFrame,
    /// A scan component's DC or AC table selector, Tdj or Taj, past what
    /// its process has: 1 for baseline, 3 for extended sequential (B.2.3).
    SosTableSelector,
    /// A sequential scan's Ss, Se, Ah or Al other than 0, 63, 0 and 0
    /// (B.2.3).
    SosNotSequential,
    /// An interleaved scan whose MCU is of over 10 blocks, the sum of its
    /// components' Hi x Vi.
    McuOver10Blocks,
    /// A frame component's quantization table, by its Tqi, not defined
    /// before the scan.
    UndefinedQuantizationTable,
    /// A scan component's DC or AC table, by its Tdj or Taj, not defined
    /// before the scan, nor Annex K's in an image that defines none.
    UndefinedHuffmanTable,
    /// A scan component's DC or AC table of no codes.
    EmptyHuffmanTable,
    /// A DC table with a category past 11, which no 8-bit sample's
    /// difference has (F.1.2.1).
    DcCategoryOver11,
    /// A DC table of over 12 codes, which repeats one of the 12 categories
    /// (F.1.2.1).
    DcTableOver12Codes,
    /// An AC table with a value that is neither EOB, ZRL nor a run and a
    /// size of 1 to 10, or of over 162 values, which repeats one (F.1.2.2).
    AcTableValues,
    /// A frame of no components, given to [`walk`].
    FrameWithoutComponents,
    /// A scan of no components, given to [`walk`].
    ScanWithoutComponents,
    /// A Huffman code in the scan that its table does not have (F.2.2.3).
    InvalidHuffmanCode,
    /// A block whose AC coefficients run past its 63rd (F.2.2.2).
    CoefficientsPast63rd,
    /// A restart marker inside a restart interval, before its last MCU.
    RstInsideInterval,
    /// Data where a restart marker is due: past the restart interval's
    /// last byte.
    DataBeforeRst,
    /// A restart marker whose number is not the one due: the count of
    /// restart markers since the scan began, modulo 8 (B.2.1).
    RstOutOfTurn,
    /// Over 32 bytes after the scan's last MCU ([`walk`]).
    Over32BytesAfterLastMcu,
    /// Data after the scan's last MCU that decodes as two whole blocks
    /// ([`walk`]).
    DataAfterLastMcu,
    /// Data after the scan's last MCU that stops inside a code with more
    /// than padding's bits left ([`walk`]).
    DataAfterLastMcuEndsInsideCode,
    /// Data after a restart marker that follows the scan's last MCU.
    DataAfterFinalRst,
    /// A valid image that uses what this decoder does not take, named.
    Unsupported(&'static str),
    /// A variable the specification derives from the stream's values,
    /// named, past what its type holds: no conforming stream makes it.
    Overflow(&'static str),
    /// The scan's data ends, at the marker after it, before its last MCU
    /// ([`walk`]).
    ScanEndsEarly,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let malformed = match self {
            Self::Truncated => return write!(f, "the image ends early"),
            Self::Unsupported(what) => return write!(f, "unsupported: {what}"),
            Self::Overflow(name) => return write!(f, "{name} overflows"),
            Self::ScanEndsEarly => {
                return write!(f, "the scan's data ends before its last MCU");
            }
            Self::NoSoi => "no SOI at the start",
            Self::EoiBeforeScan => "an EOI before any scan",
            Self::SecondSoi => "a second SOI",
            Self::RstOutsideScan => "a restart marker outside a scan",
            Self::NotAMarker => "data where a marker should be",
            Self::StuffedZeroAsMarker => "a stuffed zero where a marker should be",
            Self::SegmentLength => "a segment shorter than its length field",
            Self::SecondSof => "a second frame header",
            Self::SofTooShort => "a frame header too short",
            Self::SofPrecision => "a sample precision the process does not have",
            Self::SofZeroWidth => "a frame 0 samples wide",
            Self::SofLength => "a frame header's length is not its components'",
            Self::SofSamplingFactor => "a sampling factor outside 1 to 4",
            Self::SofQuantizationTable => "a quantization table outside 0 to 3",
            Self::SofDuplicateComponent => "two components with one identifier",
            Self::DqtPrecisionOrDestination => "a quantization table's Pq or Tq",
            Self::DqtPastSegment => "a quantization table runs past its segment",
            Self::DqtZeroValue => "a quantization value of 0",
            Self::DhtClassOrDestination => "a Huffman table's Tc or Th",
            Self::DhtPastSegment => "a Huffman table runs past its segment",
            Self::DhtOver256Codes => "a Huffman table of over 256 codes",
            Self::DhtTooManyCodes => "a Huffman table with too many codes",
            Self::DriLength => "a DRI segment not 4 bytes long",
            Self::DnlBeforeScan => "a DNL before the scan",
            Self::SosBeforeSof => "a scan before the frame header",
            Self::SosTooShort => "a scan header too short",
            Self::SosLength => "a scan header's length is not its components'",
            Self::SosComponentNotInFrame => {
                "a scan component not in the frame, or out of its order"
            }
            Self::SosTableSelector => "a scan's Huffman table outside its process's",
            Self::SosNotSequential => "a sequential scan's Ss, Se, Ah or Al",
            Self::McuOver10Blocks => "an MCU of over 10 blocks",
            Self::UndefinedQuantizationTable => "a component's quantization table is undefined",
            Self::UndefinedHuffmanTable => "a scan names an undefined Huffman table",
            Self::EmptyHuffmanTable => "a scan names an empty Huffman table",
            Self::DcCategoryOver11 => "a DC table with a category over 11",
            Self::DcTableOver12Codes => "a DC table of over 12 codes",
            Self::AcTableValues => "an AC table past 8-bit coefficients' 162 values",
            Self::FrameWithoutComponents => "a frame without components",
            Self::ScanWithoutComponents => "a scan without components",
            Self::InvalidHuffmanCode => "an invalid Huffman code in the scan",
            Self::CoefficientsPast63rd => "a block's coefficients run past the 63rd",
            Self::RstInsideInterval => "a restart marker inside an interval",
            Self::DataBeforeRst => "data where a restart marker should be",
            Self::RstOutOfTurn => "a restart marker out of turn",
            Self::Over32BytesAfterLastMcu => "over 32 bytes after the scan's last MCU",
            Self::DataAfterLastMcu => "data after the scan's last MCU",
            Self::DataAfterLastMcuEndsInsideCode => {
                "data after the scan's last MCU ends inside a code"
            }
            Self::DataAfterFinalRst => "data after a restart marker past the last MCU",
        };
        write!(f, "malformed: {malformed}")
    }
}

impl std::error::Error for Error {}
