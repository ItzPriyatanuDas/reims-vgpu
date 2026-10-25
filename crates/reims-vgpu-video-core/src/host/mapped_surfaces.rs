//! The guest surfaces the host session holds, by mapping ID, and the sweep
//! that tells the guest which ones it holds no longer.
//!
//! Apple's host session keeps one entry per guest surface it was lent:
//! the surface's ID and a weak reference to the child surface it made of
//! it. A message that names a surface again reuses the live child; a
//! child dies when the last message, reply or pixel buffer using it lets
//! go; and after every delivery the session drops the dead entries and
//! sends their IDs in a `rels`. Here the child is a [`SurfaceHold`], and the
//! weak reference a `Weak` of it.
//!
//! A hold ends once the host's hold on the surface has been let go, not
//! when its last `Arc` starts to drop: the sweep names a surface only after
//! the mapper's hold on it is gone, so the guest never learns of a release
//! the host has not made yet, whichever thread lets it go and whichever
//! sweeps. Holds a session lets go of together end together
//! ([`ending_together`]), so that no sweep names some of them and not the
//! rest: [`HostShared::let_go`](crate::host::HostShared::let_go).
//!
//! Measured on a Radeon 780M (radeonsi, Mesa 26.2.4, 2026-10-25), decoding
//! a 30-frame 3840x2160 10-bit AV1 stream one frame at a time: the device's
//! thread swept 2 to 8 µs (4.4 µs the median) before the session's thread
//! let go of the buffers due at a step, which took it about 1.5 µs. A
//! sweep of the device's thread now and then lands between two of those
//! drops, which is why the holds end together.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use crate::surfaces::{GuestSurface, MappingId, Surfaces};
use crate::sync::Mutex;

/// A guest surface the host holds: the mapper's hold and its ID. The
/// hold ends when the last `Arc` of it is dropped, once the surface is.
pub(crate) struct SurfaceHold {
    pub(crate) mapping_id: MappingId,
    pub(crate) surface: Box<dyn GuestSurface>,
    /// Dropped after `surface`, as fields drop in their order: says the
    /// hold has ended.
    _ended: Ended,
}

impl std::fmt::Debug for SurfaceHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SurfaceHold")
            .field("mapping_id", &self.mapping_id)
            .finish_non_exhaustive()
    }
}

/// Set as the hold it ends drops, after the hold's surface, or kept back
/// inside [`ending_together`].
struct Ended(Arc<AtomicBool>);

impl Drop for Ended {
    fn drop(&mut self) {
        end_or_keep(vec![self.0.clone()]);
    }
}

/// The ends kept back on a thread inside [`ending_together`], or `None`
/// outside one.
type KeptEnds = Option<Vec<Arc<AtomicBool>>>;

thread_local! {
    /// The ends of the holds this thread lets go inside [`ending_together`].
    static ENDING_TOGETHER: RefCell<KeptEnds> = const { RefCell::new(None) };
}

/// Ends `ends` now, or keeps them for the [`ending_together`] this thread
/// is inside.
fn end_or_keep(mut ends: Vec<Arc<AtomicBool>>) {
    let _ = ENDING_TOGETHER.try_with(|ending| {
        if let Some(kept) = ending.borrow_mut().as_mut() {
            kept.append(&mut ends);
        }
    });
    for end in ends {
        end.store(true, Ordering::Release);
    }
}

/// The ends of holds let go together, kept back ([`ending_together`]).
pub(crate) struct Ends(Vec<Arc<AtomicBool>>);

impl Ends {
    /// Whether no hold ended.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Ends them all now: under the table's lock, before its sweep.
    pub(crate) fn end(mut self) {
        for end in self.0.drain(..) {
            end.store(true, Ordering::Release);
        }
    }
}

impl Drop for Ends {
    /// Dropped without [`Ends::end`], they end now, or with the ends of the
    /// [`ending_together`] this thread is inside.
    fn drop(&mut self) {
        end_or_keep(std::mem::take(&mut self.0));
    }
}

