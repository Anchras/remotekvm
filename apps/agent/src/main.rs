// remotekvm-agent
//
// Phase 1 (MVP): Windows service + session agent architecture.
//
// Architecture:
//   - Service Controller (Windows Session 0): persistent server connection, spawns session agents
//   - Session Agent (user session): capture, encode, WebRTC, input injection
//
// Platform support:
//   - Windows: primary target (DXGI, NVENC, WASAPI, SendInput)
//   - macOS: development fallback (ScreenCaptureKit, VideoToolbox)

use anyhow::Result;
use clap::Parser;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(all(target_os = "macos", feature = "macos_v0"))]
mod macos;

mod config;
mod server_client;
mod signaling;

#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Server WebSocket URL.
    #[arg(long, default_value = "ws://localhost:8080/agent")]
    server_url: String,
    /// Registration token for this machine.
    #[arg(long, env = "RKVM_REGISTRATION_TOKEN")]
    token: Option<String>,
    /// Run as a Windows service.
    #[arg(long)]
    service: bool,
    /// Capture width in pixels.
    #[arg(long, default_value_t = 1920)]
    width: u32,
    /// Capture height in pixels.
    #[arg(long, default_value_t = 1080)]
    height: u32,
    /// Capture framerate cap.
    #[arg(long, default_value_t = 60)]
    fps: u32,
    /// Target bitrate, kbps.
    #[arg(long, default_value_t = 20_000)]
    bitrate_kbps: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,remotekvm_agent=debug".into()),
        )
        .init();

    let args = Args::parse();

    #[cfg(target_os = "windows")]
    {
        if args.service {
            windows::service::run_service().await
        } else {
            run_standalone(args).await
        }
    }

    #[cfg(target_os = "macos")]
    {
        run_macos(args).await
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        anyhow::bail!("remotekvm-agent currently only supports Windows and macOS")
    }
}

/// Standalone agent mode (for development/testing).
/// Connects to the server and runs the full pipeline in-process.
#[cfg(target_os = "windows")]
async fn run_standalone(args: Args) -> Result<()> {
    use std::sync::Arc;
    use tokio::sync::mpsc;

    tracing::info!(
        server_url = %args.server_url,
        width = args.width,
        height = args.height,
        fps = args.fps,
        bitrate_kbps = args.bitrate_kbps,
        "agent starting in standalone mode"
    );

    // Connect to the SaaS server
    let token = args.token.ok_or_else(|| {
        anyhow::anyhow!("Registration token required. Provide via --token or RKVM_REGISTRATION_TOKEN")
    })?;

    let server_client = server_client::ServerClient::connect(&args.server_url, &token).await?;
    tracing::info!("connected to SaaS server");

    // Wait for connection requests from the server
    // When a client wants to connect, the server sends us a ConnectRequest via WebSocket
    let (connection_tx, mut connection_rx) = mpsc::unbounded_channel::<signaling::ConnectionRequest>();
    let server_client = Arc::new(server_client);

    // Spawn a task to handle incoming connection requests
    let server_clone = Arc::clone(&server_client);
    tokio::spawn(async move {
        if let Err(e) = server_clone.handle_messages(connection_tx).await {
            tracing::error!(error = %e, "server message handler failed");
        }
    });

    // Main loop: wait for connection requests and spawn sessions
    loop {
        let request = tokio::select! {
            req = connection_rx.recv() => req,
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Ctrl+C received, shutting down");
                break;
            }
        };

        let Some(request) = request else {
            tracing::warn!("server connection closed");
            break;
        };

        tracing::info!(session_id = %request.session_id, "connection request received");

        // Spawn a session agent to handle this connection
        let server_for_session = Arc::clone(&server_client);
        tokio::spawn(async move {
            if let Err(e) = run_session(server_for_session, request, &args).await {
                tracing::error!(error = %e, session_id = %request.session_id, "session failed");
            }
        });
    }

    Ok(())
}

/// Run a single remote session (capture + encode + WebRTC + input).
#[cfg(target_os = "windows")]
async fn run_session(
    server: Arc<server_client::ServerClient>,
    request: signaling::ConnectionRequest,
    args: &Args,
) -> Result<()> {
    use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
    use webrtc::api::APIBuilder;
    use webrtc::peer_connection::configuration::RTCConfiguration;
    use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
    use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
    use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

    tracing::info!(session_id = %request.session_id, "starting session");

    // Create WebRTC peer connection
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

    // Create video track
    let video_track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "screen".to_owned(),
    ));

    let _ = peer_connection
        .add_track(video_track.clone())
        .await;

    // Set remote description (the offer from the client)
    let offer = RTCSessionDescription::offer(request.offer)?;
    peer_connection.set_remote_description(offer).await?;

    // Create answer
    let answer = peer_connection.create_answer(None).await?;
    peer_connection.set_local_description(answer.clone()).await?;

    // Send answer back to client via server
    server.send_answer(&request.session_id, &answer.sdp).await?;

    // TODO: Set up capture pipeline and start streaming
    // TODO: Set up audio track (Opus)
    // TODO: Set up DataChannel for input events

    // Wait for session to end
    tokio::signal::ctrl_c().await?;

    peer_connection.close().await?;
    tracing::info!(session_id = %request.session_id, "session ended");
    Ok(())
}

#[cfg(target_os = "macos")]
async fn run_macos(_args: Args) -> Result<()> {
    tracing::info!("macOS agent mode: using legacy capture pipeline for development");
    // For now, macOS agent just runs the v0 capture pipeline
    // TODO: Adapt macOS code to new agent architecture (Phase 3)
    anyhow::bail!("macOS agent not yet adapted to SaaS architecture")
}
