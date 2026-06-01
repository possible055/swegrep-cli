import asyncio
import base64
import gzip
import json
import os
import re
import ssl
import sys
import time
import urllib.error
import urllib.request
from collections.abc import Callable
from pathlib import Path
from typing import Any, cast
from uuid import uuid4

from swegrep_cli import credentials
from swegrep_cli.executor import ToolExecutor
from swegrep_cli.protobuf import (
    ProtobufEncoder,
    connect_frame_decode,
    connect_frame_encode,
    extract_strings,
)


# --- Credentials discovery ---
def get_config_path() -> Path:
    return credentials.get_config_path()


def _load_cached_api_key() -> str | None:
    return credentials.load_cached_api_key(get_config_path())


def _save_cached_api_key(key: str) -> None:
    credentials.save_cached_api_key(key, get_config_path())


def _auto_discover_api_key() -> str | None:
    return credentials.discover_api_key()


def get_api_key(api_key: str | None = None, *, save_discovered: bool = True) -> str:
    if api_key:
        if not credentials.looks_truncated_api_key(api_key):
            return api_key
        discovered = _auto_discover_api_key()
        if discovered:
            print(
                "[swegrep-cli] Passed API key looks truncated; using key discovered from Windsurf",
                file=sys.stderr,
            )
            return discovered
        return api_key
    key = os.environ.get(credentials.CONFIG_KEY)
    if key:
        if credentials.looks_truncated_api_key(key):
            discovered = _auto_discover_api_key()
            if discovered:
                print(
                    "[swegrep-cli] WINDSURF_API_KEY looks truncated; using key discovered from Windsurf",
                    file=sys.stderr,
                )
                return discovered
        return key
    cached = _load_cached_api_key()
    if cached:
        print("[swegrep-cli] Using cached API key from config", file=sys.stderr)
        return cached
    discovered = _auto_discover_api_key()
    if discovered:
        if save_discovered:
            _save_cached_api_key(discovered)
        return discovered
    raise RuntimeError(
        "Windsurf API Key not found. Set WINDSURF_API_KEY env var, ensure Windsurf is logged in, "
        f"or write it to config file: {get_config_path()}"
    )


# --- Errors ---
class FastContextError(Exception):
    def __init__(self, message: str, code: str, details: dict[str, Any] | None = None) -> None:
        super().__init__(message)
        self.code = code
        self.details = details or {}


def _classify_error(err: Exception) -> FastContextError:
    if isinstance(err, FastContextError):
        return err

    # Check for HTTP errors
    if isinstance(err, urllib.error.HTTPError):
        s = err.code
        try:
            body = err.read().decode("utf-8", errors="ignore")
        except Exception:
            body = ""
        details = {"status": s, "body": body}
        if s == 413:
            return FastContextError(str(err), "PAYLOAD_TOO_LARGE", details)
        if s == 429:
            return FastContextError(str(err), "RATE_LIMITED", details)
        if s in (401, 403):
            return FastContextError(str(err), "AUTH_ERROR", details)
        return FastContextError(str(err), "SERVER_ERROR", details)

    # Check for Timeout
    if isinstance(err, TimeoutError) or "timeout" in str(err).lower():
        return FastContextError(str(err), "TIMEOUT")

    # SSL / Network
    return FastContextError(str(err), "NETWORK_ERROR")


# --- Constants ---
API_BASE = "https://server.self-serve.windsurf.com/exa.api_server_pb.ApiServerService"
AUTH_BASE = "https://server.self-serve.windsurf.com/exa.auth_pb.AuthService"
WS_APP = "windsurf"
DEFAULT_WS_APP_VER = "1.48.2"
DEFAULT_WS_LS_VER = "1.9544.35"
MAX_TREE_BYTES = 250 * 1024


def _protocol_setting(value: str | None, env_name: str, default: str) -> str:
    return value or os.environ.get(env_name, default)


def _ws_app_version(value: str | None = None) -> str:
    return _protocol_setting(value, "WS_APP_VER", DEFAULT_WS_APP_VER)


def _ws_ls_version(value: str | None = None) -> str:
    return _protocol_setting(value, "WS_LS_VER", DEFAULT_WS_LS_VER)


