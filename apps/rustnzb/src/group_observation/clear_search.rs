use super::{defective_row_json, format_digest, missing_ranges, nntp_failure, row_json};
use crate::group_observation::contract::{ClearSearchInput, ClearSearchRangeInput, now_unix_ms};
use axum::{Json, extract::State};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use nzb_web::{
    error::ApiError,
    nzb_core::nzb_nntp::{
        ArticleRange, HeaderEntry, LosslessOverviewRows, NntpConnection, NntpError, OverviewFormat,
    },
    state::AppState,
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, future::Future, sync::Arc, time::Duration};
use tokio::time::Instant;

const MAX_COMMAND_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Eq, PartialEq)]
enum AcceleratorState {
    Supported,
    Unsupported,
    Defective,
    Unverified,
}

impl AcceleratorState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Defective => "defective",
            Self::Unverified => "unverified",
        }
    }
}

struct Calibration {
    state: AcceleratorState,
    positive_matches: usize,
    negative_matches: usize,
}

struct CalibrationResult {
    calibration: Calibration,
    stop: Option<Stop>,
}

struct ObservedRange {
    overview: LosslessOverviewRows,
    matches: Vec<HeaderEntry>,
    accelerator_state: AcceleratorState,
    accelerator_failure_code: Option<&'static str>,
    response_bytes: usize,
    elapsed_ms: u64,
}

#[derive(Clone, Copy, Debug)]
struct Stop {
    receipt_state: &'static str,
    failure_code: &'static str,
}

struct Budget {
    maximum: usize,
    used: usize,
}

impl Budget {
    fn new(maximum: usize) -> Self {
        Self { maximum, used: 0 }
    }

    fn command_limit(&self) -> Result<usize, Stop> {
        let remaining = self.maximum.saturating_sub(self.used);
        if remaining == 0 {
            Err(Stop {
                receipt_state: "refused",
                failure_code: "nntp_observation_response_limit_exceeded",
            })
        } else {
            Ok(remaining.min(MAX_COMMAND_RESPONSE_BYTES))
        }
    }

    fn consume(&mut self, bytes: usize) -> Result<(), Stop> {
        self.used = self.used.checked_add(bytes).ok_or(Stop {
            receipt_state: "refused",
            failure_code: "nntp_observation_response_limit_exceeded",
        })?;
        if self.used > self.maximum {
            return Err(Stop {
                receipt_state: "refused",
                failure_code: "nntp_observation_response_limit_exceeded",
            });
        }
        Ok(())
    }
}

struct Deadline {
    at: Instant,
}

impl Deadline {
    fn from_input(input: &ClearSearchInput) -> Result<Self, &'static str> {
        let now = now_unix_ms()?;
        let remaining = input.deadline_at_unix_ms.saturating_sub(now);
        Ok(Self {
            at: Instant::now() + Duration::from_millis(remaining),
        })
    }

    async fn run<F: Future>(&self, future: F) -> Result<F::Output, Stop> {
        tokio::time::timeout_at(self.at, future)
            .await
            .map_err(|_| Stop {
                receipt_state: "cancelled",
                failure_code: "nntp_operation_timed_out",
            })
    }
}

