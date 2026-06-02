use crate::protobuf::{connect_frame_encode, gzip_compress, gzip_decompress};
use reqwest::header::{CONTENT_ENCODING, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::sleep;
use uuid::Uuid;

use super::auth::ws_ls_version;
use super::{API_BASE, FastContextError};

fn classify_status(status: reqwest::StatusCode, body: String) -> FastContextError {
    let code = if status.as_u16() == 413 {
        "PAYLOAD_TOO_LARGE"
    } else if status.as_u16() == 429 {
        "RATE_LIMITED"
    } else if matches!(status.as_u16(), 401 | 403) {
        "AUTH_ERROR"
    } else {
        "SERVER_ERROR"
    };
    FastContextError::new(
        format!("HTTP {}", status.as_u16()),
        code,
        json!({ "status": status.as_u16(), "body": body }),
    )
}

fn classify_reqwest_error(err: reqwest::Error) -> FastContextError {
    let message = err.to_string();
    if err.is_timeout() || message.to_lowercase().contains("timeout") {
        return FastContextError::new(message, "TIMEOUT", Value::Null);
    }
    FastContextError::new(message, "NETWORK_ERROR", Value::Null)
}

async fn send_post(
    url: &str,
    body: Vec<u8>,
    headers: HeaderMap,
    timeout: Duration,
    allow_invalid_certs: bool,
) -> Result<(Vec<u8>, Option<String>), FastContextError> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(allow_invalid_certs)
        .build()
        .map_err(classify_reqwest_error)?;

    let response = client
        .post(url)
        .headers(headers)
        .body(body)
        .timeout(timeout)
        .send()
        .await
        .map_err(classify_reqwest_error)?;

    let status = response.status();
    let content_encoding = response
        .headers()
        .get(CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let bytes = response
        .bytes()
        .await
        .map_err(classify_reqwest_error)?
        .to_vec();

    if !status.is_success() {
        let body = String::from_utf8_lossy(&bytes).into_owned();
        return Err(classify_status(status, body));
    }

    Ok((bytes, content_encoding))
}

fn header_map(headers: &[(&str, String)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            map.insert(name, value);
        }
    }
    map
}

pub fn decode_unary_response(data: &[u8], content_encoding: Option<&str>) -> Vec<u8> {
    if content_encoding.is_some_and(|encoding| encoding.to_lowercase().contains("gzip")) {
        return gzip_decompress(data).unwrap_or_else(|_| data.to_vec());
    }
    if data.starts_with(&[0x1f, 0x8b]) {
        return gzip_decompress(data).unwrap_or_else(|_| data.to_vec());
    }
    data.to_vec()
}

pub(super) async fn unary_request(
    url: &str,
    body: &[u8],
    compress: bool,
    timeout: Duration,
) -> Result<Vec<u8>, FastContextError> {
    let mut headers = vec![
        ("Content-Type", "application/proto".to_string()),
        ("Connect-Protocol-Version", "1".to_string()),
        ("User-Agent", "connect-go/1.18.1 (go1.25.5)".to_string()),
        ("Accept-Encoding", "gzip".to_string()),
    ];
    let payload = if compress {
        headers.push(("Content-Encoding", "gzip".to_string()));
        gzip_compress(body).unwrap_or_else(|_| body.to_vec())
    } else {
        body.to_vec()
    };

    let header_map = header_map(&headers);
    match send_post(url, payload.clone(), header_map.clone(), timeout, false).await {
        Ok((data, encoding)) => Ok(decode_unary_response(&data, encoding.as_deref())),
        Err(err) if err.code == "NETWORK_ERROR" && err.message.to_lowercase().contains("cert") => {
            let (data, encoding) = send_post(url, payload, header_map, timeout, true).await?;
            Ok(decode_unary_response(&data, encoding.as_deref()))
        }
        Err(err) => Err(err),
    }
}

pub(super) async fn streaming_request(
    body: &[u8],
    timeout_ms: u64,
    max_retries: u32,
    ls_version: Option<&str>,
) -> Result<Vec<u8>, FastContextError> {
    let frame = connect_frame_encode(body, true);
    let url = format!("{API_BASE}/GetDevstralStream");
    let trace_id = Uuid::new_v4().simple().to_string();
    let span_id = Uuid::new_v4().simple().to_string()[..16].to_string();
    let timeout = Duration::from_millis(timeout_ms + 5_000);
    let ls_version = ws_ls_version(ls_version);
    let headers = header_map(&[
        ("Content-Type", "application/connect+proto".to_string()),
        ("Connect-Protocol-Version", "1".to_string()),
        ("Connect-Accept-Encoding", "gzip".to_string()),
        ("Connect-Content-Encoding", "gzip".to_string()),
        ("Connect-Timeout-Ms", timeout_ms.to_string()),
        ("User-Agent", "connect-go/1.18.1 (go1.25.5)".to_string()),
        ("Accept-Encoding", "identity".to_string()),
        (
            "Baggage",
            format!(
                "sentry-release=language-server-windsurf@{ls_version},sentry-environment=stable,sentry-sampled=false,sentry-trace_id={trace_id},sentry-public_key=b813f73488da69eedec534dba1029111"
            ),
        ),
        ("Sentry-Trace", format!("{trace_id}-{span_id}-0")),
    ]);

    let mut last_err: Option<FastContextError> = None;
    for attempt in 0..=max_retries {
        let result = match send_post(&url, frame.clone(), headers.clone(), timeout, false).await {
            Ok((data, _)) => Ok(data),
            Err(err)
                if err.code == "NETWORK_ERROR" && err.message.to_lowercase().contains("cert") =>
            {
                send_post(&url, frame.clone(), headers.clone(), timeout, true)
                    .await
                    .map(|(data, _)| data)
            }
            Err(err) => Err(err),
        };

        match result {
            Ok(data) => return Ok(data),
            Err(err) if err.code == "AUTH_ERROR" => return Err(err),
            Err(err)
                if err
                    .details
                    .get("status")
                    .and_then(Value::as_u64)
                    .is_some_and(|status| (400..500).contains(&status) && status != 429) =>
            {
                return Err(err);
            }
            Err(err) => {
                last_err = Some(err);
                if attempt < max_retries {
                    sleep(Duration::from_secs((attempt + 1) as u64)).await;
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        FastContextError::new("Streaming request failed", "NETWORK_ERROR", Value::Null)
    }))
}
