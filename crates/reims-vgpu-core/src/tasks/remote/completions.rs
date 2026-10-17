//! The remote task's outstanding completions: one [`Token`] per reply or GPU completion a FIFO
//! thread waits on, in a table ([`Completions`]) keyed by ids that ascend from 1.
//!
//! The service's reply to a message, or its `completeAsyncTokenID:` message for a GPU
//! completion, completes a token; the task's death completes every outstanding token with
//! [`ExecStatus::CRASHED`], and every token issued after it is born crashed. This is the one
//! piece of status-4 code every launcher shares. The first token born crashed writes one warning
//! naming the task, so a storm of faults (each a GPU restart in the guest) points at its death.
//!
//! Ordering contracts (each named test pins it):
//! - **Crash**: every outstanding token signals 4 in ascending id order, under the table lock;
//!   the table then empties and the drain wakes. A token issued afterwards is born 4, its handler
//!   run on the issuing thread under the table lock
//!   (`crash_signals_in_ascending_order_under_the_lock`,
//!   `a_handler_after_a_crash_runs_at_once_under_the_table_lock`).
//! - **Destroy order**: the table ([`TokenTree`]) drops the tokens it holds (each releasing its
//!   handler, and what the handler holds) in ascending id order
//!   (`crash_releases_handlers_in_ascending_order`, `table_drop_releases_in_ascending_order`).
//! - **Complete** takes the token out of the table under the table lock (waking the drain when
//!   the table is then empty, whether or not an id was found), and signals it after that lock is
//!   released; a token no longer in the table is ignored (`by_id_removes_then_signals_unlocked`,
//!   `by_token_ignored_once_removed`).
//! - **Signal** stores the status and runs the handler under the token's own lock, then wakes
//!   the waiters; a second signal of one token ends the process (`a_second_signal_dies`).
//! - **Waits have no timeout**: a token completed with 0 (not complete) never ends its wait, and
//!   the drain waits until the table is empty.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::platform::types::NsErrorText;
use crate::tasks::ExecStatus;
use crate::tasks::remote::DeathKind;

/// What a token runs when it is signalled: the status, and an error that is always `None`.
pub type TokenHandler = Box<dyn Fn(ExecStatus, Option<NsErrorText>) + Send + Sync>;

/// The production primitives: `parking_lot`'s.
#[cfg(not(loom))]
mod imp {
    pub(super) use parking_lot::{Condvar, Mutex, MutexGuard as Guard};

    /// Locks `m`.
    pub(super) fn lock<T>(m: &Mutex<T>) -> Guard<'_, T> {
        m.lock()
    }

    /// One wait on `cv`, no predicate (the callers loop).
    pub(super) fn wait<'a, T>(cv: &Condvar, mut g: Guard<'a, T>) -> Guard<'a, T> {
        cv.wait(&mut g);
        g
    }
}

/// `loom`'s primitives, for the model check.
#[cfg(loom)]
mod imp {
    pub(super) use loom::sync::{Condvar, Mutex, MutexGuard as Guard};

    /// Locks `m`. Nothing panics under it in the model, so it never poisons.
    pub(super) fn lock<T>(m: &Mutex<T>) -> Guard<'_, T> {
        m.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// One wait on `cv`.
    pub(super) fn wait<'a, T>(cv: &Condvar, g: Guard<'a, T>) -> Guard<'a, T> {
        cv.wait(g).unwrap_or_else(|p| p.into_inner())
    }
}

use imp::{Condvar, Mutex};

/// One outstanding completion, shared by the table, the waiter and each reply that completes
/// it. Dropping the last reference releases its handler.
pub struct Token {
    status: Mutex<ExecStatus>,
    cond: Condvar,
    id: u64,
    handler: Option<TokenHandler>,
}

impl Token {
    /// A token with no handler, not complete.
    fn new(id: u64, handler: Option<TokenHandler>) -> Token {
        Token {
            status: Mutex::new(ExecStatus::NOT_COMPLETE),
            cond: Condvar::new(),
            id,
            handler,
        }
    }