enum XpatOutcome {
    Complete(Vec<HeaderEntry>),
    Unsupported,
    Defective(&'static str),
    Stopped(Stop),
}

pub(crate) async fn h_clear_search(
    State(state): State<Arc<AppState>>,
    Json(input): Json<ClearSearchInput>,
) -> Result<Json<Value>, ApiError> {
    input.validate().map_err(ApiError::bad_request)?;
    let deadline = Deadline::from_input(&input).map_err(ApiError::bad_request)?;
    Ok(Json(observe(state, &input, &deadline).await))
}

async fn observe(state: Arc<AppState>, input: &ClearSearchInput, deadline: &Deadline) -> Value {
    let servers = state.queue_manager.get_servers();
    let Some(server) = servers.first() else {
        return failed_response(
            input,
            Stop {
                receipt_state: "refused",
                failure_code: "nntp_provider_not_configured",
            },
        );
    };
    let mut connection = NntpConnection::new(format!("clear-{}", input.request_id));
    match deadline.run(connection.connect(server)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return failed_response(input, stop_for_error(&error)),
        Err(stop) => return failed_response(input, stop),
    }
    let group = match deadline.run(connection.group(&input.group)).await {
        Ok(Ok(group)) => group,
        Ok(Err(error)) => return failed_response(input, stop_for_error(&error)),
        Err(stop) => return failed_response(input, stop),
    };
    if group.name != input.group
        || group.first == 0
        || group.last < group.first
        || input
            .ranges
            .iter()
            .any(|range| range.start_article < group.first || range.end_article > group.last)
    {
        return failed_response(
            input,
            Stop {
                receipt_state: "refused",
                failure_code: "nntp_group_binding_invalid",
            },
        );
    }
    let format = match deadline.run(connection.overview_format()).await {
        Ok(Ok(format)) => format,
        Ok(Err(error)) => return failed_response(input, stop_for_error(&error)),
        Err(stop) => return failed_response(input, stop),
    };
    let mut budget = Budget::new(input.max_response_bytes);
    let mut calibration = None;
    let mut range_rows = Vec::with_capacity(input.ranges.len());
    let patterns = input
        .patterns
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let mut next_range = input.ranges.len();
    let mut following_stop = None;
    for (index, range) in input.ranges.iter().enumerate() {
        let started = Instant::now();
        let range_bytes_before = budget.used;
        let response_limit = match budget.command_limit() {
            Ok(limit) => limit,
            Err(stop) => {
                next_range = index;
                following_stop = Some(stop);
                break;
            }
        };
        let overview = match deadline
            .run(connection.xover_lossless_bounded(
                range.start_article,
                range.end_article,
                &format,
                response_limit,
            ))
            .await
        {
            Ok(Ok(response)) => {
                if let Err(stop) = budget.consume(response.response_bytes) {
                    next_range = index;
                    following_stop = Some(stop);
                    break;
                }
                response.value
            }
            Ok(Err(error)) => {
                next_range = index;
                following_stop = Some(stop_for_error(&error));
                break;
            }
            Err(stop) => {
                next_range = index;
                following_stop = Some(stop);
                break;
            }
        };
        if calibration.is_none() {
            let result = calibrate(
                &mut connection,
                range,
                &overview,
                input.max_matches_per_range,
                deadline,
                &mut budget,
            )
            .await;
            if result.calibration.state != AcceleratorState::Unverified {
                calibration = Some(result.calibration);
            }
            if let Some(stop) = result.stop {
                range_rows.push(range_json(
                    range,
                    &format,
                    ObservedRange {
                        overview,
                        matches: Vec::new(),
                        accelerator_state: AcceleratorState::Defective,
                        accelerator_failure_code: Some(stop.failure_code),
                        response_bytes: budget.used.saturating_sub(range_bytes_before),
                        elapsed_ms: elapsed_millis(started),
                    },
                ));
                next_range = index + 1;
                following_stop = Some(stop);
                break;
            }
        }
        let accelerator_state = calibration
            .as_ref()
            .map_or(AcceleratorState::Unverified, |value| value.state);
        let mut accelerator_failure = None;
        let mut matches = Vec::new();
        if accelerator_state == AcceleratorState::Supported {
            match bounded_xpat(
                &mut connection,
                range,
                &patterns,
                input.max_matches_per_range,
                deadline,
                &mut budget,
            )
            .await
            {
                XpatOutcome::Complete(observed) => matches = observed,
                XpatOutcome::Unsupported => {
                    accelerator_failure = Some("nntp_header_pattern_unavailable");
                    if let Some(calibration) = calibration.as_mut() {
                        calibration.state = AcceleratorState::Defective;
                    }
                }
                XpatOutcome::Defective(code) => {
                    accelerator_failure = Some(code);
                    if let Some(calibration) = calibration.as_mut() {
                        calibration.state = AcceleratorState::Defective;
                    }
                }
                XpatOutcome::Stopped(stop) => {
                    accelerator_failure = Some(stop.failure_code);
                    if let Some(calibration) = calibration.as_mut() {
                        calibration.state = AcceleratorState::Defective;
                    }
                    range_rows.push(range_json(
                        range,
                        &format,
                        ObservedRange {
                            overview,
                            matches,
                            accelerator_state: AcceleratorState::Defective,
                            accelerator_failure_code: accelerator_failure,
                            response_bytes: budget.used.saturating_sub(range_bytes_before),
                            elapsed_ms: elapsed_millis(started),
                        },
                    ));
                    next_range = index + 1;
                    following_stop = Some(stop);
                    break;
                }
            }
        }
        let final_accelerator_state = calibration
            .as_ref()
            .map_or(AcceleratorState::Unverified, |value| value.state);
        range_rows.push(range_json(
            range,
            &format,
            ObservedRange {
                overview,
                matches,
                accelerator_state: final_accelerator_state,
                accelerator_failure_code: accelerator_failure,
                response_bytes: budget.used.saturating_sub(range_bytes_before),
                elapsed_ms: elapsed_millis(started),
            },
        ));
    }
    if let Some(stop) = following_stop {
        append_failure_receipts(&mut range_rows, &input.ranges[next_range..], stop);
    }
    let calibration = calibration.unwrap_or(Calibration {
        state: AcceleratorState::Unverified,
        positive_matches: 0,
        negative_matches: 0,
    });
    response(
        input,
        Some((&group.name, group.first, group.last)),
        Some(&format),
        calibration,
        budget.used,
        range_rows,
        true,
    )
}

async fn calibrate(
    connection: &mut NntpConnection,
    range: &ClearSearchRangeInput,
    overview: &LosslessOverviewRows,
    max_matches: usize,
    deadline: &Deadline,
    budget: &mut Budget,
) -> CalibrationResult {
    let Some((article_number, token)) = positive_control(overview) else {
        return CalibrationResult {
            calibration: Calibration {
                state: AcceleratorState::Unverified,
                positive_matches: 0,
                negative_matches: 0,
            },
            stop: None,
        };
    };
    let positive_pattern = format!("*{token}*");
    let positive = match bounded_xpat(
        connection,
        range,
        &[positive_pattern.as_str()],
        max_matches,
        deadline,
        budget,
    )
    .await
    {
        XpatOutcome::Complete(matches) => matches,
        XpatOutcome::Unsupported => {
            return calibration_result(AcceleratorState::Unsupported, 0, 0, None);
        }
        XpatOutcome::Defective(_) => {
            return calibration_result(AcceleratorState::Defective, 0, 0, None);
        }
        XpatOutcome::Stopped(stop) => {
            return calibration_result(AcceleratorState::Defective, 0, 0, Some(stop));
        }
    };
    let negative_pattern = format!("*newsgroupsnegative{article_number:x}nomatch*");
    let negative = match bounded_xpat(
        connection,
        range,
        &[negative_pattern.as_str()],
        max_matches,
        deadline,
        budget,
    )
    .await
    {
        XpatOutcome::Complete(matches) => matches,
        XpatOutcome::Unsupported | XpatOutcome::Defective(_) => {
            return calibration_result(AcceleratorState::Defective, positive.len(), 0, None);
        }
        XpatOutcome::Stopped(stop) => {
            return calibration_result(AcceleratorState::Defective, positive.len(), 0, Some(stop));
        }
    };
    let positive_is_correct = positive
        .iter()
        .any(|matched| matched.article_num == article_number);
    calibration_result(
        if positive_is_correct && negative.is_empty() {
            AcceleratorState::Supported
        } else {
            AcceleratorState::Defective
        },
        positive.len(),
        negative.len(),
        None,
    )
}

async fn bounded_xpat(
    connection: &mut NntpConnection,
    range: &ClearSearchRangeInput,
    patterns: &[&str],
    max_matches: usize,
    deadline: &Deadline,
    budget: &mut Budget,
) -> XpatOutcome {
    let response_limit = match budget.command_limit() {
        Ok(limit) => limit,
        Err(stop) => return XpatOutcome::Stopped(stop),
    };
    let response = match deadline
        .run(connection.xpat_bounded(
            "Subject",
            ArticleRange::Range(range.start_article, range.end_article),
            patterns,
            response_limit,
        ))
        .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(NntpError::UnsupportedCommand(_))) => return XpatOutcome::Unsupported,
        Ok(Err(error)) => return XpatOutcome::Stopped(stop_for_error(&error)),
        Err(stop) => return XpatOutcome::Stopped(stop),
    };
    if let Err(stop) = budget.consume(response.response_bytes) {
        return XpatOutcome::Stopped(stop);
    }
    if response.value.len() > max_matches {
        XpatOutcome::Defective("nntp_header_pattern_match_limit_exceeded")
    } else if response
        .value
        .iter()
        .try_fold(BTreeSet::new(), |mut seen, matched| {
            if matched.article_num < range.start_article
                || matched.article_num > range.end_article
                || !seen.insert(matched.article_num)
            {
                None
            } else {
                Some(seen)
            }
        })
        .is_none()
    {
        XpatOutcome::Defective("nntp_header_pattern_contract_invalid")
    } else {
        XpatOutcome::Complete(response.value)
    }
}

