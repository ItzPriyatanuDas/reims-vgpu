use std::sync::Arc;

use reims_vgpu_core::display::DisplayCoord;
use reims_vgpu_core::sync::Mutex;
use reims_vgpu_core::testkit::host::FakeHost;
use reims_vgpu_core::testkit::{FakePlatform, GuestRam};
use reims_vgpu_window::{WindowCallbacks, WindowConfig};

use super::*;
use crate::display::DisplayConfig;
use crate::platform::FAKE_HOST_SCREEN;
use crate::{Device, DeviceConfig, DisplayMode, Error, HostCallbacks};

/// Lines the test's closures write, in the order they ran.
type Log = Arc<Mutex<Vec<String>>>;

/// Writes its line into the log when dropped: where a closure holding it was let go of.
struct Note(&'static str, Log);

impl Drop for Note {
    fn drop(&mut self) {
        self.1.lock().push(self.0.to_owned());
    }
}

/// A device on a platform of its own, over the fake host's callbacks, with `ports` ports.
fn fake_device(ports: u32) -> (Arc<FakeHost>, Device<FakePlatform>) {
    let p = Arc::new(FakePlatform::new());
    let host = FakeHost::new(&p, GuestRam::with_range(0, 0x40_0000).unwrap());
    let b = host.blocks();
    let config = DeviceConfig {
        display_port_count: ports,
        callbacks: HostCallbacks {
            create_task: b.create_task,
            destroy_task: b.destroy_task,
            map_memory: b.map_memory,
            unmap_memory: b.unmap_memory,
            read_memory: b.read_memory,
            raise_interrupt: b.raise_interrupt,
            add_trace_range: b.add_trace_range,
            remove_trace_range: b.remove_trace_range,
        },
        ..DeviceConfig::default()
    };
    (host, Device::with_platform(p, config).unwrap())
}

/// A window config whose close callback holds `held`: the window lets go of it at its end.
fn window_config<T: Send + Sync + 'static>(held: T, match_host_screen: bool) -> WindowConfig {
    WindowConfig {
        match_host_screen,
        mode: None,
        mode_ppi: None,
        callbacks: WindowCallbacks {
            close: Some(Arc::new(move || {
                let _ = &held;
            })),
            ..WindowCallbacks::default()
        },
    }
}

/// The window's side of the tee, recorded.
struct Recorder(Log);

impl GuestEvents for Recorder {
    fn guest_mode(&self, width: u16, height: u16) {
        self.0.lock().push(format!("window mode {width}x{height}"));
    }

    fn new_frame(&self) {
        self.0.lock().push("window new_frame".to_owned());
    }

    fn cursor_glyph(&self, g: &CursorGlyph<'_>) {
        let (w, h, (x, y), px) = (g.width, g.height, g.hot_spot, g.rgba);
        self.0
            .lock()
            .push(format!("window glyph {w}x{h} at {x},{y} {px:?}"));
    }

    fn cursor_show(&self, show: bool) {
        self.0.lock().push(format!("window show {show}"));
    }
}

/// The host's five callbacks, recorded.
fn host_callbacks(log: &Log) -> DisplayCallbacks {
    let (a, b, c, d, e) = (
        log.clone(),
        log.clone(),
        log.clone(),
        log.clone(),
        log.clone(),
    );
    DisplayCallbacks {
        mode_change: Some(Arc::new(move |w, h, format| {
            a.lock().push(format!("host mode {w}x{h} {format:#x}"));
        })),
        new_frame: Some(Arc::new(move || b.lock().push("host new_frame".to_owned()))),
        cursor_glyph: Some(Arc::new(move |g: &CursorGlyph<'_>| {
            c.lock()
                .push(format!("host glyph {}x{}", g.width, g.height));
        })),
        cursor_show: Some(Arc::new(move |show| {
            d.lock().push(format!("host show {show}"))
        })),
        cursor_move: Some(Arc::new(move || e.lock().push("host move".to_owned()))),
    }
}

