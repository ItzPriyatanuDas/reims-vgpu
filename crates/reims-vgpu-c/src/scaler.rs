//! The scaler device: the host side of the paravirtualized M2 scaler device (virtio-PCI
//! 106b:1a0b), [`RvgScalerDeviceConfig`], and the `rvg_scaler_device_*` functions.

use std::ffi::{c_char, c_void};
use std::sync::Arc;

use reims_vgpu::{GuestMemory, ScalerDevice, ScalerDeviceConfig, ScalerOutcome};

use crate::ffi;
use crate::handle;
use crate::iosurface::RvgIOSurfaceDevice;
use crate::sized::{self, HostStruct};
use crate::status::{RVG_ERR_INVALID_ARGUMENT, RVG_OK, RvgStatus};
use crate::table::{RvgDestroyFn, Table, WithTable};
use crate::{Plat, null_handle};

/// The PCI vendor ID of the scaler device: Apple's.
pub const RVG_SCALER_PCI_VENDOR_ID: u16 = 0x106b;

/// The PCI device ID of the scaler device.
pub const RVG_SCALER_PCI_DEVICE_ID: u16 = 0x1a0b;

/// The scaler device's virtqueues: one, the guest's requests, each answered in place.
pub const RVG_SCALER_QUEUE_COUNT: u32 = 1;

/// What rvg_scaler_device_process tells the host to do with an element: one of the
/// `RVG_SCALER_*` outcomes.
pub type RvgScalerOutcome = u32;

/// Write the request, which the call rewrote in place, into the element's writable part, with a
/// used length of the request's.
pub const RVG_SCALER_REPLY: RvgScalerOutcome = 0;

/// Set DEVICE_NEEDS_RESET and raise the configuration interrupt, with no reply: Apple's host's
/// answer to an element it refuses.
pub const RVG_SCALER_NEEDS_RESET: RvgScalerOutcome = 1;

/// End the VM, as Apple's host ends its process.
pub const RVG_SCALER_END_VM: RvgScalerOutcome = 2;

/// `is_ram(ctx, address, length)`: whether all of the guest-physical range is the guest's RAM; a
/// length of 0 asks of the address alone.
///
/// Thread: the host's own, inside rvg_scaler_device_process. May call: nothing of this header.
/// Without it, nothing is RAM, and every request that names guest memory is refused.
pub type RvgScalerIsRamFn =
    Option<unsafe extern "C-unwind" fn(ctx: *mut c_void, address: u64, length: u64) -> bool>;

/// `read(ctx, address, bytes, length)`: reads `length` bytes of the guest's RAM at the
/// guest-physical `address` into `bytes`; false, reading nothing, unless all of them are RAM.
///
/// Thread: as is_ram. Without it, nothing is read.
pub type RvgScalerReadFn = Option<
    unsafe extern "C-unwind" fn(
        ctx: *mut c_void,
        address: u64,
        bytes: *mut u8,
        length: usize,
    ) -> bool,
>;

/// `write(ctx, address, bytes, length)`: writes `length` bytes at `bytes` into the guest's RAM at
/// the guest-physical `address`; false, writing nothing, unless all of them are RAM.
///
/// Thread: as is_ram. Without it, nothing is written.
pub type RvgScalerWriteFn = Option<
    unsafe extern "C-unwind" fn(
        ctx: *mut c_void,
        address: u64,
        bytes: *const u8,
        length: usize,
    ) -> bool,
>;

/// The guest's memory, as the scaler device reaches it: the capabilities a type 1 request
/// names, and a transform's side buffers. The addresses are the device's DMA addresses, which a
/// host without an IOMMU takes as guest-physical.
#[repr(C)]
#[derive(Clone, Copy)]
#[expect(
    missing_docs,
    reason = "the fields' names say what they hold, as the header shows them"
)]
pub struct RvgScalerGuestMemory {
    /// sizeof(RvgScalerGuestMemory) as the host was compiled.
    pub struct_size: u32,
    pub ctx: *mut c_void,
    /// Runs once after the scaler device's end, when its functions run no more.
    pub destroy: RvgDestroyFn,
    pub is_ram: RvgScalerIsRamFn,
    pub read: RvgScalerReadFn,
    pub write: RvgScalerWriteFn,
}

// SAFETY: repr(C), a u32 first; a pointer and optional function pointers.
unsafe impl HostStruct for RvgScalerGuestMemory {}

/// The scaler device's config.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RvgScalerDeviceConfig {
    /// sizeof(RvgScalerDeviceConfig) as the host was compiled.
    pub struct_size: u32,
    /// The IOSurface device whose mapping IDs the guest's transforms name, borrowed for the
    /// call, as RvgVideoDeviceConfig's: NULL is the process's latest at each lookup.
    pub iosurface_device: *const RvgIOSurfaceDevice,
    /// NULL is a table with every function absent.
    pub guest_memory: *const RvgScalerGuestMemory,
}