SYSTEM_PROMPT_TEMPLATE = """You are an expert software engineer, responsible for providing context \
to another engineer to solve a code issue in the current codebase. \
The user will present you with a description of the issue, and it is \
your job to provide a series of file paths with associated line ranges \
that contain ALL the information relevant to understand and correctly \
address the issue.

# IMPORTANT:
- A relevant file does not mean only the files that must be modified to \
solve the task. It means any file that contains information relevant to \
planning and implementing the fix, such as the definitions of classes \
and functions that are relevant to the pieces of code that will have to \
be modified.
- You should include enough context around the relevant lines to allow \
the engineer to understand the task correctly. You must include ENTIRE \
semantic blocks (functions, classes, definitions, etc). For example:
If addressing the issue requires modifying a method within a class, then \
you should include the entire class definition, not just the lines around \
the method we want to modify.
- NEVER truncate these blocks unless they are very large (hundreds of \
lines or more, in which case providing only a relevant portion of the \
block is acceptable).
- Your job is to essentially alleviate the job of the other engineer by \
giving them a clean starting context from which to start working. More \
precisely, you should minimize the number of files the engineer has to \
read to understand and solve the task correctly (while not providing \
irrelevant code snippets).

# ENVIRONMENT
- Working directory: /codebase. Make sure to run commands in this \
directory, not `.
- Tool access: use the restricted_exec tool ONLY
- Allowed sub-commands (schema-enforced):
  - rg: Search for patterns in files using ripgrep
    - Required: pattern (string), path (string)
    - Optional: include (array of globs), exclude (array of globs)
  - readfile: Read contents of a file with optional line range
    - Required: file (string)
    - Optional: start_line (int), end_line (int) — 1-indexed, inclusive
  - tree: Display directory structure as a tree
    - Required: path (string)
    - Optional: levels (int)

# THINKING RULES
- Think step-by-step. Plan, reason, and reflect before each tool call.
- Use tool calls liberally and purposefully to ground every conclusion \
in real code, not assumptions.
- If a command fails, rethink and try something different; do not \
complain to the user.

# FAST-SEARCH DEFAULTS (optimize rg/tree on large repos)
- Start NARROW, then widen only if needed. Prefer searching likely code \
roots first (e.g., `src/`, `lib/`, `app/`, `packages/`, `services/`) \
instead of `/codebase`.
- Prefer fixed-string search for literals: escape patterns or keep regex \
simple. Use smart case; avoid case-insensitive unless necessary.
- Prefer file-type filters and globs (in include) over full-repo scans.
- Default EXCLUDES for speed (apply via the exclude array): \
node_modules, .git, dist, build, coverage, .venv, venv, target, out, \
.cache, __pycache__, vendor, deps, third_party, logs, data, *.min.*
- Skip huge files where possible; when opening files, prefer reading \
only relevant ranges with readfile.
- Limit directory traversal with tree levels to quickly orient before \
deeper inspection.

# SOME EXAMPLES OF WORKFLOWS
- MAP – Use `tree` with small levels; `rg` on likely roots to grasp \
structure and hotspots.
- ANCHOR – `rg` for problem keywords and anchor symbols; restrict by \
language globs via include.
- TRACE – Follow imports with targeted `rg` in narrowed roots; open \
files with `readfile` scoped to entire semantic blocks.
- VERIFY – Confirm each candidate path exists by reading or additional \
searches; drop false positives (tests, vendored, generated) unless they \
must change.

# TOOL USE GUIDELINES
- You must use a SINGLE restricted_exec call in your answer, that lets \
you execute at most {max_commands} commands in a single turn. Each command must be \
an object with a `type` field of `rg`, `readfile`, or `tree` and the appropriate fields for that type.
- Example restricted_exec usage:
[TOOL_CALLS]restricted_exec[ARGS]{{
  "command1": {{
    "type": "rg",
    "pattern": "Controller",
    "path": "/codebase/slime",
    "include": ["**/*.py"],
    "exclude": ["**/node_modules/**", "**/.git/**", "**/dist/**", \
"**/build/**", "**/.venv/**", "**/__pycache__/**"]
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
- You have at most {max_turns} turns to interact with the environment by calling \
tools, so issuing multiple commands at once is necessary and encouraged \
to speed up your research.
- Each command result may be truncated to 50 lines; prefer multiple \
targeted reads/searches to build complete context.
- DO NOT EVER USE MORE THAN {max_commands} commands in a single turn, or you will \
be penalized.

# ANSWER FORMAT (strict format, including tags)
- You will output an XML structure with a root element "ANSWER" \
containing "file" elements. Each "file" element will have a "path" \
attribute and contain "range" elements.
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


Remember: Prefer narrow, fixed-string, and type-filtered searches with \
aggressive excludes and size/depth limits. Widen scope only as needed. \
Use the restricted tools available to you, and output your answer in \
exactly the specified format.

# NO RESULTS POLICY
If after thorough searching you are confident that NO relevant files exist \
for the given query (e.g., the function/class/concept does not exist in the \
codebase), you MUST return an empty ANSWER:
<ANSWER></ANSWER>
Do NOT return irrelevant files (such as entry points or config files) just \
to provide some output. An empty answer is always better than a misleading one.

# RESULT COUNT
Aim to return at most {max_results} files in your answer. Focus on the most \
relevant files first. If fewer files are relevant, return fewer.
"""

