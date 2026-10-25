//! CoreFoundation objects as the backend holds and reads them, by
//! CoreFoundation's ownership rules: an [`Owned`] reference, which a Create
//! or Copy call gives and a drop releases, and a [`Cf`] borrow, which a Get
//! call gives and which lives no longer than what it was got from. The
//! strings, numbers, data, dictionaries and arrays VideoToolbox takes and
//! answers are made and read through them, with property lists
//! ([`plist::Value`]) both ways for the guest's encoder properties.
//!
//! Every reader takes a [`Cf`], so that only a live object is ever read:
//! a raw `CFTypeRef` becomes one at the FFI edge, under that call's rule.

use std::ffi::{c_char, c_void};
use std::marker::PhantomData;
use std::ptr::NonNull;

use reims_vgpu_video_core::plist;

use super::ffi::core_foundation::{
    self as cf, CFTypeID, CFTypeRef, kCFNumberFloat64Type, kCFNumberSInt64Type,
    kCFStringEncodingUTF8,
};

/// One reference to a CoreFoundation object (a string, a dictionary, a
/// session, a pixel buffer), released when dropped. It is never cloned:
/// another reference is [`Owned::retain`] of its [`Owned::cf`].
pub(crate) struct Owned(NonNull<c_void>);

// SAFETY: CFRetain and CFRelease are thread-safe for every CoreFoundation
// type, and each object the crate holds is either immutable once made
// (strings, numbers, dictionaries, format descriptions) or used through
// its owner, which is not Sync, one thread at a time (sessions, pixel
// buffers).
unsafe impl Send for Owned {}

impl Owned {
    /// Takes a reference the caller owns, from a Create or Copy call, or
    /// None for NULL.
    ///
    /// # Safety
    ///
    /// `object` is NULL or a CoreFoundation object with a reference the
    /// caller owns and hands over.
    pub(crate) unsafe fn take(object: CFTypeRef) -> Option<Self> {
        NonNull::new(object.cast_mut()).map(Self)
    }

    /// A reference of its own to `object`, which keeps it past the borrow.
    pub(crate) fn retain(object: Cf<'_>) -> Self {
        // SAFETY: a live object, as a Cf is for its lifetime; the reference
        // CFRetain takes is this one's, given back by its drop.
        unsafe { cf::CFRetain(object.as_ptr()) };
        Self(object.object)
    }

    /// The object, borrowed for as long as `self` lives.
    pub(crate) fn cf(&self) -> Cf<'_> {
        Cf {
            object: self.0,
            _borrow: PhantomData,
        }
    }

    /// The object, for the calls that take it while `self` lives.
    pub(crate) fn as_ptr(&self) -> CFTypeRef {
        self.0.as_ptr()
    }

    /// The object as a mutable pointer, for the calls that take one.
    pub(crate) fn as_mut_ptr(&self) -> *mut c_void {
        self.0.as_ptr()
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: self owns one reference, given back once.
        unsafe { cf::CFRelease(self.as_ptr()) };
    }
}

impl std::fmt::Debug for Owned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Owned({:p})", self.0)
    }
}

/// A CoreFoundation object borrowed for `'a`: never NULL, and live for
/// `'a`. A Get call's answer is borrowed from the object it was asked of,
/// and lives as long as that does, so a value got from a dictionary is a
/// `Cf` of the dictionary's lifetime. A framework's constant is one of
/// `'static` ([`constant!`]).
#[derive(Clone, Copy)]
pub(crate) struct Cf<'a> {
    object: NonNull<c_void>,
    _borrow: PhantomData<&'a Owned>,
}

impl std::fmt::Debug for Cf<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cf({:p})", self.object)
    }
}

/// The CoreFoundation object one of the SDK's constants holds, as a
/// [`Cf`] of `'static`: `constant!(video_toolbox::kVTRotation_0)`. The name
/// is one of the statics `ffi`'s `declare!` links, and only a pointer one
/// type-checks, each of them a `const CFStringRef` or a `CFBooleanRef`.
macro_rules! constant {
    ($module:ident :: $name:ident) => {
        // SAFETY: a static `ffi`'s `declare!` links, which the framework sets
        // to one of its constant objects before any code of the process
        // runs, never NULL, and never changes or releases: reading it reads
        // an immutable static, and the object lives for the process.
        unsafe { $crate::macos::cf::Cf::constant($crate::macos::ffi::$module::$name) }
    };
}
pub(crate) use constant;

