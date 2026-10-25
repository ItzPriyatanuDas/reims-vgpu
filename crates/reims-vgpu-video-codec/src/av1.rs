//! AV1's syntax and the decoding process above the tile data, for a
//! stateless accelerator: OBUs and the `av1C` record, sequence headers,
//! the uncompressed frame header with everything it inherits from its
//! references, tile groups, and the eight reference slots (AV1 Bitstream
//! and Decoding Process Specification, version 1.0.0 with Errata 1, by
//! section).
//!
//! This module is plain Rust and runs on any host, so that its tests run
//! in `cargo xtask check` on a Mac. What decodes the tiles is an
//! [`Accelerator`], which each backend implements for its hardware; none
//! of it is here.

pub mod av1c;
mod bits;
mod decoder;
pub mod frame;
pub mod metadata;
pub mod obu;
pub mod sequence;
#[cfg(test)]
mod tests;
pub mod tile_group;

pub use decoder::{Accelerator, Decoder, FrameParams, FramesOnly, Output, Step};

/// Why a stream could not be read or decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The data ended inside a syntax element or an OBU.
    Truncated,
    /// A syntax element, named, is outside what the specification allows.
    OutOfRange(&'static str),
    /// An `av1C` record shorter than its four fixed bytes.
    Av1cTooShort,
    /// An `av1C` record whose `marker` is not 1.
    Av1cMarker,
    /// An `av1C` record whose `version` is not 1.
    Av1cVersion,
    /// A frame came before any sequence header.
    MissingSequenceHeader,
    /// A frame predicts from, or shows, a reference slot that holds no
    /// frame, as after starting or resetting somewhere other than a key
    /// frame.
    MissingReference,
    /// A feature this decoder does not implement, named.
    Unsupported(&'static str),
    /// A variable the specification derives from the stream's values,
    /// named, past what its type holds: no conforming stream makes it.
    Overflow(&'static str),
    /// A frame OBU while another frame's tile groups are still to come.
    FrameInsideFrame,
    /// A frame OBU whose header shows an existing frame, which only a
    /// frame header OBU may.
    ShownExistingFrameInFrameObu,
    /// A tile group OBU with no frame header before it.
    TileGroupWithoutFrameHeader,
    /// A second shown frame of one spatial layer in a temporal unit (7.5).
    SecondShownFrame,
    /// A new sequence header whose first frame is not a shown key frame.
    SequenceWithoutKeyFrame,
    /// A key frame shown again from its slot a second time (6.8.2).
    KeyFrameShownAgain,
    /// A reference more than twice as large, or more than 16 times as
    /// small, as the frame in either dimension (7.11.3.3).
    ReferenceScale,
    /// A component's film grain point values that do not increase
    /// (`film_grain_params()`, 5.9.30).
    FilmGrainPointsUnordered,
    /// An index past the eight reference slots.
    NoSuchReferenceSlot,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => write!(f, "the bitstream ends inside a syntax element"),
            Self::OutOfRange(name) => write!(f, "{name} is out of range"),
            Self::Av1cTooShort => write!(f, "bad av1C record: shorter than 4 bytes"),
            Self::Av1cMarker => write!(f, "bad av1C record: marker is not 1"),
            Self::Av1cVersion => write!(f, "bad av1C record: version is not 1"),
            Self::MissingSequenceHeader => write!(f, "no sequence header has been received"),
            Self::MissingReference => write!(f, "a reference frame is missing"),
            Self::Unsupported(what) => write!(f, "unsupported: {what}"),
            Self::Overflow(name) => write!(f, "{name} overflows"),
            Self::FrameInsideFrame => write!(f, "a frame OBU inside another frame"),
            Self::ShownExistingFrameInFrameObu => {
                write!(f, "show_existing_frame in a frame OBU")
            }
            Self::TileGroupWithoutFrameHeader => write!(f, "a tile group with no frame header"),
            Self::SecondShownFrame => write!(
                f,
                "a second shown frame of a spatial layer in a temporal unit"
            ),
            Self::SequenceWithoutKeyFrame => {
                write!(f, "a new sequence that does not start at a key frame")
            }
            Self::KeyFrameShownAgain => write!(f, "a key frame shown again"),
            Self::ReferenceScale => write!(f, "a reference out of scale (7.11.3.3)"),
            Self::FilmGrainPointsUnordered => {
                write!(f, "film grain point values that do not increase")
            }
            Self::NoSuchReferenceSlot => write!(f, "a reference slot index past the eight"),
        }
    }
}

impl std::error::Error for Error {}
