//! Host-only SDL cursor policy. No guest state or SDL calls: the UI thread applies the result.

/// Which window owns SDL's process-wide mouse focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Focus {
    This,
    Other,
    None,
}

/// What this window may do to SDL's process-wide cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Keep,
    Arrow,
    Guest,
    Hide,
}

/// One window's current host/input state and the guest cursor it would normally show.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Context {
    pub focus: Focus,
    pub owned: bool,
    pub closed: bool,
    pub over_image: bool,
    pub captured: bool,
    pub hide_host: bool,
    pub guest_visible: bool,
    pub has_glyph: bool,
}

impl Context {
    /// Never change another focused window's cursor. With no SDL focus, only the window that
    /// last applied it may restore it; later background callbacks do nothing.
    pub fn action(self) -> Action {
        match self.focus {
            Focus::Other => return Action::Keep,
            Focus::None => {
                return if self.owned {
                    Action::Arrow
                } else {
                    Action::Keep
                };
            }
            Focus::This => {}
        }
        if self.closed || !self.over_image || self.hide_host && !self.captured {
            Action::Arrow
        } else if self.hide_host || !self.guest_visible && self.has_glyph {
            Action::Hide
        } else {
            Action::Guest
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active() -> Context {
        Context {
            focus: Focus::This,
            owned: true,
            closed: false,
            over_image: true,
            captured: true,
            hide_host: true,
            guest_visible: true,
            has_glyph: true,
        }
    }

    #[test]
    fn host_cursor_policy_hides_only_the_owned_captured_image() {
        for over in [false, true] {
            for captured in [false, true] {
                assert_eq!(
                    Context {
                        over_image: over,
                        captured,
                        ..active()
                    }.action(),
                    if over && captured {
                        Action::Hide
                    } else {
                        Action::Arrow
                    }
                );
            }
        }
        assert_eq!(Context { closed: true, ..active() }.action(), Action::Arrow);
    }

    #[test]
    fn host_cursor_policy_never_overrides_another_window() {
        for hide in [false, true] {
            for visible in [false, true] {
                assert_eq!(
                    Context {
                        focus: Focus::Other,
                        hide_host: hide,
                        guest_visible: visible,
                        ..active()
                    }.action(),
                    Action::Keep
                );
            }
        }
        assert_eq!(
            Context { owned: false, hide_host: false, ..active() }.action(),
            Action::Guest
        );
        assert_eq!(
            Context { focus: Focus::Other, closed: true, ..active() }.action(),
            Action::Keep
        );
    }

    #[test]
    fn host_cursor_policy_restores_once_on_leave_and_preserves_default_guest_rules() {
        assert_eq!(Context { focus: Focus::None, ..active() }.action(), Action::Arrow);
        assert_eq!(
            Context { focus: Focus::None, owned: false, ..active() }.action(),
            Action::Keep
        );
        assert_eq!(
            Context { hide_host: false, guest_visible: false, ..active() }.action(),
            Action::Hide
        );
        assert_eq!(
            Context {
                hide_host: false,
                guest_visible: false,
                has_glyph: false,
                ..active()
            }.action(),
            Action::Guest
        );
        assert_eq!(
            Context {
                over_image: false,
                hide_host: false,
                guest_visible: false,
                ..active()
            }.action(),
            Action::Arrow
        );
    }
}
