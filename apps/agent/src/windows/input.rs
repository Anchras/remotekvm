use remotekvm_protocol::{InputEvent, MouseButton};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY, WHEEL_DELTA,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// Windows input injection via SendInput.
pub fn inject(pos: (f64, f64), event: &InputEvent) {
    let result = match event {
        InputEvent::MouseMove { x, y } => {
            tracing::trace!(x, y, "inject mouse move");
            send_mouse_move(*x as f64, *y as f64)
        }
        InputEvent::MouseButton { button, pressed } => {
            tracing::trace!(?button, pressed, "inject mouse button");
            send_mouse_button(*button, *pressed)
        }
        InputEvent::MouseScroll { dx, dy } => {
            tracing::trace!(dx, dy, "inject mouse scroll");
            send_mouse_scroll(*dx, *dy)
        }
        InputEvent::KeyDown { hid_usage } => {
            tracing::trace!(hid_usage, "inject key down");
            send_key(*hid_usage, true)
        }
        InputEvent::KeyUp { hid_usage } => {
            tracing::trace!(hid_usage, "inject key up");
            send_key(*hid_usage, false)
        }
    };

    if let Err(error) = result {
        tracing::warn!(?event, %error, "failed to inject Windows input");
    } else if !matches!(event, InputEvent::MouseMove { .. }) {
        tracing::trace!(
            x = pos.0,
            y = pos.1,
            "input injected at last pointer position"
        );
    }
}

fn send_mouse_move(x: f64, y: f64) -> anyhow::Result<()> {
    let desktop = VirtualDesktop::current()?;
    let dx = desktop.to_absolute_x(x);
    let dy = desktop.to_absolute_y(y);
    send_input(&[mouse_input(
        dx,
        dy,
        0,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    )])
}

fn send_mouse_button(button: MouseButton, pressed: bool) -> anyhow::Result<()> {
    let (flags, mouse_data) = match (button, pressed) {
        (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
        (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (MouseButton::X1, true) => (MOUSEEVENTF_XDOWN, 0x0001),
        (MouseButton::X1, false) => (MOUSEEVENTF_XUP, 0x0001),
        (MouseButton::X2, true) => (MOUSEEVENTF_XDOWN, 0x0002),
        (MouseButton::X2, false) => (MOUSEEVENTF_XUP, 0x0002),
    };
    send_input(&[mouse_input(0, 0, mouse_data, flags)])
}

fn send_mouse_scroll(dx: f32, dy: f32) -> anyhow::Result<()> {
    let mut inputs = Vec::with_capacity(2);
    if dy.abs() >= f32::EPSILON {
        inputs.push(mouse_input(0, 0, wheel_delta(dy), MOUSEEVENTF_WHEEL));
    }
    if dx.abs() >= f32::EPSILON {
        inputs.push(mouse_input(0, 0, wheel_delta(dx), MOUSEEVENTF_HWHEEL));
    }

    if inputs.is_empty() {
        return Ok(());
    }
    send_input(&inputs)
}

fn send_key(hid_usage: u16, down: bool) -> anyhow::Result<()> {
    let Some(vk) = hid_to_vk(hid_usage) else {
        tracing::debug!(hid_usage, "no Windows virtual-key mapping for HID usage");
        return Ok(());
    };
    let flags = if down {
        KEYBD_EVENT_FLAGS(0)
    } else {
        KEYEVENTF_KEYUP
    };
    send_input(&[INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }])
}

fn send_input(inputs: &[INPUT]) -> anyhow::Result<()> {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "SendInput sent {sent} of {} event(s)",
            inputs.len()
        ))
    }
}

fn mouse_input(dx: i32, dy: i32, mouse_data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn wheel_delta(delta: f32) -> i32 {
    let scaled = (delta * WHEEL_DELTA as f32).round();
    scaled.clamp(i32::MIN as f32, i32::MAX as f32) as i32
}

struct VirtualDesktop {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl VirtualDesktop {
    fn current() -> anyhow::Result<Self> {
        let desktop = Self {
            x: unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
            y: unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
            width: unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) },
            height: unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) },
        };
        if desktop.width <= 1 || desktop.height <= 1 {
            anyhow::bail!(
                "invalid virtual desktop metrics: {}x{}",
                desktop.width,
                desktop.height
            );
        }
        Ok(desktop)
    }

    fn to_absolute_x(&self, x: f64) -> i32 {
        to_absolute_axis(x, self.x, self.width)
    }

    fn to_absolute_y(&self, y: f64) -> i32 {
        to_absolute_axis(y, self.y, self.height)
    }
}

fn to_absolute_axis(value: f64, origin: i32, span: i32) -> i32 {
    let local = (value - origin as f64).clamp(0.0, (span - 1) as f64);
    ((local * 65_535.0) / (span - 1) as f64).round() as i32
}

fn hid_to_vk(hid: u16) -> Option<VIRTUAL_KEY> {
    let vk = match hid {
        // a..z
        0x04..=0x1D => 0x41 + (hid - 0x04),
        // 1..9, 0
        0x1E..=0x26 => 0x31 + (hid - 0x1E),
        0x27 => 0x30,
        // Enter, Esc, Backspace, Tab, Space
        0x28 => 0x0D,
        0x29 => 0x1B,
        0x2A => 0x08,
        0x2B => 0x09,
        0x2C => 0x20,
        // Arrows
        0x4F => 0x27,
        0x50 => 0x25,
        0x51 => 0x28,
        0x52 => 0x26,
        // Modifiers
        0xE0 | 0xE4 => 0x11,
        0xE1 | 0xE5 => 0x10,
        0xE2 | 0xE6 => 0x12,
        0xE3 | 0xE7 => 0x5B,
        _ => return None,
    };
    Some(VIRTUAL_KEY(vk))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_basic_hid_usages_to_virtual_keys() {
        assert_eq!(hid_to_vk(0x04), Some(VIRTUAL_KEY(0x41)));
        assert_eq!(hid_to_vk(0x1D), Some(VIRTUAL_KEY(0x5A)));
        assert_eq!(hid_to_vk(0x27), Some(VIRTUAL_KEY(0x30)));
        assert_eq!(hid_to_vk(0x52), Some(VIRTUAL_KEY(0x26)));
        assert_eq!(hid_to_vk(0xFFFF), None);
    }

    #[test]
    fn absolute_axis_conversion_clamps_to_desktop() {
        assert_eq!(to_absolute_axis(-20.0, 0, 1920), 0);
        assert_eq!(to_absolute_axis(1919.0, 0, 1920), 65_535);
        assert_eq!(to_absolute_axis(2500.0, 0, 1920), 65_535);
    }
}
