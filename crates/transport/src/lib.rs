// WebRTC transport wrapper for RemoteKVM.
//
// v0 contract:
//   - One PeerConnection per session.
//   - One outbound video track (H.265 in v0, negotiated later) host -> client.
//   - One outbound audio track (Opus) host -> client.
//   - One bidirectional DataChannel `control` for InputEvent + ControlMessage.
//
// Signaling for v0 is out-of-band stdin/stdout SDP paste — see apps/host and apps/client.
// The SaaS coordination server replaces that in v1.

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use remotekvm_protocol::{decode, encode, ChannelMessage};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, Notify};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;

/// Build a PeerConnection with sane low-latency defaults.
///
/// No ICE servers configured: we require Tailscale, so peers reach each other directly
/// on tailnet IPs and host-candidate ICE is all we need.
pub async fn new_peer_connection() -> Result<Arc<RTCPeerConnection>> {
    let mut media_engine = MediaEngine::default();
    media_engine.register_default_codecs()?;

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media_engine)?;

    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build();

    let config = RTCConfiguration {
        ice_servers: vec![],
        ..Default::default()
    };

    Ok(Arc::new(api.new_peer_connection(config).await?))
}

const CONTROL_CHANNEL: &str = "control";

/// A WebRTC session with the bidirectional `control` DataChannel wired up.
///
/// Used by both ends:
/// - the **client** is the offerer ([`PeerSession::offerer`]) and creates the
///   `control` channel;
/// - the **agent** is the answerer ([`PeerSession::answerer`]) and adopts the
///   channel the client opens.
///
/// SDP and ICE candidates are produced here and shuttled by the caller over the
/// signaling server; inbound [`ChannelMessage`]s and locally-gathered ICE
/// candidates are surfaced through async `recv_*` methods.
pub struct PeerSession {
    pc: Arc<RTCPeerConnection>,
    control: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
    incoming: Mutex<mpsc::UnboundedReceiver<ChannelMessage>>,
    local_ice: Mutex<mpsc::UnboundedReceiver<String>>,
    channel_open: Arc<Notify>,
}

impl PeerSession {
    /// Create the offering side and open the `control` DataChannel.
    pub async fn offerer() -> Result<Self> {
        let pc = new_peer_connection().await?;
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let (ice_tx, ice_rx) = mpsc::unbounded_channel();
        let channel_open = Arc::new(Notify::new());

        wire_ice(&pc, ice_tx);
        wire_connection_state(&pc);

        pc.add_transceiver_from_kind(
            RTPCodecType::Video,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
            }),
        )
        .await?;

        let dc = pc.create_data_channel(CONTROL_CHANNEL, None).await?;
        wire_data_channel(&dc, incoming_tx, channel_open.clone());

        Ok(Self {
            pc,
            control: Arc::new(Mutex::new(Some(dc))),
            incoming: Mutex::new(incoming_rx),
            local_ice: Mutex::new(ice_rx),
            channel_open,
        })
    }

    /// Create the answering side; the `control` channel is adopted when the
    /// remote offerer opens it.
    pub async fn answerer() -> Result<Self> {
        let pc = new_peer_connection().await?;
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let (ice_tx, ice_rx) = mpsc::unbounded_channel();
        let channel_open = Arc::new(Notify::new());
        let control: Arc<Mutex<Option<Arc<RTCDataChannel>>>> = Arc::new(Mutex::new(None));

        wire_ice(&pc, ice_tx);
        wire_connection_state(&pc);

        let control_slot = control.clone();
        let open = channel_open.clone();
        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let control_slot = control_slot.clone();
            let incoming_tx = incoming_tx.clone();
            let open = open.clone();
            Box::pin(async move {
                if dc.label() == CONTROL_CHANNEL {
                    wire_data_channel(&dc, incoming_tx, open);
                    *control_slot.lock().await = Some(dc);
                }
            })
        }));

        Ok(Self {
            pc,
            control,
            incoming: Mutex::new(incoming_rx),
            local_ice: Mutex::new(ice_rx),
            channel_open,
        })
    }

    /// (Offerer) Create the SDP offer and set it as the local description.
    pub async fn create_offer(&self) -> Result<String> {
        let offer = self.pc.create_offer(None).await?;
        self.pc.set_local_description(offer.clone()).await?;
        Ok(offer.sdp)
    }

    /// (Answerer) Accept the remote offer and return the SDP answer.
    pub async fn accept_offer(&self, offer_sdp: &str) -> Result<String> {
        let offer = RTCSessionDescription::offer(offer_sdp.to_string())?;
        self.pc.set_remote_description(offer).await?;
        let answer = self.pc.create_answer(None).await?;
        self.pc.set_local_description(answer.clone()).await?;
        Ok(answer.sdp)
    }

    /// (Offerer) Apply the remote SDP answer.
    pub async fn set_answer(&self, answer_sdp: &str) -> Result<()> {
        let answer = RTCSessionDescription::answer(answer_sdp.to_string())?;
        self.pc.set_remote_description(answer).await?;
        Ok(())
    }

    /// Wait until ICE gathering finishes, then return the local SDP with all
    /// candidates embedded (non-trickle). Useful when the signaling path should
    /// carry a single self-contained offer/answer instead of separate ICE
    /// messages.
    pub async fn wait_ice_gathering(&self) -> Result<String> {
        let mut done = self.pc.gathering_complete_promise().await;
        let _ = done.recv().await;
        let desc = self
            .pc
            .local_description()
            .await
            .ok_or_else(|| anyhow!("no local description after ICE gathering"))?;
        Ok(desc.sdp)
    }

    /// Add a remote ICE candidate (from the signaling channel).
    pub async fn add_remote_ice(&self, candidate: String) -> Result<()> {
        self.pc
            .add_ice_candidate(RTCIceCandidateInit {
                candidate,
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    /// Await the next locally-gathered ICE candidate to forward to the peer.
    /// Returns `None` once gathering is complete and the channel is closed.
    pub async fn next_local_ice(&self) -> Option<String> {
        self.local_ice.lock().await.recv().await
    }

    /// Send a [`ChannelMessage`] over the control channel.
    pub async fn send(&self, msg: &ChannelMessage) -> Result<()> {
        let bytes = encode(msg).map_err(|e| anyhow!("encode channel message: {e}"))?;
        let guard = self.control.lock().await;
        let dc = guard
            .as_ref()
            .ok_or_else(|| anyhow!("control channel not established yet"))?;
        dc.send(&Bytes::from(bytes))
            .await
            .context("send on control channel")?;
        Ok(())
    }

    /// Await the next inbound [`ChannelMessage`] from the peer.
    pub async fn recv(&self) -> Option<ChannelMessage> {
        self.incoming.lock().await.recv().await
    }

    /// Wait until the control channel is open, or time out.
    ///
    /// Open is signalled via `Notify::notify_one`, which stores a permit, so
    /// this resolves immediately even if the channel opened before we awaited.
    pub async fn wait_channel_open(&self, timeout: Duration) -> Result<()> {
        tokio::time::timeout(timeout, self.channel_open.notified())
            .await
            .map_err(|_| anyhow!("timed out waiting for control channel to open"))
    }

    /// The underlying peer connection (for adding media tracks, etc.).
    pub fn peer_connection(&self) -> &Arc<RTCPeerConnection> {
        &self.pc
    }

    /// Close the session.
    pub async fn close(&self) -> Result<()> {
        self.pc.close().await?;
        Ok(())
    }
}

fn wire_ice(pc: &Arc<RTCPeerConnection>, ice_tx: mpsc::UnboundedSender<String>) {
    pc.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
        let ice_tx = ice_tx.clone();
        Box::pin(async move {
            if let Some(c) = candidate {
                if let Ok(init) = c.to_json() {
                    let _ = ice_tx.send(init.candidate);
                }
            }
        })
    }));
}

