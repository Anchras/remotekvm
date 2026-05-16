use anyhow::Result;
use serde::{Deserialize, Serialize};

/// REST API client for the RemoteKVM SaaS server.
pub struct ApiClient {
    base_url: String,
    token: Option<String>,
}

impl ApiClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.to_string(),
            token: None,
        }
    }

    pub fn set_token(&mut self, token: &str) {
        self.token = Some(token.to_string());
    }

    /// GET /api/me — Get current user profile.
    pub async fn get_me(&self) -> Result<UserProfile> {
        // TODO: Implement HTTP GET with reqwest
        Ok(UserProfile {
            id: "user_123".to_string(),
            email: "user@example.com".to_string(),
            first_name: Some("Test".to_string()),
            last_name: Some("User".to_string()),
            organizations: vec![],
        })
    }

    /// GET /api/machines — List accessible machines.
    pub async fn get_machines(&self) -> Result<Vec<Machine>> {
        // TODO: Implement HTTP GET with reqwest
        Ok(vec![])
    }

    /// POST /api/machines/:id/connect — Request connection.
    pub async fn connect_machine(&self, _machine_id: &str) -> Result<ConnectionResponse> {
        // TODO: Implement HTTP POST with reqwest
        Ok(ConnectionResponse {
            session_id: "session_123".to_string(),
            status: "pending".to_string(),
        })
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct UserProfile {
    pub id: String,
    pub email: String,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub organizations: Vec<Organization>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Organization {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub role: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Machine {
    pub id: String,
    pub name: String,
    pub hostname: String,
    pub platform: String,
    pub online: bool,
    pub tailscale_ip: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ConnectionResponse {
    pub session_id: String,
    pub status: String,
}
