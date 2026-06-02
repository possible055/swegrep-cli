use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub const CONFIG_KEY: &str = "WINDSURF_API_KEY";
pub const WINDSURF_AUTH_STATUS_KEY: &str = "windsurfAuthStatus";
pub const WINDSURF_API_KEY_FIELD: &str = "apiKey";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractKeyResult {
    pub api_key: Option<String>,
    pub error: Option<String>,
    pub hint: Option<String>,
    pub db_path: String,
    pub key_type: Option<String>,
}

impl ExtractKeyResult {
    fn success(api_key: String, db_path: &Path) -> Self {
        let key_type = classify_api_key(&api_key).to_string();
        Self {
            api_key: Some(api_key),
            error: None,
            hint: None,
            db_path: db_path.to_string_lossy().into_owned(),
            key_type: Some(key_type),
        }
    }

    fn error(message: impl Into<String>, db_path: impl Into<String>) -> Self {
        Self {
            api_key: None,
            error: Some(message.into()),
            hint: None,
            db_path: db_path.into(),
            key_type: None,
        }
    }

    fn error_with_hint(
        message: impl Into<String>,
        hint: impl Into<String>,
        db_path: impl Into<String>,
    ) -> Self {
        Self {
            api_key: None,
            error: Some(message.into()),
            hint: Some(hint.into()),
            db_path: db_path.into(),
            key_type: None,
        }
    }
}

pub fn get_config_path() -> PathBuf {
    let home = home_dir();
    if cfg!(target_os = "windows") {
        home.join(".swegrep").join("config.json")
    } else {
        home.join(".config").join("swegrep").join("config.json")
    }
}

fn home_dir() -> PathBuf {
    if cfg!(target_os = "windows") {
        env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

pub fn load_cached_api_key(config_path: Option<&Path>) -> Option<String> {
    let path = config_path
        .map(Path::to_path_buf)
        .unwrap_or_else(get_config_path);
    let text = fs::read_to_string(path).ok()?;
    let data: Value = serde_json::from_str(&text).ok()?;
    data.get(CONFIG_KEY)
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .map(ToOwned::to_owned)
}

pub fn save_cached_api_key(key: &str, config_path: Option<&Path>) -> Result<PathBuf, String> {
    if key.is_empty() {
        return Err("API key is empty".to_string());
    }

    let path = config_path
        .map(Path::to_path_buf)
        .unwrap_or_else(get_config_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }

    let mut data = serde_json::Map::new();
    data.insert(CONFIG_KEY.to_string(), Value::String(key.to_string()));
    let text = serde_json::to_string_pretty(&Value::Object(data)).map_err(|err| err.to_string())?;
    fs::write(&path, format!("{text}\n")).map_err(|err| err.to_string())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|err| err.to_string())?;
    }

    Ok(path)
}

pub fn classify_api_key(value: &str) -> &'static str {
    if value.starts_with("sk-") {
        return "standard";
    }
    if let Some((_, jwt)) = value.split_once('$')
        && jwt.starts_with("eyJ")
        && jwt.contains('.')
    {
        return if value.starts_with("devin-session-token$") {
            "session-token"
        } else {
            "embedded-jwt"
        };
    }
    "unknown"
}

pub fn is_supported_api_key(value: &str) -> bool {
    !value.trim().is_empty()
}

pub fn looks_truncated_api_key(value: &str) -> bool {
    let key = value.trim();
    if !key.starts_with("devin-session-token") {
        return false;
    }
    let Some((_, jwt)) = key.split_once('$') else {
        return true;
    };
    !jwt.starts_with("eyJ")
}

pub fn get_windsurf_db_path() -> Result<PathBuf, String> {
    let home = home_dir();

    if cfg!(target_os = "macos") {
        return Ok(home
            .join("Library")
            .join("Application Support")
            .join("Windsurf")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb"));
    }

    if cfg!(target_os = "windows") {
        let appdata = env::var_os("APPDATA").ok_or("Cannot determine APPDATA path")?;
        return Ok(PathBuf::from(appdata)
            .join("Windsurf")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb"));
    }

    let c_users = Path::new("/mnt/c/Users");
    if c_users.exists()
        && let Ok(users) = fs::read_dir(c_users)
    {
        for entry in users.flatten() {
            let user_dir = entry.path();
            if !user_dir.is_dir() {
                continue;
            }
            let Some(name) = user_dir.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let candidate = user_dir
                .join("AppData")
                .join("Roaming")
                .join("Windsurf")
                .join("User")
                .join("globalStorage")
                .join("state.vscdb");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }

    let config_dir = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    Ok(config_dir
        .join("Windsurf")
        .join("User")
        .join("globalStorage")
        .join("state.vscdb"))
}

pub fn extract_key(db_path: Option<&Path>) -> ExtractKeyResult {
    let path = match db_path {
        Some(path) => path.to_path_buf(),
        None => match get_windsurf_db_path() {
            Ok(path) => path,
            Err(err) => {
                return ExtractKeyResult::error(
                    format!("Cannot determine database path: {err}"),
                    "",
                );
            }
        },
    };

    if !path.exists() {
        return ExtractKeyResult::error_with_hint(
            format!("Windsurf database not found: {}", path.display()),
            "Ensure Windsurf is installed and logged in.",
            path.to_string_lossy(),
        );
    }

    let conn = match Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => conn,
        Err(_) => match Connection::open(&path) {
            Ok(conn) => conn,
            Err(err) => {
                return ExtractKeyResult::error(
                    format!("Failed to open database: {err}"),
                    path.to_string_lossy(),
                );
            }
        },
    };

    let extraction = extract_key_from_connection(&conn, &path);
    extraction.unwrap_or_else(|err| ExtractKeyResult::error(err, path.to_string_lossy()))
}

fn extract_key_from_connection(conn: &Connection, path: &Path) -> Result<ExtractKeyResult, String> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?",
            [WINDSURF_AUTH_STATUS_KEY],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| format!("Extraction failed: {err}"))?;

    let Some(value) = value else {
        return Ok(ExtractKeyResult::error_with_hint(
            "windsurfAuthStatus record not found",
            "Ensure Windsurf is logged in.",
            path.to_string_lossy(),
        ));
    };

    let data: Value = serde_json::from_str(&value)
        .map_err(|_| "windsurfAuthStatus data parse failed".to_string())?;
    let Some(api_key_value) = data.get(WINDSURF_API_KEY_FIELD) else {
        return Ok(ExtractKeyResult::error(
            "apiKey field is empty",
            path.to_string_lossy(),
        ));
    };
    let Some(api_key) = api_key_value.as_str() else {
        return Ok(ExtractKeyResult::error(
            "apiKey field is not a string",
            path.to_string_lossy(),
        ));
    };
    if api_key.is_empty() {
        return Ok(ExtractKeyResult::error(
            "apiKey field is empty",
            path.to_string_lossy(),
        ));
    }

    Ok(ExtractKeyResult::success(api_key.to_string(), path))
}

