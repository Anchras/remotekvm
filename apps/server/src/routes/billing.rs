//! Stripe billing: Checkout session creation and webhook handling.
//!
//! Stripe is optional — if no secret key is configured these endpoints return
//! `400`. The Stripe API base is configurable (`STRIPE_API_BASE`) so the flow
//! can be integration-tested against a local mock.

use axum::{
    body::Bytes,
    extract::{Extension, Json as ExtractJson, State},
    http::HeaderMap,
    response::Json,
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use std::sync::Arc;

use crate::auth::Claims;
use crate::error::ApiError;
use crate::state::AppState;

/// Reject webhook events whose timestamp is older than this (replay guard).
const WEBHOOK_TOLERANCE_SECS: i64 = 300;

/// `POST /api/billing/checkout` — create a Stripe Checkout session for the
/// current user and return its hosted URL.
pub async fn create_checkout(
    Extension(claims): Extension<Claims>,
    State(state): State<Arc<AppState>>,
    maybe_req: Option<ExtractJson<CheckoutRequest>>,
) -> Result<Json<CheckoutResponse>, ApiError> {
    let cfg = &state.config;
    if cfg.stripe_secret_key.is_empty() || cfg.stripe_price_id.is_empty() {
        return Err(ApiError::BadRequest(
            "Billing is not configured".to_string(),
        ));
    }
    let user_id = claims
        .user_id()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let req = maybe_req.map(|ExtractJson(req)| req).unwrap_or_default();
    let target = CheckoutTarget::load(&state, user_id, req.organization_id).await?;

    // Stripe's API is form-encoded. We tie the session to our user via
    // `client_reference_id` so the webhook can attribute it back.
    let mut params = vec![
        ("mode", "subscription".to_string()),
        ("line_items[0][price]", cfg.stripe_price_id.clone()),
        ("line_items[0][quantity]", target.quantity.to_string()),
        ("client_reference_id", user_id.to_string()),
        (
            "success_url",
            format!("{}/billing/success", cfg.public_base_url),
        ),
        (
            "cancel_url",
            format!("{}/billing/cancel", cfg.public_base_url),
        ),
    ];
    if let Some(org_id) = target.organization_id {
        params.push(("metadata[organization_id]", org_id.to_string()));
        params.push((
            "subscription_data[metadata][organization_id]",
            org_id.to_string(),
        ));
    }

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/checkout/sessions", cfg.stripe_api_base))
        .bearer_auth(&cfg.stripe_secret_key)
        .form(&params)
        .send()
        .await
        .map_err(|e| ApiError::Internal(format!("Stripe request failed: {e}")))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ApiError::Internal(format!("Stripe error: {body}")));
    }

    let session: Value = resp
        .json()
        .await
        .map_err(|e| ApiError::Internal(format!("invalid Stripe response: {e}")))?;
    let url = session["url"]
        .as_str()
        .ok_or_else(|| ApiError::Internal("Stripe response missing url".to_string()))?
        .to_string();

    Ok(Json(CheckoutResponse { url }))
}