    /// The token's id: what crosses the wire for a GPU completion.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Stores `status`, runs the handler with it under the token's lock, and wakes every waiter.
    /// A token already signalled ends the process before the store.
    pub fn signal(&self, status: ExecStatus) {
        let mut st = imp::lock(&self.status);
        crate::fatal_unless!(
            *st == ExecStatus::NOT_COMPLETE,
            DeathKind::TokenSignalledTwice
        );
        *st = status;
        if let Some(handler) = &self.handler {
            handler(status, None);
        }
        self.cond.notify_all();
    }

    /// Waits, with no timeout, until the token is complete, and returns its status.
    pub fn wait(&self) -> ExecStatus {
        let mut st = imp::lock(&self.status);
        while *st == ExecStatus::NOT_COMPLETE {
            st = imp::wait(&self.cond, st);
        }
        *st
    }
}

/// The table of outstanding tokens, the crash latch and the drain.
pub struct Completions {
    state: Mutex<State>,
    drained: Condvar,
    /// The task the tokens are for, named by the warning of the first token born crashed.
    task_id: Option<u32>,
}

/// What the table's lock guards.
struct State {
    tokens: TokenTree,
    /// The next token's id; ids start at 1.
    next_id: u64,
    /// Set by the crash, never cleared.
    crashed: bool,
    /// Set when the first token born crashed has written its warning.
    warned: bool,
}

/// The table's tokens, keyed by id. It drops the tokens it still holds in ascending id order.
struct TokenTree {
    tokens: BTreeMap<u64, Arc<Token>>,
}

impl TokenTree {
    fn new() -> TokenTree {
        TokenTree {
            tokens: BTreeMap::new(),
        }
    }

    /// Inserts `token` under `key`, a key not present (the ids are unique).
    fn insert(&mut self, key: u64, token: Arc<Token>) {
        self.tokens.entry(key).or_insert(token);
    }

    /// Takes the token under `key` out and hands it back, for the caller to drop. `None` if
    /// absent.
    fn erase(&mut self, key: u64) -> Option<Arc<Token>> {
        self.tokens.remove(&key)
    }

    /// The tokens in ascending id order.
    fn in_order(&self) -> impl Iterator<Item = &Arc<Token>> {
        self.tokens.values()
    }

    /// The keys in ascending order.
    #[cfg(all(test, not(loom)))]
    fn keys(&self) -> Vec<u64> {
        self.tokens.keys().copied().collect()
    }

    /// Empties the table and yields its tokens in ascending id order; dropping the `Vec` front
    /// to back drops them in that order.
    fn clear(&mut self) -> Vec<Arc<Token>> {
        std::mem::take(&mut self.tokens).into_values().collect()
    }

    /// The number of tokens.
    #[cfg(all(test, not(loom)))]
    fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Whether the table holds no token.
    fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }
}

impl Drop for TokenTree {
    /// The tokens still held go in ascending id order.
    fn drop(&mut self) {
        drop(self.clear());
    }
}

impl Default for Completions {
    fn default() -> Completions {
        Completions::new()
    }
}

impl Completions {
    /// An empty table: first id 1, not crashed, for no task in particular.
    pub fn new() -> Completions {
        Completions::with_task_id(None)
    }

    /// An empty table for the task `task_id`, which its crash warning names.
    pub fn for_task(task_id: u32) -> Completions {
        Completions::with_task_id(Some(task_id))
    }

    fn with_task_id(task_id: Option<u32>) -> Completions {
        Completions {
            state: Mutex::new(State {
                tokens: TokenTree::new(),
                next_id: 1,
                crashed: false,
                warned: false,
            }),
            drained: Condvar::new(),
            task_id,
        }
    }

    /// A new token, with `handler` if given: in the table, or, after a crash, signalled 4 here
    /// (its handler run on this thread) under the table lock. The id advances either way. The
    /// first token born crashed writes one warning.
    fn issue_token(&self, handler: Option<TokenHandler>) -> Arc<Token> {
        let mut st = imp::lock(&self.state);
        let id = st.next_id;
        st.next_id = id.wrapping_add(1);
        let token = Arc::new(Token::new(id, handler));
        if st.crashed {
            if !st.warned {
                st.warned = true;
                self.warn_born_crashed();
            }
            token.signal(ExecStatus::CRASHED);
        } else {
            st.tokens.insert(id, token.clone());
        }
        token
    }

