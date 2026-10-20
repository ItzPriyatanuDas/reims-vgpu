//! Windows message keyboard input, including foreign-process embedded windows.
//!
//! Read from SDL's Windows backend: a child HWND cannot equal the foreground
//! top-level HWND, so SDL's keyboard focus stays null and translated keys can
//! have window ID zero. Native focus gates only this library's owned children.
//! An event watch records the native message's owner and focus generation before
//! routing; it never changes SDL's private focus or forwards another HWND's keys.
//!
//! Measured on an Apple JIS keyboard: Eisu is VK_FINAL/scan F1 and Kana is
//! VK_IME_ON/scan F2. Their Raw Input packets carry break/E1 flags even on press.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::c_void;

use sdl3_sys::everything::*;

#[link(name = "user32")]
unsafe extern "system" {
    fn GetFocus() -> *mut c_void;
    fn GetForegroundWindow() -> *mut c_void;
    fn GetAncestor(window: *mut c_void, flags: u32) -> *mut c_void;
    fn GetWindowLongPtrW(window: *mut c_void, index: i32) -> isize;
    fn IsWindowVisible(window: *mut c_void) -> i32;
    fn IsWindowEnabled(window: *mut c_void) -> i32;
    fn IsChild(parent: *mut c_void, child: *mut c_void) -> i32;
}

