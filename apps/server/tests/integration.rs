//! Integration tests for the RemoteKVM coordination server.
//!
//! Each test runs against an isolated database provisioned by `#[sqlx::test]`
//! (a fresh database per test, migrations applied automatically). The full Axum
//! app is spawned on an ephemeral port and driven over real HTTP/WebSocket, so
//! these exercise routing, the JWT auth middleware, SQL queries, and the
//! agent<->client signaling relay end-to-end.
//!
//! Requires a reachable Postgres via `DATABASE_URL`, e.g.:
//!   DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5433/remotekvm \
//!     cargo test -p remotekvm-server

use std::sync::Arc;
use std::time::Duration;

use axum::{routing::post, Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sqlx::PgPool;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use remotekvm_server::auth::create_token;
use remotekvm_server::auth::workos::WorkOsClient;
use remotekvm_server::config::Config;
use remotekvm_server::create_app;
use remotekvm_server::db::Database;
use remotekvm_server::state::AppState;
use remotekvm_server::util::RateLimiter;
use remotekvm_server::websocket::{SignalingMessage, SignalingState};

use remotekvm_protocol::{ChannelMessage, InputEvent};
use remotekvm_transport::PeerSession;

const JWT_SECRET: &str = "test-secret-test-secret-test-secret-0123456789";

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

struct TestServer {
    addr: std::net::SocketAddr,
    http: reqwest::Client,
}

impl TestServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn ws_url(&self, path: &str) -> String {
        format!("ws://{}{}", self.addr, path)
    }
}

fn build_state(pool: PgPool, workos_api_base: String) -> Arc<AppState> {
    let config = Config {
        port: 0,
        database_url: String::new(),
        workos_api_key: "sk_test_dummy".to_string(),
        workos_client_id: "client_test_dummy".to_string(),
        workos_api_base,
        public_base_url: "http://localhost:0".to_string(),
        jwt_secret: JWT_SECRET.to_string(),
        jwt_expiry_hours: 24,
        redis_url: None,
        signaling_instance_id: "test-instance".to_string(),
        signaling_ttl_seconds: 90,
        stripe_secret_key: String::new(),
        stripe_webhook_secret: "whsec_test".to_string(),
        stripe_price_id: String::new(),
        stripe_api_base: "http://unused".to_string(),
    };
    let workos = WorkOsClient::new(
        config.workos_api_key.clone(),
        config.workos_client_id.clone(),
        config.workos_api_base.clone(),
    );
    Arc::new(AppState {
        db: Database::from_pool(pool),
        workos,
        config,
        signaling: SignalingState::new(),
        auth_rate_limiter: RateLimiter::auth_defaults(),
    })
}

async fn spawn(state: Arc<AppState>) -> TestServer {
    let app = create_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    TestServer {
        addr,
        http: reqwest::Client::new(),
    }
}

/// Spawn a stand-in WorkOS API that always returns a fixed authenticated user
/// (with one organization). Returns the base URL to feed into the config.
async fn spawn_mock_workos() -> String {
    async fn authenticate() -> Json<Value> {
        Json(json!({
            "user": {
                "id": "workos_user_abc",
                "email": "sarah@example.com",
                "firstName": "Sarah",
                "lastName": "Dev",
                "profilePictureUrl": "https://example.com/sarah.png",
                "organizations": [
                    {
                        "organization": { "id": "org_acme", "name": "Acme Corp" },
                        "role": { "slug": "admin" }
                    }
                ]
            }
        }))
    }

    let app = Router::new().route("/user_management/authenticate", post(authenticate));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{}", addr)
}

async fn spawn_mock_workos_user(
    workos_user_id: &'static str,
    email: &'static str,
    first_name: &'static str,
) -> String {
    async fn authenticate(
        axum::extract::State((workos_user_id, email, first_name)): axum::extract::State<(
            &'static str,
            &'static str,
            &'static str,
        )>,
    ) -> Json<Value> {
        Json(json!({
            "user": {
                "id": workos_user_id,
                "email": email,
                "firstName": first_name,
                "lastName": "Member",
                "organizations": []
            }
        }))
    }

    let app = Router::new()
        .route("/user_management/authenticate", post(authenticate))
        .with_state((workos_user_id, email, first_name));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{}", addr)
}

async fn insert_user(pool: &PgPool, workos_id: &str, email: &str) -> Uuid {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO users (workos_user_id, email) VALUES ($1, $2) RETURNING id",
    )
    .bind(workos_id)
    .bind(email)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn jwt_for(user_id: Uuid, email: &str) -> String {
    create_token(&user_id.to_string(), "workos_x", email, JWT_SECRET, 24).unwrap()
}

