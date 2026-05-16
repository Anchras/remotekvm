use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 0;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum InputEvent {
    MouseMove { x: f32, y: f32 },
    MouseButton { button: MouseButton, pressed: bool },
    MouseScroll { dx: f32, dy: f32 },
    KeyDown { hid_usage: u16 },
    KeyUp { hid_usage: u16 },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ControlMessage {
    Hello {
        protocol_version: u32,
        client_name: String,
    },
    SetBitrateKbps(u32),
    RequestKeyframe,
    HostInfo {
        display_width: u32,
        display_height: u32,
        refresh_hz: u32,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("bincode: {0}")]
    Bincode(#[from] bincode::Error),
}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, WireError> {
    Ok(bincode::serialize(value)?)
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, WireError> {
    Ok(bincode::deserialize(bytes)?)
}
