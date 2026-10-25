//! Golden test: `WriterPrefRwLock` against libc++'s `std::shared_mutex`.
//!
//! The C++ side is compiled at test time with the host's `clang++` into a dylib and loaded with
//! `dlopen`, so this test needs no build script. Each scripted schedule runs once on
//! `std::shared_mutex` and once on `WriterPrefRwLock`, and the two event logs must be equal, and
//! equal to the schedule's expected log.
//!
//! A schedule steps on events, never on time: a thread reports that it holds the lock or has
//! started, a holder leaves when it is told to, and a writer has queued once a new read would
//! wait (`Lock::read_would_wait`). Whether a thread has reached its wait inside a lock is not
//! observable: a schedule that starts one without waiting for that has the same log either way.

#![cfg(target_os = "macos")] // Metal is macOS's: the crate is empty elsewhere
#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)] // A failure is the assertion

use std::ffi::{CString, c_void};
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use reims_vgpu_core::sync::WriterPrefRwLock;

const SHIM: &str = r#"
#include <shared_mutex>

extern "C" void *golden_sm_new() { return new std::shared_mutex; }
extern "C" void golden_sm_free(void *m) { delete static_cast<std::shared_mutex *>(m); }
extern "C" void golden_sm_lock(void *m) { static_cast<std::shared_mutex *>(m)->lock(); }
extern "C" bool golden_sm_try_lock(void *m) { return static_cast<std::shared_mutex *>(m)->try_lock(); }
extern "C" void golden_sm_unlock(void *m) { static_cast<std::shared_mutex *>(m)->unlock(); }
extern "C" void golden_sm_lock_shared(void *m) { static_cast<std::shared_mutex *>(m)->lock_shared(); }
extern "C" void golden_sm_unlock_shared(void *m) { static_cast<std::shared_mutex *>(m)->unlock_shared(); }
extern "C" bool golden_sm_try_lock_shared(void *m) { return static_cast<std::shared_mutex *>(m)->try_lock_shared(); }
"#;

/// The shim's entry points.
#[derive(Clone, Copy)]
struct Shim {
    new: unsafe extern "C" fn() -> *mut c_void,
    free: unsafe extern "C" fn(*mut c_void),
    lock: unsafe extern "C" fn(*mut c_void),
    try_lock: unsafe extern "C" fn(*mut c_void) -> bool,
    unlock: unsafe extern "C" fn(*mut c_void),
    lock_shared: unsafe extern "C" fn(*mut c_void),
    unlock_shared: unsafe extern "C" fn(*mut c_void),
    try_lock_shared: unsafe extern "C" fn(*mut c_void) -> bool,
}

