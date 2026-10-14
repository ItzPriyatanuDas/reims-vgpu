//! The scheduling hints a FIFO thread gives Linux ([`hint_fifo_thread`]): the kernel still
//! places it, on any CPU it may run on.
//!
//! A FIFO thread runs the guest's commands one after another, so wherever the GPU keeps up its
//! speed is the frame rate. Linux offers an unprivileged thread hints about itself, none of which
//! binds it to a CPU:
//! - **A utilization floor** (`sched_setattr`'s `SCHED_FLAG_UTIL_CLAMP_MIN`, kernel docs
//!   "Utilization Clamping"): the thread asks to be treated as needing the CPU's full capacity
//!   while it runs. Capacity-aware scheduling, which the kernel runs on CPUs of unequal capacity
//!   (`intel_pstate` turns it on for a hybrid processor without SMT), then prefers a performance
//!   core when it wakes the thread and moves it there when one frees up; under `schedutil` the
//!   CPU it runs on is clocked up while it is runnable. Neither is a bound: where no performance
//!   core is free it runs where the kernel puts it. On a hybrid processor with SMT, under
//!   `intel_pstate`'s or `amd-pstate`'s own P-state selection (active mode), and on a CPU of one
//!   type, the kernel reads it for none of these.
//! - **What is not given.** A lower nice needs `CAP_SYS_NICE` or `RLIMIT_NICE`, and where the host
//!   sets one (CachyOS's `ananicy-cpp` gives QEMU nice 9) a lower one would undo its policy; so
//!   the thread keeps its policy and nice. EEVDF's own slice (`sched_runtime`) is a latency hint
//!   the kernel docs give no throughput for, and is left as it is. Affinity is never set: the
//!   kernel balances the threads of every process over every CPU.
//!
//! Measured, as an unprivileged user, at nice 0 and 9: the kernel takes the floor and reads it
//! back on CachyOS's 7.2.9 and 7.2.8 kernels (i9-13900H; Core Ultra 9 275HX) and Ubuntu's 7.0.0
//! (Ryzen 7 7840HS), all built with `CONFIG_UCLAMP_TASK`, with the user's cgroup's
//! `cpu.uclamp.min` at 0 and `max` at `max` and `kernel.sched_util_clamp_min` at 1024. All three
//! run their CPUs' own P-state selection (`intel_pstate` and `amd-pstate-epp` in active mode).
//! Only the 275HX, a hybrid without SMT, runs capacity-aware scheduling (its `cpu_capacity` 1024,
//! 991 and 747); the i9-13900H, a hybrid with SMT, reads 1024 on every CPU, and there the FIFO
//! thread that runs WindowServer's commands spent 43-61% of a Finder drag on the efficiency
//! cores with the floor or without it. A kernel without `CONFIG_UCLAMP_TASK` refuses the call,
//! and the thread runs as it would have. The first hint's outcome is logged once.

use std::sync::atomic::{AtomicBool, Ordering};

/// `SCHED_FLAG_KEEP_POLICY`: the thread keeps its policy.
const KEEP_POLICY: u64 = 0x08;
/// `SCHED_FLAG_KEEP_PARAMS`: the thread keeps its nice and its slice.
const KEEP_PARAMS: u64 = 0x10;
/// `SCHED_FLAG_UTIL_CLAMP_MIN`: the request sets the utilization floor.
const UTIL_CLAMP_MIN: u64 = 0x20;
/// The floor asked for: the CPU's full capacity (`SCHED_CAPACITY_SCALE`).
const FULL_CAPACITY: u32 = 1024;

/// `struct sched_attr` as `sched_setattr` takes it, with the utilization clamps
/// (`SCHED_ATTR_SIZE_VER1`, 56 bytes).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct SchedAttr {
    size: u32,
    sched_policy: u32,
    sched_flags: u64,
    sched_nice: i32,
    sched_priority: u32,
    sched_runtime: u64,
    sched_deadline: u64,
    sched_period: u64,
    sched_util_min: u32,
    sched_util_max: u32,
}