/// `GET /api/billing/usage` — current-month usage for the user and orgs.
pub async fn usage(
    Extension(claims): Extension<Claims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<UsageResponse>, ApiError> {
    let user_id = claims
        .user_id()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let personal = sqlx::query_as::<_, UsageRow>(
        r#"
        SELECT
            COALESCE(SUM(EXTRACT(EPOCH FROM (COALESCE(s.ended_at, NOW()) - s.started_at)))::BIGINT, 0) AS connection_seconds,
            COALESCE(SUM(s.bytes_sent), 0)::BIGINT AS bytes_sent,
            COALESCE(SUM(s.bytes_received), 0)::BIGINT AS bytes_received
        FROM sessions s
        JOIN machines m ON m.id = s.machine_id
        WHERE m.user_id = $1
          AND m.organization_id IS NULL
          AND s.started_at >= date_trunc('month', NOW())
        "#,
    )
    .bind(user_id)
    .fetch_one(state.db.pool())
    .await?;

    let orgs = sqlx::query_as::<_, OrgUsageRow>(
        r#"
        SELECT
            o.id AS organization_id,
            o.name AS organization_name,
            o.plan,
            o.seat_count,
            COALESCE(SUM(EXTRACT(EPOCH FROM (COALESCE(s.ended_at, NOW()) - s.started_at)))::BIGINT, 0) AS connection_seconds,
            COALESCE(SUM(s.bytes_sent), 0)::BIGINT AS bytes_sent,
            COALESCE(SUM(s.bytes_received), 0)::BIGINT AS bytes_received
        FROM organizations o
        JOIN organization_members om ON om.organization_id = o.id
        LEFT JOIN machines m ON m.organization_id = o.id
        LEFT JOIN sessions s ON s.machine_id = m.id AND s.started_at >= date_trunc('month', NOW())
        WHERE om.user_id = $1
        GROUP BY o.id, o.name, o.plan, o.seat_count
        ORDER BY o.name ASC
        "#,
    )
    .bind(user_id)
    .fetch_all(state.db.pool())
    .await?;

    Ok(Json(UsageResponse {
        personal: UsageBucket::from_row("personal".to_string(), "free".to_string(), 1, personal),
        organizations: orgs
            .into_iter()
            .map(|row| {
                UsageBucket::from_row(
                    row.organization_id.to_string(),
                    row.plan,
                    row.seat_count,
                    UsageRow {
                        connection_seconds: row.connection_seconds,
                        bytes_sent: row.bytes_sent,
                        bytes_received: row.bytes_received,
                    },
                )
                .with_name(row.organization_name)
            })
            .collect(),
    }))
}

/// `POST /webhooks/stripe` — verify the signature and apply subscription events.
pub async fn stripe_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let secret = &state.config.stripe_webhook_secret;
    if secret.is_empty() {
        return Err(ApiError::BadRequest(
            "Billing is not configured".to_string(),
        ));
    }

    let signature = headers
        .get("Stripe-Signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::BadRequest("Missing Stripe-Signature".to_string()))?;

    verify_signature(&body, signature, secret, now_unix())?;

    let event: Value = serde_json::from_slice(&body)
        .map_err(|e| ApiError::BadRequest(format!("bad JSON: {e}")))?;
    let event_type = event["type"].as_str().unwrap_or_default();
    let object = &event["data"]["object"];

    match event_type {
        // The user completed checkout: persist their Stripe customer id.
        "checkout.session.completed" => {
            let customer = object["customer"].as_str();
            let subscription = object["subscription"].as_str();
            let org_id = object["metadata"]["organization_id"]
                .as_str()
                .and_then(|id| uuid::Uuid::parse_str(id).ok());
            let user_id = object["client_reference_id"]
                .as_str()
                .and_then(|id| uuid::Uuid::parse_str(id).ok());

            if let (Some(org_id), Some(customer)) = (org_id, customer) {
                if let Some(uid) = user_id {
                    let can_admin_org: bool = sqlx::query_scalar(
                        r#"
                        SELECT EXISTS(
                            SELECT 1
                            FROM organization_members
                            WHERE organization_id = $1
                              AND user_id = $2
                              AND role IN ('owner', 'admin')
                        )
                        "#,
                    )
                    .bind(org_id)
                    .bind(uid)
                    .fetch_one(state.db.pool())
                    .await?;

                    if can_admin_org {
                        sqlx::query(
                            r#"
                            UPDATE organizations
                            SET stripe_customer_id = $1,
                                stripe_subscription_id = COALESCE($2, stripe_subscription_id),
                                plan = 'team'
                            WHERE id = $3
                            "#,
                        )
                        .bind(customer)
                        .bind(subscription)
                        .bind(org_id)
                        .execute(state.db.pool())
                        .await?;
                    } else {
                        tracing::warn!(
                            user_id = %uid,
                            organization_id = %org_id,
                            "ignoring checkout completion for org without admin membership"
                        );
                    }
                } else {
                    tracing::warn!(
                        organization_id = %org_id,
                        "ignoring organization checkout completion without valid client reference"
                    );
                }
            } else if let (Some(uid), Some(customer)) = (user_id, customer) {
                sqlx::query("UPDATE users SET stripe_customer_id = $1 WHERE id = $2")
                    .bind(customer)
                    .bind(uid)
                    .execute(state.db.pool())
                    .await?;
            }
        }
        "customer.subscription.deleted" => {
            if let Some(org_id) = object["metadata"]["organization_id"]
                .as_str()
                .and_then(|id| uuid::Uuid::parse_str(id).ok())
            {
                sqlx::query(
                    "UPDATE organizations SET stripe_subscription_id = NULL, plan = 'free' WHERE id = $1",
                )
                .bind(org_id)
                .execute(state.db.pool())
                .await?;
            }
        }
        other => tracing::debug!(event = other, "unhandled Stripe event"),
    }

    Ok(Json(serde_json::json!({ "received": true })))
}

/// Verify a Stripe webhook signature (`t=...,v1=...`) over `{t}.{body}`.
fn verify_signature(body: &[u8], header: &str, secret: &str, now: i64) -> Result<(), ApiError> {
    let mut timestamp = None;
    // A header may carry multiple `v1` signatures during webhook-secret
    // rotation (`t=..,v1=<old>,v1=<new>`); accept if *any* matches.
    let mut v1_sigs: Vec<&str> = Vec::new();
    for part in header.split(',') {
        if let Some((k, v)) = part.split_once('=') {
            match k {
                "t" => timestamp = v.parse::<i64>().ok(),
                "v1" => v1_sigs.push(v),
                _ => {}
            }
        }
    }
    let timestamp = timestamp.ok_or_else(|| ApiError::BadRequest("bad signature".to_string()))?;
    if v1_sigs.is_empty() {
        return Err(ApiError::BadRequest("bad signature".to_string()));
    }

    // saturating arithmetic so an adversarial timestamp can't overflow/panic.
    if now.saturating_sub(timestamp).saturating_abs() > WEBHOOK_TOLERANCE_SECS {
        return Err(ApiError::BadRequest(
            "signature timestamp too old".to_string(),
        ));
    }

    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| ApiError::Internal("invalid webhook secret".to_string()))?;
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    let expected = hex::encode(mac.finalize().into_bytes());

    let matches = v1_sigs
        .iter()
        .any(|sig| bool::from(constant_eq(expected.as_bytes(), sig.as_bytes())));
    if !matches {
        return Err(ApiError::BadRequest("signature mismatch".to_string()));
    }
    Ok(())
}