    /// The one warning of a task whose service has ended and which is still given work: every
    /// later command faults with 4 and nothing else is logged for it.
    fn warn_born_crashed(&self) {
        match self.task_id {
            Some(task_id) => log::warn!(
                "task {task_id}: its service has ended: this command and every later one fault (4)"
            ),
            None => {
                log::warn!("a task's service has ended: this command and every later one fault (4)")
            }
        }
    }

    /// A new token with no handler (a message whose sender waits for the answer).
    pub fn issue(&self) -> Arc<Token> {
        self.issue_token(None)
    }

    /// A new token that runs `handler` when it is signalled.
    pub fn issue_with(&self, handler: TokenHandler) -> Arc<Token> {
        self.issue_token(Some(handler))
    }

    /// Completes the token `id` names with `status`, unvalidated. An id not in the table is
    /// ignored; the drain is woken whenever the table is empty afterwards, found or not.
    pub fn complete_id(&self, id: u64, status: ExecStatus) {
        let token = {
            let mut st = imp::lock(&self.state);
            let token = st.tokens.erase(id);
            if st.tokens.is_empty() {
                self.drained.notify_one();
            }
            token
        };
        if let Some(token) = token {
            token.signal(status);
        }
    }

    /// Completes `token` with `status`: ignored once it has left the table (completed, or
    /// crashed), with no signal and no wake.
    pub fn complete(&self, token: &Arc<Token>, status: ExecStatus) {
        {
            let mut st = imp::lock(&self.state);
            // The table's reference goes under the table lock.
            let Some(erased) = st.tokens.erase(token.id) else {
                return;
            };
            drop(erased);
            if st.tokens.is_empty() {
                self.drained.notify_one();
            }
        }
        token.signal(status);
    }

    /// The task died: latches the crash, signals every outstanding token 4 in ascending id
    /// order, drops the table's references in the same order and wakes every drain, all under
    /// the table lock. A second crash finds nothing to signal.
    pub fn crash(&self) {
        let mut st = imp::lock(&self.state);
        st.crashed = true;
        for token in st.tokens.in_order() {
            token.signal(ExecStatus::CRASHED);
        }
        drop(st.tokens.clear());
        self.drained.notify_all();
    }

    /// Waits, with no timeout, until no token is outstanding.
    pub fn drain(&self) {
        let mut st = imp::lock(&self.state);
        while !st.tokens.is_empty() {
            st = imp::wait(&self.drained, st);
        }
    }
}

#[cfg(all(test, not(loom)))]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)] // A failure is the assertion
mod tests {
    use super::*;
    use crate::raise::death::DeathSite;
    use crate::tasks::remote::DeathKind as RemoteDeath;
    use crate::testkit::capture::catch_death;
    use crate::testkit::capture::expect_death;
    use std::sync::mpsc;
    use std::time::Duration;

    const SIGNALLED_TWICE: DeathSite = RemoteDeath::TokenSignalledTwice.site();

    /// Every call a recorder handler saw, as `(status, error present)`.
    type Calls = Arc<parking_lot::Mutex<Vec<(u32, bool)>>>;

    /// A handler that records every call.
    fn recorder() -> (TokenHandler, Calls) {
        let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let l = log.clone();
        let handler: TokenHandler = Box::new(move |s, e| l.lock().push((s.raw(), e.is_some())));
        (handler, log)
    }

    /// A token as the table makes it.
    fn token(id: u64, handler: Option<TokenHandler>) -> Token {
        Token::new(id, handler)
    }

    /// Whether `f` is still blocked after a real-time settle (the waits have no clock).
    fn blocks(f: impl FnOnce() + Send + 'static) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        #[allow(clippy::disallowed_methods)] // a test thread plays the waiter
        std::thread::spawn(move || {
            f();
            let _ = tx.send(());
        });
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        rx
    }

    mod token {
        use super::*;