// SAFETY: repr(C), a u32 first; pointers.
unsafe impl HostStruct for RvgScalerDeviceConfig {}

/// The scaler device.
pub struct RvgScalerDevice {
    device: WithTable<ScalerDevice<Plat>>,
    memory: HostMemory,
}

/// The host's guest-memory functions, over their table.
struct HostMemory {
    table: Arc<Table>,
    is_ram: RvgScalerIsRamFn,
    read: RvgScalerReadFn,
    write: RvgScalerWriteFn,
}

impl GuestMemory for HostMemory {
    fn is_ram(&self, address: u64, length: u64) -> bool {
        let Some(f) = self.is_ram else {
            return false;
        };
        // SAFETY: the host's function over its ctx.
        ffi::host_call("is_ram", || unsafe { f(self.table.ctx(), address, length) })
    }

    fn read(&self, address: u64, bytes: &mut [u8]) -> bool {
        let Some(f) = self.read else {
            return false;
        };
        let (at, length) = (bytes.as_mut_ptr(), bytes.len());
        // SAFETY: the host's function over its ctx; `bytes` is writable for the call.
        ffi::host_call("read", || unsafe {
            f(self.table.ctx(), address, at, length)
        })
    }

    fn write(&self, address: u64, bytes: &[u8]) -> bool {
        let Some(f) = self.write else {
            return false;
        };
        let (at, length) = (bytes.as_ptr(), bytes.len());
        // SAFETY: the host's function over its ctx; `bytes` is readable for the call.
        ffi::host_call("write", || unsafe {
            f(self.table.ctx(), address, at, length)
        })
    }
}

/// The scaler device behind a handle, or the process's end for NULL.
///
/// # Safety
/// `device` is NULL or a live scaler device handle.
unsafe fn live<'a>(device: *const RvgScalerDevice, call: &'static str) -> &'a RvgScalerDevice {
    // SAFETY: the caller's contract.
    unsafe { handle::get(device) }.unwrap_or_else(|| null_handle(call))
}

/// How a device is made: on the host's GPU, or, in the tests, on a backend of theirs.
type Make<'a> = &'a dyn Fn(ScalerDeviceConfig<Plat>) -> ScalerDevice<Plat>;

/// rvg_scaler_device_create, with the device made by `make`.
///
/// # Safety
/// As rvg_scaler_device_create's.
unsafe fn create(
    config: *const RvgScalerDeviceConfig,
    device: *mut *mut RvgScalerDevice,
    make: Make<'_>,
) -> RvgStatus {
    // SAFETY: the caller's contract.
    let c = match unsafe { sized::read(config) } {
        Ok(Some(c)) => c,
        Ok(None) => return RVG_ERR_INVALID_ARGUMENT,
        Err(s) => return s,
    };
    // SAFETY: the config's contract: NULL or a live table.
    let memory = match unsafe { sized::read_or_zero(c.guest_memory) } {
        Ok(t) => t,
        Err(s) => return s,
    };
    // From here the table is the library's: a refusal ends it before it returns.
    let table = Table::new(memory.ctx, memory.destroy);
    if device.is_null() {
        table.end();
        return RVG_ERR_INVALID_ARGUMENT;
    }
    // SAFETY: the config's contract: NULL or a live IOSurface device handle.
    let iosurface_device = unsafe { handle::get(c.iosurface_device) }.map(|d| d.device.clone_ref());
    let config = ScalerDeviceConfig { iosurface_device };
    let d = make(config);
    let h = handle::new(RvgScalerDevice {
        device: WithTable::new(d, Arc::clone(&table)),
        memory: HostMemory {
            table,
            is_ram: memory.is_ram,
            read: memory.read,
            write: memory.write,
        },
    });
    // SAFETY: checked non-NULL; the caller's contract.
    unsafe { device.write(h) };
    RVG_OK
}

/// A new scaler device, with no accelerator until rvg_scaler_device_start, transforming with a
/// kernel of the library's own on the GPU the library draws with, which it logs: Metal's on
/// macOS, the Vulkan platform's on Linux, any GPU, a software rasterizer included. Made on every
/// host: where that opens no device, or the device refuses the kernel, one that does not scale
/// (rvg_scaler_device_scales). Refusals: RVG_ERR_INVALID_ARGUMENT, RVG_ERR_UNSUPPORTED. Calls
/// back: never, but a refusal's destroy.
///
/// # Safety
/// `config` is NULL or a live config; `device` is NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rvg_scaler_device_create(
    config: *const RvgScalerDeviceConfig,
    device: *mut *mut RvgScalerDevice,
) -> RvgStatus {
    ffi::export(|| {
        let make = |config| {
            ScalerDevice::with_platform(<Plat as reims_vgpu::ApiPlatform>::process(), config)
        };
        // SAFETY: the caller's contract.
        unsafe { create(config, device, &make) }
    })
}