/// Compiles and loads the shim once per test process. nextest runs each test in a process of
/// its own, several at once: each process builds its own copy, named by its pid, so that none
/// compiles or loads a file another is still writing.
fn shim() -> Shim {
    static S: OnceLock<[usize; 8]> = OnceLock::new();
    let a = *S.get_or_init(|| {
        // Cargo names CARGO_TARGET_TMPDIR only to the compile, and a path compiled in makes the
        // test another binary in each worktree (docs/development.md, "Building through kache"):
        // read it at run time, where a runner may set it, and take the system's temporary
        // directory where none does.
        let dir = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map_or_else(
                || std::env::temp_dir().join("reims-vgpu-metal"),
                PathBuf::from,
            )
            .join("golden_shared_mutex");
        std::fs::create_dir_all(&dir).unwrap();
        let pid = std::process::id();
        let src = dir.join(format!("sm_shim-{pid}.cc"));
        let lib = dir.join(format!("libsm_shim-{pid}.dylib"));
        std::fs::write(&src, SHIM).unwrap();
        let st = Command::new("xcrun")
            .args(["clang++", "-std=c++23", "-O2", "-shared", "-fPIC", "-o"])
            .arg(&lib)
            .arg(&src)
            .status()
            .expect("xcrun clang++");
        assert!(st.success(), "compiling the std::shared_mutex shim failed");
        let path = CString::new(lib.to_str().unwrap()).unwrap();
        // SAFETY: a NUL-terminated path; the handle is kept for the process's life.
        let h = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW) };
        assert!(!h.is_null(), "dlopen of the shim failed");
        // The loaded image outlives its file, and no other process reads these.
        std::fs::remove_file(&src).unwrap();
        std::fs::remove_file(&lib).unwrap();
        let names = [
            c"golden_sm_new",
            c"golden_sm_free",
            c"golden_sm_lock",
            c"golden_sm_try_lock",
            c"golden_sm_unlock",
            c"golden_sm_lock_shared",
            c"golden_sm_unlock_shared",
            c"golden_sm_try_lock_shared",
        ];
        names.map(|n| {
            // SAFETY: a NUL-terminated symbol name on a live handle.
            let p = unsafe { libc::dlsym(h, n.as_ptr()) };
            assert!(!p.is_null(), "dlsym {n:?} failed");
            p as usize
        })
    });
    // SAFETY: each address is the shim function of that name, whose C signature is the field's.
    unsafe {
        Shim {
            new: std::mem::transmute::<usize, unsafe extern "C" fn() -> *mut c_void>(a[0]),
            free: std::mem::transmute::<usize, unsafe extern "C" fn(*mut c_void)>(a[1]),
            lock: std::mem::transmute::<usize, unsafe extern "C" fn(*mut c_void)>(a[2]),
            try_lock: std::mem::transmute::<usize, unsafe extern "C" fn(*mut c_void) -> bool>(a[3]),
            unlock: std::mem::transmute::<usize, unsafe extern "C" fn(*mut c_void)>(a[4]),
            lock_shared: std::mem::transmute::<usize, unsafe extern "C" fn(*mut c_void)>(a[5]),
            unlock_shared: std::mem::transmute::<usize, unsafe extern "C" fn(*mut c_void)>(a[6]),
            try_lock_shared: std::mem::transmute::<usize, unsafe extern "C" fn(*mut c_void) -> bool>(
                a[7],
            ),
        }
    }
}

/// The operations a schedule uses, over either lock. Each runs `f` while holding.
trait Lock: Sync {
    fn with_read(&self, f: &mut dyn FnMut());
    fn with_write(&self, f: &mut dyn FnMut());
    /// `try_lock()`; on success runs `f` holding, then unlocks.
    fn try_write(&self, f: &mut dyn FnMut()) -> bool;
    /// Whether a new reader would wait now: a writer holds the lock or is queued for it. A
    /// schedule waits on it for a writer to have queued.
    fn read_would_wait(&self) -> bool;
}

/// libc++'s `std::shared_mutex`, through the shim.
struct Libcxx {
    s: Shim,
    m: usize,
}

impl Libcxx {
    fn new() -> Self {
        let s = shim();
        // SAFETY: `golden_sm_new` allocates a fresh mutex.
        let m = unsafe { (s.new)() } as usize;
        Libcxx { s, m }
    }
    fn p(&self) -> *mut c_void {
        self.m as *mut c_void
    }
}

impl Drop for Libcxx {
    fn drop(&mut self) {
        // SAFETY: the mutex is unlocked (every schedule joins its threads) and freed once.
        unsafe { (self.s.free)(self.p()) }
    }
}

impl Lock for Libcxx {
    fn with_read(&self, f: &mut dyn FnMut()) {
        // SAFETY: a live mutex; lock and unlock pair on this thread.
        unsafe { (self.s.lock_shared)(self.p()) };
        f();
        // SAFETY: as above.
        unsafe { (self.s.unlock_shared)(self.p()) };
    }
    fn with_write(&self, f: &mut dyn FnMut()) {
        // SAFETY: a live mutex; lock and unlock pair on this thread.
        unsafe { (self.s.lock)(self.p()) };
        f();
        // SAFETY: as above.
        unsafe { (self.s.unlock)(self.p()) };
    }
    fn try_write(&self, f: &mut dyn FnMut()) -> bool {
        // SAFETY: a live mutex; a successful try_lock is unlocked on this thread.
        let ok = unsafe { (self.s.try_lock)(self.p()) };
        if ok {
            f();
            // SAFETY: as above.
            unsafe { (self.s.unlock)(self.p()) };
        }
        ok
    }
    fn read_would_wait(&self) -> bool {
        // `try_lock_shared` fails once a writer has set `__write_entered_`, before it waits for
        // the readers to leave.
        // SAFETY: a live mutex; a successful try_lock_shared is unlocked on this thread.
        let ok = unsafe { (self.s.try_lock_shared)(self.p()) };
        if ok {
            // SAFETY: as above.
            unsafe { (self.s.unlock_shared)(self.p()) };
        }
        !ok
    }
}

