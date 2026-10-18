use std::ffi::c_void;

use reims_vgpu_core::guest::records::APV_PAGE_SHIFT;

use super::*;
use crate::device::RvgDevice;
use crate::display::{
    RvgDisplay, RvgDisplayCallbacks, RvgDisplayConfig, RvgDisplayMode, rvg_display_create,
    rvg_display_release, rvg_display_set_host_cursor_hidden, rvg_display_set_modes,
};
use crate::sized;
use crate::status::*;
use crate::testkit::{Rec, Rig, t_destroy};

unsafe extern "C-unwind" fn t_pointer(ctx: *mut c_void, x: u16, y: u16, w: u16, h: u16, b: u32) {
    Rec::push(ctx, format!("pointer {x},{y} in {w}x{h} buttons {b:#x}"));
}
unsafe extern "C-unwind" fn t_wheel(ctx: *mut c_void, dy: i32, dx: i32) {
    Rec::push(ctx, format!("wheel {dy},{dx}"));
}
unsafe extern "C-unwind" fn t_key(ctx: *mut c_void, usage: u16, down: bool) {
    Rec::push(ctx, format!("key {usage:#x} {down}"));
}
unsafe extern "C-unwind" fn t_close(ctx: *mut c_void) {
    Rec::push(ctx, "close".to_owned());
}
unsafe extern "C-unwind" fn t_scroll(ctx: *mut c_void, scroll: *const RvgWindowScroll) {
    // SAFETY: the library's event, live for the call.
    let s = unsafe { &*scroll };
    Rec::push(
        ctx,
        format!(
            "scroll {} bytes {},{} phase {} momentum {} notches {},{}",
            s.struct_size, s.dy, s.dx, s.phase, s.momentum, s.notches_dy, s.notches_dx
        ),
    );
}
/// A destroy that writes its table's name into the shared recorder, for the order of two.
unsafe extern "C-unwind" fn t_window_destroy(ctx: *mut c_void) {
    Rec::push(ctx, "window destroy".to_owned());
}
unsafe extern "C-unwind" fn t_display_destroy(ctx: *mut c_void) {
    Rec::push(ctx, "display destroy".to_owned());
}

fn window_table(rec: &Arc<Rec>) -> RvgWindowCallbacks {
    RvgWindowCallbacks {
        struct_size: sized::size_u32::<RvgWindowCallbacks>(),
        ctx: rec.ctx(),
        destroy: Some(t_destroy),
        pointer: Some(t_pointer),
        wheel: Some(t_wheel),
        key: Some(t_key),
        close: Some(t_close),
        scroll: Some(t_scroll),
    }
}

fn display_table(rec: &Arc<Rec>) -> RvgDisplayCallbacks {
    RvgDisplayCallbacks {
        struct_size: sized::size_u32::<RvgDisplayCallbacks>(),
        ctx: rec.ctx(),
        destroy: Some(t_destroy),
        mode_change: None,
        new_frame: None,
        cursor_glyph: None,
        cursor_show: None,
        cursor_move: None,
    }
}

fn window(flags: u32, cb: *const RvgWindowCallbacks) -> RvgWindowConfig {
    RvgWindowConfig {
        struct_size: sized::size_u32::<RvgWindowConfig>(),
        flags,
        callbacks: cb,
        mode_width: 0,
        mode_height: 0,
        mode_pixels_per_inch: 0.0,
    }
}

fn config(
    port: u64,
    serial: u32,
    cb: *const RvgDisplayCallbacks,
    window: *const RvgWindowConfig,
) -> RvgDisplayConfig {
    RvgDisplayConfig {
        struct_size: sized::size_u32::<RvgDisplayConfig>(),
        serial_number: serial,
        port,
        name: std::ptr::null(),
        name_length: 0,
        width_millimeters: 0.0,
        height_millimeters: 0.0,
        callbacks: cb,
        window,
    }
}