/// Runs `release` and keeps back the end of every hold it lets go on this
/// thread, [`Ends`] dropped in it included: their surfaces are let go as it
/// runs, outside the table's lock, and the holds end as one at
/// [`Ends::end`]. A hold whose last `Arc` another thread drops meanwhile
/// ends as that thread drops it.
pub(crate) fn ending_together(release: impl FnOnce()) -> Ends {
    /// The ends kept outside this call, until it puts them back, which it
    /// does however `release` ends.
    struct Scope(Option<KeptEnds>);

    impl Scope {
        /// Puts the outer ends back, and gives this call's.
        fn close(&mut self) -> Ends {
            let Some(mut outer) = self.0.take() else {
                return Ends(Vec::new());
            };
            let ours = ENDING_TOGETHER
                .try_with(|ending| std::mem::replace(&mut *ending.borrow_mut(), outer.take()))
                .ok()
                .flatten();
            // Left only where the thread's locals are gone: they end now.
            drop(outer.map(Ends));
            Ends(ours.unwrap_or_default())
        }
    }

    impl Drop for Scope {
        fn drop(&mut self) {
            drop(self.close());
        }
    }

    let Ok(outer) = ENDING_TOGETHER.try_with(|ending| ending.borrow_mut().replace(Vec::new()))
    else {
        // The thread's locals are gone: each hold ends as it drops.
        release();
        return Ends(Vec::new());
    };
    let mut scope = Scope(Some(outer));
    release();
    scope.close()
}

/// One entry: a mapping ID, the hold made for it while it lives, whether
/// that hold has ended, and the retains for it in flight.
struct MappedSurface {
    mapping_id: MappingId,
    hold: Weak<SurfaceHold>,
    /// Set for an entry that has had no hold yet, which the sweep names as
    /// soon as no retain for it is in flight.
    ended: Arc<AtomicBool>,
    /// Whether a hold was ever made for it: one that never had a hold and
    /// whose retain failed stands for no surface, and is named as ID 0.
    ever_held: bool,
    /// Retains for it in flight, outside the table's lock: the sweep keeps
    /// it while there are any.
    retaining: usize,
    /// Tells it from every other entry the table has had, for a retain's
    /// [`Claim`].
    serial: u64,
}

impl MappedSurface {
    /// Whether it still stands for `mapping_id`: a retain that failed for
    /// a new entry made it ID 0.
    fn stands_for(&self, mapping_id: MappingId) -> bool {
        self.mapping_id == mapping_id
    }
}

/// The table.
#[derive(Default)]
pub(crate) struct MappedSurfaces {
    entries: Vec<MappedSurface>,
    /// The next entry's serial.
    next_serial: u64,
}

/// What a message's surface needs, as the table has it.
pub(crate) enum Lookup {
    /// The entry's live hold, reused.
    Held(Arc<SurfaceHold>),
    /// A retain, outside the table's lock, for the entry claimed.
    Retain(Claim),
}

/// An entry claimed for a retain of its surface: the sweep keeps it until
/// [`MappedSurfaces::publish`] gives it the retain's result.
#[must_use]
pub(crate) struct Claim {
    mapping_id: MappingId,
    serial: u64,
}

/// A surface the mapper retained for a [`Claim`], outside the table's lock:
/// its hold and the hold's end.
pub(crate) struct Retained {
    hold: Arc<SurfaceHold>,
    ended: Arc<AtomicBool>,
}

/// What [`MappedSurfaces::publish`] makes of a retain.
pub(crate) struct Published {
    /// The surface's hold for the message; `None` when it cannot be held.
    pub(crate) hold: Option<Arc<SurfaceHold>>,
    /// The retain's own hold, when another retain's was published first:
    /// the caller lets it go outside the table's lock.
    pub(crate) surplus: Option<Arc<SurfaceHold>>,
}