fn calibration_result(
    state: AcceleratorState,
    positive_matches: usize,
    negative_matches: usize,
    stop: Option<Stop>,
) -> CalibrationResult {
    CalibrationResult {
        calibration: Calibration {
            state,
            positive_matches,
            negative_matches,
        },
        stop,
    }
}

fn positive_control(overview: &LosslessOverviewRows) -> Option<(u64, String)> {
    overview.rows.iter().find_map(|row| {
        let subject = std::str::from_utf8(row.fields.first()?).ok()?;
        let token = subject
            .split(|character: char| !character.is_ascii_alphanumeric())
            .find(|token| {
                token.len() >= 4 && token.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })?;
        Some((row.article_number, token.to_string()))
    })
}

fn range_json(
    range: &ClearSearchRangeInput,
    format: &OverviewFormat,
    observed: ObservedRange,
) -> Value {
    let ObservedRange {
        overview,
        matches,
        accelerator_state,
        accelerator_failure_code,
        response_bytes,
        elapsed_ms,
    } = observed;
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
    let missing = missing_ranges(range.start_article, range.end_article, &present);
    let digest = format_digest(format);
    let receipt_state = if overview.defective_rows.is_empty() {
        "complete"
    } else {
        "partial"
    };
    json!({
        "start_article": range.start_article,
        "end_article": range.end_article,
        "receipt_state": receipt_state,
        "failure_code": Value::Null,
        "response_bytes": response_bytes,
        "elapsed_ms": elapsed_ms,
        "returned_row_count": overview.rows.len() + overview.defective_rows.len(),
        "valid_row_count": overview.rows.len(),
        "defective_row_count": overview.defective_rows.len(),
        "missing_ranges": missing,
        "unobserved_ranges": Vec::<(u64, u64)>::new(),
        "rows": overview.rows.into_iter().map(|row| row_json(row, &digest)).collect::<Vec<_>>(),
        "defective_rows": overview.defective_rows.into_iter().map(defective_row_json).collect::<Vec<_>>(),
        "accelerator_state": accelerator_state.as_str(),
        "accelerator_failure_code": accelerator_failure_code,
        "xpat_match_count": matches.len(),
        "xpat_matches": matches.into_iter().map(|matched| json!({
            "article_number": matched.article_num,
            "value": matched.value
        })).collect::<Vec<_>>()
    })
}