fn create(device: *mut RvgDevice, c: *const RvgDisplayConfig) -> (RvgStatus, *mut RvgDisplay) {
    let mut d = std::ptr::null_mut();
    // SAFETY: a live device or NULL, a live config; `d` is writable.
    let s = unsafe { rvg_display_create(device, c, &mut d) };
    (s, d)
}

/// The config's first version ends before `window`, at 56 bytes: a host built against it passes
/// 56, and the library reads no window
#[test]
fn a_first_version_display_config_is_56_bytes() {
    assert_eq!(<RvgDisplayConfig as HostStruct>::V1_SIZE, 56);
    assert_eq!(std::mem::offset_of!(RvgDisplayConfig, window), 56);
    assert_eq!(size_of::<RvgDisplayConfig>(), 64);
}

#[test]
fn host_cursor_policy_flag_is_additive_and_default_off() {
    assert_eq!(RVG_WINDOW_HIDE_HOST_CURSOR, 2);
    assert!(
        !window(0, std::ptr::null())
            .to_config(Default::default())
            .hide_host_cursor
    );
    assert!(
        window(RVG_WINDOW_HIDE_HOST_CURSOR, std::ptr::null())
            .to_config(Default::default())
            .hide_host_cursor
    );
}

#[test]
fn host_cursor_policy_c_api_keeps_guest_capabilities_and_refuses_missing_windows() {
    use reims_vgpu_core::display::page as sp;
    let rig = Rig::new(3);
    let (status, plain) = create(
        rig.device,
        &config(1, 0x5eed_d111, std::ptr::null(), std::ptr::null()),
    );
    assert_eq!(status, RVG_OK);
    // SAFETY: live handles or NULL, borrowed for each call.
    unsafe {
        assert_eq!(
            rvg_display_set_host_cursor_hidden(std::ptr::null_mut(), 1),
            RVG_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            rvg_display_set_host_cursor_hidden(plain, 1),
            RVG_ERR_UNSUPPORTED
        );
        rvg_display_release(plain);
        crate::device::rvg_device_mmio_write(rig.device, 0x101c, 1);
    }
    // SAFETY: the rig owns its live device.
    let core = unsafe { &*rig.device }.device.core();
    core.set_shared_display_state_page(0x10, 0, false).unwrap();
    let normal_window = window(0, std::ptr::null());
    let (status, display) = create(
        rig.device,
        &config(0, 0x5eed_d112, std::ptr::null(), &normal_window),
    );
    assert_eq!(status, RVG_OK);
    let mode = RvgDisplayMode {
        width: 640,
        height: 480,
        flags: 0,
        refresh_rate_hz: 60.0,
    };
    // SAFETY: a live display and one mode.
    unsafe { rvg_display_set_modes(display, &mode, 1) };
    rig.p.fake_vm().sync();
    let ram = rig._host.ram();
    let read = |offset, length| {
        let bytes = ram
            .read_vec((0x10 << APV_PAGE_SHIFT) + offset, length)
            .unwrap();
        bytes
            .iter()
            .rev()
            .fold(0u32, |value, byte| (value << 8) | u32::from(*byte))
    };
    let seen = || {
        (
            read(sp::CURSOR_GLYPH_MAX_WIDTH, 2),
            read(sp::CURSOR_GLYPH_MAX_HEIGHT, 2),
            read(sp::CURSOR_HANDLERS, 4),
        )
    };
    assert_eq!(seen(), (256, 256, 3));
    for hidden in [1, 255, 0] {
        // SAFETY: the display is live throughout this loop.
        let result = unsafe { rvg_display_set_host_cursor_hidden(display, hidden) };
        assert_eq!(
            result,
            if cfg!(any(target_os = "linux", windows)) {
                RVG_OK
            } else {
                RVG_ERR_UNSUPPORTED
            }
        );
        assert_eq!(seen(), (256, 256, 3));
    }
    // SAFETY: the rig owns the display's reference.
    unsafe { rvg_display_release(display) };
    let initial = window(RVG_WINDOW_HIDE_HOST_CURSOR, std::ptr::null());
    let (result, display) = create(
        rig.device,
        &config(2, 0x5eed_d113, std::ptr::null(), &initial),
    );
    if cfg!(any(target_os = "linux", windows)) {
        assert_eq!(result, RVG_OK);
        // SAFETY: successful create returned this reference.
        unsafe { rvg_display_release(display) };
    } else {
        assert_eq!(result, RVG_ERR_UNSUPPORTED);
        assert!(display.is_null());
    }
}

