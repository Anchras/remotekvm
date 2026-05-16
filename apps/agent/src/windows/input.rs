use remotekvm_protocol::InputEvent;

/// Windows input injection via SendInput.
pub struct InputInjector;

impl InputInjector {
    pub fn new() -> Self {
        InputInjector
    }

    pub fn inject(&self, event: &InputEvent) {
        match event {
            InputEvent::MouseMove { x, y } => {
                tracing::trace!(x, y, "inject mouse move");
                // TODO: Convert normalized coordinates (0..1) to absolute (0..65535)
                // TODO: Call SendInput with INPUT_MOUSE, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE
            }
            InputEvent::MouseButton { button, pressed } => {
                tracing::trace!(?button, pressed, "inject mouse button");
                // TODO: Map MouseButton to MOUSEEVENTF_LEFTDOWN/UP, etc.
                // TODO: Call SendInput
            }
            InputEvent::MouseScroll { dx, dy } => {
                tracing::trace!(dx, dy, "inject mouse scroll");
                // TODO: SendInput with MOUSEEVENTF_WHEEL, WHEEL_DELTA * dy
            }
            InputEvent::KeyDown { hid_usage } => {
                tracing::trace!(hid_usage, "inject key down");
                // TODO: Map HID usage to VK code, SendInput with INPUT_KEYBOARD, keydown
            }
            InputEvent::KeyUp { hid_usage } => {
                tracing::trace!(hid_usage, "inject key up");
                // TODO: Map HID usage to VK code, SendInput with INPUT_KEYBOARD, keyup
            }
        }
    }
}
