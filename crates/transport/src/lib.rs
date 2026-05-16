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

use anyhow::Result;
use std::sync::Arc;
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;

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
