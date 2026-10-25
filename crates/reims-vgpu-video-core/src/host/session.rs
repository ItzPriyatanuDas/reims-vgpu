//! What the decoder, encoder and motion-estimation sessions share.
//!
//! Each `+dec`, `+enc` and `+mep` makes an entry in the host session's
//! table for its kind ([`SessionEntry`]): a session with no codec, which
//! answers nothing ([`RefusedSession`]), or a running one, with a thread of
//! its own, its serial queue ([`SessionQueue`]). The device's thread hands
//! that thread each message and returns at once, and the thread handles
//! them in order. A reply the guest sends the session goes to its reply
//! clerk on the device's thread instead, for the session's thread waiting
//! for it ([`ReplyClerk`]). What the two threads share is the session's
//! [`SessionLink`].

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Weak};
use std::time::Duration;

use crate::fourcc::FourCc;
use crate::host::codec_lists::PROTOCOL_VERSION;
use crate::host::guest_bounds::{Held, HeldMessages};
use crate::host::mapped_surfaces::SurfaceHold;
use crate::host::names;
use crate::host::refused::RefusedSession;
use crate::host::reply_clerk::{Reply, ReplyClerk};
use crate::host::session_thread::{Invalidating, SessionThread};
use crate::host::{HOST_BUILD_VERSION, HostShared};
use crate::runtime::{self, Answerer, MonoTime};
use crate::status::{NO_ERROR, OsStatus, Status};
use crate::sync::Mutex;
use crate::wire::WireError;
use crate::wire::message::{GuestUuid, Message, MessageFlags, MessageHeader, MessageWriter};
use crate::wire::property_list::dictionary;

/// The reply to a `+dec`, `+enc` or `+mep`: `status` in its `,err`, the
/// protocol version, and `,hin`, which says what the host is.
pub(crate) fn create_reply(request: &Message<'_>, status: OsStatus) -> Result<Vec<u8>, WireError> {
    let mut reply = reply(request);
    reply.s32(names::ERROR, status);
    reply.u32(names::VERSION, PROTOCOL_VERSION);
    reply.property_list(
        names::HOST_INFO,
        Some(&plist::Value::Dictionary(dictionary([(
            "VTHostBuildVersion",
            plist::Value::String(HOST_BUILD_VERSION.to_owned()),
        )]))),
    )?;
    Ok(reply.finish())
}

/// The reply to `request`: its type, UUID and reply identifier, flagged as
/// a reply to the guest.
pub(crate) fn reply(request: &Message<'_>) -> MessageWriter {
    MessageWriter::reply_to(&request.header(), MessageFlags::REPLY_TO_GUEST)
}

/// `,key`, which must be a string.
pub(crate) fn property_key(message: &Message<'_>) -> Result<String, WireError> {
    match message.property_list(names::PROPERTY_KEY)? {
        Some(plist::Value::String(key)) => Ok(key),
        _ => Err(WireError::Malformed("property key")),
    }
}

/// A guest reply's `,err`, noErr where it has none, or why there was no
/// reply.
pub(crate) fn reply_error(reply: Result<Reply, Status>) -> OsStatus {
    let reply = match reply {
        Ok(reply) => reply,
        Err(status) => return status.code(),
    };
    match Message::parse(&reply.bytes).and_then(|message| message.s32(names::ERROR)) {
        Ok(status) => status.unwrap_or(NO_ERROR),
        Err(error) => error.status().code(),
    }
}

/// A job for a session's thread, in the order its queue gives them.
pub(crate) enum Job<E = Infallible> {
    /// A message, the surfaces it lent, held until it is handled, and its
    /// share of what the host session holds of the guest's messages.
    Message {
        bytes: Vec<u8>,
        surfaces: Vec<Arc<SurfaceHold>>,
        held: Held,
    },
    /// The session's invalidation: its backend goes, and the answer says
    /// when.
    Invalidate(Answerer<()>),
    /// The session's own: none for an encoder or a motion-estimation
    /// session, and a decoder's copy thread's word.
    Own(E),
}

/// What a session's thread and the device's thread share.
pub(crate) struct SessionLink {
    /// The session's UUID, which each message it sends carries.
    pub(crate) guest_uuid: GuestUuid,
    host: Weak<HostShared>,
    /// Whether the session is invalidated. Sends hold the lock, so none
    /// starts once the session's invalidate has begun.
    invalidated: Mutex<bool>,
    /// The session's waits for the guest's replies.
    pub(crate) clerk: ReplyClerk,
}

