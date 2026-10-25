//! The fuzzers' inputs, written from the wire's own writers and mutated,
//! so that inputs get past the first header. Each file makes one kind of
//! value.

pub mod description;
pub mod message;
pub mod plist;
pub mod record;
pub mod sample_buffer;