impl<'a> Cf<'a> {
    /// `object`, borrowed for `'a`, or None for NULL.
    ///
    /// # Safety
    ///
    /// `object` is NULL or a CoreFoundation object that stays live for
    /// `'a`: one a Get call answered, for as long as what it was asked of
    /// lives unchanged, or one a callback lends, for the call.
    pub(crate) unsafe fn borrow(object: CFTypeRef) -> Option<Self> {
        NonNull::new(object.cast_mut()).map(|object| Self {
            object,
            _borrow: PhantomData,
        })
    }

    /// The object a framework's constant holds ([`constant!`]).
    ///
    /// # Safety
    ///
    /// `object` is the value of one of a framework's constant objects,
    /// which is never NULL and lives for the process.
    pub(crate) unsafe fn constant(object: CFTypeRef) -> Cf<'static> {
        Cf {
            // SAFETY: never NULL, as the caller says.
            object: unsafe { NonNull::new_unchecked(object.cast_mut()) },
            _borrow: PhantomData,
        }
    }

    /// The object, for the calls that take it during `'a`.
    pub(crate) fn as_ptr(self) -> CFTypeRef {
        self.object.as_ptr()
    }

    /// Whether the object is of the type whose ID `type_id` gives.
    fn is(self, type_id: unsafe extern "C" fn() -> CFTypeID) -> bool {
        // SAFETY: a type ID getter takes nothing; CFGetTypeID reads a live
        // object, which a Cf is.
        unsafe { cf::CFGetTypeID(self.as_ptr()) == type_id() }
    }

    /// The text of a CFString, or None for anything else.
    pub(crate) fn string(self) -> Option<String> {
        if !self.is(cf::CFStringGetTypeID) {
            return None;
        }
        // SAFETY: a live CFString.
        let length = unsafe { cf::CFStringGetLength(self.as_ptr()) };
        // SAFETY: a computation on numbers alone.
        let size = unsafe { cf::CFStringGetMaximumSizeForEncoding(length, kCFStringEncodingUTF8) };
        let mut buffer = vec![0u8; usize::try_from(size).ok()?.checked_add(1)?];
        // SAFETY: a live CFString, and a buffer writable for the length
        // given, its own.
        let done = unsafe {
            cf::CFStringGetCString(
                self.as_ptr(),
                buffer.as_mut_ptr().cast::<c_char>(),
                index(buffer.len()),
                kCFStringEncodingUTF8,
            )
        };
        if done == 0 {
            return None;
        }
        let end = buffer.iter().position(|&byte| byte == 0)?;
        buffer.truncate(end);
        String::from_utf8(buffer).ok()
    }

    /// A CFNumber's value as an i64, or None for anything else.
    pub(crate) fn i64(self) -> Option<i64> {
        if !self.is(cf::CFNumberGetTypeID) {
            return None;
        }
        let mut value = 0i64;
        // SAFETY: a live CFNumber, read into a live i64 of the type named.
        // A false return (a lossy conversion, from a float) still writes.
        unsafe {
            cf::CFNumberGetValue(self.as_ptr(), kCFNumberSInt64Type, (&raw mut value).cast())
        };
        Some(value)
    }

    /// A CFNumber's value as an f64, or None for anything else.
    pub(crate) fn f64(self) -> Option<f64> {
        if !self.is(cf::CFNumberGetTypeID) {
            return None;
        }
        let mut value = 0f64;
        // SAFETY: a live CFNumber, read into a live f64 of the type named.
        unsafe {
            cf::CFNumberGetValue(self.as_ptr(), kCFNumberFloat64Type, (&raw mut value).cast())
        };
        Some(value)
    }

    /// A CFBoolean's value, or None for anything else.
    pub(crate) fn bool(self) -> Option<bool> {
        // SAFETY: a live CFBoolean.
        self.is(cf::CFBooleanGetTypeID)
            .then(|| unsafe { cf::CFBooleanGetValue(self.as_ptr()) } != 0)
    }

    /// A CFData's bytes, copied, or None for anything else.
    pub(crate) fn data(self) -> Option<Vec<u8>> {
        if !self.is(cf::CFDataGetTypeID) {
            return None;
        }
        // SAFETY: a live CFData.
        let length = usize::try_from(unsafe { cf::CFDataGetLength(self.as_ptr()) }).ok()?;
        // SAFETY: as above.
        let bytes = unsafe { cf::CFDataGetBytePtr(self.as_ptr()) };
        if bytes.is_null() {
            return Some(Vec::new());
        }
        // SAFETY: a CFData's byte pointer is valid for its length while it
        // lives, which it does for this call; the bytes are copied out.
        Some(unsafe { std::slice::from_raw_parts(bytes, length) }.to_vec())
    }

    /// The value a CFDictionary holds for `key`, borrowed from it: None for
    /// none, or for anything but a dictionary.
    pub(crate) fn get(self, key: Cf<'_>) -> Option<Cf<'a>> {
        if !self.is(cf::CFDictionaryGetTypeID) {
            return None;
        }
        // SAFETY: a live CFDictionary and a live key. The value is the
        // dictionary's (the Get rule), live while it is, which is 'a.
        unsafe { Cf::borrow(cf::CFDictionaryGetValue(self.as_ptr(), key.as_ptr())) }
    }

    /// The value a CFDictionary holds for the key `key` spells.
    pub(crate) fn get_str(self, key: &str) -> Option<Cf<'a>> {
        self.get(string(key).cf())
    }

    /// A CFDictionary's keys and values, borrowed from it; empty for
    /// anything but a dictionary.
    pub(crate) fn entries(self) -> Vec<(Cf<'a>, Cf<'a>)> {
        if !self.is(cf::CFDictionaryGetTypeID) {
            return Vec::new();
        }
        // SAFETY: a live CFDictionary.
        let count = unsafe { cf::CFDictionaryGetCount(self.as_ptr()) };
        let count = usize::try_from(count).unwrap_or(0);
        let mut keys = vec![std::ptr::null(); count];
        let mut values = vec![std::ptr::null(); count];
        // SAFETY: both arrays hold the dictionary's count of entries.
        unsafe {
            cf::CFDictionaryGetKeysAndValues(self.as_ptr(), keys.as_mut_ptr(), values.as_mut_ptr())
        };
        keys.into_iter()
            .zip(values)
            .filter_map(|(key, value)| {
                // SAFETY: each the dictionary's (the Get rule), live while
                // it is, which is 'a.
                unsafe { Some((Cf::borrow(key)?, Cf::borrow(value)?)) }
            })
            .collect()
    }

    /// A CFArray's values, borrowed from it; empty for anything but an
    /// array.
    pub(crate) fn values(self) -> Vec<Cf<'a>> {
        if !self.is(cf::CFArrayGetTypeID) {
            return Vec::new();
        }
        // SAFETY: a live CFArray.
        let count = unsafe { cf::CFArrayGetCount(self.as_ptr()) };
        (0..count)
            .filter_map(|at| {
                // SAFETY: a live CFArray, read at an index below its count;
                // the value is the array's (the Get rule), live while it
                // is, which is 'a.
                unsafe { Cf::borrow(cf::CFArrayGetValueAtIndex(self.as_ptr(), at)) }
            })
            .collect()
    }

    /// A CoreFoundation property list object as a property list value:
    /// what VideoToolbox's lists and capabilities answer. Anything else is
    /// None.
    pub(crate) fn to_plist(self) -> Option<plist::Value> {
        if let Some(text) = self.string() {
            return Some(plist::Value::String(text));
        }
        if let Some(value) = self.bool() {
            return Some(plist::Value::Boolean(value));
        }
        if self.is(cf::CFNumberGetTypeID) {
            // SAFETY: a live CFNumber.
            let float = unsafe { cf::CFNumberIsFloatType(self.as_ptr()) } != 0;
            return if float {
                self.f64().map(plist::Value::Real)
            } else {
                self.i64().map(|value| plist::Value::Integer(value.into()))
            };
        }
        if let Some(bytes) = self.data() {
            return Some(plist::Value::Data(bytes));
        }
        if self.is(cf::CFArrayGetTypeID) {
            return (self.values().into_iter())
                .map(Cf::to_plist)
                .collect::<Option<Vec<_>>>()
                .map(plist::Value::Array);
        }
        if self.is(cf::CFDictionaryGetTypeID) {
            let mut dictionary = plist::Dictionary::new();
            for (key, value) in self.entries() {
                dictionary.insert(key.string()?, value.to_plist()?);
            }
            return Some(plist::Value::Dictionary(dictionary));
        }
        None
    }
}