        /// A plain token starts not complete, with no handler
        #[test]
        fn plain_token_starts_not_complete_without_handler() {
            let t = token(7, None);
            assert_eq!(t.id(), 7);
            assert!(t.handler.is_none());
            assert_eq!(*t.status.lock(), ExecStatus::NOT_COMPLETE);
        }

        /// A token with a handler runs it only when signalled
        #[test]
        fn a_token_keeps_its_handler() {
            let (b, log) = recorder();
            let t = token(9, Some(b));
            assert_eq!(t.id(), 9);
            assert_eq!(*t.status.lock(), ExecStatus::NOT_COMPLETE);
            assert!(log.lock().is_empty());
            t.signal(ExecStatus::OK);
            assert_eq!(*log.lock(), vec![(1, false)]);
        }

        /// The handler is released with the last reference
        #[test]
        fn drop_releases_the_handler() {
            let marker = Arc::new(());
            let m = marker.clone();
            let t = token(
                1,
                Some(Box::new(move |_, _| {
                    let _ = &m;
                })),
            );
            assert_eq!(Arc::strong_count(&marker), 2);
            drop(t);
            assert_eq!(Arc::strong_count(&marker), 1);
        }
    }

    mod signal {
        use super::*;

        #[test]
        fn stores_runs_the_handler_with_no_error_and_wakes() {
            let (b, log) = recorder();
            let t = Arc::new(token(1, Some(b)));
            let t2 = t.clone();
            let rx = blocks(move || assert_eq!(t2.wait(), ExecStatus::GPU_OR_TIMEOUT));
            t.signal(ExecStatus::GPU_OR_TIMEOUT);
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(*log.lock(), vec![(3, false)]);
            assert_eq!(*t.status.lock(), ExecStatus::GPU_OR_TIMEOUT);
        }

        /// The handler runs with the token lock held
        #[test]
        fn the_handler_runs_under_the_token_lock() {
            let slot: Arc<parking_lot::Mutex<Option<Arc<Token>>>> =
                Arc::new(parking_lot::Mutex::new(None));
            let s = slot.clone();
            let held = Arc::new(parking_lot::Mutex::new(None));
            let h = held.clone();
            let t = Arc::new(token(
                1,
                Some(Box::new(move |_, _| {
                    let t = s.lock().clone().unwrap();
                    *h.lock() = Some(t.status.try_lock().is_none());
                })),
            ));
            *slot.lock() = Some(t.clone());
            t.signal(ExecStatus::OK);
            assert_eq!(*held.lock(), Some(true));
            *slot.lock() = None;
        }

        /// A second signal dies before it stores
        #[test]
        fn a_second_signal_dies() {
            let t = token(1, None);
            t.signal(ExecStatus::OK);
            assert_eq!(
                catch_death(|| t.signal(ExecStatus::FAIL)),
                Err(SIGNALLED_TWICE)
            );
            // The status stays the first one: the check precedes the store.
            assert_eq!(*t.status.lock(), ExecStatus::OK);
        }

        /// A "not complete" signal leaves it signallable
        #[test]
        fn signal_zero_stores_zero() {
            let (b, log) = recorder();
            let t = token(1, Some(b));
            t.signal(ExecStatus::NOT_COMPLETE);
            t.signal(ExecStatus::OK);
            assert_eq!(*log.lock(), vec![(0, false), (1, false)]);
        }
    }

    mod wait {
        use super::*;

        /// Already complete: returns at once
        #[test]
        fn returns_status_when_complete() {
            let t = token(1, None);
            t.signal(ExecStatus(0x1234));
            assert_eq!(t.wait(), ExecStatus(0x1234));
            assert_eq!(t.wait(), ExecStatus(0x1234));
        }

        /// Blocks with no timeout until signalled
        #[test]
        fn blocks_until_signalled() {
            let t = Arc::new(token(1, None));
            let t2 = t.clone();
            let rx = blocks(move || assert_eq!(t2.wait(), ExecStatus::CRASHED));
            assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
            t.signal(ExecStatus::CRASHED);
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
    }

    mod issue {
        use super::*;

