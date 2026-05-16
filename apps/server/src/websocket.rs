use axum::{
    extract::{ws::{Message, WebSocket}, State, WebSocketUpgrade},
    response::Response,
};
use serde::{Deserialize, Serialize};
use serde_json;
use std::sync::Arc;
use std::collections::HashMap;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::auth::routes::AppState;
use crate::auth::validate_token;

/// Shared state for WebSocket connections.
#[derive(Default, Clone)]
pub struct SignalingState {
    /// Map of machine_id -> agent WebSocket sender
    agents: Arc<RwLock<HashMap<String, tokio::sync::mpsc::UnboundedSender<SignalingMessage>>>>,
    /// Map of session_id -> client WebSocket sender
    clients: Arc<RwLock<HashMap<String, tokio::sync::mpsc::UnboundedSender<SignalingMessage>>>>,
}

impl SignalingState {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum SignalingMessage {
    #[serde(rename = "connect_request")]
    ConnectRequest { session_id: String, client_id: String, offer: String },
    #[serde(rename = "connect_response")]
    ConnectResponse { session_id: String, status: String, answer: Option<String> },
    #[serde(rename = "signaling_offer")]
    SignalingOffer { session_id: String, offer: String },
    #[serde(rename = "signaling_answer")]
    SignalingAnswer { session_id: String, answer: String },
    #[serde(rename = "ice_candidate")]
    IceCandidate { session_id: String, candidate: String },
    #[serde(rename = "heartbeat")]
    Heartbeat,
    #[serde(rename = "hello")]
    Hello { machine_id: Option<String>, version: Option<String> },
    #[serde(rename = "machine_status")]
    MachineStatus { online: bool, tailscale_ip: Option<String> },
}

// --- Agent WebSocket ---

pub async fn agent_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_agent_socket(socket, state))
}

async fn handle_agent_socket(
    mut socket: WebSocket,
    state: Arc<AppState>,
) {
    info!("agent websocket connected");

    // First message must be the registration token
    let machine_id = match authenticate_agent(&mut socket, &state).await {
        Some(id) => id,
        None => {
            warn!("agent authentication failed");
            return;
        }
    };

    info!(machine_id = %machine_id, "agent authenticated");

    // Mark machine as online
    if let Err(e) = sqlx::query(
        "UPDATE machines SET online = true, last_seen = NOW() WHERE id = $1"
    )
    .bind(uuid::Uuid::parse_str(&machine_id).unwrap_or_default())
    .execute(state.db.pool())
    .await
    {
        error!(error = %e, "failed to mark machine as online");
    }

    // Create a channel for forwarding messages to this agent
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SignalingMessage>();
    {
        let mut agents = state.signaling.agents.write().await;
        agents.insert(machine_id.clone(), tx);
    }

    // Main loop: read from socket and from the forward channel
    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Err(e) = handle_agent_message(&text, &machine_id, &state).await {
                            warn!(error = %e, "failed to handle agent message");
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        info!("agent websocket closed");
                        break;
                    }
                    Some(Err(e)) => {
                        error!(error = %e, "agent websocket error");
                        break;
                    }
                    _ => {}
                }
            }
            msg = rx.recv() => {
                match msg {
                    Some(signaling_msg) => {
                        let json = serde_json::to_string(&signaling_msg).unwrap_or_default();
                        if let Err(e) = socket.send(Message::Text(json)).await {
                            error!(error = %e, "failed to send to agent");
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }

    // Cleanup: mark offline and remove from agents map
    {
        let mut agents = state.signaling.agents.write().await;
        agents.remove(&machine_id);
    }

    if let Err(e) = sqlx::query(
        "UPDATE machines SET online = false WHERE id = $1"
    )
    .bind(uuid::Uuid::parse_str(&machine_id).unwrap_or_default())
    .execute(state.db.pool())
    .await
    {
        error!(error = %e, "failed to mark machine as offline");
    }

    info!(machine_id = %machine_id, "agent disconnected");
}

async fn authenticate_agent(socket: &mut WebSocket, state: &AppState) -> Option<String> {
    // Wait for the first message (should be the registration token)
    let token_msg = tokio::time::timeout(
        tokio::time::Duration::from_secs(10),
        socket.recv()
    ).await.ok()??;

    let token = match token_msg {
        Ok(Message::Text(text)) => text.trim().to_string(),
        _ => return None,
    };

    // Hash the token and look it up in the database
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let token_hash = format!("{:x}", hasher.finalize());

    let result = sqlx::query_as::<_, MachineIdRow>(
        "SELECT id FROM machines WHERE registration_token_hash = $1"
    )
    .bind(&token_hash)
    .fetch_optional(state.db.pool())
    .await;

    match result {
        Ok(Some(row)) => Some(row.id.to_string()),
        _ => None,
    }
}

async fn handle_agent_message(
    text: &str,
    machine_id: &str,
    state: &AppState,
) -> anyhow::Result<()> {
    let msg: SignalingMessage = serde_json::from_str(text)?;

    match msg {
        SignalingMessage::SignalingAnswer { session_id, answer } => {
            // Forward answer to the client
            let clients = state.signaling.clients.read().await;
            if let Some(client_tx) = clients.get(&session_id) {
                let _ = client_tx.send(SignalingMessage::ConnectResponse {
                    session_id: session_id.clone(),
                    status: "accepted".to_string(),
                    answer: Some(answer),
                });
            }
        }
        SignalingMessage::IceCandidate { session_id, candidate } => {
            // Forward ICE candidate to the client
            let clients = state.signaling.clients.read().await;
            if let Some(client_tx) = clients.get(&session_id) {
                let _ = client_tx.send(SignalingMessage::IceCandidate { session_id, candidate });
            }
        }
        _ => {
            info!("agent message: {:?}", msg);
        }
    }

    Ok(())
}

// --- Client WebSocket ---

pub async fn client_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_client_socket(socket, state))
}