/// Where a Create call gave NULL, which it does only when out of memory:
/// the process ends as on any allocation that fails, through std's
/// allocation error handler. The layout is a stand-in: CoreFoundation does
/// not say what it asked for.
fn out_of_memory() -> ! {
    std::alloc::handle_alloc_error(std::alloc::Layout::new::<usize>())
}

/// A Create call's result, which only running out of memory makes NULL.
///
/// # Safety
///
/// `made` is NULL or an object whose reference the caller owns.
unsafe fn made(made: CFTypeRef) -> Owned {
    // SAFETY: as the caller says.
    unsafe { Owned::take(made) }.unwrap_or_else(|| out_of_memory())
}

/// A CFIndex for a length, saturating (no length here comes near it).
fn index(length: usize) -> cf::CFIndex {
    cf::CFIndex::try_from(length).unwrap_or(cf::CFIndex::MAX)
}

/// A CFString of `text`.
pub(crate) fn string(text: &str) -> Owned {
    // SAFETY: the bytes are valid UTF-8 for their length; the result is a
    // Create call's.
    unsafe {
        made(cf::CFStringCreateWithBytes(
            std::ptr::null(),
            text.as_ptr(),
            index(text.len()),
            kCFStringEncodingUTF8,
            0,
        ))
    }
}

