//! Platform input injection for received [`InputEvent`]s.
//!
//! macOS uses CoreGraphics event posting (`core-graphics`). Windows uses
//! `SendInput`. Other platforms currently log the event. Injection requires OS
//! permission at runtime (macOS: Accessibility).

use remotekvm_protocol::InputEvent;
use std::sync::Mutex;

pub struct InputInjector {
    /// Last known absolute cursor position; CoreGraphics button/scroll events
    /// carry a location, so we track moves to place subsequent clicks.
    last_pos: Mutex<(f64, f64)>,
}

impl Default for InputInjector {
    fn default() -> Self {
        Self::new()
    }
}

impl InputInjector {
    pub fn new() -> Self {
        Self {
            last_pos: Mutex::new((0.0, 0.0)),
        }
    }

    /// Inject one input event. Must be called between `.await` points (the
    /// CoreGraphics handles are created and dropped within the call and are not
    /// held across awaits, so `InputInjector` stays `Send`).
    pub fn inject(&self, event: &InputEvent) {
        if let InputEvent::MouseMove { x, y } = event {
            *self.last_pos.lock().unwrap() = (*x as f64, *y as f64);
        }

        #[cfg(target_os = "macos")]
        macos::inject(self.last_pos.lock().unwrap().to_owned(), event);

        #[cfg(target_os = "windows")]
        crate::windows::input::inject(self.last_pos.lock().unwrap().to_owned(), event);

        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        tracing::debug!(?event, "input injection not implemented on this platform");
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use core_graphics::event::{
        CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, ScrollEventUnit,
    };
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;
    use remotekvm_protocol::{InputEvent, MouseButton};

    pub fn inject(pos: (f64, f64), event: &InputEvent) {
        let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
            tracing::warn!("failed to create CGEventSource (Accessibility permission?)");
            return;
        };
        let point = CGPoint::new(pos.0, pos.1);

        match event {
            InputEvent::MouseMove { .. } => {
                post_mouse(&source, CGEventType::MouseMoved, point, CGMouseButton::Left);
            }
            InputEvent::MouseButton { button, pressed } => {
                let (etype, cg_btn) = mouse_button_event(*button, *pressed);
                post_mouse(&source, etype, point, cg_btn);
            }
            InputEvent::MouseScroll { dx, dy } => {
                if let Ok(ev) = CGEvent::new_scroll_event(
                    source,
                    ScrollEventUnit::PIXEL,
                    2,
                    *dy as i32,
                    *dx as i32,
                    0,
                ) {
                    ev.post(CGEventTapLocation::HID);
                }
            }
            InputEvent::KeyDown { hid_usage } => post_key(&source, *hid_usage, true),
            InputEvent::KeyUp { hid_usage } => post_key(&source, *hid_usage, false),
        }
    }

    fn post_mouse(
        source: &CGEventSource,
        etype: CGEventType,
        point: CGPoint,
        button: CGMouseButton,
    ) {
        if let Ok(ev) = CGEvent::new_mouse_event(source.clone(), etype, point, button) {
            ev.post(CGEventTapLocation::HID);
        }
    }

    fn mouse_button_event(button: MouseButton, pressed: bool) -> (CGEventType, CGMouseButton) {
        match button {
            MouseButton::Left => (
                if pressed {
                    CGEventType::LeftMouseDown
                } else {
                    CGEventType::LeftMouseUp
                },
                CGMouseButton::Left,
            ),
            MouseButton::Right => (
                if pressed {
                    CGEventType::RightMouseDown
                } else {
                    CGEventType::RightMouseUp
                },
                CGMouseButton::Right,
            ),
            // Middle / X1 / X2 all map to "other".
            _ => (
                if pressed {
                    CGEventType::OtherMouseDown
                } else {
                    CGEventType::OtherMouseUp
                },
                CGMouseButton::Center,
            ),
        }
    }

    fn post_key(source: &CGEventSource, hid_usage: u16, down: bool) {
        match hid_to_macos_keycode(hid_usage) {
            Some(code) => {
                if let Ok(ev) = CGEvent::new_keyboard_event(source.clone(), code, down) {
                    ev.post(CGEventTapLocation::HID);
                }
            }
            None => tracing::debug!(hid_usage, "no macOS keycode mapping for HID usage"),
        }
    }

    /// Map USB HID Usage IDs (Usage Page 0x07, Keyboard) to macOS ANSI virtual
    /// key codes for the common set. Returns `None` for unmapped usages.
    fn hid_to_macos_keycode(hid: u16) -> Option<u16> {
        Some(match hid {
            // a..z
            0x04 => 0,
            0x05 => 11,
            0x06 => 8,
            0x07 => 2,
            0x08 => 14,
            0x09 => 3,
            0x0A => 5,
            0x0B => 4,
            0x0C => 34,
            0x0D => 38,
            0x0E => 40,
            0x0F => 37,
            0x10 => 46,
            0x11 => 45,
            0x12 => 31,
            0x13 => 35,
            0x14 => 12,
            0x15 => 15,
            0x16 => 1,
            0x17 => 17,
            0x18 => 32,
            0x19 => 9,
            0x1A => 13,
            0x1B => 7,
            0x1C => 16,
            0x1D => 6,
            // 1..9, 0
            0x1E => 18,
            0x1F => 19,
            0x20 => 20,
            0x21 => 21,
            0x22 => 23,
            0x23 => 22,
            0x24 => 26,
            0x25 => 28,
            0x26 => 25,
            0x27 => 29,
            // Enter, Esc, Backspace, Tab, Space
            0x28 => 36,
            0x29 => 53,
            0x2A => 51,
            0x2B => 48,
            0x2C => 49,
            // Arrows
            0x4F => 124,
            0x50 => 123,
            0x51 => 125,
            0x52 => 126,
            // Modifiers
            0xE0 => 59,
            0xE1 => 56,
            0xE2 => 58,
            0xE3 => 55,
            0xE4 => 62,
            0xE5 => 60,
            0xE6 => 61,
            0xE7 => 54,
            _ => return None,
        })
    }
}
