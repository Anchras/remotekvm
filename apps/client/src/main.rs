// remotekvm-client
//
// v0 milestone (macOS-only path):
//   1. Connect to host (signaling via stdin/stdout SDP paste in v0).
//   2. Receive H.265 NALs from WebRTC video track.
//   3. Decode via VideoToolbox.
//   4. Render via wgpu to a winit window.
//   5. Capture local mouse/keyboard, serialize to InputEvent, send over `control` DataChannel.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,remotekvm_client=debug".into()),
        )
        .init();

    tracing::info!("remotekvm-client v0 — scaffold only, no decode/render yet");

    let pc = remotekvm_transport::new_peer_connection().await?;
    tracing::info!(
        signaling_state = ?pc.signaling_state(),
        "peer connection created"
    );

    // TODO(v0):
    //   - winit + wgpu window
    //   - VideoToolbox H.265 decode
    //   - winit input -> InputEvent over DataChannel

    Ok(())
}
