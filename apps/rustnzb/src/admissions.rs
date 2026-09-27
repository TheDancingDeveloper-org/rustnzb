use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use sha2::{Digest, Sha256};

use nzb_web::error::ApiError;
use nzb_web::nzb_core::models::QueueAdmissionObservation;
use nzb_web::state::AppState;

pub const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;

#[derive(Debug, Clone)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    pub fn from_headers(headers: &HeaderMap) -> Result<Option<Self>, ApiError> {
        let mut values = headers.get_all(IDEMPOTENCY_KEY_HEADER).iter();
        let Some(value) = values.next() else {
            return Ok(None);
        };
        if values.next().is_some() {
            return Err(ApiError::bad_request(
                "Idempotency-Key must be supplied exactly once",
            ));
        }
        let value = value
            .to_str()
            .map_err(|_| ApiError::bad_request("Idempotency-Key is not valid ASCII"))?;
        Self::parse(value).map(Some)
    }

    pub fn parse(value: &str) -> Result<Self, ApiError> {
        if value.is_empty()
            || value.len() > MAX_IDEMPOTENCY_KEY_BYTES
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b':')
            })
        {
            return Err(ApiError::bad_request("Idempotency-Key is invalid"));
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub fn payload_digest(payload: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(payload)))
}

/// GET /api/queue/admissions/{idempotency_key} -- Resolve one exact admission.
pub async fn h_queue_admission_get(
    State(state): State<Arc<AppState>>,
    Path(idempotency_key): Path<String>,
) -> Result<Json<QueueAdmissionObservation>, ApiError> {
    let idempotency_key = IdempotencyKey::parse(&idempotency_key)?;
    let observation = state
        .queue_manager
        .queue_admission_observe(idempotency_key.as_str())
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found("Queue admission not found"))?;
    Ok(Json(observation))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_bounded_and_path_safe() {
        assert!(IdempotencyKey::parse("acquisition:018f-abc_DEF.2").is_ok());
        assert!(IdempotencyKey::parse("").is_err());
        assert!(IdempotencyKey::parse("contains/slash").is_err());
        assert!(IdempotencyKey::parse(&"a".repeat(129)).is_err());
    }

    #[test]
    fn digest_binds_exact_payload_bytes() {
        assert_eq!(
            payload_digest(b"nzb"),
            "sha256:5099941fc6e5440244a41b3f6e466d8933f73ea1647f042bf051b380a43acdcc"
        );
        assert_ne!(payload_digest(b"nzb"), payload_digest(b"NZB"));
    }
}
