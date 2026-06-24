use axum::{
    extract::{
        ws::{Message, WebSocket},
        Query, State, WebSocketUpgrade,
    },
    response::Response,
};
use redis::{aio::ConnectionManager, AsyncCommands};
use serde::{Deserialize, Serialize};
use serde_json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::auth::validate_token;
use crate::config::Config;
use crate::error::ApiError;
use crate::state::AppState;

const WS_CHANNEL_SIZE: usize = 64;
const REDIS_KEY_PREFIX: &str = "remotekvm:signaling";

/// A registered agent socket: its forward channel plus a unique connection id
/// so a reconnecting agent doesn't get torn down by the old socket's cleanup.
type AgentEntry = (u64, tokio::sync::mpsc::Sender<SignalingMessage>);

/// Shared state for WebSocket connections.
#[derive(Clone)]
pub struct SignalingState {
    /// Map of machine_id -> (connection id, agent WebSocket sender)
    agents: Arc<RwLock<HashMap<String, AgentEntry>>>,
    /// Map of session_id -> client WebSocket sender
    clients: Arc<RwLock<HashMap<String, tokio::sync::mpsc::Sender<SignalingMessage>>>>,
    /// Monotonic source of per-connection ids.
    next_conn_id: Arc<std::sync::atomic::AtomicU64>,
    instance_id: String,
    redis: Option<RedisSignalingStore>,
}

#[derive(Clone)]
struct RedisSignalingStore {
    connection: ConnectionManager,
    instance_id: String,
    ttl_seconds: u64,
}

#[derive(Debug, Clone)]
struct AgentPresence {
    instance_id: String,
    conn_id: u64,
}

impl Default for SignalingState {
    fn default() -> Self {
        Self::new()
    }
}

impl SignalingState {
    pub fn new() -> Self {
        Self {
            agents: Arc::new(RwLock::new(HashMap::new())),
            clients: Arc::new(RwLock::new(HashMap::new())),
            next_conn_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            instance_id: format!("local-{}", uuid::Uuid::new_v4()),
            redis: None,
        }
    }

    pub async fn from_config(config: &Config) -> anyhow::Result<Self> {
        let mut state = Self {
            instance_id: config.signaling_instance_id.clone(),
            ..Self::new()
        };

        if let Some(redis_url) = &config.redis_url {
            state.redis = Some(
                RedisSignalingStore::connect(
                    redis_url,
                    config.signaling_instance_id.clone(),
                    config.signaling_ttl_seconds,
                )
                .await?,
            );
            info!(
                instance_id = %config.signaling_instance_id,
                ttl_seconds = config.signaling_ttl_seconds,
                "redis signaling metadata enabled"
            );
        } else {
            info!(
                instance_id = %config.signaling_instance_id,
                "redis signaling metadata disabled; using process-local signaling state"
            );
        }

        Ok(state)
    }

