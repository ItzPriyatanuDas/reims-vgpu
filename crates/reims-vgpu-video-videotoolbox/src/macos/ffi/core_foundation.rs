//! CoreFoundation: references, strings, numbers, booleans, data,
//! dictionaries and arrays (`CoreFoundation/CFBase.h`, `CFString.h`,
//! `CFNumber.h`, `CFData.h`, `CFDictionary.h`, `CFArray.h`).

#![expect(non_upper_case_globals, reason = "the SDK's own names")]

use std::ffi::{c_char, c_void};

use super::{constants, declare};

/// `CFTypeRef`.
pub(crate) type CFTypeRef = *const c_void;
/// `CFAllocatorRef`; every call here passes NULL, the default allocator.
pub(crate) type CFAllocatorRef = *const c_void;
/// `CFStringRef`.
pub(crate) type CFStringRef = *const c_void;
/// `CFDictionaryRef`.
pub(crate) type CFDictionaryRef = *const c_void;
/// `CFArrayRef`.
pub(crate) type CFArrayRef = *const c_void;
/// `CFNumberRef`.
pub(crate) type CFNumberRef = *const c_void;
/// `CFBooleanRef`.
pub(crate) type CFBooleanRef = *const c_void;
/// `CFDataRef`.
pub(crate) type CFDataRef = *const c_void;
/// `CFPropertyListRef`.
pub(crate) type CFPropertyListRef = *const c_void;
/// `CFIndex`: `signed long`.
pub(crate) type CFIndex = isize;
/// `CFTypeID`: `unsigned long`.
pub(crate) type CFTypeID = usize;
/// `CFStringEncoding`: `UInt32`.
pub(crate) type CFStringEncoding = u32;
/// `CFNumberType`: a `CFIndex` enum.
pub(crate) type CFNumberType = CFIndex;
/// `Boolean`: `unsigned char`.
pub(crate) type Boolean = u8;
/// `OSStatus`: `SInt32`.
pub(crate) type OSStatus = i32;
/// `OSType`: `FourCharCode`, a `UInt32`.
pub(crate) type OSType = u32;

/// `CFDictionaryKeyCallBacks`, only ever passed by address.
#[repr(C)]
pub(crate) struct CFDictionaryKeyCallBacks {
    _opaque: [u8; 0],
}

/// `CFDictionaryValueCallBacks`, only ever passed by address.
#[repr(C)]
pub(crate) struct CFDictionaryValueCallBacks {
    _opaque: [u8; 0],
}

/// `CFArrayCallBacks`, only ever passed by address.
#[repr(C)]
pub(crate) struct CFArrayCallBacks {
    _opaque: [u8; 0],
}

declare! {
    #[link(name = "CoreFoundation", kind = "framework")]
    functions {
        fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
        fn CFRelease(cf: CFTypeRef);
        fn CFGetRetainCount(cf: CFTypeRef) -> CFIndex;
        fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;
        fn CFStringGetTypeID() -> CFTypeID;
        fn CFNumberGetTypeID() -> CFTypeID;
        fn CFBooleanGetTypeID() -> CFTypeID;
        fn CFDataGetTypeID() -> CFTypeID;
        fn CFDictionaryGetTypeID() -> CFTypeID;
        fn CFArrayGetTypeID() -> CFTypeID;
        fn CFStringCreateWithBytes(
            alloc: CFAllocatorRef,
            bytes: *const u8,
            num_bytes: CFIndex,
            encoding: CFStringEncoding,
            is_external_representation: Boolean,
        ) -> CFStringRef;
        fn CFStringGetLength(the_string: CFStringRef) -> CFIndex;
        fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: CFStringEncoding) -> CFIndex;
        fn CFStringGetCString(
            the_string: CFStringRef,
            buffer: *mut c_char,
            buffer_size: CFIndex,
            encoding: CFStringEncoding,
        ) -> Boolean;
        fn CFNumberCreate(
            allocator: CFAllocatorRef,
            the_type: CFNumberType,
            value_ptr: *const c_void,
        ) -> CFNumberRef;
        fn CFNumberGetValue(
            number: CFNumberRef,
            the_type: CFNumberType,
            value_ptr: *mut c_void,
        ) -> Boolean;
        fn CFNumberIsFloatType(number: CFNumberRef) -> Boolean;
        fn CFBooleanGetValue(boolean: CFBooleanRef) -> Boolean;
        fn CFDataCreate(allocator: CFAllocatorRef, bytes: *const u8, length: CFIndex) -> CFDataRef;
        fn CFDataGetLength(the_data: CFDataRef) -> CFIndex;
        fn CFDataGetBytePtr(the_data: CFDataRef) -> *const u8;
        fn CFDictionaryCreate(
            allocator: CFAllocatorRef,
            keys: *mut *const c_void,
            values: *mut *const c_void,
            num_values: CFIndex,
            key_call_backs: *const CFDictionaryKeyCallBacks,
            value_call_backs: *const CFDictionaryValueCallBacks,
        ) -> CFDictionaryRef;
        fn CFDictionaryGetCount(the_dict: CFDictionaryRef) -> CFIndex;
        fn CFDictionaryGetValue(the_dict: CFDictionaryRef, key: *const c_void) -> *const c_void;
        fn CFDictionaryGetKeysAndValues(
            the_dict: CFDictionaryRef,
            keys: *mut *const c_void,
            values: *mut *const c_void,
        );
        fn CFArrayCreate(
            allocator: CFAllocatorRef,
            values: *mut *const c_void,
            num_values: CFIndex,
            call_backs: *const CFArrayCallBacks,
        ) -> CFArrayRef;
        fn CFArrayGetCount(the_array: CFArrayRef) -> CFIndex;
        fn CFArrayGetValueAtIndex(the_array: CFArrayRef, idx: CFIndex) -> *const c_void;
    }
    statics {
        static kCFBooleanTrue: CFBooleanRef;
        static kCFBooleanFalse: CFBooleanRef;
        static kCFTypeDictionaryKeyCallBacks: CFDictionaryKeyCallBacks;
        static kCFTypeDictionaryValueCallBacks: CFDictionaryValueCallBacks;
        static kCFTypeArrayCallBacks: CFArrayCallBacks;
    }
}

constants! {
    /// `kCFStringEncodingUTF8`.
    pub(crate) const kCFStringEncodingUTF8: CFStringEncoding = 0x0800_0100;
    /// `kCFNumberSInt64Type`.
    pub(crate) const kCFNumberSInt64Type: CFNumberType = 4;
    /// `kCFNumberFloat64Type`.
    pub(crate) const kCFNumberFloat64Type: CFNumberType = 6;
}
