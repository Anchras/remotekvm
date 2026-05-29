//! Stripe billing: Checkout session creation and webhook handling.
//!
//! Stripe is optional — if no secret key is configured these endpoints return
//! `400`. The Stripe API base is configurable (`STRIPE_API_BASE`) so the flow
//! can be integration-tested against a local mock.

use axum::{
    body::Bytes,
    extract::{Extension, State},
    http::HeaderMap,
    response::Json,
};
use hmac::{Hmac, Mac};
use serde::Serialize;
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

    // Stripe's API is form-encoded. We tie the session to our user via
    // `client_reference_id` so the webhook can attribute it back.
    let params = [
        ("mode", "subscription".to_string()),
        ("line_items[0][price]", cfg.stripe_price_id.clone()),
        ("line_items[0][quantity]", "1".to_string()),
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
            if let (Some(user_id), Some(customer)) = (
                object["client_reference_id"].as_str(),
                object["customer"].as_str(),
            ) {
                if let Ok(uid) = uuid::Uuid::parse_str(user_id) {
                    sqlx::query("UPDATE users SET stripe_customer_id = $1 WHERE id = $2")
                        .bind(customer)
                        .bind(uid)
                        .execute(state.db.pool())
                        .await?;
                }
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
