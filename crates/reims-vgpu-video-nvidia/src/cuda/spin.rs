//! A GPU that does not finish, for the tests of the crate's bounded waits
//! (`crate::wait`): a CUDA kernel that spins on a flag in host memory
//! until the host sets it. Queued on a stream ahead of other work, it holds
//! that work back for as long.
//!
//! The kernel is PTX, which the driver compiles for the GPU at load
//! (`cuModuleLoadData`). The flag is page-locked host memory mapped into
//! the GPU's address space (`CU_MEMHOSTALLOC_DEVICEMAP`), which the kernel
//! reads with volatile loads, so that it sees the host's write. A spin
//! still held after [`HELD_AT_MOST`] is let go by a thread of its own, so
//! that a test whose wait does not end fails rather than hangs.
//!
//! **It ends on its own**, whatever the host does: after
//! [`SPUN_AT_MOST`] by the GPU's own clock (`%globaltimer`), or after
//! [`TURNS_AT_MOST`] reads of the flag, whichever comes first, so that a
//! process that never sets the flag holds the GPU for seconds, not for
//! good. It faults nothing: one thread reads four bytes of the host's and
//! writes what it counted beside them as it ends ([`Spin::ran`]).

use std::ffi::{c_char, c_uint, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use reims_vgpu_video_core::runtime::StdRuntime;

use super::Gpu;
use super::ffi::{CUdeviceptr, CUresult, CUstream};
use crate::dl::{Library, functions};
use crate::error::Error;

type CUmodule = *mut c_void;
type CUfunction = *mut c_void;

/// `CU_MEMHOSTALLOC_DEVICEMAP`: the memory is mapped into the GPU's
/// address space.
const CU_MEMHOSTALLOC_DEVICEMAP: c_uint = 0x02;

/// How long a spin is held at most.
pub(crate) const HELD_AT_MOST: Duration = Duration::from_secs(3);

/// How long the kernel spins at most, by the GPU's clock, whether or not
/// the flag is ever set: 10 s (`KERNEL`'s bound).
pub(crate) const SPUN_AT_MOST: Duration = Duration::from_secs(10);

/// How many times the kernel reads the flag at most: 6 million, 5.6 s at
/// the 0.93 µs a read of host memory took on an RTX 5080 Laptop GPU
/// (driver 615.71.09, 2026-10-25: 431,885 reads in 400 ms), and 12 s at
/// 2 µs, should the GPU's clock not move (`KERNEL`'s bound).
pub(crate) const TURNS_AT_MOST: u64 = 6_000_000;

functions! {
    /// libcuda's functions that load and launch a kernel, written from the
    /// CUDA driver API's reference.
    struct SpinFunctions {
        cuModuleLoadData: fn(*mut CUmodule, *const c_void) -> CUresult = c"cuModuleLoadData";
        cuModuleGetFunction: fn(*mut CUfunction, CUmodule, *const c_char) -> CUresult
            = c"cuModuleGetFunction";
        cuModuleUnload: fn(CUmodule) -> CUresult = c"cuModuleUnload";
        cuMemHostGetDevicePointer_v2: fn(*mut CUdeviceptr, *mut c_void, c_uint) -> CUresult
            = c"cuMemHostGetDevicePointer_v2";
        cuLaunchKernel: fn(
            CUfunction, c_uint, c_uint, c_uint, c_uint, c_uint, c_uint, c_uint, CUstream,
            *mut *mut c_void, *mut *mut c_void
        ) -> CUresult = c"cuLaunchKernel";
        cuCtxSynchronize: fn() -> CUresult = c"cuCtxSynchronize";
    }
}

/// One thread that loads the flag until it is not 0, or until it has
/// spun [`SPUN_AT_MOST`] by `%globaltimer` (ns) or read it
/// [`TURNS_AT_MOST`] times; then it writes the reads it made and the
/// nanoseconds it spun at bytes 8 and 16 from the flag.
const KERNEL: &std::ffi::CStr = c"
.version 7.0
.target sm_50
.address_size 64

.visible .entry spin(.param .u64 flag)
{
    .reg .pred %p;
    .reg .b32 %r;
    .reg .b64 %a, %start, %spun, %turns;
    ld.param.u64 %a, [flag];
    cvta.to.global.u64 %a, %a;
    mov.u64 %start, %globaltimer;
    mov.u64 %turns, 0;
WAIT:
    ld.volatile.global.u32 %r, [%a];
    add.u64 %turns, %turns, 1;
    mov.u64 %spun, %globaltimer;
    sub.u64 %spun, %spun, %start;
    setp.ne.u32 %p, %r, 0;
    @%p bra DONE;
    setp.ge.u64 %p, %turns, 6000000;
    @%p bra DONE;
    setp.lt.u64 %p, %spun, 10000000000;
    @%p bra WAIT;
DONE:
    st.volatile.global.u64 [%a+8], %turns;
    st.volatile.global.u64 [%a+16], %spun;
    ret;
}
";

/// The kernel, loaded on a GPU's context, and its flag.
pub(crate) struct Spin {
    gpu: Arc<Gpu>,
    f: SpinFunctions,
    module: CUmodule,
    function: CUfunction,
    /// The flag, in page-locked host memory: 0 holds the kernel.
    flag: Arc<Flag>,
    on_gpu: CUdeviceptr,
    _library: Library,
}

/// The flag, then what the kernel counted (`KERNEL`): 24 bytes of
/// page-locked host memory, freed as the last holder drops it: the memory
/// is its GPU context's, which it keeps alive till then, as the context's
/// end frees it.
struct Flag {
    memory: *mut c_void,
    gpu: Arc<Gpu>,
}

// SAFETY: page-locked host memory, which the host reaches only through
// atomics.
unsafe impl Send for Flag {}
// SAFETY: as above.
unsafe impl Sync for Flag {}

impl Flag {
    fn atomic(&self) -> &AtomicU32 {
        // SAFETY: four aligned bytes of page-locked memory, live while
        // `self` is (`Drop`).
        unsafe { &*self.memory.cast::<AtomicU32>() }
    }

    /// The kernel's count at byte `at`, 8 or 16.
    fn count(&self, at: usize) -> u64 {
        // SAFETY: eight bytes at an 8-aligned offset of the page-aligned
        // 24 bytes allocated, live while `self` is.
        let word = unsafe { self.memory.cast::<u8>().add(at) };
        // SAFETY: as above.
        unsafe { &*word.cast::<AtomicU64>() }.load(Ordering::SeqCst)
    }
}

impl Drop for Flag {
    fn drop(&mut self) {
        let cuda = self.gpu.cuda();
        let Ok(_current) = self.gpu.push() else {
            return;
        };
        // SAFETY: allocated by cuMemHostAlloc on the context, freed once,
        // with no kernel reading it (`Spin`'s `Drop`).
        unsafe { (cuda.f.cuMemFreeHost)(self.memory) };
    }
}

impl Spin {
    /// Loads the kernel on `gpu`'s context.
    pub(crate) fn new(gpu: &Arc<Gpu>) -> Result<Self, Error> {
        let cuda = gpu.cuda();
        let library = Library::open(c"libcuda.so.1")?;
        let f = SpinFunctions::load(&library)?;
        let _current = gpu.push()?;
        let mut module: CUmodule = std::ptr::null_mut();
        // SAFETY: NUL-terminated PTX; the pointer is to a live handle.
        cuda.check("cuModuleLoadData", unsafe {
            (f.cuModuleLoadData)(&raw mut module, KERNEL.as_ptr().cast())
        })?;
        let mut function: CUfunction = std::ptr::null_mut();
        // SAFETY: the module just loaded, and the name of its kernel.
        cuda.check("cuModuleGetFunction", unsafe {
            (f.cuModuleGetFunction)(&raw mut function, module, c"spin".as_ptr())
        })?;
        let mut memory: *mut c_void = std::ptr::null_mut();
        // SAFETY: the pointer is to a live pointer; the flag and the two
        // counts.
        cuda.check("cuMemHostAlloc", unsafe {
            (cuda.f.cuMemHostAlloc)(&raw mut memory, 24, CU_MEMHOSTALLOC_DEVICEMAP)
        })?;
        let flag = Arc::new(Flag {
            memory,
            gpu: Arc::clone(gpu),
        });
        flag.atomic().store(1, Ordering::SeqCst);
        let mut on_gpu: CUdeviceptr = 0;
        // SAFETY: the memory just allocated, mapped for the GPU.
        cuda.check("cuMemHostGetDevicePointer", unsafe {
            (f.cuMemHostGetDevicePointer_v2)(&raw mut on_gpu, flag.memory, 0)
        })?;
        Ok(Self {
            gpu: Arc::clone(gpu),
            f,
            module,
            function,
            flag,
            on_gpu,
            _library: library,
        })
    }

    /// Queues the kernel on `stream`, held until [`Self::release`] or
    /// [`HELD_AT_MOST`] from now.
    pub(crate) fn hold(&self, stream: CUstream) -> Result<(), Error> {
        let cuda = self.gpu.cuda();
        self.flag.atomic().store(0, Ordering::SeqCst);
        let _current = self.gpu.push()?;
        let mut argument = self.on_gpu;
        let mut arguments = [(&raw mut argument).cast::<c_void>()];
        // SAFETY: the loaded kernel, one thread, its one argument the
        // flag's address on the GPU; a live stream of the context.
        cuda.check("cuLaunchKernel", unsafe {
            (self.f.cuLaunchKernel)(
                self.function,
                1,
                1,
                1,
                1,
                1,
                1,
                0,
                stream,
                arguments.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        })?;
        let flag = Arc::clone(&self.flag);
        StdRuntime::thread("test-spin-release", move || {
            std::thread::sleep(HELD_AT_MOST);
            flag.atomic().store(1, Ordering::SeqCst);
        })
        .map_err(|e| Error::Unsupported(format!("no thread lets the spin go: {e}")))?;
        Ok(())
    }

    /// Lets the kernel end.
    pub(crate) fn release(&self) {
        self.flag.atomic().store(1, Ordering::SeqCst);
    }

    /// What the kernel last held counted as it ended: how many times it
    /// read the flag, and for how long it spun by the GPU's clock.
    pub(crate) fn ran(&self) -> (u64, Duration) {
        (
            self.flag.count(8),
            Duration::from_nanos(self.flag.count(16)),
        )
    }
}

impl Drop for Spin {
    fn drop(&mut self) {
        self.release();
        let Ok(_current) = self.gpu.push() else {
            return;
        };
        // SAFETY: the context is current; the kernel ends with the flag
        // set.
        unsafe { (self.f.cuCtxSynchronize)() };
        // SAFETY: the module loaded, unloaded once, its kernel done.
        unsafe { (self.f.cuModuleUnload)(self.module) };
        // The flag's memory goes with its last holder, this or the release
        // thread.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kernel_ends_by_its_bounds() {
        let kernel = KERNEL.to_str().expect("PTX is ASCII");
        let spun = format!("%spun, {};", SPUN_AT_MOST.as_nanos());
        let turns = format!("%turns, {};", TURNS_AT_MOST);
        assert!(kernel.contains(&spun), "no {spun}");
        assert!(kernel.contains(&turns), "no {turns}");
    }
}