fn failure_receipt(range: &ClearSearchRangeInput, stop: Stop) -> Value {
    json!({
        "start_article": range.start_article,
        "end_article": range.end_article,
        "receipt_state": stop.receipt_state,
        "failure_code": stop.failure_code,
        "response_bytes": 0,
        "elapsed_ms": 0,
        "returned_row_count": 0,
        "valid_row_count": 0,
        "defective_row_count": 0,
        "missing_ranges": Vec::<(u64, u64)>::new(),
        "unobserved_ranges": [(range.start_article, range.end_article)],
        "rows": Vec::<Value>::new(),
        "defective_rows": Vec::<Value>::new(),
        "accelerator_state": "unverified",
        "accelerator_failure_code": Value::Null,
        "xpat_match_count": 0,
        "xpat_matches": Vec::<Value>::new()
    })
}

fn append_failure_receipts(
    receipts: &mut Vec<Value>,
    ranges: &[ClearSearchRangeInput],
    stop: Stop,
) {
    receipts.extend(ranges.iter().map(|range| failure_receipt(range, stop)));
}

fn failed_response(input: &ClearSearchInput, stop: Stop) -> Value {
    let mut receipts = Vec::with_capacity(input.ranges.len());
    append_failure_receipts(&mut receipts, &input.ranges, stop);
    response(
        input,
        None,
        None,
        Calibration {
            state: AcceleratorState::Unverified,
            positive_matches: 0,
            negative_matches: 0,
        },
        0,
        receipts,
        false,
    )
}

