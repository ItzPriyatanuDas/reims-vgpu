//! The VideoToolbox device's wire format: `vtpv` messages and their
//! parameters, the property lists, image descriptions and sample buffers
//! they carry, and the device's `subs` and `rels` boxes.
//!
//! Every reader takes guest bytes and fails with [`WireError`] rather than
//! panicking; every failure is the guest-visible
//! [`Status::MalformedMessage`] unless its doc says otherwise.

pub mod atom;
pub mod image_description;
pub mod known_keys;
pub mod message;
pub mod property_list;
pub mod sample_buffer;
pub mod surface_boxes;

use crate::status::Status;

/// Why guest bytes could not be read or a message could not be written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    /// The bytes do not follow the format: an atom's size, a parameter's
    /// length or type, a missing header.
    #[error("malformed: {0}")]
    Malformed(&'static str),
    /// A property list that does not decode.
    #[error("property list: {0}")]
    PropertyList(String),
    /// A value this side refuses to write, such as a property list the
    /// encoder rejects.
    #[error("cannot write: {0}")]
    Unwritable(String),
}

impl WireError {
    /// The status the guest sees for this failure.
    pub fn status(&self) -> Status {
        match self {
            Self::Malformed(_) | Self::PropertyList(_) => Status::MalformedMessage,
            Self::Unwritable(_) => Status::Parameter,
        }
    }
}
