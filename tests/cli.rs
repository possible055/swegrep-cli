use assert_cmd::Command;
use rusqlite::{Connection, params};
use serde_json::json;
use tempfile::TempDir;

fn write_auth_db(path: &std::path::Path, key: &str) {
    let conn = Connection::open(path).unwrap();
    conn.execute(
        "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
        params!["windsurfAuthStatus", json!({ "apiKey": key }).to_string()],
    )
    .unwrap();
}

#[test]
fn extract_key_show_prints_full_key_and_source() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("state.vscdb");
    write_auth_db(&db_path, "sk-ws-01-mock");

    let output = Command::cargo_bin("swegrep-cli")
        .unwrap()
        .args(["extract-key", "--show", "--db-path"])
        .arg(&db_path)
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("Windsurf API Key: sk-ws-01-mock"));
    assert!(stdout.contains("export WINDSURF_API_KEY=\"sk-ws-01-mock\""));
    assert!(stderr.contains("Source DB:"));
}

#[test]
fn search_requires_rg() {
    let output = Command::cargo_bin("swegrep-cli")
        .unwrap()
        .args(["search", "dummy_query"])
        .env("PATH", "")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ripgrep"));
}
