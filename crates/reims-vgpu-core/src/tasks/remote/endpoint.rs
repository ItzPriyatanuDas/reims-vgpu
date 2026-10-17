//! The host's side of a remote task's connection: the service's messages to the host
//! arrive here, one at a time on the link's queue, and so do the connection's interruption and
//! invalidation ([`HostSide`]).
//!
//! Ordering contracts (each named test pins it):
//! - **A completion** from the service is traced, then completes its token by id with the
//!   service's status, unvalidated; the error it carries is dropped
//!   (`completes_by_id_unvalidated_and_drops_the_error`).
//! - **A crash** (the connection's interruption or its invalidation, so possibly twice) drops
//!   every kept surface, then signals every outstanding token 4 in ascending id order, all under
//!   the endpoint's lock (`clears_surfaces_and_signals_every_token_four_in_order`).
//! - **Drop** releases the kept surfaces, then the completions' tokens
//!   (`drop_releases_the_kept_surfaces`).

use parking_lot::Mutex;

use crate::platform::Platform;
use crate::rt::Hold;
use crate::seams::links::TaskDeviceLink;
use crate::tasks::ExecStatus;
use crate::tasks::launcher::HostSide;
use crate::tasks::remote::completions::Completions;
use crate::tasks::remote::device_link::DeviceLink;
use crate::tasks::wire::ToHost;

/// A remote task's endpoint: its completions and its link to the device.
///
/// Fields drop in declaration order: the kept surfaces (with the device link), then the
/// completions' tokens.
pub struct Endpoint<P: Platform> {
    /// Held across a surface's retain (the mapper service's lookup and the reply) and across a
    /// crash.
    lock: Mutex<()>,
    device: DeviceLink<P>,
    completions: Completions,
}

impl<P: Platform> Endpoint<P> {
    /// An endpoint for a task of `device`, with no token and no kept surface.
    pub fn new(device: TaskDeviceLink<P>) -> Endpoint<P> {
        Endpoint::with_completions(device, Completions::new())
    }

    /// An endpoint for the task `task_id` of `device`: the same, its crash warning naming the
    /// task.
    pub fn for_task(device: TaskDeviceLink<P>, task_id: u32) -> Endpoint<P> {
        Endpoint::with_completions(device, Completions::for_task(task_id))
    }

    fn with_completions(device: TaskDeviceLink<P>, completions: Completions) -> Endpoint<P> {
        Endpoint {
            lock: Mutex::new(()),
            device: DeviceLink::new(device),
            completions,
        }
    }

    /// The task's outstanding completions.
    pub(crate) fn completions(&self) -> &Completions {
        &self.completions
    }

    /// The link to the device.
    pub(crate) fn device_link(&self) -> &DeviceLink<P> {
        &self.device
    }

    /// The task died: every kept surface goes, then every outstanding token completes with
    /// 4, under the lock. A second call finds nothing to complete.
    fn crashed(&self) {
        let _lock = self.lock.lock();
        self.device.release_all_surfaces();
        self.completions.crash();
    }
}

impl<P: Platform> HostSide<P> for Endpoint<P> {
    fn on_message(&self, m: ToHost<P>) {
        match m {
            ToHost::RetainIoSurface { mapping_id, reply } => {
                let _lock = self.lock.lock();
                self.device.retain_surface(mapping_id, &reply);
            }
            ToHost::ReleaseIoSurface { mapping_id } => self.device.release_surface(mapping_id),
            ToHost::CompleteAsyncToken {
                token_id,
                success,
                error,
            } => {
                // Tokens signal with no error.
                drop(error);
                self.completions.complete_id(token_id, ExecStatus(success));
            }
            ToHost::NewSharedTextureHandle { id, reply } => {
                // A block of the link's queue: no handler.
                self.device.shared_texture_handle(Hold::NONE, id, &reply);
            }
        }
    }

    fn on_interrupted(&self) {
        self.crashed();
    }

    fn on_invalidated(&self) {
        self.crashed();
    }
}

#[cfg(test)]
mod tests {
    use crate::raise::OnFault;
    use crate::rt::Hold;
    use std::sync::Arc;

    use super::*;
    use crate::device::test_support::{Opts, TestDevice, device};
    use crate::platform::Handle;
    use crate::platform::types::NsErrorText;
    use crate::tasks::remote::completions::TokenHandler;
    use crate::tasks::test_support::Lines;
    use crate::tasks::wire::{Reply, ToHost};
    use crate::testkit::FakePlatform;

    type P = FakePlatform;

    struct Fixture {
        platform: Arc<P>,
        td: TestDevice,
        d: Endpoint<P>,
        /// The tasks area's lines, from the endpoint's making on.
        logs: Lines,
    }

