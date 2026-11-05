//! Scoped-input spike: an extra `wl_seat` ("agent-0") whose virtual input is delivered to one
//! pinned window.
//!
//! Pointer events come from `zwlr_virtual_pointer_v1` and are routed by hand into the agent
//! seat's own pointer handle. Keyboard events need no code here: a `zwp_virtual_keyboard_v1`
//! created against the agent seat is delivered by smithay straight to that seat's keyboard
//! focus, which is the pinned window (see `State::agent_pin_target`).
//!
//! The whole feature is inert unless `NIRI_AGENT_PIN_APP_ID` is set: with no app id there is
//! nothing to pin, so every function here bails out before touching any state and niri behaves
//! exactly like upstream. Events coming from the extra seat never go through
//! `State::process_input_event`, so seat0's focus, cursor and key binds are untouched.

use std::env;

use smithay::backend::input::{
    AbsolutePositionEvent, Event, PointerButtonEvent, PointerMotionEvent,
};
use smithay::input::pointer::{ButtonEvent, MotionEvent};
use smithay::input::Seat;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Size, SERIAL_COUNTER};

use crate::layout::LayoutElement as _;
use crate::niri::State;
use crate::protocols::virtual_pointer::{
    VirtualPointerAxisEvent, VirtualPointerButtonEvent, VirtualPointerInputBackend,
    VirtualPointerMotionAbsoluteEvent, VirtualPointerMotionEvent,
};
use crate::utils::with_toplevel_role;
use crate::window::mapped::MappedId;

/// Name of the extra seat. Clients create their virtual pointer on the `wl_seat` with this name.
pub const SEAT_NAME: &str = "agent-0";

/// What the pin needs to know about one window currently in the layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinCandidate {
    /// X11/XWayland window. It has no xdg role, hence no surface to deliver input to, and
    /// `Mapped::toplevel()` panics on it: it is never a candidate.
    X11,
    /// Wayland window, with the app id it currently reports (which may be unset).
    Wayland(Option<String>),
}

impl PinCandidate {
    /// App id to match against, or `None` when this window cannot be pinned at all.
    fn app_id(&self) -> Option<&str> {
        match self {
            Self::X11 => None,
            Self::Wayland(app_id) => app_id.as_deref(),
        }
    }
}

/// Pin bookkeeping for the agent seat.
///
/// Deliberately free of compositor state so that the state machine can be unit-tested: it only
/// maps "app id to look for" plus "windows currently in the layout" to "window to pin".
#[derive(Debug, Default)]
pub struct AgentPin {
    /// App id to pin, from `NIRI_AGENT_PIN_APP_ID`. `None` disables the whole feature.
    app_id: Option<String>,
    /// Window the agent seat's pointer is currently delivered to.
    pinned: Option<MappedId>,
}

impl AgentPin {
    /// Reads the app id to pin from the environment.
    pub fn from_env() -> Self {
        Self::with_app_id(env::var("NIRI_AGENT_PIN_APP_ID").ok())
    }

    pub fn with_app_id(app_id: Option<String>) -> Self {
        Self {
            app_id,
            pinned: None,
        }
    }

    /// Whether the feature is enabled at all.
    pub fn is_enabled(&self) -> bool {
        self.app_id.is_some()
    }

    pub fn pinned(&self) -> Option<MappedId> {
        self.pinned
    }

    /// Reconciles the pin with the windows currently in the layout, given as `(id, candidate)`
    /// pairs, and returns whether the pin changed.
    ///
    /// The pin is made once and kept as long as the window lives, so a second window with the
    /// same app id does not steal it. A closed window drops the pin, after which the next
    /// matching window gets pinned.
    pub fn reconcile(&mut self, windows: &[(MappedId, PinCandidate)]) -> bool {
        let Some(want) = self.app_id.as_deref() else {
            return false;
        };

        let mut candidate = None;
        let mut pinned_alive = false;
        for (id, window) in windows {
            if Some(*id) == self.pinned {
                pinned_alive = true;
            }
            if candidate.is_none() && window.app_id() == Some(want) {
                candidate = Some(*id);
            }
        }

        let pinned = if pinned_alive { self.pinned } else { candidate };
        let changed = pinned != self.pinned;
        self.pinned = pinned;
        changed
    }
}

impl State {
    /// Whether `seat` is the extra agent seat.
    ///
    /// Virtual pointers created without a seat (`seat() == None`) keep going through the normal
    /// seat0 path.
    fn is_agent_seat(&self, seat: Option<&WlSeat>) -> bool {
        // Go through the resource's user data rather than `Seat::owns()`: this compares the seat
        // itself, so it stays correct regardless of which client bound the global.
        seat.and_then(Seat::<State>::from_resource)
            .is_some_and(|seat| seat.name() == SEAT_NAME)
    }

    /// Establishes the pin when a window maps, so keyboard input works before any pointer
    /// event arrives.
    ///
    /// Keyboard events reach the pinned window through the agent seat's keyboard focus, which
    /// only this path sets. Without a map-time call, a window that is typed into but never
    /// pointed at would silently receive nothing.
    pub fn refresh_agent_pin(&mut self) {
        let _ = self.agent_pin_target();
    }

