//! Scoped-input spike: an extra `wl_seat` ("agent-0") whose virtual-pointer input is
//! delivered to one pinned window.
//!
//! The whole feature is inert unless `NIRI_AGENT_PIN_APP_ID` is set: with no app id there is
//! nothing to pin, so every function here bails out before touching any state and niri behaves
//! exactly like upstream. Pointer events coming from the extra seat never go through
//! `State::process_input_event`, so seat0's focus, cursor and keyboard routing are untouched.

use std::env;

use smithay::backend::input::{
    AbsolutePositionEvent, Event, PointerButtonEvent, PointerMotionEvent,
};
use smithay::input::pointer::{ButtonEvent, MotionEvent};
use smithay::input::Seat;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Size, SERIAL_COUNTER};

use crate::niri::State;
use crate::protocols::virtual_pointer::{
    VirtualPointerAxisEvent, VirtualPointerButtonEvent, VirtualPointerInputBackend,
    VirtualPointerMotionAbsoluteEvent, VirtualPointerMotionEvent,
};
use crate::utils::with_toplevel_role;
use crate::window::mapped::MappedId;

/// Name of the extra seat. Clients create their virtual pointer on the `wl_seat` with this name.
pub const SEAT_NAME: &str = "agent-0";

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

    /// Reconciles the pin with the windows currently in the layout, given as `(id, app_id)`
    /// pairs, and returns whether the pin changed.
    ///
    /// The pin is made once and kept as long as the window lives, so a second window with the
    /// same app id does not steal it. A closed window drops the pin, after which the next
    /// matching window gets pinned.
    pub fn reconcile(&mut self, windows: &[(MappedId, Option<String>)]) -> bool {
        let Some(want) = self.app_id.as_deref() else {
            return false;
        };

        let mut candidate = None;
        let mut pinned_alive = false;
        for (id, app_id) in windows {
            if Some(*id) == self.pinned {
                pinned_alive = true;
            }
            if candidate.is_none() && app_id.as_deref() == Some(want) {
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
                (
                    mapped.id(),
                    with_toplevel_role(mapped.toplevel(), |role| role.app_id.clone()),
                )
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

        let surface = mapped.toplevel().wl_surface().clone();
        // Set by the layout, falling back to the surface's current size, which is what we want
        // to scale absolute pointer coordinates into.
        let size = mapped.window.geometry().size;

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

    #[test]
    fn inert_without_env_var() {
        let mut pin = AgentPin::default();
        assert!(!pin.is_enabled());

        // Even a window that would match is left alone, and nothing is reported as changed.
        let id = MappedId::next();
        assert!(!pin.reconcile(&[(id, app_id("foot"))]));
        assert_eq!(pin.pinned(), None);
    }

    #[test]
    fn no_matching_window_is_not_pinned() {
        let mut pin = AgentPin::with_app_id(app_id("foot"));
        assert!(pin.is_enabled());

        assert!(!pin.reconcile(&[(MappedId::next(), app_id("kitty"))]));
        assert_eq!(pin.pinned(), None);

        // Windows without an app id never match either.
        assert!(!pin.reconcile(&[(MappedId::next(), None)]));
        assert_eq!(pin.pinned(), None);

        assert!(!pin.reconcile(&[]));
        assert_eq!(pin.pinned(), None);
    }

    #[test]
    fn pins_matching_window_once() {
        let mut pin = AgentPin::with_app_id(app_id("foot"));
        let target = MappedId::next();

        let windows = [
            (MappedId::next(), app_id("kitty")),
            (target, app_id("foot")),
        ];
        assert!(pin.reconcile(&windows));
        assert_eq!(pin.pinned(), Some(target));

        // Already pinned: subsequent reconciles are no-ops, so focus is only set once.
        assert!(!pin.reconcile(&windows));

        // A second window with the same app id does not steal the pin.
        let other = MappedId::next();
        assert!(!pin.reconcile(&[(target, app_id("foot")), (other, app_id("foot"))]));
        assert_eq!(pin.pinned(), Some(target));
    }

    #[test]
    fn clears_pin_when_window_closes() {
        let mut pin = AgentPin::with_app_id(app_id("foot"));
        let target = MappedId::next();
        assert!(pin.reconcile(&[(target, app_id("foot"))]));
        assert_eq!(pin.pinned(), Some(target));

        // Window closed: the pin is dropped.
        assert!(pin.reconcile(&[]));
        assert_eq!(pin.pinned(), None);

        // Repeated reconciles of the empty layout do not report changes anymore.
        assert!(!pin.reconcile(&[]));

        // A window that appears afterwards gets pinned again.
        let new = MappedId::next();
        assert!(pin.reconcile(&[(new, app_id("foot"))]));
        assert_eq!(pin.pinned(), Some(new));
    }
}