/// Calls each of the five callbacks once, the cursor's show twice (hide, then show).
fn call_all(cb: &DisplayCallbacks) {
    (cb.mode_change.as_ref().unwrap())(640, 480, 0x4247_5241);
    (cb.new_frame.as_ref().unwrap())();
    (cb.cursor_glyph.as_ref().unwrap())(&CursorGlyph {
        width: 1,
        height: 1,
        hot_spot: (3, 4),
        rgba: &[1, 2, 3, 4],
    });
    (cb.cursor_show.as_ref().unwrap())(0);
    (cb.cursor_show.as_ref().unwrap())(2);
    (cb.cursor_move.as_ref().unwrap())();
}

/// Each guest callback reaches the window, then the host's, with the arguments core passed; the
/// cursor's show byte is the window's bool; a cursor move is the host's alone
#[test]
fn the_tee_tells_the_window_before_the_host() {
    let log = Log::default();
    let window = Arc::new(Recorder(log.clone()));
    let cb = tee(&window, host_callbacks(&log));
    call_all(&cb);
    assert_eq!(
        *log.lock(),
        [
            "window mode 640x480",
            "host mode 640x480 0x42475241",
            "window new_frame",
            "host new_frame",
            "window glyph 1x1 at 3,4 [1, 2, 3, 4]",
            "host glyph 1x1",
            "window show false",
            "host show 0",
            "window show true",
            "host show 2",
            "host move",
        ]
    );
}

/// With no callback of the host's, the tee still has all five, each the window's alone (a cursor
/// move does nothing); its closures hold the window, which they let go of when they drop
#[test]
fn the_tee_has_all_five_callbacks_without_the_host_s() {
    let log = Log::default();
    let window = Arc::new(Recorder(log.clone()));
    let cb = tee(&window, DisplayCallbacks::default());
    call_all(&cb);
    assert_eq!(
        *log.lock(),
        [
            "window mode 640x480",
            "window new_frame",
            "window glyph 1x1 at 3,4 [1, 2, 3, 4]",
            "window show false",
            "window show true",
        ]
    );
    assert_eq!(Arc::strong_count(&window), 5);
    drop(cb);
    assert_eq!(Arc::strong_count(&window), 1);
}

/// A display with a window has all five callbacks, which the guest reads, and each calls the
/// host's where it gave one; the window takes the port, and a title without the display's name.
/// Not matching the host screen, the display keeps the config's size, no scale factor and no
/// modes
#[test]
fn a_window_display_has_every_callback() {
    let (_host, device) = fake_device(2);
    let log = Log::default();
    let mut c = DisplayConfig::new(1, 0x5eed_a001);
    c.name = Some("Studio".to_owned());
    c.size_in_millimeters = (400.0, 300.0);
    let host = host_callbacks(&log);
    c.callbacks.mode_change = host.mode_change;
    c.window = Some(window_config((), false));
    let d = device.new_display(c).unwrap();
    let core = d.core();
    assert!(core.mode_change_handler().is_some());
    assert!(core.new_frame_event_handler().is_some());
    assert!(core.cursor_glyph_handler().is_some());
    assert!(core.cursor_show_handler().is_some());
    assert!(core.cursor_move_handler().is_some());
    (core.mode_change_handler().unwrap())(DisplayCoord { x: 640, y: 480 }, 7);
    (core.cursor_show_handler().unwrap())(1);
    (core.cursor_move_handler().unwrap())();
    assert_eq!(*log.lock(), ["host mode 640x480 0x7"]);
    let window = d.window().unwrap();
    assert_eq!((window.port(), window.title()), (1, "Reims vGPU (port 1)"));
    assert!(!window.match_host_screen() && !window.is_closed());
    assert_eq!(core.size_in_millimeters(), (400.0, 300.0));
    assert_eq!(core.scale_factor(), 0);
    assert!(core.mode_list().is_empty());
}

