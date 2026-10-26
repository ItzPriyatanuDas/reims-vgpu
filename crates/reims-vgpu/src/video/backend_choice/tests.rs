use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use reims_vgpu_video_core::backend::NoDecoders;
#[cfg(target_os = "macos")]
use reims_vgpu_video_core::runtime::StdRuntime;

use super::*;

/// A host of stand-in GPUs: the GPU device's, the hardware ones in the loader's order, and the
/// backends that open on each, by kind and GPU name, with decoders, or with no part; any other
/// does not open. It counts the opens it is asked for.
#[derive(Default)]
struct StandIn {
    gpu_device: Option<Gpu>,
    hardware: Vec<Gpu>,
    offers: Vec<(Kind, &'static str)>,
    empty: Vec<(Kind, &'static str)>,
    opens: RefCell<Vec<(Kind, String)>>,
}

impl Host for StandIn {
    fn gpu_device(&self) -> Option<Gpu> {
        self.gpu_device.clone()
    }

    fn hardware(&self) -> Vec<Gpu> {
        self.hardware.clone()
    }

    fn open(&self, kind: Kind, gpu: &Gpu) -> Result<Backend, String> {
        self.opens.borrow_mut().push((kind, gpu.name.clone()));
        let at = (kind, gpu.name.as_str());
        if self.offers.contains(&at) {
            Ok(Backend {
                decoders: Some(Arc::new(NoDecoders)),
                ..Backend::none(format!("{} stand-in", kind.label()))
            })
        } else if self.empty.contains(&at) {
            Ok(Backend::none(format!("{} stand-in", kind.label())))
        } else {
            Err("no driver".to_owned())
        }
    }
}

fn gpu(name: &str, kind: GpuKind, vendor_id: u32, at: Option<(&str, &str)>) -> Gpu {
    Gpu {
        name: name.to_owned(),
        kind,
        vendor_id,
        pci_address: at.map(|(pci, _)| pci.to_owned()),
        render_node: at.map(|(_, node)| PathBuf::from(node)),
    }
}

const XTX: &str = "RX 7900 XTX";
const IGPU: &str = "Radeon 780M";
const RTX: &str = "RTX 5080";

fn xtx() -> Gpu {
    let at = ("0000:03:00.0", "/stand-in/renderD128");
    gpu(XTX, GpuKind::Discrete, 0x1002, Some(at))
}

fn igpu() -> Gpu {
    let at = ("0000:c9:00.0", "/stand-in/renderD129");
    gpu(IGPU, GpuKind::Integrated, 0x1002, Some(at))
}

fn rtx() -> Gpu {
    let at = ("0000:01:00.0", "/stand-in/renderD130");
    gpu(RTX, GpuKind::Discrete, NVIDIA_VENDOR, Some(at))
}

fn lavapipe() -> Gpu {
    gpu("llvmpipe", GpuKind::Cpu, 0x10005, None)
}

/// What `host` picks for a device that needs `need`, with its own open backends: its label, its
/// place, and why.
fn picked_for(host: &StandIn, need: Need) -> (&'static str, Result<String, String>, String) {
    let open = OpenBackends::new();
    let Picked { label, why, opened } = pick("a device", need, host, &open);
    (label, opened.map(|(shared, _)| shared.place.clone()), why)
}

/// What `host` picks for a video device ([`picked_for`]).
fn picked(host: &StandIn) -> (&'static str, Result<String, String>, String) {
    picked_for(host, Need::Video)
}

fn opens(host: &StandIn) -> Vec<(Kind, String)> {
    host.opens.borrow().clone()
}

/// The devices run on the GPU device's GPU: VA-API on its render node, opened once
#[test]
fn the_devices_follow_the_gpu_devices_gpu() {
    let host = StandIn {
        gpu_device: Some(xtx()),
        hardware: vec![igpu(), xtx(), lavapipe()],
        offers: vec![(Kind::Vaapi, XTX), (Kind::Vaapi, IGPU)],
        ..StandIn::default()
    };
    let (label, place, why) = picked(&host);
    assert_eq!(label, "VA-API");
    assert_eq!(place.as_deref(), Ok("VA-API at /stand-in/renderD128"));
    assert_eq!(
        why,
        "the GPU device's GPU, RX 7900 XTX (discrete, 0000:03:00.0, /stand-in/renderD128)"
    );
    assert_eq!(opens(&host), [(Kind::Vaapi, XTX.to_owned())]);
}

/// A GPU device pinned to an integrated GPU has the devices there, though a discrete one offers
/// video too
#[test]
fn a_pinned_integrated_gpu_is_the_devices_too() {
    let host = StandIn {
        gpu_device: Some(igpu()),
        hardware: vec![igpu()],
        offers: vec![(Kind::Vaapi, XTX), (Kind::Vaapi, IGPU)],
        ..StandIn::default()
    };
    let (label, place, _) = picked(&host);
    assert_eq!(label, "VA-API");
    assert_eq!(place.as_deref(), Ok("VA-API at /stand-in/renderD129"));
}

/// An NVIDIA GPU runs NVIDIA's backend, by its PCI address
#[test]
fn an_nvidia_gpu_runs_nvidias_backend() {
    let host = StandIn {
        gpu_device: Some(rtx()),
        offers: vec![(Kind::Nvidia, RTX), (Kind::Vaapi, RTX)],
        ..StandIn::default()
    };
    let (label, place, _) = picked(&host);
    assert_eq!(label, "NVIDIA");
    assert_eq!(place.as_deref(), Ok("NVIDIA at 0000:01:00.0"));
    assert_eq!(opens(&host), [(Kind::Nvidia, RTX.to_owned())]);
}

/// On an NVIDIA GPU whose backend does not open (no CUDA), VA-API on its render node, saying why
#[test]
fn an_nvidia_gpu_without_cuda_runs_vaapi() {
    let host = StandIn {
        gpu_device: Some(rtx()),
        offers: vec![(Kind::Vaapi, RTX)],
        ..StandIn::default()
    };
    let (label, place, why) = picked(&host);
    assert_eq!(label, "VA-API");
    assert_eq!(place.as_deref(), Ok("VA-API at /stand-in/renderD130"));
    assert!(why.ends_with(", where NVIDIA's: no driver"), "{why}");
}

/// A GPU device on lavapipe has the devices pick a hardware GPU: a discrete one first, whatever
/// the loader's order
#[test]
fn a_software_rasterizer_has_the_devices_pick_a_hardware_gpu() {
    let host = StandIn {
        gpu_device: Some(lavapipe()),
        hardware: vec![igpu(), xtx()],
        offers: vec![(Kind::Vaapi, XTX), (Kind::Vaapi, IGPU)],
        ..StandIn::default()
    };
    let (label, place, why) = picked(&host);
    assert_eq!(label, "VA-API");
    assert_eq!(place.as_deref(), Ok("VA-API at /stand-in/renderD128"));
    assert_eq!(
        why,
        "the GPU device's GPU, llvmpipe (CPU), is a software rasterizer: the first hardware GPU \
         that offers video, discrete ones first, RX 7900 XTX (discrete, 0000:03:00.0, \
         /stand-in/renderD128)"
    );
    assert_eq!(opens(&host), [(Kind::Vaapi, XTX.to_owned())]);
}

/// A discrete GPU whose backend opens with no part offers no video: the integrated one is picked
#[test]
fn a_gpu_without_video_is_passed_over() {
    let host = StandIn {
        gpu_device: Some(lavapipe()),
        hardware: vec![xtx(), igpu()],
        offers: vec![(Kind::Vaapi, IGPU)],
        empty: vec![(Kind::Vaapi, XTX)],
        ..StandIn::default()
    };
    let (_, place, _) = picked(&host);
    assert_eq!(place.as_deref(), Ok("VA-API at /stand-in/renderD129"));
}

/// The GPU device's GPU that offers no video is not tried again among the hardware ones
#[test]
fn the_gpu_devices_gpu_without_video_is_tried_once() {
    let host = StandIn {
        gpu_device: Some(xtx()),
        hardware: vec![xtx(), igpu()],
        offers: vec![(Kind::Vaapi, IGPU)],
        ..StandIn::default()
    };
    let (_, place, why) = picked(&host);
    assert_eq!(place.as_deref(), Ok("VA-API at /stand-in/renderD129"));
    assert!(
        why.starts_with(
            "the GPU device's GPU, RX 7900 XTX (discrete, 0000:03:00.0, /stand-in/renderD128), \
             offers no video (VA-API's: no driver): the first hardware GPU"
        ),
        "{why}"
    );
    assert_eq!(
        opens(&host),
        [
            (Kind::Vaapi, XTX.to_owned()),
            (Kind::Vaapi, IGPU.to_owned())
        ]
    );
}

/// Without a hardware GPU that offers video, no backend, saying why; without a GPU device's
/// GPU, a hardware one
#[test]
fn a_host_without_video_offers_none() {
    let host = StandIn {
        gpu_device: Some(lavapipe()),
        ..StandIn::default()
    };
    let (label, place, why) = picked(&host);
    assert_eq!(label, "none");
    assert_eq!(place, Err("the host has no other hardware GPU".to_owned()));
    assert!(why.ends_with("is a software rasterizer"), "{why}");

    let host = StandIn {
        hardware: vec![xtx()],
        ..StandIn::default()
    };
    let (_, place, _) = picked(&host);
    assert_eq!(
        place,
        Err(
            "no other hardware GPU that offers video: RX 7900 XTX (discrete, 0000:03:00.0, \
             /stand-in/renderD128): VA-API's: no driver"
                .to_owned()
        )
    );

    let host = StandIn {
        hardware: vec![xtx()],
        offers: vec![(Kind::Vaapi, XTX)],
        ..StandIn::default()
    };
    let (_, place, why) = picked(&host);
    assert_eq!(place.as_deref(), Ok("VA-API at /stand-in/renderD128"));
    assert!(
        why.starts_with("the Vulkan runtime makes no device"),
        "{why}"
    );
}

/// On macOS the video device's backend is VideoToolbox, and on this Mac it decodes.
#[cfg(target_os = "macos")]
#[test]
fn a_macs_devices_run_videotoolbox() {
    let chosen = choose(
        &OpenBackends::new(),
        "video device",
        Need::Video,
        &StdRuntime::shared(),
    );
    assert_eq!(chosen.label, "VideoToolbox");
    assert!(chosen.has_backend());
    let summary = chosen.backend().summary();
    assert!(summary.starts_with("VideoToolbox: decodes"), "{summary}");
}

/// A stand-in for a backend's open: counts its calls, and fails when told to.
fn opener(opens: &Cell<u32>, fail: bool) -> impl FnOnce() -> Result<Backend, String> {
    move || {
        opens.set(opens.get() + 1);
        if fail {
            Err("no such driver".to_owned())
        } else {
            Ok(Backend::none("stand-in"))
        }
    }
}

fn place(node: &str) -> Place {
    Place {
        kind: Kind::Vaapi,
        at: Some(node.to_owned()),
    }
}

/// Two devices on one node open the backend once and share it; another node opens its own.
#[test]
fn devices_on_one_node_share_one_backend() {
    let open = OpenBackends::new();
    let opens = Cell::new(0);
    let (video, was_open) = open
        .get_or_open(place("/n/128"), "video device", opener(&opens, false))
        .expect("opens");
    assert!(!was_open);
    let (another, was_open) = open
        .get_or_open(
            place("/n/128"),
            "another video device",
            opener(&opens, false),
        )
        .expect("opens");
    assert!(was_open);
    assert!(Arc::ptr_eq(&video, &another));
    assert_eq!(another.opened_for, "video device");
    let (other, was_open) = open
        .get_or_open(
            place("/n/129"),
            "another video device",
            opener(&opens, false),
        )
        .expect("opens");
    assert!(!was_open);
    assert!(!Arc::ptr_eq(&video, &other));
    assert_eq!(opens.get(), 2);
}

/// The backend lives while either device holds it, in either order they go, and a device
/// made after both went opens it anew.
#[test]
fn a_shared_backend_lives_until_its_last_device_goes() {
    for video_first in [true, false] {
        let open = OpenBackends::new();
        let opens = Cell::new(0);
        let get = |device| {
            open.get_or_open(place("/n/128"), device, opener(&opens, false))
                .expect("opens")
                .0
        };
        let (video, another) = (get("video device"), get("another video device"));
        let weak = Arc::downgrade(&video);
        let (first, second) = if video_first {
            (video, another)
        } else {
            (another, video)
        };
        drop(first);
        assert!(weak.upgrade().is_some(), "the other device still holds it");
        let third = get("video device");
        assert_eq!(opens.get(), 1, "a device made meanwhile shares it");
        drop((second, third));
        assert!(weak.upgrade().is_none(), "released with its last device");
        let _again = get("video device");
        assert_eq!(opens.get(), 2, "opened anew after");
    }
}

/// A backend that did not open is not kept: the next device asks again.
#[test]
fn a_failed_open_is_asked_again() {
    let open = OpenBackends::new();
    let opens = Cell::new(0);
    assert!(
        open.get_or_open(place("/n/128"), "video device", opener(&opens, true))
            .is_err()
    );
    assert!(
        open.get_or_open(
            place("/n/128"),
            "another video device",
            opener(&opens, false)
        )
        .is_ok()
    );
    assert_eq!(opens.get(), 2);
}

/// Two video devices themselves share the backend, and it is released with the last of them, in
/// either order, as their destroys (QEMU's unrealize) drop them. This Mac's VideoToolbox is the
/// backend, on std's runtime, not the testkit platform's, whose virtual clock no backend's wait
/// for its GPU would pass. The devices pick from backends open of the test's own, not the
/// process's, which the other tests' devices in the process share meanwhile, so the count is of
/// what this test made.
#[cfg(target_os = "macos")]
#[test]
fn the_devices_share_their_backend_until_the_last_goes() {
    use reims_vgpu_core::testkit::FakePlatform;

    use crate::video::{VideoDevice, VideoDeviceConfig};

    let open = OpenBackends::new();
    for first_first in [true, false] {
        let platform = Arc::new(FakePlatform::new());
        let device = |platform| {
            VideoDevice::picking_from(
                &open,
                platform,
                StdRuntime::shared(),
                VideoDeviceConfig::default(),
            )
        };
        let first = device(Arc::clone(&platform));
        let second = device(platform);
        assert_eq!(
            videotoolbox_holders(&open),
            2,
            "both devices hold one backend"
        );
        let (gone, left) = if first_first {
            (first, second)
        } else {
            (second, first)
        };
        drop(gone);
        assert_eq!(
            videotoolbox_holders(&open),
            1,
            "the other device still holds it"
        );
        drop(left);
        assert_eq!(
            videotoolbox_holders(&open),
            0,
            "released with the last device"
        );
    }
}

/// A place is its kind and where it runs: a render node for VA-API, a PCI address for NVIDIA,
/// nothing for VideoToolbox.
#[test]
fn a_place_says_its_backend_and_gpu() {
    assert_eq!(place("/n/128").to_string(), "VA-API at /n/128");
    let nvidia = Place::on(Kind::Nvidia, &rtx()).expect("a PCI address");
    assert_eq!(nvidia.to_string(), "NVIDIA at 0000:01:00.0");
    assert_eq!(Place::on(Kind::Vaapi, &lavapipe()), None);
    let mac = Place {
        kind: Kind::VideoToolbox,
        at: None,
    };
    assert_eq!(mac.to_string(), "VideoToolbox");
}