        /// Ids from 1, registered
        #[test]
        fn ids_ascend_from_one_and_register() {
            let a = Completions::new();
            let t1 = a.issue();
            let (b, _) = recorder();
            let t2 = a.issue_with(b);
            let t3 = a.issue();
            assert_eq!((t1.id(), t2.id(), t3.id()), (1, 2, 3));
            let ids: Vec<u64> = a.state.lock().tokens.keys();
            assert_eq!(ids, vec![1, 2, 3]);
            assert_eq!(*t1.status.lock(), ExecStatus::NOT_COMPLETE);
            assert_eq!(Completions::new().issue().id(), 1);
        }

        /// After a crash: born signalled 4, not registered, the id still advances
        #[test]
        fn after_crash_born_signalled_four() {
            let a = Completions::new();
            let _ = a.issue();
            a.crash();
            let t = a.issue();
            assert_eq!(t.id(), 2);
            assert_eq!(t.wait(), ExecStatus::CRASHED);
            assert!(a.state.lock().tokens.is_empty());
            assert_eq!(a.issue().id(), 3);
        }

        /// After a crash: the first token born crashed warns once, naming the task; a crash
        /// with nothing issued after it, and the tokens it completes, write nothing
        #[test]
        fn the_first_token_born_crashed_warns_once() {
            let lines = crate::tasks::test_support::Lines::capture();
            let a = Completions::for_task(7);
            let t = a.issue();
            a.crash();
            assert_eq!(t.wait(), ExecStatus::CRASHED);
            assert!(lines.take().is_empty());
            let _ = a.issue();
            let _ = a.issue();
            assert_eq!(
                lines.take(),
                ["task 7: its service has ended: this command and every later one fault (4)"]
            );
            Completions::new().crash();
            assert!(lines.take().is_empty());
        }

        /// With a handler, after a crash: the handler runs with 4 on the issuing thread, under
        /// the table lock, before the call returns
        #[test]
        fn a_handler_after_a_crash_runs_at_once_under_the_table_lock() {
            let a = Arc::new(Completions::new());
            a.crash();
            let me = std::thread::current().id();
            let seen = Arc::new(parking_lot::Mutex::new(None));
            let (s, a2) = (seen.clone(), a.clone());
            let t = a.issue_with(Box::new(move |st, e| {
                *s.lock() = Some((
                    st,
                    e.is_none(),
                    std::thread::current().id() == me,
                    a2.state.try_lock().is_none(),
                ));
            }));
            assert_eq!(*seen.lock(), Some((ExecStatus::CRASHED, true, true, true)));
            assert_eq!(t.wait(), ExecStatus::CRASHED);
            assert!(a.state.lock().tokens.is_empty());
        }

        /// Before a crash a token with a handler is registered, not run
        #[test]
        fn a_handler_before_a_crash_waits() {
            let a = Completions::new();
            let (b, log) = recorder();
            let t = a.issue_with(b);
            assert!(log.lock().is_empty());
            a.complete(&t, ExecStatus::OK);
            assert_eq!(*log.lock(), vec![(1, false)]);
        }
    }

    mod complete {
        use super::*;

        /// By id: removes, then signals outside the table lock
        #[test]
        fn by_id_removes_then_signals_unlocked() {
            let a = Arc::new(Completions::new());
            let a2 = a.clone();
            let locked = Arc::new(parking_lot::Mutex::new(None));
            let l = locked.clone();
            let t = a.issue_with(Box::new(move |_, _| {
                let was_locked = a2.state.try_lock().is_none();
                let left = a2.state.lock().tokens.len();
                *l.lock() = Some((was_locked, left));
            }));
            let _other = a.issue();
            a.complete_id(t.id(), ExecStatus::FAIL);
            assert_eq!(*locked.lock(), Some((false, 1)));
            assert_eq!(t.wait(), ExecStatus::FAIL);
        }