/// Without a window, the display has the callbacks the host gave and no others, the config's
/// size and no scale factor
#[test]
fn without_a_window_the_display_is_as_its_config_says() {
    let (_host, device) = fake_device(1);
    let log = Log::default();
    let mut c = DisplayConfig::new(0, 0x5eed_a011);
    c.size_in_millimeters = (400.0, 300.0);
    c.callbacks.mode_change = host_callbacks(&log).mode_change;
    let d = device.new_display(c).unwrap();
    let core = d.core();
    assert!(core.mode_change_handler().is_some());
    assert!(core.new_frame_event_handler().is_none());
    assert!(core.cursor_glyph_handler().is_none());
    assert!(core.cursor_show_handler().is_none());
    assert!(core.cursor_move_handler().is_none());
    assert!(d.window().is_none());
    assert_eq!(core.size_in_millimeters(), (400.0, 300.0));
    assert_eq!(core.scale_factor(), 0);
    assert!(core.mode_list().is_empty());
}

/// Matching the host screen, the display offers the modes the policy gives for the platform's
/// screen (the testkit's is Retina: scale factor 2), with the first mode's size in millimetres,
/// in place of the config's; the host's own modes then replace them
#[test]
fn matching_the_host_screen_offers_its_modes_and_density() {
    let (_host, device) = fake_device(1);
    let mut c = DisplayConfig::new(0, 0x5eed_a021);
    c.size_in_millimeters = (400.0, 300.0);
    c.window = Some(window_config((), true));
    let d = device.new_display(c).unwrap();
    let core = d.core();
    assert!(d.window().unwrap().match_host_screen());
    let (modes, density) = host_matched(Some(&FAKE_HOST_SCREEN), None, None, None).unwrap();
    assert_eq!(density.scale_factor, 2);
    assert_eq!(core.scale_factor(), 2);
    assert_eq!(core.size_in_millimeters(), density.size_in_millimeters);
    let got: Vec<(u16, u16, f64, bool, bool)> = core
        .mode_list()
        .iter()
        .map(|m| {
            let size = m.size_in_pixels;
            let rate = m.refresh_rate_in_hz;
            (size.x, size.y, rate, m.supports_edr, m.supports_10bpc)
        })
        .collect();
    let want: Vec<(u16, u16, f64, bool, bool)> = modes
        .iter()
        .map(|m| (m.width, m.height, m.refresh_hz, false, false))
        .collect();
    assert_eq!(got, want);
    d.set_modes(&[DisplayMode::new(1280, 720, 60.0)]);
    assert_eq!(core.mode_list().len(), 1);
}

/// Matching the host screen with a chosen mode, the display offers that mode first, then the
/// standard sizes within it, at the screen's rate and a 1× density, and its window opens sized
/// for it
#[test]
fn a_chosen_mode_is_offered_first_at_1x() {
    let (_host, device) = fake_device(1);
    let mut c = DisplayConfig::new(0, 0x5eed_a041);
    c.size_in_millimeters = (400.0, 300.0);
    let mut w = window_config((), true);
    w.mode = Some((1920, 1080));
    c.window = Some(w);
    let d = device.new_display(c).unwrap();
    let core = d.core();
    let (modes, density) =
        host_matched(Some(&FAKE_HOST_SCREEN), Some((1920, 1080)), None, None).unwrap();
    assert_eq!((modes[0].width, modes[0].height), (1920, 1080));
    assert_eq!(density.scale_factor, 1);
    assert_eq!(core.scale_factor(), 1);
    assert_eq!(core.size_in_millimeters(), density.size_in_millimeters);
    let got: Vec<(u16, u16, f64)> = core
        .mode_list()
        .iter()
        .map(|m| (m.size_in_pixels.x, m.size_in_pixels.y, m.refresh_rate_in_hz))
        .collect();
    let want: Vec<(u16, u16, f64)> = modes
        .iter()
        .map(|m| (m.width, m.height, m.refresh_hz))
        .collect();
    assert_eq!(got, want);
    assert_eq!(d.window().unwrap().offered(), Some((1920, 1080)));
}

