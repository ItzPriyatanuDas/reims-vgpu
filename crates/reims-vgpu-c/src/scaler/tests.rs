use reims_vgpu::reims_vgpu_video_core::scaler::backend::ReferenceBackend;
use reims_vgpu::reims_vgpu_video_core::scaler::capabilities::{Capabilities, FormatClass};

use super::*;
use crate::testkit::{Rec, t_destroy, test_platform};

/// Where the fake guest's RAM starts, and its size: the bytes behind it are the test's.
const RAM: u64 = 0x8000_0000;
const RAM_SIZE: usize = 0x1000;

/// The test's guest RAM, behind the table's ctx: its Rec, and the bytes in a static, each test
/// with its own.
struct Guest {
    rec: Arc<Rec>,
    bytes: reims_vgpu_core::sync::Mutex<Vec<u8>>,
}

fn guest<'a>(ctx: *mut c_void) -> &'a Guest {
    // SAFETY: the tests' ctx is a live Guest, for as long as the device is.
    unsafe { &*ctx.cast::<Guest>() }
}

fn range(address: u64, length: usize) -> Option<std::ops::Range<usize>> {
    let start = usize::try_from(address.checked_sub(RAM)?).ok()?;
    let end = start.checked_add(length)?;
    (end <= RAM_SIZE).then_some(start..end)
}

unsafe extern "C-unwind" fn t_is_ram(ctx: *mut c_void, address: u64, length: u64) -> bool {
    let g = guest(ctx);
    Rec::push(g.rec.ctx(), format!("is_ram {address:#x} {length:#x}"));
    range(
        address,
        usize::try_from(length.max(1)).unwrap_or(usize::MAX),
    )
    .is_some()
}

unsafe extern "C-unwind" fn t_read(
    ctx: *mut c_void,
    address: u64,
    bytes: *mut u8,
    length: usize,
) -> bool {
    let Some(r) = range(address, length) else {
        return false;
    };
    // SAFETY: the library's contract: `length` writable bytes at `bytes`.
    let out = unsafe { std::slice::from_raw_parts_mut(bytes, length) };
    out.copy_from_slice(&guest(ctx).bytes.lock()[r]);
    true
}

unsafe extern "C-unwind" fn t_write(
    ctx: *mut c_void,
    address: u64,
    bytes: *const u8,
    length: usize,
) -> bool {
    let Some(r) = range(address, length) else {
        return false;
    };
    // SAFETY: the library's contract: `length` readable bytes at `bytes`.
    let from = unsafe { std::slice::from_raw_parts(bytes, length) };
    guest(ctx).bytes.lock()[r].copy_from_slice(from);
    true
}

unsafe extern "C-unwind" fn t_guest_destroy(ctx: *mut c_void) {
    // SAFETY: as `guest`.
    unsafe { t_destroy(guest(ctx).rec.ctx()) };
}

fn capabilities() -> Capabilities {
    Capabilities {
        scaler_version: 1,
        formats: vec![FormatClass::TwoPlanes420],
        min_source: 4,
        max_source: (64, 64),
        min_destination: 4,
        max_destination: (64, 64),
        max_scale: [8, 8, 8, 4],
        xy_offsets_with_tiling: (false, false),
    }
}

/// The C API's create, on core's CPU stand-in rather than the host's GPU.
///
/// # Safety
/// As rvg_scaler_device_create's.
unsafe fn create_on_reference(
    config: *const RvgScalerDeviceConfig,
    device: *mut *mut RvgScalerDevice,
) -> RvgStatus {
    let make = |config| {
        ScalerDevice::with_backend(
            <Plat as reims_vgpu::ApiPlatform>::process(),
            config,
            Arc::new(ReferenceBackend {
                capabilities: capabilities(),
            }),
        )
    };
    // SAFETY: the caller's contract.
    unsafe { create(config, device, &make) }
}

/// The guest kext's type 1 request: 0x288 bytes of capabilities at `address`.
fn capabilities_request(address: u64) -> Vec<u8> {
    let mut r = vec![0u8; 0x20];
    r[0..4].copy_from_slice(&1u32.to_le_bytes());
    r[8..16].copy_from_slice(&address.to_le_bytes());
    r[0x18..0x20].copy_from_slice(&0x288u64.to_le_bytes());
    r
}

