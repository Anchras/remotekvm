use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone)]
/// WorkOS API client.
pub struct WorkOsClient {
    api_key: String,
    client_id: String,
    api_base: String,
    http: reqwest::Client,
}

impl WorkOsClient {
    /// Create a client. `api_base` is the WorkOS API root (e.g.
    /// `https://api.workos.com`); integration tests point this at a local mock.
    pub fn new(api_key: String, client_id: String, api_base: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            api_key,
            client_id,
            api_base,
            http,
        }
    }

    /// Exchange an authorization code for a user profile.
    pub async fn authenticate_with_code(&self, code: &str) -> Result<WorkOsUserProfile> {
        let url = format!("{}/user_management/authenticate", self.api_base);

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&serde_json::json!({
                "client_id": self.client_id,
                "code": code,
                "grant_type": "authorization_code"
            }))
            .send()
            .await
            .context("Failed to send WorkOS authentication request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("WorkOS authentication failed: {} - {}", status, body);
        }

        let auth_response: WorkOsAuthResponse = response
            .json()
            .await
            .context("Failed to parse WorkOS authentication response")?;

        Ok(auth_response.user)
    }
}

#[derive(Debug, Deserialize)]
pub struct WorkOsAuthResponse {
    pub user: WorkOsUserProfile,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WorkOsUserProfile {
    pub id: String,
    pub email: String,
    #[serde(rename = "firstName")]
    pub first_name: Option<String>,
    #[serde(rename = "lastName")]
    pub last_name: Option<String>,
    #[serde(rename = "profilePictureUrl")]
    pub profile_picture_url: Option<String>,
    #[serde(default)]
    pub organizations: Vec<WorkOsOrganizationMembership>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WorkOsOrganizationMembership {
    pub organization: WorkOsOrganization,
    #[serde(default)]
    pub role: Option<WorkOsRole>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WorkOsOrganization {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WorkOsRole {
    pub slug: String,
}
