use anyhow::Result;
use sqlx::postgres::PgPoolOptions;
use sqlx::{migrate::MigrateDatabase, PgPool};
use tracing::info;

#[derive(Clone)]
pub struct Database {
    pool: PgPool,
}

impl Database {
    pub async fn connect(url: &str) -> Result<Self> {
        // Create database if it doesn't exist
        if !sqlx::Postgres::database_exists(url).await.unwrap_or(false) {
            info!("creating database");
            sqlx::Postgres::create_database(url).await?;
        }

        let pool = PgPoolOptions::new()
            .max_connections(20)
            .connect(url)
            .await?;

        Ok(Database { pool })
    }

    /// Build a `Database` from an already-established pool.
    ///
    /// Used by integration tests, which obtain an isolated pool from
    /// `#[sqlx::test]` rather than connecting by URL.
    pub fn from_pool(pool: PgPool) -> Self {
        Database { pool }
    }

    pub async fn run_migrations(&self) -> Result<()> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        Ok(())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}
