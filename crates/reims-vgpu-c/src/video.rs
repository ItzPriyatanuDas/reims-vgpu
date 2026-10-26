//! The video device: the host side of the paravirtualized VideoToolbox device (virtio-PCI
//! 106b:1a03), [`RvgVideoDeviceConfig`], and the `rvg_video_device_*` functions.

use std::ffi::c_void;
use std::sync::Arc;

use reims_vgpu::{SendToGuest, VideoDevice, VideoDeviceConfig};

use crate::ffi;
use crate::handle;
use crate::iosurface::RvgIOSurfaceDevice;
use crate::sized::{self, HostStruct};
use crate::status::{RVG_ERR_INVALID_ARGUMENT, RVG_OK, RvgStatus};
use crate::table::{RvgDestroyFn, Table, WithTable};
use crate::{Plat, null_handle};

/// The PCI vendor ID of the video device: Apple's.
pub const RVG_VIDEO_PCI_VENDOR_ID: u16 = 0x106b;

/// The PCI device ID of the video device.
pub const RVG_VIDEO_PCI_DEVICE_ID: u16 = 0x1a03;

/// The video device's virtqueues: 0 carries the guest's messages to the host, 1 the host's to the
/// guest.
pub const RVG_VIDEO_QUEUE_COUNT: u32 = 2;

/// The size of each buffer the guest posts on queue 1, which one host-to-guest message fills.
pub const RVG_VIDEO_GUEST_BUFFER_SIZE: u32 = 0x4000;

/// What rvg_video_device_deliver answers while the device has no host session (before
/// rvg_video_device_start, after rvg_video_device_reset): VideoToolbox's kVTInvalidSessionErr.
pub const RVG_VIDEO_INVALID_SESSION: i32 = -12903;

const _: () = assert!(RVG_VIDEO_INVALID_SESSION == reims_vgpu::INVALID_SESSION);

/// The longest message rvg_video_device_deliver takes, 16 MiB: ours, far past what the guest's
/// VideoToolbox sends, which cuts a value across messages at 0x200000 bytes. A host checks an
/// element's readable part against it before it copies it out of the guest's memory, and
/// answers a longer one RVG_VIDEO_ALLOCATION_FAILED itself, as rvg_video_device_deliver would,
/// since a guest's descriptors may add up to many times the host's memory.
pub const RVG_VIDEO_MAX_MESSAGE_SIZE: u32 = 0x100_0000;

const _: () = assert!(RVG_VIDEO_MAX_MESSAGE_SIZE as usize == reims_vgpu::MAX_MESSAGE_SIZE);

/// What rvg_video_device_deliver answers a message longer than RVG_VIDEO_MAX_MESSAGE_SIZE,
/// unread: VideoToolbox's kVTAllocationFailedErr, as the host session answers a delivery past
/// what it holds of the guest's messages.
pub const RVG_VIDEO_ALLOCATION_FAILED: i32 = -12904;

const _: () = assert!(RVG_VIDEO_ALLOCATION_FAILED == reims_vgpu::ALLOCATION_FAILED);

/// `send_to_guest(ctx, message, length)`: one host-to-guest message, the whole container, which
/// the host copies and writes into the next buffer the guest posts on queue 1, in the order the
/// calls come. `message` is borrowed for the call.
///
/// Thread: a library thread, or the host's own inside rvg_video_device_deliver; calls are not
/// serialized with the host's calls. May call: rvg_status_message. It must not wait for a host
/// thread that is inside a call on the same device. Without it, host-to-guest messages are
/// dropped.
pub type RvgVideoSendToGuestFn =
    Option<unsafe extern "C-unwind" fn(ctx: *mut c_void, message: *const u8, length: usize)>;

/// The video device's callbacks.
#[repr(C)]
#[derive(Clone, Copy)]
#[expect(
    missing_docs,
    reason = "the fields' names say what they hold, as the header shows them"
)]
pub struct RvgVideoCallbacks {
    /// sizeof(RvgVideoCallbacks) as the host was compiled.
    pub struct_size: u32,
    pub ctx: *mut c_void,
    /// Runs once after the video device's end, when its send_to_guest runs no more.
    pub destroy: RvgDestroyFn,
    pub send_to_guest: RvgVideoSendToGuestFn,
}

