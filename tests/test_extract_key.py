import json
import sqlite3
import tempfile
from pathlib import Path

from swegrep_cli.credentials import discover_api_key, extract_key, looks_truncated_api_key


def test_extract_key_success() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        db_path = Path(tmpdir) / "state.vscdb"
        conn = sqlite3.connect(db_path)
        cursor = conn.cursor()
        cursor.execute("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)")
        auth_status = {"apiKey": "sk-ws-01-testkey123456"}
        cursor.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
            ("windsurfAuthStatus", json.dumps(auth_status)),
        )
        conn.commit()
        conn.close()

        result = extract_key(db_path)
        assert "api_key" in result
        assert result["api_key"] == "sk-ws-01-testkey123456"
        assert result["db_path"] == str(db_path)


def test_extract_key_missing_record() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        db_path = Path(tmpdir) / "state.vscdb"
        conn = sqlite3.connect(db_path)
        cursor = conn.cursor()
        cursor.execute("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)")
        conn.commit()
        conn.close()

        result = extract_key(db_path)
        assert "error" in result
        assert "windsurfAuthStatus record not found" in result["error"]


def test_extract_key_empty_key() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        db_path = Path(tmpdir) / "state.vscdb"
        conn = sqlite3.connect(db_path)
        cursor = conn.cursor()
        cursor.execute("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)")
        auth_status = {"apiKey": ""}
        cursor.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
            ("windsurfAuthStatus", json.dumps(auth_status)),
        )
        conn.commit()
        conn.close()

        result = extract_key(db_path)
        assert "error" in result
        assert "apiKey field is empty" in result["error"]


def test_extract_key_keeps_unknown_key_format() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        db_path = Path(tmpdir) / "state.vscdb"
        conn = sqlite3.connect(db_path)
        cursor = conn.cursor()
        cursor.execute("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)")
        auth_status = {"apiKey": "not-a-windsurf-key"}
        cursor.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
            ("windsurfAuthStatus", json.dumps(auth_status)),
        )
        conn.commit()
        conn.close()

        result = extract_key(db_path)
        assert result["api_key"] == "not-a-windsurf-key"
        assert result["key_type"] == "unknown"


def test_extract_key_accepts_session_token_key() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        db_path = Path(tmpdir) / "state.vscdb"
        conn = sqlite3.connect(db_path)
        cursor = conn.cursor()
        cursor.execute("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)")
        auth_status = {"apiKey": "devin-session-token$eyJhbGciOiJIUzI1NiJ9.payload.signature"}
        cursor.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
            ("windsurfAuthStatus", json.dumps(auth_status)),
        )
        conn.commit()
        conn.close()

        result = extract_key(db_path)
        assert result["api_key"] == "devin-session-token$eyJhbGciOiJIUzI1NiJ9.payload.signature"
        assert result["key_type"] == "session-token"


def test_discover_api_key_accepts_session_token_key() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        db_path = Path(tmpdir) / "state.vscdb"
        conn = sqlite3.connect(db_path)
        cursor = conn.cursor()
        cursor.execute("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)")
        auth_status = {"apiKey": "devin-session-token$eyJhbGciOiJIUzI1NiJ9.payload.signature"}
        cursor.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
            ("windsurfAuthStatus", json.dumps(auth_status)),
        )
        conn.commit()
        conn.close()

        assert discover_api_key(db_path) == "devin-session-token$eyJhbGciOiJIUzI1NiJ9.payload.signature"


def test_looks_truncated_api_key_detects_missing_session_jwt() -> None:
    assert looks_truncated_api_key("devin-session-token")
    assert looks_truncated_api_key("devin-session-token$")
    assert not looks_truncated_api_key(
        "devin-session-token$eyJhbGciOiJIUzI1NiJ9.payload.signature"
    )
    assert not looks_truncated_api_key("sk-ws-01-testkey123456")


def test_extract_key_not_exist() -> None:
    db_path = Path("/nonexistent/path/to/db.vscdb")
    result = extract_key(db_path)
    assert "error" in result
    assert "Windsurf database not found" in result["error"]