/// Matching the host screen with a chosen mode and density, the display tells the guest the
/// mode's size at that density: 2560×1600 at 200 ppi has a scale factor of 2, where the same
/// mode without a density is 1×
#[test]
fn a_chosen_mode_at_a_chosen_ppi_is_told_that_density() {
    let (_host, device) = fake_device(1);
    let mut c = DisplayConfig::new(0, 0x5eed_a042);
    c.size_in_millimeters = (400.0, 300.0);
    let mut w = window_config((), true);
    w.mode = Some((2560, 1600));
    w.mode_ppi = Some(200.0);
    c.window = Some(w);
    let d = device.new_display(c).unwrap();
    let core = d.core();
    let (modes, density) = host_matched(
        Some(&FAKE_HOST_SCREEN),
        Some((2560, 1600)),
        Some(200.0),
        None,
    )
    .unwrap();
    assert_eq!((modes[0].width, modes[0].height), (2560, 1600));
    assert_eq!(density.scale_factor, 2);
    assert_eq!(core.scale_factor(), 2);
    assert_eq!(core.size_in_millimeters(), density.size_in_millimeters);
    assert_eq!(d.window().unwrap().offered(), Some((2560, 1600)));
    let (_, at_1x) = host_matched(Some(&FAKE_HOST_SCREEN), Some((2560, 1600)), None, None).unwrap();
    assert_eq!(at_1x.scale_factor, 1);
}

/// With no host screen (as with no window system to reach), a display matching it still offers
/// modes: the chosen mode and the sizes within it at its density, as on a screen, at 60 Hz
/// unless the rate is overridden; with no mode chosen, 1920×1080 at 1×.
/// Never none, which left a macOS guest no mode to set and stopped its boot at the Apple logo
#[test]
fn without_a_host_screen_the_chosen_mode_is_offered_at_60_hz() {
    let sizes = |m: &[OfferedMode]| m.iter().map(|m| (m.width, m.height)).collect::<Vec<_>>();
    let matched = |screen| host_matched(screen, Some((3840, 2400)), Some(331.0), None).unwrap();
    let (modes, density) = matched(None);
    let (on_screen, screen_density) = matched(Some(&FAKE_HOST_SCREEN));
    assert_eq!(sizes(&modes), sizes(&on_screen));
    assert!(modes.iter().all(|m| m.refresh_hz == 60.0));
    assert_eq!(density, screen_density);
    assert_eq!(density.scale_factor, 2);
    let (modes, _) = host_matched(None, None, None, Some(120.0)).unwrap();
    assert!(modes.iter().all(|m| m.refresh_hz == 120.0));
    let (modes, density) = host_matched(None, None, None, None).unwrap();
    assert_eq!(
        (modes[0].width, modes[0].height, modes[0].refresh_hz),
        (1920, 1080, 60.0)
    );
    assert_eq!(density, policy::density_1x((1920, 1080)));
}

/// A window display's window is told the first mode the display offers, which it opens sized
/// for: matching the host screen, the policy's first; otherwise the first the host sets, each
/// time it sets a list
#[test]
fn the_window_is_told_the_first_mode_offered() {
    let (_host, device) = fake_device(2);
    let mut c = DisplayConfig::new(0, 0x5eed_a031);
    c.window = Some(window_config((), true));
    let matched = device.new_display(c).unwrap();
    let (modes, _) = host_matched(Some(&FAKE_HOST_SCREEN), None, None, None).unwrap();
    let first = (modes[0].width, modes[0].height);
    assert_eq!(matched.window().unwrap().offered(), Some(first));
    let mut c = DisplayConfig::new(1, 0x5eed_a032);
    c.window = Some(window_config((), false));
    let d = device.new_display(c).unwrap();
    let window = d.window().unwrap();
    assert_eq!(window.offered(), None);
    d.set_modes(&[
        DisplayMode::new(1280, 720, 60.0),
        DisplayMode::new(1920, 1080, 60.0),
    ]);
    assert_eq!(window.offered(), Some((1280, 720)));
    d.set_modes(&[]);
    assert_eq!(window.offered(), Some((1280, 720)));
    d.set_modes(&[DisplayMode::new(1024, 768, 60.0)]);
    assert_eq!(window.offered(), Some((1024, 768)));
}

