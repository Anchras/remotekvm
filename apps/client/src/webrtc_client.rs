use anyhow::Result;
use std::sync::Arc;
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

/// WebRTC client for peer connections to remote machines.
pub struct WebRtcClient {
    peer_connection: Arc<RTCPeerConnection>,
}

impl WebRtcClient {
    pub async fn new() -> Result<Self> {
        let mut media_engine = MediaEngine::default();
        media_engine.register_default_codecs()?;

        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .build();

        let config = RTCConfiguration {
            ice_servers: vec![],
            ..Default::default()
        };

        let peer_connection = Arc::new(api.new_peer_connection(config).await?);

        Ok(Self { peer_connection })
    }

    /// Create an offer and return the SDP.
    pub async fn create_offer(&self) -> Result<String> {
        let offer = self.peer_connection.create_offer(None).await?;
        self.peer_connection.set_local_description(offer.clone()).await?;
        Ok(offer.sdp)
    }

    /// Set the remote description (answer from the agent).
    pub async fn set_answer(&self, answer_sdp: &str) -> Result<()> {
        let answer = webrtc::peer_connection::sdp::session_description::RTCSessionDescription::answer(answer_sdp.to_string())?;
        self.peer_connection.set_remote_description(answer).await?;
        Ok(())
    }

    /// Add an ICE candidate.
    pub async fn add_ice_candidate(&self, candidate: &str) -> Result<()> {
        let candidate_init = webrtc::ice_transport::ice_candidate::RTCIceCandidateInit {
            candidate: candidate.to_string(),
            sdp_mid: None,
            sdp_mline_index: None,
            username_fragment: None,
        };
        self.peer_connection.add_ice_candidate(candidate_init).await?;
        Ok(())
    }

    /// Close the peer connection.
    pub async fn close(&self) -> Result<()> {
        self.peer_connection.close().await?;
        Ok(())
    }
}