/// The window config's first version ends before its mode, at 16 bytes: a host built against it
/// passes 16, and the library reads no mode; a mode with either side 0 is none
#[test]
fn a_first_version_window_config_is_16_bytes_and_has_no_mode() {
    assert_eq!(<RvgWindowConfig as HostStruct>::V1_SIZE, 16);
    assert_eq!(std::mem::offset_of!(RvgWindowConfig, mode_width), 16);
    assert_eq!(size_of::<RvgWindowConfig>(), 32);
    // A v1 config, followed in memory by a mode it does not have.
    let mut w = window(RVG_WINDOW_MATCH_HOST_SCREEN, std::ptr::null());
    (w.mode_width, w.mode_height) = (1920, 1080);
    w.struct_size = 16;
    // SAFETY: `w` is a live config of `struct_size` readable bytes.
    let read = unsafe { sized::read(&raw const w) }.unwrap().unwrap();
    let c = read.to_config(WindowCallbacks::default());
    assert!(c.match_host_screen && c.mode.is_none());
    let mut w = window(RVG_WINDOW_MATCH_HOST_SCREEN, std::ptr::null());
    (w.mode_width, w.mode_height) = (1920, 1080);
    assert_eq!(
        w.to_config(WindowCallbacks::default()).mode,
        Some((1920, 1080))
    );
    w.mode_height = 0;
    assert_eq!(w.to_config(WindowCallbacks::default()).mode, None);
}

/// The window config's second version ends at its mode, at 24 bytes: a host built against it
/// passes 24, and the library reads no density; the density is read past it, and one that is
/// not a positive finite number is none
#[test]
fn a_second_version_window_config_is_24_bytes_and_has_no_density() {
    assert_eq!(
        std::mem::offset_of!(RvgWindowConfig, mode_pixels_per_inch),
        24
    );
    // A v2 config, followed in memory by a density it does not have.
    let mut w = window(RVG_WINDOW_MATCH_HOST_SCREEN, std::ptr::null());
    (w.mode_width, w.mode_height) = (2560, 1600);
    w.mode_pixels_per_inch = 200.0;
    w.struct_size = 24;
    // SAFETY: `w` is a live config of `struct_size` readable bytes.
    let read = unsafe { sized::read(&raw const w) }.unwrap().unwrap();
    let c = read.to_config(WindowCallbacks::default());
    assert_eq!((c.mode, c.mode_ppi), (Some((2560, 1600)), None));
    w.struct_size = 32;
    // SAFETY: as above.
    let read = unsafe { sized::read(&raw const w) }.unwrap().unwrap();
    let c = read.to_config(WindowCallbacks::default());
    assert_eq!((c.mode, c.mode_ppi), (Some((2560, 1600)), Some(200.0)));
    for ppi in [0.0, -200.0, f64::NAN, f64::INFINITY] {
        w.mode_pixels_per_inch = ppi;
        assert_eq!(
            w.to_config(WindowCallbacks::default()).mode_ppi,
            None,
            "{ppi}"
        );
    }
}

/// A config of 56 bytes is read, and the bytes past it are not: what follows it in the host's
/// memory is no window, and the display has only the callbacks its table gives
#[test]
fn a_56_byte_display_config_is_accepted() {
    let rig = Rig::new(1);
    let rec = Rec::new();
    let cb = display_table(&rec);
    // A v1 config, followed in memory by a word that is no window config.
    let mut c = config(
        0,
        0x5eed_d001,
        &cb,
        std::ptr::without_provenance(0xdead_0000),
    );
    c.struct_size = 56;
    let (s, d) = create(rig.device, &c);
    assert_eq!(s, RVG_OK);
    // SAFETY: the display made above.
    let core = unsafe { &*d }.display.core();
    assert!(core.cursor_glyph_handler().is_none() && core.new_frame_event_handler().is_none());
    // SAFETY: the display's reference.
    unsafe { rvg_display_release(d) };
    assert_eq!(rec.wait_destroys(1).len(), 1);
    c.struct_size = 55;
    assert_eq!(create(rig.device, &c).0, RVG_ERR_INVALID_ARGUMENT);
    assert_eq!(rec.destroys().len(), 1);
}

