use std::{collections::BTreeSet, sync::Arc};

use axum::{Json, extract::State};
use nzb_web::{
    error::ApiError,
    nzb_core::nzb_nntp::{ArticleRange, NntpConnection, NntpError, XoverEntry},
    state::AppState,
};
use serde_json::{Value, json};

mod contract;

use contract::{HeaderPatternInput, OverviewRangeInput};

fn blocked(operation: &str, request_id: &str, group: &str, failure_code: &str) -> Json<Value> {
    Json(json!({
        "status": "blocked",
        "operation": operation,
        "request_id": request_id,
        "group": group,
        "failure_code": failure_code
    }))
}

fn nntp_failure(error: &NntpError, operation: &str) -> &'static str {
    match error {
        NntpError::Auth(_) | NntpError::AuthRequired(_) => "nntp_authentication_failed",
        NntpError::PermissionDenied(_) => "nntp_operation_not_authorized",
        NntpError::NoSuchGroup(_) => "nntp_group_unavailable",
        NntpError::ServiceUnavailable(_) => "nntp_service_unavailable",
        NntpError::Timeout(_) => "nntp_operation_timed_out",
        NntpError::Connection(_) | NntpError::Io(_) | NntpError::Tls(_) => {
            "nntp_transport_unavailable"
        }
        NntpError::Protocol(_) if operation == "header_pattern" => {
            "nntp_header_pattern_unavailable"
        }
        NntpError::Protocol(_) => "nntp_overview_unavailable",
        _ => "nntp_operation_failed",
    }
}

fn missing_ranges(start: u64, end: u64, entries: &[XoverEntry]) -> Vec<(u64, u64)> {
    let present = entries
        .iter()
        .map(|entry| entry.article_num)
        .collect::<BTreeSet<_>>();
    let mut ranges = Vec::new();
    let mut missing_start = None;
    for article in start..=end {
        if present.contains(&article) {
            if let Some(first) = missing_start.take() {
                ranges.push((first, article - 1));
            }
        } else if missing_start.is_none() {
            missing_start = Some(article);
        }
    }
    if let Some(first) = missing_start {
        ranges.push((first, end));
    }
    ranges
}

pub(crate) async fn h_overview_range(
    State(state): State<Arc<AppState>>,
    Json(input): Json<OverviewRangeInput>,
) -> Result<Json<Value>, ApiError> {
    input.validate().map_err(ApiError::bad_request)?;
    let servers = state.queue_manager.get_servers();
    let Some(server) = servers.first() else {
        return Ok(blocked(
            "overview_range",
            &input.request_id,
            &input.group,
            "nntp_provider_not_configured",
        ));
    };
    let mut connection = NntpConnection::new(format!("overview-{}", input.request_id));
    if let Err(error) = connection.connect(server).await {
        return Ok(blocked(
            "overview_range",
            &input.request_id,
            &input.group,
            nntp_failure(&error, "overview_range"),
        ));
    }
    let group = match connection.group(&input.group).await {
        Ok(group) => group,
        Err(error) => {
            let _ = connection.quit().await;
            return Ok(blocked(
                "overview_range",
                &input.request_id,
                &input.group,
                nntp_failure(&error, "overview_range"),
            ));
        }
    };
    if group.name != input.group {
        let _ = connection.quit().await;
        return Ok(blocked(
            "overview_range",
            &input.request_id,
            &input.group,
            "nntp_group_binding_invalid",
        ));
    }
    let headers = match connection
        .xover(input.start_article, input.end_article)
        .await
    {
        Ok(headers) => headers,
        Err(error) => {
            let _ = connection.quit().await;
            return Ok(blocked(
                "overview_range",
                &input.request_id,
                &input.group,
                nntp_failure(&error, "overview_range"),
            ));
        }
    };
    let _ = connection.quit().await;
    let missing = missing_ranges(input.start_article, input.end_article, &headers);
    let returned = headers.len();
    Ok(Json(json!({
        "status": "complete",
        "operation": "overview_range",
        "request_id": input.request_id,
        "group": group.name,
        "group_first_article": group.first,
        "group_last_article": group.last,
        "requested_start_article": input.start_article,
        "requested_end_article": input.end_article,
        "returned_header_count": returned,
        "missing_ranges": missing,
        "headers": headers.into_iter().map(|header| json!({
            "article_number": header.article_num,
            "subject": header.subject,
            "author": header.from,
            "date": header.date,
            "message_id": header.message_id,
            "references": header.references,
            "bytes": header.bytes,
            "lines": header.lines
        })).collect::<Vec<_>>()
    })))
}

pub(crate) async fn h_header_pattern(
    State(state): State<Arc<AppState>>,
    Json(input): Json<HeaderPatternInput>,
) -> Result<Json<Value>, ApiError> {
    input.validate().map_err(ApiError::bad_request)?;
    let servers = state.queue_manager.get_servers();
    let Some(server) = servers.first() else {
        return Ok(blocked(
            "header_pattern",
            &input.request_id,
            &input.group,
            "nntp_provider_not_configured",
        ));
    };
    let mut connection = NntpConnection::new(format!("pattern-{}", input.request_id));
    if let Err(error) = connection.connect(server).await {
        return Ok(blocked(
            "header_pattern",
            &input.request_id,
            &input.group,
            nntp_failure(&error, "header_pattern"),
        ));
    }
    let group = match connection.group(&input.group).await {
        Ok(group) => group,
        Err(error) => {
            let _ = connection.quit().await;
            return Ok(blocked(
                "header_pattern",
                &input.request_id,
                &input.group,
                nntp_failure(&error, "header_pattern"),
            ));
        }
    };
    if group.name != input.group {
        let _ = connection.quit().await;
        return Ok(blocked(
            "header_pattern",
            &input.request_id,
            &input.group,
            "nntp_group_binding_invalid",
        ));
    }
    let patterns = input
        .patterns
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let matches = match connection
        .xpat(
            "Subject",
            ArticleRange::Range(input.start_article, input.end_article),
            &patterns,
        )
        .await
    {
        Ok(matches) => matches,
        Err(error) => {
            let _ = connection.quit().await;
            return Ok(blocked(
                "header_pattern",
                &input.request_id,
                &input.group,
                nntp_failure(&error, "header_pattern"),
            ));
        }
    };
    let _ = connection.quit().await;
    if matches.len() > input.max_matches {
        return Ok(blocked(
            "header_pattern",
            &input.request_id,
            &input.group,
            "nntp_header_pattern_match_limit_exceeded",
        ));
    }
    let matched = matches.len();
    Ok(Json(json!({
        "status": "complete",
        "operation": "header_pattern",
        "request_id": input.request_id,
        "group": group.name,
        "group_first_article": group.first,
        "group_last_article": group.last,
        "requested_start_article": input.start_article,
        "requested_end_article": input.end_article,
        "match_count": matched,
        "matches": matches.into_iter().map(|header| json!({
            "article_number": header.article_num,
            "value": header.value
        })).collect::<Vec<_>>()
    })))
}

#[cfg(test)]
mod tests;
