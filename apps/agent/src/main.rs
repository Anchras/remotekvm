// remotekvm-agent
//
// Connects to the SaaS coordination server, accepts client connection requests,
// and establishes a WebRTC session per connection. The control DataChannel
// carries input events (client → host), which are injected into the OS.
//
// Platform support:
//   - macOS: signaling + control channel + input injection work today; with
//     `--features macos_v0`, ScreenCaptureKit/VideoToolbox publishes H.264
//     samples to an advertised WebRTC video track.
//   - Windows: service/controller scaffolding, WASAPI audio, and H.264 video
//     pipeline boundaries are in place.

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
    /// Fail a session when the platform video pipeline cannot start.
    #[arg(long, env = "RKVM_REQUIRE_VIDEO", default_value_t = false)]
    require_video: bool,
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
            let token = args.token.clone().ok_or_else(|| {
                anyhow::anyhow!("Registration token required (--token or RKVM_REGISTRATION_TOKEN)")
            })?;
            return windows::service::run_service(windows::service::ServiceConfig::new(
                args.server_url.clone(),
                token,
            ))
            .await;
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
        let args = args.clone();
        tokio::spawn(async move {
            let session_id = request.session_id.clone();
            if let Err(e) = run_session(server, request, args).await {
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
    args: Args,
) -> Result<()> {
    tracing::info!(session_id = %request.session_id, "starting session");

    let session = PeerSession::answerer().await?;
    let control_state = signaling::AgentControlState::new(args.bitrate_kbps);
    let host_video = signaling::HostVideoConfig {
        display_width: args.width,
        display_height: args.height,
        refresh_hz: args.fps,
        bitrate_kbps: args.bitrate_kbps,
    };
    let video_pipeline = start_platform_video(&session, &args).await?;
    #[cfg(not(any(target_os = "windows", all(target_os = "macos", feature = "macos_v0"))))]
    let _ = &video_pipeline;
    let audio_pipeline = start_platform_audio(&session, &args).await?;
    session.accept_offer(&request.offer).await?;

    // Non-trickle: the answer embeds our ICE candidates, matching the offer the
    // client sends. (The server relay also supports trickle ICE messages.)
    let answer = session.wait_ice_gathering().await?;
    server.send_answer(&request.session_id, &answer)?;

    let injector = input::InputInjector::new();

    while let Some(msg) = session.recv().await {
        match msg {
            ChannelMessage::Input(event) => injector.inject(&event),
            ChannelMessage::Control(ctrl) => {
                signaling::handle_agent_control(&session, &control_state, &host_video, ctrl)
                    .await?;
                #[cfg(target_os = "windows")]
                if let Some(video_pipeline) = &video_pipeline {
                    if let Err(error) = video_pipeline.apply_control_state(&control_state) {
                        tracing::warn!(%error, "failed to apply video control state");
                    }
                }
            }
        }
    }

    #[cfg(all(target_os = "macos", feature = "macos_v0"))]
    if let Some(video_pipeline) = video_pipeline {
        if let Err(e) = video_pipeline.stop().await {
            tracing::warn!(error = %e, "failed to stop macOS video pipeline cleanly");
        }
    }

    #[cfg(target_os = "windows")]
    if let Some(video_pipeline) = video_pipeline {
        video_pipeline.stop().await;
    }

    #[cfg(target_os = "windows")]
    if let Some(audio_pipeline) = audio_pipeline {
        audio_pipeline.stop().await;
    }
    #[cfg(not(target_os = "windows"))]
    let _ = audio_pipeline;

    session.close().await?;
    tracing::info!(session_id = %request.session_id, "session ended");
    Ok(())
}

#[cfg(all(target_os = "macos", feature = "macos_v0"))]
async fn start_platform_video(
    session: &PeerSession,
    args: &Args,
) -> Result<Option<macos::VideoPipeline>> {
    let config = macos::VideoConfig {
        width: args.width,
        height: args.height,
        fps: args.fps,
        bitrate_kbps: args.bitrate_kbps,
    };

    match macos::VideoPipeline::start(session.peer_connection().clone(), config).await {
        Ok(pipeline) => Ok(Some(pipeline)),
        Err(e) if args.require_video => Err(e),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "macOS video pipeline unavailable; continuing control-only"
            );
            Ok(None)
        }
    }
}

#[cfg(target_os = "windows")]
async fn start_platform_video(
    session: &PeerSession,
    args: &Args,
) -> Result<Option<windows::VideoPipeline>> {
    let config = windows::VideoConfig {
        width: args.width,
        height: args.height,
        fps: args.fps,
        bitrate_kbps: args.bitrate_kbps,
    };

    match windows::VideoPipeline::start(session.peer_connection().clone(), config).await {
        Ok(pipeline) => Ok(pipeline),
        Err(e) if args.require_video => Err(e),
        Err(error) => {
            tracing::warn!(%error, "Windows video pipeline unavailable; continuing without video");
            Ok(None)
        }
    }
}

#[cfg(not(any(target_os = "windows", all(target_os = "macos", feature = "macos_v0"))))]
async fn start_platform_video(_session: &PeerSession, args: &Args) -> Result<Option<()>> {
    if args.require_video {
        anyhow::bail!("video pipeline is not available in this build");
    }
    Ok(None)
}

#[cfg(target_os = "windows")]
async fn start_platform_audio(
    session: &PeerSession,
    _args: &Args,
) -> Result<Option<windows::audio::AudioPipeline>> {
    match windows::audio::AudioPipeline::start(session.peer_connection().clone(), 64).await {
        Ok(pipeline) => Ok(pipeline),
        Err(error) => {
            tracing::warn!(%error, "Windows audio pipeline unavailable; continuing without audio");
            Ok(None)
        }
    }
}

#[cfg(not(target_os = "windows"))]
async fn start_platform_audio(_session: &PeerSession, _args: &Args) -> Result<Option<()>> {
    Ok(None)
}
