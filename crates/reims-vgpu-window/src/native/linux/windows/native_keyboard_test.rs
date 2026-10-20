//! Run alone with all VMs and GPU tests stopped. No renderer, Vulkan or global input.

use super::*;
use crate::config::WindowCallbacks;
use crate::native::linux::{input, keymap};
use reims_vgpu_core::sync::Mutex;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[repr(C)]
#[derive(Default)]
struct NativeMessage {
    window: *mut c_void, message: u32, wparam: usize, lparam: isize,
    time: u32, point: [i32; 2], private: u32,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn CreateWindowExW(ex: u32, class: *const u16, name: *const u16, style: u32,
        x: i32, y: i32, width: i32, height: i32, parent: *mut c_void,
        menu: *mut c_void, instance: *mut c_void, param: *mut c_void) -> *mut c_void;
    fn DestroyWindow(window: *mut c_void) -> i32;
    fn IsWindow(window: *mut c_void) -> i32;
    fn SetForegroundWindow(window: *mut c_void) -> i32;
    fn SetFocus(window: *mut c_void) -> *mut c_void;
    fn SetParent(window: *mut c_void, parent: *mut c_void) -> *mut c_void;
    fn SetWindowLongPtrW(window: *mut c_void, index: i32, value: isize) -> isize;
    fn ShowWindowAsync(window: *mut c_void, command: i32) -> i32;
    fn PostMessageW(window: *mut c_void, message: u32, wparam: usize, lparam: isize) -> i32;
    fn PeekMessageW(message: *mut NativeMessage, window: *mut c_void, first: u32, last: u32, remove: u32) -> i32;
    fn TranslateMessage(message: *const NativeMessage) -> i32;
    fn DispatchMessageW(message: *const NativeMessage) -> isize;
    fn GetWindowThreadProcessId(window: *mut c_void, pid: *mut u32) -> u32;
    fn GetWindowRect(window: *mut c_void, rect: *mut [i32; 4]) -> i32;
}

const PARENT_TEST: &str = "native::linux::windows::native_keyboard_test::native_parent_fixture";

