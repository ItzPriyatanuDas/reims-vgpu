//! The host's screens, which the OS's side lists (`native`): "screen i" is the i-th of them, the
//! main one first, and the window of the display on port i opens on screen i
//! ([`screen_for_port`]). On macOS they come from CoreGraphics, which answers on any thread; on
//! Linux from SDL, on the library's UI thread, which takes a snapshot of them
//! (`native/linux/ui.rs`). Where there are none (no window system to reach), a display matching
//! its host screen offers its chosen mode, or `policy::UNMATCHED_MODE`, at 60 Hz (`policy`).

use crate::env;
use crate::native;

/// A host screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HostScreen {
    /// Its display ID: CoreGraphics' on macOS, SDL's on Linux.
    pub id: u32,
    /// Its current mode's size in pixels, width then height.
    pub pixels: (u32, u32),
    /// Pixels per point.
    pub scale: f64,
    /// The highest refresh rate it runs at, in Hz.
    pub max_refresh_hz: f64,
}

/// The host's screens, the main one first. Empty where the OS lists none.
pub fn host_screens() -> Vec<HostScreen> {
    native::host_screens()
}

/// The index of the screen the display on `port` opens on, among `count` screens:
/// `list[port]` where the list names one that exists, else `port` where that exists, else 0,
/// the main screen.
pub fn pick_screen(port: u64, list: &[usize], count: usize) -> usize {
    let port = usize::try_from(port).ok();
    let listed = port.and_then(|port| list.get(port)).copied();
    listed
        .filter(|&index| index < count)
        .or(port.filter(|&port| port < count))
        .unwrap_or(0)
}

/// The screen the window of the display on `port` opens on: the one `REIMS_VGPU_WINDOW_SCREENS`
/// names for it, else screen `port`, else the main screen. `None` where there is no screen.
pub fn screen_for_port(port: u64) -> Option<HostScreen> {
    let screens = host_screens();
    let index = pick_screen(port, &env::settings().screens, screens.len());
    screens.get(index).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Display i opens on screen i when there is one
    #[test]
    fn display_i_opens_on_screen_i() {
        assert_eq!(pick_screen(0, &[], 3), 0);
        assert_eq!(pick_screen(1, &[], 3), 1);
        assert_eq!(pick_screen(2, &[], 3), 2);
    }

    /// Display i opens on the main screen when there is no screen i
    #[test]
    fn a_missing_screen_is_the_main_one() {
        assert_eq!(pick_screen(1, &[], 1), 0);
        assert_eq!(pick_screen(7, &[], 2), 0);
        assert_eq!(pick_screen(u64::MAX, &[], 2), 0);
        assert_eq!(pick_screen(0, &[], 0), 0);
    }

    /// The list names each display's screen; an entry naming no screen falls back to screen i,
    /// then the main one
    #[test]
    fn the_list_names_each_display_its_screen() {
        assert_eq!(pick_screen(0, &[1, 0], 2), 1);
        assert_eq!(pick_screen(1, &[1, 0], 2), 0);
        // The list is shorter than the ports: screen i.
        assert_eq!(pick_screen(1, &[1], 2), 1);
        // The entry names a screen that is not there: screen i, else the main one.
        assert_eq!(pick_screen(1, &[0, 5], 2), 1);
        assert_eq!(pick_screen(2, &[0, 0, 5], 2), 0);
    }
}