// SAFETY: repr(C), a u32 first; a pointer and optional function pointers.
unsafe impl HostStruct for RvgVideoCallbacks {}

/// The video device's config.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RvgVideoDeviceConfig {
    /// sizeof(RvgVideoDeviceConfig) as the host was compiled.
    pub struct_size: u32,
    /// The IOSurface device whose mapping IDs the guest's messages name, borrowed for the call:
    /// the video device holds no reference to it, and finds no surface after its teardown. NULL
    /// is the process's latest IOSurface device at each lookup, as a device with
    /// `use_iosurface_mapper` finds it.
    pub iosurface_device: *const RvgIOSurfaceDevice,
    /// NULL is a table with every callback absent.
    pub callbacks: *const RvgVideoCallbacks,
}

// SAFETY: repr(C), a u32 first; pointers.
unsafe impl HostStruct for RvgVideoDeviceConfig {}

/// The video device.
pub struct RvgVideoDevice {
    device: WithTable<VideoDevice<Plat>>,
}

/// The video device behind a handle, or the process's end for NULL.
///
/// # Safety
/// `device` is NULL or a live video device handle.
unsafe fn live<'a>(device: *const RvgVideoDevice, call: &'static str) -> &'a RvgVideoDevice {
    // SAFETY: the caller's contract.
    unsafe { handle::get(device) }.unwrap_or_else(|| null_handle(call))
}

/// How a device is made: opening the host's backend, or, in the tests, opening none.
type Make<'a> = &'a dyn Fn(VideoDeviceConfig<Plat>) -> VideoDevice<Plat>;

/// rvg_video_device_create, with the device made by `make`.
///
/// # Safety
/// As rvg_video_device_create's.
unsafe fn create(
    config: *const RvgVideoDeviceConfig,
    device: *mut *mut RvgVideoDevice,
    make: Make<'_>,
) -> RvgStatus {
    // SAFETY: the caller's contract.
    let c = match unsafe { sized::read(config) } {
        Ok(Some(c)) => c,
        Ok(None) => return RVG_ERR_INVALID_ARGUMENT,
        Err(s) => return s,
    };
    // SAFETY: the config's contract: NULL or a live table.
    let callbacks = match unsafe { sized::read_or_zero(c.callbacks) } {
        Ok(t) => t,
        Err(s) => return s,
    };
    // From here the table is the library's: a refusal ends it before it returns.
    let table = Table::new(callbacks.ctx, callbacks.destroy);
    if device.is_null() {
        table.end();
        return RVG_ERR_INVALID_ARGUMENT;
    }
    // SAFETY: the config's contract: NULL or a live IOSurface device handle.
    let iosurface_device = unsafe { handle::get(c.iosurface_device) }.map(|d| d.device.clone_ref());
    let send_to_guest = callbacks.send_to_guest.map(|f| {
        let t = table.clone();
        let s: SendToGuest = Arc::new(move |message: &[u8]| {
            let (at, length) = (message.as_ptr(), message.len());
            // SAFETY: the host's function over its ctx; the message lives for the call.
            ffi::host_call("send_to_guest", || unsafe { f(t.ctx(), at, length) });
        });
        s
    });
    let config = VideoDeviceConfig {
        iosurface_device,
        send_to_guest,
    };
    let d = make(config);
    let h = handle::new(RvgVideoDevice {
        device: WithTable::new(d, table),
    });
    // SAFETY: checked non-NULL; the caller's contract.
    unsafe { device.write(h) };
    RVG_OK
}

