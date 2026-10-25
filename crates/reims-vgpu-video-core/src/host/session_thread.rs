//! A session's thread, and how long the session's end waits for it.
//!
//! Each decoder, encoder and motion-estimation session handles its
//! messages on a thread of its own, which the host's runtime starts
//! ([`crate::runtime`]). Its end, at `-dec`, `-enc`, `-mep` or
//! the device's reset, waits on the device's thread, which is QEMU's main
//! loop, in two steps: for that thread to finish the message in hand and
//! let its backend go, and then, its queue closed, for the thread to end,
//! dropping the rest of what it holds. Apple's host waits with no limit,
//! so a backend call that never returned would stop the whole VM.
//!
//! Ours, for host safety: the two steps wait [`TEARDOWN_WAIT`] at most in
//! all, on the runtime's clock. A reset's invalidate waits one bound for
//! the first step of every session it ends, and its complete-invalidate one
//! more for the second. Past it, the thread is left running, detached, with
//! an error logged, and the VM goes on. It keeps what it holds: its
//! backend, the message in hand and those queued behind it, and their
//! guest surfaces, which the guest sees as Busy. A detached thread whose
//! call returns at last drops all of it and ends. While
//! [`MAX_DETACHED_THREADS`] are detached and running in the process, the
//! host makes no new session (`HostSession`'s creates), so that what they
//! hold stays bounded.
//!
//! The JPEG engine's decodes run on such threads too, one for each `jdec`,
//! whose answer the delivering thread waits for with the same bound
//! ([`SessionThread::answer`]). One left running holds its backend and the
//! host's copies of the source and the output, and no guest surface; it
//! counts against the same bound, and while that is reached the engine
//! decodes nothing (`host::jpeg`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::runtime::{self, Answer, Bell, MonoTime, Runtime};
use crate::sync::Mutex;

/// How long a session's end waits for its thread: 10 s, the guest's own
/// longest wait for the host, a codec operation's
/// (`kVTParavirtualizationCodecOperationReplyTimeout`), which a
/// synchronous `dfrm` and a `mepf` wait. Ours.
pub(crate) const TEARDOWN_WAIT: Duration = Duration::from_secs(10);

/// The most session threads the process leaves detached and running
/// before it makes no new session. Each holds a backend session, a
/// session's share of the guest's messages and their surfaces. A backend
/// call that never returns is a fault of the backend or its driver, so a
/// guest past this gets VideoToolbox's "not available now" and works in
/// software. Ours.
pub(crate) const MAX_DETACHED_THREADS: usize = 4;

/// The session threads left detached and still running.
pub(crate) struct DetachedThreads {
    count: AtomicUsize,
    most: usize,
}

impl DetachedThreads {
    /// The process's: every host session counts against one bound, since
    /// a detached thread outlives the host session that made it.
    pub(crate) fn process() -> Arc<Self> {
        static PROCESS: OnceLock<Arc<DetachedThreads>> = OnceLock::new();
        PROCESS
            .get_or_init(|| Arc::new(Self::new(MAX_DETACHED_THREADS)))
            .clone()
    }

    /// A count of its own, with `most` the bound: for a test, whose
    /// detached threads must not count against the rest's.
    pub(crate) fn new(most: usize) -> Self {
        Self {
            count: AtomicUsize::new(0),
            most,
        }
    }

    /// How many are detached and running now.
    pub(crate) fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// Whether the bound is reached.
    pub(crate) fn are_full(&self) -> bool {
        self.count() >= self.most
    }
}

/// Where a session's thread is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Running,
    /// Left running by a teardown that waited its bound.
    Detached,
    Ended,
}

/// What a session's thread and its teardown share.
struct ThreadState {
    /// The session, for the log: "a decoder session", say.
    kind: &'static str,
    phase: Mutex<Phase>,
    /// Rung as the thread ends.
    ended: Bell,
    detached: Arc<DetachedThreads>,
}