/// Each closure calls its C function with the table's ctx and the arguments the window passes;
/// an absent function is no closure
#[test]
fn window_callbacks_forward() {
    let rec = Rec::new();
    let t = Table::new(rec.ctx(), None);
    let cb = window_callbacks(&t, &window_table(&rec));
    (cb.pointer.unwrap())(
        10,
        20,
        640,
        480,
        RVG_WINDOW_BUTTON_LEFT | RVG_WINDOW_BUTTON_EXTRA,
    );
    (cb.wheel.unwrap())(-2, 1);
    (cb.key.as_ref().unwrap())(0x04, true);
    (cb.key.unwrap())(0x04, false);
    (cb.close.unwrap())();
    let scroll = cb.scroll.unwrap();
    let event = |dy, dx, phase, momentum, notches| ScrollEvent {
        dy,
        dx,
        phase,
        momentum,
        notches,
    };
    scroll(event(
        1536,
        -256,
        ScrollPhase::Began,
        MomentumPhase::None,
        (0, 0),
    ));
    scroll(event(0, 0, ScrollPhase::Ended, MomentumPhase::None, (0, 0)));
    scroll(event(
        10240,
        0,
        ScrollPhase::None,
        MomentumPhase::Changed,
        (1, -2),
    ));
    assert_eq!(
        rec.lines(),
        [
            "pointer 10,20 in 640x480 buttons 0x11",
            "wheel -2,1",
            "key 0x4 true",
            "key 0x4 false",
            "close",
            "scroll 28 bytes 1536,-256 phase 2 momentum 0 notches 0,0",
            "scroll 28 bytes 0,0 phase 4 momentum 0 notches 0,0",
            "scroll 28 bytes 10240,0 phase 0 momentum 2 notches 1,-2",
        ]
    );
    let none = RvgWindowCallbacks {
        pointer: None,
        wheel: None,
        key: None,
        close: None,
        scroll: None,
        ..window_table(&rec)
    };
    let cb = window_callbacks(&t, &none);
    assert!(cb.pointer.is_none() && cb.wheel.is_none() && cb.key.is_none());
    assert!(cb.close.is_none() && cb.scroll.is_none());
}

/// Each phase is its RVG_WINDOW_SCROLL_PHASE_* and each momentum its
/// RVG_WINDOW_SCROLL_MOMENTUM_*
#[test]
fn scroll_phases_are_their_constants() {
    let phases = [
        (ScrollPhase::None, RVG_WINDOW_SCROLL_PHASE_NONE),
        (ScrollPhase::MayBegin, RVG_WINDOW_SCROLL_PHASE_MAY_BEGIN),
        (ScrollPhase::Began, RVG_WINDOW_SCROLL_PHASE_BEGAN),
        (ScrollPhase::Changed, RVG_WINDOW_SCROLL_PHASE_CHANGED),
        (ScrollPhase::Ended, RVG_WINDOW_SCROLL_PHASE_ENDED),
        (ScrollPhase::Cancelled, RVG_WINDOW_SCROLL_PHASE_CANCELLED),
    ];
    for (phase, value) in phases {
        assert_eq!(phase_value(phase), value, "{phase:?}");
    }
    let momenta = [
        (MomentumPhase::None, RVG_WINDOW_SCROLL_MOMENTUM_NONE),
        (MomentumPhase::Began, RVG_WINDOW_SCROLL_MOMENTUM_BEGAN),
        (MomentumPhase::Changed, RVG_WINDOW_SCROLL_MOMENTUM_CHANGED),
        (MomentumPhase::Ended, RVG_WINDOW_SCROLL_MOMENTUM_ENDED),
    ];
    for (momentum, value) in momenta {
        assert_eq!(momentum_value(momentum), value, "{momentum:?}");
    }
}