    impl Fixture {
        /// The tasks area's lines since the fixture was made or this was last asked.
        fn lines(&self) -> Vec<String> {
            self.logs.take()
        }
    }

    /// An endpoint for a device with no kernel task.
    fn fixture() -> Fixture {
        fixture_on(device(Opts::default()))
    }

    /// An endpoint for a device whose kernel task (task 0, local) has no objects.
    fn kernel_fixture() -> Fixture {
        let td = device(Opts::default()).with_root_page();
        td.task_root_node(0x55, 0x4000);
        {
            {
                let hold = Hold::NONE;
                td.device
                    .create_task_id(hold, 0, 0x55, 0x4000, false)
                    .unwrap();
            }
        }
        fixture_on(td)
    }

    fn fixture_on(td: TestDevice) -> Fixture {
        let platform = td.platform.clone();
        let dev: Arc<crate::device::DeviceCore<P>> = Arc::clone(&td.device);
        let logs = Lines::capture();
        let d = Endpoint::new(Arc::downgrade(&dev));
        Fixture {
            platform,
            td,
            d,
            logs,
        }
    }

    /// The device's kernel task.
    fn kernel(f: &Fixture) -> Arc<crate::tasks::local::LocalTask<P>> {
        match f.td.device.get_task_id(OnFault::Return, 0).unwrap() {
            crate::tasks::Task::Local(t) => t,
            _ => panic!("no local kernel task"),
        }
    }

    type Seen<T> = Arc<Mutex<Vec<T>>>;