impl SessionLink {
    /// The link of `host`'s session for `guest_uuid`.
    pub(crate) fn new(guest_uuid: GuestUuid, host: &Arc<HostShared>) -> Arc<Self> {
        Arc::new(Self {
            guest_uuid,
            host: Arc::downgrade(host),
            invalidated: Mutex::new(false),
            clerk: ReplyClerk::new(host.runtime.clone()),
        })
    }

    /// Whether the session is invalidated.
    pub(crate) fn is_invalidated(&self) -> bool {
        *self.invalidated.lock()
    }

    /// Sends a message unless the session or its host session is
    /// invalidated: -19098.
    pub(crate) fn send(&self, message: Vec<u8>) -> Result<(), Status> {
        let invalidated = self.invalidated.lock();
        if *invalidated {
            return Err(Status::SessionInvalidated);
        }
        let host = self.host.upgrade().ok_or(Status::SessionInvalidated)?;
        host.send(message)
    }

    /// Lets go of `holds` and sweeps, the host session's sweep: `rels` for
    /// the surfaces nothing holds now ([`HostShared::let_go`]).
    pub(crate) fn let_go<T>(&self, holds: T) {
        match self.host.upgrade() {
            Some(host) => host.let_go(holds),
            None => drop(holds),
        }
    }

    /// Sends a `message_type` that expects a reply, with the parameters
    /// `parameters` writes, and waits up to `timeout` for the reply
    /// ([`SessionLink::send_and_wait`]).
    pub(crate) fn request(
        &self,
        message_type: FourCc,
        timeout: Duration,
        parameters: impl FnOnce(&mut MessageWriter),
    ) -> Result<Reply, Status> {
        let mut message = MessageWriter::new(
            message_type,
            MessageFlags::HOST_EXPECTS_REPLY,
            Some(self.guest_uuid),
            0,
        );
        parameters(&mut message);
        self.send_and_wait(message, timeout)
    }

    /// Gives `message` the clerk's next reply identifier, sends it and
    /// waits up to `timeout` for the reply, on the runtime's clock. A reply
    /// that comes as the wait ends lets its surface go at once. An
    /// invalidated session sends nothing: -19095.
    pub(crate) fn send_and_wait(
        &self,
        mut message: MessageWriter,
        timeout: Duration,
    ) -> Result<Reply, Status> {
        let reply_id = self.clerk.prepare()?;
        message.set_reply_id(reply_id);
        let reply = self
            .send(message.finish())
            .and_then(|()| self.clerk.wait(reply_id, timeout));
        let late = self.clerk.clean_up(reply_id);
        if late.is_some() {
            self.let_go(late);
        }
        reply
    }

    /// Marks the session invalidated and cancels its waits for the guest:
    /// false when it was invalidated already.
    fn invalidate(&self) -> bool {
        {
            let mut invalidated = self.invalidated.lock();
            if *invalidated {
                return false;
            }
            *invalidated = true;
        }
        self.clerk.invalidate();
        true
    }
}

/// A kind of session, for its thread and the log.
pub(crate) struct SessionKind {
    /// Its thread's name (ours).
    pub(crate) thread: &'static str,
    /// What the log calls it: "a decoder session", say.
    pub(crate) name: &'static str,
}

/// A running session, on the device's side: the queue to its thread, and
/// the thread.
pub(crate) struct SessionQueue<E = Infallible> {
    link: Arc<SessionLink>,
    /// What the host session holds of the guest's messages.
    held: Arc<HeldMessages>,
    /// The queue, until the session's end closes it.
    jobs: Mutex<Option<Sender<Job<E>>>>,
    thread: SessionThread,
    kind: &'static SessionKind,
    /// Told of each message queued, before the thread can take it up.
    queued: Option<Box<Queued>>,
}

/// What a [`SessionQueue`] tells of each message it queues.
type Queued = dyn Fn(&MessageHeader) + Send + Sync;