fn response(
    input: &ClearSearchInput,
    group: Option<(&str, u64, u64)>,
    format: Option<&OverviewFormat>,
    calibration: Calibration,
    response_bytes: usize,
    ranges: Vec<Value>,
    connection_reused: bool,
) -> Value {
    let execution_state = if ranges.iter().all(|range| {
        matches!(
            range.get("receipt_state").and_then(Value::as_str),
            Some("complete" | "partial")
        )
    }) {
        "complete"
    } else {
        "incomplete"
    };
    let overview_format = format.map(|format| {
        json!({
            "fields_base64": format.fields.iter().map(|field| BASE64.encode(field)).collect::<Vec<_>>(),
            "sha256": format_digest(format)
        })
    });
    json!({
        "status": "complete",
        "operation": "clear_search",
        "request_id": input.request_id,
        "cancellation_id": input.cancellation_id,
        "group": input.group,
        "group_first_article": group.map(|value| value.1),
        "group_last_article": group.map(|value| value.2),
        "predicate_sha256": input.predicate_sha256,
        "deadline_at_unix_ms": input.deadline_at_unix_ms,
        "execution_state": execution_state,
        "connection_reused": connection_reused,
        "response_bytes": response_bytes,
        "overview_format": overview_format,
        "accelerator": {
            "state": calibration.state.as_str(),
            "positive_match_count": calibration.positive_matches,
            "negative_match_count": calibration.negative_matches
        },
        "ranges": ranges
    })
}

fn stop_for_error(error: &NntpError) -> Stop {
    let receipt_state = match error {
        NntpError::Connection(_)
        | NntpError::Io(_)
        | NntpError::Tls(_)
        | NntpError::Timeout(_)
        | NntpError::ServiceUnavailable(_) => "transport_failed",
        _ => "refused",
    };
    Stop {
        receipt_state,
        failure_code: match error {
            NntpError::ResponseTooLarge(_) => "nntp_observation_response_limit_exceeded",
            NntpError::UnsupportedCommand(_) => "nntp_operation_unsupported",
            NntpError::Protocol(_) => "nntp_overview_unavailable",
            _ => nntp_failure(error, "overview_range"),
        },
    }
}

fn elapsed_millis(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nzb_web::nzb_core::nzb_nntp::LosslessOverviewRow;

    #[test]
    fn positive_control_comes_from_an_observed_ascii_subject() {
        let overview = LosslessOverviewRows {
            rows: vec![LosslessOverviewRow {
                article_number: 42,
                fields: vec![b"Traitors Espana S02E01".to_vec()],
            }],
            defective_rows: Vec::new(),
        };
        assert_eq!(
            positive_control(&overview),
            Some((42, "Traitors".to_string()))
        );
    }

    #[test]
    fn aggregate_budget_never_grants_more_than_the_remaining_bytes() {
        let mut budget = Budget::new(10);
        assert_eq!(budget.command_limit().expect("initial limit"), 10);
        budget.consume(7).expect("consume bytes");
        assert_eq!(budget.command_limit().expect("remaining limit"), 3);
        budget.consume(3).expect("consume remaining");
        assert!(budget.command_limit().is_err());
    }
}
