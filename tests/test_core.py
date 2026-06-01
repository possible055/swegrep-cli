import asyncio
import gzip
import json
import tempfile
from pathlib import Path
from unittest.mock import MagicMock, patch

from swegrep_cli.core import (
    _decode_unary_response,
    _limit_tool_args,
    _parse_answer,
    _trim_messages,
    check_auth,
    get_api_key,
    get_repo_map,
    search,
)
from swegrep_cli.protobuf import ProtobufEncoder, connect_frame_encode





def test_decode_unary_response_decompresses_gzip() -> None:
    data = gzip.compress(b"proto-response")

    assert _decode_unary_response(data, "gzip") == b"proto-response"
    assert _decode_unary_response(data, None) == b"proto-response"


def test_limit_tool_args_enforces_max_commands() -> None:
    tool_args = {
        "command3": {"type": "rg"},
        "command1": {"type": "tree"},
        "command2": {"type": "readfile"},
        "command10": {"type": "ls"},
    }

    assert _limit_tool_args(tool_args, 2) == {
        "command1": {"type": "tree"},
        "command2": {"type": "readfile"},
    }


def base64_url_encode(data: bytes) -> str:
    import base64

    return base64.urlsafe_b64encode(data).decode("utf-8").rstrip("=")


def test_trim_messages() -> None:
    messages = [
        {"role": 5, "content": "system"},
        {"role": 1, "content": "user"},
        {"role": 2, "content": "thinking 1"},
        {"role": 4, "content": "result 1"},
        {"role": 2, "content": "thinking 2"},
        {"role": 4, "content": "result 2"},
    ]
    trimmed = _trim_messages(messages)
    assert trimmed is True
    assert len(messages) == 5
    assert messages[0]["content"] == "system"
    assert messages[1]["content"] == "user"
    assert "omitted" in messages[2]["content"]
    assert messages[3]["content"] == "thinking 2"
    assert messages[4]["content"] == "result 2"


def test_parse_answer() -> None:
    xml = """
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
    """
    with tempfile.TemporaryDirectory() as tmpdir:
        res = _parse_answer(xml, tmpdir)
        files = res["files"]
        assert len(files) == 2
        # Verify paths are correct and path traversal was filtered
        assert files[0]["path"] == "src/main.py"
        assert files[0]["ranges"] == [[10, 20], [30, 40]]
        assert files[1]["path"] == "tests/test_main.py"
        assert files[1]["ranges"] == [[1, 5]]





def test_get_repo_map_uses_untruncated_tree() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        for i in range(60):
            Path(tmpdir, f"file_{i:03}.txt").touch()

        result = get_repo_map(tmpdir, target_depth=1)

        assert "... (lines truncated) ..." not in result["tree"]
        assert "file_059.txt" in result["tree"]
        assert result["size_bytes"] == len(result["tree"].encode("utf-8"))


@patch("swegrep_cli.core._streaming_request")
def test_search_loop_success(
    mock_streaming: MagicMock,
) -> None:

    # Turn 1: request returns restricted_exec call
    t1_encoder = ProtobufEncoder()
    # Write some headers and strings
    t1_encoder.write_string(1, "thinking about doing search")
    t1_encoder.write_string(
        2,
        '[TOOL_CALLS]restricted_exec[ARGS]{"command1": {"type": "readfile", "file": "/codebase/test.txt"}}',
    )
    t1_frame = connect_frame_encode(t1_encoder.to_bytes(), compress=False)

    # Turn 2: answer returned
    t2_encoder = ProtobufEncoder()
    t2_encoder.write_string(1, "found answer")
    t2_encoder.write_string(
        2,
        '[TOOL_CALLS]answer[ARGS]{"answer": "<ANSWER><file path=\\"/codebase/test.txt\\"><range>1-10</range></file></ANSWER>"}',
    )
    t2_frame = connect_frame_encode(t2_encoder.to_bytes(), compress=False)

    mock_streaming.side_effect = [t1_frame, t2_frame]

    with tempfile.TemporaryDirectory() as tmpdir:
        # Create test file to let readfile succeed
        Path(tmpdir, "test.txt").write_text("line1\nline2", encoding="utf-8")

        res = asyncio.run(
            search(
                query="find main",
                project_root=tmpdir,
                api_key="sk-ws-01-key",
                max_turns=2,
            )
        )
        assert "files" in res
        assert len(res["files"]) == 1
        assert res["files"][0]["path"] == "test.txt"
        assert res["files"][0]["ranges"] == [[1, 10]]
        assert mock_streaming.call_count == 2


@patch("swegrep_cli.core.get_api_key", return_value="fake-api-key")
def test_check_auth_success(mock_get_key: MagicMock) -> None:
    res = check_auth()
    assert res["ok"] is True
    assert res["jwt_source"] == "api-key"


