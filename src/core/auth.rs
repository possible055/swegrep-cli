use crate::credentials;
use crate::protobuf::{ProtobufEncoder, extract_strings};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE;
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::http::unary_request;
use super::{
    AUTH_BASE, AuthCheck, DEFAULT_WS_APP_VER, DEFAULT_WS_LS_VER, FastContextError, WS_APP,
};

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

pub(super) fn ws_app_version(value: Option<&str>) -> String {
    protocol_setting(value, "WS_APP_VER", DEFAULT_WS_APP_VER)
}

pub(super) fn ws_ls_version(value: Option<&str>) -> String {
    protocol_setting(value, "WS_LS_VER", DEFAULT_WS_LS_VER)
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

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    #[test]
    fn jwt_exp_decodes_payload() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"exp":12345}"#);
        assert_eq!(get_jwt_exp(&format!("header.{payload}.sig")), 12345.0);
        assert_eq!(get_jwt_exp("not-a-jwt"), 0.0);
    }

    #[test]
    fn check_auth_success_with_explicit_key() {
        let result = check_auth(Some("fake-api-key"), None, None);
        assert!(result.ok);
        assert_eq!(result.jwt_source, "api-key");
    }
}