    fn seen<T: Send + 'static>() -> (Seen<T>, Reply<T>) {
        let s: Seen<T> = Arc::new(Mutex::new(Vec::new()));
        let s2 = s.clone();
        (s, Reply::new(move |v| s2.lock().push(v)))
    }

    fn statuses() -> (Seen<u32>, TokenHandler) {
        let s: Seen<u32> = Arc::new(Mutex::new(Vec::new()));
        let s2 = s.clone();
        (s, Box::new(move |st, _| s2.lock().push(st.raw())))
    }

    /// The service's retain of `mapping_id`, as the link delivers it.
    fn retain(f: &Fixture, mapping_id: u64, reply: Reply<Option<<P as Platform>::Surface>>) {
        f.d.on_message(ToHost::RetainIoSurface { mapping_id, reply });
    }

    /// The service's completion of `token_id`.
    fn complete(f: &Fixture, token_id: u64, success: u32) {
        f.d.on_message(ToHost::CompleteAsyncToken {
            token_id,
            success,
            error: None,
        });
    }

    mod new {
        use super::*;

        /// A new endpoint keeps nothing, and its first token is 1
        #[test]
        fn starts_empty_with_first_token_one() {
            let f = fixture();
            assert_eq!(f.d.device_link().kept_count(), 0);
            let t = f.d.completions().issue();
            assert_eq!(t.id(), 1);
            let (s, b) = statuses();
            let t2 = f.d.completions().issue_with(b);
            assert_eq!(t2.id(), 2);
            f.d.completions().complete(&t2, ExecStatus(7));
            assert_eq!(*s.lock(), vec![7]);
            f.d.completions().complete(&t, ExecStatus::OK);
            assert_eq!(t.wait(), ExecStatus::OK);
        }
    }

    mod retain_surface {
        use super::*;

        /// With no mapper service the reply is nil and the id is kept as a nil entry
        #[test]
        fn no_mapper_service_replies_nil_and_keeps_a_nil_entry() {
            let f = fixture();
            let (s, r) = seen();
            retain(&f, 0x55, r);
            assert_eq!(s.lock().len(), 1);
            assert!(s.lock()[0].is_none());
            assert_eq!(f.d.device_link().kept(0x55), Some(false));
            assert!(f.lines().is_empty());
        }

        /// A second retain of one id keeps the first entry, replies again and logs
        #[test]
        fn a_second_retain_of_one_id_keeps_the_entry_and_logs() {
            let f = fixture();
            let (s, r) = seen();
            retain(&f, 3, r.clone());
            retain(&f, 3, r);
            assert_eq!(s.lock().len(), 2);
            assert_eq!(f.d.device_link().kept_count(), 1);
            assert_eq!(
                f.lines(),
                ["surface of mapping id 3 retained again: the first stays kept"]
            );
        }

        /// A device that is gone answers as one with no mapper service
        #[test]
        fn a_device_that_is_gone_is_nil() {
            let d = {
                let other = device(Opts::default());
                let dev: Arc<crate::device::DeviceCore<P>> = Arc::clone(&other.device);
                Endpoint::new(Arc::downgrade(&dev))
            };
            let (s, r) = seen();
            d.on_message(ToHost::RetainIoSurface {
                mapping_id: 1,
                reply: r,
            });
            assert!(s.lock()[0].is_none());
        }

        /// The retain holds the endpoint's lock across the lookup and the reply
        #[test]
        fn the_retain_holds_the_endpoint_lock() {
            let f = fixture();
            let held = Arc::new(Mutex::new(None));
            let (h2, d) = (held.clone(), Arc::new(f.d));
            let d2 = Arc::downgrade(&d);
            d.on_message(ToHost::RetainIoSurface {
                mapping_id: 4,
                reply: Reply::new(move |_| {
                    *h2.lock() = d2.upgrade().map(|d| d.lock.try_lock().is_none());
                }),
            });
            assert_eq!(*held.lock(), Some(true));
            assert_eq!(d.device_link().kept(4), Some(false));
        }
    }

    mod release_surface {
        use super::*;

        /// A surface's release drops its entry without the endpoint's lock
        #[test]
        fn releases_without_the_endpoint_lock() {
            let f = fixture();
            let (_s, r) = seen();
            retain(&f, 8, r);
            // The endpoint's lock held elsewhere does not block the release.
            let _held = f.d.lock.lock();
            f.d.on_message(ToHost::ReleaseIoSurface { mapping_id: 8 });
            assert_eq!(f.d.device_link().kept_count(), 0);
            // An unknown id is nothing.
            f.d.on_message(ToHost::ReleaseIoSurface { mapping_id: 99 });
        }

        /// The release drops the kept reference
        #[test]
        fn releases_the_kept_surface() {
            let f = fixture();
            let s = f.platform.fake_surfaces().make_simple(4, 4, 0, 4);
            f.d.device_link().keep(1, s.clone_ref());
            assert_eq!(s.retain_count(), 2);
            f.d.on_message(ToHost::ReleaseIoSurface { mapping_id: 1 });
            assert_eq!(s.retain_count(), 1);
        }
    }

    mod complete_token {
        use super::*;

        /// The service's status completes the token by id, unvalidated; a repeat or an unknown
        /// id is nothing
        #[test]
        fn completes_by_id_unvalidated_and_drops_the_error() {
            let f = fixture();
            let (s, b) = statuses();
            let t = f.d.completions().issue_with(b);
            f.d.on_message(ToHost::CompleteAsyncToken {
                token_id: t.id(),
                success: 0xdead,
                error: Some(NsErrorText::default()),
            });
            assert_eq!(*s.lock(), vec![0xdead]);
            // A second completion, and an unknown id, are ignored.
            complete(&f, t.id(), 1);
            complete(&f, 77, 1);
            assert_eq!(*s.lock(), vec![0xdead]);
        }
    }

    mod crash {
        use super::*;

        /// The interruption drops every kept surface and completes every token 4 in id order;
        /// the invalidation after it finds nothing, and later tokens are born 4
        #[test]
        fn clears_surfaces_and_signals_every_token_four_in_order() {
            let f = fixture();
            let surf = f.platform.fake_surfaces().make_simple(4, 4, 0, 4);
            f.d.device_link().keep(1, surf.clone_ref());
            let order: Seen<u64> = Arc::new(Mutex::new(Vec::new()));
            let mut toks = Vec::new();
            for _ in 0..3 {
                let o = order.clone();
                let id = Arc::new(Mutex::new(0u64));
                let id2 = id.clone();
                let t = f.d.completions().issue_with(Box::new(move |st, _| {
                    assert_eq!(st, ExecStatus::CRASHED);
                    o.lock().push(*id2.lock());
                }));
                *id.lock() = t.id();
                toks.push(t);
            }
            f.d.on_interrupted();
            assert_eq!(*order.lock(), vec![1, 2, 3]);
            assert_eq!(f.d.device_link().kept_count(), 0);
            assert_eq!(surf.retain_count(), 1);
            // A second call (invalidation after interruption) finds nothing.
            f.d.on_invalidated();
            assert_eq!(order.lock().len(), 3);
            // Tokens made afterwards are born 4, synchronously.
            let (s, b) = statuses();
            let _t = f.d.completions().issue_with(b);
            assert_eq!(*s.lock(), vec![4]);
        }

        /// The invalidation alone is a crash too
        #[test]
        fn invalidation_alone_crashes_too() {
            let f = fixture();
            let t = f.d.completions().issue();
            f.d.on_invalidated();
            assert_eq!(t.wait(), ExecStatus::CRASHED);
        }

        /// The endpoint's drop releases the kept surfaces before the completions' tokens
        #[test]
        fn drop_releases_the_kept_surfaces() {
            let f = fixture();
            let surf = f.platform.fake_surfaces().make_simple(4, 4, 0, 4);
            f.d.device_link().keep(1, surf.clone_ref());
            let seen = Arc::new(Mutex::new(None));
            let (s2, surf2) = (seen.clone(), surf.clone_ref());
            let probe = OnDrop(move || *s2.lock() = Some(surf2.retain_count()));
            // Only the table holds the token.
            drop(f.d.completions().issue_with(Box::new(move |_, _| {
                let _held = &probe;
            })));
            drop(f.d);
            assert_eq!(surf.retain_count(), 1);
            // The token's handler went after the kept surface (only the probe's and the test's
            // references were left).
            assert_eq!(*seen.lock(), Some(2));
        }
    }

    /// Runs its closure when dropped.
    struct OnDrop<F: FnMut()>(F);

    impl<F: FnMut()> Drop for OnDrop<F> {
        fn drop(&mut self) {
            (self.0)()
        }
    }

    /// The service's request for a handle to the kernel task's shared texture `id`.
    fn handle_for(
        f: &Fixture,
        id: u32,
        reply: Reply<Option<<P as Platform>::SharedTextureHandle>>,
    ) {
        f.d.on_message(ToHost::NewSharedTextureHandle { id, reply });
    }

    mod shared_texture_handle {
        use super::*;

        /// A raise from the kernel task is logged and the reply is nil
        #[test]
        fn a_raise_from_the_kernel_task_is_logged_and_replies_nil() {
            let f = kernel_fixture();
            let (s, r) = seen();
            // The kernel task has no object 3: its lookup raises.
            handle_for(&f, 3, r);
            assert_eq!(s.lock().len(), 1);
            assert!(s.lock()[0].is_none());
            let lines: Vec<String> = f
                .lines()
                .into_iter()
                .filter(|l| l.starts_with("no shared texture handle for backing 3: "))
                .collect();
            assert_eq!(lines.len(), 1, "{lines:?}");
        }

        /// The kernel task's lookup is let go by the end of the call
        #[test]
        fn the_kernel_lookup_goes_with_the_call() {
            let f = kernel_fixture();
            let k = kernel(&f);
            let held = Arc::strong_count(&k);
            let (s, r) = seen();
            handle_for(&f, 12, r);
            assert_eq!(s.lock().len(), 1);
            assert_eq!(Arc::strong_count(&k), held);
        }

        /// A raise from the kernel task's lookup is caught too
        #[test]
        fn a_raise_from_get_kernel_task_is_logged_too() {
            // No kernel task: its lookup raises.
            let f = fixture();
            let (s, r) = seen();
            handle_for(&f, 3, r);
            assert!(s.lock()[0].is_none());
            let lines = f.lines();
            assert!(
                !lines
                    .iter()
                    .any(|l| l.starts_with("shared texture backing 3 not found")),
                "{lines:?}"
            );
            assert!(
                lines
                    .iter()
                    .any(|l| l.starts_with("no shared texture handle for backing 3: ")),
                "{lines:?}"
            );
        }
    }

    mod invalidate_backings {
        use super::*;

        /// The kernel task's log lines for backings it does not have, in order.
        fn backing_misses(f: &Fixture) -> Vec<String> {
            f.lines()
                .into_iter()
                .filter(|l| l.starts_with("shared texture backing "))
                .collect()
        }

        /// No backings ask nothing of the device
        #[test]
        fn count_zero_does_nothing() {
            // No kernel task: asking for it would raise and log.
            let f = fixture();
            f.d.device_link()
                .invalidate_shared_texture_backings(Hold::NONE, &[]);
            assert!(f.lines().is_empty());
        }

        /// The ids go to the kernel task as its records
        #[test]
        fn forwards_the_ids_to_the_kernel_task() {
            let f = kernel_fixture();
            f.d.device_link()
                .invalidate_shared_texture_backings(Hold::NONE, &[7, 0x0102_0304]);
            assert_eq!(
                backing_misses(&f),
                [
                    "shared texture backing 7 to invalidate not found",
                    "shared texture backing 16909060 to invalidate not found"
                ]
            );
        }

        /// The kernel task's lookup is let go by the end of the call
        #[test]
        fn the_kernel_lookup_goes_with_the_call() {
            let f = kernel_fixture();
            let k = kernel(&f);
            let held = Arc::strong_count(&k);
            f.d.device_link()
                .invalidate_shared_texture_backings(Hold::NONE, &[7]);
            assert_eq!(backing_misses(&f).len(), 1);
            assert_eq!(Arc::strong_count(&k), held);
        }

        /// A raise is caught and logged
        #[test]
        fn a_raise_is_logged() {
            // No kernel task: its lookup raises.
            let f = fixture();
            f.d.device_link()
                .invalidate_shared_texture_backings(Hold::NONE, &[1]);
            let lines = f.lines();
            assert_eq!(lines.len(), 1);
            assert!(
                lines[0].starts_with("shared texture backings not invalidated for the guest: "),
                "{lines:?}"
            );
        }
    }
}