impl MappedSurfaces {
    /// The live hold of `mapping_id`'s entry, or a claim on that entry for a
    /// retain, made for the ID if the table has none: added where Apple's
    /// adds it, before its surface is held, and kept from the sweep while
    /// the retain is in flight.
    pub(crate) fn look_up(&mut self, mapping_id: MappingId) -> Lookup {
        let existing = self
            .entries
            .iter_mut()
            .find(|entry| entry.stands_for(mapping_id));
        if let Some(entry) = existing {
            if let Some(hold) = entry.hold.upgrade() {
                return Lookup::Held(hold);
            }
            entry.retaining = entry.retaining.saturating_add(1);
            return Lookup::Retain(Claim {
                mapping_id,
                serial: entry.serial,
            });
        }
        let serial = self.take_serial();
        self.entries.push(MappedSurface {
            mapping_id,
            hold: Weak::new(),
            ended: Arc::new(AtomicBool::new(true)),
            ever_held: false,
            retaining: 1,
            serial,
        });
        Lookup::Retain(Claim { mapping_id, serial })
    }

    /// Gives the claimed entry the retain's result, `None` for one that
    /// failed, as if the whole lookup had been made now:
    /// - a hold another retain published for the entry meanwhile is the
    ///   surface's, and the retain's own is the surplus;
    /// - a retain that succeeded makes the entry's hold. One whose hold
    ///   ended meanwhile is held anew, and no sweep names that end, as a
    ///   lookup made under the lock finds a dead hold and makes another;
    /// - a failed one leaves an entry that had a hold as it was, to be swept
    ///   as its ID once that hold ends, and one that never had one as ID 0,
    ///   as Apple's failed child is;
    /// - where another retain's failure made the entry ID 0 meanwhile, the
    ///   result goes in an entry of its own, at the table's end, as a
    ///   lookup after that failure would add it.
    pub(crate) fn publish(&mut self, claim: Claim, retained: Option<Retained>) -> Published {
        let entry = (self.entries.iter_mut()).find(|entry| entry.serial == claim.serial);
        let entry = entry.and_then(|entry| {
            entry.retaining = entry.retaining.saturating_sub(1);
            entry.stands_for(claim.mapping_id).then_some(entry)
        });
        if let Some(hold) = entry.as_ref().and_then(|entry| entry.hold.upgrade()) {
            return Published {
                hold: Some(hold),
                surplus: retained.map(|retained| retained.hold),
            };
        }
        match (entry, retained) {
            (Some(entry), Some(retained)) => {
                entry.hold = Arc::downgrade(&retained.hold);
                entry.ended = retained.ended;
                entry.ever_held = true;
                Published {
                    hold: Some(retained.hold),
                    surplus: None,
                }
            }
            (Some(entry), None) => {
                if !entry.ever_held {
                    entry.mapping_id = MappingId(0);
                }
                Published {
                    hold: None,
                    surplus: None,
                }
            }
            (None, retained) => {
                let serial = self.take_serial();
                let (mapping_id, hold, ended) = match &retained {
                    Some(retained) => (
                        claim.mapping_id,
                        Arc::downgrade(&retained.hold),
                        retained.ended.clone(),
                    ),
                    None => (MappingId(0), Weak::new(), Arc::new(AtomicBool::new(true))),
                };
                self.entries.push(MappedSurface {
                    mapping_id,
                    hold,
                    ended,
                    ever_held: retained.is_some(),
                    retaining: 0,
                    serial,
                });
                Published {
                    hold: retained.map(|retained| retained.hold),
                    surplus: None,
                }
            }
        }
    }

    /// Drops the entries whose hold has ended and returns their IDs, in
    /// table order. A hold whose last `Arc` is dropping, its surface not
    /// yet let go, has not ended: a later sweep names it. An entry with a
    /// retain in flight is kept.
    pub(crate) fn sweep(&mut self) -> Vec<MappingId> {
        let mut released = Vec::new();
        self.entries.retain(|entry| {
            if entry.retaining == 0 && entry.ended.load(Ordering::Acquire) {
                released.push(entry.mapping_id);
                false
            } else {
                true
            }
        });
        released
    }