        /// By id: an unknown id is ignored, as a late completion finding its token gone is
        #[test]
        fn by_id_unknown_or_repeated_is_ignored() {
            let a = Completions::new();
            let (b, log) = recorder();
            let t = a.issue_with(b);
            a.complete_id(99, ExecStatus::OK);
            assert_eq!(a.state.lock().tokens.len(), 1);
            a.complete_id(t.id(), ExecStatus::FAIL);
            a.complete_id(t.id(), ExecStatus::OK);
            assert_eq!(*log.lock(), vec![(2, false)]);
        }

        /// By id: the drain is notified when empty, found or not
        #[test]
        fn by_id_unknown_on_empty_table_wakes_drain() {
            let a = Arc::new(Completions::new());
            let t = a.issue();
            let a2 = a.clone();
            let rx = blocks(move || a2.drain());
            // Remove it behind the table's back, as a racing path would leave it.
            drop(a.state.lock().tokens.clear());
            a.complete_id(12345, ExecStatus::OK);
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(*t.status.lock(), ExecStatus::NOT_COMPLETE);
        }

        /// By token: removes, signals
        #[test]
        fn by_token_signals() {
            let a = Completions::new();
            let t = a.issue();
            let _u = a.issue();
            a.complete(&t, ExecStatus(7));
            assert_eq!(t.wait(), ExecStatus(7));
            let ids: Vec<u64> = a.state.lock().tokens.keys();
            assert_eq!(ids, vec![2]);
        }

        /// By token: ignored once removed, so no double signal
        #[test]
        fn by_token_ignored_once_removed() {
            let a = Completions::new();
            let (b, log) = recorder();
            let t = a.issue_with(b);
            a.complete_id(t.id(), ExecStatus::OK);
            a.complete(&t, ExecStatus::FAIL);
            a.crash();
            a.complete(&t, ExecStatus::FAIL);
            assert_eq!(*log.lock(), vec![(1, false)]);
            // A token born crashed is not in the table either.
            let c = a.issue();
            a.complete(&c, ExecStatus::OK);
            assert_eq!(c.wait(), ExecStatus::CRASHED);
        }

        /// By token: the last one wakes the drain
        #[test]
        fn by_token_last_wakes_drain() {
            let a = Arc::new(Completions::new());
            let t1 = a.issue();
            let t2 = a.issue();
            let a2 = a.clone();
            let rx = blocks(move || a2.drain());
            a.complete(&t1, ExecStatus::OK);
            assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
            a.complete(&t2, ExecStatus::OK);
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
    }

    mod crash {
        use super::*;

        /// Every token 4, ascending id order, under the table lock
        #[test]
        fn crash_signals_in_ascending_order_under_the_lock() {
            let a = Arc::new(Completions::new());
            let order = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let mut toks = Vec::new();
            for _ in 0..5 {
                let (o, a2) = (order.clone(), a.clone());
                let next = a.state.lock().next_id;
                toks.push(a.issue_with(Box::new(move |s, _| {
                    o.lock()
                        .push((next, s.raw(), a2.state.try_lock().is_none()));
                })));
            }
            a.complete(&toks[1], ExecStatus::OK);
            a.complete_id(toks[3].id(), ExecStatus::FAIL);
            order.lock().clear();
            a.crash();
            assert_eq!(
                *order.lock(),
                vec![(1, 4, true), (3, 4, true), (5, 4, true)]
            );
            assert!(a.state.lock().tokens.is_empty());
            assert!(a.state.lock().crashed);
            assert_eq!(toks[1].wait(), ExecStatus::OK);
        }

        /// Wakes the drain; a second crash signals nothing
        #[test]
        fn wakes_drain_and_repeats_harmlessly() {
            let a = Arc::new(Completions::new());
            let t = a.issue();
            let a2 = a.clone();
            let rx = blocks(move || a2.drain());
            a.crash();
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
            a.crash();
            assert_eq!(t.wait(), ExecStatus::CRASHED);
        }

        /// A token already signalled outside the table dies at the crash's signal
        #[test]
        fn crash_of_a_signalled_registered_token_dies() {
            expect_death(SIGNALLED_TWICE, || {
                let a = Completions::new();
                let t = a.issue();
                t.signal(ExecStatus::OK);
                a.crash();
            });
        }
    }

    mod drain {
        use super::*;

