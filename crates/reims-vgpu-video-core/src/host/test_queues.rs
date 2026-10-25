//! The device's two queues as the host session's tests drive them: a
//! guest's handler thread takes queue 1 through [`CountingSink`], and a
//! device thread delivers queue 0 in order. [`Progress`] says how far each
//! has got, so that a test reads the guest's record once what it reads has
//! reached it.
//!
//! A delivery sweeps once its message is handled, so the `rels` its sweep
//! sends comes after the message's reply: a round trip's reply does not say
//! that its delivery's `rels` has reached the guest, and a test that reads
//! the record at the reply can miss it, or find it in its next read.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::host::GuestSink;
use crate::runtime::{Runtime, StdRuntime};
use crate::sync::Mutex;
use crate::wire::message::{MessageFlags, read_header};

/// How far the queues have got.
#[derive(Default)]
pub(crate) struct Progress {
    /// The host's messages sent to the guest.
    sent: AtomicU64,
    /// Those the guest's handler has handled.
    handled: AtomicU64,
    /// The highest reply ID of a guest's message the device thread has
    /// delivered whole, its sweep included. The tests number their messages
    /// in the order they send them, and the device thread delivers them in
    /// that order.
    delivered: AtomicU64,
}

impl Progress {
    /// The guest's handler has handled one more of the host's messages.
    pub(crate) fn handled_one(&self) {
        self.handled.fetch_add(1, Ordering::SeqCst);
    }

    /// The device thread has delivered `message` whole.
    pub(crate) fn delivered(&self, message: &[u8]) {
        if let Ok(header) = read_header(message, 0)
            && header.flags.contains(MessageFlags::GUEST_EXPECTS_REPLY)
        {
            self.delivered.fetch_max(header.reply_id, Ordering::SeqCst);
        }
    }

    /// Waits, `wait` at most, until the device thread has delivered the
    /// guest's message `reply_id` whole, its sweep included, and then
    /// until the guest's handler has handled what the host sent until
    /// then: whether both came within the wait.
    pub(crate) fn settled(&self, reply_id: u64, wait: Duration) -> bool {
        let started = StdRuntime.now();
        while self.delivered.load(Ordering::SeqCst) < reply_id {
            if StdRuntime.elapsed_since(started) >= wait {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        self.caught_up(wait.saturating_sub(StdRuntime.elapsed_since(started)))
    }

    /// Waits, `wait` at most, until the guest's handler has handled every
    /// message the host has sent so far: whether it did.
    pub(crate) fn caught_up(&self, wait: Duration) -> bool {
        let sent = self.sent.load(Ordering::SeqCst);
        let started = StdRuntime.now();
        while self.handled.load(Ordering::SeqCst) < sent {
            if StdRuntime.elapsed_since(started) >= wait {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        true
    }
}

/// Queue 1: each of the host's messages into the guest's channel, counted
/// in its [`Progress`].
pub(crate) struct CountingSink {
    to_guest: Mutex<Sender<Vec<u8>>>,
    progress: Arc<Progress>,
}

impl CountingSink {
    pub(crate) fn new(to_guest: Sender<Vec<u8>>, progress: Arc<Progress>) -> Self {
        Self {
            to_guest: Mutex::new(to_guest),
            progress,
        }
    }
}

impl GuestSink for CountingSink {
    fn send(&self, message: Vec<u8>) {
        self.progress.sent.fetch_add(1, Ordering::SeqCst);
        // A closed channel is a guest that is gone.
        let _ = self.to_guest.lock().send(message);
    }
}