    /// Reconciles the pin against the current layout and resolves it to the pinned window's
    /// surface plus its logical size.
    ///
    /// Runs on every agent-seat pointer event, so the pin is established on first use and
    /// dropped as soon as the window leaves the layout.
    ///
    // ponytail: full layout scan per event; cache surface + size if this ever shows up in a
    // profile.
    fn agent_pin_target(&mut self) -> Option<(WlSurface, Size<i32, Logical>)> {
        if !self.niri.agent_pin.is_enabled() {
            return None;
        }

        let windows = self
            .niri
            .layout
            .windows()
            .map(|(_, mapped)| {
                // Ask the smithay window rather than `Mapped::toplevel()`: the latter panics on
                // X11 windows, which have no xdg role and can never be a target.
                let candidate = match mapped.window.toplevel() {
                    Some(toplevel) => PinCandidate::Wayland(with_toplevel_role(toplevel, |role| {
                        role.app_id.clone()
                    })),
                    None => PinCandidate::X11,
                };
                (mapped.id(), candidate)
            })
            .collect::<Vec<_>>();

        let pin_changed = self.niri.agent_pin.reconcile(&windows);
        let id = self.niri.agent_pin.pinned()?;

        let mapped = self
            .niri
            .layout
            .windows()
            .find(|(_, mapped)| mapped.id() == id)?
            .1;

        // Bounce on X11 windows instead of panicking: the candidate filter above keeps them out
        // of the pin, this is only the belt to that pair of braces.
        let surface = mapped.window.toplevel()?.wl_surface().clone();
        // Scale absolute pointer coordinates into the layout size, not the toplevel geometry:
        // a client that never sets a window geometry reports zero there, which collapsed every
        // absolute position onto the surface origin.
        let mut size = mapped.size();
        if size.w <= 0 || size.h <= 0 {
            size = mapped.window.geometry().size;
        }

        // Focus the pinned window on the agent seat once, so the client knows it is the one this
        // seat targets. The spike never sends key events through either seat's new path.
        if pin_changed {
            if let Some(keyboard) = self.niri.agent_seat.get_keyboard() {
                let serial = SERIAL_COUNTER.next_serial();
                keyboard.set_focus(self, Some(surface.clone()), serial);
            }
        }

        Some((surface, size))
    }

    /// Delivers a virtual-pointer motion event from the agent seat to the pinned window.
    ///
    /// Returns `false` if the event did not come from the agent seat, in which case the caller
    /// must fall through to the normal seat0 path.
    pub fn agent_seat_pointer_motion(&mut self, event: &VirtualPointerMotionEvent) -> bool {
        if !self.is_agent_seat(event.pointer().seat()) {
            return false;
        }
        let Some((surface, _)) = self.agent_pin_target() else {
            return true;
        };
        let Some(pointer) = self.niri.agent_seat.get_pointer() else {
            return true;
        };

        // Relative motion has no absolute reference, so accumulate it on top of the last
        // position we sent. The coordinates are surface-local on purpose, not compositor-global.
        let last = pointer.current_location();
        let location = Point::from((last.x + event.delta_x(), last.y + event.delta_y()));

        pointer.motion(
            self,
            Some((surface, location)),
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: event_time_msec(event),
            },
        );
        pointer.frame(self);

        true
    }

    /// Delivers a virtual-pointer absolute motion event, scaled to the pinned window's size.
    pub fn agent_seat_pointer_motion_absolute(
        &mut self,
        event: &VirtualPointerMotionAbsoluteEvent,
    ) -> bool {
        if !self.is_agent_seat(event.pointer().seat()) {
            return false;
        }
        let Some((surface, size)) = self.agent_pin_target() else {
            return true;
        };
        let Some(pointer) = self.niri.agent_seat.get_pointer() else {
            return true;
        };

        // Maps the event's 0..=extent range onto the window's logical size.
        let location = Point::from((event.x_transformed(size.w), event.y_transformed(size.h)));

        // The focus tuple carries the surface's origin in global space and the motion
        // carries the pointer's global location; smithay subtracts one from the other.
        // This path never composes a global position, so it declares the surface at the
        // origin and passes the surface-local point as the location. Passing the point in
        // both places collapsed every position onto (0, 0).
        pointer.motion(
            self,
            Some((surface, Point::from((0.0, 0.0)))),
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: event_time_msec(event),
            },
        );
        pointer.frame(self);

        true
    }

    /// Delivers a virtual-pointer button event to the pinned window.
    pub fn agent_seat_pointer_button(&mut self, event: &VirtualPointerButtonEvent) -> bool {
        if !self.is_agent_seat(event.pointer().seat()) {
            return false;
        }
        if self.agent_pin_target().is_none() {
            return true;
        }
        let Some(pointer) = self.niri.agent_seat.get_pointer() else {
            return true;
        };

        pointer.button(
            self,
            &ButtonEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time: event_time_msec(event),
                button: event.button_code(),
                state: event.state(),
            },
        );
        pointer.frame(self);

        true
    }

    /// Delivers a virtual-pointer axis event to the pinned window.
    pub fn agent_seat_pointer_axis(&mut self, event: &VirtualPointerAxisEvent) -> bool {
        if !self.is_agent_seat(event.pointer().seat()) {
            return false;
        }
        if self.agent_pin_target().is_none() {
            return true;
        }
        let Some(pointer) = self.niri.agent_seat.get_pointer() else {
            return true;
        };

        // The vendored event already carries the client's accumulated axis frame, so it can go
        // straight into the agent seat's own pointer handle. Like any wayland axis event it only
        // reaches the client once the pointer is on the surface, i.e. after a motion event.
        pointer.axis(self, event.axis_frame());
        pointer.frame(self);

        true
    }
}

