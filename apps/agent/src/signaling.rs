use anyhow::Result;
use remotekvm_protocol::{ChannelMessage, ControlMessage, PROTOCOL_VERSION};
use remotekvm_transport::PeerSession;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Connection request from the SaaS server.
#[derive(Debug, Clone)]
pub struct ConnectionRequest {
    pub session_id: String,
    pub offer: String,
}

#[derive(Debug, Clone)]
pub struct HostVideoConfig {
    pub display_width: u32,
    pub display_height: u32,
    pub refresh_hz: u32,
    pub bitrate_kbps: u32,
}

pub struct AgentControlState {
    bitrate_kbps: AtomicU32,
    keyframe_requested: AtomicBool,
}

impl AgentControlState {
    pub fn new(initial_bitrate_kbps: u32) -> Self {
        Self {
            bitrate_kbps: AtomicU32::new(initial_bitrate_kbps),
            keyframe_requested: AtomicBool::new(false),
        }
    }

    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps.load(Ordering::Acquire)
    }

    #[allow(dead_code)]
    pub fn take_keyframe_request(&self) -> bool {
        self.keyframe_requested.swap(false, Ordering::AcqRel)
    }

    fn set_bitrate_kbps(&self, bitrate_kbps: u32) {
        self.bitrate_kbps.store(bitrate_kbps, Ordering::Release);
    }

    fn request_keyframe(&self) {
        self.keyframe_requested.store(true, Ordering::Release);
    }
}

pub async fn handle_agent_control(
    session: &PeerSession,
    state: &AgentControlState,
    host: &HostVideoConfig,
    message: ControlMessage,
) -> Result<()> {
    match message {
        ControlMessage::Hello {
            protocol_version,
            client_name,
        } => {
            if protocol_version != PROTOCOL_VERSION {
                tracing::warn!(
                    protocol_version,
                    supported = PROTOCOL_VERSION,
                    client_name,
                    "client protocol version differs from agent"
                );
            } else {
                tracing::debug!(client_name, "client control channel hello received");
            }

            session
                .send(&ChannelMessage::Control(ControlMessage::HostInfo {
                    display_width: host.display_width,
                    display_height: host.display_height,
                    refresh_hz: host.refresh_hz,
                }))
                .await?;
            tracing::debug!(
                bitrate_kbps = host.bitrate_kbps,
                "sent host video info over control channel"
            );
        }
        ControlMessage::SetBitrateKbps(bitrate_kbps) => {
            if bitrate_kbps == 0 {
                tracing::warn!("ignoring zero bitrate control message");
            } else {
                state.set_bitrate_kbps(bitrate_kbps);
                tracing::info!(
                    bitrate_kbps = state.bitrate_kbps(),
                    "control channel updated target bitrate"
                );
            }
        }
        ControlMessage::RequestKeyframe => {
            state.request_keyframe();
            tracing::debug!("control channel requested keyframe");
        }
        ControlMessage::HostInfo {
            display_width,
            display_height,
            refresh_hz,
        } => {
            tracing::debug!(
                display_width,
                display_height,
                refresh_hz,
                "received peer HostInfo on agent control channel"
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_state_tracks_bitrate_and_keyframes() {
        let state = AgentControlState::new(20_000);
        assert_eq!(state.bitrate_kbps(), 20_000);
        assert!(!state.take_keyframe_request());

        state.set_bitrate_kbps(8_000);
        state.request_keyframe();

        assert_eq!(state.bitrate_kbps(), 8_000);
        assert!(state.take_keyframe_request());
        assert!(!state.take_keyframe_request());
    }
}