fn wire_connection_state(pc: &Arc<RTCPeerConnection>) {
    pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
        tracing::debug!(?state, "peer connection state changed");
        Box::pin(async {})
    }));
}

fn wire_data_channel(
    dc: &Arc<RTCDataChannel>,
    incoming_tx: mpsc::UnboundedSender<ChannelMessage>,
    channel_open: Arc<Notify>,
) {
    dc.on_open(Box::new(move || {
        channel_open.notify_one();
        Box::pin(async {})
    }));

    dc.on_message(Box::new(move |msg: DataChannelMessage| {
        let incoming_tx = incoming_tx.clone();
        Box::pin(async move {
            match decode::<ChannelMessage>(&msg.data) {
                Ok(m) => {
                    let _ = incoming_tx.send(m);
                }
                Err(e) => tracing::warn!(error = %e, "failed to decode channel message"),
            }
        })
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

    #[tokio::test]
    async fn builds_a_fresh_peer_connection() {
        let pc = new_peer_connection().await.expect("build peer connection");
        assert_eq!(pc.connection_state(), RTCPeerConnectionState::New);
        pc.close().await.expect("close");
    }

    #[tokio::test]
    async fn can_create_a_data_channel_and_offer() {
        let pc = new_peer_connection().await.expect("build peer connection");

        // The v0 contract calls for a `control` DataChannel; make sure the
        // configured engine supports creating one and producing an SDP offer.
        let _dc = pc
            .create_data_channel("control", None)
            .await
            .expect("create data channel");

        let offer = pc.create_offer(None).await.expect("create offer");
        assert!(
            offer.sdp.contains("m=application"),
            "offer should advertise the data channel media section"
        );

        pc.close().await.expect("close");
    }

    /// Two PeerSessions negotiate directly (trickle ICE over loopback) and a
    /// client→agent InputEvent round-trips over the control channel. No GPU or
    /// signaling server involved — this isolates the WebRTC/control plumbing.
    #[tokio::test]
    async fn sessions_round_trip_input_over_control_channel() {
        use remotekvm_protocol::{InputEvent, MouseButton};

        let client = Arc::new(PeerSession::offerer().await.unwrap());
        let agent = Arc::new(PeerSession::answerer().await.unwrap());

        let offer = client.create_offer().await.unwrap();
        let answer = agent.accept_offer(&offer).await.unwrap();
        client.set_answer(&answer).await.unwrap();

        // Trickle ICE in both directions until the connection establishes.
        {
            let (c, a) = (client.clone(), agent.clone());
            tokio::spawn(async move {
                while let Some(ice) = c.next_local_ice().await {
                    let _ = a.add_remote_ice(ice).await;
                }
            });
            let (c, a) = (client.clone(), agent.clone());
            tokio::spawn(async move {
                while let Some(ice) = a.next_local_ice().await {
                    let _ = c.add_remote_ice(ice).await;
                }
            });
        }

        client
            .wait_channel_open(Duration::from_secs(15))
            .await
            .expect("control channel should open");

        let sent = InputEvent::MouseButton {
            button: MouseButton::Right,
            pressed: true,
        };
        client.send(&ChannelMessage::Input(sent)).await.unwrap();

        let got = tokio::time::timeout(Duration::from_secs(5), agent.recv())
            .await
            .expect("recv timed out")
            .expect("channel closed");
        match got {
            ChannelMessage::Input(InputEvent::MouseButton { button, pressed }) => {
                assert_eq!(button, MouseButton::Right);
                assert!(pressed);
            }
            other => panic!("unexpected channel message: {other:?}"),
        }

        client.close().await.unwrap();
        agent.close().await.unwrap();
    }
}