/// A new video device, with no host session until rvg_video_device_start, decoding and encoding
/// on the backend the library picks itself, and logs: VideoToolbox on macOS; on Linux, on the GPU
/// the library's GPU device runs on, NVIDIA's NVDEC and NVENC on an NVIDIA GPU, else VA-API, and a
/// hardware GPU of the host's where that GPU is a software rasterizer or offers no video. Where no
/// backend opens the guest is offered no codec. Refusals: RVG_ERR_INVALID_ARGUMENT,
/// RVG_ERR_UNSUPPORTED. Calls back: never, but a refusal's destroy.
///
/// # Safety
/// `config` is NULL or a live config; `device` is NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rvg_video_device_create(
    config: *const RvgVideoDeviceConfig,
    device: *mut *mut RvgVideoDevice,
) -> RvgStatus {
    ffi::export(|| {
        let make = |config| {
            VideoDevice::with_platform(<Plat as reims_vgpu::ApiPlatform>::process(), config)
        };
        // SAFETY: the caller's contract.
        unsafe { create(config, device, &make) }
    })
}

/// Drops the host's reference; the last one resets the device (rvg_video_device_reset), which
/// may wait for the host session's threads.
///
/// # Safety
/// `device` is NULL or a live video device handle, not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rvg_video_device_release(device: *mut RvgVideoDevice) {
    ffi::export(|| {
        // SAFETY: the caller's contract.
        unsafe { handle::release(device) }
    })
}

/// The guest's DRIVER_OK: a new host session, which replaces the one there was, if any. A
/// session replaced ends as at rvg_video_device_reset, which may block on its threads. Calls
/// back: never.
///
/// # Safety
/// `device` is a live video device handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rvg_video_device_start(device: *mut RvgVideoDevice) {
    ffi::export(|| {
        // SAFETY: the caller's contract.
        unsafe { live(device, "rvg_video_device_start") }
            .device
            .start();
    })
}

/// One guest-to-host message of queue 0, `length` bytes at `message`, borrowed for the call: the
/// IOSurfaces its leading `subs` atom names are retained through the IOSurface device while the
/// host session needs them, and the session answers the status the host writes into the
/// element's writable part. RVG_VIDEO_INVALID_SESSION while there is no session, and
/// RVG_VIDEO_ALLOCATION_FAILED for a message longer than RVG_VIDEO_MAX_MESSAGE_SIZE, which the
/// session never sees. May call back: send_to_guest, on this thread. May block on the IOSurface
/// device's thread, on a JPEG decode (`jdec`) and on a session's end (`-dec`, `-enc`, `-mep`),
/// 10 s at most each.
///
/// # Safety
/// `device` is a live video device handle; `message` is readable for `length` bytes, or NULL
/// with `length` 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rvg_video_device_deliver(
    device: *mut RvgVideoDevice,
    message: *const u8,
    length: usize,
) -> i32 {
    ffi::export(|| {
        // SAFETY: the caller's contract.
        let d = unsafe { live(device, "rvg_video_device_deliver") };
        let bytes = if message.is_null() || length == 0 {
            &[][..]
        } else {
            // SAFETY: the caller's contract: `length` readable bytes at `message`.
            unsafe { std::slice::from_raw_parts(message, length) }
        };
        d.device.deliver(bytes)
    })
}

/// The first phase of a reset or a stop: the host session sends the guest nothing more. Each of
/// its decoder, encoder and motion-estimation sessions takes it on its thread, after the message
/// in hand there, and this waits for them, 10 s at most in all. Calls back: never.
///
/// # Safety
/// `device` is a live video device handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rvg_video_device_invalidate(device: *mut RvgVideoDevice) {
    ffi::export(|| {
        // SAFETY: the caller's contract.
        unsafe { live(device, "rvg_video_device_invalidate") }
            .device
            .invalidate();
    })
}

/// A reset or a stop: the host session is invalidated, if it was not, its work drains and it
/// ends; the next message answers RVG_VIDEO_INVALID_SESSION until rvg_video_device_start. May
/// block on the session's threads: 10 s at most for them to take the invalidation, as
/// rvg_video_device_invalidate waits, then for those that took it to end; one left running
/// past the bound is left detached. Calls back: never.
///
/// # Safety
/// `device` is a live video device handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rvg_video_device_reset(device: *mut RvgVideoDevice) {
    ffi::export(|| {
        // SAFETY: the caller's contract.
        unsafe { live(device, "rvg_video_device_reset") }
            .device
            .reset();
    })
}

#[cfg(test)]
mod tests;