async fn handle_client_socket(
    mut socket: WebSocket,
    state: Arc<AppState>,
) {
    info!("client websocket connected");

    // First message must be the JWT token
    let user_id = match authenticate_client(&mut socket, &state).await {
        Some(id) => id,
        None => {
            warn!("client authentication failed");
            return;
        }
    };

    info!(user_id = %user_id, "client authenticated");

    // Create a channel for forwarding messages to this client
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SignalingMessage>();

    // Main loop
    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Err(e) = handle_client_message(
                            &text, &user_id, &state, &tx
                        ).await {
                            warn!(error = %e, "failed to handle client message");
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        info!("client websocket closed");
                        break;
                    }
                    Some(Err(e)) => {
                        error!(error = %e, "client websocket error");
                        break;
                    }
                    _ => {}
                }
            }
            msg = rx.recv() => {
                match msg {
                    Some(signaling_msg) => {
                        let json = serde_json::to_string(&signaling_msg).unwrap_or_default();
                        if let Err(e) = socket.send(Message::Text(json)).await {
                            error!(error = %e, "failed to send to client");
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }

    // Cleanup: remove from clients map
    {
        let mut clients = state.signaling.clients.write().await;
        clients.retain(|_, client_tx| !client_tx.is_closed());
    }

    info!(user_id = %user_id, "client disconnected");
}

async fn authenticate_client(socket: &mut WebSocket, state: &AppState) -> Option<String> {
    let token_msg = tokio::time::timeout(
        tokio::time::Duration::from_secs(10),
        socket.recv()
    ).await.ok()??;

    let token = match token_msg {
        Ok(Message::Text(text)) => text.trim().to_string(),
        _ => return None,
    };

    let claims = validate_token(&token, &state.config.jwt_secret).ok()?;
    Some(claims.sub)
}

async fn handle_client_message(
    text: &str,
    _user_id: &str,
    state: &AppState,
    client_tx: &tokio::sync::mpsc::UnboundedSender<SignalingMessage>,
) -> anyhow::Result<()> {
    let msg: SignalingMessage = serde_json::from_str(text)?;

    match msg {
        SignalingMessage::ConnectRequest { session_id, offer, .. } => {
            // Look up the machine from the session
            let machine = sqlx::query_as::<_, SessionMachineRow>(
                r#"
                SELECT m.id as machine_id
                FROM sessions s
                JOIN machines m ON s.machine_id = m.id
                WHERE s.id = $1
                "#
            )
            .bind(uuid::Uuid::parse_str(&session_id)?)
            .fetch_optional(state.db.pool())
            .await?;

            if let Some(machine) = machine {
                // Register client for this session
                {
                    let mut clients = state.signaling.clients.write().await;
                    clients.insert(session_id.clone(), client_tx.clone());
                }

                // Forward to agent
                let agents = state.signaling.agents.read().await;
                if let Some(agent_tx) = agents.get(&machine.machine_id.to_string()) {
                    let _ = agent_tx.send(SignalingMessage::ConnectRequest {
                        session_id: session_id.clone(),
                        client_id: _user_id.to_string(),
                        offer,
                    });
                }
            }
        }
        SignalingMessage::SignalingOffer { session_id, offer } => {
            // Forward offer to agent
            let machine = sqlx::query_as::<_, SessionMachineRow>(
                r#"
                SELECT m.id as machine_id
                FROM sessions s
                JOIN machines m ON s.machine_id = m.id
                WHERE s.id = $1
                "#
            )
            .bind(uuid::Uuid::parse_str(&session_id)?)
            .fetch_optional(state.db.pool())
            .await?;

            if let Some(machine) = machine {
                let agents = state.signaling.agents.read().await;
                if let Some(agent_tx) = agents.get(&machine.machine_id.to_string()) {
                    let _ = agent_tx.send(SignalingMessage::SignalingOffer { session_id, offer });
                }
            }
        }
        SignalingMessage::IceCandidate { session_id, candidate } => {
            // Forward ICE candidate to agent
            let machine = sqlx::query_as::<_, SessionMachineRow>(
                r#"
                SELECT m.id as machine_id
                FROM sessions s
                JOIN machines m ON s.machine_id = m.id
                WHERE s.id = $1
                "#
            )
            .bind(uuid::Uuid::parse_str(&session_id)?)
            .fetch_optional(state.db.pool())
            .await?;

            if let Some(machine) = machine {
                let agents = state.signaling.agents.read().await;
                if let Some(agent_tx) = agents.get(&machine.machine_id.to_string()) {
                    let _ = agent_tx.send(SignalingMessage::IceCandidate { session_id, candidate });
                }
            }
        }
        _ => {
            info!("client message: {:?}", msg);
        }
    }

    Ok(())
}

#[derive(sqlx::FromRow)]
struct MachineIdRow {
    id: uuid::Uuid,
}

#[derive(sqlx::FromRow)]
struct SessionMachineRow {
    machine_id: uuid::Uuid,
}
