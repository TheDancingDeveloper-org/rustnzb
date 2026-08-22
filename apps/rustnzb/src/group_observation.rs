use std::{collections::BTreeSet, sync::Arc};

use axum::{Json, extract::State};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use nzb_web::{
    error::ApiError,
    nzb_core::nzb_nntp::{
        ArticleRange, DefectiveOverviewRow, LosslessOverviewRow, NntpConnection, NntpError,
        OverviewFormat,
    },
    state::AppState,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

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

fn missing_ranges(start: u64, end: u64, present: &BTreeSet<u64>) -> Vec<(u64, u64)> {
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

fn digest_parts<'a>(prefix: &[u8], parts: impl IntoIterator<Item = &'a [u8]>) -> String {
    let mut digest = Sha256::new();
    digest.update(prefix);
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    format!("{:x}", digest.finalize())
}

fn format_digest(format: &OverviewFormat) -> String {
    digest_parts(b"overview-format", format.fields.iter().map(Vec::as_slice))
}

fn row_json(row: LosslessOverviewRow, format_digest: &str) -> Value {
    let article = row.article_number.to_be_bytes();
    let digest = digest_parts(
        b"overview-row",
        std::iter::once(article.as_slice())
            .chain(std::iter::once(format_digest.as_bytes()))
            .chain(row.fields.iter().map(Vec::as_slice)),
    );
    json!({
        "article_number": row.article_number,
        "fields_base64": row.fields.into_iter().map(|field| BASE64.encode(field)).collect::<Vec<_>>(),
        "row_sha256": digest
    })
}

fn defective_row_json(row: DefectiveOverviewRow) -> Value {
    let digest = digest_parts(b"defective-overview-row", [row.wire_line.as_slice()]);
    json!({
        "article_number": row.article_number,
        "wire_row_base64": BASE64.encode(row.wire_line),
        "raw_sha256": digest,
        "failure_code": row.failure_code.as_str()
    })
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
    let format = match connection.overview_format().await {
        Ok(format) => format,
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
    let overview = match connection
        .xover_lossless(input.start_article, input.end_article, &format)
        .await
    {
        Ok(overview) => overview,
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
    let returned = overview.rows.len() + overview.defective_rows.len();
    if returned as u64 > input.max_headers {
        return Ok(blocked(
            "overview_range",
            &input.request_id,
            &input.group,
            "nntp_overview_header_limit_exceeded",
        ));
    }
    let present = overview
        .rows
        .iter()
        .map(|row| row.article_number)
        .chain(
            overview
                .defective_rows
                .iter()
                .filter_map(|row| row.article_number),
        )
        .collect::<BTreeSet<_>>();
    let missing = missing_ranges(input.start_article, input.end_article, &present);
    let format_digest = format_digest(&format);
    let valid = overview.rows.len();
    let defective = overview.defective_rows.len();
    let format_fields = format
        .fields
        .into_iter()
        .map(|field| BASE64.encode(field))
        .collect::<Vec<_>>();
    let rows = overview
        .rows
        .into_iter()
        .map(|row| row_json(row, &format_digest))
        .collect::<Vec<_>>();
    let defective_rows = overview
        .defective_rows
        .into_iter()
        .map(defective_row_json)
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "status": "complete",
        "operation": "overview_range",
        "request_id": input.request_id,
        "group": group.name,
        "group_first_article": group.first,
        "group_last_article": group.last,
        "requested_start_article": input.start_article,
        "requested_end_article": input.end_article,
        "returned_row_count": returned,
        "valid_row_count": valid,
        "defective_row_count": defective,
        "missing_ranges": missing,
        "overview_format": {
            "fields_base64": format_fields,
            "sha256": format_digest
        },
        "rows": rows,
        "defective_rows": defective_rows
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
