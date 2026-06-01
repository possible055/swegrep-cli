from __future__ import annotations

import json
import os
import sqlite3
import stat
import sys
from pathlib import Path

CONFIG_KEY = "WINDSURF_API_KEY"
WINDSURF_AUTH_STATUS_KEY = "windsurfAuthStatus"
WINDSURF_API_KEY_FIELD = "apiKey"


def get_config_path() -> Path:
    home = Path.home()
    if sys.platform == "win32":
        return home / ".swegrep" / "config.json"
    return home / ".config" / "swegrep" / "config.json"


def load_cached_api_key(config_path: Path | None = None) -> str | None:
    path = config_path or get_config_path()
    if not path.exists():
        return None
    try:
        with path.open(encoding="utf-8") as f:
            data = json.load(f)
        key = data.get(CONFIG_KEY)
        return key if isinstance(key, str) and key else None
    except Exception:
        return None


def save_cached_api_key(key: str, config_path: Path | None = None) -> Path:
    if not key:
        raise ValueError("API key is empty")

    path = config_path or get_config_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as f:
        json.dump({CONFIG_KEY: key}, f, indent=2)
        f.write("\n")

    if sys.platform != "win32":
        path.chmod(stat.S_IRUSR | stat.S_IWUSR)
    return path


def classify_api_key(value: str) -> str:
    if value.startswith("sk-"):
        return "standard"
    if value.startswith("devin-session-token$"):
        _, jwt = value.split("$", 1)
        if jwt.startswith("eyJ") and "." in jwt:
            return "session-token"
    if "$" in value:
        _, jwt = value.split("$", 1)
        if jwt.startswith("eyJ") and "." in jwt:
            return "embedded-jwt"
    return "unknown"


def is_supported_api_key(value: str) -> bool:
    return bool(value.strip())


def looks_truncated_api_key(value: str) -> bool:
    key = value.strip()
    if not key.startswith("devin-session-token"):
        return False
    if "$" not in key:
        return True
    _, jwt = key.split("$", 1)
    return not jwt.startswith("eyJ")


def get_windsurf_db_path() -> Path:
    home = Path.home()
    if sys.platform == "darwin":
        return (
            home
            / "Library"
            / "Application Support"
            / "Windsurf"
            / "User"
            / "globalStorage"
            / "state.vscdb"
        )
    if sys.platform == "win32":
        appdata_str = os.environ.get("APPDATA")
        if not appdata_str:
            raise RuntimeError("Cannot determine APPDATA path")
        return Path(appdata_str) / "Windsurf" / "User" / "globalStorage" / "state.vscdb"

    c_users = Path("/mnt/c/Users")
    if c_users.exists():
        try:
            for user_dir in c_users.iterdir():
                if user_dir.is_dir() and not user_dir.name.startswith("."):
                    candidate = (
                        user_dir
                        / "AppData"
                        / "Roaming"
                        / "Windsurf"
                        / "User"
                        / "globalStorage"
                        / "state.vscdb"
                    )
                    try:
                        exists = candidate.exists()
                    except OSError:
                        continue
                    if exists:
                        return candidate
        except Exception:
            pass

    xdg_config = os.environ.get("XDG_CONFIG_HOME")
    config_dir = Path(xdg_config) if xdg_config else (home / ".config")
    return config_dir / "Windsurf" / "User" / "globalStorage" / "state.vscdb"


def extract_key(db_path: Path | str | None = None) -> dict[str, str]:
    if db_path is None:
        try:
            path = get_windsurf_db_path()
        except Exception as e:
            return {"error": f"Cannot determine database path: {e}", "db_path": ""}
    else:
        path = Path(db_path)

    if not path.exists():
        return {
            "error": f"Windsurf database not found: {path}",
            "hint": "Ensure Windsurf is installed and logged in.",
            "db_path": str(path),
        }

    try:
        db_uri = f"file:{path.as_posix()}?mode=ro"
        conn = sqlite3.connect(db_uri, uri=True)
    except Exception:
        try:
            conn = sqlite3.connect(path)
        except Exception as e:
            return {"error": f"Failed to open database: {e}", "db_path": str(path)}

    try:
        cursor = conn.cursor()
        cursor.execute("SELECT value FROM ItemTable WHERE key = ?", (WINDSURF_AUTH_STATUS_KEY,))
        row = cursor.fetchone()
        if not row:
            return {
                "error": "windsurfAuthStatus record not found",
                "hint": "Ensure Windsurf is logged in.",
                "db_path": str(path),
            }

        try:
            data = json.loads(row[0])
        except Exception:
            return {"error": "windsurfAuthStatus data parse failed", "db_path": str(path)}

        api_key = data.get(WINDSURF_API_KEY_FIELD, "")
        if not api_key:
            return {"error": "apiKey field is empty", "db_path": str(path)}
        if not isinstance(api_key, str):
            return {"error": "apiKey field is not a string", "db_path": str(path)}

        return {"api_key": api_key, "db_path": str(path), "key_type": classify_api_key(api_key)}
    except Exception as e:
        return {"error": f"Extraction failed: {e}", "db_path": str(path)}
    finally:
        conn.close()


def discover_api_key(db_path: Path | str | None = None) -> str | None:
    result = extract_key(db_path)
    key = result.get("api_key")
    if not key or not is_supported_api_key(key):
        return None
    return key


def mask_api_key(key: str) -> str:
    if len(key) <= 16:
        return "*" * len(key)
    return f"{key[:10]}...{key[-6:]}"
