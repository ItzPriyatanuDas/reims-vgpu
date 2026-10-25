//! The host session: everything the VideoToolbox device's host side does
//! with the guest's messages, apart from the virtio transport.
//!
//! The device hands each queue-0 message to [`HostSession::deliver`] and
//! writes the status it returns back to the guest; every message the host
//! sends goes out through the [`GuestSink`] the session was made with,
//! which the device rings onto queue 1. This follows Apple's
//! `VTParavirtualizationHostSession`
//! and the device around it.

mod codec_lists;
mod decoder;
mod encoder;
mod guest_bounds;
mod jpeg;
mod mapped_surfaces;
mod motion_estimation;
mod names;
mod refused;
mod reply_clerk;
mod session;
mod session_thread;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::backend::{Backend, DecoderBackend};
use crate::encoder_backend::{EncoderBackend, NoEncoders};
use crate::jpeg::JpegDecoder;
use crate::motion::MotionEstimationBackend;
use crate::runtime::Runtime;
use crate::status::{NO_ERROR, OsStatus, Status};
use crate::surfaces::{MappingId, Surfaces};
use crate::sync::Mutex;
use crate::wire::WireError;
use crate::wire::message::{Message, MessageFlags, MessageWriter, read_header};
use crate::wire::surface_boxes::{
    SurfaceList, SurfaceListRejection, surface_list, surface_release,
};

use self::codec_lists::MotionEstimationAnswer;
use self::decoder::CopyEvent;
use self::guest_bounds::HeldMessages;
pub use self::guest_bounds::{
    GUEST_BUFFER_SIZE, MAX_FRAME_MESSAGE_BYTES, MAX_HELD_BYTES, MAX_HELD_BYTES_PER_SESSION,
    MAX_HELD_MESSAGES, MAX_HELD_MESSAGES_PER_SESSION, MAX_RELEASES_PER_MESSAGE, MAX_REPLY_BYTES,
    MAX_SESSIONS,
};
use self::jpeg::JpegOperation;
use self::mapped_surfaces::{MappedSurfaces, SurfaceHold};
use self::names::SessionMessage;
use self::refused::{RefusedKind, RefusedSession};
use self::session::{Created, Ending, SessionEntry, SessionTable};
use self::session_thread::{DetachedThreads, TEARDOWN_WAIT};

/// Where the host's messages to the guest go: the device's queue 1.
///
/// `send` is called with one complete message at a time, from the thread
/// that delivered a guest message or from a session's own thread, and in
/// the order the guest is to see them. It must copy or queue the message
/// and return: it must not wait for the guest, and must not deliver a
/// guest message back into the session on the calling thread.
///
/// Every message is at most [`GUEST_BUFFER_SIZE`], the 0x4000 bytes of the
/// one buffer the guest posts: the session sends none longer. Apple's VM
/// ends itself on a larger one; the device may do the same.
pub trait GuestSink: Send + Sync {
    /// Sends one message: a `vtpv` message, or a `rels` box.
    fn send(&self, message: Vec<u8>);
}

/// The terminate-all message: the 48-byte header of a `Bye!` with no flags
/// and the zero UUID, which a restored device sends the guest so that it
/// drops the sessions the new host does not have. The guest kext copies it
/// to every session it has.
pub fn terminate_all_sessions_message() -> Vec<u8> {
    MessageWriter::new(names::TERMINATE_ALL, MessageFlags::NONE, None, 0).finish()
}

/// What `,hin`'s `VTHostBuildVersion` says of this host. Apple's says the
/// host's macOS build; the guest only logs it. The value is ours.
pub(crate) const HOST_BUILD_VERSION: &str = concat!("reims-vgpu-video ", env!("CARGO_PKG_VERSION"));