/// Drops the host's reference; the last one resets the device and ends its guest-memory table.
///
/// # Safety
/// `device` is NULL or a live scaler device handle, not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rvg_scaler_device_release(device: *mut RvgScalerDevice) {
    ffi::export(|| {
        // SAFETY: the caller's contract.
        unsafe { handle::release(device) }
    })
}

/// Whether the device transforms: false where the host has no GPU for it, which the library
/// logged at its create. The host then refuses the guest's features (FEATURES_OK
/// left unset), so that the guest's driver fails its start before DRIVER_OK and does without the
/// device, as in a VM without it; were the guest to go on, its start would be a needs-reset, as
/// Apple's host answers where it cannot make an accelerator. Calls back: never.
///
/// # Safety
/// `device` is a live scaler device handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rvg_scaler_device_scales(device: *const RvgScalerDevice) -> bool {
    ffi::export(|| {
        // SAFETY: the caller's contract.
        unsafe { live(device, "rvg_scaler_device_scales") }
            .device
            .scales()
    })
}

/// The guest's DRIVER_OK: a new accelerator, replacing the one there was. False where the host
/// cannot make one, which the library logs: the host then sets DEVICE_NEEDS_RESET, as Apple's
/// does. Calls back: never.
///
/// # Safety
/// `device` is a live scaler device handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rvg_scaler_device_start(device: *mut RvgScalerDevice) -> bool {
    ffi::export(|| {
        // SAFETY: the caller's contract.
        unsafe { live(device, "rvg_scaler_device_start") }
            .device
            .start()
            .is_ok()
    })
}

/// One element of the queue: `length` bytes at `request`, its readable part, which the call may
/// rewrite in place, and `writable`, the size of its writable part. Answers what the host does
/// with it (`RVG_SCALER_*`): on RVG_SCALER_REPLY, `request` holds the reply, `length` bytes. On
/// the others, Apple's reason goes into `reason`, NUL-terminated and cut to `reason_size`
/// bytes, where it is not NULL. May call back: the guest-memory functions, on this thread. May
/// block on the IOSurface device's thread and on the GPU.
///
/// # Safety
/// `device` is a live scaler device handle; `request` is readable and writable for `length`
/// bytes, or NULL with `length` 0; `reason` is NULL or writable for `reason_size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rvg_scaler_device_process(
    device: *mut RvgScalerDevice,
    request: *mut u8,
    length: usize,
    writable: usize,
    reason: *mut c_char,
    reason_size: usize,
) -> RvgScalerOutcome {
    ffi::export(|| {
        // SAFETY: the caller's contract.
        let d = unsafe { live(device, "rvg_scaler_device_process") };
        let bytes = if request.is_null() || length == 0 {
            &mut [][..]
        } else {
            // SAFETY: the caller's contract: `length` readable and writable bytes at `request`.
            unsafe { std::slice::from_raw_parts_mut(request, length) }
        };
        let (outcome, why) = match d.device.process(bytes, writable, &d.memory) {
            ScalerOutcome::Reply(reply) => {
                let n = reply.len().min(bytes.len());
                bytes[..n].copy_from_slice(&reply[..n]);
                return RVG_SCALER_REPLY;
            }
            ScalerOutcome::NeedsReset(why) => (RVG_SCALER_NEEDS_RESET, why),
            ScalerOutcome::EndVm(why) => (RVG_SCALER_END_VM, why),
        };
        if !reason.is_null() && reason_size > 0 {
            let n = why.len().min(reason_size - 1);
            // SAFETY: the caller's contract: `reason_size` writable bytes at `reason`, and `n`
            // is below it.
            unsafe {
                std::ptr::copy_nonoverlapping(why.as_ptr(), reason.cast::<u8>(), n);
                reason.add(n).write(0);
            }
        }
        outcome
    })
}

/// A reset or a stop: the accelerator goes; the next request is a needs-reset until
/// rvg_scaler_device_start. Calls back: never.
///
/// # Safety
/// `device` is a live scaler device handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rvg_scaler_device_reset(device: *mut RvgScalerDevice) {
    ffi::export(|| {
        // SAFETY: the caller's contract.
        unsafe { live(device, "rvg_scaler_device_reset") }
            .device
            .reset();
    })
}

#[cfg(test)]
mod tests;
