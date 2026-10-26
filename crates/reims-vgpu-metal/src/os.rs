//! The OS services under core's platform boundary: core's threads ([`threads`]), the
//! Objective-C autorelease pool ([`autorelease`]), the host's clocks ([`clock`]), IOSurface
//! ([`iosurface`]) and Mach VM ([`vm`]); a surface's bytes and planes as a host outside Metal
//! reaches them ([`surface_bytes`]), and its IOSurface as a device outside Metal writes it
//! ([`device_lend`]).

pub mod autorelease;
pub mod clock;
pub mod device_lend;
pub mod iosurface;
pub mod surface_bytes;
pub mod threads;
pub mod vm;