/// A CFNumber holding a 64-bit integer.
pub(crate) fn number_i64(value: i64) -> Owned {
    // SAFETY: the pointer is to a live i64 of the type named; the result is
    // a Create call's.
    unsafe {
        made(cf::CFNumberCreate(
            std::ptr::null(),
            kCFNumberSInt64Type,
            (&raw const value).cast(),
        ))
    }
}

/// A CFNumber holding a double.
pub(crate) fn number_f64(value: f64) -> Owned {
    // SAFETY: the pointer is to a live f64 of the type named; the result is
    // a Create call's.
    unsafe {
        made(cf::CFNumberCreate(
            std::ptr::null(),
            kCFNumberFloat64Type,
            (&raw const value).cast(),
        ))
    }
}

/// `kCFBooleanTrue` or `kCFBooleanFalse`, which live for the process.
pub(crate) fn boolean(value: bool) -> Cf<'static> {
    if value {
        constant!(core_foundation::kCFBooleanTrue)
    } else {
        constant!(core_foundation::kCFBooleanFalse)
    }
}

/// A CFData holding a copy of `bytes`.
pub(crate) fn data(bytes: &[u8]) -> Owned {
    // SAFETY: the bytes are live for their length; the result is a Create
    // call's.
    unsafe {
        made(cf::CFDataCreate(
            std::ptr::null(),
            bytes.as_ptr(),
            index(bytes.len()),
        ))
    }
}

/// An immutable CFDictionary of `entries`, each key and value retained by
/// it.
pub(crate) fn dictionary(entries: &[(Cf<'_>, Cf<'_>)]) -> Owned {
    let mut keys: Vec<CFTypeRef> = entries.iter().map(|(key, _)| key.as_ptr()).collect();
    let mut values: Vec<CFTypeRef> = entries.iter().map(|(_, value)| value.as_ptr()).collect();
    // SAFETY: the arrays hold live objects for the count given; the CFType
    // callbacks, CoreFoundation's statics, retain each key and value. The
    // result is a Create call's.
    unsafe {
        made(cf::CFDictionaryCreate(
            std::ptr::null(),
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            index(entries.len()),
            &raw const cf::kCFTypeDictionaryKeyCallBacks,
            &raw const cf::kCFTypeDictionaryValueCallBacks,
        ))
    }
}