/// A refused window display closes its window before the create returns: the window's
/// callbacks are let go of, even where the refused display's new-frame closure, which the
/// display thread frees, still holds the window
#[test]
fn a_refused_window_display_closes_its_window() {
    let (_host, device) = fake_device(1);
    let first = device
        .new_display(DisplayConfig::new(0, 0x5eed_a101))
        .unwrap();
    for (port, serial, refusal) in [
        (0, 0x5eed_a102, Error::PortInUse),
        (1, 0x5eed_a103, Error::PortOutOfRange),
        (0, 0x5eed_a101, Error::SerialInUse),
    ] {
        let held = Arc::new(());
        let weak = Arc::downgrade(&held);
        let mut c = DisplayConfig::new(port, serial);
        c.window = Some(window_config(held, true));
        assert_eq!(device.new_display(c).err(), Some(refusal));
        assert!(weak.upgrade().is_none(), "{refusal:?}");
    }
    drop(first);
}

/// The window lives while any reference to the display does; the last reference closes it,
/// letting go of the host's window callbacks, before the display's end lets go of the host's
/// display callbacks
#[test]
fn the_last_reference_closes_the_window_before_the_display_ends() {
    let (_host, device) = fake_device(1);
    let log = Log::default();
    let display_note = Note("display ended", log.clone());
    let mut c = DisplayConfig::new(0, 0x5eed_a201);
    c.callbacks.cursor_move = Some(Arc::new(move || {
        let _ = &display_note;
    }));
    c.window = Some(window_config(Note("window closed", log.clone()), false));
    let d = device.new_display(c).unwrap();
    let window = d.window().unwrap().clone();
    let d2 = d.clone_ref();
    drop(d);
    assert!(log.lock().is_empty() && !window.is_closed());
    drop(d2);
    assert!(window.is_closed());
    assert_eq!(*log.lock(), ["window closed", "display ended"]);
}

/// Set in the child of [`a_window_frame_keeps_its_display_until_it_completes`].
#[cfg(target_os = "macos")]
const METAL_CHILD: &str = "RVG_WINDOW_METAL_CHILD";

