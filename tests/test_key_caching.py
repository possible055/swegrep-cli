import json
import os
import tempfile
from pathlib import Path
from unittest.mock import patch

import pytest

from swegrep_cli.core import (
    _load_cached_api_key,
    _save_cached_api_key,
    get_api_key,
    get_config_path,
)


def test_get_config_path() -> None:
    path = get_config_path()
    assert isinstance(path, Path)
    assert path.name == "config.json"
    assert "swegrep" in path.parts


def test_save_and_load_cache() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp_config = Path(tmpdir) / "config.json"

        with patch("swegrep_cli.core.get_config_path", return_value=tmp_config):
            assert _load_cached_api_key() is None

            _save_cached_api_key("sk-test-caching-key")

            assert _load_cached_api_key() == "sk-test-caching-key"

            with tmp_config.open(encoding="utf-8") as f:
                data = json.load(f)
                assert data["WINDSURF_API_KEY"] == "sk-test-caching-key"


@patch.dict(os.environ, {}, clear=True)
def test_get_api_key_priority() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp_config = Path(tmpdir) / "config.json"

        with patch.dict(os.environ, {"WINDSURF_API_KEY": "env-key"}):
            assert get_api_key() == "env-key"

        with patch("swegrep_cli.core.get_config_path", return_value=tmp_config):
            with patch("swegrep_cli.core._auto_discover_api_key", return_value="discovered-key"):
                assert get_api_key() == "discovered-key"
                assert _load_cached_api_key() == "discovered-key"

        if tmp_config.exists():
            tmp_config.unlink()
        with patch("swegrep_cli.core.get_config_path", return_value=tmp_config):
            _save_cached_api_key("cached-fallback-key")
            with patch("swegrep_cli.core._auto_discover_api_key", return_value="ignored-key"):
                assert get_api_key() == "cached-fallback-key"

        if tmp_config.exists():
            tmp_config.unlink()
        with patch("swegrep_cli.core.get_config_path", return_value=tmp_config):
            with patch("swegrep_cli.core._auto_discover_api_key", return_value=None):
                with pytest.raises(RuntimeError) as excinfo:
                    get_api_key()
                assert "Windsurf API Key not found" in str(excinfo.value)