/// An immutable CFArray of `values`, each retained by it.
pub(crate) fn array(values: &[Cf<'_>]) -> Owned {
    let mut values: Vec<CFTypeRef> = values.iter().map(|value| value.as_ptr()).collect();
    // SAFETY: the array holds live objects for the count given; the CFType
    // callbacks, a CoreFoundation static, retain each. The result is a
    // Create call's.
    unsafe {
        made(cf::CFArrayCreate(
            std::ptr::null(),
            values.as_mut_ptr(),
            index(values.len()),
            &raw const cf::kCFTypeArrayCallBacks,
        ))
    }
}

/// A property list value as a CoreFoundation object: what VideoToolbox's
/// properties take. Dates and UIDs, which no property here has, are None.
pub(crate) fn from_plist(value: &plist::Value) -> Option<Owned> {
    Some(match value {
        plist::Value::Boolean(value) => Owned::retain(boolean(*value)),
        plist::Value::Integer(integer) => match integer.as_signed() {
            Some(value) => number_i64(value),
            // Past i64, the nearest double: a CFNumber holds no u64.
            None => number_f64(integer.as_unsigned()? as f64),
        },
        plist::Value::Real(value) => number_f64(*value),
        plist::Value::String(text) => string(text),
        plist::Value::Data(bytes) => data(bytes),
        plist::Value::Array(items) => {
            let items: Vec<Owned> = items.iter().map(from_plist).collect::<Option<_>>()?;
            array(&items.iter().map(Owned::cf).collect::<Vec<_>>())
        }
        plist::Value::Dictionary(entries) => {
            let owned: Vec<(Owned, Owned)> = entries
                .iter()
                .map(|(key, value)| Some((string(key), from_plist(value)?)))
                .collect::<Option<_>>()?;
            let borrowed: Vec<(Cf<'_>, Cf<'_>)> = owned
                .iter()
                .map(|(key, value)| (key.cf(), value.cf()))
                .collect();
            dictionary(&borrowed)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_numbers_and_data_read_back() {
        assert_eq!(
            string("VTRating").cf().string().as_deref(),
            Some("VTRating")
        );
        assert_eq!(string("").cf().string().as_deref(), Some(""));
        assert_eq!(number_i64(-7).cf().i64(), Some(-7));
        assert_eq!(number_i64(1 << 40).cf().i64(), Some(1 << 40));
        assert_eq!(number_f64(0.25).cf().f64(), Some(0.25));
        assert_eq!(boolean(true).bool(), Some(true));
        assert_eq!(data(&[1, 2, 3]).cf().data(), Some(vec![1, 2, 3]));
        assert_eq!(number_i64(1).cf().string(), None);
    }

    #[test]
    fn property_lists_round_trip_through_core_foundation() {
        let mut inner = plist::Dictionary::new();
        inner.insert("level".into(), plist::Value::Integer(41.into()));
        inner.insert("rate".into(), plist::Value::Real(29.97));
        let value = plist::Value::Array(vec![
            plist::Value::String("H264_High_AutoLevel".into()),
            plist::Value::Boolean(false),
            plist::Value::Data(vec![0, 1]),
            plist::Value::Dictionary(inner),
        ]);
        let object = from_plist(&value);
        let back = object.as_ref().and_then(|object| object.cf().to_plist());
        assert_eq!(back, Some(value));
    }

    #[test]
    fn a_dictionary_answers_its_keys() {
        let key = string("PixelFormatType");
        let value = number_i64(0x3432_3076);
        let made = dictionary(&[(key.cf(), value.cf())]);
        assert_eq!(made.cf().get(key.cf()).and_then(Cf::i64), Some(0x3432_3076));
        assert_eq!(
            made.cf().get_str("PixelFormatType").and_then(Cf::i64),
            Some(0x3432_3076)
        );
        assert!(made.cf().get_str("Width").is_none());
        assert_eq!(made.cf().entries().len(), 1);
        // Anything but a dictionary answers no key.
        assert!(value.cf().get(key.cf()).is_none());
        assert!(value.cf().entries().is_empty() && value.cf().values().is_empty());
    }

    /// A retained reference is one more until it drops, and a dictionary
    /// holds its own of each value: references balance.
    #[test]
    fn references_balance() {
        use crate::macos::ffi::core_foundation::CFGetRetainCount;
        // A CFData, which is never a tagged pointer, as a short string or a
        // small number may be.
        let value = data(&[7; 64]);
        // SAFETY: a live object.
        let count = || unsafe { CFGetRetainCount(value.as_ptr()) };
        let before = count();
        let other = Owned::retain(value.cf());
        assert_eq!(count(), before + 1);
        drop(other);
        assert_eq!(count(), before);
        let key = string("key");
        let held = dictionary(&[(key.cf(), value.cf())]);
        assert_eq!(count(), before + 1);
        drop(held);
        assert_eq!(count(), before);
    }
}
