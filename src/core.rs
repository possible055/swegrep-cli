use crate::credentials;
use crate::executor::{ToolExecutor, command_keys, command_number, valid_command_count};
use crate::protobuf::{
    ProtobufEncoder, connect_frame_decode, connect_frame_encode, extract_strings, gzip_compress,
    gzip_decompress,
};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE;
use regex::Regex;
use reqwest::header::{CONTENT_ENCODING, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::env;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::time::sleep;
use uuid::Uuid;

pub const API_BASE: &str =
    "https://server.self-serve.windsurf.com/exa.api_server_pb.ApiServerService";
pub const AUTH_BASE: &str = "https://server.self-serve.windsurf.com/exa.auth_pb.AuthService";
pub const WS_APP: &str = "windsurf";
pub const DEFAULT_WS_APP_VER: &str = "1.48.2";
pub const DEFAULT_WS_LS_VER: &str = "1.9544.35";
pub const MAX_TREE_BYTES: usize = 250 * 1024;

#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct FastContextError {
    pub message: String,
    pub code: String,
    pub details: Value,
}

impl FastContextError {
    pub fn new(message: impl Into<String>, code: impl Into<String>, details: Value) -> Self {
        Self {
            message: message.into(),
            code: code.into(),
            details,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub full_path: String,
    pub ranges: Vec<(usize, usize)>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchMeta {
    pub tree_depth: Option<usize>,
    pub tree_size_kb: Option<f64>,
    pub fell_back: Option<bool>,
    pub project_root: Option<String>,
    pub error_code: Option<String>,
    pub context_trimmed: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    pub files: Vec<FileEntry>,
    pub rg_patterns: Vec<String>,
    pub raw_response: Option<String>,
    pub error: Option<String>,
    pub meta: SearchMeta,
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub query: String,
    pub project_root: PathBuf,
    pub api_key: Option<String>,
    pub jwt: Option<String>,
    pub app_version: Option<String>,
    pub ls_version: Option<String>,
    pub max_turns: usize,
    pub max_commands: usize,
    pub max_results: usize,
    pub tree_depth: usize,
    pub timeout_ms: u64,
    pub exclude_paths: Vec<String>,
    pub result_max_lines: Option<usize>,
    pub line_max_chars: Option<usize>,
}

impl SearchOptions {
    pub fn new(query: impl Into<String>, project_root: impl Into<PathBuf>) -> Self {
        Self {
            query: query.into(),
            project_root: project_root.into(),
            api_key: None,
            jwt: None,
            app_version: None,
            ls_version: None,
            max_turns: 3,
            max_commands: 8,
            max_results: 10,
            tree_depth: 4,
            timeout_ms: 30_000,
            exclude_paths: Vec::new(),
            result_max_lines: None,
            line_max_chars: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthCheck {
    pub ok: bool,
    pub error_code: Option<String>,
    pub error: Option<String>,
    pub jwt_source: String,
    pub app_version: String,
    pub ls_version: String,
}

#[derive(Debug, Clone)]
struct ChatMessage {
    role: u64,
    content: String,
    tool_call_id: Option<String>,
    tool_name: Option<String>,
    tool_args_json: Option<String>,
    ref_call_id: Option<String>,
}

impl ChatMessage {
    fn simple(role: u64, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_call_id: None,
            tool_name: None,
            tool_args_json: None,
            ref_call_id: None,
        }
    }
}

pub fn get_config_path() -> PathBuf {
    credentials::get_config_path()
}

pub fn load_cached_api_key() -> Option<String> {
    credentials::load_cached_api_key(Some(&get_config_path()))
}

pub fn save_cached_api_key(key: &str) -> Result<PathBuf, String> {
    credentials::save_cached_api_key(key, Some(&get_config_path()))
}

pub fn get_api_key(api_key: Option<&str>, save_discovered: bool) -> Result<String, String> {
    if let Some(api_key) = api_key {
        if !credentials::looks_truncated_api_key(api_key) {
            return Ok(api_key.to_string());
        }
        if let Some(discovered) = credentials::discover_api_key(None) {
            eprintln!(
                "[swegrep-cli] Passed API key looks truncated; using key discovered from Windsurf"
            );
            return Ok(discovered);
        }
        return Ok(api_key.to_string());
    }

    if let Ok(key) = env::var(credentials::CONFIG_KEY) {
        if credentials::looks_truncated_api_key(&key)
            && let Some(discovered) = credentials::discover_api_key(None)
        {
            eprintln!(
                "[swegrep-cli] WINDSURF_API_KEY looks truncated; using key discovered from Windsurf"
            );
            return Ok(discovered);
        }
        return Ok(key);
    }

    if let Some(cached) = load_cached_api_key() {
        eprintln!("[swegrep-cli] Using cached API key from config");
        return Ok(cached);
    }

    if let Some(discovered) = credentials::discover_api_key(None) {
        if save_discovered {
            let _ = save_cached_api_key(&discovered);
        }
        return Ok(discovered);
    }

    Err(format!(
        "Windsurf API Key not found. Set WINDSURF_API_KEY env var, ensure Windsurf is logged in, or write it to config file: {}",
        get_config_path().display()
    ))
}

fn protocol_setting(value: Option<&str>, env_name: &str, default: &str) -> String {
    value
        .map(ToOwned::to_owned)
        .or_else(|| env::var(env_name).ok())
        .unwrap_or_else(|| default.to_string())
}

fn ws_app_version(value: Option<&str>) -> String {
    protocol_setting(value, "WS_APP_VER", DEFAULT_WS_APP_VER)
}

fn ws_ls_version(value: Option<&str>) -> String {
    protocol_setting(value, "WS_LS_VER", DEFAULT_WS_LS_VER)
}

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

async fn unary_request(
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

async fn streaming_request(
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

pub fn check_auth(
    api_key: Option<&str>,
    app_version: Option<&str>,
    ls_version: Option<&str>,
) -> AuthCheck {
    match get_api_key(api_key, false) {
        Ok(_) => AuthCheck {
            ok: true,
            error_code: None,
            error: None,
            jwt_source: "api-key".to_string(),
            app_version: ws_app_version(app_version),
            ls_version: ws_ls_version(ls_version),
        },
        Err(err) => AuthCheck {
            ok: false,
            error_code: Some("API_KEY_ERROR".to_string()),
            error: Some(err),
            jwt_source: "api-key".to_string(),
            app_version: ws_app_version(app_version),
            ls_version: ws_ls_version(ls_version),
        },
    }
}

static JWT_CACHE: OnceLock<Mutex<HashMap<String, (String, f64)>>> = OnceLock::new();

fn jwt_cache() -> &'static Mutex<HashMap<String, (String, f64)>> {
    JWT_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn get_jwt_exp(jwt: &str) -> f64 {
    let parts = jwt.split('.').collect::<Vec<_>>();
    if parts.len() < 2 {
        return 0.0;
    }
    let mut payload_b64 = parts[1].to_string();
    let padding = (4 - payload_b64.len() % 4) % 4;
    payload_b64.push_str(&"=".repeat(padding));
    let Ok(payload) = URL_SAFE.decode(payload_b64) else {
        return 0.0;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&payload) else {
        return 0.0;
    };
    value.get("exp").and_then(Value::as_f64).unwrap_or(0.0)
}

pub async fn fetch_jwt(api_key: &str, timeout_ms: u64) -> Result<String, FastContextError> {
    let mut meta = ProtobufEncoder::new();
    meta.write_string(1, WS_APP);
    meta.write_string(2, &ws_app_version(None));
    meta.write_string(3, api_key);
    meta.write_string(4, "zh-cn");
    meta.write_string(7, &ws_ls_version(None));
    meta.write_string(12, WS_APP);
    meta.write_bytes(30, b"\x00\x01");

    let mut outer = ProtobufEncoder::new();
    outer.write_message(1, &meta);

    let response = unary_request(
        &format!("{AUTH_BASE}/GetUserJwt"),
        &outer.to_bytes(),
        false,
        Duration::from_millis(timeout_ms),
    )
    .await?;

    for value in extract_strings(&response) {
        if value.starts_with("eyJ") && value.contains('.') {
            return Ok(value);
        }
    }

    Err(FastContextError::new(
        "Failed to extract JWT from GetUserJwt response",
        "NETWORK_ERROR",
        Value::Null,
    ))
}

pub async fn get_cached_jwt(api_key: &str, timeout_ms: u64) -> Result<String, FastContextError> {
    let now = now_seconds();
    if let Ok(cache) = jwt_cache().lock()
        && let Some((token, expires_at)) = cache.get(api_key)
        && *expires_at > now + 60.0
    {
        return Ok(token.clone());
    }

    let token = fetch_jwt(api_key, timeout_ms).await?;
    let exp = get_jwt_exp(&token);
    let expires_at = if exp > 0.0 { exp } else { now + 3600.0 };
    if let Ok(mut cache) = jwt_cache().lock() {
        cache.insert(api_key.to_string(), (token.clone(), expires_at));
    }
    Ok(token)
}

fn build_metadata(
    api_key: &str,
    jwt: &str,
    app_version: Option<&str>,
    ls_version: Option<&str>,
) -> ProtobufEncoder {
    let mut meta = ProtobufEncoder::new();
    meta.write_string(1, WS_APP);
    meta.write_string(2, &ws_app_version(app_version));
    meta.write_string(3, api_key);
    meta.write_string(4, "zh-cn");

    let os_name = env::consts::OS;
    let arch = env::consts::ARCH;
    let sysname = match os_name {
        "macos" => "Darwin",
        "windows" => "Windows_NT",
        _ => "Linux",
    };
    meta.write_string(
        5,
        &json!({
            "Os": os_name,
            "Arch": arch,
            "Release": "1.0",
            "Version": "1.0",
            "Machine": arch,
            "Nodename": "localhost",
            "Sysname": sysname,
            "ProductVersion": "",
        })
        .to_string(),
    );
    meta.write_string(7, &ws_ls_version(ls_version));

    let cpu_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4);
    meta.write_string(
        8,
        &json!({
            "NumSockets": 1,
            "NumCores": cpu_count,
            "NumThreads": cpu_count,
            "VendorID": "",
            "Family": "0",
            "Model": "0",
            "ModelName": "Unknown CPU",
            "Memory": 16_u64 * 1024 * 1024 * 1024,
        })
        .to_string(),
    );
    meta.write_string(12, WS_APP);
    meta.write_string(21, jwt);
    meta.write_bytes(30, b"\x00\x01");
    meta
}

fn build_chat_message(role: u64, content: &str, message: &ChatMessage) -> ProtobufEncoder {
    let mut msg = ProtobufEncoder::new();
    msg.write_varint(2, role);
    msg.write_string(3, content);

    if let (Some(tool_call_id), Some(tool_name), Some(tool_args_json)) = (
        message.tool_call_id.as_ref(),
        message.tool_name.as_ref(),
        message.tool_args_json.as_ref(),
    ) {
        let mut tc = ProtobufEncoder::new();
        tc.write_string(1, tool_call_id);
        tc.write_string(2, tool_name);
        tc.write_string(3, tool_args_json);
        msg.write_message(6, &tc);
    }

    if let Some(ref_call_id) = &message.ref_call_id {
        msg.write_string(7, ref_call_id);
    }

    msg
}

fn build_request(
    api_key: &str,
    jwt: &str,
    messages: &[ChatMessage],
    tool_defs: &str,
    app_version: Option<&str>,
    ls_version: Option<&str>,
) -> Vec<u8> {
    let mut req = ProtobufEncoder::new();
    req.write_message(1, &build_metadata(api_key, jwt, app_version, ls_version));

    for message in messages {
        let msg = build_chat_message(message.role, &message.content, message);
        req.write_message(2, &msg);
    }

    req.write_string(3, tool_defs);
    req.to_bytes()
}

fn parse_tool_call(text: &str) -> Option<(String, String, Value)> {
    let text = text.replace("</s>", "");
    let regex = Regex::new(r"(?s)\[TOOL_CALLS\](\w+)\[ARGS\](\{.+)").ok()?;
    let captures = regex.captures(&text)?;
    let full_match = captures.get(0)?;
    let name = captures.get(1)?.as_str().to_string();
    let raw = captures.get(2)?.as_str().trim();

    let mut depth = 0_i32;
    let mut end = 0_usize;
    for (idx, ch) in raw.char_indices() {
        if ch == '{' {
            depth += 1;
        } else if ch == '}' {
            depth -= 1;
            if depth == 0 {
                end = idx + ch.len_utf8();
                break;
            }
        }
    }
    if end == 0 {
        end = raw.len();
    }

    let args = serde_json::from_str::<Value>(&raw[..end]).ok()?;
    let thinking = text[..full_match.start()].trim().to_string();
    Some((thinking, name, args))
}

fn parse_response(data: &[u8]) -> (String, Option<(String, Value)>) {
    let frames = connect_frame_decode(data);
    let mut all_text = String::new();

    for frame_data in frames {
        let text_candidate = String::from_utf8_lossy(&frame_data).to_string();
        if text_candidate.starts_with('{')
            && let Ok(error) = serde_json::from_str::<Value>(&text_candidate)
            && let Some(error) = error.get("error")
        {
            let code = error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let msg = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return (format!("[Error] {code}: {msg}"), None);
        }

        let raw_text = text_candidate.replace('\u{fffd}', "");
        if raw_text.contains("[TOOL_CALLS]") {
            all_text = raw_text;
            break;
        }

        for value in extract_strings(&frame_data) {
            if value.len() > 10 {
                all_text.push_str(&value);
            }
        }
    }

    if let Some((thinking, name, args)) = parse_tool_call(&all_text) {
        (thinking, Some((name, args)))
    } else {
        (all_text, None)
    }
}

const SYSTEM_PROMPT_TEMPLATE: &str = r#"You are an expert software engineer, responsible for providing context to another engineer to solve a code issue in the current codebase. The user will present you with a description of the issue, and it is your job to provide a series of file paths with associated line ranges that contain ALL the information relevant to understand and correctly address the issue.

# IMPORTANT:
- A relevant file does not mean only the files that must be modified to solve the task. It means any file that contains information relevant to planning and implementing the fix, such as the definitions of classes and functions that are relevant to the pieces of code that will have to be modified.
- You should include enough context around the relevant lines to allow the engineer to understand the task correctly. You must include ENTIRE semantic blocks (functions, classes, definitions, etc). For example: If addressing the issue requires modifying a method within a class, then you should include the entire class definition, not just the lines around the method we want to modify.
- NEVER truncate these blocks unless they are very large (hundreds of lines or more, in which case providing only a relevant portion of the block is acceptable).
- Your job is to essentially alleviate the job of the other engineer by giving them a clean starting context from which to start working. More precisely, you should minimize the number of files the engineer has to read to understand and solve the task correctly (while not providing irrelevant code snippets).

# ENVIRONMENT
- Working directory: /codebase. Make sure to run commands in this directory, not `.
- Tool access: use the restricted_exec tool ONLY
- Allowed sub-commands (schema-enforced):
  - rg: Search for patterns in files using ripgrep
    - Required: pattern (string), path (string)
    - Optional: include (array of globs), exclude (array of globs)
  - readfile: Read contents of a file with optional line range
    - Required: file (string)
    - Optional: start_line (int), end_line (int) - 1-indexed, inclusive
  - tree: Display directory structure as a tree
    - Required: path (string)
    - Optional: levels (int)

# THINKING RULES
- Think step-by-step. Plan, reason, and reflect before each tool call.
- Use tool calls liberally and purposefully to ground every conclusion in real code, not assumptions.
- If a command fails, rethink and try something different; do not complain to the user.

# FAST-SEARCH DEFAULTS (optimize rg/tree on large repos)
- Start NARROW, then widen only if needed. Prefer searching likely code roots first (e.g., `src/`, `lib/`, `app/`, `packages/`, `services/`) instead of `/codebase`.
- Prefer fixed-string search for literals: escape patterns or keep regex simple. Use smart case; avoid case-insensitive unless necessary.
- Prefer file-type filters and globs (in include) over full-repo scans.
- Default EXCLUDES for speed (apply via the exclude array): node_modules, .git, dist, build, coverage, .venv, venv, target, out, .cache, __pycache__, vendor, deps, third_party, logs, data, *.min.*
- Skip huge files where possible; when opening files, prefer reading only relevant ranges with readfile.
- Limit directory traversal with tree levels to quickly orient before deeper inspection.

# SOME EXAMPLES OF WORKFLOWS
- MAP - Use `tree` with small levels; `rg` on likely roots to grasp structure and hotspots.
- ANCHOR - `rg` for problem keywords and anchor symbols; restrict by language globs via include.
- TRACE - Follow imports with targeted `rg` in narrowed roots; open files with `readfile` scoped to entire semantic blocks.
- VERIFY - Confirm each candidate path exists by reading or additional searches; drop false positives (tests, vendored, generated) unless they must change.

# TOOL USE GUIDELINES
- You must use a SINGLE restricted_exec call in your answer, that lets you execute at most {max_commands} commands in a single turn. Each command must be an object with a `type` field of `rg`, `readfile`, or `tree` and the appropriate fields for that type.
- Example restricted_exec usage:
[TOOL_CALLS]restricted_exec[ARGS]{{
  "command1": {{
    "type": "rg",
    "pattern": "Controller",
    "path": "/codebase/slime",
    "include": ["**/*.py"],
    "exclude": ["**/node_modules/**", "**/.git/**", "**/dist/**", "**/build/**", "**/.venv/**", "**/__pycache__/**"]
  }},
  "command2": {{
    "type": "readfile",
    "file": "/codebase/slime/train.py",
    "start_line": 1,
    "end_line": 200
  }},
  "command3": {{
    "type": "tree",
    "path": "/codebase/slime/",
    "levels": 2
  }}
}}
- You have at most {max_turns} turns to interact with the environment by calling tools, so issuing multiple commands at once is necessary and encouraged to speed up your research.
- Each command result may be truncated to 50 lines; prefer multiple targeted reads/searches to build complete context.
- DO NOT EVER USE MORE THAN {max_commands} commands in a single turn, or you will be penalized.

# ANSWER FORMAT (strict format, including tags)
- You will output an XML structure with a root element "ANSWER" containing "file" elements. Each "file" element will have a "path" attribute and contain "range" elements.
- You will output this as your final response.
- The line ranges must be inclusive.

Output example inside the "answer" tool argument:
<ANSWER>
  <file path="/codebase/info_theory/formulas/entropy.py">
    <range>10-60</range>
    <range>150-210</range>
  </file>
  <file path="/codebase/info_theory/data_structures/bits.py">
    <range>1-40</range>
    <range>110-170</range>
  </file>
</ANSWER>

Remember: Prefer narrow, fixed-string, and type-filtered searches with aggressive excludes and size/depth limits. Widen scope only as needed. Use the restricted tools available to you, and output your answer in exactly the specified format.

# NO RESULTS POLICY
If after thorough searching you are confident that NO relevant files exist for the given query (e.g., the function/class/concept does not exist in the codebase), you MUST return an empty ANSWER:
<ANSWER></ANSWER>
Do NOT return irrelevant files (such as entry points or config files) just to provide some output. An empty answer is always better than a misleading one.

# RESULT COUNT
Aim to return at most {max_results} files in your answer. Focus on the most relevant files first. If fewer files are relevant, return fewer.
"#;

const FINAL_FORCE_ANSWER: &str =
    "You have no turns left. Now you MUST provide your final ANSWER, even if it's not complete.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoMap {
    pub tree: String,
    pub depth: usize,
    pub size_bytes: usize,
    pub fell_back: bool,
}

fn build_command_schema(n: usize) -> Value {
    json!({
        "type": "object",
        "description": format!("Command {n} to execute. Must be one of: rg, readfile, or tree."),
        "oneOf": [
            {
                "properties": {
                    "type": {"type": "string", "const": "rg", "description": "Search for patterns in files using ripgrep."},
                    "pattern": {"type": "string", "description": "The regex pattern to search for."},
                    "path": {"type": "string", "description": "The path to search in."},
                    "include": {"type": "array", "items": {"type": "string"}, "description": "File patterns to include."},
                    "exclude": {"type": "array", "items": {"type": "string"}, "description": "File patterns to exclude."}
                },
                "required": ["type", "pattern", "path"]
            },
            {
                "properties": {
                    "type": {"type": "string", "const": "readfile", "description": "Read contents of a file with optional line range."},
                    "file": {"type": "string", "description": "Path to the file to read."},
                    "start_line": {"type": "integer", "description": "Starting line number (1-indexed)."},
                    "end_line": {"type": "integer", "description": "Ending line number (1-indexed)."}
                },
                "required": ["type", "file"]
            },
            {
                "properties": {
                    "type": {"type": "string", "const": "tree", "description": "Display directory structure as a tree."},
                    "path": {"type": "string", "description": "Path to the directory."},
                    "levels": {"type": "integer", "description": "Number of directory levels."}
                },
                "required": ["type", "path"]
            },
            {
                "properties": {
                    "type": {"type": "string", "const": "ls", "description": "List files in a directory."},
                    "path": {"type": "string", "description": "Path to the directory."},
                    "long_format": {"type": "boolean"},
                    "all": {"type": "boolean"}
                },
                "required": ["type", "path"]
            },
            {
                "properties": {
                    "type": {"type": "string", "const": "glob", "description": "Find files matching a glob pattern."},
                    "pattern": {"type": "string"},
                    "path": {"type": "string"},
                    "type_filter": {"type": "string", "enum": ["file", "directory", "all"]}
                },
                "required": ["type", "pattern", "path"]
            }
        ]
    })
}

pub fn get_tool_definitions(max_commands: usize) -> String {
    let mut props = Map::new();
    for i in 1..=max_commands {
        props.insert(format!("command{i}"), build_command_schema(i));
    }

    json!([
        {
            "type": "function",
            "function": {
                "name": "restricted_exec",
                "description": "Execute restricted commands (rg, readfile, tree, ls, glob) in parallel.",
                "parameters": {"type": "object", "properties": props, "required": ["command1"]}
            }
        },
        {
            "type": "function",
            "function": {
                "name": "answer",
                "description": "Final answer with relevant files and line ranges.",
                "parameters": {
                    "type": "object",
                    "properties": {"answer": {"type": "string", "description": "The final answer in XML format."}},
                    "required": ["answer"]
                }
            }
        }
    ])
    .to_string()
}

pub fn limit_tool_args(tool_args: &Value, max_commands: usize) -> Value {
    let Some(map) = tool_args.as_object() else {
        return Value::Object(Map::new());
    };
    let mut command_keys = map
        .keys()
        .filter(|key| key.starts_with("command"))
        .cloned()
        .collect::<Vec<_>>();
    command_keys.sort_by_key(|key| command_number(key));
    let allowed = command_keys
        .into_iter()
        .take(max_commands)
        .collect::<HashSet<_>>();

    let mut limited = Map::new();
    for (key, value) in map {
        if !key.starts_with("command") || allowed.contains(key) {
            limited.insert(key.clone(), value.clone());
        }
    }
    Value::Object(limited)
}

pub fn build_system_prompt(max_turns: usize, max_commands: usize, max_results: usize) -> String {
    SYSTEM_PROMPT_TEMPLATE
        .replace("{max_turns}", &max_turns.to_string())
        .replace("{max_commands}", &max_commands.to_string())
        .replace("{max_results}", &max_results.to_string())
}

fn trim_messages(messages: &mut Vec<ChatMessage>) -> bool {
    if messages.len() <= 4 {
        return false;
    }
    let head = messages[..2].to_vec();
    let tail = messages[messages.len() - 2..].to_vec();
    messages.clear();
    messages.extend(head);
    messages.push(ChatMessage::simple(
        1,
        "[Prior search rounds omitted to reduce payload. Provide your best answer based on available context.]",
    ));
    messages.extend(tail);
    true
}

pub fn get_repo_map(project_root: &Path, target_depth: usize, exclude_paths: &[String]) -> RepoMap {
    let executor = ToolExecutor::new(project_root);
    for depth in (1..=target_depth).rev() {
        let tree = executor.tree("/codebase", Some(depth), Some(exclude_paths), false);
        let size_bytes = tree.len();
        if size_bytes <= MAX_TREE_BYTES {
            return RepoMap {
                tree,
                depth,
                size_bytes,
                fell_back: depth < target_depth,
            };
        }
    }

    match std::fs::read_dir(project_root) {
        Ok(entries) => {
            let mut names = entries
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| !exclude_paths.iter().any(|pat| glob_match(pat, name)))
                .collect::<Vec<_>>();
            names.sort();
            let tree = std::iter::once("/codebase".to_string())
                .chain(names.into_iter().map(|name| format!("├── {name}")))
                .collect::<Vec<_>>()
                .join("\n");
            RepoMap {
                size_bytes: tree.len(),
                tree,
                depth: 0,
                fell_back: true,
            }
        }
        Err(_) => {
            let tree = "/codebase\n(empty or inaccessible)".to_string();
            RepoMap {
                size_bytes: tree.len(),
                tree,
                depth: 0,
                fell_back: true,
            }
        }
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
    globset::Glob::new(pattern)
        .map(|glob| glob.compile_matcher().is_match(text))
        .unwrap_or(false)
}

pub fn parse_answer(xml_text: &str, project_root: &Path) -> SearchResult {
    let mut files = Vec::new();
    let resolved_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let file_regex = Regex::new(r#"<file\s+path=["']([^"']+)["']>([\s\S]*?)</file>"#).unwrap();
    let range_regex = Regex::new(r"<range>(\d+)-(\d+)</range>").unwrap();

    for captures in file_regex.captures_iter(xml_text) {
        let Some(vpath) = captures.get(1).map(|m| m.as_str()) else {
            continue;
        };
        let body = captures.get(2).map(|m| m.as_str()).unwrap_or_default();
        let rel = vpath
            .strip_prefix("/codebase")
            .unwrap_or(vpath)
            .trim_start_matches(['/', '\\'])
            .replace('\\', "/");

        let rel_path = Path::new(&rel);
        if rel_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        }) {
            continue;
        }

        let full_path = resolved_root.join(&rel);
        let ranges = range_regex
            .captures_iter(body)
            .filter_map(|range| {
                let start = range.get(1)?.as_str().parse::<usize>().ok()?;
                let end = range.get(2)?.as_str().parse::<usize>().ok()?;
                Some((start, end))
            })
            .collect::<Vec<_>>();

        files.push(FileEntry {
            path: rel,
            full_path: full_path.to_string_lossy().into_owned(),
            ranges,
        });
    }

    SearchResult {
        files,
        ..SearchResult::default()
    }
}

pub async fn search(
    options: SearchOptions,
    on_progress: Option<&(dyn Fn(&str) + Sync)>,
) -> SearchResult {
    search_with_streaming(
        options,
        on_progress,
        |proto, timeout_ms, max_retries, ls_version| async move {
            streaming_request(&proto, timeout_ms, max_retries, ls_version.as_deref()).await
        },
    )
    .await
}

pub async fn search_with_streaming<F, Fut>(
    mut options: SearchOptions,
    on_progress: Option<&(dyn Fn(&str) + Sync)>,
    mut streaming: F,
) -> SearchResult
where
    F: FnMut(Vec<u8>, u64, u32, Option<String>) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, FastContextError>>,
{
    let project_root = options
        .project_root
        .canonicalize()
        .unwrap_or_else(|_| options.project_root.clone());
    options.project_root = project_root.clone();

    let log = |message: &str| {
        if let Some(on_progress) = on_progress {
            on_progress(message);
        }
    };

    let api_key = match get_api_key(options.api_key.as_deref(), true) {
        Ok(api_key) => api_key,
        Err(err) => {
            return SearchResult {
                error: Some(err),
                meta: SearchMeta {
                    project_root: Some(project_root.to_string_lossy().into_owned()),
                    error_code: Some("API_KEY_ERROR".to_string()),
                    ..SearchMeta::default()
                },
                ..SearchResult::default()
            };
        }
    };

    let mut timeout_ms = options.timeout_ms;
    if let Ok(raw_timeout) = env::var("TIMEOUT")
        && let Ok(value) = raw_timeout.parse::<u64>()
    {
        timeout_ms = value;
    }

    let jwt = match options.jwt.clone() {
        Some(jwt) => jwt,
        None => match get_cached_jwt(&api_key, timeout_ms).await {
            Ok(jwt) => jwt,
            Err(err) => {
                return SearchResult {
                    error: Some(format!("{}: {}", err.code, err.message)),
                    meta: SearchMeta {
                        project_root: Some(project_root.to_string_lossy().into_owned()),
                        error_code: Some(err.code),
                        ..SearchMeta::default()
                    },
                    ..SearchResult::default()
                };
            }
        },
    };

    let executor = ToolExecutor::with_limits(
        &project_root,
        options.result_max_lines,
        options.line_max_chars,
    );
    let tool_defs = get_tool_definitions(options.max_commands);
    let system_prompt =
        build_system_prompt(options.max_turns, options.max_commands, options.max_results);

    let repo_map = get_repo_map(&project_root, options.tree_depth, &options.exclude_paths);
    let tree_size_kb = repo_map.size_bytes as f64 / 1024.0;
    log(&format!(
        "Repo map: tree -L {} ({tree_size_kb:.1}KB){}",
        repo_map.depth,
        if repo_map.fell_back {
            " [fell back]"
        } else {
            ""
        }
    ));

    let user_content = format!(
        "Problem Statement: {}\n\nRepo Map (tree -L {} /codebase):\n```text\n{}\n```",
        options.query, repo_map.depth, repo_map.tree
    );

    let mut messages = vec![
        ChatMessage::simple(5, system_prompt),
        ChatMessage::simple(1, user_content),
    ];

    let total_api_calls = options.max_turns + 1;
    let mut compensated_turns = 0_usize;
    let max_compensations = 2_usize;
    let mut force_answer_injected = false;

    for turn in 0..(total_api_calls + max_compensations) {
        if turn >= total_api_calls + compensated_turns {
            break;
        }

        log(&format!(
            "Turn {}/{}",
            turn + 1,
            total_api_calls + compensated_turns
        ));

        let proto = build_request(
            &api_key,
            &jwt,
            &messages,
            &tool_defs,
            options.app_version.as_deref(),
            options.ls_version.as_deref(),
        );

        let response = match streaming(proto, timeout_ms, 2, options.ls_version.clone()).await {
            Ok(response) => response,
            Err(err) => {
                let base_meta = SearchMeta {
                    tree_depth: Some(repo_map.depth),
                    tree_size_kb: Some((tree_size_kb * 10.0).round() / 10.0),
                    fell_back: Some(repo_map.fell_back),
                    project_root: Some(project_root.to_string_lossy().into_owned()),
                    error_code: Some(err.code.clone()),
                    ..SearchMeta::default()
                };

                if matches!(err.code.as_str(), "PAYLOAD_TOO_LARGE" | "TIMEOUT")
                    && messages.len() > 4
                {
                    log(&format!(
                        "{} on turn {}: trimming context and retrying...",
                        err.code,
                        turn + 1
                    ));
                    trim_messages(&mut messages);
                    let retry_proto = build_request(
                        &api_key,
                        &jwt,
                        &messages,
                        &tool_defs,
                        options.app_version.as_deref(),
                        options.ls_version.as_deref(),
                    );
                    match streaming(retry_proto, timeout_ms, 2, options.ls_version.clone()).await {
                        Ok(response) => response,
                        Err(retry_err) => {
                            return SearchResult {
                                files: Vec::new(),
                                error: Some(format!(
                                    "{}: {} (retry failure)",
                                    retry_err.code, retry_err.message
                                )),
                                meta: SearchMeta {
                                    error_code: Some(retry_err.code),
                                    context_trimmed: Some(true),
                                    ..base_meta
                                },
                                ..SearchResult::default()
                            };
                        }
                    }
                } else {
                    return SearchResult {
                        files: Vec::new(),
                        error: Some(format!("{}: {}", err.code, err.message)),
                        meta: base_meta,
                        ..SearchResult::default()
                    };
                }
            }
        };

        let (thinking, tool_info) = parse_response(&response);
        let Some((tool_name, tool_args)) = tool_info else {
            if thinking.starts_with("[Error]") {
                return SearchResult {
                    files: Vec::new(),
                    error: Some(thinking),
                    ..SearchResult::default()
                };
            }
            return SearchResult {
                files: Vec::new(),
                raw_response: Some(thinking),
                ..SearchResult::default()
            };
        };

        if tool_name == "answer" {
            let answer_xml = tool_args
                .get("answer")
                .and_then(Value::as_str)
                .unwrap_or_default();
            log("Received final answer");
            let mut result = parse_answer(answer_xml, &project_root);
            result.rg_patterns = unique_patterns(executor.collected_rg_patterns());
            result.meta = SearchMeta {
                tree_depth: Some(repo_map.depth),
                tree_size_kb: Some((tree_size_kb * 10.0).round() / 10.0),
                fell_back: Some(repo_map.fell_back),
                ..SearchMeta::default()
            };
            return result;
        }

        if tool_name == "restricted_exec" {
            let call_id = Uuid::new_v4().to_string();
            let limited_args = limit_tool_args(&tool_args, options.max_commands);
            let args_json = limited_args.to_string();
            let cmds = command_keys(&limited_args);
            log(&format!("Executing {} local commands", cmds.len()));

            let results = executor.exec_tool_call(&limited_args);
            if valid_command_count(&limited_args) == 0 && compensated_turns < max_compensations {
                compensated_turns += 1;
                log(&format!(
                    "Turn compensation: no valid commands, extending search ({compensated_turns}/{max_compensations})"
                ));
            } else if valid_command_count(&limited_args) == 0 {
                log("Turn compensation skipped: limit reached");
            }

            messages.push(ChatMessage {
                role: 2,
                content: thinking,
                tool_call_id: Some(call_id.clone()),
                tool_name: Some("restricted_exec".to_string()),
                tool_args_json: Some(args_json),
                ref_call_id: None,
            });
            messages.push(ChatMessage {
                role: 4,
                content: results,
                tool_call_id: None,
                tool_name: None,
                tool_args_json: None,
                ref_call_id: Some(call_id),
            });

            let effective_turn = turn.saturating_sub(compensated_turns);
            if effective_turn >= options.max_turns.saturating_sub(1) && !force_answer_injected {
                messages.push(ChatMessage::simple(1, FINAL_FORCE_ANSWER));
                force_answer_injected = true;
                log("Injected force-answer prompt");
            }
        }
    }

    SearchResult {
        files: Vec::new(),
        rg_patterns: unique_patterns(executor.collected_rg_patterns()),
        error: Some("Max turns reached without getting an answer".to_string()),
        meta: SearchMeta {
            tree_depth: Some(repo_map.depth),
            tree_size_kb: Some((tree_size_kb * 10.0).round() / 10.0),
            fell_back: Some(repo_map.fell_back),
            project_root: Some(project_root.to_string_lossy().into_owned()),
            ..SearchMeta::default()
        },
        ..SearchResult::default()
    }
}

fn unique_patterns(patterns: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for pattern in patterns {
        if seen.insert(pattern.clone()) {
            unique.push(pattern);
        }
    }
    unique
}

pub async fn search_with_content(options: SearchOptions) -> String {
    let max_turns = options.max_turns;
    let max_results = options.max_results;
    let result = search(options.clone(), None).await;

    if let Some(error) = result.error {
        let mut err_msg = format!("Error: {error}");
        if result.meta.tree_depth.is_some() || result.meta.error_code.is_some() {
            err_msg.push_str(&format!(
                "\n\n[diagnostic] error_type={}, tree_depth_used={:?}, tree_size={:?}KB",
                result.meta.error_code.as_deref().unwrap_or("unknown"),
                result.meta.tree_depth,
                result.meta.tree_size_kb
            ));
            if result.meta.fell_back == Some(true) {
                err_msg.push_str(" (auto fell back)");
            }
            if result.meta.context_trimmed == Some(true) {
                err_msg.push_str(", context_trimmed=true");
            }
            if let Some(project_root) = result.meta.project_root {
                err_msg.push_str(&format!("\n[diagnostic] project_path={project_root}"));
            }
            err_msg.push_str(&format!(
                "\n[config] max_turns={max_turns}, max_results={max_results}, max_commands={}",
                options.max_commands
            ));

            match result.meta.error_code.as_deref() {
                Some("PAYLOAD_TOO_LARGE" | "TIMEOUT") => {
                    err_msg.push_str(
                        "\n[hint] Try: reduce tree_depth, add exclude_paths, or narrow project_path.",
                    );
                }
                Some("AUTH_ERROR") => {
                    err_msg.push_str(
                        "\n[hint] Authentication error. Ensure a fresh WINDSURF_API_KEY is configured.",
                    );
                }
                Some("RATE_LIMITED") => {
                    err_msg.push_str("\n[hint] Rate limited. Wait a moment and retry.");
                }
                _ => {}
            }
        }
        return err_msg;
    }

    let unique_patterns = result
        .rg_patterns
        .iter()
        .filter(|pattern| pattern.len() >= 3)
        .cloned()
        .collect::<Vec<_>>();

    if result.files.is_empty() && unique_patterns.is_empty() {
        return result
            .raw_response
            .map(|raw| format!("No relevant files found.\n\nRaw response:\n{raw}"))
            .unwrap_or_else(|| "No relevant files found.".to_string());
    }

    let mut parts = Vec::new();
    if result.files.is_empty() {
        parts.push("No files found.".to_string());
    } else {
        parts.push(format!("Found {} relevant files.\n", result.files.len()));
        for (idx, entry) in result.files.iter().enumerate() {
            let ranges = entry
                .ranges
                .iter()
                .map(|(start, end)| format!("L{start}-{end}"))
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!(
                "  [{}/{}] {} ({ranges})",
                idx + 1,
                result.files.len(),
                entry.full_path
            ));
        }
    }

    if !unique_patterns.is_empty() {
        parts.push(String::new());
        parts.push(format!("grep keywords: {}", unique_patterns.join(", ")));
    }

    if result.meta.tree_depth.is_some() {
        parts.push(String::new());
        let fb_note = if result.meta.fell_back == Some(true) {
            " (fell back)"
        } else {
            ""
        };
        parts.push(format!(
            "[config] tree_depth={}{}{}, tree_size={}KB, max_turns={max_turns}, max_results={max_results}",
            result.meta.tree_depth.unwrap_or_default(),
            fb_note,
            "",
            result.meta.tree_size_kb.unwrap_or_default()
        ));
    }

    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protobuf::{ProtobufEncoder, connect_frame_encode};
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    #[test]
    fn decode_unary_response_decompresses_gzip() {
        let data = gzip_compress(b"proto-response").unwrap();
        assert_eq!(
            decode_unary_response(&data, Some("gzip")),
            b"proto-response"
        );
        assert_eq!(decode_unary_response(&data, None), b"proto-response");
    }

    #[test]
    fn limit_tool_args_enforces_max_commands() {
        let tool_args = json!({
            "command3": {"type": "rg"},
            "command1": {"type": "tree"},
            "command2": {"type": "readfile"},
            "command10": {"type": "ls"}
        });

        assert_eq!(
            limit_tool_args(&tool_args, 2),
            json!({
                "command1": {"type": "tree"},
                "command2": {"type": "readfile"}
            })
        );
    }

    #[test]
    fn trim_messages_keeps_head_bridge_and_tail() {
        let mut messages = vec![
            ChatMessage::simple(5, "system"),
            ChatMessage::simple(1, "user"),
            ChatMessage::simple(2, "thinking 1"),
            ChatMessage::simple(4, "result 1"),
            ChatMessage::simple(2, "thinking 2"),
            ChatMessage::simple(4, "result 2"),
        ];

        assert!(trim_messages(&mut messages));
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0].content, "system");
        assert_eq!(messages[1].content, "user");
        assert!(messages[2].content.contains("omitted"));
        assert_eq!(messages[3].content, "thinking 2");
        assert_eq!(messages[4].content, "result 2");
    }

    #[test]
    fn parse_answer_filters_path_traversal() {
        let xml = r#"
        Some thoughts first.
        <ANSWER>
          <file path="/codebase/src/main.py">
            <range>10-20</range>
            <range>30-40</range>
          </file>
          <file path="/codebase/tests/test_main.py">
            <range>1-5</range>
          </file>
          <file path="/codebase/../../etc/passwd">
            <range>1-2</range>
          </file>
        </ANSWER>
        "#;
        let tmp = TempDir::new().unwrap();
        let result = parse_answer(xml, tmp.path());
        assert_eq!(result.files.len(), 2);
        assert_eq!(result.files[0].path, "src/main.py");
        assert_eq!(result.files[0].ranges, vec![(10, 20), (30, 40)]);
        assert_eq!(result.files[1].path, "tests/test_main.py");
        assert_eq!(result.files[1].ranges, vec![(1, 5)]);
    }

    #[test]
    fn get_repo_map_uses_untruncated_tree() {
        let tmp = TempDir::new().unwrap();
        for i in 0..60 {
            fs::write(tmp.path().join(format!("file_{i:03}.txt")), "").unwrap();
        }

        let result = get_repo_map(tmp.path(), 1, &[]);
        assert!(!result.tree.contains("... (lines truncated) ..."));
        assert!(result.tree.contains("file_059.txt"));
        assert_eq!(result.size_bytes, result.tree.len());
    }

    #[test]
    fn jwt_exp_decodes_payload() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"exp":12345}"#);
        assert_eq!(get_jwt_exp(&format!("header.{payload}.sig")), 12345.0);
        assert_eq!(get_jwt_exp("not-a-jwt"), 0.0);
    }

    #[tokio::test]
    async fn search_loop_success_with_mock_streaming() {
        let mut t1_encoder = ProtobufEncoder::new();
        t1_encoder.write_string(1, "thinking about doing search");
        t1_encoder.write_string(
            2,
            r#"[TOOL_CALLS]restricted_exec[ARGS]{"command1": {"type": "readfile", "file": "/codebase/test.txt"}}"#,
        );
        let t1_frame = connect_frame_encode(&t1_encoder.to_bytes(), false);

        let mut t2_encoder = ProtobufEncoder::new();
        t2_encoder.write_string(1, "found answer");
        t2_encoder.write_string(
            2,
            r#"[TOOL_CALLS]answer[ARGS]{"answer": "<ANSWER><file path=\"/codebase/test.txt\"><range>1-10</range></file></ANSWER>"}"#,
        );
        let t2_frame = connect_frame_encode(&t2_encoder.to_bytes(), false);

        let responses = Arc::new(Mutex::new(VecDeque::from([t1_frame, t2_frame])));
        let call_count = Arc::new(AtomicUsize::new(0));
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.txt"), "line1\nline2").unwrap();

        let mut options = SearchOptions::new("find main", tmp.path());
        options.api_key = Some("sk-ws-01-key".to_string());
        options.jwt = Some("mocked.jwt.token".to_string());
        options.max_turns = 2;

        let result = search_with_streaming(options, None, {
            let responses = Arc::clone(&responses);
            let call_count = Arc::clone(&call_count);
            move |_, _, _, _| {
                let responses = Arc::clone(&responses);
                let call_count = Arc::clone(&call_count);
                async move {
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(responses.lock().unwrap().pop_front().unwrap())
                }
            }
        })
        .await;

        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, "test.txt");
        assert_eq!(result.files[0].ranges, vec![(1, 10)]);
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn check_auth_success_with_explicit_key() {
        let result = check_auth(Some("fake-api-key"), None, None);
        assert!(result.ok);
        assert_eq!(result.jwt_source, "api-key");
    }
}