fn event_time_msec<E: Event<VirtualPointerInputBackend>>(event: &E) -> u32 {
    (event.time() / 1000) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_id(app_id: &str) -> Option<String> {
        Some(app_id.to_owned())
    }

    /// A Wayland window reporting this app id.
    fn wl(app_id: &str) -> PinCandidate {
        PinCandidate::Wayland(Some(app_id.to_owned()))
    }

    #[test]
    fn inert_without_env_var() {
        let mut pin = AgentPin::default();
        assert!(!pin.is_enabled());

        // Even a window that would match is left alone, and nothing is reported as changed.
        let id = MappedId::next();
        assert!(!pin.reconcile(&[(id, wl("foot"))]));
        assert_eq!(pin.pinned(), None);
    }

    #[test]
    fn no_matching_window_is_not_pinned() {
        let mut pin = AgentPin::with_app_id(app_id("foot"));
        assert!(pin.is_enabled());

        assert!(!pin.reconcile(&[(MappedId::next(), wl("kitty"))]));
        assert_eq!(pin.pinned(), None);

        // Windows without an app id never match either.
        assert!(!pin.reconcile(&[(MappedId::next(), PinCandidate::Wayland(None))]));
        assert_eq!(pin.pinned(), None);

        assert!(!pin.reconcile(&[]));
        assert_eq!(pin.pinned(), None);
    }

    #[test]
    fn pins_matching_window_once() {
        let mut pin = AgentPin::with_app_id(app_id("foot"));
        let target = MappedId::next();

        let windows = [(MappedId::next(), wl("kitty")), (target, wl("foot"))];
        assert!(pin.reconcile(&windows));
        assert_eq!(pin.pinned(), Some(target));

        // Already pinned: subsequent reconciles are no-ops, so focus is only set once.
        assert!(!pin.reconcile(&windows));

        // A second window with the same app id does not steal the pin.
        let other = MappedId::next();
        assert!(!pin.reconcile(&[(target, wl("foot")), (other, wl("foot"))]));
        assert_eq!(pin.pinned(), Some(target));
    }

    #[test]
    fn clears_pin_when_window_closes() {
        let mut pin = AgentPin::with_app_id(app_id("foot"));
        let target = MappedId::next();
        assert!(pin.reconcile(&[(target, wl("foot"))]));
        assert_eq!(pin.pinned(), Some(target));

        // Window closed: the pin is dropped.
        assert!(pin.reconcile(&[]));
        assert_eq!(pin.pinned(), None);

        // Repeated reconciles of the empty layout do not report changes anymore.
        assert!(!pin.reconcile(&[]));

        // A window that appears afterwards gets pinned again.
        let new = MappedId::next();
        assert!(pin.reconcile(&[(new, wl("foot"))]));
        assert_eq!(pin.pinned(), Some(new));
    }

    #[test]
    fn x11_windows_are_never_pinned() {
        let mut pin = AgentPin::with_app_id(app_id("foot"));
        let x11 = MappedId::next();

        // X11 windows have no xdg role, so they cannot match no matter what their class is.
        assert!(!pin.reconcile(&[(x11, PinCandidate::X11)]));
        assert_eq!(pin.pinned(), None);

        // Repeated reconciles stay no-ops: nothing was pinned behind our back.
        assert!(!pin.reconcile(&[(x11, PinCandidate::X11)]));
        assert_eq!(pin.pinned(), None);
    }

    #[test]
    fn x11_windows_do_not_hide_other_candidates() {
        let mut pin = AgentPin::with_app_id(app_id("foot"));
        let target = MappedId::next();

        // The X11 window comes first in the layout: it must not block the Wayland candidate.
        let layout = [
            (MappedId::next(), PinCandidate::X11),
            (target, wl("foot")),
            (MappedId::next(), wl("kitty")),
        ];
        assert!(pin.reconcile(&layout));
        assert_eq!(pin.pinned(), Some(target));

        // And it keeps winning after the pin has been established.
        assert!(!pin.reconcile(&layout));
        assert_eq!(pin.pinned(), Some(target));
    }
}
