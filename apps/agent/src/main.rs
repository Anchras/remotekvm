// remotekvm-agent
//
// Connects to the SaaS coordination server, accepts client connection requests,
// and establishes a WebRTC session per connection. The control DataChannel
// carries input events (client → host), which are injected into the OS.
//
// Platform support:
//   - macOS: signaling + control channel + input injection work today; the
//     ScreenCaptureKit/VideoToolbox capture→video-track wiring is the next step.
//   - Windows: capture/encode/audio/service are Phase 1 (still stubbed).

use anyhow::Result;
use clap::Parser;
use std::sync::Arc;

use remotekvm_protocol::ChannelMessage;
use remotekvm_transport::PeerSession;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(all(target_os = "macos", feature = "macos_v0"))]
mod macos;

mod input;
mod server_client;
mod signaling;

#[derive(Parser, Debug, Clone)]
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
            return windows::service::run_service().await;
        }
    }

    run_standalone(args).await
}

/// Connect to the server and run a session per inbound connection request.
async fn run_standalone(args: Args) -> Result<()> {
    tracing::info!(
        server_url = %args.server_url,
        width = args.width,
        height = args.height,
        fps = args.fps,
        bitrate_kbps = args.bitrate_kbps,
        "agent starting"
    );

    let token = args.token.clone().ok_or_else(|| {
        anyhow::anyhow!("Registration token required (--token or RKVM_REGISTRATION_TOKEN)")
    })?;

    let (server, mut requests) =
        server_client::ServerClient::connect(&args.server_url, &token).await?;
    let server = Arc::new(server);
    tracing::info!("connected to SaaS server");

    loop {
        let request = tokio::select! {
            req = requests.recv() => req,
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
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            let session_id = request.session_id.clone();
            if let Err(e) = run_session(server, request).await {
                tracing::error!(error = %e, %session_id, "session failed");
            }
        });
    }

    Ok(())
}

/// Establish a WebRTC session for one connection request and inject the input
/// events that arrive over the control channel.
async fn run_session(
    server: Arc<server_client::ServerClient>,
    request: signaling::ConnectionRequest,
) -> Result<()> {
    tracing::info!(session_id = %request.session_id, "starting session");

    let session = PeerSession::answerer().await?;
    session.accept_offer(&request.offer).await?;

    // Non-trickle: the answer embeds our ICE candidates, matching the offer the
    // client sends. (The server relay also supports trickle ICE messages.)
    let answer = session.wait_ice_gathering().await?;
    server.send_answer(&request.session_id, &answer)?;

    // TODO (Phase 1/3): create a video track from the platform capture+encode
    // pipeline (DXGI/NVENC on Windows, ScreenCaptureKit/VideoToolbox on macOS)
    // and add it to `session.peer_connection()` before answering.

    let injector = input::InputInjector::new();
    while let Some(msg) = session.recv().await {
        match msg {
            ChannelMessage::Input(event) => injector.inject(&event),
            ChannelMessage::Control(ctrl) => {
                tracing::debug!(?ctrl, "control message received")
            }
        }
    }

    session.close().await?;
    tracing::info!(session_id = %request.session_id, "session ended");
    Ok(())
}