FINAL_FORCE_ANSWER = (
    "You have no turns left. Now you MUST provide your final ANSWER, even if it's not complete."
)


# --- Network layer ---
def _unary_request(url: str, body: bytes, compress: bool = True, timeout: float = 30.0) -> bytes:
    headers = {
        "Content-Type": "application/proto",
        "Connect-Protocol-Version": "1",
        "User-Agent": "connect-go/1.18.1 (go1.25.5)",
        "Accept-Encoding": "gzip",
    }
    if compress:
        payload = gzip.compress(body)
        headers["Content-Encoding"] = "gzip"
    else:
        payload = body

    req = urllib.request.Request(url, data=payload, headers=headers, method="POST")
    ctx = ssl.create_default_context()

    try:
        with urllib.request.urlopen(req, timeout=timeout, context=ctx) as resp:
            return _decode_unary_response(cast(bytes, resp.read()), resp.headers.get("Content-Encoding"))
    except urllib.error.URLError as e:
        # TLS fallback
        if "certificate verify failed" in str(e.reason).lower():
            unverified_ctx = ssl._create_unverified_context()
            try:
                with urllib.request.urlopen(req, timeout=timeout, context=unverified_ctx) as resp:
                    return _decode_unary_response(
                        cast(bytes, resp.read()), resp.headers.get("Content-Encoding")
                    )
            except Exception as e_inner:
                raise _classify_error(e_inner) from e_inner
        raise _classify_error(e) from e
    except Exception as e:
        raise _classify_error(e) from e


def _decode_unary_response(data: bytes, content_encoding: str | None) -> bytes:
    if content_encoding and "gzip" in content_encoding.lower():
        return gzip.decompress(data)
    if data.startswith(b"\x1f\x8b"):
        return gzip.decompress(data)
    return data


def _streaming_request(
    body: bytes,
    timeout_ms: int = 30000,
    max_retries: int = 2,
    ls_version: str | None = None,
) -> bytes:
    frame = connect_frame_encode(body)
    url = f"{API_BASE}/GetDevstralStream"
    trace_id = uuid4().hex
    span_id = uuid4().hex[:16]
    base_timeout = timeout_ms / 1000.0

    headers = {
        "Content-Type": "application/connect+proto",
        "Connect-Protocol-Version": "1",
        "Connect-Accept-Encoding": "gzip",
        "Connect-Content-Encoding": "gzip",
        "Connect-Timeout-Ms": str(timeout_ms),
        "User-Agent": "connect-go/1.18.1 (go1.25.5)",
        "Accept-Encoding": "identity",
        "Baggage": (
            f"sentry-release=language-server-windsurf@{_ws_ls_version(ls_version)},"
            "sentry-environment=stable,sentry-sampled=false,"
            f"sentry-trace_id={trace_id},"
            "sentry-public_key=b813f73488da69eedec534dba1029111"
        ),
        "Sentry-Trace": f"{trace_id}-{span_id}-0",
    }

    last_err: Exception | None = None
    for attempt in range(max_retries + 1):
        req = urllib.request.Request(url, data=frame, headers=headers, method="POST")
        ctx = ssl.create_default_context()
        try:
            with urllib.request.urlopen(req, timeout=base_timeout + 5.0, context=ctx) as resp:
                return cast(bytes, resp.read())
        except urllib.error.HTTPError as e:
            # Don't retry on 4xx client errors except 429
            if 400 <= e.code < 500 and e.code != 429:
                raise _classify_error(e) from e
            last_err = e
        except urllib.error.URLError as e:
            if "certificate verify failed" in str(e.reason).lower():
                unverified_ctx = ssl._create_unverified_context()
                try:
                    with urllib.request.urlopen(
                        req, timeout=base_timeout + 5.0, context=unverified_ctx
                    ) as resp:
                        return cast(bytes, resp.read())
                except urllib.error.HTTPError as e_inner:
                    if 400 <= e_inner.code < 500 and e_inner.code != 429:
                        raise _classify_error(e_inner) from e_inner
                    last_err = e_inner
                except Exception as e_inner:
                    last_err = e_inner
            else:
                last_err = e
        except Exception as e:
            last_err = e

        if attempt < max_retries:
            time.sleep(1.0 * (attempt + 1))

    raise _classify_error(last_err or RuntimeError("Streaming request failed")) from (
        last_err or None
    )