fn constant_eq(a: &[u8], b: &[u8]) -> subtle_eq::Choice {
    subtle_eq::ct_eq(a, b)
}

/// Tiny constant-time byte comparison (avoids a new dependency).
mod subtle_eq {
    pub struct Choice(u8);
    impl From<Choice> for bool {
        fn from(c: Choice) -> bool {
            c.0 == 1
        }
    }
    pub fn ct_eq(a: &[u8], b: &[u8]) -> Choice {
        if a.len() != b.len() {
            return Choice(0);
        }
        let mut diff = 0u8;
        for (x, y) in a.iter().zip(b.iter()) {
            diff |= x ^ y;
        }
        Choice(u8::from(diff == 0))
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Serialize)]
pub struct CheckoutResponse {
    pub url: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct CheckoutRequest {
    pub organization_id: Option<uuid::Uuid>,
}

struct CheckoutTarget {
    organization_id: Option<uuid::Uuid>,
    quantity: i64,
}

impl CheckoutTarget {
    async fn load(
        state: &AppState,
        user_id: uuid::Uuid,
        organization_id: Option<uuid::Uuid>,
    ) -> Result<Self, ApiError> {
        let Some(org_id) = organization_id else {
            return Ok(Self {
                organization_id: None,
                quantity: 1,
            });
        };

        let role = sqlx::query_scalar::<_, String>(
            "SELECT role FROM organization_members WHERE organization_id = $1 AND user_id = $2",
        )
        .bind(org_id)
        .bind(user_id)
        .fetch_optional(state.db.pool())
        .await?;
        match role.as_deref() {
            Some("owner") | Some("admin") => {}
            Some(_) => return Err(ApiError::Forbidden),
            None => return Err(ApiError::NotFound("organization not found".to_string())),
        }

        let quantity = sqlx::query_scalar::<_, i64>(
            "SELECT GREATEST(COUNT(*), 1)::BIGINT FROM organization_members WHERE organization_id = $1",
        )
        .bind(org_id)
        .fetch_one(state.db.pool())
        .await?;
        sqlx::query("UPDATE organizations SET seat_count = $1 WHERE id = $2")
            .bind(quantity as i32)
            .bind(org_id)
            .execute(state.db.pool())
            .await?;

        Ok(Self {
            organization_id: Some(org_id),
            quantity,
        })
    }
}

#[derive(Debug, Serialize)]
pub struct UsageResponse {
    pub personal: UsageBucket,
    pub organizations: Vec<UsageBucket>,
}

#[derive(Debug, Serialize)]
pub struct UsageBucket {
    pub id: String,
    pub name: Option<String>,
    pub plan: String,
    pub seat_count: i32,
    pub connection_seconds: i64,
    pub connection_minutes: i64,
    pub bytes_sent: i64,
    pub bytes_received: i64,
}

impl UsageBucket {
    fn from_row(id: String, plan: String, seat_count: i32, row: UsageRow) -> Self {
        Self {
            id,
            name: None,
            plan,
            seat_count,
            connection_seconds: row.connection_seconds,
            connection_minutes: row.connection_seconds.div_euclid(60),
            bytes_sent: row.bytes_sent,
            bytes_received: row.bytes_received,
        }
    }