    /// How many entries the table has.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    fn take_serial(&mut self) -> u64 {
        let serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1);
        serial
    }
}

/// Holds the surfaces a message names, in order, reusing a live hold for
/// an ID `table` has. When one cannot be held, the message gets none: the
/// holds made for it before are the `Err`, which the caller lets go outside
/// the table's lock, to be swept, as Apple's `mapFailed` path drops the
/// whole array. A failed new entry is swept as ID 0, as Apple's is; the
/// guest kext skips a zero ID.
///
/// Each retain runs outside the table's lock, between a
/// [`MappedSurfaces::look_up`] and a [`MappedSurfaces::publish`] under it.
/// Over reims-vgpu's mapper a retain waits for the IOSurface device's
/// queue (`HostDeviceCore::retain_surface_with_mapping_id`, a `sync`),
/// behind the guest's ring commands, which map guest memory through the
/// platform and call the host's `raise_interrupt` after each; it takes the
/// process's and the device's locks, and logs through the host's handler.
/// Under the table's lock every delivery and sweep would wait for that
/// queue, and a host whose `raise_interrupt` delivers a message, as the C
/// API lets it, would wait on the lock the delivery waiting for its queue
/// holds, for ever.
pub(crate) fn hold(
    table: &Mutex<MappedSurfaces>,
    surfaces: &dyn Surfaces,
    ids: &[MappingId],
) -> Result<Vec<Arc<SurfaceHold>>, Vec<Arc<SurfaceHold>>> {
    let mut held = Vec::with_capacity(ids.len());
    for &mapping_id in ids {
        let lookup = table.lock().look_up(mapping_id);
        let claim = match lookup {
            Lookup::Held(hold) => {
                held.push(hold);
                continue;
            }
            Lookup::Retain(claim) => claim,
        };
        let retained = retain(surfaces, mapping_id);
        let Published { hold, surplus } = table.lock().publish(claim, retained);
        // Another retain's hold was published first: this one's surface is
        // let go here, outside the lock. It was never in the table.
        drop(surplus);
        match hold {
            Some(hold) => held.push(hold),
            None => return Err(held),
        }
    }
    Ok(held)
}