/// The callbacks' first version ends before `scroll`, at 56 bytes: a host built against it
/// passes 56, and has no scroll callback, whatever follows the table in its memory
#[test]
fn a_first_version_window_callbacks_is_56_bytes_and_has_no_scroll() {
    assert_eq!(<RvgWindowCallbacks as HostStruct>::V1_SIZE, 56);
    assert_eq!(std::mem::offset_of!(RvgWindowCallbacks, scroll), 56);
    assert_eq!(size_of::<RvgWindowCallbacks>(), 64);
    let rec = Rec::new();
    // A v1 table, followed in memory by a scroll callback it does not have.
    let mut c = window_table(&rec);
    c.struct_size = 56;
    // SAFETY: `c` is a live table of `struct_size` readable bytes.
    let read = unsafe { sized::read(&raw const c) }.unwrap().unwrap();
    assert!(read.scroll.is_none() && read.close.is_some());
    c.struct_size = 64;
    // SAFETY: as above.
    let read = unsafe { sized::read(&raw const c) }.unwrap().unwrap();
    assert!(read.scroll.is_some());
    c.struct_size = 55;
    // SAFETY: as above.
    assert_eq!(
        unsafe { sized::read(&raw const c) }.err(),
        Some(RVG_ERR_INVALID_ARGUMENT)
    );
}

/// RVG_WINDOW_MATCH_HOST_SCREEN is the config's host-screen match; other bits are ignored
#[test]
fn window_flags() {
    let to = |flags| window(flags, std::ptr::null()).to_config(WindowCallbacks::default());
    assert!(!to(0).match_host_screen);
    assert!(to(RVG_WINDOW_MATCH_HOST_SCREEN).match_host_screen);
    assert!(!to(!RVG_WINDOW_MATCH_HOST_SCREEN).match_host_screen);
    assert!(to(u32::MAX).match_host_screen);
}

/// With a window and no display callback of the host's, the guest's display page reads a
/// cursor glyph size limit of 256 and both cursor handler bits, as with all five callbacks;
/// without a window and with none, it reads 0 and none
#[test]
fn a_window_display_has_every_callback_the_guest_reads() {
    use reims_vgpu_core::display::page as sp;
    let rig = Rig::new(2);
    // The guest's root node, then its display pages for ports 0 and 1 at guest pages 0x10, 0x11.
    // SAFETY: the rig's live device.
    unsafe { crate::device::rvg_device_mmio_write(rig.device, 0x101c, 1) };
    // SAFETY: as above.
    let core = unsafe { &*rig.device }.device.core();
    for (page, port) in [(0x10, 0), (0x11, 1)] {
        core.set_shared_display_state_page(page, port, false)
            .unwrap();
    }
    let wrec = Rec::new();
    let wcb = window_table(&wrec);
    let w = window(0, &wcb);
    let (s, with) = create(rig.device, &config(0, 0x5eed_d101, std::ptr::null(), &w));
    assert_eq!(s, RVG_OK);
    let (s, without) = create(
        rig.device,
        &config(1, 0x5eed_d102, std::ptr::null(), std::ptr::null()),
    );
    assert_eq!(s, RVG_OK);
    let mode = RvgDisplayMode {
        width: 640,
        height: 480,
        flags: 0,
        refresh_rate_hz: 60.0,
    };
    // SAFETY: the displays made above; one mode each.
    unsafe {
        rvg_display_set_modes(with, &mode, 1);
        rvg_display_set_modes(without, &mode, 1);
    }
    rig.p.fake_vm().sync();
    let ram = rig._host.ram();
    let read = |page: u64, off: u64, len: u64| {
        let b = ram.read_vec((page << APV_PAGE_SHIFT) + off, len).unwrap();
        b.iter().rev().fold(0u32, |v, &x| (v << 8) | u32::from(x))
    };
    let seen = |page| {
        (
            read(page, sp::CURSOR_GLYPH_MAX_WIDTH, 2),
            read(page, sp::CURSOR_GLYPH_MAX_HEIGHT, 2),
            read(page, sp::CURSOR_HANDLERS, 4),
        )
    };
    assert_eq!(seen(0x10), (256, 256, 3));
    assert_eq!(seen(0x11), (0, 0, 0));
    // SAFETY: the displays' references.
    unsafe {
        rvg_display_release(with);
        rvg_display_release(without);
    }
    assert_eq!(wrec.destroys().len(), 1);
}