def check_auth(
    api_key: str | None = None,
    app_version: str | None = None,
    ls_version: str | None = None,
) -> dict[str, Any]:
    try:
        resolved_api_key = get_api_key(api_key, save_discovered=False)
        return {
            "ok": True,
            "jwt_source": "api-key",
            "app_version": _ws_app_version(app_version),
            "ls_version": _ws_ls_version(ls_version),
        }
    except FastContextError as e:
        return {
            "ok": False,
            "error_code": e.code,
            "error": str(e),
            "jwt_source": "api-key",
            "app_version": _ws_app_version(app_version),
            "ls_version": _ws_ls_version(ls_version),
        }
    except Exception as e:
        return {
            "ok": False,
            "error_code": "API_KEY_ERROR",
            "error": str(e),
            "jwt_source": "api-key",
            "app_version": _ws_app_version(app_version),
            "ls_version": _ws_ls_version(ls_version),
        }


# --- JWT Cache ---
_jwt_cache: dict[str, tuple[str, float]] = {}


def _get_jwt_exp(jwt: str) -> float:
    try:
        parts = jwt.split(".")
        if len(parts) < 2:
            return 0.0
        payload_b64 = parts[1]
        payload_b64 += "=" * (4 - len(payload_b64) % 4)
        payload = json.loads(base64.urlsafe_b64decode(payload_b64).decode("utf-8"))
        return float(payload.get("exp", 0.0))
    except Exception:
        return 0.0


def fetch_jwt(api_key: str, timeout_ms: int = 30000) -> str:
    meta = ProtobufEncoder()
    meta.write_string(1, WS_APP)
    meta.write_string(2, _ws_app_version())
    meta.write_string(3, api_key)
    meta.write_string(4, "zh-cn")
    meta.write_string(7, _ws_ls_version())
    meta.write_string(12, WS_APP)
    meta.write_bytes(30, b"\x00\x01")

    outer = ProtobufEncoder()
    outer.write_message(1, meta)

    resp = _unary_request(
        f"{AUTH_BASE}/GetUserJwt",
        outer.to_bytes(),
        compress=False,
        timeout=timeout_ms / 1000.0,
    )
    for s in extract_strings(resp):
        if s.startswith("eyJ") and "." in s:
            return s
    raise RuntimeError("Failed to extract JWT from GetUserJwt response")


async def get_cached_jwt(api_key: str, timeout_ms: int = 30000) -> str:
    now = time.time()
    cached = _jwt_cache.get(api_key)
    if cached:
        token, expires_at = cached
        if expires_at > now + 60:
            return token

    token = await asyncio.to_thread(fetch_jwt, api_key, timeout_ms)
    exp = _get_jwt_exp(token)
    expires_at = exp if exp > 0 else now + 3600
    _jwt_cache[api_key] = (token, expires_at)
    return token





# --- Protobuf request builders ---
def _build_metadata(
    api_key: str,
    jwt: str,
    app_version: str | None = None,
    ls_version: str | None = None,
) -> ProtobufEncoder:
    meta = ProtobufEncoder()
    meta.write_string(1, WS_APP)
    meta.write_string(2, _ws_app_version(app_version))
    meta.write_string(3, api_key)
    meta.write_string(4, "zh-cn")

    sys_info = {
        "Os": sys.platform,
        "Arch": os.uname().machine if hasattr(os, "uname") else "x86_64",
        "Release": os.uname().release if hasattr(os, "uname") else "1.0",
        "Version": os.uname().version if hasattr(os, "uname") else "1.0",
        "Machine": os.uname().machine if hasattr(os, "uname") else "x86_64",
        "Nodename": os.uname().nodename if hasattr(os, "uname") else "localhost",
        "Sysname": "Darwin"
        if sys.platform == "darwin"
        else ("Windows_NT" if sys.platform == "win32" else "Linux"),
        "ProductVersion": "",
    }
    meta.write_string(5, json.dumps(sys_info))
    meta.write_string(7, _ws_ls_version(ls_version))

    cpu_info = {
        "NumSockets": 1,
        "NumCores": os.cpu_count() or 4,
        "NumThreads": os.cpu_count() or 4,
        "VendorID": "",
        "Family": "0",
        "Model": "0",
        "ModelName": "Unknown CPU",
        "Memory": 16 * 1024 * 1024 * 1024,
    }
    meta.write_string(8, json.dumps(cpu_info))
    meta.write_string(12, WS_APP)
    meta.write_string(21, jwt)
    meta.write_bytes(30, b"\x00\x01")
    return meta


def _build_chat_message(
    role: int, content: str, opts: dict[str, Any] | None = None
) -> ProtobufEncoder:
    msg = ProtobufEncoder()
    msg.write_varint(2, role)
    msg.write_string(3, content)

    if opts:
        tool_call_id = opts.get("toolCallId")
        tool_name = opts.get("toolName")
        tool_args_json = opts.get("toolArgsJson")
        if tool_call_id and tool_name and tool_args_json:
            tc = ProtobufEncoder()
            tc.write_string(1, tool_call_id)
            tc.write_string(2, tool_name)
            tc.write_string(3, tool_args_json)
            msg.write_message(6, tc)

        ref_call_id = opts.get("refCallId")
        if ref_call_id:
            msg.write_string(7, ref_call_id)

    return msg