impl ThreadState {
    /// Leaves the thread running, detached, if it has not ended: it did
    /// not do `what`, "finish its message" say, in `waited`.
    fn detach(&self, what: &str, waited: Duration) {
        let mut phase = self.phase.lock();
        if *phase == Phase::Running {
            *phase = Phase::Detached;
            let count = self.detached.count.fetch_add(1, Ordering::AcqRel) + 1;
            log::error!(
                "{}'s thread did not {what} in {:.1} s: it is left running, detached, with \
                 its backend and the guest surfaces it holds, and the VM goes on ({count} \
                 detached in this process)",
                self.kind,
                waited.as_secs_f64()
            );
        }
    }

    /// The thread's end, from its last drop.
    fn end(&self) {
        let mut phase = self.phase.lock();
        if *phase == Phase::Detached {
            self.detached.count.fetch_sub(1, Ordering::AcqRel);
            log::warn!(
                "{}'s thread, left running, has ended, and let its backend and surfaces go",
                self.kind
            );
        }
        *phase = Phase::Ended;
        drop(phase);
        self.ended.ring();
    }
}

/// Marks the thread's end when the thread's body returns or unwinds, or
/// where the thread never starts.
struct Ending(Arc<ThreadState>);

impl Drop for Ending {
    fn drop(&mut self) {
        self.0.end();
    }
}

/// A session's thread, which the runtime starts: detached, so that its
/// end is waited for on the runtime's clock, as the thread says it with
/// its last drop.
pub(crate) struct SessionThread {
    state: Arc<ThreadState>,
    /// How long the session's end waits for it alone.
    wait: Duration,
    runtime: Arc<dyn Runtime>,
}

impl SessionThread {
    /// Starts `body` on a thread of `runtime`'s named `name`, for `kind`,
    /// "a decoder session" say, whose end waits `wait` at most, and which
    /// counts against `detached` while it is left running.
    pub(crate) fn spawn(
        runtime: &Arc<dyn Runtime>,
        name: &'static str,
        kind: &'static str,
        wait: Duration,
        detached: &Arc<DetachedThreads>,
        body: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<Self> {
        let state = Arc::new(ThreadState {
            kind,
            phase: Mutex::new(Phase::Running),
            ended: Bell::new(),
            detached: detached.clone(),
        });
        let ending = Ending(state.clone());
        runtime.spawn(
            name,
            Box::new(move || {
                let _ending = ending;
                body();
            }),
        )?;
        Ok(Self {
            state,
            wait,
            runtime: Arc::clone(runtime),
        })
    }

    /// Whether a teardown left it running.
    pub(crate) fn is_detached(&self) -> bool {
        *self.state.phase.lock() == Phase::Detached
    }

    /// The session's invalidate under way: `finished` hears from the
    /// thread when its invalidate is done.
    pub(crate) fn invalidating(&self, finished: Answer<()>) -> Invalidating {
        Invalidating {
            finished,
            state: self.state.clone(),
            started: self.runtime.now(),
            runtime: Arc::clone(&self.runtime),
        }
    }

    /// The end of the thread's whole bound from now, on the runtime's
    /// clock: for a session that ends alone, both its steps.
    pub(crate) fn deadline(&self) -> MonoTime {
        self.runtime.deadline_in(self.wait)
    }

    /// The answer of a thread that makes one call and gives its result on
    /// `answer`, and then its end, waited for the thread's whole bound
    /// together: the JPEG engine's decodes (`host::jpeg`). Past the bound
    /// it is left running, detached, as a session's thread is: with no
    /// answer, or, answered, still ending. A thread that ended with no
    /// answer, by a panic, has none.
    pub(crate) fn answer<T>(&self, answer: &Answer<T>) -> Result<T, Unanswered> {
        let runtime = &*self.runtime;
        let started = runtime.now();
        let deadline = self.deadline();
        match answer.wait_until(runtime, deadline) {
            Ok(answered) => {
                self.join_until(deadline);
                Ok(answered)
            }
            Err(runtime::Unanswered::TimedOut) => {
                self.state
                    .detach("give its answer", runtime.elapsed_since(started));
                Err(Unanswered::Detached)
            }
            Err(runtime::Unanswered::Gone) => {
                self.join_until(deadline);
                Err(Unanswered::Ended)
            }
        }
    }

    /// The second step of a session's end: waits until `deadline`, on the
    /// runtime's clock, for the thread to end, once it has done its
    /// invalidate and its queue is closed; at once for one left running.
    /// Past the deadline the thread is left running, detached, with what
    /// it still holds, until it ends.
    pub(crate) fn join_until(&self, deadline: MonoTime) {
        let state = &*self.state;
        let runtime = &*self.runtime;
        let started = runtime.now();
        let ended = state.ended.wait_until(runtime, deadline, || {
            (*state.phase.lock() != Phase::Running).then_some(())
        });
        if ended.is_none() {
            state.detach("end", runtime.elapsed_since(started));
        }
    }
}

/// Why [`SessionThread::answer`] has no answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unanswered {
    /// The bound passed: the thread is left running, detached.
    Detached,
    /// The thread ended without answering: its call panicked.
    Ended,
}

