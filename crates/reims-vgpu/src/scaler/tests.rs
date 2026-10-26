use std::sync::Arc;

use reims_vgpu_core::sync::Mutex;
use reims_vgpu_core::testkit::FakePlatform;
use reims_vgpu_video_core::scaler::backend::ReferenceBackend;
use reims_vgpu_video_core::scaler::capabilities::{Capabilities, FormatClass};

use super::*;

/// Where the fake guest's RAM starts, and its size.
const RAM: u64 = 0x4000_0000;
const RAM_SIZE: usize = 0x2000;

/// A guest's RAM: one run of bytes at [`RAM`].
#[derive(Default)]
struct Ram(Mutex<Vec<u8>>);

impl Ram {
    fn new() -> Ram {
        Ram(Mutex::new(vec![0; RAM_SIZE]))
    }

    fn range(address: u64, length: usize) -> Option<std::ops::Range<usize>> {
        let start = usize::try_from(address.checked_sub(RAM)?).ok()?;
        let end = start.checked_add(length)?;
        (end <= RAM_SIZE).then_some(start..end)
    }
}

impl GuestMemory for Ram {
    fn is_ram(&self, address: u64, length: u64) -> bool {
        Ram::range(
            address,
            usize::try_from(length.max(1)).unwrap_or(usize::MAX),
        )
        .is_some()
    }

    fn read(&self, address: u64, bytes: &mut [u8]) -> bool {
        let Some(r) = Ram::range(address, bytes.len()) else {
            return false;
        };
        bytes.copy_from_slice(&self.0.lock()[r]);
        true
    }

    fn write(&self, address: u64, bytes: &[u8]) -> bool {
        let Some(r) = Ram::range(address, bytes.len()) else {
            return false;
        };
        self.0.lock()[r].copy_from_slice(bytes);
        true
    }
}

fn capabilities() -> Capabilities {
    Capabilities {
        scaler_version: 1,
        formats: vec![FormatClass::TwoPlanes420, FormatClass::Argb8888],
        min_source: 4,
        max_source: (4096, 4096),
        min_destination: 4,
        max_destination: (4096, 4096),
        max_scale: [8, 8, 8, 4],
        xy_offsets_with_tiling: (false, false),
    }
}

fn device() -> ScalerDevice<FakePlatform> {
    ScalerDevice::with_backend(
        Arc::new(FakePlatform::new()),
        ScalerDeviceConfig::default(),
        Arc::new(ReferenceBackend {
            capabilities: capabilities(),
        }),
    )
}

/// The guest kext's type 1 request: its capabilities at `address`, 0x288 bytes.
fn capabilities_request(address: u64) -> Vec<u8> {
    let mut r = vec![0u8; 0x20];
    r[0..4].copy_from_slice(&1u32.to_le_bytes());
    r[8..16].copy_from_slice(&address.to_le_bytes());
    r[0x18..0x20].copy_from_slice(&0x288u64.to_le_bytes());
    r
}

/// A type 2 request from surface mapping ID `source` to `destination`, with no options.
fn transform_request(source: u64, destination: u64) -> Vec<u8> {
    let mut r = vec![0u8; 0x610];
    r[0..4].copy_from_slice(&2u32.to_le_bytes());
    r[0x1b8..0x1c0].copy_from_slice(&source.to_le_bytes());
    r[0x1c0..0x1c8].copy_from_slice(&destination.to_le_bytes());
    r
}

fn status(outcome: &ScalerOutcome) -> Option<u32> {
    match outcome {
        ScalerOutcome::Reply(r) => Some(u32::from_le_bytes([r[4], r[5], r[6], r[7]])),
        _ => None,
    }
}

/// Before the start a request is a needs-reset; once started, type 1 writes the backend's
/// capabilities into the guest's RAM and puts the count it wrote at R+0x18; after a reset,
/// needs-resets again
#[test]
fn a_device_answers_from_start_to_reset() {
    let d = device();
    let ram = Ram::new();
    let caps = capabilities_request(RAM + 0x100);
    assert!(matches!(
        d.process(&caps, caps.len(), &ram),
        ScalerOutcome::NeedsReset(_)
    ));
    assert_eq!(d.start(), Ok(()));
    let outcome = d.process(&caps, caps.len(), &ram);
    assert_eq!(status(&outcome), Some(0));
    let ScalerOutcome::Reply(reply) = outcome else {
        unreachable!("a reply, by the status above");
    };
    assert_eq!(reply[0x18..0x20], 0x288u64.to_le_bytes());
    let written = ram.0.lock()[0x100..0x100 + 0x288].to_vec();
    assert_eq!(written, capabilities().to_bytes());
    d.reset();
    assert!(matches!(
        d.process(&caps, caps.len(), &ram),
        ScalerOutcome::NeedsReset(_)
    ));
}

/// Capabilities outside the guest's RAM are a needs-reset, as Apple's host answers an address
/// it cannot translate
#[test]
fn capabilities_outside_ram_are_a_needs_reset() {
    let d = device();
    let ram = Ram::new();
    d.start().unwrap();
    let caps = capabilities_request(RAM + RAM_SIZE as u64 - 0x100);
    assert!(matches!(
        d.process(&caps, caps.len(), &ram),
        ScalerOutcome::NeedsReset(_)
    ));
}

/// With no IOSurface device in the process, a transform's surfaces are not found: the request is
/// answered, with kIOReturnNotFound
#[test]
fn surfaces_without_a_mapper_are_not_found() {
    let d = device();
    let ram = Ram::new();
    d.start().unwrap();
    let t = transform_request(3, 4);
    assert_eq!(
        status(&d.process(&t, t.len(), &ram)),
        Some(reims_vgpu_video_core::scaler::io_return::NOT_FOUND)
    );
}

/// A Mac's device transforms on its GPU, Metal's default device, whatever the platform. Its waits
/// for the GPU are on std's runtime, not the testkit platform's, whose virtual clock no such wait
/// would pass.
#[cfg(target_os = "macos")]
#[test]
fn a_mac_transforms_on_its_gpu() {
    let made = ScalerDevice::on_host_gpu(
        Arc::new(FakePlatform::new()),
        &reims_vgpu_video_core::runtime::StdRuntime::shared(),
        ScalerDeviceConfig::default(),
    );
    assert!(made.scales(), "{made:?}");
}

/// A host without a GPU has a device all the same, one that does not scale: its
/// start is a needs-reset, as Apple's where it cannot make an accelerator, and so is a request
/// before it and after
#[test]
fn a_host_without_a_gpu_has_a_device_that_does_not_scale() {
    let d = ScalerDevice::without_gpu(
        Arc::new(FakePlatform::new()),
        ScalerDeviceConfig::default(),
        "a stand-in host with none".to_owned(),
    );
    assert!(!d.scales());
    let ram = Ram::new();
    let caps = capabilities_request(RAM);
    assert!(matches!(
        d.process(&caps, caps.len(), &ram),
        ScalerOutcome::NeedsReset(_)
    ));
    assert_eq!(d.start(), Err("IOSurfaceAcceleratorCreate failed"));
    assert!(matches!(
        d.process(&caps, caps.len(), &ram),
        ScalerOutcome::NeedsReset(_)
    ));
    assert!(device().scales());
}