def _build_request(
    api_key: str,
    jwt: str,
    messages: list[dict[str, Any]],
    tool_defs: str,
    app_version: str | None = None,
    ls_version: str | None = None,
) -> bytes:
    req = ProtobufEncoder()
    req.write_message(1, _build_metadata(api_key, jwt, app_version, ls_version))

    for m in messages:
        opts = {
            "toolCallId": m.get("tool_call_id"),
            "toolName": m.get("tool_name"),
            "toolArgsJson": m.get("tool_args_json"),
            "refCallId": m.get("ref_call_id"),
        }
        msg_enc = _build_chat_message(m["role"], m["content"], opts)
        req.write_message(2, msg_enc)

    req.write_string(3, tool_defs)
    return req.to_bytes()


# --- Response parsing ---
def _parse_tool_call(text: str) -> tuple[str, str, dict[str, Any]] | None:
    text = text.replace("</s>", "")
    m = re.search(r"\[TOOL_CALLS\](\w+)\[ARGS\](\{.+)", text, re.DOTALL)
    if not m:
        return None

    name = m.group(1)
    raw = m.group(2).strip()

    # Match brace depth
    depth = 0
    end = 0
    for idx, char in enumerate(raw):
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                end = idx + 1
                break
    if end == 0:
        end = len(raw)

    try:
        args = json.loads(raw[:end])
    except Exception:
        return None

    thinking = text[: m.start()].strip()
    return thinking, name, args


def _parse_response(data: bytes) -> tuple[str, tuple[str, dict[str, Any]] | None]:
    frames = connect_frame_decode(data)
    all_text = ""

    for frame_data in frames:
        try:
            text_candidate = frame_data.decode("utf-8", errors="ignore")
            if text_candidate.startswith("{"):
                err_obj = json.loads(text_candidate)
                if "error" in err_obj:
                    code = err_obj["error"].get("code", "unknown")
                    msg = err_obj["error"].get("message", "")
                    return f"[Error] {code}: {msg}", None
        except Exception:
            pass

        # Strip invalid characters
        raw_text = frame_data.decode("utf-8", errors="ignore").replace("\ufffd", "")
        if "[TOOL_CALLS]" in raw_text:
            all_text = raw_text
            break

        for s in extract_strings(frame_data):
            if len(s) > 10:
                all_text += s

    parsed = _parse_tool_call(all_text)
    if parsed:
        thinking, name, args = parsed
        return thinking, (name, args)
    return all_text, None


# --- Adaptive Tree / RepoMap ---
def get_repo_map(
    project_root: str, target_depth: int = 3, exclude_paths: list[str] | None = None
) -> dict[str, Any]:
    executor = ToolExecutor(project_root)
    # Target depth loop
    for L in range(target_depth, 0, -1):
        try:
            tree_str = executor.tree(
                "/codebase", levels=L, exclude_paths=exclude_paths, truncate=False
            )
            size = len(tree_str.encode("utf-8"))
            if size <= MAX_TREE_BYTES:
                return {
                    "tree": tree_str,
                    "depth": L,
                    "size_bytes": size,
                    "fell_back": L < target_depth,
                }
        except Exception:
            pass

    # Simple walk fallback
    try:
        entries = sorted(os.listdir(project_root))
        if exclude_paths:
            import fnmatch

            entries = [e for e in entries if not any(fnmatch.fnmatch(e, p) for p in exclude_paths)]
        tree_str = "/codebase\n" + "\n".join(f"├── {e}" for e in entries)
        return {
            "tree": tree_str,
            "depth": 0,
            "size_bytes": len(tree_str.encode("utf-8")),
            "fell_back": True,
        }
    except Exception:
        tree_str = "/codebase\n(empty or inaccessible)"
        return {"tree": tree_str, "depth": 0, "size_bytes": len(tree_str), "fell_back": True}


