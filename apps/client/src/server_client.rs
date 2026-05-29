use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// REST API client for the RemoteKVM SaaS server.
///
/// `base_url` is the API root including the `/api` prefix, e.g.
/// `http://localhost:8080/api` (see [`crate::config::Config::api_url`]).
#[derive(Clone)]
pub struct ApiClient {
    base_url: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl ApiClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: None,
            http: reqwest::Client::new(),
        }
    }

    pub fn set_token(&mut self, token: &str) {
        self.token = Some(token.to_string());
    }

    fn bearer(&self, rb: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        let token = self
            .token
            .as_ref()
            .ok_or_else(|| anyhow!("not authenticated: no session token set"))?;
        Ok(rb.bearer_auth(token))
    }

    /// Map a non-2xx response into a descriptive error, otherwise deserialize.
    async fn json_or_error<T: for<'de> Deserialize<'de>>(resp: reqwest::Response) -> Result<T> {
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(anyhow!(
                "unauthorized — session token is missing or expired"
            ));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("server returned {status}: {body}"));
        }
        resp.json::<T>()
            .await
            .context("failed to decode server response")
    }

    /// GET /api/me — current user profile.
    pub async fn get_me(&self) -> Result<UserProfile> {
        let resp = self
            .bearer(self.http.get(format!("{}/me", self.base_url)))?
            .send()
            .await
            .context("GET /me request failed")?;
        Self::json_or_error(resp).await
    }

    /// GET /api/machines — list accessible machines.
    pub async fn get_machines(&self) -> Result<Vec<Machine>> {
        let resp = self
            .bearer(self.http.get(format!("{}/machines", self.base_url)))?
            .send()
            .await
            .context("GET /machines request failed")?;
        let wrapper: MachinesResponse = Self::json_or_error(resp).await?;
        Ok(wrapper.machines)
    }

    /// POST /api/machines/:id/connect — request a connection, provisioning a session.
    pub async fn connect_machine(&self, machine_id: &str) -> Result<ConnectionResponse> {
        let resp = self
            .bearer(
                self.http
                    .post(format!("{}/machines/{machine_id}/connect", self.base_url)),
            )?
            .send()
            .await
            .context("POST /machines/:id/connect request failed")?;
        Self::json_or_error(resp).await
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct UserProfile {
    pub id: String,
    pub email: String,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    #[serde(default)]
    pub organizations: Vec<Organization>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Organization {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub role: String,
}

/// The server wraps the machine list in `{ "machines": [...] }`.
#[derive(Debug, Deserialize)]
struct MachinesResponse {
    machines: Vec<Machine>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Machine {
    pub id: String,
    pub name: String,
    pub hostname: String,
    pub platform: String,
    pub online: bool,
    #[serde(default)]
    pub tailscale_ip: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ConnectionResponse {
    pub session_id: String,
    pub status: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::Path,
        http::HeaderMap,
        routing::{get, post},
        Json, Router,
    };
    use serde_json::{json, Value};

    /// Spawn a mock server speaking the real server's JSON contract. Every
    /// handler asserts the bearer token arrived.
    async fn spawn_mock_api() -> String {
        fn require_bearer(headers: &HeaderMap) {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            assert_eq!(auth, "Bearer test.jwt.token", "missing/incorrect bearer");
        }

        async fn me(headers: HeaderMap) -> Json<Value> {
            require_bearer(&headers);
            Json(json!({
                "id": "user-1",
                "email": "sarah@example.com",
                "first_name": "Sarah",
                "last_name": "Dev",
                "organizations": [
                    {"id": "org-1", "name": "Acme", "slug": "acme", "role": "admin"}
                ]
            }))
        }

        async fn machines(headers: HeaderMap) -> Json<Value> {
            require_bearer(&headers);
            Json(json!({
                "machines": [
                    {"id": "m-1", "name": "Workstation", "hostname": "host", "platform": "macos", "online": true, "tailscale_ip": "100.64.0.1"},
                    {"id": "m-2", "name": "Laptop", "hostname": "lap", "platform": "windows", "online": false, "tailscale_ip": null}
                ]
            }))
        }

        async fn connect(headers: HeaderMap, Path(id): Path<String>) -> Json<Value> {
            require_bearer(&headers);
            Json(json!({ "session_id": format!("sess-for-{id}"), "status": "pending" }))
        }

        let app = Router::new()
            .route("/api/me", get(me))
            .route("/api/machines", get(machines))
            .route("/api/machines/:id/connect", post(connect));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/api")
    }

    #[tokio::test]
    async fn parses_me_machines_and_connect() {
        let base = spawn_mock_api().await;
        let mut client = ApiClient::new(&base);
        client.set_token("test.jwt.token");

        let me = client.get_me().await.unwrap();
        assert_eq!(me.email, "sarah@example.com");
        assert_eq!(me.organizations[0].slug, "acme");

        let machines = client.get_machines().await.unwrap();
        assert_eq!(machines.len(), 2);
        assert_eq!(machines[0].name, "Workstation");
        assert!(machines[0].online);
        assert_eq!(machines[1].tailscale_ip, None);

        let conn = client.connect_machine("m-1").await.unwrap();
        assert_eq!(conn.session_id, "sess-for-m-1");
        assert_eq!(conn.status, "pending");
    }

    #[tokio::test]
    async fn calls_without_token_error_before_request() {
        let client = ApiClient::new("http://127.0.0.1:1/api");
        let err = client.get_me().await.unwrap_err();
        assert!(err.to_string().contains("not authenticated"));
    }

    #[test]
    fn base_url_trailing_slash_is_normalized() {
        let client = ApiClient::new("http://localhost:8080/api/");
        assert_eq!(client.base_url, "http://localhost:8080/api");
    }
}