impl<E> SessionQueue<E> {
    /// Starts `run` on a thread of the host's runtime for a session of
    /// `kind`, whose jobs `jobs` sends to `run`'s queue. Its end waits the
    /// host's bound for the thread ([`SessionThread`]). A thread that does
    /// not start is -12904.
    pub(crate) fn start(
        link: Arc<SessionLink>,
        host: &Arc<HostShared>,
        kind: &'static SessionKind,
        jobs: Sender<Job<E>>,
        run: impl FnOnce() + Send + 'static,
    ) -> Result<Self, Status> {
        let thread = SessionThread::spawn(
            &host.runtime,
            kind.thread,
            kind.name,
            host.teardown_wait,
            &host.detached,
            run,
        )
        .map_err(|error| {
            log::error!("cannot start {}'s thread: {error}", kind.name);
            Status::AllocationFailed
        })?;
        Ok(Self {
            link,
            held: HeldMessages::per_session(&host.held),
            jobs: Mutex::new(Some(jobs)),
            thread,
            kind,
            queued: None,
        })
    }

    /// The queue, telling `queued` of each message it queues, on the
    /// device's thread, before the session's thread can take it up.
    pub(crate) fn telling(
        mut self,
        queued: impl Fn(&MessageHeader) + Send + Sync + 'static,
    ) -> Self {
        self.queued = Some(Box::new(queued));
        self
    }

    /// A message for the session. A reply (flag 0x8) goes to the clerk at
    /// once, with its surface when it lent exactly one, and its status is
    /// the clerk's; anything else is queued with the surfaces it lent, and
    /// is noErr. A message that would take the host session past what it
    /// holds of the guest's is not queued, and is -12904; one for a session
    /// whose thread is left running, detached, is -19098 (ours).
    fn deliver(&self, message: &Message<'_>, surfaces: &[Arc<SurfaceHold>]) -> OsStatus {
        let header = message.header();
        if header.flags.contains(MessageFlags::REPLY_TO_HOST) {
            let surface = match surfaces {
                [only] => Some(only.clone()),
                _ => None,
            };
            return match self.link.clerk.deliver(message.bytes(), surface) {
                Ok(()) => NO_ERROR,
                Err(status) => status.code(),
            };
        }
        if self.thread.is_detached() {
            // Ours: its thread is stuck in its backend
            // ([`crate::host::session_thread`]), and would hold the
            // message and its surfaces for as long.
            return Status::SessionInvalidated.code();
        }
        if let Some(jobs) = self.jobs.lock().as_ref() {
            let Some(held) = self.held.hold(message.bytes().len()) else {
                log::warn!(
                    "{}'s message is refused: the host holds too many",
                    self.kind.name
                );
                return Status::AllocationFailed.code();
            };
            if let Some(queued) = &self.queued {
                queued(&header);
            }
            let job = Job::Message {
                bytes: message.bytes().to_vec(),
                surfaces: surfaces.to_vec(),
                held,
            };
            if jobs.send(job).is_err() {
                log::warn!("{}'s thread has ended", self.kind.name);
            }
        }
        NO_ERROR
    }

    /// The first phase of the session's end, begun: the session is marked
    /// invalidated, its waits for the guest cancelled first, and its thread
    /// told to let its backend go once the message in hand is done; the
    /// messages queued behind are dropped unhandled. The wait for the
    /// thread is the caller's. None when it was invalidated already, or
    /// its thread has ended.
    ///
    /// Apple's waits for the message in hand with its waits for the guest
    /// still running, and as the guest's replies come through the thread
    /// that waits here, they time out. This cancels them first.
    fn begin_invalidate(&self) -> Option<Invalidating> {
        if !self.link.invalidate() {
            return None;
        }
        let (done, finished) = runtime::answer();
        let jobs = self.jobs.lock();
        // An Err is a thread that has already ended.
        let told = jobs
            .as_ref()
            .is_some_and(|jobs| jobs.send(Job::Invalidate(done)).is_ok());
        told.then(|| self.thread.invalidating(finished))
    }

    /// The second phase: the queue closes, and the session's thread ends,
    /// and what it held with it, waited for until `deadline` on the
    /// runtime's clock. Past it the thread is left running, detached, and
    /// keeps what it holds until it ends.
    fn complete_invalidate(&self, deadline: MonoTime) {
        self.close();
        self.thread.join_until(deadline);
    }

    /// Closes the queue: the session's thread ends once it has handled
    /// what is queued.
    fn close(&self) {
        self.jobs.lock().take();
    }
}