# --- Tool Schemas ---
def _build_command_schema(n: int) -> dict[str, Any]:
    return {
        "type": "object",
        "description": f"Command {n} to execute. Must be one of: rg, readfile, or tree.",
        "oneOf": [
            {
                "properties": {
                    "type": {
                        "type": "string",
                        "const": "rg",
                        "description": "Search for patterns in files using ripgrep.",
                    },
                    "pattern": {
                        "type": "string",
                        "description": "The regex pattern to search for.",
                    },
                    "path": {"type": "string", "description": "The path to search in."},
                    "include": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "File patterns to include.",
                    },
                    "exclude": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "File patterns to exclude.",
                    },
                },
                "required": ["type", "pattern", "path"],
            },
            {
                "properties": {
                    "type": {
                        "type": "string",
                        "const": "readfile",
                        "description": "Read contents of a file with optional line range.",
                    },
                    "file": {"type": "string", "description": "Path to the file to read."},
                    "start_line": {
                        "type": "integer",
                        "description": "Starting line number (1-indexed).",
                    },
                    "end_line": {
                        "type": "integer",
                        "description": "Ending line number (1-indexed).",
                    },
                },
                "required": ["type", "file"],
            },
            {
                "properties": {
                    "type": {
                        "type": "string",
                        "const": "tree",
                        "description": "Display directory structure as a tree.",
                    },
                    "path": {"type": "string", "description": "Path to the directory."},
                    "levels": {"type": "integer", "description": "Number of directory levels."},
                },
                "required": ["type", "path"],
            },
            {
                "properties": {
                    "type": {
                        "type": "string",
                        "const": "ls",
                        "description": "List files in a directory.",
                    },
                    "path": {"type": "string", "description": "Path to the directory."},
                    "long_format": {"type": "boolean"},
                    "all": {"type": "boolean"},
                },
                "required": ["type", "path"],
            },
            {
                "properties": {
                    "type": {
                        "type": "string",
                        "const": "glob",
                        "description": "Find files matching a glob pattern.",
                    },
                    "pattern": {"type": "string"},
                    "path": {"type": "string"},
                    "type_filter": {"type": "string", "enum": ["file", "directory", "all"]},
                },
                "required": ["type", "pattern", "path"],
            },
        ],
    }


def get_tool_definitions(max_commands: int = 8) -> str:
    props = {}
    for i in range(1, max_commands + 1):
        props[f"command{i}"] = _build_command_schema(i)

    tools = [
        {
            "type": "function",
            "function": {
                "name": "restricted_exec",
                "description": "Execute restricted commands (rg, readfile, tree, ls, glob) in parallel.",
                "parameters": {"type": "object", "properties": props, "required": ["command1"]},
            },
        },
        {
            "type": "function",
            "function": {
                "name": "answer",
                "description": "Final answer with relevant files and line ranges.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "answer": {
                            "type": "string",
                            "description": "The final answer in XML format.",
                        }
                    },
                    "required": ["answer"],
                },
            },
        },
    ]
    return json.dumps(tools)


def _limit_tool_args(tool_args: dict[str, Any], max_commands: int) -> dict[str, Any]:
    command_keys = sorted(
        (key for key in tool_args if key.startswith("command")),
        key=lambda key: int(key.removeprefix("command"))
        if key.removeprefix("command").isdigit()
        else 9999,
    )
    allowed = set(command_keys[:max_commands])
    return {
        key: value
        for key, value in tool_args.items()
        if not key.startswith("command") or key in allowed
    }


def build_system_prompt(max_turns: int = 3, max_commands: int = 8, max_results: int = 10) -> str:
    return (
        SYSTEM_PROMPT_TEMPLATE.replace("{max_turns}", str(max_turns))
        .replace("{max_commands}", str(max_commands))
        .replace("{max_results}", str(max_results))
    )


def _trim_messages(messages: list[dict[str, Any]]) -> bool:
    if len(messages) <= 4:
        return False
    head = messages[:2]
    tail = messages[-2:]
    messages.clear()
    messages.extend(head)
    messages.append(
        {
            "role": 1,
            "content": "[Prior search rounds omitted to reduce payload. Provide your best answer based on available context.]",
        }
    )
    messages.extend(tail)
    return True


def _parse_answer(xml_text: str, project_root: str) -> dict[str, Any]:
    files = []
    resolved_root = Path(project_root).resolve()

    file_regex = re.compile(r'<file\s+path=(["\'])([^"\']+)\1>([\s\S]*?)</file>')
    range_regex = re.compile(r"<range>(\d+)-(\d+)</range>")

    for fm in file_regex.finditer(xml_text):
        vpath = fm.group(2)
        rel = vpath.replace("/codebase", "").lstrip("/\\")

        # Path safety check
        try:
            full_path = (resolved_root / rel).resolve()
            full_path.relative_to(resolved_root)
        except (ValueError, RuntimeError):
            continue

        ranges = []
        for rm in range_regex.finditer(fm.group(3)):
            ranges.append([int(rm.group(1)), int(rm.group(2))])

        files.append({"path": rel, "full_path": str(full_path), "ranges": ranges})

    return {"files": files}


