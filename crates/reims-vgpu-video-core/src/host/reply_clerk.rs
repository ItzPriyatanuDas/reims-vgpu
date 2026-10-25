//! The reply clerk: how a host decoder session waits for the guest to
//! answer a message it sent, as Apple's `VTParavirtualizationReplyClerk`
//! does.
//!
//! A message that expects a reply is given the next reply identifier,
//! from 100, before it is sent; the guest's reply carries the identifier
//! back, flagged 0x8, and the host session hands it to the clerk on the
//! delivering thread while the session's own thread waits for it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::host::guest_bounds::MAX_REPLY_BYTES;
use crate::host::mapped_surfaces::SurfaceHold;
use crate::runtime::{Bell, Runtime};
use crate::status::Status;
use crate::sync::Mutex;
use crate::wire::message::{MessageFlags, read_header};

/// The first reply identifier a clerk gives.
const FIRST_REPLY_ID: u64 = 100;

/// How long a request waits by default: 1.1 s
/// (`kVTParavirtualizationDefaultReplyTimeout`).
pub(crate) const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_millis(1100);

/// How long a codec operation waits: 10 s
/// (`kVTParavirtualizationCodecOperationReplyTimeout`).
pub(crate) const CODEC_OPERATION_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// A reply as the clerk hands it over: the whole message, every fragment
/// in order, and the one surface it lent, if it lent exactly one.
pub(crate) struct Reply {
    pub(crate) bytes: Vec<u8>,
    pub(crate) surface: Option<Arc<SurfaceHold>>,
}

#[derive(Default)]
struct PendingReply {
    /// The fragments so far of a reply flagged with more to come.
    partial: Vec<u8>,
    reply: Option<Reply>,
    cancelled: bool,
}

struct ClerkState {
    invalidated: bool,
    next_reply_id: u64,
    pending: HashMap<u64, PendingReply>,
}

/// One session's clerk.
pub(crate) struct ReplyClerk {
    state: Mutex<ClerkState>,
    /// Rung when a reply arrives and when the clerk is invalidated.
    arrived: Bell,
    /// The clock its waits are bounded on.
    runtime: Arc<dyn Runtime>,
}

impl ReplyClerk {
    /// A clerk whose waits are bounded on `runtime`'s clock.
    pub(crate) fn new(runtime: Arc<dyn Runtime>) -> Self {
        Self {
            state: Mutex::new(ClerkState {
                invalidated: false,
                next_reply_id: FIRST_REPLY_ID,
                pending: HashMap::new(),
            }),
            arrived: Bell::new(),
            runtime,
        }
    }

    /// Registers a reply to wait for and returns its identifier, which the
    /// message is to carry. An invalidated clerk takes none: -19095.
    pub(crate) fn prepare(&self) -> Result<u64, Status> {
        let mut state = self.state.lock();
        if state.invalidated {
            return Err(Status::ReplyClerkInvalidated);
        }
        let reply_id = state.next_reply_id;
        state.next_reply_id += 1;
        state.pending.insert(reply_id, PendingReply::default());
        Ok(reply_id)
    }

    /// Waits up to `timeout`, on the runtime's clock, for the reply to
    /// `reply_id`: -19092 when it does not come, and -19095 when the clerk
    /// is invalidated meanwhile.
    pub(crate) fn wait(&self, reply_id: u64, timeout: Duration) -> Result<Reply, Status> {
        let runtime = &*self.runtime;
        let answered = self
            .arrived
            .wait_until(runtime, runtime.deadline_in(timeout), || {
                let mut state = self.state.lock();
                if state.invalidated && !state.pending.contains_key(&reply_id) {
                    return Some(Err(Status::ReplyClerkInvalidated));
                }
                let Some(pending) = state.pending.get_mut(&reply_id) else {
                    return Some(Err(Status::MalformedMessage));
                };
                if pending.cancelled {
                    return Some(Err(Status::ReplyClerkInvalidated));
                }
                pending.reply.take().map(Ok)
            });
        answered.unwrap_or_else(|| {
            log::warn!("timeout waiting for pending reply with identifier {reply_id}");
            Err(Status::ReplyTimedOut)
        })
    }

    /// Forgets `reply_id`, whether or not its reply came: the surface a
    /// reply lent that came once its wait was over, for the caller to let
    /// go of with a sweep.
    pub(crate) fn clean_up(&self, reply_id: u64) -> Option<Arc<SurfaceHold>> {
        let pending = self.state.lock().pending.remove(&reply_id);
        pending
            .and_then(|pending| pending.reply)
            .and_then(|reply| reply.surface)
    }