impl<E> Drop for SessionQueue<E> {
    /// The session's end on its own, at its close: both phases, within
    /// the session's one bound for its thread, past which the thread is
    /// left running, detached, with what it holds, and the end goes on
    /// (ours, for host safety: [`crate::host::session_thread`]). After
    /// [`end_all`] there is nothing left to wait for.
    fn drop(&mut self) {
        let deadline = self.thread.deadline();
        if let Some(invalidating) = self.begin_invalidate() {
            invalidating.wait_until(deadline);
        }
        self.complete_invalidate(deadline);
    }
}

/// A `+dec`, `+enc` or `+mep`'s session, in its table: one with no codec,
/// or a running one.
pub(crate) enum SessionEntry<E = Infallible> {
    /// No codec or processor the guest asked for.
    Refused(RefusedSession),
    /// A running session.
    Running(SessionQueue<E>),
}

impl<E> SessionEntry<E> {
    /// A message for the session, with the surfaces it lent.
    pub(crate) fn deliver(&self, message: &Message<'_>, surfaces: &[Arc<SurfaceHold>]) -> OsStatus {
        match self {
            Self::Refused(session) => session.deliver(message),
            Self::Running(session) => session.deliver(message, surfaces),
        }
    }

    /// The first phase of teardown, begun: the wait for the session's
    /// thread, if it has one ([`SessionQueue::begin_invalidate`]).
    pub(crate) fn begin_invalidate(&self) -> Option<Invalidating> {
        match self {
            Self::Refused(_) => None,
            Self::Running(session) => session.begin_invalidate(),
        }
    }

    /// The session's end, step by step, beside other kinds' ([`end_all`]).
    pub(crate) fn ending(&self) -> &dyn Ending {
        self
    }
}

/// A session's end, step by step, whatever its kind: for one wait over the
/// sessions of every kind ([`end_all`]).
pub(crate) trait Ending {
    /// The first phase, begun ([`SessionEntry::begin_invalidate`]).
    fn begin_invalidate(&self) -> Option<Invalidating>;
    /// The queue's close, for a session that has one
    /// ([`SessionQueue::close`]).
    fn close(&self);
    /// The wait for the thread's end, until `deadline`, for a session that
    /// has one ([`SessionThread::join_until`]).
    fn join_until(&self, deadline: MonoTime);
}

impl<E> Ending for SessionEntry<E> {
    fn begin_invalidate(&self) -> Option<Invalidating> {
        SessionEntry::begin_invalidate(self)
    }

    fn close(&self) {
        if let Self::Running(session) = self {
            session.close();
        }
    }

    fn join_until(&self, deadline: MonoTime) {
        if let Self::Running(session) = self {
            session.thread.join_until(deadline);
        }
    }
}

/// A create's session and its reply, or why there is neither.
pub(crate) type Created<E = Infallible> = Result<(SessionEntry<E>, Vec<u8>), Status>;

/// A host session's sessions of one kind, by UUID.
pub(crate) type SessionTable<E = Infallible> = Mutex<HashMap<GuestUuid, SessionEntry<E>>>;

/// Removes `uuid`'s session from `table`, if it has one, and ends it
/// outside the table's lock: its end waits for its thread.
pub(crate) fn destroy<E>(table: &SessionTable<E>, uuid: GuestUuid) {
    let removed = table.lock().remove(&uuid);
    drop(removed);
}

/// Ends `sessions`, taken out of their tables, within `deadline` on the
/// runtime's clock: the host session's complete-invalidate. Those its
/// invalidate has not reached are invalidated first, and their threads
/// waited for to finish the message in hand; then every queue closes, and
/// every thread is waited for to end. A thread still busy at `deadline` is
/// left running, detached, with what it holds. Each queue closes before
/// any wait, so that no thread's end waits on another's. The sessions'
/// drops then have nothing left to wait for.
pub(crate) fn end_all(sessions: &[&dyn Ending], deadline: MonoTime) {
    let invalidating: Vec<Invalidating> = sessions
        .iter()
        .filter_map(|session| session.begin_invalidate())
        .collect();
    for session in invalidating {
        session.wait_until(deadline);
    }
    for session in sessions {
        session.close();
    }
    for session in sessions {
        session.join_until(deadline);
    }
}

/// Begins the first phase of the end of every session in `table`, and
/// gives the waits for their threads ([`SessionEntry::begin_invalidate`]).
pub(crate) fn begin_invalidating<E>(table: &SessionTable<E>) -> Vec<Invalidating> {
    table
        .lock()
        .values()
        .filter_map(SessionEntry::begin_invalidate)
        .collect()
}
