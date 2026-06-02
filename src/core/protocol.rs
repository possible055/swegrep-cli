use crate::executor::command_number;
use crate::protobuf::{ProtobufEncoder, connect_frame_decode, extract_strings};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::env;

use super::WS_APP;
use super::auth::{ws_app_version, ws_ls_version};

#[derive(Debug, Clone)]
pub(crate) struct ChatMessage {
    pub(super) role: u64,
    pub(super) content: String,
    pub(super) tool_call_id: Option<String>,
    pub(super) tool_name: Option<String>,
    pub(super) tool_args_json: Option<String>,
    pub(super) ref_call_id: Option<String>,
}

impl ChatMessage {
    pub(crate) fn simple(role: u64, content: impl Into<String>) -> Self {
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

pub(crate) fn build_request(
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

pub(crate) fn parse_response(data: &[u8]) -> (String, Option<(String, Value)>) {
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

pub(crate) const FINAL_FORCE_ANSWER: &str =
    "You have no turns left. Now you MUST provide your final ANSWER, even if it's not complete.";

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

pub(crate) fn trim_messages(messages: &mut Vec<ChatMessage>) -> bool {
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
