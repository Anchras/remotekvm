use anyhow::Result;
use tracing::{error, info, warn};

/// Run the agent as a Windows service.
///
/// The service controller:
///   1. Connects to the SaaS server via WebSocket
///   2. Monitors active user sessions via WTS
///   3. Spawns a session agent when a user logs in
///   4. Handles machine registration and heartbeats
pub async fn run_service() -> Result<()> {
    info!("starting Windows service controller");

    // TODO: Implement Windows service scaffolding using windows-service crate
    // 1. Register service main handler
    // 2. Create service status handle
    // 3. Report SERVICE_RUNNING
    // 4. Start the main loop

    warn!("Windows service mode is a stub — running in foreground for development");

    // For now, just run the standalone mode in the foreground
    // This allows testing without installing the service
    run_foreground().await
}

/// Run the service controller in foreground mode (for development).
async fn run_foreground() -> Result<()> {
    info!("running service controller in foreground mode");

    let config = crate::config::Config::from_env()?;

    // Connect to the SaaS server
    let server = crate::server_client::ServerClient::connect(
        &config.server_url,
        &config.registration_token,
    ).await?;

    info!("service controller connected to server");

    // TODO: Monitor Windows Terminal Services (WTS) for session events
    // TODO: Spawn session agent processes for active sessions
    // TODO: Handle session logoff (kill agent process)

    // Placeholder: just wait for Ctrl+C
    tokio::signal::ctrl_c().await?;
    info!("shutting down service controller");

    Ok(())
}

// TODO: Implement WTS session monitoring
// fn monitor_sessions() -> ...

// TODO: Implement session agent spawning
// fn spawn_session_agent(session_id: u32) -> ...
