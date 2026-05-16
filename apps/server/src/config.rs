use anyhow::Result;

#[derive(Clone)]
pub struct Config {
    pub port: u16,
    pub database_url: String,
    pub workos_api_key: String,
    pub workos_client_id: String,
    pub jwt_secret: String,
    pub jwt_expiry_hours: i64,
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

        let jwt_secret = std::env::var("JWT_SECRET")
            .map_err(|_| anyhow::anyhow!("JWT_SECRET must be set"))?;
        if jwt_secret.len() < 32 {
            anyhow::bail!("JWT_SECRET must be at least 32 bytes");
        }

        let jwt_expiry_hours = std::env::var("JWT_EXPIRY_HOURS")
            .unwrap_or_else(|_| "24".to_string())
            .parse()?;

        Ok(Config {
            port,
            database_url,
            workos_api_key,
            workos_client_id,
            jwt_secret,
            jwt_expiry_hours,
        })
    }
}