# --- Core Search Main Execution ---
async def search(
    query: str,
    project_root: str,
    api_key: str | None = None,
    app_version: str | None = None,
    ls_version: str | None = None,
    max_turns: int = 3,
    max_commands: int = 8,
    max_results: int = 10,
    tree_depth: int = 4,
    timeout_ms: int = 30000,
    exclude_paths: list[str] | None = None,
    result_max_lines: int | None = None,
    line_max_chars: int | None = None,
    on_progress: Callable[[str], None] | None = None,
) -> dict[str, Any]:
    def log(msg: str) -> None:
        if on_progress:
            on_progress(msg)

    project_root = str(Path(project_root).resolve())

    # Credentials
    api_key = get_api_key(api_key)
    jwt = await get_cached_jwt(api_key, timeout_ms)

    # Overwrite timeout_ms if TIMEOUT env var is set
    env_timeout = os.environ.get("TIMEOUT")
    if env_timeout:
        try:
            timeout_ms = int(env_timeout)
        except ValueError:
            pass

    executor = ToolExecutor(
        project_root, result_max_lines=result_max_lines, line_max_chars=line_max_chars
    )
    tool_defs = get_tool_definitions(max_commands)
    system_prompt = build_system_prompt(max_turns, max_commands, max_results)

    # Adaptive repo map
    map_res = get_repo_map(project_root, tree_depth, exclude_paths)
    actual_depth = map_res["depth"]
    tree_size_kb = map_res["size_bytes"] / 1024.0
    fell_back = map_res["fell_back"]
    repo_map = map_res["tree"]

    log(
        f"Repo map: tree -L {actual_depth} ({tree_size_kb:.1f}KB){' [fell back]' if fell_back else ''}"
    )
    user_content = f"Problem Statement: {query}\n\nRepo Map (tree -L {actual_depth} /codebase):\n```text\n{repo_map}\n```"

    messages = [
        {"role": 5, "content": system_prompt},
        {"role": 1, "content": user_content},
    ]

    total_api_calls = max_turns + 1
    compensated_turns = 0
    max_compensations = 2
    force_answer_injected = False

    for turn in range(total_api_calls + max_compensations):
        # Prevent loop if compensations boundary is reached
        if turn >= total_api_calls + compensated_turns:
            break

        log(f"Turn {turn + 1}/{total_api_calls + compensated_turns}")

        proto = _build_request(api_key, jwt, messages, tool_defs, app_version, ls_version)

        try:
            resp_data = await asyncio.to_thread(
                _streaming_request, proto, timeout_ms, 2, ls_version
            )
        except FastContextError as e:
            base_meta = {
                "treeDepth": actual_depth,
                "treeSizeKB": round(tree_size_kb, 1),
                "fellBack": fell_back,
                "projectRoot": project_root,
                "errorCode": e.code,
            }
            # Auto-retry with trimmed context on payload/timeout
            if e.code in ("PAYLOAD_TOO_LARGE", "TIMEOUT") and len(messages) > 4:
                log(f"{e.code} on turn {turn + 1}: trimming context and retrying...")
                _trim_messages(messages)
                retry_proto = _build_request(
                    api_key, jwt, messages, tool_defs, app_version, ls_version
                )
                try:
                    resp_data = await asyncio.to_thread(
                        _streaming_request, retry_proto, timeout_ms, 2, ls_version
                    )
                except FastContextError as retry_err:
                    return {
                        "files": [],
                        "error": f"{retry_err.code}: {retry_err} (retry failure)",
                        "_meta": {**base_meta, "errorCode": retry_err.code, "contextTrimmed": True},
                    }
            else:
                return {"files": [], "error": f"{e.code}: {e}", "_meta": base_meta}

        thinking, tool_info = _parse_response(resp_data)

        if tool_info is None:
            if thinking.startswith("[Error]"):
                return {"files": [], "error": thinking}
            return {"files": [], "raw_response": thinking}

        tool_name, tool_args = tool_info

        if tool_name == "answer":
            answer_xml = tool_args.get("answer", "")
            log("Received final answer")
            result = _parse_answer(answer_xml, project_root)
            result["rg_patterns"] = list(set(executor.collected_rg_patterns))
            result["_meta"] = {
                "treeDepth": actual_depth,
                "treeSizeKB": round(tree_size_kb, 1),
                "fellBack": fell_back,
            }
            return result

        if tool_name == "restricted_exec":
            call_id = str(uuid4())
            tool_args = _limit_tool_args(tool_args, max_commands)
            args_json = json.dumps(tool_args)

            cmds = [k for k in tool_args.keys() if k.startswith("command")]
            log(f"Executing {len(cmds)} local commands")

            # Execute tools (can block thread since they are IO heavy)
            results = executor.exec_tool_call(tool_args)

            # Turn compensation check
            valid_cmds = [k for k in cmds if tool_args[k] and tool_args[k].get("type")]
            if not valid_cmds and compensated_turns < max_compensations:
                compensated_turns += 1
                log(
                    f"Turn compensation: no valid commands, extending search ({compensated_turns}/{max_compensations})"
                )
            elif not valid_cmds:
                log("Turn compensation skipped: limit reached")

            messages.append(
                {
                    "role": 2,
                    "content": thinking,
                    "tool_call_id": call_id,
                    "tool_name": "restricted_exec",
                    "tool_args_json": args_json,
                }
            )
            messages.append({"role": 4, "content": results, "ref_call_id": call_id})

            # Force-answer injection
            effective_turn = turn - compensated_turns
            if effective_turn >= max_turns - 1 and not force_answer_injected:
                messages.append({"role": 1, "content": FINAL_FORCE_ANSWER})
                force_answer_injected = True
                log("Injected force-answer prompt")

    return {
        "files": [],
        "error": "Max turns reached without getting an answer",
        "rg_patterns": list(set(executor.collected_rg_patterns)),
        "_meta": {
            "treeDepth": actual_depth,
            "treeSizeKB": round(tree_size_kb, 1),
            "fellBack": fell_back,
            "projectRoot": project_root,
        },
    }