async fn register_machine(srv: &TestServer, jwt: &str, name: &str) -> (String, String) {
    let resp = srv
        .http
        .post(srv.url("/api/machines"))
        .bearer_auth(jwt)
        .json(&json!({ "name": name, "hostname": "host-1", "platform": "macos" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "register_machine should succeed");
    let body: Value = resp.json().await.unwrap();
    (
        body["id"].as_str().unwrap().to_string(),
        body["registration_token"].as_str().unwrap().to_string(),
    )
}

async fn next_signaling(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> SignalingMessage {
    let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("timed out waiting for a websocket message")
        .expect("websocket closed")
        .expect("websocket error");
    match msg {
        Message::Text(text) => serde_json::from_str(&text).expect("invalid signaling JSON"),
        other => panic!("expected text frame, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Health + auth
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn health_ok(pool: PgPool) {
    let srv = spawn(build_state(pool.clone(), "http://unused".into())).await;
    let resp = srv.http.get(srv.url("/health")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert!(body["version"].is_string());
    assert_eq!(body["dependencies"]["database"], "ok");
}

#[sqlx::test]
async fn health_reports_unavailable_when_db_is_down(pool: PgPool) {
    let srv = spawn(build_state(pool.clone(), "http://unused".into())).await;
    pool.close().await;

    let resp = srv.http.get(srv.url("/health")).send().await.unwrap();
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["dependencies"]["database"], "unavailable");
}

#[sqlx::test]
async fn protected_routes_require_jwt(pool: PgPool) {
    let srv = spawn(build_state(pool.clone(), "http://unused".into())).await;

    // No Authorization header.
    for path in ["/api/me", "/api/machines"] {
        let resp = srv.http.get(srv.url(path)).send().await.unwrap();
        assert_eq!(resp.status(), 401, "{path} without token should be 401");
    }

    // Garbage token.
    let resp = srv
        .http
        .get(srv.url("/api/me"))
        .bearer_auth("not-a-jwt")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // Token signed with the wrong secret.
    let bad = create_token(
        &Uuid::new_v4().to_string(),
        "w",
        "e@x.com",
        "a-totally-different-secret-aaaaaaaaaaaaaaa",
        24,
    )
    .unwrap();
    let resp = srv
        .http
        .get(srv.url("/api/me"))
        .bearer_auth(bad)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[sqlx::test]
async fn workos_callback_issues_jwt_and_syncs_user(pool: PgPool) {
    let workos_base = spawn_mock_workos().await;
    let state = build_state(pool.clone(), workos_base);
    let srv = spawn(state).await;

    // Exchange a code at the callback; mock WorkOS returns Sarah + Acme Corp.
    let resp = srv
        .http
        .get(srv.url("/auth/workos/callback?code=test_code"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let token = body["token"].as_str().unwrap().to_string();
    assert_eq!(body["user"]["email"], "sarah@example.com");
    assert_eq!(body["user"]["organizations"][0]["name"], "Acme Corp");
    assert_eq!(body["user"]["organizations"][0]["role"], "admin");

    // The user was actually persisted.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE workos_user_id = 'workos_user_abc'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);

    // The issued JWT works against /api/me and reflects the synced org.
    let me: Value = srv
        .http
        .get(srv.url("/api/me"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["email"], "sarah@example.com");
    assert_eq!(me["organizations"][0]["slug"], "acme-corp");

    // Logging in again must not create a duplicate user (upsert) and must still
    // issue a usable token via the UPDATE branch of the ON CONFLICT upsert.
    let relogin: Value = srv
        .http
        .get(srv.url("/auth/workos/callback?code=second_login"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let relogin_token = relogin["token"].as_str().expect("re-login returns a token");
    let me2 = srv
        .http
        .get(srv.url("/api/me"))
        .bearer_auth(relogin_token)
        .send()
        .await
        .unwrap();
    assert_eq!(me2.status(), 200, "re-login token must authenticate");

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE workos_user_id = 'workos_user_abc'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1, "re-login should upsert, not duplicate");
}

// ---------------------------------------------------------------------------
// Machines CRUD + permissions
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn machine_crud_lifecycle(pool: PgPool) {
    let user = insert_user(&pool, "workos_owner", "owner@example.com").await;
    let jwt = jwt_for(user, "owner@example.com");
    let srv = spawn(build_state(pool.clone(), "http://unused".into())).await;

    // Register.
    let (machine_id, token1) = register_machine(&srv, &jwt, "Workstation").await;
    assert!(token1.starts_with("rkvm_"));

    // List shows it.
    let list: Value = srv
        .http
        .get(srv.url("/api/machines"))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["machines"].as_array().unwrap().len(), 1);
    assert_eq!(list["machines"][0]["name"], "Workstation");
    assert_eq!(list["machines"][0]["online"], false);

    // Get by id.
    let resp = srv
        .http
        .get(srv.url(&format!("/api/machines/{machine_id}")))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Rotate token => different value.
    let rotated: Value = srv
        .http
        .post(srv.url(&format!("/api/machines/{machine_id}/rotate-token")))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token2 = rotated["registration_token"].as_str().unwrap();
    assert_ne!(token1, token2, "rotation must change the token");

    // Delete, then it's gone.
    let resp = srv
        .http
        .delete(srv.url(&format!("/api/machines/{machine_id}")))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = srv
        .http
        .get(srv.url(&format!("/api/machines/{machine_id}")))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[sqlx::test]
async fn machines_are_isolated_between_users(pool: PgPool) {
    let alice = insert_user(&pool, "workos_alice", "alice@example.com").await;
    let bob = insert_user(&pool, "workos_bob", "bob@example.com").await;
    let alice_jwt = jwt_for(alice, "alice@example.com");
    let bob_jwt = jwt_for(bob, "bob@example.com");
    let srv = spawn(build_state(pool, "http://unused".into())).await;

    let (machine_id, _) = register_machine(&srv, &alice_jwt, "Alice-Box").await;

    // Bob cannot see Alice's machine in his list.
    let bob_list: Value = srv
        .http
        .get(srv.url("/api/machines"))
        .bearer_auth(&bob_jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(bob_list["machines"].as_array().unwrap().len(), 0);

    // Bob cannot fetch it by id.
    let resp = srv
        .http
        .get(srv.url(&format!("/api/machines/{machine_id}")))
        .bearer_auth(&bob_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // Bob cannot delete it.
    let resp = srv
        .http
        .delete(srv.url(&format!("/api/machines/{machine_id}")))
        .bearer_auth(&bob_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[sqlx::test]
async fn organization_endpoints_enforce_admin_and_share_machines(pool: PgPool) {
    let admin = insert_user(&pool, "workos_admin", "admin@example.com").await;
    let member = insert_user(&pool, "workos_member", "member@example.com").await;
    let admin_jwt = jwt_for(admin, "admin@example.com");
    let member_jwt = jwt_for(member, "member@example.com");

    let org_id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (workos_org_id, name, slug) VALUES ('org_acme', 'Acme Corp', 'acme-corp') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'admin'), ($1, $3, 'member')",
    )
    .bind(org_id)
    .bind(admin)
    .bind(member)
    .execute(&pool)
    .await
    .unwrap();

    let srv = spawn(build_state(pool.clone(), "http://unused".into())).await;
    let (machine_id, _) = register_machine(&srv, &admin_jwt, "Admin-Box").await;

    let orgs: Value = srv
        .http
        .get(srv.url("/api/organizations"))
        .bearer_auth(&admin_jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(orgs["organizations"][0]["slug"], "acme-corp");
    assert_eq!(orgs["organizations"][0]["role"], "admin");

    let resp = srv
        .http
        .get(srv.url(&format!("/api/organizations/{org_id}/members")))
        .bearer_auth(&member_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "plain members cannot list members");

    let members: Value = srv
        .http
        .get(srv.url(&format!("/api/organizations/{org_id}/members")))
        .bearer_auth(&admin_jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(members["members"].as_array().unwrap().len(), 2);
    assert_eq!(members["pending_invites"].as_array().unwrap().len(), 0);

    let invited: Value = srv
        .http
        .post(srv.url(&format!("/api/organizations/{org_id}/invite")))
        .bearer_auth(&admin_jwt)
        .json(&json!({ "email": "new@example.com", "role": "member" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(invited["status"], "invited");

    let shared: Value = srv
        .http
        .put(srv.url(&format!(
            "/api/organizations/{org_id}/machines/{machine_id}"
        )))
        .bearer_auth(&admin_jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(shared["status"], "shared");
    let org_machine: Option<Uuid> =
        sqlx::query_scalar("SELECT organization_id FROM machines WHERE id = $1")
            .bind(Uuid::parse_str(&machine_id).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(org_machine, Some(org_id));
}

#[sqlx::test]
async fn organization_creation_invites_and_accepts_pending_membership(pool: PgPool) {
    let owner = insert_user(&pool, "workos_owner", "owner@example.com").await;
    let owner_jwt = jwt_for(owner, "owner@example.com");
    let srv = spawn(build_state(pool.clone(), "http://unused".into())).await;

    let created: Value = srv
        .http
        .post(srv.url("/api/organizations"))
        .bearer_auth(&owner_jwt)
        .json(&json!({ "name": "Remote Ops", "slug": "Remote Ops Team!" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let org_id = created["id"].as_str().unwrap();
    assert_eq!(created["slug"], "remote-ops-team");
    assert_eq!(created["role"], "owner");

    let owner_role: String = sqlx::query_scalar(
        "SELECT role FROM organization_members WHERE organization_id = $1 AND user_id = $2",
    )
    .bind(Uuid::parse_str(org_id).unwrap())
    .bind(owner)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(owner_role, "owner");

    let resp = srv
        .http
        .post(srv.url(&format!("/api/organizations/{org_id}/invite")))
        .bearer_auth(&owner_jwt)
        .json(&json!({ "email": "Pending.Member@Example.com", "role": "member" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let members: Value = srv
        .http
        .get(srv.url(&format!("/api/organizations/{org_id}/members")))
        .bearer_auth(&owner_jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(members["members"].as_array().unwrap().len(), 1);
    assert_eq!(
        members["pending_invites"][0]["email"],
        "pending.member@example.com"
    );

    let workos = spawn_mock_workos_user(
        "workos_pending_member",
        "pending.member@example.com",
        "Pending",
    )
    .await;
    let invited_srv = spawn(build_state(pool.clone(), workos)).await;
    let auth: Value = invited_srv
        .http
        .get(invited_srv.url("/auth/workos/callback?code=accept_invite"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let invited_jwt = auth["token"].as_str().unwrap();

    let me: Value = invited_srv
        .http
        .get(invited_srv.url("/api/me"))
        .bearer_auth(invited_jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["organizations"][0]["id"], org_id);
    assert_eq!(me["organizations"][0]["role"], "member");

    let members: Value = srv
        .http
        .get(srv.url(&format!("/api/organizations/{org_id}/members")))
        .bearer_auth(&owner_jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(members["members"].as_array().unwrap().len(), 2);
    assert_eq!(members["pending_invites"].as_array().unwrap().len(), 0);
}

#[sqlx::test]
async fn organization_rbac_blocks_member_admin_paths(pool: PgPool) {
    let owner = insert_user(&pool, "workos_owner", "owner@example.com").await;
    let admin = insert_user(&pool, "workos_admin", "admin@example.com").await;
    let member = insert_user(&pool, "workos_member", "member@example.com").await;
    let owner_jwt = jwt_for(owner, "owner@example.com");
    let admin_jwt = jwt_for(admin, "admin@example.com");
    let member_jwt = jwt_for(member, "member@example.com");

    let org_id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (workos_org_id, name, slug) VALUES ('org_rbac', 'RBAC Org', 'rbac-org') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'owner'), ($1, $3, 'admin'), ($1, $4, 'member')",
    )
    .bind(org_id)
    .bind(owner)
    .bind(admin)
    .bind(member)
    .execute(&pool)
    .await
    .unwrap();

    let srv = spawn(build_state(pool, "http://unused".into())).await;
    let (machine_id, _) = register_machine(&srv, &owner_jwt, "Owner-Box").await;

    let resp = srv
        .http
        .post(srv.url(&format!("/api/organizations/{org_id}/invite")))
        .bearer_auth(&admin_jwt)
        .json(&json!({ "email": "new-owner@example.com", "role": "owner" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "admins cannot grant owner");

    let resp = srv
        .http
        .post(srv.url(&format!("/api/organizations/{org_id}/invite")))
        .bearer_auth(&admin_jwt)
        .json(&json!({ "email": "owner@example.com", "role": "member" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "admins cannot demote owners");

    let resp = srv
        .http
        .post(srv.url(&format!("/api/organizations/{org_id}/invite")))
        .bearer_auth(&admin_jwt)
        .json(&json!({ "email": "new-admin@example.com", "role": "admin" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "admins can invite admins");

    let resp = srv
        .http
        .post(srv.url(&format!("/api/organizations/{org_id}/invite")))
        .bearer_auth(&member_jwt)
        .json(&json!({ "email": "new-member@example.com", "role": "member" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "members cannot invite");

    let resp = srv
        .http
        .put(srv.url(&format!(
            "/api/organizations/{org_id}/machines/{machine_id}"
        )))
        .bearer_auth(&member_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "members cannot share machines");

    let resp = srv
        .http
        .post(srv.url("/api/machines"))
        .bearer_auth(&member_jwt)
        .json(&json!({
            "name": "Bypass-Box",
            "hostname": "bypass-host",
            "platform": "macos",
            "organization_id": org_id
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        403,
        "members cannot attach machines directly to an org"
    );
}

#[sqlx::test]
async fn shared_machines_are_visible_and_connectable_to_members_only(pool: PgPool) {
    let owner = insert_user(&pool, "workos_owner", "owner@example.com").await;
    let member = insert_user(&pool, "workos_member", "member@example.com").await;
    let outsider = insert_user(&pool, "workos_outsider", "outsider@example.com").await;
    let owner_jwt = jwt_for(owner, "owner@example.com");
    let member_jwt = jwt_for(member, "member@example.com");
    let outsider_jwt = jwt_for(outsider, "outsider@example.com");

    let org_id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (workos_org_id, name, slug) VALUES ('org_share', 'Share Org', 'share-org') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'owner'), ($1, $3, 'member')",
    )
    .bind(org_id)
    .bind(owner)
    .bind(member)
    .execute(&pool)
    .await
    .unwrap();

    let srv = spawn(build_state(pool, "http://unused".into())).await;
    let (machine_id, reg_token) = register_machine(&srv, &owner_jwt, "Shared-Box").await;

    let resp = srv
        .http
        .put(srv.url(&format!(
            "/api/organizations/{org_id}/machines/{machine_id}"
        )))
        .bearer_auth(&owner_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let member_list: Value = srv
        .http
        .get(srv.url("/api/machines"))
        .bearer_auth(&member_jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(member_list["machines"].as_array().unwrap().len(), 1);
    assert_eq!(member_list["machines"][0]["id"], machine_id);

    let resp = srv
        .http
        .get(srv.url(&format!("/api/machines/{machine_id}")))
        .bearer_auth(&member_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "members can fetch shared machines");

    let outsider_list: Value = srv
        .http
        .get(srv.url("/api/machines"))
        .bearer_auth(&outsider_jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(outsider_list["machines"].as_array().unwrap().len(), 0);

    let resp = srv
        .http
        .get(srv.url(&format!("/api/machines/{machine_id}")))
        .bearer_auth(&outsider_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "outsiders cannot fetch shared machines");

    let (agent_ws, _) =
        tokio_tungstenite::connect_async(srv.ws_url(&format!("/agent?token={reg_token}")))
            .await
            .expect("agent websocket connect");

    let mut connected = false;
    for _ in 0..50 {
        let resp = srv
            .http
            .post(srv.url(&format!("/api/machines/{machine_id}/connect")))
            .bearer_auth(&member_jwt)
            .send()
            .await
            .unwrap();
        if resp.status() == 200 {
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(connected, "members can connect to shared machines");

    let resp = srv
        .http
        .post(srv.url(&format!("/api/machines/{machine_id}/connect")))
        .bearer_auth(&outsider_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "outsiders cannot connect");

    drop(agent_ws);
}

#[sqlx::test]
async fn connect_rejects_when_no_agent_connected(pool: PgPool) {
    let user = insert_user(&pool, "workos_owner", "owner@example.com").await;
    let jwt = jwt_for(user, "owner@example.com");
    let srv = spawn(build_state(pool.clone(), "http://unused".into())).await;
    let (machine_id, _) = register_machine(&srv, &jwt, "Workstation").await;

    // Offline machine => 400.
    let resp = srv
        .http
        .post(srv.url(&format!("/api/machines/{machine_id}/connect")))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Force the stale-online case: DB says online but no live signaling socket.
    sqlx::query("UPDATE machines SET online = true WHERE id = $1")
        .bind(Uuid::parse_str(&machine_id).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let resp = srv
        .http
        .post(srv.url(&format!("/api/machines/{machine_id}/connect")))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        400,
        "connect must reject when no agent holds a signaling socket"
    );
}

// ---------------------------------------------------------------------------
// End-to-end WebSocket signaling relay
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn signaling_relay_end_to_end(pool: PgPool) {
    let user = insert_user(&pool, "workos_owner", "owner@example.com").await;
    let jwt = jwt_for(user, "owner@example.com");
    let srv = spawn(build_state(pool.clone(), "http://unused".into())).await;

    let (_machine_id, reg_token) = register_machine(&srv, &jwt, "Workstation").await;

    // 1. Agent connects. On connect the server marks the machine online and
    //    registers it in the relay.
    let (mut agent_ws, _) =
        tokio_tungstenite::connect_async(srv.ws_url(&format!("/agent?token={reg_token}")))
            .await
            .expect("agent websocket connect");

    // 2. Poll /connect until it succeeds — this confirms the agent is both
    //    DB-online and present in the relay, and yields the session id.
    let session_id = {
        let mut got = None;
        for _ in 0..50 {
            let resp = srv
                .http
                .post(srv.url(&format!("/api/machines/{_machine_id}/connect")))
                .bearer_auth(&jwt)
                .send()
                .await
                .unwrap();
            if resp.status() == 200 {
                let body: Value = resp.json().await.unwrap();
                got = Some(body["session_id"].as_str().unwrap().to_string());
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        got.expect("connect never succeeded; agent did not come online")
    };

    // 3. Client connects with its JWT.
    let (mut client_ws, _) =
        tokio_tungstenite::connect_async(srv.ws_url(&format!("/client?token={jwt}")))
            .await
            .expect("client websocket connect");

    // 4. Client sends its WebRTC offer for this session.
    let offer = SignalingMessage::ConnectRequest {
        session_id: session_id.clone(),
        client_id: String::new(),
        offer: "OFFER_SDP".to_string(),
    };
    client_ws
        .send(Message::Text(serde_json::to_string(&offer).unwrap()))
        .await
        .unwrap();

    // 5. Agent receives the relayed offer.
    match next_signaling(&mut agent_ws).await {
        SignalingMessage::ConnectRequest {
            session_id: sid,
            offer,
            ..
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(offer, "OFFER_SDP");
        }
        other => panic!("agent expected ConnectRequest, got {other:?}"),
    }

    // 6. Agent answers; client receives the relayed answer.
    let answer = SignalingMessage::SignalingAnswer {
        session_id: session_id.clone(),
        answer: "ANSWER_SDP".to_string(),
    };
    agent_ws
        .send(Message::Text(serde_json::to_string(&answer).unwrap()))
        .await
        .unwrap();

    match next_signaling(&mut client_ws).await {
        SignalingMessage::ConnectResponse {
            session_id: sid,
            status,
            answer,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(status, "accepted");
            assert_eq!(answer.as_deref(), Some("ANSWER_SDP"));
        }
        other => panic!("client expected ConnectResponse, got {other:?}"),
    }
    let status: String = sqlx::query_scalar("SELECT status FROM sessions WHERE id = $1")
        .bind(Uuid::parse_str(&session_id).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "active");

    // 7. ICE candidate from agent is relayed to the client.
    let ice = SignalingMessage::IceCandidate {
        session_id: session_id.clone(),
        candidate: "candidate:1 udp".to_string(),
    };
    agent_ws
        .send(Message::Text(serde_json::to_string(&ice).unwrap()))
        .await
        .unwrap();

    match next_signaling(&mut client_ws).await {
        SignalingMessage::IceCandidate { candidate, .. } => {
            assert_eq!(candidate, "candidate:1 udp");
        }
        other => panic!("client expected IceCandidate, got {other:?}"),
    }

    drop(client_ws);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let list: Value = srv
        .http
        .get(srv.url("/api/sessions"))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["sessions"][0]["id"], session_id);
    assert_eq!(list["sessions"][0]["status"], "ended");
}

// ---------------------------------------------------------------------------
// Native loopback login flow
// ---------------------------------------------------------------------------

fn no_redirect_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

#[sqlx::test]
async fn login_init_redirects_to_workos_authorize(pool: PgPool) {
    let srv = spawn(build_state(pool, "https://workos.test".into())).await;
    let client = no_redirect_client();

    // Valid loopback redirect_uri => 307 to the WorkOS authorize endpoint.
    let resp = client
        .get(srv.url("/auth/login?redirect_uri=http://127.0.0.1:9999/callback"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 307);
    let location = resp.headers()["location"].to_str().unwrap().to_string();
    assert!(location.starts_with("https://workos.test/user_management/authorize"));
    assert!(location.contains("provider=authkit"));
    // The loopback target is carried through a signed JWT `state` (3 dot-parts).
    let state = location
        .split("state=")
        .nth(1)
        .expect("state param present");
    assert_eq!(
        state.matches('.').count(),
        2,
        "state should be a JWT: {state}"
    );

    // Non-loopback redirect_uri is rejected (no open redirect)...
    let resp = client
        .get(srv.url("/auth/login?redirect_uri=https://evil.example.com/steal"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // ...and `localhost` (DNS-resolved, not a guaranteed loopback IP) is rejected.
    let resp = client
        .get(srv.url("/auth/login?redirect_uri=http://localhost:9999/callback"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[sqlx::test]
async fn auth_routes_are_rate_limited(pool: PgPool) {
    let srv = spawn(build_state(pool, "https://workos.test".into())).await;
    let client = no_redirect_client();

    for _ in 0..30 {
        let resp = client
            .get(srv.url("/auth/login?redirect_uri=https://evil.example.com/steal"))
            .header("x-forwarded-for", "203.0.113.10")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    let resp = client
        .get(srv.url("/auth/login?redirect_uri=https://evil.example.com/steal"))
        .header("x-forwarded-for", "203.0.113.10")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
}

#[sqlx::test]
async fn callback_with_state_redirects_token_to_loopback(pool: PgPool) {
    let workos_base = spawn_mock_workos().await;
    let srv = spawn(build_state(pool, workos_base)).await;
    let client = no_redirect_client();

    // First obtain a legitimately-signed `state` from /auth/login.
    let login = client
        .get(srv.url("/auth/login?redirect_uri=http://127.0.0.1:9999/callback"))
        .send()
        .await
        .unwrap();
    let authorize = login.headers()["location"].to_str().unwrap().to_string();
    let state = authorize.split("state=").nth(1).unwrap().to_string();

    // WorkOS would echo that `state` back to our callback alongside the code.
    let resp = client
        .get(srv.url(&format!("/auth/workos/callback?code=abc&state={state}")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 307);
    let location = resp.headers()["location"].to_str().unwrap().to_string();
    assert!(
        location.starts_with("http://127.0.0.1:9999/callback?token="),
        "unexpected location: {location}"
    );

    // The redirected token is a usable session JWT.
    let token = location.split_once("token=").unwrap().1;
    let me = srv
        .http
        .get(srv.url("/api/me"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(me.status(), 200);

    // A forged / unsigned state is refused even with a valid code.
    let resp = client
        .get(srv.url("/auth/workos/callback?code=abc&state=http://127.0.0.1:9999/callback"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[sqlx::test]
async fn reconnecting_agent_keeps_machine_online(pool: PgPool) {
    // When a second agent socket registers for the same machine, the first
    // socket's teardown must not evict the newer registration.
    let user = insert_user(&pool, "workos_owner", "owner@example.com").await;
    let jwt = jwt_for(user, "owner@example.com");
    let srv = spawn(build_state(pool, "http://unused".into())).await;
    let (machine_id, reg_token) = register_machine(&srv, &jwt, "Workstation").await;

    async fn connect_ok(srv: &TestServer, jwt: &str, machine_id: &str) -> bool {
        for _ in 0..50 {
            let resp = srv
                .http
                .post(srv.url(&format!("/api/machines/{machine_id}/connect")))
                .bearer_auth(jwt)
                .send()
                .await
                .unwrap();
            if resp.status() == 200 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    // Agent A connects and becomes reachable.
    let agent_a =
        tokio_tungstenite::connect_async(srv.ws_url(&format!("/agent?token={reg_token}")))
            .await
            .unwrap()
            .0;
    assert!(connect_ok(&srv, &jwt, &machine_id).await);

    // Agent B connects for the same machine; A's slot is superseded and A's
    // socket tears down. The machine must remain reachable via B.
    let _agent_b =
        tokio_tungstenite::connect_async(srv.ws_url(&format!("/agent?token={reg_token}")))
            .await
            .unwrap()
            .0;
    drop(agent_a);
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert!(
        connect_ok(&srv, &jwt, &machine_id).await,
        "machine should stay online after a reconnect superseded the old socket"
    );
}

#[sqlx::test]
async fn webrtc_datachannel_establishes_through_relay(pool: PgPool) {
    // The full vertical slice: a real WebRTC `control` DataChannel is negotiated
    // between a client (offerer) and an agent (answerer) entirely through the
    // server's signaling relay, and a client→agent InputEvent is delivered.
    // No GPU/display needed — webrtc-rs negotiates in software over loopback,
    // and we use non-trickle ICE (candidates embedded in the offer/answer SDP).
    let user = insert_user(&pool, "workos_owner", "owner@example.com").await;
    let jwt = jwt_for(user, "owner@example.com");
    let srv = spawn(build_state(pool, "http://unused".into())).await;
    let (machine_id, reg_token) = register_machine(&srv, &jwt, "Workstation").await;

    // Agent and client connect their signaling WebSockets.
    let (mut agent_ws, _) =
        tokio_tungstenite::connect_async(srv.ws_url(&format!("/agent?token={reg_token}")))
            .await
            .expect("agent ws");

    // Provision a session once the agent is registered in the relay.
    let session_id = {
        let mut got = None;
        for _ in 0..50 {
            let resp = srv
                .http
                .post(srv.url(&format!("/api/machines/{machine_id}/connect")))
                .bearer_auth(&jwt)
                .send()
                .await
                .unwrap();
            if resp.status() == 200 {
                let body: Value = resp.json().await.unwrap();
                got = Some(body["session_id"].as_str().unwrap().to_string());
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        got.expect("agent never came online")
    };

    let (mut client_ws, _) =
        tokio_tungstenite::connect_async(srv.ws_url(&format!("/client?token={jwt}")))
            .await
            .expect("client ws");

    // WebRTC peers.
    let client = std::sync::Arc::new(PeerSession::offerer().await.unwrap());
    let agent = std::sync::Arc::new(PeerSession::answerer().await.unwrap());

    // Client builds a self-contained offer and sends it over the relay.
    client.create_offer().await.unwrap();
    let offer = client.wait_ice_gathering().await.unwrap();
    let req = SignalingMessage::ConnectRequest {
        session_id: session_id.clone(),
        client_id: String::new(),
        offer,
    };
    client_ws
        .send(Message::Text(serde_json::to_string(&req).unwrap()))
        .await
        .unwrap();

    // Agent receives the relayed offer, answers, and returns it over the relay.
    let offer = match next_signaling(&mut agent_ws).await {
        SignalingMessage::ConnectRequest { offer, .. } => offer,
        other => panic!("agent expected ConnectRequest, got {other:?}"),
    };
    agent.accept_offer(&offer).await.unwrap();
    let answer = agent.wait_ice_gathering().await.unwrap();
    let ans = SignalingMessage::SignalingAnswer {
        session_id: session_id.clone(),
        answer,
    };
    agent_ws
        .send(Message::Text(serde_json::to_string(&ans).unwrap()))
        .await
        .unwrap();

    // Client applies the relayed answer.
    match next_signaling(&mut client_ws).await {
        SignalingMessage::ConnectResponse {
            answer: Some(answer),
            ..
        } => client.set_answer(&answer).await.unwrap(),
        other => panic!("client expected ConnectResponse, got {other:?}"),
    }

    // The DataChannel must open, and an InputEvent must traverse it.
    client
        .wait_channel_open(Duration::from_secs(20))
        .await
        .expect("control channel should open over the relay-negotiated connection");

    client
        .send(&ChannelMessage::Input(InputEvent::KeyDown {
            hid_usage: 0x07,
        }))
        .await
        .unwrap();

    let got = tokio::time::timeout(Duration::from_secs(5), agent.recv())
        .await
        .expect("recv timed out")
        .expect("channel closed");
    match got {
        ChannelMessage::Input(InputEvent::KeyDown { hid_usage }) => assert_eq!(hid_usage, 0x07),
        other => panic!("agent expected KeyDown, got {other:?}"),
    }

    client.close().await.unwrap();
    agent.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// Stripe billing (verified against a mock Stripe + a signed webhook)
// ---------------------------------------------------------------------------

/// AppState with Stripe configured to talk to `stripe_base`.
fn build_state_with_stripe(pool: PgPool, stripe_base: String) -> Arc<AppState> {
    let state = build_state(pool, "http://unused".into());
    // Rebuild with stripe fields set (Config is cheap to clone/replace).
    let mut config = state.config.clone();
    config.stripe_secret_key = "sk_test_dummy".to_string();
    config.stripe_price_id = "price_test".to_string();
    config.stripe_api_base = stripe_base;
    config.public_base_url = "http://localhost:18080".to_string();
    Arc::new(AppState {
        db: state.db.clone(),
        workos: state.workos.clone(),
        config,
        signaling: SignalingState::new(),
        auth_rate_limiter: RateLimiter::auth_defaults(),
    })
}

async fn spawn_mock_stripe() -> String {
    async fn create_session() -> Json<Value> {
        Json(json!({ "id": "cs_test_123", "url": "https://checkout.stripe.test/c/cs_test_123" }))
    }
    let app = Router::new().route("/v1/checkout/sessions", post(create_session));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[sqlx::test]
async fn billing_checkout_returns_stripe_url(pool: PgPool) {
    let stripe = spawn_mock_stripe().await;
    let user = insert_user(&pool, "workos_owner", "owner@example.com").await;
    let jwt = jwt_for(user, "owner@example.com");
    let srv = spawn(build_state_with_stripe(pool, stripe)).await;

    let resp = srv
        .http
        .post(srv.url("/api/billing/checkout"))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["url"], "https://checkout.stripe.test/c/cs_test_123");

    // Unauthenticated callers are rejected by the JWT middleware.
    let resp = srv
        .http
        .post(srv.url("/api/billing/checkout"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[sqlx::test]
async fn billing_checkout_supports_org_seat_quantity_and_usage(pool: PgPool) {
    let stripe = spawn_mock_stripe().await;
    let admin = insert_user(&pool, "workos_admin", "admin@example.com").await;
    let member = insert_user(&pool, "workos_member", "member@example.com").await;
    let admin_jwt = jwt_for(admin, "admin@example.com");
    let org_id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (workos_org_id, name, slug) VALUES ('org_billing', 'Billing Org', 'billing-org') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'admin'), ($1, $3, 'member')",
    )
    .bind(org_id)
    .bind(admin)
    .bind(member)
    .execute(&pool)
    .await
    .unwrap();

    let srv = spawn(build_state_with_stripe(pool.clone(), stripe)).await;
    let resp = srv
        .http
        .post(srv.url("/api/billing/checkout"))
        .bearer_auth(&admin_jwt)
        .json(&json!({ "organization_id": org_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let seat_count: i32 = sqlx::query_scalar("SELECT seat_count FROM organizations WHERE id = $1")
        .bind(org_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(seat_count, 2);

    let (machine_id, _) = register_machine(&srv, &admin_jwt, "Billing-Box").await;
    let machine_id = Uuid::parse_str(&machine_id).unwrap();
    sqlx::query("UPDATE machines SET organization_id = $1 WHERE id = $2")
        .bind(org_id)
        .bind(machine_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"
        INSERT INTO sessions (user_id, machine_id, started_at, ended_at, status, bytes_sent, bytes_received)
        VALUES ($1, $2, NOW() - INTERVAL '15 minutes', NOW(), 'ended', 1024, 2048)
        "#,
    )
    .bind(admin)
    .bind(machine_id)
    .execute(&pool)
    .await
    .unwrap();

    let usage: Value = srv
        .http
        .get(srv.url("/api/billing/usage"))
        .bearer_auth(&admin_jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(usage["organizations"][0]["id"], org_id.to_string());
    assert_eq!(usage["organizations"][0]["connection_minutes"], 15);
    assert_eq!(usage["organizations"][0]["bytes_sent"], 1024);
    assert_eq!(usage["organizations"][0]["bytes_received"], 2048);
}

#[sqlx::test]
async fn free_plan_usage_quota_blocks_new_sessions(pool: PgPool) {
    let user = insert_user(&pool, "workos_owner", "owner@example.com").await;
    let jwt = jwt_for(user, "owner@example.com");
    let srv = spawn(build_state(pool.clone(), "http://unused".into())).await;
    let (machine_id, _) = register_machine(&srv, &jwt, "Quota-Box").await;
    let machine_id = Uuid::parse_str(&machine_id).unwrap();

    sqlx::query(
        r#"
        INSERT INTO sessions (user_id, machine_id, started_at, ended_at, status)
        VALUES ($1, $2, NOW() - INTERVAL '121 minutes', NOW(), 'ended')
        "#,
    )
    .bind(user)
    .bind(machine_id)
    .execute(&pool)
    .await
    .unwrap();

    let resp = srv
        .http
        .post(srv.url(&format!("/api/machines/{machine_id}/connect")))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "free monthly quota should be enforced");
}

#[sqlx::test]
async fn stripe_webhook_verifies_signature_and_persists_customer(pool: PgPool) {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let user = insert_user(&pool, "workos_owner", "owner@example.com").await;
    // Default build_state uses webhook secret "whsec_test".
    let srv = spawn(build_state(pool.clone(), "http://unused".into())).await;

    let body = json!({
        "type": "checkout.session.completed",
        "data": { "object": {
            "client_reference_id": user.to_string(),
            "customer": "cus_test_abc",
            "subscription": "sub_test_abc"
        }}
    })
    .to_string();

    let sign = |secret: &str, ts: i64, body: &str| {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(ts.to_string().as_bytes());
        mac.update(b".");
        mac.update(body.as_bytes());
        format!("t={ts},v1={}", hex::encode(mac.finalize().into_bytes()))
    };

    // Use the real current time so the handler's ±300s tolerance check passes.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let header = sign("whsec_test", now, &body);
    let resp = srv
        .http
        .post(srv.url("/webhooks/stripe"))
        .header("Stripe-Signature", header)
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "valid signature should be accepted");

    let customer: Option<String> =
        sqlx::query_scalar("SELECT stripe_customer_id FROM users WHERE id = $1")
            .bind(user)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(customer.as_deref(), Some("cus_test_abc"));

    // Wrong signature → 400.
    let bad = sign("whsec_wrong", now, &body);
    let resp = srv
        .http
        .post(srv.url("/webhooks/stripe"))
        .header("Stripe-Signature", bad)
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "forged signature must be rejected");
}

#[sqlx::test]
async fn stripe_webhook_rechecks_org_admin_before_team_upgrade(pool: PgPool) {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let admin = insert_user(&pool, "workos_admin", "admin@example.com").await;
    let member = insert_user(&pool, "workos_member", "member@example.com").await;
    let org: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO organizations (workos_org_id, name, slug)
        VALUES ('org_acme', 'Acme Corp', 'acme')
        RETURNING id
        "#,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"
        INSERT INTO organization_members (organization_id, user_id, role)
        VALUES ($1, $2, 'admin'), ($1, $3, 'member')
        "#,
    )
    .bind(org)
    .bind(admin)
    .bind(member)
    .execute(&pool)
    .await
    .unwrap();

    let srv = spawn(build_state(pool.clone(), "http://unused".into())).await;

    let sign = |body: &str| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"whsec_test").unwrap();
        mac.update(now.to_string().as_bytes());
        mac.update(b".");
        mac.update(body.as_bytes());
        format!("t={now},v1={}", hex::encode(mac.finalize().into_bytes()))
    };

    let forged_org_body = json!({
        "type": "checkout.session.completed",
        "data": { "object": {
            "client_reference_id": member.to_string(),
            "customer": "cus_member",
            "subscription": "sub_member",
            "metadata": { "organization_id": org.to_string() }
        }}
    })
    .to_string();
    let resp = srv
        .http
        .post(srv.url("/webhooks/stripe"))
        .header("Stripe-Signature", sign(&forged_org_body))
        .body(forged_org_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let org_plan: String = sqlx::query_scalar("SELECT plan FROM organizations WHERE id = $1")
        .bind(org)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(org_plan, "free");

    let admin_body = json!({
        "type": "checkout.session.completed",
        "data": { "object": {
            "client_reference_id": admin.to_string(),
            "customer": "cus_admin",
            "subscription": "sub_admin",
            "metadata": { "organization_id": org.to_string() }
        }}
    })
    .to_string();
    let resp = srv
        .http
        .post(srv.url("/webhooks/stripe"))
        .header("Stripe-Signature", sign(&admin_body))
        .body(admin_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let (org_plan, subscription_id): (String, Option<String>) =
        sqlx::query_as("SELECT plan, stripe_subscription_id FROM organizations WHERE id = $1")
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(org_plan, "team");
    assert_eq!(subscription_id.as_deref(), Some("sub_admin"));
}

#[sqlx::test]
async fn agent_with_invalid_token_is_rejected(pool: PgPool) {
    let srv = spawn(build_state(pool, "http://unused".into())).await;
    let result =
        tokio_tungstenite::connect_async(srv.ws_url("/agent?token=bogus-registration-token")).await;
    assert!(
        result.is_err(),
        "agent websocket with an unknown token must be rejected"
    );
}