        /// Empty: returns at once
        #[test]
        fn empty_returns() {
            let a = Completions::new();
            a.drain();
            let t = a.issue();
            a.complete(&t, ExecStatus::OK);
            a.drain();
        }

        /// Blocks while any token is outstanding, no timeout
        #[test]
        fn blocks_until_empty() {
            let a = Arc::new(Completions::new());
            let t = a.issue();
            let a2 = a.clone();
            let rx = blocks(move || a2.drain());
            assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
            a.complete_id(t.id(), ExecStatus::OK);
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
    }

    mod destroy_order {
        use super::*;

        /// Drops push their id to a shared log.
        struct OnDrop(u64, Arc<parking_lot::Mutex<Vec<u64>>>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                self.1.lock().push(self.0);
            }
        }

        /// `n` tokens whose only reference is the table, each handler logging its release.
        fn table_of(n: u64) -> (Completions, Arc<parking_lot::Mutex<Vec<u64>>>) {
            let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let t = Completions::new();
            for id in 1..=n {
                let d = OnDrop(id, log.clone());
                let b: TokenHandler = Box::new(move |_, _| {
                    let _ = &d;
                });
                drop(t.issue_with(b));
            }
            (t, log)
        }

        /// A crash releases the handlers in ascending id order
        #[test]
        fn crash_releases_handlers_in_ascending_order() {
            for n in [1, 2, 3, 5, 8] {
                let (t, log) = table_of(n);
                t.crash();
                assert_eq!(*log.lock(), (1..=n).collect::<Vec<_>>());
            }
        }

        /// The table's own drop, in ascending id order
        #[test]
        fn table_drop_releases_in_ascending_order() {
            let (t, log) = table_of(5);
            drop(t);
            assert_eq!(*log.lock(), vec![1, 2, 3, 4, 5]);
        }

        /// Completions take tokens out wherever they are; the rest still go in ascending id
        /// order
        #[test]
        fn completions_leave_the_rest_in_ascending_order() {
            let (t, log) = table_of(8);
            for id in [4, 1, 7] {
                t.complete_id(id, ExecStatus::OK);
            }
            assert_eq!(*log.lock(), vec![4, 1, 7]);
            log.lock().clear();
            t.crash();
            assert_eq!(*log.lock(), vec![2, 3, 5, 6, 8]);
        }
    }
}

/// The `loom` model check: `RUSTFLAGS="--cfg loom" cargo test -p reims-vgpu-core --lib
/// tasks::remote::completions::loom_tests --release`.
#[cfg(all(test, loom))]
#[allow(clippy::unwrap_used)] // A failure is the assertion
mod loom_tests {
    use super::*;
    use loom::sync::atomic::{AtomicU32, Ordering};

    /// A reply and a crash race for one token: whichever takes it from the table signals it,
    /// exactly once, and the other finds it gone (no double signal, no lost signal).
    #[test]
    fn reply_and_crash_signal_once() {
        loom::model(|| {
            let a = loom::sync::Arc::new(Completions::new());
            let n = loom::sync::Arc::new(AtomicU32::new(0));
            let n2 = n.clone();
            let t = a.issue_with(Box::new(move |_, _| {
                n2.fetch_add(1, Ordering::SeqCst);
            }));
            let (a2, id) = (a.clone(), t.id());
            let h = loom::thread::spawn(move || a2.complete_id(id, ExecStatus::OK));
            a.crash();
            h.join().unwrap();
            assert_eq!(n.load(Ordering::SeqCst), 1);
            let s = t.wait();
            assert!(s == ExecStatus::OK || s == ExecStatus::CRASHED);
        });
    }

    /// A drain racing the last completion never misses its wakeup.
    #[test]
    fn drain_sees_the_last_completion() {
        loom::model(|| {
            let a = loom::sync::Arc::new(Completions::new());
            let t = a.issue();
            let a2 = a.clone();
            let h = loom::thread::spawn(move || a2.drain());
            a.complete(&t, ExecStatus::OK);
            h.join().unwrap();
            assert_eq!(t.wait(), ExecStatus::OK);
        });
    }
}
