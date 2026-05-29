use axum::{
    extract::{
        ws::{Message, WebSocket},
        Query, State, WebSocketUpgrade,
    },
    response::Response,
};
use serde::{Deserialize, Serialize};
use serde_json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::auth::validate_token;
use crate::error::ApiError;
use crate::state::AppState;

const WS_CHANNEL_SIZE: usize = 64;

/// A registered agent socket: its forward channel plus a unique connection id
/// so a reconnecting agent doesn't get torn down by the old socket's cleanup.
type AgentEntry = (u64, tokio::sync::mpsc::Sender<SignalingMessage>);

/// Shared state for WebSocket connections.
#[derive(Default, Clone)]
pub struct SignalingState {
    /// Map of machine_id -> (connection id, agent WebSocket sender)
    agents: Arc<RwLock<HashMap<String, AgentEntry>>>,
    /// Map of session_id -> client WebSocket sender
    clients: Arc<RwLock<HashMap<String, tokio::sync::mpsc::Sender<SignalingMessage>>>>,
    /// Monotonic source of per-connection ids.
    next_conn_id: Arc<std::sync::atomic::AtomicU64>,
}

impl SignalingState {
    pub fn new() -> Self {
        Self::default()
    }

    fn next_conn_id(&self) -> u64 {
        self.next_conn_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether an agent for `machine_id` currently holds a live signaling socket.
    ///
    /// This reflects the in-memory relay state, which is the real source of
    /// truth for "can we signal this machine right now" — the `machines.online`
    /// DB column can lag if an agent died without a clean WebSocket close.
    pub async fn is_agent_online(&self, machine_id: &str) -> bool {
        self.agents.read().await.contains_key(machine_id)
    }

    /// Push a message to a connected agent.
    ///
    /// Returns `false` if no agent is connected for `machine_id` or its forward
    /// channel is closed/full.
    pub async fn notify_agent(&self, machine_id: &str, msg: SignalingMessage) -> bool {
        let agents = self.agents.read().await;
        match agents.get(machine_id) {
            Some((_, tx)) => tx.send(msg).await.is_ok(),
            None => false,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum SignalingMessage {
    #[serde(rename = "connect_request")]
    ConnectRequest {
        session_id: String,
        client_id: String,
        offer: String,
    },
    #[serde(rename = "connect_response")]
    ConnectResponse {
        session_id: String,
        status: String,
        answer: Option<String>,
    },
    #[serde(rename = "signaling_offer")]
    SignalingOffer { session_id: String, offer: String },
    #[serde(rename = "signaling_answer")]
    SignalingAnswer { session_id: String, answer: String },
    #[serde(rename = "ice_candidate")]
    IceCandidate {
        session_id: String,
        candidate: String,
    },
    #[serde(rename = "heartbeat")]
    Heartbeat,
    #[serde(rename = "hello")]
    Hello {
        machine_id: Option<String>,
        version: Option<String>,
    },
    #[serde(rename = "machine_status")]
    MachineStatus {
        online: bool,
        tailscale_ip: Option<String>,
    },
}

// --- Agent WebSocket ---

#[derive(Debug, Deserialize)]
pub struct AgentTokenQuery {
    pub token: String,
}

pub async fn agent_ws_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<AgentTokenQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<Response, ApiError> {
    // Validate registration token *before* upgrading the connection
    let machine_id = validate_agent_token(&query.token, &state)
        .await
        .ok_or(ApiError::Unauthorized)?;

    Ok(ws.on_upgrade(move |socket| handle_agent_socket(socket, state, machine_id)))
}

async fn handle_agent_socket(mut socket: WebSocket, state: Arc<AppState>, machine_id: uuid::Uuid) {
    info!(machine_id = %machine_id, "agent websocket connected");

    // Mark machine as online
    if let Err(e) =
        sqlx::query("UPDATE machines SET online = true, last_seen = NOW() WHERE id = $1")
            .bind(machine_id)
            .execute(state.db.pool())
            .await
    {
        error!(error = %e, "failed to mark machine as online");
    }

    // Create a bounded channel for forwarding messages to this agent. The
    // connection id lets cleanup distinguish "my socket" from a newer one that
    // reconnected for the same machine while this one was tearing down.
    let conn_id = state.signaling.next_conn_id();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<SignalingMessage>(WS_CHANNEL_SIZE);
    {
        let mut agents = state.signaling.agents.write().await;
        agents.insert(machine_id.to_string(), (conn_id, tx));
    }

    // Main loop: read from socket and from the forward channel
    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Err(e) = handle_agent_message(&text, &machine_id.to_string(), &state).await {
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
                        let json = match serde_json::to_string(&signaling_msg) {
                            Ok(j) => j,
                            Err(e) => {
                                error!(error = %e, "failed to serialize signaling message");
                                continue;
                            }
                        };
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

    // Cleanup: only tear down the relay slot / mark offline if it's still *our*
    // connection. A newer socket may have reconnected for this machine while we
    // were shutting down; in that case leave its registration intact.
    let was_current = {
        let mut agents = state.signaling.agents.write().await;
        match agents.get(&machine_id.to_string()) {
            Some((id, _)) if *id == conn_id => {
                agents.remove(&machine_id.to_string());
                true
            }
            _ => false,
        }
    };

    if was_current {
        if let Err(e) = sqlx::query("UPDATE machines SET online = false WHERE id = $1")
            .bind(machine_id)
            .execute(state.db.pool())
            .await
        {
            error!(error = %e, "failed to mark machine as offline");
        }
    }

    info!(machine_id = %machine_id, superseded = !was_current, "agent disconnected");
}

async fn validate_agent_token(token: &str, state: &AppState) -> Option<uuid::Uuid> {
    let token_hash = crate::util::hash_token(token);

    let result = sqlx::query_as::<_, MachineIdRow>(
        "SELECT id FROM machines WHERE registration_token_hash = $1",
    )
    .bind(&token_hash)
    .fetch_optional(state.db.pool())
    .await;

    match result {
        Ok(Some(row)) => Some(row.id),
        _ => None,
    }
}

async fn handle_agent_message(
    text: &str,
    _machine_id: &str,
    state: &AppState,
) -> anyhow::Result<()> {
    let msg: SignalingMessage = serde_json::from_str(text)?;

    match msg {
        SignalingMessage::SignalingAnswer { session_id, answer } => {
            // Forward answer to the client
            let clients = state.signaling.clients.read().await;
            if let Some(client_tx) = clients.get(&session_id) {
                let _ = client_tx
                    .send(SignalingMessage::ConnectResponse {
                        session_id: session_id.clone(),
                        status: "accepted".to_string(),
                        answer: Some(answer),
                    })
                    .await;
            }
        }
        SignalingMessage::IceCandidate {
            session_id,
            candidate,
        } => {
            // Forward ICE candidate to the client
            let clients = state.signaling.clients.read().await;
            if let Some(client_tx) = clients.get(&session_id) {
                let _ = client_tx
                    .send(SignalingMessage::IceCandidate {
                        session_id,
                        candidate,
                    })
                    .await;
            }
        }
        SignalingMessage::Heartbeat => {
            // Heartbeat received, agent is alive
            info!("agent heartbeat");
        }
        _ => {
            info!("agent message: {:?}", msg);
        }
    }

    Ok(())
}

// --- Client WebSocket ---

#[derive(Debug, Deserialize)]
pub struct ClientTokenQuery {
    pub token: String,
}

pub async fn client_ws_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<ClientTokenQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<Response, ApiError> {
    // Validate JWT *before* upgrading the connection
    let claims = validate_token(&query.token, &state.config.jwt_secret)
        .map_err(|_| ApiError::Unauthorized)?;

    Ok(ws.on_upgrade(move |socket| handle_client_socket(socket, state, claims.sub)))
}

async fn handle_client_socket(mut socket: WebSocket, state: Arc<AppState>, user_id: String) {
    info!(user_id = %user_id, "client websocket connected");

    // Create a bounded channel for forwarding messages to this client
    let (tx, mut rx) = tokio::sync::mpsc::channel::<SignalingMessage>(WS_CHANNEL_SIZE);

    // Track which sessions this client owns for cleanup
    let mut owned_sessions: Vec<String> = Vec::new();

    // Main loop
    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Err(e) = handle_client_message(
                            &text, &user_id, &state, &tx, &mut owned_sessions
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
                        let json = match serde_json::to_string(&signaling_msg) {
                            Ok(j) => j,
                            Err(e) => {
                                error!(error = %e, "failed to serialize signaling message");
                                continue;
                            }
                        };
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

    // Cleanup: remove only this client's owned sessions
    {
        let mut clients = state.signaling.clients.write().await;
        for session_id in owned_sessions {
            clients.remove(&session_id);
        }
    }

    info!(user_id = %user_id, "client disconnected");
}

async fn handle_client_message(
    text: &str,
    user_id: &str,
    state: &AppState,
    client_tx: &tokio::sync::mpsc::Sender<SignalingMessage>,
    owned_sessions: &mut Vec<String>,
) -> anyhow::Result<()> {
    let msg: SignalingMessage = serde_json::from_str(text)?;

    match msg {
        SignalingMessage::ConnectRequest {
            session_id, offer, ..
        } => {
            // Verify session ownership before relaying
            let session = sqlx::query_as::<_, SessionOwnerRow>(
                "SELECT user_id, machine_id FROM sessions WHERE id = $1",
            )
            .bind(uuid::Uuid::parse_str(&session_id)?)
            .fetch_optional(state.db.pool())
            .await?;

            if let Some(session) = session {
                if session.user_id.to_string() != user_id {
                    warn!(session_id = %session_id, "session ownership mismatch");
                    return Ok(());
                }

                // Register client for this session
                {
                    let mut clients = state.signaling.clients.write().await;
                    clients.insert(session_id.clone(), client_tx.clone());
                    owned_sessions.push(session_id.clone());
                }

                // Forward to agent
                let agents = state.signaling.agents.read().await;
                if let Some((_, agent_tx)) = agents.get(&session.machine_id.to_string()) {
                    let _ = agent_tx
                        .send(SignalingMessage::ConnectRequest {
                            session_id: session_id.clone(),
                            client_id: user_id.to_string(),
                            offer,
                        })
                        .await;
                }
            }
        }
        SignalingMessage::SignalingOffer { session_id, offer } => {
            // Verify session ownership
            let session = sqlx::query_as::<_, SessionOwnerRow>(
                "SELECT user_id, machine_id FROM sessions WHERE id = $1",
            )
            .bind(uuid::Uuid::parse_str(&session_id)?)
            .fetch_optional(state.db.pool())
            .await?;

            if let Some(session) = session {
                if session.user_id.to_string() != user_id {
                    warn!(session_id = %session_id, "session ownership mismatch");
                    return Ok(());
                }

                // Forward offer to agent
                let agents = state.signaling.agents.read().await;
                if let Some((_, agent_tx)) = agents.get(&session.machine_id.to_string()) {
                    let _ = agent_tx
                        .send(SignalingMessage::SignalingOffer { session_id, offer })
                        .await;
                }
            }
        }
        SignalingMessage::IceCandidate {
            session_id,
            candidate,
        } => {
            // Verify session ownership
            let session = sqlx::query_as::<_, SessionOwnerRow>(
                "SELECT user_id, machine_id FROM sessions WHERE id = $1",
            )
            .bind(uuid::Uuid::parse_str(&session_id)?)
            .fetch_optional(state.db.pool())
            .await?;

            if let Some(session) = session {
                if session.user_id.to_string() != user_id {
                    warn!(session_id = %session_id, "session ownership mismatch");
                    return Ok(());
                }

                // Forward ICE candidate to agent
                let agents = state.signaling.agents.read().await;
                if let Some((_, agent_tx)) = agents.get(&session.machine_id.to_string()) {
                    let _ = agent_tx
                        .send(SignalingMessage::IceCandidate {
                            session_id,
                            candidate,
                        })
                        .await;
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
struct SessionOwnerRow {
    user_id: uuid::Uuid,
    machine_id: uuid::Uuid,
}
