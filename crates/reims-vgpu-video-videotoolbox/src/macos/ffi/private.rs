//! What VideoToolbox exports without a header: the decoders' capabilities,
//! which is where Apple's own host reads the profiles and levels it lists
//! to a guest.
//!
//! The names are the SDK's: each is in
//! `VideoToolbox.framework/VideoToolbox.tbd`'s exports (SDK 27.0, read
//! 2026-10-24). The signature is in no header:
//! `VTCopyDecoderCapabilitiesDictionaryForCodecTypes` takes a CFArray of
//! codec types as CFNumbers and answers a retained CFDictionary.
//!
//! No header declares them, so they are looked up at run time with
//! `dlsym`, not linked: a VideoToolbox without one answers `None`, and the
//! backend lists what the public calls alone say. The keys' strings are
//! read from VideoToolbox's own constants the same way.

use std::ffi::{CStr, c_char, c_void};
use std::sync::OnceLock;

use super::constants;
use super::core_foundation::{CFArrayRef, CFDictionaryRef, CFStringRef};
use crate::macos::cf::Cf;

unsafe extern "C" {
    /// `dlsym(3)`, from `<dlfcn.h>`.
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

constants! {
    /// `RTLD_DEFAULT`: every image the process has loaded, VideoToolbox
    /// among them, since this crate links it.
    pub(crate) const RTLD_DEFAULT: isize = -2;
}

/// `VTCopyDecoderCapabilitiesDictionaryForCodecTypes`.
pub(crate) type CopyDecoderCapabilities = unsafe extern "C" fn(CFArrayRef) -> CFDictionaryRef;

/// The private function and keys, those this VideoToolbox has.
pub(crate) struct Private {
    /// `VTCopyDecoderCapabilitiesDictionaryForCodecTypes`.
    pub(crate) copy_decoder_capabilities: Option<CopyDecoderCapabilities>,
    /// `kVTDecoderCapability_CodecSupportDict`.
    pub(crate) codec_support: Option<Cf<'static>>,
    /// `kVTDecoderCodecCapability_PerProfileSupport`.
    pub(crate) per_profile_support: Option<Cf<'static>>,
    /// `kVTDecoderProfileCapability_MaxDecodeLevel`.
    pub(crate) max_decode_level: Option<Cf<'static>>,
    /// `kVTDecoderProfileCapability_IsHardwareAccelerated`.
    pub(crate) is_hardware_accelerated: Option<Cf<'static>>,
}

// SAFETY: the function pointers are plain code addresses, and the keys are
// immutable CFStrings that VideoToolbox's constants hold for the process's
// life; CFStrings may be read from any thread.
unsafe impl Send for Private {}
// SAFETY: as for Send: nothing in it is ever written after the lookup.
unsafe impl Sync for Private {}

/// The address of `name` in the loaded images, or None.
pub(crate) fn symbol(name: &CStr) -> Option<*mut c_void> {
    // SAFETY: dlsym reads the NUL-terminated name and searches the loaded
    // images; RTLD_DEFAULT is a handle it defines.
    let address = unsafe { dlsym(RTLD_DEFAULT as *mut c_void, name.as_ptr()) };
    (!address.is_null()).then_some(address)
}

/// The CFString a `const CFStringRef` constant named `name` holds.
pub(crate) fn key(name: &CStr) -> Option<Cf<'static>> {
    let address = symbol(name)?;
    // SAFETY: the symbol is one of VideoToolbox's `const CFStringRef`
    // constants (the tbd's), so its address holds a CFStringRef, set before
    // any code of the process runs.
    let value = unsafe { *address.cast::<CFStringRef>() };
    // SAFETY: NULL or the constant's CFString, which VideoToolbox never
    // releases: it lives for the process.
    unsafe { Cf::borrow(value) }
}

/// The private functions and keys, looked up once.
pub(crate) fn private() -> &'static Private {
    static PRIVATE: OnceLock<Private> = OnceLock::new();
    PRIVATE.get_or_init(|| Private {
        copy_decoder_capabilities: symbol(c"VTCopyDecoderCapabilitiesDictionaryForCodecTypes")
            // SAFETY: the symbol is that function, of the signature the
            // module doc gives; a function pointer has a data pointer's
            // size.
            .map(|address| unsafe {
                std::mem::transmute::<*mut c_void, CopyDecoderCapabilities>(address)
            }),
        codec_support: key(c"kVTDecoderCapability_CodecSupportDict"),
        per_profile_support: key(c"kVTDecoderCodecCapability_PerProfileSupport"),
        max_decode_level: key(c"kVTDecoderProfileCapability_MaxDecodeLevel"),
        is_hardware_accelerated: key(c"kVTDecoderProfileCapability_IsHardwareAccelerated"),
    })
}

#[cfg(test)]
mod tests {
    use super::private;
    use crate::macos::cf::Cf;

    /// Every private name resolves on the host, and the keys hold the
    /// strings measured with a probe (macOS 27.0.1 26A434, 2026-10-24).
    #[test]
    fn every_private_name_resolves_here() {
        let private = private();
        assert!(private.copy_decoder_capabilities.is_some());
        let strings = [
            (private.codec_support, "VTCodecSupportDict"),
            (private.per_profile_support, "VTPerProfileSupport"),
            (private.max_decode_level, "VTMaxDecodeLevel"),
            (private.is_hardware_accelerated, "VTIsHardwareAccelerated"),
        ];
        for (key, expected) in strings {
            assert_eq!(key.and_then(Cf::string).as_deref(), Some(expected));
        }
    }
}
