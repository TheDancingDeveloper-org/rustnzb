use super::{
    contract::{ArticleAvailabilityInput, article_availability_digest},
    nntp_failure,
};
use axum::{Json, extract::State};
use nzb_web::{
    error::ApiError,
    nzb_core::nzb_nntp::{NntpConnection, StatPipeline},
    state::AppState,
};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) async fn h_article_availability(
    State(state): State<Arc<AppState>>,
    Json(input): Json<ArticleAvailabilityInput>,
) -> Result<Json<Value>, ApiError> {
    input.validate().map_err(ApiError::bad_request)?;
    let servers = state.queue_manager.get_servers();
    let Some(server) = servers.first() else {
        return Ok(blocked(&input, "nntp_provider_not_configured"));
    };
    let mut connection = NntpConnection::new(format!("availability-{}", input.request_id));
    if let Err(error) = connection.connect(server).await {
        return Ok(blocked(
            &input,
            nntp_failure(&error, "article_availability"),
        ));
    }
    let mut pipeline = StatPipeline::new();
    for message_id in &input.message_ids {
        pipeline.add(message_id.clone());
    }
    let observations = match pipeline.execute(&mut connection).await {
        Ok(observations) => observations,
        Err(error) => {
            let _ = connection.quit().await;
            return Ok(blocked(
                &input,
                nntp_failure(&error, "article_availability"),
            ));
        }
    };
    let _ = connection.quit().await;
    let available_segment_count = observations.iter().filter(|item| item.exists).count();
    let unavailable_segment_count = observations.len().saturating_sub(available_segment_count);
    Ok(Json(json!({
        "status": "complete",
        "operation": "article_availability",
        "request_id": input.request_id,
        "sample_sha256": input.sample_sha256,
        "sampled_segment_count": observations.len(),
        "available_segment_count": available_segment_count,
        "unavailable_segment_count": unavailable_segment_count
    })))
}

fn blocked(input: &ArticleAvailabilityInput, failure_code: &str) -> Json<Value> {
    Json(json!({
        "status": "blocked",
        "operation": "article_availability",
        "request_id": input.request_id,
        "sample_sha256": article_availability_digest(&input.message_ids),
        "failure_code": failure_code
    }))
}