    /// Takes a reply from the guest. One for an identifier nobody waits
    /// for, or after invalidation, is malformed (-19093) and dropped with
    /// its surface. A fragment flagged with more to come is kept until the
    /// last one. One that takes the reply past [`MAX_REPLY_BYTES`] is
    /// malformed too, and what was gathered of the reply is dropped, so
    /// the wait times out.
    pub(crate) fn deliver(
        &self,
        bytes: &[u8],
        surface: Option<Arc<SurfaceHold>>,
    ) -> Result<(), Status> {
        let header = read_header(bytes, 0).map_err(|error| error.status())?;
        let mut state = self.state.lock();
        if state.invalidated {
            return Err(Status::MalformedMessage);
        }
        let pending = state
            .pending
            .get_mut(&header.reply_id)
            .ok_or(Status::MalformedMessage)?;
        if pending.partial.len() + bytes.len() > MAX_REPLY_BYTES {
            log::warn!("a reply from the guest is longer than {MAX_REPLY_BYTES} bytes");
            pending.partial = Vec::new();
            return Err(Status::MalformedMessage);
        }
        pending.partial.extend_from_slice(bytes);
        if header.flags.contains(MessageFlags::MORE_FRAGMENTS) {
            return Ok(());
        }
        pending.reply = Some(Reply {
            bytes: std::mem::take(&mut pending.partial),
            surface,
        });
        drop(state);
        self.arrived.ring();
        Ok(())
    }

    /// Takes no more replies and wakes every wait, which ends with -19095.
    pub(crate) fn invalidate(&self) {
        let mut state = self.state.lock();
        state.invalidated = true;
        for pending in state.pending.values_mut() {
            pending.cancelled = true;
        }
        drop(state);
        self.arrived.ring();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fourcc::FourCc;
    use crate::runtime::StdRuntime;
    use crate::wire::message::{MessageWriter, read_header};

    fn reply(reply_id: u64, flags: MessageFlags) -> Vec<u8> {
        MessageWriter::new(FourCc::new(*b"Dbuf"), flags, None, reply_id).finish()
    }

    #[test]
    fn identifiers_start_at_100() -> Result<(), Status> {
        let clerk = ReplyClerk::new(StdRuntime::shared());
        assert_eq!(clerk.prepare()?, 100);
        assert_eq!(clerk.prepare()?, 101);
        Ok(())
    }

    #[test]
    fn a_reply_reaches_the_wait_from_another_thread() -> Result<(), Status> {
        let clerk = Arc::new(ReplyClerk::new(StdRuntime::shared()));
        let reply_id = clerk.prepare()?;
        let delivering = {
            let clerk = clerk.clone();
            StdRuntime::thread("delivering", move || {
                clerk.deliver(&reply(reply_id, MessageFlags::REPLY_TO_HOST), None)
            })
            .expect("a thread")
        };
        let received = clerk.wait(reply_id, Duration::from_secs(5))?;
        assert_eq!(
            read_header(&received.bytes, 0).map(|header| header.reply_id),
            Ok(reply_id)
        );
        assert_eq!(delivering.join().ok(), Some(Ok(())));
        Ok(())
    }

    #[test]
    fn fragments_of_a_reply_arrive_as_one() -> Result<(), Status> {
        let clerk = ReplyClerk::new(StdRuntime::shared());
        let reply_id = clerk.prepare()?;
        let first = reply(
            reply_id,
            MessageFlags(MessageFlags::REPLY_TO_HOST.0 | MessageFlags::MORE_FRAGMENTS.0),
        );
        let last = reply(reply_id, MessageFlags::REPLY_TO_HOST);
        clerk.deliver(&first, None)?;
        clerk.deliver(&last, None)?;
        let received = clerk.wait(reply_id, Duration::ZERO)?;
        assert_eq!(received.bytes, [first, last].concat());
        Ok(())
    }

    #[test]
    fn an_unanswered_wait_times_out() -> Result<(), Status> {
        let clerk = ReplyClerk::new(StdRuntime::shared());
        let reply_id = clerk.prepare()?;
        assert_eq!(
            clerk.wait(reply_id, Duration::from_millis(10)).err(),
            Some(Status::ReplyTimedOut)
        );
        Ok(())
    }

    #[test]
    fn a_reply_nobody_waits_for_is_malformed() -> Result<(), Status> {
        let clerk = ReplyClerk::new(StdRuntime::shared());
        let reply_id = clerk.prepare()?;
        clerk.clean_up(reply_id);
        assert_eq!(
            clerk.deliver(&reply(reply_id, MessageFlags::REPLY_TO_HOST), None),
            Err(Status::MalformedMessage)
        );
        Ok(())
    }

    #[test]
    fn invalidation_ends_a_wait_at_once() -> Result<(), Status> {
        let clerk = Arc::new(ReplyClerk::new(StdRuntime::shared()));
        let reply_id = clerk.prepare()?;
        let invalidating = {
            let clerk = clerk.clone();
            StdRuntime::thread("invalidating", move || {
                std::thread::sleep(Duration::from_millis(20));
                clerk.invalidate();
            })
            .expect("a thread")
        };
        let started = StdRuntime.now();
        assert_eq!(
            clerk.wait(reply_id, Duration::from_secs(10)).err(),
            Some(Status::ReplyClerkInvalidated)
        );
        assert!(StdRuntime.elapsed_since(started) < Duration::from_secs(5));
        let _ = invalidating.join();
        assert_eq!(clerk.prepare(), Err(Status::ReplyClerkInvalidated));
        Ok(())
    }
}