/// The release ends the window's table once, on the releasing thread before it returns, with
/// functions or without; the display's own table ends once too, maybe after, on the display
/// thread
#[test]
fn a_release_ends_the_window_table_once() {
    let rig = Rig::new(1);
    let me = std::thread::current().id();
    for with_functions in [true, false] {
        let (drec, wrec) = (Rec::new(), Rec::new());
        let dcb = display_table(&drec);
        let mut wcb = window_table(&wrec);
        if !with_functions {
            (wcb.pointer, wcb.wheel, wcb.key, wcb.close) = (None, None, None, None);
            wcb.scroll = None;
        }
        let w = window(RVG_WINDOW_MATCH_HOST_SCREEN, &wcb);
        let serial = if with_functions {
            0x5eed_d201
        } else {
            0x5eed_d202
        };
        let (s, d) = create(rig.device, &config(0, serial, &dcb, &w));
        assert_eq!(s, RVG_OK);
        assert!(wrec.destroys().is_empty() && drec.destroys().is_empty());
        // SAFETY: the display's reference.
        unsafe { rvg_display_release(d) };
        assert_eq!(wrec.destroys(), [me], "{with_functions}");
        assert_eq!(drec.wait_destroys(1).len(), 1);
        assert_eq!(wrec.destroys().len(), 1);
        assert!(wrec.lines().is_empty());
    }
}