/// No config, or no out-pointer, is refused, the latter with the table's destroy; a device
/// refuses requests before its start and after a reset, with Apple's reason; once started it
/// writes the capabilities through the host's guest memory and answers in place; a short
/// writable part is a needs-reset; and the device ends its table at its release
#[test]
fn a_device_answers_through_the_hosts_guest_memory() {
    let _p = test_platform();
    let g = Box::new(Guest {
        rec: Rec::new(),
        bytes: reims_vgpu_core::sync::Mutex::new(vec![0; RAM_SIZE]),
    });
    let memory = RvgScalerGuestMemory {
        struct_size: sized::size_u32::<RvgScalerGuestMemory>(),
        ctx: std::ptr::from_ref(&*g).cast_mut().cast(),
        destroy: Some(t_guest_destroy),
        is_ram: Some(t_is_ram),
        read: Some(t_read),
        write: Some(t_write),
    };
    let c = RvgScalerDeviceConfig {
        struct_size: sized::size_u32::<RvgScalerDeviceConfig>(),
        iosurface_device: std::ptr::null(),
        guest_memory: &memory,
    };
    let mut d = std::ptr::null_mut();
    let mut reason = [0 as c_char; 64];
    let reason_text = |r: &[c_char]| {
        // SAFETY: the library wrote a NUL-terminated string into `r`.
        unsafe { std::ffi::CStr::from_ptr(r.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };
    // SAFETY: live configs; `d` is writable; the device made is used until its release.
    unsafe {
        assert_eq!(
            create_on_reference(std::ptr::null(), &mut d),
            RVG_ERR_INVALID_ARGUMENT
        );
        assert_eq!(g.rec.destroys().len(), 0);
        assert_eq!(
            create_on_reference(&c, std::ptr::null_mut()),
            RVG_ERR_INVALID_ARGUMENT
        );
        assert_eq!(g.rec.destroys().len(), 1);
        assert_eq!(create_on_reference(&c, &mut d), RVG_OK);

        let mut r = capabilities_request(RAM + 0x100);
        let (at, n) = (r.as_mut_ptr(), r.len());
        assert_eq!(
            rvg_scaler_device_process(d, at, n, n, reason.as_mut_ptr(), reason.len()),
            RVG_SCALER_NEEDS_RESET
        );
        assert_eq!(
            reason_text(&reason),
            "Invalid capabilities request: no accelerator"
        );
        assert!(rvg_scaler_device_start(d));
        assert_eq!(
            rvg_scaler_device_process(d, at, n, n, std::ptr::null_mut(), 0),
            RVG_SCALER_REPLY
        );
        assert_eq!(r[4..8], [0; 4], "the status, success");
        assert_eq!(r[0x18..0x20], 0x288u64.to_le_bytes());
        assert_eq!(
            g.bytes.lock()[0x100..0x100 + 0x288],
            capabilities().to_bytes()
        );
        assert!(
            g.rec
                .lines()
                .contains(&format!("is_ram {:#x} 0x288", RAM + 0x100)),
            "{:?}",
            g.rec.lines()
        );
        assert_eq!(
            rvg_scaler_device_process(d, at, n, n - 1, reason.as_mut_ptr(), 8),
            RVG_SCALER_NEEDS_RESET
        );
        assert_eq!(reason_text(&reason), "Write b", "cut to the buffer");
        rvg_scaler_device_reset(d);
        assert_eq!(
            rvg_scaler_device_process(d, at, n, n, std::ptr::null_mut(), 0),
            RVG_SCALER_NEEDS_RESET
        );
        rvg_scaler_device_release(d);
        rvg_scaler_device_release(std::ptr::null_mut());
    }
    assert_eq!(g.rec.destroys().len(), 2);
}

/// Where the host has no GPU for it there is a scaler device all the same, one that does not
/// scale: its start fails, a needs-reset, and every request needs-resets; its release ends the
/// table
#[test]
fn a_host_without_a_gpu_has_a_device_that_does_not_scale() {
    let _p = test_platform();
    let rec = Rec::new();
    let memory = RvgScalerGuestMemory {
        struct_size: sized::size_u32::<RvgScalerGuestMemory>(),
        ctx: rec.ctx(),
        destroy: Some(t_destroy),
        is_ram: None,
        read: None,
        write: None,
    };
    let c = RvgScalerDeviceConfig {
        struct_size: sized::size_u32::<RvgScalerDeviceConfig>(),
        iosurface_device: std::ptr::null(),
        guest_memory: &memory,
    };
    let none = |config| {
        ScalerDevice::without_gpu(
            <Plat as reims_vgpu::ApiPlatform>::process(),
            config,
            "a stand-in host with none".to_owned(),
        )
    };
    let mut d = std::ptr::null_mut();
    let mut request = [0u8; 0x20];
    request[0] = 1;
    // SAFETY: a live config; `d` is writable, and the device is released.
    unsafe {
        assert_eq!(create(&c, &mut d, &none), RVG_OK);
        assert!(!rvg_scaler_device_scales(d));
        assert!(!rvg_scaler_device_start(d));
        assert_eq!(
            rvg_scaler_device_process(
                d,
                request.as_mut_ptr(),
                request.len(),
                request.len(),
                std::ptr::null_mut(),
                0
            ),
            RVG_SCALER_NEEDS_RESET
        );
        assert_eq!(rec.destroys().len(), 0);
        rvg_scaler_device_release(d);
    }
    assert_eq!(rec.destroys().len(), 1);
}
