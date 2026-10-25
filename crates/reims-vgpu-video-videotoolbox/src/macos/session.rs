//! A VideoToolbox session of any kind, as the backend holds one: torn down
//! by its kind's `...Invalidate` before its reference is released, as each
//! session's header asks ("call ...Invalidate to tear it down and then
//! CFRelease", `VTDecompressionSession.h`, `VTCompressionSession.h`), so
//! that it ends where the backend lets it go, and calls back no more, not
//! whenever VideoToolbox lets go of its own references.

use std::ffi::c_void;

use super::cf::{Cf, Owned};
use super::ffi::core_foundation::{CFAllocatorRef, OSStatus};
use super::ffi::video_toolbox::{
    VTPixelTransferSessionCreate, VTPixelTransferSessionInvalidate, VTSessionSetProperty,
};

/// A session kind's `...Invalidate`.
pub(crate) type Invalidate = unsafe extern "C" fn(session: *mut c_void);

/// One session, invalidated once, before its reference is released: by
/// [`Session::invalidate`] where its owner orders its parts, else on drop.
pub(crate) struct Session {
    session: Owned,
    invalidate: Invalidate,
    invalidated: bool,
}

impl Session {
    /// Takes the session a Create call made, which `invalidate` tears down;
    /// None for NULL.
    ///
    /// # Safety
    ///
    /// `session` is NULL or a session of the kind `invalidate` tears down,
    /// whose reference the caller owns and hands over.
    pub(crate) unsafe fn take(session: *mut c_void, invalidate: Invalidate) -> Option<Self> {
        // SAFETY: as the caller says.
        let session = unsafe { Owned::take(session.cast_const()) }?;
        Some(Self {
            session,
            invalidate,
            invalidated: false,
        })
    }

    /// The session, for the calls that take it while `self` lives.
    pub(crate) fn as_ptr(&self) -> *mut c_void {
        self.session.as_mut_ptr()
    }

    /// Sets `key` to `value`, or clears it for None (`VTSessionSetProperty`),
    /// on a session that is a `VTSessionRef`: a decompression, compression,
    /// pixel transfer or pixel rotation session.
    pub(crate) fn set_property(&self, key: Cf<'_>, value: Option<Cf<'_>>) -> Result<(), OSStatus> {
        // SAFETY: a live session, a live key, and a live value or NULL to
        // clear it; the session retains what it keeps.
        let status = unsafe {
            VTSessionSetProperty(
                self.session.as_ptr(),
                key.as_ptr(),
                value.map_or(std::ptr::null(), Cf::as_ptr),
            )
        };
        if status == 0 { Ok(()) } else { Err(status) }
    }

    /// Tears the session down, once: VideoToolbox calls back for it no more
    /// once this returns, so what its callbacks reach may go after it.
    pub(crate) fn invalidate(&mut self) {
        if !self.invalidated {
            self.invalidated = true;
            // SAFETY: a live session of the kind `invalidate` tears down,
            // torn down once.
            unsafe { (self.invalidate)(self.as_ptr()) };
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Then `session` releases the reference.
        self.invalidate();
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Session").field(&self.session).finish()
    }
}

/// A frame's reference value, which a decode or encode call hands its
/// output callback: the frame's ID's bits as an address, which nothing
/// dereferences.
pub(crate) fn frame_reference(frame_id: i64) -> *mut c_void {
    std::ptr::without_provenance_mut(frame_id.cast_unsigned() as usize)
}

/// The frame ID a reference value of [`frame_reference`] carries.
pub(crate) fn frame_id_of(reference: *mut c_void) -> i64 {
    (reference.addr() as u64).cast_signed()
}

/// A session kind's Create, which makes one with the allocator given.
type Create = unsafe extern "C" fn(allocator: CFAllocatorRef, out: *mut *mut c_void) -> OSStatus;

/// A session `create` makes, which `invalidate` tears down, or the
/// Create call's status.
fn made_by(create: Create, invalidate: Invalidate) -> Result<Session, OSStatus> {
    let mut session: *mut c_void = std::ptr::null_mut();
    // SAFETY: a session kind's Create, with the default allocator and a
    // live local for its out pointer.
    let status = unsafe { create(std::ptr::null(), &raw mut session) };
    // SAFETY: a Create call's result, which `invalidate` tears down.
    let session = unsafe { Session::take(session, invalidate) };
    match session {
        Some(session) if status == 0 => Ok(session),
        _ => Err(status),
    }
}

/// A new pixel transfer session, with no property set.
pub(crate) fn pixel_transfer() -> Result<Session, OSStatus> {
    made_by(
        VTPixelTransferSessionCreate,
        VTPixelTransferSessionInvalidate,
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::macos::cf;

    static INVALIDATED: AtomicUsize = AtomicUsize::new(0);

    /// A stand-in `...Invalidate` that counts its calls.
    unsafe extern "C" fn counted(_session: *mut c_void) {
        INVALIDATED.fetch_add(1, Ordering::SeqCst);
    }

    /// A session is invalidated once, whether its owner invalidates it
    /// first or it is only dropped, and before its reference goes.
    #[test]
    fn a_session_is_invalidated_once_before_it_is_released() {
        let object = cf::data(&[1; 64]);
        let take = || {
            let reference = std::mem::ManuallyDrop::new(Owned::retain(object.cf()));
            // SAFETY: a reference of its own to a live CF object, handed
            // over, which the stand-in's invalidate only counts.
            unsafe { Session::take(reference.as_mut_ptr(), counted) }
        };
        let before = INVALIDATED.load(Ordering::SeqCst);
        drop(take());
        assert_eq!(INVALIDATED.load(Ordering::SeqCst), before + 1);
        let mut session = take();
        if let Some(session) = session.as_mut() {
            session.invalidate();
            session.invalidate();
        }
        drop(session);
        assert_eq!(INVALIDATED.load(Ordering::SeqCst), before + 2);
    }

    /// A frame's ID comes back from its reference value whole, negative
    /// ones included.
    #[test]
    fn frame_ids_round_trip() {
        for frame_id in [0, 1, -1, i64::MAX, i64::MIN, 0x1234_5678_9abc] {
            assert_eq!(frame_id_of(frame_reference(frame_id)), frame_id);
        }
    }
}