/// A hold of `mapping_id`'s surface, and its end.
fn retain(surfaces: &dyn Surfaces, mapping_id: MappingId) -> Option<Retained> {
    match surfaces.retain(mapping_id) {
        Ok(surface) => {
            let ended = Arc::new(AtomicBool::new(false));
            let hold = Arc::new(SurfaceHold {
                mapping_id,
                surface,
                _ended: Ended(ended.clone()),
            });
            Some(Retained { hold, ended })
        }
        Err(error) => {
            log::warn!("cannot hold guest surface {:#x}: {error}", mapping_id.0);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::fourcc::FourCc;
    use crate::runtime::StdRuntime;
    use crate::surfaces::{SurfaceError, SurfaceLayout};

    /// What runs as a surface's hold is let go, before it is.
    type OnRelease = Box<dyn FnOnce() + Send>;

    /// How long a test waits for another thread's work, which takes
    /// microseconds: past it the work is stuck behind the table's lock.
    const WAIT: Duration = Duration::from_secs(10);

    /// Surfaces whose release runs what is set for it first.
    #[derive(Default)]
    struct Releasing(Arc<Mutex<Option<OnRelease>>>);

    struct Held(Arc<Mutex<Option<OnRelease>>>);

    impl GuestSurface for Held {
        fn layout(&self) -> SurfaceLayout {
            SurfaceLayout {
                pixel_format: FourCc::new(*b"420v"),
                width: 2,
                height: 2,
                planes: Vec::new(),
                bytes_per_row: 0,
            }
        }

        fn write(&self, _: &mut dyn FnMut(&mut [u8])) -> Result<(), SurfaceError> {
            Ok(())
        }
    }

    impl Drop for Held {
        fn drop(&mut self) {
            let on_release = self.0.lock().take();
            if let Some(on_release) = on_release {
                on_release();
            }
        }
    }

    impl Surfaces for Releasing {
        fn retain(&self, _: MappingId) -> Result<Box<dyn GuestSurface>, SurfaceError> {
            Ok(Box::new(Held(self.0.clone())))
        }
    }

    /// What runs, once, inside a surface's retain, before it answers.
    type OnRetain = Box<dyn FnOnce() + Send>;

    /// Surfaces that run what is set for the next retain inside it, and
    /// hold, or refuse, as `holds` says, counting the surfaces held.
    struct Retaining {
        on_retain: Mutex<Option<OnRetain>>,
        holds: fn(MappingId) -> bool,
        held: Arc<AtomicUsize>,
    }

    struct Counted(Arc<AtomicUsize>);

    impl GuestSurface for Counted {
        fn layout(&self) -> SurfaceLayout {
            Held(Arc::default()).layout()
        }

        fn write(&self, _: &mut dyn FnMut(&mut [u8])) -> Result<(), SurfaceError> {
            Ok(())
        }
    }

    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl Retaining {
        fn new(holds: fn(MappingId) -> bool) -> Arc<Self> {
            Arc::new(Retaining {
                on_retain: Mutex::new(None),
                holds,
                held: Arc::default(),
            })
        }

        /// Runs `inside` within the next retain.
        fn on_retain(&self, inside: impl FnOnce() + Send + 'static) {
            *self.on_retain.lock() = Some(Box::new(inside));
        }

        fn held(&self) -> usize {
            self.held.load(Ordering::SeqCst)
        }
    }

    impl Surfaces for Retaining {
        fn retain(&self, id: MappingId) -> Result<Box<dyn GuestSurface>, SurfaceError> {
            let inside = self.on_retain.lock().take();
            if let Some(inside) = inside {
                inside();
            }
            if !(self.holds)(id) {
                return Err(SurfaceError::Unknown(id.0));
            }
            self.held.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Counted(self.held.clone())))
        }
    }

    /// Runs `work` on a thread of its own, as the device's thread or a
    /// session's would, and waits for its answer, `None` past [`WAIT`].
    fn on_another_thread<T: Send + 'static>(
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let (answer, answered) = mpsc::channel();
        let working = StdRuntime::thread("test-another", move || {
            let _ = answer.send(work());
        });
        assert!(working.is_ok());
        answered.recv_timeout(WAIT).ok()
    }

    /// A sweep while a hold's surface is being let go, its last `Arc`
    /// dropped, does not name it: the next does.
    #[test]
    fn a_hold_being_let_go_is_named_once_it_has_ended() {
        let surfaces = Releasing::default();
        let table = Arc::new(Mutex::new(MappedSurfaces::default()));
        let held = hold(&table, &surfaces, &[MappingId(7)]);
        assert!(held.is_ok());
        let swept_meanwhile = Arc::new(Mutex::new(None));
        *surfaces.0.lock() = Some(Box::new({
            let table = table.clone();
            let swept_meanwhile = swept_meanwhile.clone();
            move || *swept_meanwhile.lock() = Some(table.lock().sweep())
        }));
        drop(held);
        assert_eq!(*swept_meanwhile.lock(), Some(Vec::new()));
        assert_eq!(table.lock().sweep(), [MappingId(7)]);
        assert_eq!(table.lock().len(), 0);
    }

    /// A surface named again while its hold is being let go is held anew,
    /// and its entry stays.
    #[test]
    fn a_surface_named_again_as_its_hold_is_let_go_is_held_anew() {
        let surfaces = Releasing::default();
        let table = Arc::new(Mutex::new(MappedSurfaces::default()));
        let first = hold(&table, &surfaces, &[MappingId(7)]);
        assert!(first.is_ok());
        let again = Arc::new(Mutex::new(Ok(Vec::new())));
        *surfaces.0.lock() = Some(Box::new({
            let table = table.clone();
            let again = again.clone();
            move || *again.lock() = hold(&table, &Releasing::default(), &[MappingId(7)])
        }));
        drop(first);
        assert_eq!(again.lock().as_ref().map(Vec::len).ok(), Some(1));
        assert_eq!(table.lock().sweep(), Vec::<MappingId>::new());
        *again.lock() = Ok(Vec::new());
        assert_eq!(table.lock().sweep(), [MappingId(7)]);
    }

    /// A surface that cannot be held leaves an ID-0 entry, which the next
    /// sweep names.
    #[test]
    fn a_failed_hold_is_named_as_id_0() {
        struct Refusing;
        impl Surfaces for Refusing {
            fn retain(&self, id: MappingId) -> Result<Box<dyn GuestSurface>, SurfaceError> {
                Err(SurfaceError::Unknown(id.0))
            }
        }
        let table = Mutex::new(MappedSurfaces::default());
        assert!(hold(&table, &Refusing, &[MappingId(3)]).is_err_and(|made| made.is_empty()));
        assert_eq!(table.lock().sweep(), [MappingId(0)]);
    }

    /// When a message's later surface cannot be held, the holds made for
    /// it before are handed back, not let go inside the call: the caller
    /// lets them go, outside the table's lock.
    #[test]
    fn a_failed_hold_hands_back_those_made_before_it() {
        /// Holds 7, refusing every other ID.
        struct Only7(Releasing);
        impl Surfaces for Only7 {
            fn retain(&self, id: MappingId) -> Result<Box<dyn GuestSurface>, SurfaceError> {
                match id {
                    MappingId(7) => self.0.retain(id),
                    _ => Err(SurfaceError::Unknown(id.0)),
                }
            }
        }
        let surfaces = Only7(Releasing::default());
        let released = Arc::new(AtomicBool::new(false));
        *surfaces.0.0.lock() = Some(Box::new({
            let released = released.clone();
            move || released.store(true, Ordering::Release)
        }));
        let table = Mutex::new(MappedSurfaces::default());
        let Err(made) = hold(&table, &surfaces, &[MappingId(7), MappingId(3)]) else {
            panic!("ID 3 was held");
        };
        assert_eq!(made.len(), 1);
        assert!(!released.load(Ordering::Acquire));
        assert_eq!(table.lock().sweep(), [MappingId(0)]);
        drop(made);
        assert!(released.load(Ordering::Acquire));
        assert_eq!(table.lock().sweep(), [MappingId(7)]);
    }

    /// The mapper's retain runs outside the table's lock: another thread
    /// sweeps while it is in flight, and that sweep names neither the new
    /// entry it is for, as ID 0, nor an entry whose hold ended and is being
    /// held anew. The surfaces are held once each.
    #[test]
    fn a_sweep_while_a_surface_is_retained_names_neither_its_entry_nor_a_dead_one() {
        let surfaces = Retaining::new(|_| true);
        let table = Arc::new(Mutex::new(MappedSurfaces::default()));
        let first = hold(&table, &*surfaces, &[MappingId(5)]);
        assert!(first.is_ok());
        drop(first);
        // 5's hold has ended, unswept; 7 is new.
        let swept = Arc::new(Mutex::new(Vec::new()));
        let mut holds = Vec::new();
        for id in [5, 7] {
            surfaces.on_retain({
                let (table, swept) = (table.clone(), swept.clone());
                move || {
                    let sweep = on_another_thread(move || table.lock().sweep());
                    swept.lock().push(sweep);
                }
            });
            let Ok(held) = hold(&table, &*surfaces, &[MappingId(id)]) else {
                panic!("{id} was not held");
            };
            holds.push(held);
        }
        assert_eq!(*swept.lock(), [Some(Vec::new()), Some(Vec::new())]);
        assert_eq!(surfaces.held(), 2);
        drop(holds);
        assert_eq!(surfaces.held(), 0);
        assert_eq!(table.lock().sweep(), [MappingId(5), MappingId(7)]);
    }

    /// Two retains of one surface at once, the second inside the first, as
    /// a host delivering from inside the IOSurface device's interrupt would
    /// make them: the first published is the surface's hold, the other's
    /// surface is let go, and the entry is one, named once.
    #[test]
    fn a_surface_retained_twice_at_once_is_held_once() {
        let surfaces = Retaining::new(|_| true);
        let table = Arc::new(Mutex::new(MappedSurfaces::default()));
        let inner = Arc::new(Mutex::new(None));
        surfaces.on_retain({
            let (table, surfaces, inner) = (table.clone(), surfaces.clone(), inner.clone());
            move || {
                let held = on_another_thread(move || hold(&table, &*surfaces, &[MappingId(7)]));
                *inner.lock() = held;
            }
        });
        let outer = hold(&table, &*surfaces, &[MappingId(7)]);
        let inner = inner.lock().take();
        let (Ok(outer), Some(Ok(inner))) = (outer, inner) else {
            panic!("7 was not held by both");
        };
        assert!(Arc::ptr_eq(&outer[0], &inner[0]));
        assert_eq!(surfaces.held(), 1);
        assert_eq!(table.lock().len(), 1);
        drop(outer);
        assert_eq!(table.lock().sweep(), Vec::<MappingId>::new());
        drop(inner);
        assert_eq!(table.lock().sweep(), [MappingId(7)]);
    }

    /// A retain that fails while another of the same new surface is in
    /// flight leaves the ID-0 entry a failure leaves, and the other's hold
    /// goes in an entry of its own after it, as if the failure came first,
    /// whole; a failure that finds the other's hold published uses it.
    #[test]
    fn a_retain_failing_beside_another_of_its_surface_is_ordered_whole() {
        // The inner retain of 7 fails, the outer one holds.
        let surfaces = Retaining::new(|_| true);
        let refusing = Retaining::new(|_| false);
        let table = Arc::new(Mutex::new(MappedSurfaces::default()));
        let inner = Arc::new(Mutex::new(None));
        surfaces.on_retain({
            let (table, refusing, inner) = (table.clone(), refusing.clone(), inner.clone());
            move || {
                let held = on_another_thread(move || hold(&table, &*refusing, &[MappingId(7)]));
                *inner.lock() = held;
            }
        });
        let outer = hold(&table, &*surfaces, &[MappingId(7)]);
        assert!(inner.lock().take().is_some_and(|held| held.is_err()));
        assert_eq!(outer.as_ref().map(Vec::len).ok(), Some(1));
        assert_eq!(table.lock().len(), 2);
        assert_eq!(table.lock().sweep(), [MappingId(0)]);
        drop(outer);
        assert_eq!(table.lock().sweep(), [MappingId(7)]);

        // The outer retain of 9 fails, after the inner one held it.
        let inner = Arc::new(Mutex::new(None));
        refusing.on_retain({
            let (table, surfaces, inner) = (table.clone(), surfaces.clone(), inner.clone());
            move || {
                let held = on_another_thread(move || hold(&table, &*surfaces, &[MappingId(9)]));
                *inner.lock() = held;
            }
        });
        let outer = hold(&table, &*refusing, &[MappingId(9)]);
        let inner = inner.lock().take();
        let (Ok(outer), Some(Ok(inner))) = (outer, inner) else {
            panic!("9 was not held by both");
        };
        assert!(Arc::ptr_eq(&outer[0], &inner[0]));
        drop((outer, inner));
        assert_eq!(table.lock().sweep(), [MappingId(9)]);
        assert_eq!(table.lock().len(), 0);
    }
}
