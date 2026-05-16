// remotekvm-host
//
// v0 dev driver: capture + encode only, dump raw HEVC annex-B to a file.
// Validate with: `ffplay -f hevc out.h265` (or `ffmpeg -i out.h265 out.mp4`).

use anyhow::Result;
use clap::Parser;

#[cfg(target_os = "macos")]
mod macos;

#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Output HEVC annex-B file.
    #[arg(short, long, default_value = "out.h265")]
    output: String,
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
    /// Run duration in seconds; 0 means until Ctrl+C.
    #[arg(long, default_value_t = 10)]
    seconds: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,remotekvm_host=debug".into()),
        )
        .init();

    let args = Args::parse();

    #[cfg(target_os = "macos")]
    {
        run_macos(args).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        anyhow::bail!("remotekvm-host currently only supports macOS (v0 milestone)");
    }
}

#[cfg(target_os = "macos")]
async fn run_macos(args: Args) -> Result<()> {
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::mpsc;

    let (tx, mut rx) = mpsc::unbounded_channel::<macos::EncodedFrame>();

    let encoder = Arc::new(macos::Encoder::new(
        macos::EncoderConfig {
            width: args.width,
            height: args.height,
            bitrate_kbps: args.bitrate_kbps,
        },
        tx,
    )?);
    tracing::info!(
        width = args.width,
        height = args.height,
        fps = args.fps,
        bitrate_kbps = args.bitrate_kbps,
        "encoder ready"
    );

    let capturer = macos::Capturer::start(
        macos::capture::CapturerConfig {
            width: args.width,
            height: args.height,
            fps: args.fps,
        },
        encoder.clone(),
    )
    .await?;
    tracing::info!(output = %args.output, "capture started; writing annex-B HEVC");

    let mut out_file = tokio::fs::File::create(&args.output).await?;
    let mut frame_count: u64 = 0;
    let mut byte_count: u64 = 0;

    let deadline = if args.seconds > 0 {
        Some(tokio::time::Instant::now() + std::time::Duration::from_secs(args.seconds))
    } else {
        None
    };

    loop {
        let frame = tokio::select! {
            f = rx.recv() => f,
            _ = tokio::signal::ctrl_c() => break,
            _ = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            } => break,
        };
        let Some(frame) = frame else { break };
        out_file.write_all(&frame.data).await?;
        frame_count += 1;
        byte_count += frame.data.len() as u64;
        if frame.is_keyframe {
            tracing::debug!(frame_count, bytes = frame.data.len(), "keyframe");
        }
    }

    out_file.flush().await?;
    capturer.stop().await?;
    drop(encoder);

    tracing::info!(
        frames = frame_count,
        bytes = byte_count,
        avg_bytes_per_frame = if frame_count > 0 { byte_count / frame_count } else { 0 },
        "capture stopped"
    );
    Ok(())
}