/// A session's invalidate, sent to its thread, and not yet done.
pub(crate) struct Invalidating {
    finished: Answer<()>,
    state: Arc<ThreadState>,
    started: MonoTime,
    runtime: Arc<dyn Runtime>,
}

impl Invalidating {
    /// Waits for the thread's invalidate until `deadline`, on the
    /// runtime's clock. Past it, the thread is left running, detached. A
    /// thread that has ended, its answer gone, needs no wait.
    pub(crate) fn wait_until(self, deadline: MonoTime) {
        let runtime = &*self.runtime;
        if let Err(runtime::Unanswered::TimedOut) = self.finished.wait_until(runtime, deadline) {
            self.state
                .detach("finish its message", runtime.elapsed_since(self.started));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::virtual_time::VirtualRuntime;
    use crate::runtime::{StdRuntime, answer};

    const BOUND: Duration = Duration::from_millis(50);

    fn thread(
        runtime: &Arc<dyn Runtime>,
        detached: &Arc<DetachedThreads>,
        body: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<SessionThread> {
        SessionThread::spawn(runtime, "test", "a test session", BOUND, detached, body)
    }

    /// A thread that answers in time is joined, and none is detached.
    #[test]
    fn a_thread_that_answers_is_joined() -> std::io::Result<()> {
        let detached = Arc::new(DetachedThreads::new(1));
        let (done, finished) = answer();
        let session = thread(&StdRuntime::shared(), &detached, move || done.give(()))?;
        let deadline = session.deadline();
        session.invalidating(finished).wait_until(deadline);
        assert!(!session.is_detached());
        session.join_until(deadline);
        assert_eq!(*session.state.phase.lock(), Phase::Ended);
        assert_eq!(detached.count(), 0);
        Ok(())
    }

    /// A thread that does not answer is left running once the bound has
    /// passed on the runtime's clock, here virtual, and counted until it
    /// ends.
    #[test]
    fn a_thread_that_does_not_answer_is_detached_until_it_ends() -> std::io::Result<()> {
        let clock = Arc::new(VirtualRuntime::default());
        let runtime: Arc<dyn Runtime> = clock.clone();
        let detached = Arc::new(DetachedThreads::new(1));
        let (release, released) = answer::<()>();
        let (_done, finished) = answer::<()>();
        let session = thread(&runtime, &detached, move || {
            let _ = released.wait_until(&StdRuntime, MonoTime::END);
        })?;
        let invalidating = session.invalidating(finished);
        let deadline = runtime.deadline_in(BOUND);
        clock.advance(BOUND);
        invalidating.wait_until(deadline);
        assert!(session.is_detached());
        assert!(detached.are_full());
        session.join_until(MonoTime::END);
        drop(release);
        let real = StdRuntime;
        let state = &session.state;
        let ended = state
            .ended
            .wait_until(&real, real.deadline_in(Duration::from_secs(5)), || {
                (*state.phase.lock() == Phase::Ended).then_some(())
            });
        assert_eq!(ended, Some(()), "never ended");
        assert_eq!(detached.count(), 0);
        assert!(!session.is_detached());
        Ok(())
    }

    /// A deadline already past still takes an answer that is there.
    #[test]
    fn an_answer_already_given_is_taken_past_the_deadline() -> std::io::Result<()> {
        let clock = Arc::new(VirtualRuntime::default());
        let runtime: Arc<dyn Runtime> = clock.clone();
        let detached = Arc::new(DetachedThreads::new(1));
        let (done, finished) = answer();
        let (release, released) = answer::<()>();
        let (given, gave) = answer();
        let session = thread(&runtime, &detached, move || {
            done.give(());
            given.give(());
            let _ = released.wait_until(&StdRuntime, MonoTime::END);
        })?;
        let invalidating = session.invalidating(finished);
        assert_eq!(gave.wait_until(&StdRuntime, MonoTime::END), Ok(()));
        clock.advance(BOUND);
        invalidating.wait_until(runtime.now());
        assert!(!session.is_detached());
        drop(release);
        session.join_until(MonoTime::END);
        Ok(())
    }

    /// A thread that does its invalidate and then does not end is left
    /// running once the bound has passed, and counted until it ends.
    #[test]
    fn a_thread_that_does_not_end_is_detached_until_it_ends() -> std::io::Result<()> {
        let clock = Arc::new(VirtualRuntime::default());
        let runtime: Arc<dyn Runtime> = clock.clone();
        let detached = Arc::new(DetachedThreads::new(1));
        let (done, finished) = answer();
        let (release, released) = answer::<()>();
        let session = thread(&runtime, &detached, move || {
            done.give(());
            let _ = released.wait_until(&StdRuntime, MonoTime::END);
        })?;
        let deadline = session.deadline();
        session.invalidating(finished).wait_until(deadline);
        assert!(!session.is_detached());
        clock.advance(BOUND);
        session.join_until(deadline);
        assert!(session.is_detached());
        assert_eq!(detached.count(), 1);
        drop(release);
        let real = StdRuntime;
        let state = &session.state;
        let ended = state
            .ended
            .wait_until(&real, real.deadline_in(Duration::from_secs(5)), || {
                (*state.phase.lock() == Phase::Ended).then_some(())
            });
        assert_eq!(ended, Some(()), "never ended");
        assert_eq!(detached.count(), 0);
        Ok(())
    }

    /// The JPEG engine's wait: an answer given and then no end is the
    /// answer, with the thread left running once the one bound has passed.
    #[test]
    fn an_answer_whose_thread_does_not_end_is_taken_and_the_thread_detached() -> std::io::Result<()>
    {
        let clock = Arc::new(VirtualRuntime::default());
        let runtime: Arc<dyn Runtime> = clock.clone();
        let detached = Arc::new(DetachedThreads::new(1));
        let (give, given) = answer();
        let (gave, has_given) = answer::<()>();
        let (release, released) = answer::<()>();
        let session = thread(&runtime, &detached, move || {
            give.give(7);
            gave.give(());
            let _ = released.wait_until(&StdRuntime, MonoTime::END);
        })?;
        assert_eq!(has_given.wait_until(&StdRuntime, MonoTime::END), Ok(()));
        // The clock moves on while the answer's thread is waited for, until
        // the wait has given up on it or 5 s of real time have passed.
        let ticking = {
            let clock = clock.clone();
            let state = session.state.clone();
            StdRuntime::thread("ticking", move || {
                let real = StdRuntime;
                let until = real.deadline_in(Duration::from_secs(5));
                while *state.phase.lock() == Phase::Running && real.now() < until {
                    clock.advance(BOUND);
                    std::thread::sleep(Duration::from_millis(1));
                }
            })?
        };
        assert_eq!(session.answer(&given), Ok(7));
        assert!(session.is_detached());
        let _ = ticking.join();
        drop(release);
        Ok(())
    }
}