type Subclass = unsafe extern "system" fn(*mut c_void, u32, usize, isize, usize, usize) -> isize;
#[link(name = "comctl32")]
unsafe extern "system" {
    fn SetWindowSubclass(
        window: *mut c_void,
        callback: Option<Subclass>,
        id: usize,
        data: usize,
    ) -> i32;
    fn RemoveWindowSubclass(window: *mut c_void, callback: Option<Subclass>, id: usize) -> i32;
    fn DefSubclassProc(window: *mut c_void, message: u32, wparam: usize, lparam: isize) -> isize;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Owner {
    window: u32,
    generation: u64,
}

#[derive(Default)]
struct NativeKeyboard {
    windows: Vec<usize>,
    focused: Option<u32>,
    generation: u64,
    message_owner: Option<Owner>,
    change: Option<(Option<u32>, Option<u32>)>,
    resetting: bool,
    keys: VecDeque<(SDL_KeyboardEvent, Option<Owner>)>,
}

impl NativeKeyboard {
    fn observe(&mut self, focused: Option<u32>) -> bool {
        if self.focused == focused {
            return false;
        }
        let old = self.focused;
        self.focused = focused;
        self.generation = self
            .generation
            .checked_add(1)
            .unwrap_or_else(|| panic!("native keyboard generation exhausted"));
        self.message_owner = None;
        let original = self.change.map_or(old, |(original, _)| original);
        self.change = Some((original, focused));
        self.resetting = old.is_some();
        old.is_some()
    }

    fn owner(&self) -> Option<Owner> {
        self.focused.map(|window| Owner {
            window,
            generation: self.generation,
        })
    }

    fn record(&mut self, key: SDL_KeyboardEvent) {
        if key.windowID.0 == 0 || self.resetting || self.message_owner.is_some() {
            let owner = if self.resetting {
                None
            } else {
                self.message_owner
                    .filter(|owner| key.windowID.0 == 0 || key.windowID.0 == owner.window)
            };
            if self.keys.len() == 65536 {
                self.keys.pop_front();
            }
            self.keys.push_back((key, owner));
        }
    }

    fn route(&mut self, key: &SDL_KeyboardEvent, embedded: bool) -> Option<u32> {
        if let Some(index) = self.keys.iter().position(|(saved, _)| same_key(saved, key)) {
            let (_, owner) = self.keys.remove(index)?;
            return owner
                .filter(|owner| Some(*owner) == self.owner())
                .map(|owner| owner.window);
        }
        (!embedded && key.windowID.0 != 0).then_some(key.windowID.0)
    }
}

thread_local! {
    static KEYBOARD: RefCell<NativeKeyboard> = RefCell::new(NativeKeyboard::default());
}

/// Tracks only the SDL windows created by this library, on its UI thread.
pub(crate) fn track(window: *mut SDL_Window, add: bool) {
    let native = hwnd(window);
    // SAFETY: only the owning SDL thread subclasses its own HWND. No keyboard
    // message is replaced; the callback observes focus and delegates to SDL.
    unsafe {
        if add && SetWindowSubclass(native, Some(focus_message), 1, window as usize) == 0 {
            log::error!(target: "reims_vgpu_window", "cannot observe native embedded focus");
            return;
        }
        if !add {
            RemoveWindowSubclass(native, Some(focus_message), 1);
        }
    }
    KEYBOARD.with_borrow_mut(|state| {
        state.windows.retain(|&w| w != window as usize);
        if add {
            state.windows.push(window as usize);
        }
    });
}

unsafe extern "system" fn focus_message(
    window: *mut c_void,
    message: u32,
    wparam: usize,
    lparam: isize,
    _: usize,
    data: usize,
) -> isize {
    if message == 0x0008 {
        // WM_KILLFOCUS is sent, not necessarily returned by SDL's message pump.
        let id = unsafe { SDL_GetWindowID(data as *mut SDL_Window).0 };
        if KEYBOARD.with_borrow(|state| state.focused == Some(id)) {
            change_focus(None);
        }
    } else if message == 0x0007 {
        observe_focus();
    }
    // SAFETY: this is the registered subclass callback for this owned window.
    unsafe { DefSubclassProc(window, message, wparam, lparam) }
}

fn hwnd(window: *mut SDL_Window) -> *mut c_void {
    // SAFETY: an owned, live SDL window on SDL's UI thread.
    unsafe {
        SDL_GetPointerProperty(
            SDL_GetWindowProperties(window),
            SDL_PROP_WINDOW_WIN32_HWND_POINTER,
            std::ptr::null_mut(),
        )
    }
}

/// None for a top-level window; an embedded child uses real Win32 focus.
pub(crate) fn embedded_focus(window: *mut SDL_Window) -> Option<bool> {
    let window = hwnd(window);
    // SAFETY: these Win32 queries do not change focus or window state.
    unsafe {
        if window.is_null() || GetWindowLongPtrW(window, -16) & 0x4000_0000 == 0 {
            return None;
        }
        let focus = GetFocus();
        let root = GetAncestor(window, 2);
        Some(
            !root.is_null()
                && root != window
                && root == GetForegroundWindow()
                && IsWindowVisible(window) != 0
                && IsWindowEnabled(window) != 0
                && (focus == window || IsChild(window, focus) != 0),
        )
    }
}

/// Effective Root focus; SDL's grab and relative-mode checks still use SDL focus.
pub(crate) fn keyboard_focus(window: *mut SDL_Window) -> bool {
    embedded_focus(window).unwrap_or_else(|| {
        // SAFETY: SDL's UI thread; top-level routing stays SDL's.
        unsafe { SDL_GetKeyboardFocus() == window }
    })
}

fn native_window() -> Option<*mut SDL_Window> {
    let windows = KEYBOARD.with_borrow(|state| state.windows.clone());
    let mut focused = windows
        .into_iter()
        .map(|w| w as *mut SDL_Window)
        .filter(|&w| embedded_focus(w) == Some(true));
    let result = focused.next();
    if focused.next().is_some() {
        None
    } else {
        result
    }
}

fn observe_focus() {
    let focused = native_window().map(|w| {
        // SAFETY: owned live SDL window on its UI thread.
        unsafe { SDL_GetWindowID(w).0 }
    });
    change_focus(focused);
}

fn change_focus(focused: Option<u32>) {
    let reset = KEYBOARD.with_borrow_mut(|state| state.observe(focused));
    if reset {
        // SAFETY: public SDL API, before the next native key is translated. Its
        // generated ups are tagged by our event watch, not routed to a new owner.
        unsafe { SDL_ResetKeyboard() };
        KEYBOARD.with_borrow_mut(|state| state.resetting = false);
    }
}

/// Called after SDL pumps messages, before routing its returned event, and on timeout.
pub(crate) fn sync_focus() {
    if let Some((old, new)) = take_focus_change() {
        crate::sdl::shell::native_focus_changed(old, new);
    }
}

fn take_focus_change() -> Option<(Option<u32>, Option<u32>)> {
    observe_focus();
    KEYBOARD.with_borrow_mut(|state| state.change.take())
}

unsafe extern "C" fn key_watch(_: *mut c_void, event: *mut SDL_Event) -> bool {
    // SAFETY: SDL passes a live event to the watch; read only its tagged key variant.
    let event = unsafe { &*event };
    let kind = unsafe { event.r#type };
    if kind != SDL_EVENT_KEY_DOWN.0 && kind != SDL_EVENT_KEY_UP.0 {
        return true;
    }
    let key = unsafe { event.key };
    // SDL's keyboard and our native hook run on the UI thread. Events pushed by
    // another thread acquire no native owner and remain undeliverable when ID=0.
    KEYBOARD.with_borrow_mut(|state| state.record(key));
    true
}

fn same_key(a: &SDL_KeyboardEvent, b: &SDL_KeyboardEvent) -> bool {
    a.timestamp == b.timestamp
        && a.windowID == b.windowID
        && a.scancode == b.scancode
        && a.which == b.which
        && a.raw == b.raw
        && a.down == b.down
        && a.repeat == b.repeat
}

/// Keeps SDL's nonzero routes, but ID=0 requires the same actual native owner.
pub(crate) fn key_window(key: &SDL_KeyboardEvent) -> Option<u32> {
    // An embedded JIS event also needs its stamp. Overflow or a filtered SDL
    // event must never turn a missing stamp into an ordinary nonzero route.
    // SAFETY: public SDL window lookup on its owning video thread.
    let embedded = key.windowID.0 != 0
        && unsafe {
            let window = SDL_GetWindowFromID(key.windowID);
            !window.is_null() && embedded_focus(window).is_some()
        };
    KEYBOARD.with_borrow_mut(|state| state.route(key, embedded))
}

/// The documented prefix of Win32 MSG; the callback does not read its trailing fields.
#[repr(C)]
struct MessagePrefix {
    window: *mut c_void,
    message: u32,
    wparam: usize,
    lparam: isize,
}

fn language_usage(vkey: usize, scan: u8) -> Option<u16> {
    match (vkey, scan) {
        (0x19 | 0x1a, 0x71 | 0xf1) => Some(0x91),
        (0x15 | 0x16, 0x72 | 0xf2) => Some(0x90),
        _ => None,
    }
}

unsafe extern "C" fn message_hook(_: *mut c_void, message: *mut MSG) -> bool {
    // SAFETY: SDL passes a mutable MSG on its UI thread; its documented prefix fits here.
    let Some(message) = (unsafe { message.cast::<MessagePrefix>().as_ref() }) else {
        return true;
    };
    KEYBOARD.with_borrow_mut(|state| state.message_owner = None);
    if !matches!(message.message, 0x100 | 0x101 | 0x104 | 0x105) {
        return true;
    }
    observe_focus();
    let native = native_window();
    let embedded_message = KEYBOARD.with_borrow(|state| {
        state.windows.iter().copied().any(|window| {
            let window = window as *mut SDL_Window;
            hwnd(window) == message.window && embedded_focus(window).is_some()
        })
    });
    if embedded_message && !native.is_some_and(|window| hwnd(window) == message.window) {
        // Do not let an old, unfocused HWND's queued key corrupt SDL's global key state.
        return false;
    }
    KEYBOARD.with_borrow_mut(|state| {
        if native.is_some_and(|window| hwnd(window) == message.window) {
            state.message_owner = state.owner();
        }
    });
    let scan = ((message.lparam as usize >> 16) & 0xff) as u8;
    let Some(usage) = language_usage(message.wparam, scan) else {
        return true;
    };
    // SAFETY: SDL video is initialized and this callback runs on its UI thread.
    let focus = native_window().unwrap_or_else(|| unsafe { SDL_GetKeyboardFocus() });
    if focus.is_null() {
        return true;
    }
    // SAFETY: focus is a live SDL window. Only that window's key messages are consumed.
    let window = unsafe {
        SDL_GetPointerProperty(
            SDL_GetWindowProperties(focus),
            SDL_PROP_WINDOW_WIN32_HWND_POINTER,
            std::ptr::null_mut(),
        )
    };
    if window != message.window {
        return true;
    }
    let down = matches!(message.message, 0x100 | 0x104);
    // SAFETY: initialize the keyboard variant and its tag together. SDL copies it.
    unsafe {
        let mut event = SDL_Event {
            key: SDL_KeyboardEvent {
                r#type: if down {
                    SDL_EVENT_KEY_DOWN
                } else {
                    SDL_EVENT_KEY_UP
                },
                timestamp: SDL_GetTicksNS(),
                windowID: SDL_GetWindowID(focus),
                scancode: SDL_Scancode::new(i32::from(usage)),
                down,
                repeat: down && (message.lparam as usize & (1 << 30)) != 0,
                raw: u16::from(scan),
                ..Default::default()
            },
        };
        SDL_PushEvent(&mut event);
    }
    false
}

pub(super) fn install_message_hook() {
    // SAFETY: called after SDL_Init on the UI thread; the callback has static lifetime.
    unsafe {
        SDL_SetWindowsMessageHook(Some(message_hook), std::ptr::null_mut());
        if !SDL_AddEventWatch(Some(key_watch), std::ptr::null_mut()) {
            log::error!(target: "reims_vgpu_window", "native embedded key ownership watch: {}", crate::sdl::sdl_error());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_messages_preserve_jis_and_leave_other_keys_alone() {
        assert_eq!(language_usage(0x1a, 0xf1), Some(0x91));
        assert_eq!(language_usage(0x16, 0xf2), Some(0x90));
        assert_eq!(language_usage(0x19, 0x71), Some(0x91));
        assert_eq!(language_usage(0x15, 0x72), Some(0x90));
        assert_eq!(language_usage(0x41, 0x1e), None);
        assert_eq!(language_usage(0x41, 0xf1), None);
        assert_eq!(language_usage(0x1a, 0x1e), None);
        assert_eq!(language_usage(0x5b, 0x5b), None);
    }

    fn key(timestamp: u64, window: u32, down: bool) -> SDL_KeyboardEvent {
        SDL_KeyboardEvent {
            timestamp,
            windowID: SDL_WindowID(window),
            scancode: SDL_SCANCODE_A,
            raw: 0x1e,
            down,
            ..Default::default()
        }
    }

    #[test]
    fn embedded_key_owner_is_bounded_and_never_follows_a_later_focus() {
        let mut state = NativeKeyboard::default();
        state.observe(Some(1));
        state.message_owner = state.owner();
        let old = key(1, 0, true);
        state.record(old);
        assert_eq!(state.route(&key(2, 0, true), true), None);
        state.observe(Some(2));
        assert_eq!(state.route(&old, true), None);
        let reset = key(3, 2, false);
        state.record(reset);
        state.resetting = false;
        state.message_owner = state.owner();
        assert_eq!(state.route(&reset, true), None);
        let current = key(4, 0, true);
        state.record(current);
        assert_eq!(state.route(&current, true), Some(2));
        assert_eq!(state.route(&key(5, 3, true), false), Some(3));
        state.observe(None);
        state.resetting = false;
        let background = key(6, 0, true);
        state.record(background);
        assert_eq!(state.route(&background, true), None);
        for timestamp in 10..65550 {
            state.record(key(timestamp, 0, true));
        }
        assert_eq!(state.keys.len(), 65536);
        assert_eq!(state.route(&key(7, 1, true), true), None);
        assert_eq!(state.change, Some((None, None)));
    }

    #[test]
    fn embedded_generation_rejects_old_keys_after_leave_and_reentry() {
        let mut state = NativeKeyboard::default();
        state.observe(Some(1));
        state.message_owner = state.owner();
        let old = key(1, 0, true);
        let jis = key(2, 1, true);
        state.record(old);
        state.record(jis);
        state.observe(None);
        state.resetting = false;
        state.observe(Some(1));
        assert_eq!(state.route(&old, true), None);
        assert_eq!(state.route(&jis, true), None);
        state.message_owner = None;
        let wrong_native_target = key(3, 0, true);
        state.record(wrong_native_target);
        assert_eq!(state.route(&wrong_native_target, true), None);
    }
}

#[cfg(test)]
#[path = "windows/native_keyboard_test.rs"]
mod native_keyboard_test;
