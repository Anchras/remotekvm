use anyhow::Result;

/// Default WorkOS API base URL. Overridable via `WORKOS_API_BASE` (used by
/// integration tests to point at a local mock).
pub const DEFAULT_WORKOS_API_BASE: &str = "https://api.workos.com";

/// Default Stripe API base URL. Overridable via `STRIPE_API_BASE` (used by
/// integration tests to point at a local mock).
pub const DEFAULT_STRIPE_API_BASE: &str = "https://api.stripe.com";

#[derive(Clone)]
pub struct Config {
    pub port: u16,
    pub database_url: String,
    pub workos_api_key: String,
    pub workos_client_id: String,
    pub workos_api_base: String,
    /// Public base URL of this server, used to build the WorkOS redirect URI
    /// (`{public_base_url}/auth/workos/callback`). Defaults to
    /// `http://localhost:{port}`.
    pub public_base_url: String,
    pub jwt_secret: String,
    pub jwt_expiry_hours: i64,
    /// Optional Redis URL used for cross-instance ephemeral signaling metadata.
    /// WebSocket senders remain process-local; Redis stores presence and routing
    /// hints with TTLs for deployments that need instance affinity.
    pub redis_url: Option<String>,
    pub signaling_instance_id: String,
    pub signaling_ttl_seconds: u64,
    // --- Stripe billing (optional; endpoints 400 if the key is unset) ---
    pub stripe_secret_key: String,
    pub stripe_webhook_secret: String,
    pub stripe_price_id: String,
    pub stripe_api_base: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let port = std::env::var("PORT")
            .unwrap_or_else(|_| "8080".to_string())
            .parse()?;

        let database_url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://postgres:postgres@localhost/remotekvm".to_string());

        let workos_api_key = std::env::var("WORKOS_API_KEY")
            .map_err(|_| anyhow::anyhow!("WORKOS_API_KEY must be set"))?;
        if workos_api_key.is_empty() {
            anyhow::bail!("WORKOS_API_KEY must not be empty");
        }

        let workos_client_id = std::env::var("WORKOS_CLIENT_ID")
            .map_err(|_| anyhow::anyhow!("WORKOS_CLIENT_ID must be set"))?;
        if workos_client_id.is_empty() {
            anyhow::bail!("WORKOS_CLIENT_ID must not be empty");
        }

        let jwt_secret =
            std::env::var("JWT_SECRET").map_err(|_| anyhow::anyhow!("JWT_SECRET must be set"))?;
        if jwt_secret.len() < 32 {
            anyhow::bail!("JWT_SECRET must be at least 32 bytes");
        }

        let jwt_expiry_hours = std::env::var("JWT_EXPIRY_HOURS")
            .unwrap_or_else(|_| "24".to_string())
            .parse()?;

        let workos_api_base = std::env::var("WORKOS_API_BASE")
            .unwrap_or_else(|_| DEFAULT_WORKOS_API_BASE.to_string());

        let public_base_url =
            std::env::var("PUBLIC_BASE_URL").unwrap_or_else(|_| format!("http://localhost:{port}"));

        let redis_url = std::env::var("REDIS_URL")
            .ok()
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty());

        let signaling_instance_id = std::env::var("SIGNALING_INSTANCE_ID")
            .ok()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| format!("server-{}", uuid::Uuid::new_v4()));

        let signaling_ttl_seconds = std::env::var("SIGNALING_TTL_SECONDS")
            .unwrap_or_else(|_| "90".to_string())
            .parse()?;
        if signaling_ttl_seconds == 0 {
            anyhow::bail!("SIGNALING_TTL_SECONDS must be greater than 0");
        }

        // Stripe is optional: billing endpoints return an error if unconfigured.
        let stripe_secret_key = std::env::var("STRIPE_SECRET_KEY").unwrap_or_default();
        let stripe_webhook_secret = std::env::var("STRIPE_WEBHOOK_SECRET").unwrap_or_default();
        let stripe_price_id = std::env::var("STRIPE_PRICE_ID").unwrap_or_default();
        let stripe_api_base = std::env::var("STRIPE_API_BASE")
            .unwrap_or_else(|_| DEFAULT_STRIPE_API_BASE.to_string());

        Ok(Config {
            port,
            database_url,
            workos_api_key,
            workos_client_id,
            workos_api_base,
            public_base_url,
            jwt_secret,
            jwt_expiry_hours,
            redis_url,
            signaling_instance_id,
            signaling_ttl_seconds,
            stripe_secret_key,
            stripe_webhook_secret,
            stripe_price_id,
            stripe_api_base,
        })
    }
}