async def search_with_content(
    query: str,
    project_root: str,
    api_key: str | None = None,
    app_version: str | None = None,
    ls_version: str | None = None,
    max_turns: int = 3,
    max_commands: int = 8,
    max_results: int = 10,
    tree_depth: int = 4,
    timeout_ms: int = 30000,
    exclude_paths: list[str] | None = None,
    result_max_lines: int | None = None,
    line_max_chars: int | None = None,
) -> str:
    result = await search(
        query=query,
        project_root=project_root,
        api_key=api_key,
        app_version=app_version,
        ls_version=ls_version,
        max_turns=max_turns,
        max_commands=max_commands,
        max_results=max_results,
        tree_depth=tree_depth,
        timeout_ms=timeout_ms,
        exclude_paths=exclude_paths,
        result_max_lines=result_max_lines,
        line_max_chars=line_max_chars,
    )

    if "error" in result:
        meta = result.get("_meta", {})
        err_msg = f"Error: {result['error']}"
        if meta:
            err_msg += f"\n\n[diagnostic] error_type={meta.get('errorCode', 'unknown')}, tree_depth_used={meta.get('treeDepth')}, tree_size={meta.get('treeSizeKB')}KB"
            if meta.get("fellBack"):
                err_msg += " (auto fell back)"
            if meta.get("contextTrimmed"):
                err_msg += ", context_trimmed=true"
            if meta.get("projectRoot"):
                err_msg += f"\n[diagnostic] project_path={meta.get('projectRoot')}"
            err_msg += f"\n[config] max_turns={max_turns}, max_results={max_results}, max_commands={max_commands}"

            # Helpful hints
            ec = meta.get("errorCode")
            if ec in ("PAYLOAD_TOO_LARGE", "TIMEOUT"):
                err_msg += (
                    "\n[hint] Try: reduce tree_depth, add exclude_paths, or narrow project_path."
                )
            elif ec == "AUTH_ERROR":
                err_msg += (
                    "\n[hint] Authentication error. Ensure a fresh WINDSURF_API_KEY is configured."
                )
            elif ec == "RATE_LIMITED":
                err_msg += "\n[hint] Rate limited. Wait a moment and retry."
        return err_msg

    files = result.get("files", [])
    rg_patterns = result.get("rg_patterns", [])
    unique_patterns = [p for p in set(rg_patterns) if len(p) >= 3]

    if not files and not unique_patterns:
        raw = result.get("raw_response", "")
        return (
            f"No relevant files found.\n\nRaw response:\n{raw}"
            if raw
            else "No relevant files found."
        )

    parts = []
    n = len(files)
    if files:
        parts.append(f"Found {n} relevant files.\n")
        for idx, entry in enumerate(files):
            ranges_str = ", ".join(f"L{r[0]}-{r[1]}" for r in entry["ranges"])
            parts.append(f"  [{idx + 1}/{n}] {entry['full_path']} ({ranges_str})")
    else:
        parts.append("No files found.")

    if unique_patterns:
        parts.append("")
        parts.append(f"grep keywords: {', '.join(unique_patterns)}")

    meta = result.get("_meta")
    if meta:
        parts.append("")
        fb_note = " (fell back)" if meta.get("fellBack") else ""
        parts.append(
            f"[config] tree_depth={meta.get('treeDepth')}{fb_note}, tree_size={meta.get('treeSizeKB')}KB, max_turns={max_turns}, max_results={max_results}"
        )

    return "\n".join(parts)