    fn next_conn_id(&self) -> u64 {
        self.next_conn_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether an agent for `machine_id` currently holds a live socket in this
    /// process.
    ///
    /// Redis, when configured, stores cross-instance presence/routing metadata,
    /// but the actual WebSocket sender channel is still process-local. Until the
    /// relay has Redis/NATS pub-sub forwarding or strict load-balancer affinity,
    /// this local check remains the safe source of truth for "can this process
    /// signal this machine right now".
    pub async fn is_agent_online(&self, machine_id: &str) -> bool {
        if self.agents.read().await.contains_key(machine_id) {
            return true;
        }

        if let Some(presence) = self.redis_agent_presence(machine_id).await {
            warn!(
                machine_id,
                agent_instance_id = %presence.instance_id,
                agent_conn_id = presence.conn_id,
                local_instance_id = %self.instance_id,
                "agent is present in redis on another instance, but websocket relay is process-local"
            );
        }

        false
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

    async fn register_agent(
        &self,
        machine_id: &str,
        conn_id: u64,
        tx: tokio::sync::mpsc::Sender<SignalingMessage>,
    ) {
        {
            let mut agents = self.agents.write().await;
            agents.insert(machine_id.to_string(), (conn_id, tx));
        }

        if let Some(redis) = &self.redis {
            if let Err(e) = redis.set_agent_online(machine_id, conn_id).await {
                warn!(error = %e, machine_id, "failed to persist agent presence in redis");
            }
        }
    }

    async fn refresh_agent(&self, machine_id: &str, conn_id: u64) {
        if let Some(redis) = &self.redis {
            if let Err(e) = redis.refresh_agent(machine_id, conn_id).await {
                warn!(error = %e, machine_id, "failed to refresh agent presence in redis");
            }
        }
    }

    async fn unregister_agent_if_current(&self, machine_id: &str, conn_id: u64) -> bool {
        let was_current = {
            let mut agents = self.agents.write().await;
            match agents.get(machine_id) {
                Some((id, _)) if *id == conn_id => {
                    agents.remove(machine_id);
                    true
                }
                _ => false,
            }
        };

        if was_current {
            if let Some(redis) = &self.redis {
                if let Err(e) = redis.clear_agent(machine_id, conn_id).await {
                    warn!(error = %e, machine_id, "failed to clear agent presence from redis");
                }
            }
        }

        was_current
    }

    pub async fn record_session_route(&self, session_id: &str, machine_id: &str, user_id: &str) {
        if let Some(redis) = &self.redis {
            let agent_instance_id = self
                .redis_agent_presence(machine_id)
                .await
                .map(|presence| presence.instance_id)
                .unwrap_or_else(|| self.instance_id.clone());
            if let Err(e) = redis
                .set_session_route(session_id, machine_id, user_id, &agent_instance_id)
                .await
            {
                warn!(error = %e, session_id, "failed to persist session route in redis");
            }
        }
    }

    async fn register_client_session(
        &self,
        session_id: &str,
        machine_id: &str,
        user_id: &str,
        tx: tokio::sync::mpsc::Sender<SignalingMessage>,
    ) {
        {
            let mut clients = self.clients.write().await;
            clients.insert(session_id.to_string(), tx);
        }

        if let Some(redis) = &self.redis {
            let agent_instance_id = self
                .redis_agent_presence(machine_id)
                .await
                .map(|presence| presence.instance_id)
                .unwrap_or_else(|| self.instance_id.clone());
            if let Err(e) = redis
                .set_session_route(session_id, machine_id, user_id, &agent_instance_id)
                .await
            {
                warn!(error = %e, session_id, "failed to refresh session route in redis");
            }
        }
    }

    async fn unregister_client_sessions(&self, session_ids: &[String]) {
        {
            let mut clients = self.clients.write().await;
            for session_id in session_ids {
                clients.remove(session_id);
            }
        }

        if let Some(redis) = &self.redis {
            for session_id in session_ids {
                if let Err(e) = redis.clear_session_route(session_id).await {
                    warn!(error = %e, session_id, "failed to clear session route from redis");
                }
            }
        }
    }

    async fn send_to_client(&self, session_id: &str, msg: SignalingMessage) -> bool {
        let clients = self.clients.read().await;
        match clients.get(session_id) {
            Some(tx) => tx.send(msg).await.is_ok(),
            None => false,
        }
    }

    async fn redis_agent_presence(&self, machine_id: &str) -> Option<AgentPresence> {
        let redis = self.redis.as_ref()?;
        match redis.agent_presence(machine_id).await {
            Ok(presence) => presence,
            Err(e) => {
                warn!(error = %e, machine_id, "failed to read agent presence from redis");
                None
            }
        }
    }
}

impl RedisSignalingStore {
    async fn connect(url: &str, instance_id: String, ttl_seconds: u64) -> anyhow::Result<Self> {
        let client = redis::Client::open(url)?;
        let connection = ConnectionManager::new(client).await?;
        Ok(Self {
            connection,
            instance_id,
            ttl_seconds,
        })
    }

    async fn set_agent_online(&self, machine_id: &str, conn_id: u64) -> redis::RedisResult<()> {
        let key = agent_key(machine_id);
        let now = now_millis();
        let mut conn = self.connection.clone();
        redis::pipe()
            .atomic()
            .cmd("HSET")
            .arg(&key)
            .arg("machine_id")
            .arg(machine_id)
            .arg("instance_id")
            .arg(&self.instance_id)
            .arg("conn_id")
            .arg(conn_id)
            .arg("connected_at_ms")
            .arg(now)
            .arg("last_seen_ms")
            .arg(now)
            .cmd("EXPIRE")
            .arg(&key)
            .arg(self.ttl_seconds)
            .query_async::<_, ()>(&mut conn)
            .await
    }

    async fn refresh_agent(&self, machine_id: &str, conn_id: u64) -> redis::RedisResult<()> {
        let key = agent_key(machine_id);
        let mut conn = self.connection.clone();
        let _: i32 = redis::Script::new(
            r#"
            if redis.call("HGET", KEYS[1], "instance_id") == ARGV[1]
              and redis.call("HGET", KEYS[1], "conn_id") == ARGV[2] then
              redis.call("HSET", KEYS[1], "last_seen_ms", ARGV[3])
              redis.call("EXPIRE", KEYS[1], ARGV[4])
              return 1
            end
            return 0
            "#,
        )
        .key(key)
        .arg(&self.instance_id)
        .arg(conn_id.to_string())
        .arg(now_millis().to_string())
        .arg(self.ttl_seconds.to_string())
        .invoke_async(&mut conn)
        .await?;
        Ok(())
    }

    async fn clear_agent(&self, machine_id: &str, conn_id: u64) -> redis::RedisResult<()> {
        let key = agent_key(machine_id);
        let mut conn = self.connection.clone();
        let _: i32 = redis::Script::new(
            r#"
            if redis.call("HGET", KEYS[1], "instance_id") == ARGV[1]
              and redis.call("HGET", KEYS[1], "conn_id") == ARGV[2] then
              return redis.call("DEL", KEYS[1])
            end
            return 0
            "#,
        )
        .key(key)
        .arg(&self.instance_id)
        .arg(conn_id.to_string())
        .invoke_async(&mut conn)
        .await?;
        Ok(())
    }

    async fn agent_presence(&self, machine_id: &str) -> redis::RedisResult<Option<AgentPresence>> {
        let key = agent_key(machine_id);
        let mut conn = self.connection.clone();
        let fields: HashMap<String, String> = conn.hgetall(key).await?;
        if fields.is_empty() {
            return Ok(None);
        }

        let Some(instance_id) = fields.get("instance_id").cloned() else {
            return Ok(None);
        };
        let conn_id = fields
            .get("conn_id")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default();

        Ok(Some(AgentPresence {
            instance_id,
            conn_id,
        }))
    }

    async fn set_session_route(
        &self,
        session_id: &str,
        machine_id: &str,
        user_id: &str,
        agent_instance_id: &str,
    ) -> redis::RedisResult<()> {
        let key = session_key(session_id);
        let mut conn = self.connection.clone();
        redis::pipe()
            .atomic()
            .cmd("HSET")
            .arg(&key)
            .arg("session_id")
            .arg(session_id)
            .arg("machine_id")
            .arg(machine_id)
            .arg("user_id")
            .arg(user_id)
            .arg("agent_instance_id")
            .arg(agent_instance_id)
            .arg("client_instance_id")
            .arg(&self.instance_id)
            .arg("updated_at_ms")
            .arg(now_millis())
            .cmd("EXPIRE")
            .arg(&key)
            .arg(self.ttl_seconds)
            .query_async::<_, ()>(&mut conn)
            .await
    }

    async fn clear_session_route(&self, session_id: &str) -> redis::RedisResult<()> {
        let mut conn = self.connection.clone();
        conn.del(session_key(session_id)).await
    }
}

fn agent_key(machine_id: &str) -> String {
    format!("{REDIS_KEY_PREFIX}:agent:{machine_id}")
}

fn session_key(session_id: &str) -> String {
    format!("{REDIS_KEY_PREFIX}:session:{session_id}")
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
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
    state
        .signaling
        .register_agent(&machine_id.to_string(), conn_id, tx)
        .await;
    let mut presence_refresh = tokio::time::interval(Duration::from_secs(
        state.config.signaling_ttl_seconds.saturating_div(3).max(10),
    ));
    presence_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Main loop: read from socket and from the forward channel
    loop {
        tokio::select! {
            _ = presence_refresh.tick() => {
                state.signaling.refresh_agent(&machine_id.to_string(), conn_id).await;
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Err(e) = handle_agent_message(&text, &machine_id.to_string(), conn_id, &state).await {
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
    let was_current = state
        .signaling
        .unregister_agent_if_current(&machine_id.to_string(), conn_id)
        .await;

    if was_current {
        if let Err(e) = sqlx::query("UPDATE machines SET online = false WHERE id = $1")
            .bind(machine_id)
            .execute(state.db.pool())
            .await
        {
            error!(error = %e, "failed to mark machine as offline");
        }
        if let Err(e) = end_machine_sessions(&state, machine_id).await {
            error!(error = %e, "failed to end machine sessions");
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
    machine_id: &str,
    conn_id: u64,
    state: &AppState,
) -> anyhow::Result<()> {
    let msg: SignalingMessage = serde_json::from_str(text)?;

    match msg {
        SignalingMessage::SignalingAnswer { session_id, answer } => {
            // A compromised agent must not be able to answer (and thus inject its
            // SDP into) a session that belongs to a different machine, nor inflate
            // another session's usage counters. Bind every relayed message to a
            // session owned by this authenticated machine.
            if !session_belongs_to_machine(state, &session_id, machine_id).await? {
                warn!(
                    machine_id,
                    session_id, "agent sent answer for a session it does not own; dropping"
                );
                return Ok(());
            }
            mark_session_active(state, &session_id).await?;
            record_session_usage(state, &session_id, text.len() as i64, 0).await?;
            // Forward answer to the client
            let _ = state
                .signaling
                .send_to_client(
                    &session_id,
                    SignalingMessage::ConnectResponse {
                        session_id: session_id.clone(),
                        status: "accepted".to_string(),
                        answer: Some(answer),
                    },
                )
                .await;
        }
        SignalingMessage::IceCandidate {
            session_id,
            candidate,
        } => {
            if !session_belongs_to_machine(state, &session_id, machine_id).await? {
                warn!(
                    machine_id,
                    session_id, "agent sent ICE for a session it does not own; dropping"
                );
                return Ok(());
            }
            record_session_usage(state, &session_id, text.len() as i64, 0).await?;
            // Forward ICE candidate to the client
            let _ = state
                .signaling
                .send_to_client(
                    &session_id,
                    SignalingMessage::IceCandidate {
                        session_id: session_id.clone(),
                        candidate,
                    },
                )
                .await;
        }
        SignalingMessage::Heartbeat => {
            // Heartbeat received, agent is alive
            state.signaling.refresh_agent(machine_id, conn_id).await;
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
    state
        .signaling
        .unregister_client_sessions(&owned_sessions)
        .await;
    for session_id in owned_sessions {
        if let Err(e) = mark_session_ended(&state, &session_id).await {
            warn!(error = %e, session_id = %session_id, "failed to end client session");
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
                state
                    .signaling
                    .register_client_session(
                        &session_id,
                        &session.machine_id.to_string(),
                        user_id,
                        client_tx.clone(),
                    )
                    .await;
                owned_sessions.push(session_id.clone());
                record_session_usage(state, &session_id, 0, text.len() as i64).await?;

                // Forward to agent
                let _ = state
                    .signaling
                    .notify_agent(
                        &session.machine_id.to_string(),
                        SignalingMessage::ConnectRequest {
                            session_id: session_id.clone(),
                            client_id: user_id.to_string(),
                            offer,
                        },
                    )
                    .await;
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

                record_session_usage(state, &session_id, 0, text.len() as i64).await?;

                // Forward offer to agent
                let _ = state
                    .signaling
                    .notify_agent(
                        &session.machine_id.to_string(),
                        SignalingMessage::SignalingOffer { session_id, offer },
                    )
                    .await;
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

                record_session_usage(state, &session_id, 0, text.len() as i64).await?;

                // Forward ICE candidate to agent
                let _ = state
                    .signaling
                    .notify_agent(
                        &session.machine_id.to_string(),
                        SignalingMessage::IceCandidate {
                            session_id,
                            candidate,
                        },
                    )
                    .await;
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

/// Whether `session_id` exists and is owned by `machine_id`. Used to reject an
/// agent relaying signaling for sessions belonging to other machines/tenants.
async fn session_belongs_to_machine(
    state: &AppState,
    session_id: &str,
    machine_id: &str,
) -> anyhow::Result<bool> {
    let session_uuid = uuid::Uuid::parse_str(session_id)?;
    let machine_uuid = uuid::Uuid::parse_str(machine_id)?;
    let row = sqlx::query_scalar::<_, bool>(
        r#"SELECT EXISTS(SELECT 1 FROM sessions WHERE id = $1 AND machine_id = $2)"#,
    )
    .bind(session_uuid)
    .bind(machine_uuid)
    .fetch_one(state.db.pool())
    .await?;
    Ok(row)
}

async fn mark_session_active(state: &AppState, session_id: &str) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        UPDATE sessions
        SET status = 'active'
        WHERE id = $1 AND status = 'pending'
        "#,
    )
    .bind(uuid::Uuid::parse_str(session_id)?)
    .execute(state.db.pool())
    .await?;
    Ok(())
}

async fn record_session_usage(
    state: &AppState,
    session_id: &str,
    bytes_sent: i64,
    bytes_received: i64,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        UPDATE sessions
        SET bytes_sent = bytes_sent + $2,
            bytes_received = bytes_received + $3
        WHERE id = $1
        "#,
    )
    .bind(uuid::Uuid::parse_str(session_id)?)
    .bind(bytes_sent)
    .bind(bytes_received)
    .execute(state.db.pool())
    .await?;
    Ok(())
}

async fn mark_session_ended(state: &AppState, session_id: &str) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        UPDATE sessions
        SET status = 'ended', ended_at = COALESCE(ended_at, NOW())
        WHERE id = $1 AND status <> 'ended'
        "#,
    )
    .bind(uuid::Uuid::parse_str(session_id)?)
    .execute(state.db.pool())
    .await?;
    Ok(())
}

async fn end_machine_sessions(state: &AppState, machine_id: uuid::Uuid) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        UPDATE sessions
        SET status = 'ended', ended_at = COALESCE(ended_at, NOW())
        WHERE machine_id = $1 AND status <> 'ended'
        "#,
    )
    .bind(machine_id)
    .execute(state.db.pool())
    .await?;
    Ok(())
}