/// Gives the calling thread, a FIFO thread, its scheduling hints (the module docs), keeping its
/// policy, nice and CPUs. The first call logs what the kernel took.
pub(crate) fn hint_fifo_thread() {
    let line = match set_util_floor() {
        Ok(()) => format!(
            "FIFO threads ask the scheduler for a utilization floor of {FULL_CAPACITY} of 1024"
        ),
        Err(e) => {
            format!("FIFO threads give the scheduler no hint: sched_setattr refused it ({e})")
        }
    };
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        log::info!("{line}");
    }
}

/// The request that sets the calling thread's utilization floor to [`FULL_CAPACITY`], leaving its
/// policy, nice and slice as they are.
fn util_floor_request() -> SchedAttr {
    SchedAttr {
        size: u32::try_from(size_of::<SchedAttr>()).unwrap_or(u32::MAX),
        sched_flags: KEEP_POLICY | KEEP_PARAMS | UTIL_CLAMP_MIN,
        sched_util_min: FULL_CAPACITY,
        ..SchedAttr::default()
    }
}

/// Sets the calling thread's utilization floor ([`util_floor_request`]).
fn set_util_floor() -> std::io::Result<()> {
    let request = util_floor_request();
    // SAFETY: the calling thread's attributes, from a live `sched_attr` of the size it names.
    let rc = unsafe { libc::syscall(libc::SYS_sched_setattr, 0, std::ptr::from_ref(&request), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    /// The calling thread's attributes, as `sched_getattr` reads them.
    fn read_back() -> SchedAttr {
        let mut attr = SchedAttr::default();
        // SAFETY: the calling thread's attributes, into a live `sched_attr` of the size given.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_sched_getattr,
                0,
                std::ptr::from_mut(&mut attr),
                size_of::<SchedAttr>(),
                0,
            )
        };
        assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
        attr
    }

    /// The request is the 56-byte `sched_attr` that sets the floor alone
    #[test]
    fn the_request_sets_only_the_floor() {
        assert_eq!(size_of::<SchedAttr>(), 56);
        let request = util_floor_request();
        assert_eq!(request.size, 56);
        assert_eq!(
            request.sched_flags,
            KEEP_POLICY | KEEP_PARAMS | UTIL_CLAMP_MIN
        );
        assert_eq!(request.sched_util_min, FULL_CAPACITY);
    }

    /// A FIFO thread runs with the floor, its policy, nice and CPUs its creator's
    #[test]
    fn a_fifo_thread_keeps_its_policy_nice_and_cpus() {
        use reims_vgpu_core::platform::os::Threads;

        let creator = read_back();
        // SAFETY: a zeroed set, filled by the kernel for the calling thread.
        let mut creator_cpus: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        // SAFETY: the calling thread's mask, into a set of the size given.
        let rc =
            unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut creator_cpus) };
        assert_eq!(rc, 0);
        let (tx, rx) = mpsc::channel();
        super::super::LinuxThreads::new().spawn(
            "reims-vgpu.fifo",
            512 * 1024,
            Box::new(move || {
                // SAFETY: a zeroed set, filled by the kernel for the calling thread.
                let mut cpus: libc::cpu_set_t = unsafe { std::mem::zeroed() };
                // SAFETY: the calling thread's mask, into a set of the size given.
                let rc =
                    unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut cpus) };
                assert_eq!(rc, 0);
                // SAFETY: two live sets.
                let same = unsafe { libc::CPU_EQUAL(&cpus, &creator_cpus) };
                tx.send((read_back(), same)).unwrap();
            }),
        );
        let (attr, same_cpus) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(attr.sched_util_min, FULL_CAPACITY);
        assert_eq!(attr.sched_policy, creator.sched_policy);
        assert_eq!(attr.sched_nice, creator.sched_nice);
        assert_eq!(attr.sched_runtime, creator.sched_runtime);
        assert!(same_cpus);
    }
}
