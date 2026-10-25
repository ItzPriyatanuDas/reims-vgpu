//! VP9's syntax and the decoding process above the compressed header, for
//! a stateless accelerator: superframes, the uncompressed frame header
//! with everything a frame inherits (the colour configuration, the loop
//! filter's deltas, the segmentation features), the eight reference slots
//! with their sizes, `show_existing_frame`, the `vpcC` record, and the
//! per-segment quantizers and filter levels an accelerator is given (VP9
//! Bitstream & Decoding Process Specification, version 0.6, by section).
//!
//! The compressed header, the probability contexts and the tiles are the
//! accelerator's: VA-API's drivers and NVDEC read them from the frame's
//! bytes.
//!
//! This module is plain Rust and runs on any host, so that its tests run
//! in `cargo xtask check` on a Mac. What decodes the frames is an
//! [`Accelerator`], which each backend implements for its hardware.

mod bits;
mod decoder;
pub mod header;
pub mod quant;
pub mod superframe;
#[cfg(test)]
mod tests;
pub mod vpcc;

pub use decoder::{
    Accelerator, Decoder, FrameParams, FramesOnly, Output, SegmentParams, Step, StreamFormat,
    check_supported,
};

/// Why a stream could not be read or decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The data ended inside a syntax element, or a frame is shorter than
    /// its headers say.
    Truncated,
    /// A syntax element, named, is outside what the specification allows.
    OutOfRange(&'static str),
    /// A `vpcC` box shorter than its version and flags.
    VpccTooShort,
    /// A `vpcC` box of version 1 shorter than its record's fixed fields.
    VpccVersion1TooShort,
    /// A `vpcC` record whose `codecInitializationData` runs past it.
    VpccInitializationDataPastEnd,
    /// A `vpcC` box of version 0 shorter than its record's fields.
    VpccVersion0TooShort,
    /// A `vpcC` box of a version other than 0 and 1.
    VpccUnknownVersion,
    /// A frame predicts from, takes its size from, or shows a reference
    /// slot that holds no frame, as after starting or resetting somewhere
    /// other than a key frame.
    MissingReference,
    /// A feature this decoder does not implement, named.
    Unsupported(&'static str),
    /// A variable the specification derives from the stream's values,
    /// named, past what its type holds: no conforming stream makes it.
    Overflow(&'static str),
    /// A superframe index whose frame sizes run past the data before it
    /// (Annex B).
    SuperframeIndexPastData,
    /// An inter frame whose profile is of another format than the frames
    /// it predicts from: it inherits their bit depth and subsampling.
    InterFrameFormat,
    /// A reference of another bit depth or subsampling than the frame.
    ReferenceFormat,
    /// No reference within the scaling motion vector prediction takes: at
    /// most twice as large and at least a sixteenth as large as the frame
    /// in each dimension.
    NoReferenceInScale,
    /// A frame after one that did not decode, refused until a key frame
    /// decodes ([`decoder::Decoder::decode`]).
    Unrecovered,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => write!(f, "the bitstream ends inside a syntax element"),
            Self::OutOfRange(name) => write!(f, "{name} is out of range"),
            Self::VpccTooShort => {
                write!(f, "bad vpcC record: shorter than its version and flags")
            }
            Self::VpccVersion1TooShort => {
                write!(f, "bad vpcC record: shorter than a version 1 record")
            }
            Self::VpccInitializationDataPastEnd => write!(
                f,
                "bad vpcC record: codecInitializationData runs past the record"
            ),
            Self::VpccVersion0TooShort => {
                write!(f, "bad vpcC record: shorter than a version 0 record")
            }
            Self::VpccUnknownVersion => write!(f, "bad vpcC record: an unknown version"),
            Self::MissingReference => write!(f, "a reference frame is missing"),
            Self::Unsupported(what) => write!(f, "unsupported: {what}"),
            Self::Overflow(name) => write!(f, "{name} overflows"),
            Self::SuperframeIndexPastData => {
                write!(f, "a superframe index's sizes run past its data")
            }
            Self::InterFrameFormat => write!(f, "an inter frame's profile changes the format"),
            Self::ReferenceFormat => {
                write!(f, "a reference of another bit depth or subsampling")
            }
            Self::NoReferenceInScale => write!(f, "no reference within the scaling allowed"),
            Self::Unrecovered => write!(
                f,
                "a frame after one that did not decode, before a key frame decoded"
            ),
        }
    }
}

impl std::error::Error for Error {}
