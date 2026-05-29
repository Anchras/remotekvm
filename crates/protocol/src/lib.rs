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

/// A single message on the bidirectional `control` DataChannel.
///
/// Multiplexes input events (client → host) and control messages (either
/// direction) over one channel so the transport layer has a single wire type.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ChannelMessage {
    Input(InputEvent),
    Control(ControlMessage),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_input(ev: InputEvent) {
        let bytes = encode(&ev).expect("encode");
        let back: InputEvent = decode(&bytes).expect("decode");
        // InputEvent has no PartialEq; compare via re-encoding (stable for bincode).
        let bytes2 = encode(&back).expect("re-encode");
        assert_eq!(
            bytes, bytes2,
            "round-trip changed the wire bytes for {ev:?}"
        );
    }

    #[test]
    fn input_events_round_trip() {
        roundtrip_input(InputEvent::MouseMove { x: 12.5, y: -3.0 });
        roundtrip_input(InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        });
        roundtrip_input(InputEvent::MouseButton {
            button: MouseButton::X2,
            pressed: false,
        });
        roundtrip_input(InputEvent::MouseScroll { dx: 0.0, dy: 4.0 });
        roundtrip_input(InputEvent::KeyDown { hid_usage: 0x04 });
        roundtrip_input(InputEvent::KeyUp { hid_usage: 0xFFFF });
    }

    #[test]
    fn control_messages_round_trip() {
        for msg in [
            ControlMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                client_name: "client-α".to_string(),
            },
            ControlMessage::SetBitrateKbps(8_000),
            ControlMessage::RequestKeyframe,
            ControlMessage::HostInfo {
                display_width: 2560,
                display_height: 1440,
                refresh_hz: 120,
            },
        ] {
            let bytes = encode(&msg).expect("encode");
            let back: ControlMessage = decode(&bytes).expect("decode");
            assert_eq!(encode(&back).unwrap(), bytes);
        }
    }

    #[test]
    fn decode_rejects_garbage() {
        // A single byte cannot be a valid enum discriminant + payload.
        let err = decode::<ControlMessage>(&[0xFF]);
        assert!(err.is_err(), "garbage bytes should fail to decode");
    }

    #[test]
    fn mouse_button_variants_are_distinct_on_the_wire() {
        let l = encode(&MouseButton::Left).unwrap();
        let r = encode(&MouseButton::Right).unwrap();
        assert_ne!(l, r);
    }
}