/// The state a host session's sessions share with it.
pub(crate) struct HostShared {
    /// The sink, until the session is invalidated. Sends hold this lock,
    /// so none starts after [`HostSession::invalidate`] returns, as
    /// Apple's sends hold its invalidated lock.
    sink: Mutex<Option<Arc<dyn GuestSink>>>,
    surfaces: Arc<dyn Surfaces>,
    backend: Arc<dyn DecoderBackend>,
    encoder_backend: Arc<dyn EncoderBackend>,
    /// The JPEG engine's decoder, if the backend has one.
    jpeg: Option<Arc<dyn JpegDecoder>>,
    /// The buffer the JPEG engine's last output went into, kept for the
    /// next (`host::jpeg`).
    jpeg_output: Mutex<Vec<u8>>,
    /// The motion estimation, if the backend has it.
    motion: Option<Arc<dyn MotionEstimationBackend>>,
    decoders: SessionTable<CopyEvent>,
    encoders: SessionTable,
    motion_estimators: SessionTable,
    mapped: Mutex<MappedSurfaces>,
    /// The whole host session's backstop for the guest's messages its
    /// sessions hold, within [`MAX_HELD_BYTES`] and [`MAX_HELD_MESSAGES`].
    /// Each session holds against a counter of its own over this one, so
    /// one stalled session cannot starve the rest.
    held: Arc<HeldMessages>,
    /// How long a session's end waits for its thread
    /// ([`session_thread::TEARDOWN_WAIT`]).
    teardown_wait: Duration,
    /// The session threads left detached and running: the process's.
    detached: Arc<DetachedThreads>,
    /// Where its sessions' threads come from, and the clock their bounds
    /// and timings are on: the host's ([`crate::runtime`]).
    runtime: Arc<dyn Runtime>,
    /// Whether decoder sessions started from now on let a backend's device
    /// write frames into the guest's buffers itself: on, unless a check
    /// turned it off ([`HostSession::set_zero_copy`]).
    zero_copy: AtomicBool,
}

impl HostShared {
    /// Sends `message` unless the session is invalidated, which is
    /// -19098 with nothing sent, or the message is longer than the guest's
    /// buffers, which is -12902 with nothing sent: the device would end
    /// the VM on it, as Apple's does.
    pub(crate) fn send(&self, message: Vec<u8>) -> Result<(), Status> {
        if message.len() > GUEST_BUFFER_SIZE {
            log::error!(
                "a {}-byte message is longer than the guest's {GUEST_BUFFER_SIZE}-byte buffers, and is not sent",
                message.len()
            );
            return Err(Status::Parameter);
        }
        let sink = self.sink.lock();
        match sink.as_ref() {
            Some(sink) => {
                sink.send(message);
                Ok(())
            }
            None => Err(Status::SessionInvalidated),
        }
    }

    /// Tells the guest which surfaces the host holds no longer: drops the
    /// table's dead entries and sends their IDs in one `rels`, or in as
    /// many as it takes to fit the guest's buffers,
    /// [`MAX_RELEASES_PER_MESSAGE`] each. The table stays locked while the
    /// `rels` goes, so that two threads sweeping at once send in the order
    /// they swept: a decoder's thread sends its reply after its sweep,
    /// never before a `rels` another thread swept first.
    pub(crate) fn relinquish_unused_surfaces(&self) {
        self.let_go(());
    }

    /// Lets go of `holds`, whatever holds surfaces in it, and sweeps: the
    /// holds it lets go of end together, under the table's lock, just
    /// before the sweep ([`mapped_surfaces::ending_together`]), so that no
    /// sweep of another thread finds some of them ended and the rest not,
    /// and their `rels` goes once, after the host has let go of each, as
    /// Apple's sweep after a delivery finds every child its handler let go.
    /// The surfaces themselves are let go first, outside the lock.
    pub(crate) fn let_go<T>(&self, holds: T) {
        let ends = mapped_surfaces::ending_together(|| drop(holds));
        let mut mapped = self.mapped.lock();
        ends.end();
        let released = mapped.sweep();
        for released in released.chunks(MAX_RELEASES_PER_MESSAGE) {
            // Dropped once the session is invalidated, as Apple's is.
            let _ = self.send(surface_release(released));
        }
        drop(mapped);
    }
}

/// A host session: one per device, made when the guest's driver starts the
/// device and invalidated when it resets.
pub struct HostSession {
    shared: Arc<HostShared>,
}