    fn with_name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }
}

#[derive(sqlx::FromRow)]
struct UsageRow {
    connection_seconds: i64,
    bytes_sent: i64,
    bytes_received: i64,
}

#[derive(sqlx::FromRow)]
struct OrgUsageRow {
    organization_id: uuid::Uuid,
    organization_name: String,
    plan: String,
    seat_count: i32,
    connection_seconds: i64,
    bytes_sent: i64,
    bytes_received: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_verifies_and_rejects_tampering() {
        let secret = "whsec_test";
        let body = br#"{"type":"checkout.session.completed"}"#;
        let t = 1_000_000i64;

        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(t.to_string().as_bytes());
        mac.update(b".");
        mac.update(body);
        let sig = hex::encode(mac.finalize().into_bytes());
        let header = format!("t={t},v1={sig}");

        // Valid (within tolerance of "now" = t).
        assert!(verify_signature(body, &header, secret, t).is_ok());
        // Wrong secret.
        assert!(verify_signature(body, &header, "whsec_other", t).is_err());
        // Tampered body.
        assert!(verify_signature(b"{}", &header, secret, t).is_err());
        // Stale timestamp.
        assert!(verify_signature(body, &header, secret, t + 10_000).is_err());

        // Secret-rotation header with multiple v1s: accept if any matches.
        let rotated = format!("t={t},v1=deadbeef,v1={sig}");
        assert!(verify_signature(body, &rotated, secret, t).is_ok());
        // ...but if none match, reject.
        let none_match = format!("t={t},v1=deadbeef,v1=cafebabe");
        assert!(verify_signature(body, &none_match, secret, t).is_err());

        // Adversarial timestamp must not panic (saturating arithmetic).
        let extreme = format!("t={},v1={sig}", i64::MIN);
        assert!(verify_signature(body, &extreme, secret, i64::MAX).is_err());
    }
}