/// The crate's own lock.
struct Port(WriterPrefRwLock);

impl Lock for Port {
    fn with_read(&self, f: &mut dyn FnMut()) {
        let _g = self.0.read();
        f();
    }
    fn with_write(&self, f: &mut dyn FnMut()) {
        let _g = self.0.write();
        f();
    }
    fn try_write(&self, f: &mut dyn FnMut()) -> bool {
        match self.0.try_write() {
            Some(_g) => {
                f();
                true
            }
            None => false,
        }
    }
    fn read_would_wait(&self) -> bool {
        // The lock shows no `try_read`; its `Debug` (lock_api's) tries one and prints
        // `<locked>` when it fails, which it does once a writer has set the writer bit, before
        // it waits for the readers to leave.
        format!("{:?}", self.0).contains("<locked>")
    }
}

/// How long a schedule waits for an event before it fails.
const PATIENCE: Duration = Duration::from_secs(10);

/// Waits for the event `rx` reports; fails the test after `PATIENCE`.
fn wait_for(what: &str, rx: &Receiver<()>) {
    if let Err(e) = rx.recv_timeout(PATIENCE) {
        panic!("{what}: {e}");
    }
}

/// Waits until `f` holds, polling every millisecond; fails the test after 10 000 polls.
fn wait_until(what: &str, f: impl Fn() -> bool) {
    for _ in 0..10_000 {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("timed out: {what}");
}

/// Runs `schedule` on both locks and checks both logs against `want`.
fn check(schedule: fn(&dyn Lock, Sender<&'static str>), want: &[&str]) {
    for (name, lock) in [
        ("libc++", Box::new(Libcxx::new()) as Box<dyn Lock>),
        (
            "WriterPrefRwLock",
            Box::new(Port(WriterPrefRwLock::default())),
        ),
    ] {
        let (tx, rx) = channel();
        schedule(&*lock, tx);
        let got: Vec<&str> = rx.iter().collect();
        assert_eq!(got, want, "{name}");
    }
}

/// Writer preference: a writer queued behind a reader blocks a new reader, and gets the lock
/// before it; `try_lock` fails while the writer is queued.
///
/// The writer starts once r1 holds, r2 once the writer has queued, and r1 leaves once
/// `try_lock` has answered. r2's read waits behind the queued writer whenever it arrives,
/// before or after r1 leaves.
#[test]
fn queued_writer_blocks_new_readers() {
    check(
        |l, tx| {
            std::thread::scope(|s| {
                let (r1_held, r1_holds) = channel();
                let (r1_leave, r1_may_leave) = channel();
                let (r2_start, r2_starts) = channel();
                let t = tx.clone();
                s.spawn(move || {
                    l.with_read(&mut || {
                        t.send("r1 in").unwrap();
                        r1_held.send(()).unwrap();
                        wait_for("r1's release", &r1_may_leave);
                        t.send("r1 out").unwrap();
                    })
                });
                wait_for("r1's read", &r1_holds);
                let t = tx.clone();
                s.spawn(move || {
                    l.with_write(&mut || {
                        t.send("w in").unwrap();
                        t.send("w out").unwrap();
                    })
                });
                wait_until("the writer queues", || l.read_would_wait());
                let t = tx.clone();
                s.spawn(move || {
                    r2_start.send(()).unwrap();
                    l.with_read(&mut || t.send("r2 in").unwrap());
                });
                wait_for("r2's start", &r2_starts);
                let ok = l.try_write(&mut || {});
                tx.send(if ok { "try ok" } else { "try busy" }).unwrap();
                r1_leave.send(()).unwrap();
            });
        },
        &["r1 in", "try busy", "r1 out", "w in", "w out", "r2 in"],
    );
}

/// Readers share; a writer waits for the last of them (`gate2`), not the first.
///
/// r2 reads while r1 holds, the writer queues behind both, and r1 leaves, with its unlock,
/// before r2 is told to.
#[test]
fn writer_waits_for_every_reader() {
    check(
        |l, tx| {
            std::thread::scope(|s| {
                let (r1_held, r1_holds) = channel();
                let (r1_leave, r1_may_leave) = channel();
                let (r2_held, r2_holds) = channel();
                let (r2_leave, r2_may_leave) = channel();
                let t = tx.clone();
                let r1 = s.spawn(move || {
                    l.with_read(&mut || {
                        t.send("r1 in").unwrap();
                        r1_held.send(()).unwrap();
                        wait_for("r1's release", &r1_may_leave);
                        t.send("r1 out").unwrap();
                    })
                });
                wait_for("r1's read", &r1_holds);
                let t = tx.clone();
                s.spawn(move || {
                    l.with_read(&mut || {
                        t.send("r2 in").unwrap();
                        r2_held.send(()).unwrap();
                        wait_for("r2's release", &r2_may_leave);
                        t.send("r2 out").unwrap();
                    })
                });
                wait_for("r2's read beside r1's", &r2_holds);
                let t = tx.clone();
                s.spawn(move || l.with_write(&mut || t.send("w in").unwrap()));
                wait_until("the writer queues", || l.read_would_wait());
                r1_leave.send(()).unwrap();
                // r1's thread ends after its unlock; the writer still waits for r2.
                r1.join().unwrap();
                r2_leave.send(()).unwrap();
            });
        },
        &["r1 in", "r2 in", "r1 out", "r2 out", "w in"],
    );
}

/// A writer holding blocks readers until `unlock()`, which wakes all of them together.
///
/// The readers start once the writer holds, and the writer leaves once both have started.
/// Each reader, once in, holds until the other is in too, so the two hold the lock at once. No
/// read is granted while the writer holds, so the log is the same whether or not a reader has
/// reached its wait when the writer leaves.
#[test]
fn unlock_releases_every_waiting_reader() {
    check(
        |l, tx| {
            std::thread::scope(|s| {
                let (w_held, w_holds) = channel();
                let (w_leave, w_may_leave) = channel();
                let (started, readers_start) = channel();
                let (a_in, a_is_in) = channel();
                let (b_in, b_is_in) = channel();
                let t = tx.clone();
                s.spawn(move || {
                    l.with_write(&mut || {
                        t.send("w in").unwrap();
                        w_held.send(()).unwrap();
                        wait_for("the writer's release", &w_may_leave);
                        t.send("w out").unwrap();
                    })
                });
                wait_for("the write", &w_holds);
                for (mine, theirs) in [(a_in, b_is_in), (b_in, a_is_in)] {
                    let (t, started) = (tx.clone(), started.clone());
                    s.spawn(move || {
                        started.send(()).unwrap();
                        l.with_read(&mut || {
                            t.send("r in").unwrap();
                            mine.send(()).unwrap();
                            wait_for("the other reader, beside this one", &theirs);
                        });
                    });
                }
                wait_for("a reader's start", &readers_start);
                wait_for("a reader's start", &readers_start);
                w_leave.send(()).unwrap();
            });
        },
        &["w in", "w out", "r in", "r in"],
    );
}

/// `try_lock` succeeds only on `__state_ == 0`.
#[test]
fn try_lock_matrix() {
    check(
        |l, tx| {
            let ok = l.try_write(&mut || {
                let inner = l.try_write(&mut || {});
                tx.send(if inner { "nested ok" } else { "nested busy" })
                    .unwrap();
            });
            tx.send(if ok { "free ok" } else { "free busy" }).unwrap();
            l.with_read(&mut || {
                let ok = l.try_write(&mut || {});
                tx.send(if ok { "read ok" } else { "read busy" }).unwrap();
            });
            std::thread::scope(|s| {
                let (held, w_holds) = channel();
                let (leave, w_may_leave) = channel();
                s.spawn(move || {
                    l.with_write(&mut || {
                        held.send(()).unwrap();
                        wait_for("the writer's release", &w_may_leave);
                    })
                });
                wait_for("the write", &w_holds);
                let ok = l.try_write(&mut || {});
                tx.send(if ok { "write ok" } else { "write busy" }).unwrap();
                leave.send(()).unwrap();
            });
            let ok = l.try_write(&mut || {});
            tx.send(if ok { "after ok" } else { "after busy" }).unwrap();
        },
        &[
            "nested busy",
            "free ok",
            "read busy",
            "write busy",
            "after ok",
        ],
    );
}