/// A display released while a frame the sink drew is in flight ends when the frame completes,
/// not at the release: its port stays taken until then, as for a frame's display
/// (`api.lifetime.display_through_a_committed_frame`). The sink then draws nothing, and has no
/// device to give. On real Metal, in a child, since the process's platform installs the aborting
/// panic hook. A command buffer ahead of the frame's in the frame queue waits for an event the
/// test signals after the release, so the frame is in flight then.
#[cfg(target_os = "macos")]
#[test]
fn a_window_frame_keeps_its_display_until_it_completes() {
    use reims_vgpu_core::platform::device::GpuDevice as _;
    use reims_vgpu_core::platform::event::{EventRef, SharedEvent as _};

    use crate::PixelFormat;
    use crate::platform::api_state;
    use crate::window::metal::MetalSink;

    if std::env::var_os(METAL_CHILD).is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "window::tests::a_window_frame_keeps_its_display_until_it_completes",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(METAL_CHILD, "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains(" 1 passed"),
            "{out:?}"
        );
        return;
    }
    let device = Device::new(DeviceConfig::default()).unwrap();
    let target = device
        .new_frame_target(64, 64, PixelFormat::Bgra8Unorm)
        .unwrap();
    let held = Arc::new(());
    let alive = Arc::downgrade(&held);
    let mut c = DisplayConfig::new(0, 0x5eed_7a00);
    c.callbacks.cursor_move = Some(Arc::new(move || {
        let _ = &held;
    }));
    let display = device.new_display(c).unwrap();
    let (queue, event) = entry::run(|_| {
        let metal = device.core().mtl_device_ref();
        let queue = api_state(device.platform()).frame_queue(metal).unwrap();
        let event = metal.new_shared_event().unwrap();
        let blocker = no_catch(queue.command_buffer(OnFault::Die)).unwrap();
        no_catch(blocker.encode_wait(Some(EventRef::Shared(&event)), 1));
        no_catch(blocker.commit(OnFault::Die));
        (queue, event)
    });
    let sink = MetalSink::new(display.core().downgrade(), queue);
    assert!(sink.device().is_some());
    let region = PixelRect {
        x: 8,
        y: 8,
        width: 48,
        height: 48,
    };
    assert!(sink.encode_into(target.texture(), region, None));
    drop(display);
    assert!(alive.upgrade().is_some());
    assert_eq!(
        device.new_display(DisplayConfig::new(0, 0x5eed_7a01)).err(),
        Some(Error::PortInUse)
    );
    entry::run(|_| no_catch(event.set_signaled_value(1)));
    for _ in 0..500 {
        if alive.upgrade().is_none() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(alive.upgrade().is_none());
    let again = device.new_display(DisplayConfig::new(0, 0x5eed_7a02));
    assert!(again.is_ok());
    assert!(sink.device().is_none());
    assert!(!sink.encode_into(target.texture(), region, None));
}

/// On the Vulkan platform, the window opens on the host screen CoreGraphics gives for the port,
/// as on Metal; the testkit's platform does not ask CoreGraphics
#[cfg(all(feature = "vulkan", target_os = "macos"))]
#[test]
fn a_vulkan_display_s_window_opens_on_the_host_screen() {
    use reims_vgpu_metal::MetalPlatform;
    use reims_vgpu_vulkan::VulkanPlatform;

    assert!(on_host_screens::<VulkanPlatform>());
    assert!(on_host_screens::<MetalPlatform>());
    assert!(!on_host_screens::<FakePlatform>());
    for port in 0..3 {
        assert_eq!(
            host_screen::<VulkanPlatform>(port),
            host_screen::<MetalPlatform>(port)
        );
    }
}

/// A Vulkan display's window has a sink over it, which presents itself, through a swapchain of
/// the window's layer: it names no device for the layer, which the swapchain sets, and the
/// layer's drawables are not framebuffer-only, as the swapchain's images take transfers too;
/// without a frame queue it has none, and the window never opens
#[cfg(all(feature = "vulkan", target_os = "macos"))]
#[test]
fn a_vulkan_display_has_a_window_sink() {
    use reims_vgpu_core::tasks::isolation::TaskThreads;
    use reims_vgpu_vulkan::VulkanPlatform;

    use crate::platform::api_state;

    let platform = Arc::new(VulkanPlatform::new(Box::new(TaskThreads::new())));
    let device = Device::with_platform(platform.clone(), DeviceConfig::default())
        .expect("a Vulkan device (MoltenVK)");
    let display = device
        .new_display(DisplayConfig::new(0, 0x5eed_7a10))
        .unwrap();
    let queue = entry::run(|_| {
        api_state(&platform)
            .frame_queue(device.core().mtl_device_ref())
            .unwrap()
    });
    let sink = window_sink(&platform, display.core().downgrade(), Some(queue)).unwrap();
    assert!(sink.presents());
    assert!(!sink.render_target_only());
    assert!(sink.device().is_none());
    assert!(window_sink(&platform, display.core().downgrade(), None).is_none());
}