impl HostSession {
    /// A session whose threads and clock are `runtime`'s, that sends
    /// through `sink`, holds guest surfaces through `surfaces` and decodes
    /// with `backend`, and offers no encoders, no motion estimation and no
    /// JPEG engine.
    pub fn new(
        runtime: Arc<dyn Runtime>,
        sink: Arc<dyn GuestSink>,
        surfaces: Arc<dyn Surfaces>,
        backend: Arc<dyn DecoderBackend>,
    ) -> Self {
        Self::with_encoders(runtime, sink, surfaces, backend, Arc::new(NoEncoders))
    }

    /// A session that also encodes, with `encoders`.
    pub fn with_encoders(
        runtime: Arc<dyn Runtime>,
        sink: Arc<dyn GuestSink>,
        surfaces: Arc<dyn Surfaces>,
        backend: Arc<dyn DecoderBackend>,
        encoders: Arc<dyn EncoderBackend>,
    ) -> Self {
        Self::with_parts(runtime, sink, surfaces, backend, encoders, None, None)
    }

    /// A session on `backend`'s decoders, encoders, motion estimation and
    /// JPEG decoder, any of which may be missing: the guest is then offered
    /// none of that part ([`Backend`]).
    pub fn with_backend(
        runtime: Arc<dyn Runtime>,
        sink: Arc<dyn GuestSink>,
        surfaces: Arc<dyn Surfaces>,
        backend: &Backend,
    ) -> Self {
        Self::with_parts(
            runtime,
            sink,
            surfaces,
            backend.decoders_or_none(),
            backend.encoders_or_none(),
            backend.jpeg.clone(),
            backend.motion.clone(),
        )
    }

    fn with_parts(
        runtime: Arc<dyn Runtime>,
        sink: Arc<dyn GuestSink>,
        surfaces: Arc<dyn Surfaces>,
        backend: Arc<dyn DecoderBackend>,
        encoders: Arc<dyn EncoderBackend>,
        jpeg: Option<Arc<dyn JpegDecoder>>,
        motion: Option<Arc<dyn MotionEstimationBackend>>,
    ) -> Self {
        Self {
            shared: Arc::new(HostShared {
                sink: Mutex::new(Some(sink)),
                surfaces,
                backend,
                encoder_backend: encoders,
                jpeg,
                jpeg_output: Mutex::new(Vec::new()),
                motion,
                decoders: Mutex::new(HashMap::new()),
                encoders: Mutex::new(HashMap::new()),
                motion_estimators: Mutex::new(HashMap::new()),
                mapped: Mutex::new(MappedSurfaces::default()),
                held: HeldMessages::backstop(),
                teardown_wait: TEARDOWN_WAIT,
                detached: DetachedThreads::process(),
                runtime,
                zero_copy: AtomicBool::new(true),
            }),
        }
    }

    /// Whether decoder sessions started from now on let a backend's device
    /// write each frame into the guest's buffer itself, where the host
    /// lends that buffer's memory to devices; off, every frame is copied by
    /// the CPU. On by default: whether a driver's writes are trusted is its
    /// backend's to decide, per driver. Off is for the checks that compare
    /// the two, which turn it off through this.
    pub fn set_zero_copy(&self, on: bool) {
        self.shared.zero_copy.store(on, Ordering::Relaxed);
    }

    /// Whether zero copy is on for decoder sessions started from now on
    /// ([`HostSession::set_zero_copy`]).
    pub fn zero_copy(&self) -> bool {
        self.shared.zero_copy.load(Ordering::Relaxed)
    }

    /// The session with its sessions' ends bounded at `wait`, and their
    /// detached threads counted in `detached`, for a test: before any
    /// session is made.
    #[cfg(test)]
    pub(crate) fn with_teardown(mut self, wait: Duration, detached: Arc<DetachedThreads>) -> Self {
        if let Some(shared) = Arc::get_mut(&mut self.shared) {
            shared.teardown_wait = wait;
            shared.detached = detached;
        }
        self
    }

