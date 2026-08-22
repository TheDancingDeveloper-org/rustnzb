use super::{blocked, contract::ArticleBodyPrefixInput, nntp_failure};
use axum::{Json, extract::State};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use nzb_web::{error::ApiError, nzb_core::nzb_nntp::NntpConnection, state::AppState};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub(crate) async fn h_article_body_prefix(
    State(state): State<Arc<AppState>>,
    Json(input): Json<ArticleBodyPrefixInput>,
) -> Result<Json<Value>, ApiError> {
    input.validate().map_err(ApiError::bad_request)?;
    let servers = state.queue_manager.get_servers();
    let Some(server) = servers.first() else {
        return Ok(blocked(
            "article_body_prefix",
            &input.request_id,
            &input.group,
            "nntp_provider_not_configured",
        ));
    };
    let mut server = server.clone();
    server.compress = false;
    let mut connection = NntpConnection::new(format!("body-prefix-{}", input.request_id));
    if let Err(error) = connection.connect(&server).await {
        return Ok(blocked(
            "article_body_prefix",
            &input.request_id,
            &input.group,
            nntp_failure(&error, "article_body_prefix"),
        ));
    }
    let group = match connection.group(&input.group).await {
        Ok(group) => group,
        Err(error) => {
            let _ = connection.quit().await;
            return Ok(blocked(
                "article_body_prefix",
                &input.request_id,
                &input.group,
                nntp_failure(&error, "article_body_prefix"),
            ));
        }
    };
    let prefix = match connection
        .fetch_body_prefix(&input.message_id, input.max_wire_bytes)
        .await
    {
        Ok(prefix) => prefix,
        Err(error) => {
            let _ = connection.quit().await;
            return Ok(blocked(
                "article_body_prefix",
                &input.request_id,
                &input.group,
                nntp_failure(&error, "article_body_prefix"),
            ));
        }
    };
    let _ = connection.quit().await;
    let decoded = decode_payload_prefix(&prefix.data, prefix.complete, input.max_payload_bytes);
    let wire_sha256 = format!("{:x}", Sha256::digest(&prefix.data));
    let payload_sha256 = format!("{:x}", Sha256::digest(&decoded.bytes));
    Ok(Json(json!({
        "status": "complete",
        "operation": "article_body_prefix",
        "request_id": input.request_id,
        "group": group.name,
        "group_first_article": group.first,
        "group_last_article": group.last,
        "message_id": input.message_id,
        "wire_prefix_byte_count": prefix.data.len(),
        "wire_prefix_base64": BASE64.encode(prefix.data),
        "wire_prefix_sha256": wire_sha256,
        "body_complete": prefix.complete,
        "payload_encoding": decoded.encoding,
        "payload_prefix_byte_count": decoded.bytes.len(),
        "payload_prefix_base64": BASE64.encode(decoded.bytes),
        "payload_prefix_sha256": payload_sha256,
        "payload_complete": decoded.complete
    })))
}

struct DecodedPrefix {
    bytes: Vec<u8>,
    encoding: &'static str,
    complete: bool,
}

fn decode_payload_prefix(wire: &[u8], body_complete: bool, maximum: usize) -> DecodedPrefix {
    let mut started = false;
    let mut ended = false;
    let mut truncated = false;
    let mut output = Vec::with_capacity(maximum.min(wire.len()));
    for raw_line in wire.split_inclusive(|byte| *byte == b'\n') {
        let line = raw_line.strip_suffix(b"\n").unwrap_or(raw_line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if !started {
            if line.starts_with(b"=ybegin ") {
                started = true;
            }
            continue;
        }
        if line.starts_with(b"=ypart ") {
            continue;
        }
        if line.starts_with(b"=yend ") {
            ended = true;
            break;
        }
        let mut index = 0usize;
        while index < line.len() {
            if output.len() == maximum {
                truncated = true;
                break;
            }
            let encoded = if line[index] == b'=' {
                let Some(escaped) = line.get(index + 1) else {
                    truncated = true;
                    break;
                };
                index += 2;
                escaped.wrapping_sub(64)
            } else {
                let value = line[index];
                index += 1;
                value
            };
            output.push(encoded.wrapping_sub(42));
        }
        if truncated {
            break;
        }
    }
    if started {
        DecodedPrefix {
            bytes: output,
            encoding: "yenc",
            complete: body_complete && ended && !truncated,
        }
    } else {
        let length = wire.len().min(maximum);
        DecodedPrefix {
            bytes: wire[..length].to_vec(),
            encoding: "plain",
            complete: body_complete && wire.len() <= maximum,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(data: &[u8]) -> Vec<u8> {
        let mut output = b"=ybegin line=128 size=8 name=random.bin\r\n".to_vec();
        for byte in data {
            let encoded = byte.wrapping_add(42);
            if matches!(encoded, 0 | b'\n' | b'\r' | b'=') {
                output.push(b'=');
                output.push(encoded.wrapping_add(64));
            } else {
                output.push(encoded);
            }
        }
        output.extend_from_slice(b"\r\n=yend size=8\r\n");
        output
    }

    #[test]
    fn yenc_prefix_decodes_binary_magic_without_requiring_a_complete_article() {
        let wire = encode(b"PAR2\0PKT");
        let prefix = decode_payload_prefix(&wire[..wire.len() - 8], false, 64);
        assert_eq!(prefix.bytes, b"PAR2\0PKT");
        assert_eq!(prefix.encoding, "yenc");
        assert!(!prefix.complete);
    }

    #[test]
    fn plain_small_metadata_remains_byte_exact() {
        let prefix = decode_payload_prefix(b"release.nfo\r\n", true, 64);
        assert_eq!(prefix.bytes, b"release.nfo\r\n");
        assert_eq!(prefix.encoding, "plain");
        assert!(prefix.complete);
    }
}