pub fn discover_api_key(db_path: Option<&Path>) -> Option<String> {
    let result = extract_key(db_path);
    result
        .api_key
        .filter(|api_key| is_supported_api_key(api_key))
}

pub fn mask_api_key(key: &str) -> String {
    if key.len() <= 16 {
        "*".repeat(key.len())
    } else {
        format!("{}...{}", &key[..10], &key[key.len() - 6..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use serde_json::json;
    use tempfile::TempDir;

    fn write_auth_db(db_path: &Path, value: Value) {
        let conn = Connection::open(db_path).unwrap();
        conn.execute(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            params![WINDSURF_AUTH_STATUS_KEY, value.to_string()],
        )
        .unwrap();
    }

    #[test]
    fn save_and_load_cache() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.json");

        assert_eq!(load_cached_api_key(Some(&config_path)), None);
        save_cached_api_key("sk-test-caching-key", Some(&config_path)).unwrap();
        assert_eq!(
            load_cached_api_key(Some(&config_path)),
            Some("sk-test-caching-key".to_string())
        );

        let data: Value = serde_json::from_str(&fs::read_to_string(config_path).unwrap()).unwrap();
        assert_eq!(data[CONFIG_KEY], "sk-test-caching-key");
    }

    #[test]
    fn extract_key_success() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("state.vscdb");
        write_auth_db(&db_path, json!({ "apiKey": "sk-ws-01-testkey123456" }));

        let result = extract_key(Some(&db_path));
        assert_eq!(result.api_key.as_deref(), Some("sk-ws-01-testkey123456"));
        assert_eq!(result.db_path, db_path.to_string_lossy());
    }

    #[test]
    fn extract_key_missing_record() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("state.vscdb");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .unwrap();

        let result = extract_key(Some(&db_path));
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("windsurfAuthStatus record not found")
        );
    }

    #[test]
    fn extract_key_empty_key() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("state.vscdb");
        write_auth_db(&db_path, json!({ "apiKey": "" }));

        let result = extract_key(Some(&db_path));
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("apiKey field is empty")
        );
    }

    #[test]
    fn extract_key_keeps_unknown_key_format() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("state.vscdb");
        write_auth_db(&db_path, json!({ "apiKey": "not-a-windsurf-key" }));

        let result = extract_key(Some(&db_path));
        assert_eq!(result.api_key.as_deref(), Some("not-a-windsurf-key"));
        assert_eq!(result.key_type.as_deref(), Some("unknown"));
    }

    #[test]
    fn extract_key_accepts_session_token_key() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("state.vscdb");
        let key = "devin-session-token$eyJhbGciOiJIUzI1NiJ9.payload.signature";
        write_auth_db(&db_path, json!({ "apiKey": key }));

        let result = extract_key(Some(&db_path));
        assert_eq!(result.api_key.as_deref(), Some(key));
        assert_eq!(result.key_type.as_deref(), Some("session-token"));
        assert_eq!(discover_api_key(Some(&db_path)).as_deref(), Some(key));
    }

    #[test]
    fn truncated_session_token_detection_matches_python() {
        assert!(looks_truncated_api_key("devin-session-token"));
        assert!(looks_truncated_api_key("devin-session-token$"));
        assert!(!looks_truncated_api_key(
            "devin-session-token$eyJhbGciOiJIUzI1NiJ9.payload.signature"
        ));
        assert!(!looks_truncated_api_key("sk-ws-01-testkey123456"));
    }

    #[test]
    fn extract_key_not_exist() {
        let db_path = Path::new("/nonexistent/path/to/db.vscdb");
        let result = extract_key(Some(db_path));
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("Windsurf database not found")
        );
    }
}