    /// Delivers one queue-0 message, as the device received it, and
    /// returns the status the device writes back.
    ///
    /// A message that names surfaces starts with the guest kext's `subs`
    /// box; the session reads it as Apple's device does: the box's leading
    /// nonzero IDs, and none when the message is shorter than 8 bytes, its
    /// first box is longer than it, or the box is not 48 bytes.
    pub fn deliver(&self, message: &[u8]) -> OsStatus {
        let ids = match surface_list(message) {
            SurfaceList::Surfaces(ids) => ids,
            SurfaceList::None => Vec::new(),
            SurfaceList::Rejected(rejection) => {
                match rejection {
                    SurfaceListRejection::TooShort => {
                        log::error!(
                            "message_length {} too small (minimum 8 bytes)",
                            message.len()
                        );
                    }
                    SurfaceListRejection::FirstBoxPastTheEnd => {
                        log::error!("first box exceeds message_length {}", message.len());
                    }
                    SurfaceListRejection::WrongSize => {
                        log::error!("first box is a subs box but not 48 bytes");
                    }
                }
                Vec::new()
            }
        };
        self.deliver_from_guest(message, &ids)
    }

    /// Delivers one guest message whose surfaces the caller has read: the
    /// message whole, `subs` box included, and the mapping IDs it names.
    pub fn deliver_from_guest(&self, message: &[u8], surfaces: &[MappingId]) -> OsStatus {
        if let Err(error) = read_header(message, 0) {
            // Every delivery ends with the sweep, a malformed one too.
            self.shared.relinquish_unused_surfaces();
            return error.status().code();
        }
        // Each surface is retained outside the table's lock: the mapper's
        // retain waits for the IOSurface device's queue, which calls the
        // host (`mapped_surfaces::hold`).
        let (held, refused) = if surfaces.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            match mapped_surfaces::hold(&self.shared.mapped, &*self.shared.surfaces, surfaces) {
                Ok(held) => (held, Vec::new()),
                Err(made) => (Vec::new(), made),
            }
        };
        // A message one of whose surfaces cannot be held gets none: those
        // held for it are let go here, outside the table's lock, and end
        // in the sweep below.
        let refused = mapped_surfaces::ending_together(|| drop(refused));
        let status = self.dispatch(message, &held);
        // The sweep, with the message's surfaces let go in it, where
        // nothing else holds them now.
        self.shared.let_go((held, refused));
        status
    }

    fn dispatch(&self, bytes: &[u8], surfaces: &[Arc<SurfaceHold>]) -> OsStatus {
        let message = match Message::parse(bytes) {
            Ok(message) => message,
            Err(error) => return error.status().code(),
        };
        let header = message.header();
        let Some(kind) = SessionMessage::from_fourcc(header.message_type) else {
            return self.route(&message, surfaces);
        };
        let shared = &self.shared;
        let answer = |reply: Result<Vec<u8>, WireError>| match reply {
            Ok(reply) => match shared.send(reply) {
                Ok(()) => NO_ERROR,
                Err(status) => status.code(),
            },
            Err(error) => error.status().code(),
        };
        match kind {
            SessionMessage::DecoderList => answer(codec_lists::decoder_list(
                &message,
                &shared.backend.capabilities(),
            )),
            SessionMessage::DecoderCapabilities => answer(codec_lists::decoder_capabilities(
                &message,
                &shared.backend.capabilities(),
            )),
            SessionMessage::EncoderList => answer(codec_lists::encoder_list(
                &message,
                &shared.encoder_backend.encoders(),
            )),
            SessionMessage::MotionEstimationList => {
                match codec_lists::motion_estimation_list(&message, shared.motion.is_some()) {
                    Ok(MotionEstimationAnswer::Reply(reply)) => answer(Ok(reply)),
                    Ok(MotionEstimationAnswer::Refused(status)) => status,
                    Err(error) => error.status().code(),
                }
            }
            SessionMessage::CreateDecoder => self.create(
                &message,
                &shared.decoders,
                RefusedKind::Decoder,
                decoder::create,
            ),
            SessionMessage::CreateEncoder => self.create(
                &message,
                &shared.encoders,
                RefusedKind::Encoder,
                encoder::create,
            ),
            SessionMessage::CreateMotionEstimation => self.create(
                &message,
                &shared.motion_estimators,
                RefusedKind::MotionEstimation,
                motion_estimation::create,
            ),
            SessionMessage::DestroyAll => {
                // Only an internal build answers it, with a `Bye!` and the
                // decoder and encoder sessions dropped; a customer build
                // does nothing. This host is a customer build.
                NO_ERROR
            }
            SessionMessage::DestroyDecoder => {
                session::destroy(&shared.decoders, header.guest_uuid);
                NO_ERROR
            }
            SessionMessage::DestroyEncoder => {
                session::destroy(&shared.encoders, header.guest_uuid);
                NO_ERROR
            }
            SessionMessage::DestroyMotionEstimation => {
                // Its end waits for its thread, as Apple's waits for its
                // frames.
                session::destroy(&shared.motion_estimators, header.guest_uuid);
                NO_ERROR
            }
            SessionMessage::DestroyUuid => {
                session::destroy(&shared.decoders, header.guest_uuid);
                session::destroy(&shared.encoders, header.guest_uuid);
                NO_ERROR
            }
            SessionMessage::JpegCapabilities
            | SessionMessage::JpegDecode
            | SessionMessage::JpegEncode => {
                let operation = match kind {
                    SessionMessage::JpegDecode => JpegOperation::Decode,
                    SessionMessage::JpegEncode => JpegOperation::Encode,
                    _ => JpegOperation::Capabilities,
                };
                let engine = shared.jpeg.as_ref().map(|decoder| jpeg::JpegEngine {
                    decoder,
                    wait: shared.teardown_wait,
                    detached: &shared.detached,
                    output: &shared.jpeg_output,
                    runtime: &shared.runtime,
                });
                match jpeg::answer(&message, operation, surfaces, engine.as_ref()) {
                    Ok(reply) => {
                        // Sent unless invalidated, and noErr either way.
                        let _ = shared.send(reply);
                        NO_ERROR
                    }
                    Err(error) => error.status().code(),
                }
            }
        }
    }

    /// `+dec`, `+enc` or `+mep`: a session of `kind` in `table`, made by
    /// `create`, running when the backend has what the guest asks for and
    /// refused otherwise. A UUID that has one already is -19093 with no
    /// reply, and the table is left as it was: for `+mep`, Apple's host
    /// leaves the table's lock held, and its next lookup ends the VM. Past
    /// [`MAX_SESSIONS`], or while the process has as many session threads
    /// left detached as it keeps, the create is refused for now
    /// ([`HostSession::refuse_for_now`]).
    fn create<E>(
        &self,
        message: &Message<'_>,
        table: &SessionTable<E>,
        kind: RefusedKind,
        create: fn(&Message<'_>, &Arc<HostShared>) -> Created<E>,
    ) -> OsStatus {
        let uuid = message.header().guest_uuid;
        let sessions = {
            let table = table.lock();
            if table.contains_key(&uuid) {
                return Status::MalformedMessage.code();
            }
            table.len()
        };
        if sessions >= MAX_SESSIONS || self.shared.detached.are_full() {
            return self.refuse_for_now(message, kind);
        }
        let (session, reply) = match create(message, &self.shared) {
            Ok(created) => created,
            Err(status) => return status.code(),
        };
        table.lock().insert(uuid, session);
        match self.shared.send(reply) {
            Ok(()) => NO_ERROR,
            Err(status) => status.code(),
        }
    }

    /// A create past [`MAX_SESSIONS`]: the create reply with
    /// [`RefusedKind::not_available_now`], and no session, so that the
    /// table stays within the bound. The guest fails its create on the
    /// reply's `,err`, as for a codec the host lacks; its close then finds
    /// no session, which is noErr. The same while the process has as many
    /// session threads left detached as it keeps ([`session_thread`]).
    fn refuse_for_now(&self, message: &Message<'_>, kind: RefusedKind) -> OsStatus {
        if self.shared.detached.are_full() {
            log::warn!(
                "{kind:?} session refused: {} session threads are left detached in this \
                 process, stuck in their backends",
                self.shared.detached.count()
            );
        } else {
            log::warn!("{kind:?} session refused: this host keeps {MAX_SESSIONS} at most");
        }
        match RefusedSession::reply(message, kind, kind.not_available_now()) {
            Ok(reply) => match self.shared.send(reply) {
                Ok(()) => NO_ERROR,
                Err(status) => status.code(),
            },
            Err(error) => error.status().code(),
        }
    }

    /// A message for a session: decoder first, then encoder, then motion
    /// estimation, and -19094 for a UUID with none.
    fn route(&self, message: &Message<'_>, surfaces: &[Arc<SurfaceHold>]) -> OsStatus {
        let uuid = message.header().guest_uuid;
        if let Some(session) = self.shared.decoders.lock().get(&uuid) {
            return session.deliver(message, surfaces);
        }
        if let Some(session) = self.shared.encoders.lock().get(&uuid) {
            return session.deliver(message, surfaces);
        }
        if let Some(session) = self.shared.motion_estimators.lock().get(&uuid) {
            return session.deliver(message, surfaces);
        }
        Status::SessionNotFound.code()
    }

    /// The first phase of teardown: no message goes to the guest from now
    /// on, and every session stops. It waits for each session's thread to
    /// finish the message in hand. Apple's leaves its motion-estimation
    /// sessions to its second phase; this host stops them here too, so that
    /// none waits for a guest that can no longer answer.
    ///
    /// Every session is told at once, and the wait for all of them is one
    /// bound, 10 s ([`session_thread::TEARDOWN_WAIT`]) on the runtime's
    /// clock; a thread still busy then is left running, detached, and the
    /// reset goes on (ours, for host safety).
    pub fn invalidate(&self) {
        self.shared.sink.lock().take();
        let deadline = self.shared.runtime.deadline_in(self.shared.teardown_wait);
        let shared = &*self.shared;
        let mut invalidating = session::begin_invalidating(&shared.decoders);
        invalidating.extend(session::begin_invalidating(&shared.encoders));
        invalidating.extend(session::begin_invalidating(&shared.motion_estimators));
        for session in invalidating {
            session.wait_until(deadline);
        }
    }

    /// The second phase: every session ends, and the held surfaces are let
    /// go. A surface still held after that is logged, as Apple's host logs
    /// one.
    ///
    /// Every session's queue closes, and the wait for all their threads to
    /// end is one bound, 10 s ([`session_thread::TEARDOWN_WAIT`]) on the
    /// runtime's clock, as the first phase's; a session the first phase
    /// did not reach is invalidated within it too. A thread still busy then
    /// is left running, detached, with what it holds, and the reset goes on
    /// (ours, for host safety).
    pub fn complete_invalidate(&self) {
        let shared = &*self.shared;
        // Out of the tables first, so that each session ends outside their
        // locks.
        let decoders = std::mem::take(&mut *shared.decoders.lock());
        let encoders = std::mem::take(&mut *shared.encoders.lock());
        let motion_estimators = std::mem::take(&mut *shared.motion_estimators.lock());
        let sessions: Vec<&dyn Ending> = (decoders.values().map(SessionEntry::ending))
            .chain(encoders.values().map(SessionEntry::ending))
            .chain(motion_estimators.values().map(SessionEntry::ending))
            .collect();
        session::end_all(&sessions, shared.runtime.deadline_in(shared.teardown_wait));
        drop(sessions);
        drop((decoders, encoders, motion_estimators));
        self.shared.relinquish_unused_surfaces();
        let persisting = self.shared.mapped.lock().len();
        if persisting > 0 {
            log::warn!(
                "{persisting} mapped guest surfaces persist after every session was torn down"
            );
        }
    }
}

impl Drop for HostSession {
    fn drop(&mut self) {
        self.invalidate();
        self.complete_invalidate();
    }
}

#[cfg(test)]
mod bounds_tests;
#[cfg(test)]
mod teardown_tests;
#[cfg(test)]
mod test_queues;
#[cfg(test)]
mod tests;
