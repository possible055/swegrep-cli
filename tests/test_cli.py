import sys
from unittest.mock import patch

from swegrep_cli.cli import main


def test_cli_extract_key() -> None:
    with patch.object(sys, "argv", ["swegrep-cli", "extract-key"]):
        with patch("swegrep_cli.cli.extract_key") as mock_extract:
            mock_extract.return_value = {"api_key": "sk-ws-01-mock", "db_path": "/fake/db"}
            with patch("builtins.print"):
                try:
                    main()
                except SystemExit as e:
                    assert e.code == 0
            mock_extract.assert_called_once_with(None)


def test_cli_extract_key_save() -> None:
    argv = ["swegrep-cli", "extract-key", "--db-path", "/fake/db", "--save"]
    with patch.object(sys, "argv", argv):
        with patch("swegrep_cli.cli.extract_key") as mock_extract:
            with patch("swegrep_cli.cli.save_cached_api_key") as mock_save:
                mock_extract.return_value = {"api_key": "sk-ws-01-mock", "db_path": "/fake/db"}
                mock_save.return_value = "/fake/config.json"
                with patch("builtins.print"):
                    try:
                        main()
                    except SystemExit as e:
                        assert e.code == 0
                mock_extract.assert_called_once_with("/fake/db")
                mock_save.assert_called_once_with("sk-ws-01-mock")


def test_cli_extract_key_save_accepts_session_token_key() -> None:
    argv = ["swegrep-cli", "extract-key", "--db-path", "/fake/db", "--save"]
    with patch.object(sys, "argv", argv):
        with patch("swegrep_cli.cli.extract_key") as mock_extract:
            with patch("swegrep_cli.cli.save_cached_api_key") as mock_save:
                mock_extract.return_value = {
                    "api_key": "devin-session-token$eyJ.fake.jwt",
                    "db_path": "/fake/db",
                    "key_type": "session-token",
                }
                mock_save.return_value = "/fake/config.json"
                with patch("builtins.print"):
                    try:
                        main()
                    except SystemExit as e:
                        assert e.code == 0
                mock_save.assert_called_once_with("devin-session-token$eyJ.fake.jwt")


def test_cli_extract_key_show() -> None:
    argv = ["swegrep-cli", "extract-key", "--db-path", "/fake/db", "--show"]
    with patch.object(sys, "argv", argv):
        with patch("swegrep_cli.cli.extract_key") as mock_extract:
            mock_extract.return_value = {"api_key": "sk-ws-01-mock", "db_path": "/fake/db"}
            with patch("builtins.print") as mock_print:
                try:
                    main()
                except SystemExit as e:
                    assert e.code == 0
                
                # Check that export command was printed
                printed_lines = [call.args[0] for call in mock_print.call_args_list if call.args]
                assert any('export WINDSURF_API_KEY="sk-ws-01-mock"' in line for line in printed_lines)


def test_cli_requires_rg() -> None:
    with patch.object(sys, "argv", ["swegrep-cli", "search", "dummy_query"]):
        with patch("swegrep_cli.cli.shutil.which", return_value=None):
            with patch("builtins.print") as mock_print:
                try:
                    main()
                except SystemExit as e:
                    assert e.code == 1
            assert "ripgrep" in mock_print.call_args.args[0]


def test_cli_search() -> None:
    argv = [
        "swegrep-cli",
        "search",
        "where is auth",
        "--path",
        ".",
        "--api-key",
        "sk-ws-01-key",
    ]
    with patch.object(sys, "argv", argv):
        with patch("swegrep_cli.cli.search") as mock_search:
            mock_search.return_value = {"files": []}
            with patch("builtins.print"):
                try:
                    main()
                except SystemExit as e:
                    assert e.code == 0
            mock_search.assert_called_once()
            kwargs = mock_search.call_args.kwargs
            assert kwargs["api_key"] == "sk-ws-01-key"