/// At the release, the window's table ends before the display's: the display's last reference
/// closes the window before it lets the display go
#[test]
fn a_release_ends_the_window_table_first() {
    let rig = Rig::new(1);
    let rec = Rec::new();
    let dcb = RvgDisplayCallbacks {
        destroy: Some(t_display_destroy),
        cursor_move: Some(t_close),
        ..display_table(&rec)
    };
    let wcb = RvgWindowCallbacks {
        destroy: Some(t_window_destroy),
        ..window_table(&rec)
    };
    let w = window(0, &wcb);
    let (s, d) = create(rig.device, &config(0, 0x5eed_d301, &dcb, &w));
    assert_eq!(s, RVG_OK);
    // SAFETY: the display's reference.
    unsafe { rvg_display_release(d) };
    for _ in 0..1000 {
        if rec.lines().len() >= 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(rec.lines(), ["window destroy", "display destroy"]);
}

/// Each refusal after the reads ends both tables once, on the caller's thread before the create
/// returns, the window's first
#[test]
fn refusals_end_both_tables() {
    let rig = Rig::new(1);
    let rec = Rec::new();
    let cb = display_table(&rec);
    let (s, d) = create(rig.device, &config(0, 0x5eed_d401, &cb, std::ptr::null()));
    assert_eq!(s, RVG_OK);
    let me = std::thread::current().id();
    let (drec, wrec) = (Rec::new(), Rec::new());
    let dcb = display_table(&drec);
    let wcb = window_table(&wrec);
    let w = window(RVG_WINDOW_MATCH_HOST_SCREEN, &wcb);
    let mut c = config(0, 0x5eed_d402, &dcb, &w);
    let mut refused = 0;
    let mut check = |s: RvgStatus, want: RvgStatus| {
        assert_eq!(s, want);
        refused += 1;
        assert_eq!(drec.destroys(), vec![me; refused], "{want}");
        assert_eq!(wrec.destroys(), vec![me; refused], "{want}");
    };
    check(create(std::ptr::null_mut(), &c).0, RVG_ERR_INVALID_ARGUMENT);
    let mut out = std::ptr::null_mut();
    // SAFETY: the rig's live device, a live config and a NULL out-pointer.
    check(
        unsafe { rvg_display_create(rig.device, &c, std::ptr::null_mut()) },
        RVG_ERR_INVALID_ARGUMENT,
    );
    c.port = 1;
    check(create(rig.device, &c).0, RVG_ERR_PORT_OUT_OF_RANGE);
    c.port = 0;
    c.serial_number = 0x5eed_d401;
    check(create(rig.device, &c).0, RVG_ERR_SERIAL_IN_USE);
    c.serial_number = 0x5eed_d402;
    check(create(rig.device, &c).0, RVG_ERR_PORT_IN_USE);
    assert!(wrec.lines().is_empty() && drec.lines().is_empty());
    // The window's destroy before the display's.
    let order = Rec::new();
    let dcb = RvgDisplayCallbacks {
        ctx: order.ctx(),
        destroy: Some(t_display_destroy),
        ..dcb
    };
    let wcb = RvgWindowCallbacks {
        ctx: order.ctx(),
        destroy: Some(t_window_destroy),
        ..wcb
    };
    let w = window(0, &wcb);
    let c = config(0, 0x5eed_d403, &dcb, &w);
    // SAFETY: the rig's live device, a live config; `out` is writable.
    let s = unsafe { rvg_display_create(rig.device, &c, &mut out) };
    assert_eq!(s, RVG_ERR_PORT_IN_USE);
    assert_eq!(order.lines(), ["window destroy", "display destroy"]);
    // SAFETY: the display made above.
    unsafe { rvg_display_release(d) };
    assert_eq!(rec.wait_destroys(1).len(), 1);
}

/// A window config, or its callbacks, refused for its `struct_size` runs neither table's
/// destroy: the create has not taken either table
#[test]
fn a_window_refused_for_its_size_runs_no_destroy() {
    let rig = Rig::new(1);
    let (drec, wrec) = (Rec::new(), Rec::new());
    let dcb = display_table(&drec);
    let mut wcb = window_table(&wrec);
    let mut w = window(0, &wcb);
    w.struct_size = 4;
    assert_eq!(
        create(rig.device, &config(0, 0x5eed_d501, &dcb, &w)).0,
        RVG_ERR_INVALID_ARGUMENT
    );
    w.struct_size = sized::size_u32::<RvgWindowConfig>();
    wcb.struct_size = 4;
    assert_eq!(
        create(rig.device, &config(0, 0x5eed_d502, &dcb, &w)).0,
        RVG_ERR_INVALID_ARGUMENT
    );
    // A longer window config with a byte the library does not know.
    #[repr(C)]
    struct Longer {
        known: RvgWindowConfig,
        more: u64,
    }
    wcb.struct_size = sized::size_u32::<RvgWindowCallbacks>();
    let mut longer = Longer {
        known: window(0, &wcb),
        more: 1,
    };
    longer.known.struct_size = sized::size_u32::<Longer>();
    let lw = (&raw const longer).cast::<RvgWindowConfig>();
    assert_eq!(
        create(rig.device, &config(0, 0x5eed_d503, &dcb, lw)).0,
        RVG_ERR_UNSUPPORTED
    );
    assert!(drec.destroys().is_empty() && wrec.destroys().is_empty());
    // With the unknown bytes zero, it is read.
    longer.more = 0;
    let lw = (&raw const longer).cast::<RvgWindowConfig>();
    let (s, d) = create(rig.device, &config(0, 0x5eed_d504, &dcb, lw));
    assert_eq!(s, RVG_OK);
    // SAFETY: the display's reference.
    unsafe { rvg_display_release(d) };
    assert_eq!(wrec.destroys().len(), 1);
    assert_eq!(drec.wait_destroys(1).len(), 1);
}