/// A separate process owns the non-SDL parent, matching Manager/QEMU ownership.
#[test]
#[ignore = "owned helper process for the isolated native SDL test"]
fn native_parent_fixture() {
    let path = std::path::PathBuf::from(std::env::var_os("REIMS_NATIVE_PARENT_RECEIPT").expect("owned helper only"));
    let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
    let title: Vec<u16> = "Reims native input test (not a VM)\0".encode_utf16().collect();
    // SAFETY: this test creates and operates only its own ordinary Win32 window.
    unsafe {
        let window = CreateWindowExW(0, class.as_ptr(), title.as_ptr(), 0x10cf0000,
            60, 60, 680, 480, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
        assert!(!window.is_null(), "owned parent creation failed");
        SetForegroundWindow(window);
        std::fs::write(&path, (window as usize).to_string()).expect("parent receipt");
        let mut activated = false;
        let start = Instant::now();
        while IsWindow(window) != 0 && start.elapsed() < Duration::from_secs(150) {
            if !activated && path.with_extension("activate").is_file() {
                activated = true;
                let accepted = SetForegroundWindow(window);
                std::fs::write(path.with_extension("active"), accepted.to_string()).expect("activation receipt");
            }
            let mut message = NativeMessage::default();
            while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, 1) != 0 {
                TranslateMessage(&message); DispatchMessageW(&message);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        if IsWindow(window) != 0 { DestroyWindow(window); }
    }
}

struct Receiver { window: *mut SDL_Window, input: input::Input, callbacks: WindowCallbacks }

fn pump(receivers: &mut [Receiver]) -> usize {
    let mut zero_keys = 0;
    loop {
        let mut event = SDL_Event::default();
        // SAFETY: this fixture owns SDL's video thread and its windows.
        let ready = unsafe { SDL_PollEvent(&mut event) };
        if let Some((old, new)) = take_focus_change() {
            for receiver in receivers.iter_mut() {
                let id = unsafe { SDL_GetWindowID(receiver.window).0 };
                if old == Some(id) { receiver.input.focus_lost(); }
                if new == Some(id) { receiver.input.focus_gained(unsafe { SDL_GetTicksNS() }); }
                input::send(&receiver.callbacks, receiver.input.take());
            }
        }
        if !ready { break; }
        let kind = unsafe { event.r#type };
        if kind != SDL_EVENT_KEY_DOWN.0 && kind != SDL_EVENT_KEY_UP.0 { continue; }
        let key = unsafe { event.key };
        // Never include an unexpected physical key or a key sequence in diagnostics.
        assert!(matches!(key.scancode.0, 4 | 5 | 0xe3 | 0x90 | 0x91), "unexpected input during owned native fixture");
        zero_keys += usize::from(key.windowID.0 == 0);
        if let Some(id) = key_window(&key) {
            for receiver in receivers.iter_mut() {
                if unsafe { SDL_GetWindowID(receiver.window).0 } == id {
                    if let Some(usage) = keymap::usb_usage(key.scancode.0) {
                        receiver.input.key(usage, key.down, key.repeat);
                        input::send(&receiver.callbacks, receiver.input.take());
                    }
                }
            }
        }
    }
    zero_keys
}

fn post(window: *mut SDL_Window, vk: usize, scan: u8, down: bool) {
    let flags = (isize::from(scan) << 16) | 1 | if down { 0 } else { 0xc0000000_u32 as isize };
    // SAFETY: targeted messages reach only the owned SDL HWND, through SDL's real WndProc.
    assert_ne!(unsafe { PostMessageW(hwnd(window), if down { 0x100 } else { 0x101 }, vk, flags) }, 0);
}

#[test]
#[ignore = "opens owned cross-process windows; run alone with VM and GPU work stopped"]
fn native_embedded_sdl_keyboard_reaches_input() {
    let receipt = std::env::temp_dir().join(format!("reims-native-parent-{}", std::process::id()));
    let _ = std::fs::remove_file(&receipt);
    let mut parent = Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", PARENT_TEST, "--ignored", "--test-threads=1"])
        .env("REIMS_NATIVE_PARENT_RECEIPT", &receipt).stdout(Stdio::null()).stderr(Stdio::null())
        .spawn().expect("owned parent helper");
    let start = Instant::now();
    let root = loop {
        if let Ok(value) = std::fs::read_to_string(&receipt) {
            if let Ok(value) = value.parse::<usize>() { break value as *mut c_void; }
        }
        assert!(start.elapsed() < Duration::from_secs(10), "owned parent startup timed out");
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut windows = Vec::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(root, &mut pid);
        assert_eq!(pid, parent.id(), "parent must belong to the owned other process");
        assert!(SDL_Init(SDL_INIT_VIDEO), "SDL video initialization failed");
        SDL_SetHint(c"SDL_WINDOWS_RAW_KEYBOARD".as_ptr(), c"0".as_ptr());
        install_message_hook();
        let records = Arc::new(Mutex::new(Vec::new()));
        let mut receivers = Vec::new();
        for index in 0..2 {
            let window = SDL_CreateWindow(c"Reims native input test (not a VM)".as_ptr(), 320, 200, SDL_WINDOW_HIDDEN);
            assert!(!window.is_null(), "owned SDL window creation failed");
            windows.push(window); track(window, true);
            let native = hwnd(window);
            let style = GetWindowLongPtrW(native, -16);
            SetWindowLongPtrW(native, -16, (style & !(0x80000000_u32 as isize)) | 0x40000000);
            SetParent(native, root); ShowWindowAsync(native, 8);
            let records = Arc::clone(&records);
            receivers.push(Receiver { window, input: input::Input::new(), callbacks: WindowCallbacks {
                key: Some(Arc::new(move |usage, down| records.lock().push((index, usage, down)))),
                ..WindowCallbacks::default()
            }});
        }
        std::fs::write(receipt.with_extension("activate"), b"").expect("owned activation request");
        let start = Instant::now();
        while !receipt.with_extension("active").is_file() {
            assert!(start.elapsed() < Duration::from_secs(5), "owned parent activation timed out");
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut bounds = [0; 4];
        assert_ne!(GetWindowRect(root, &mut bounds), 0, "owned parent bounds query failed");
        println!("native entry ready: parent_pid={} parent_hwnd={} controller_pid={} bounds={bounds:?} visible={} wait_seconds=120; click Reims native input test (not a VM), do not type", parent.id(), root as usize, std::process::id(), IsWindowVisible(root));
        let start = Instant::now();
        while GetForegroundWindow() != root {
            assert!(start.elapsed() < Duration::from_secs(120), "owned native entry was not activated; no keyboard route was tested");
            pump(&mut receivers);
            std::thread::sleep(Duration::from_millis(10));
        }
        SetFocus(hwnd(windows[0]));
        pump(&mut receivers);
        assert_eq!(GetForegroundWindow(), root, "owned parent must be genuinely foreground");
        assert_eq!(embedded_focus(windows[0]), Some(true));
        assert!(SDL_GetKeyboardFocus().is_null(), "SDL must retain its real foreign-child focus limitation");
        post(windows[0], 0x41, 0x1e, true); post(windows[0], 0x41, 0x1e, false);
        assert!(pump(&mut receivers) >= 2, "real SDL translated keys must have ID zero");
        assert!(*records.lock() == [(0, 4, true), (0, 4, false)], "owned key callbacks failed");
        records.lock().clear();
        for (vk, scan, usage) in [(0x19, 0xf1, 0x91), (0x16, 0xf2, 0x90)] {
            post(windows[0], vk, scan, true); post(windows[0], vk, scan, false);
            pump(&mut receivers);
            assert!(*records.lock() == [(0, usage, true), (0, usage, false)], "owned JIS callbacks failed");
            records.lock().clear();
        }
        post(windows[0], 0x41, 0x1e, true); SDL_PumpEvents();
        SetFocus(hwnd(windows[1]));
        post(windows[0], 0x41, 0x1e, true); // queued old native HWND must not become owner B.
        post(windows[1], 0x42, 0x30, true); post(windows[1], 0x42, 0x30, false);
        pump(&mut receivers);
        assert!(*records.lock() == [(1, 5, true), (1, 5, false)], "old or reset keys leaked to another owner");
        records.lock().clear();
        SetFocus(hwnd(windows[0])); pump(&mut receivers);
        post(windows[0], 0x41, 0x1e, true); SDL_PumpEvents();
        SetFocus(root); SetFocus(hwnd(windows[0]));
        post(windows[0], 0x42, 0x30, true); post(windows[0], 0x42, 0x30, false);
        pump(&mut receivers);
        assert!(*records.lock() == [(0, 5, true), (0, 5, false)], "old ownership generation survived reentry");
        records.lock().clear();
        post(windows[0], 0x41, 0x1e, true); pump(&mut receivers);
        SetFocus(root); pump(&mut receivers);
        assert!(*records.lock() == [(0, 4, true), (0, 4, false)], "native focus loss did not release held keys");
        println!("foreign_parent=true real_sdl_id_zero=true callbacks=true jis=true owner_generation=true reset_no_leak=true held_release=true no_renderer=true");
    }));
    // Only this fixture's windows and helper process are cleaned up.
    unsafe {
        for window in windows { track(window, false); SDL_DestroyWindow(window); }
        SDL_RemoveEventWatch(Some(key_watch), std::ptr::null_mut()); SDL_Quit();
        PostMessageW(root, 0x0010, 0, 0);
    }
    let deadline = Instant::now();
    while parent.try_wait().expect("owned parent status").is_none() {
        if deadline.elapsed() > Duration::from_secs(5) { parent.kill().expect("owned helper cleanup"); break; }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = parent.wait();
    let _ = std::fs::remove_file(receipt.with_extension("activate"));
    let _ = std::fs::remove_file(receipt.with_extension("active"));
    let _ = std::fs::remove_file(receipt);
    if let Err(error) = result { std::panic::resume_unwind(error); }
}
