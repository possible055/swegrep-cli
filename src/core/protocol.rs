use crate::protobuf::{ProtobufEncoder, connect_frame_decode, extract_strings};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::env;

use super::WS_APP;
use super::auth::{ws_app_version, ws_ls_version};

#[derive(Debug, Clone)]
pub(crate) struct ChatMessage {
    pub(super) role: MessageRole,
    pub(super) content: String,
    pub(super) tool_call_id: Option<String>,
    pub(super) tool_name: Option<String>,
    pub(super) tool_args_json: Option<String>,
    pub(super) ref_call_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageRole {
    User,
    Assistant,
    Tool,
    System,
}

impl MessageRole {
    fn wire_value(self) -> u64 {
        match self {
            Self::User => 1,
            Self::Assistant => 2,
            Self::Tool => 4,
            Self::System => 5,
        }
    }
}

impl ChatMessage {
    pub(crate) fn simple(role: MessageRole, content: impl Into<String>) -> Self {
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

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParsedToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) args: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ParsedModelTurn {
    ToolCalls {
        thinking: String,
        calls: Vec<ParsedToolCall>,
    },
    Text(String),
    Error(String),
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

fn build_chat_message(message: &ChatMessage) -> ProtobufEncoder {
    let mut msg = ProtobufEncoder::new();
    msg.write_varint(2, message.role.wire_value());
    msg.write_string(3, &message.content);

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
        let msg = build_chat_message(message);
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

    let args = parse_tool_call_args(&name, &raw[..end])?;
    let thinking = text[..full_match.start()].trim().to_string();
    Some((thinking, name, args))
}

fn parse_tool_call_args(name: &str, raw: &str) -> Option<Value> {
    serde_json::from_str::<Value>(raw)
        .ok()
        .or_else(|| (name == "restricted_exec").then(|| repair_restricted_exec_args(raw))?)
}

fn repair_restricted_exec_args(raw: &str) -> Option<Value> {
    let regex = Regex::new(r#""(command[1-8])"\s*:"#).ok()?;
    let matches = regex
        .captures_iter(raw)
        .filter_map(|captures| {
            let key = captures.get(1)?.as_str().to_string();
            let full_match = captures.get(0)?;
            Some((key, full_match.start(), full_match.end()))
        })
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return None;
    }

    let mut repaired = Map::new();
    for (idx, (key, _, value_start)) in matches.iter().enumerate() {
        let value_end = matches
            .get(idx + 1)
            .map(|(_, key_start, _)| *key_start)
            .unwrap_or(raw.len());
        if let Some(value) = repair_json_object_fragment(&raw[*value_start..value_end]) {
            repaired.insert(key.clone(), value);
        }
    }

    (!repaired.is_empty()).then_some(Value::Object(repaired))
}

fn repair_json_object_fragment(fragment: &str) -> Option<Value> {
    let mut candidate = fragment
        .trim()
        .trim_end_matches(|ch: char| ch.is_whitespace() || ch == ',')
        .to_string();
    if candidate.is_empty() {
        return None;
    }

    for _ in 0..=4 {
        if let Ok(value) = serde_json::from_str::<Value>(&candidate) {
            return value.as_object().is_some().then_some(value);
        }
        if let Some(completed) = complete_json_fragment(&candidate)
            && let Ok(value) = serde_json::from_str::<Value>(&completed)
        {
            return value.as_object().is_some().then_some(value);
        }
        if !remove_last_non_ws_char(&mut candidate, '}') {
            break;
        }
    }

    None
}

fn complete_json_fragment(fragment: &str) -> Option<String> {
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    for ch in fragment.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' if stack.pop() != Some(ch) => return None,
            '}' | ']' => {}
            _ => {}
        }
    }
    if in_string {
        return None;
    }

    let mut completed = fragment.to_string();
    for ch in stack.iter().rev() {
        completed.push(*ch);
    }
    Some(completed)
}

fn remove_last_non_ws_char(value: &mut String, expected: char) -> bool {
    let Some((idx, ch)) = value
        .char_indices()
        .rev()
        .find(|(_, ch)| !ch.is_whitespace())
    else {
        return false;
    };
    if ch != expected {
        return false;
    }
    value.truncate(idx);
    true
}

fn parse_structured_tool_call(value: &Value) -> Option<ParsedModelTurn> {
    let tool_calls = value.get("tool_calls")?.as_array()?;
    let calls = tool_calls
        .iter()
        .enumerate()
        .filter_map(|(idx, call)| {
            let name = call
                .get("name")
                .or_else(|| {
                    call.get("function")
                        .and_then(|function| function.get("name"))
                })?
                .as_str()?
                .to_string();
            let args = call
                .get("args")
                .or_else(|| call.get("arguments"))
                .or_else(|| {
                    call.get("function")
                        .and_then(|function| function.get("arguments"))
                })?;
            let args = if let Some(raw) = args.as_str() {
                serde_json::from_str(raw).ok()?
            } else {
                args.clone()
            };
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("tool-call-{}", idx + 1));
            Some(ParsedToolCall { id, name, args })
        })
        .collect::<Vec<_>>();
    if calls.is_empty() {
        return None;
    }
    let thinking = value
        .get("thinking")
        .or_else(|| value.get("output"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some(ParsedModelTurn::ToolCalls { thinking, calls })
}

pub(crate) fn parse_response(data: &[u8]) -> ParsedModelTurn {
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
            return ParsedModelTurn::Error(format!("[Error] {code}: {msg}"));
        }

        if text_candidate.starts_with('{')
            && let Ok(value) = serde_json::from_str::<Value>(&text_candidate)
            && let Some(turn) = parse_structured_tool_call(&value)
        {
            return turn;
        }

        let raw_text = text_candidate.replace('\u{fffd}', "");
        let extracted_strings = extract_strings(&frame_data);
        if raw_text.contains("[TOOL_CALLS]") {
            all_text = if extracted_strings
                .iter()
                .any(|value| value.contains("[TOOL_CALLS]"))
            {
                extracted_strings.join("")
            } else {
                raw_text
            };
            break;
        }

        for value in extracted_strings {
            if value.len() > 10 {
                all_text.push_str(&value);
            }
        }
    }

    if let Some((thinking, name, args)) = parse_tool_call(&all_text) {
        ParsedModelTurn::ToolCalls {
            thinking,
            calls: vec![ParsedToolCall {
                id: "tool-call-1".to_string(),
                name,
                args,
            }],
        }
    } else {
        ParsedModelTurn::Text(all_text)
    }
}

const SYSTEM_PROMPT_TEMPLATE: &str = r#"Search the codebase based on the user's question and provide a concise, accurate set of file paths and relevant line ranges containing all information needed to understand and correctly solve the issue.

# OBJECTIVE

* Return a minimal but precise set of relevant files that include the implementations, definitions, callers, configuration, tests, and entry points needed to plan or verify changes.
* Prioritize complete semantic blocks, such as functions, classes, implementations, or modules. If a block is too large, trim it as needed while preserving meaning.
* Provide enough code-related context to ensure the relevant mechanisms can be correctly understood.
* Exclude irrelevant code snippets to avoid polluting the context, but do not reduce the result set merely for the sake of reduction.
* Draw conclusions from the actual code. If a command fails or the returned evidence is insufficient, try different tool queries to obtain the answer.

# ENVIRONMENT

* Working directory: /codebase. Make sure to run commands in this directory, not `.`.
* Allowed sub-commands:

  * rg: Search for patterns in files.
  * readfile: Read the contents of a file, optionally within a line range.
  * tree: Display the directory structure as a tree.
  * ls: List files in a directory.
  * glob: Find files matching a glob pattern.

# TOOL USE GUIDELINES

* Use at most {max_commands} `restricted_exec` commands. Each command `type` must be one of `rg`, `readfile`, `tree`, `ls`, or `glob`.
* Each command object must put the command type in the top-level `type` field. Do not wrap commands as `{"rg": {...}}`, `{"readfile": "path"}`, or omit `type`.
* You have at most {max_turns} turns to interact with the environment by calling tools, so parallel commands are encouraged.
* `restricted_exec` arguments must have this shape:

[TOOL_CALLS]restricted_exec[ARGS]{
  "command1": {
    "type": "rg",
    "pattern": "Controller",
    "path": "/codebase/src",
    "include": ["**/*.py"]
  },
  "command2": {
    "type": "readfile",
    "file": "/codebase/src/controller.py",
    "start_line": 1,
    "end_line": 200
  },
  "command3": {
    "type": "tree",
    "path": "/codebase/src",
    "levels": 2
  },
  "command4": {
    "type": "ls",
    "path": "/codebase/src",
    "all": false
  },
  "command5": {
    "type": "glob",
    "pattern": "**/*.rs",
    "path": "/codebase/src",
    "type_filter": "file"
  }
}

# ANSWER FORMAT

Strictly follow this format, including the tags.

* Use the answer tool with XML rooted at `ANSWER`.
* Each `file` element must have a `path` attribute.
* Each file must contain one or more inclusive `range` elements.
* Example output inside the answer tool argument:

<ANSWER><file path="/codebase/src/auth.rs"><range>10-80</range></file></ANSWER>

# NO RESULTS POLICY

If, after thorough searching, you are confident that no relevant files exist for the given query, such as when the requested function, class, or concept does not exist in the codebase, you MUST return an empty `ANSWER`:

<ANSWER></ANSWER>

Do NOT return irrelevant files, such as entry points or configuration files, merely to provide some output. An empty answer is always better than a misleading one.
"#;

pub(crate) const FINAL_FORCE_ANSWER: &str =
    "No search turns remain. You MUST call the answer tool now, even if the answer is incomplete.";

pub fn get_tool_definitions(max_commands: usize) -> String {
    let max_commands = max_commands.clamp(1, 8);
    let command_schema = json!({
        "type": "object",
        "description": "Command to execute. Must be one of: rg, readfile, tree, ls, or glob.",
        "oneOf": [
            {
                "properties": {
                    "type": {"type": "string", "const": "rg", "description": "Search for patterns in files using ripgrep."},
                    "pattern": {"type": "string", "description": "The regex pattern to search for."},
                    "path": {"type": "string", "description": "The path to search in (file or directory)."},
                    "include": {"type": "array", "items": {"type": "string"}, "description": "File patterns to include in the search."},
                    "exclude": {"type": "array", "items": {"type": "string"}, "description": "File patterns to exclude from the search."}
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
                    "path": {"type": "string", "description": "Path to the directory to display."},
                    "levels": {"type": "integer", "description": "Number of directory levels to show."}
                },
                "required": ["type", "path"]
            },
            {
                "properties": {
                    "type": {"type": "string", "const": "ls", "description": "List files in a directory."},
                    "path": {"type": "string", "description": "Path to the directory to list."},
                    "long_format": {"type": "boolean", "description": "Use long format."},
                    "all": {"type": "boolean", "description": "Show all files, including hidden files."}
                },
                "required": ["type", "path"]
            },
            {
                "properties": {
                    "type": {"type": "string", "const": "glob", "description": "Find files matching a glob pattern."},
                    "pattern": {"type": "string", "description": "Glob pattern to match."},
                    "path": {"type": "string", "description": "Path to search in."},
                    "type_filter": {"type": "string", "enum": ["file", "directory", "all"], "description": "Type of files to match."}
                },
                "required": ["type", "pattern", "path"]
            }
        ]
    });
    let mut properties = serde_json::Map::new();
    for idx in 1..=max_commands {
        properties.insert(format!("command{idx}"), command_schema.clone());
    }
    let required = vec!["command1"];

    json!([
        {
            "type": "function",
            "function": {
                "name": "restricted_exec",
                "description": "Execute restricted commands (rg, readfile, tree, ls, glob) in parallel.",
                "parameters": {
                    "type": "object",
                    "properties": properties,
                    "required": required
                }
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

pub fn build_system_prompt(max_turns: usize, max_commands: usize, _max_results: usize) -> String {
    let max_commands = max_commands.clamp(1, 8);
    SYSTEM_PROMPT_TEMPLATE
        .replace("{max_commands}", &max_commands.to_string())
        .replace("{max_turns}", &max_turns.to_string())
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
        MessageRole::User,
        "[Prior search rounds omitted to reduce payload. Provide your best answer based on available context.]",
    ));
    messages.extend(tail);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protobuf::{ProtobufEncoder, connect_frame_encode};

    #[test]
    fn trim_messages_keeps_head_bridge_and_tail() {
        let mut messages = vec![
            ChatMessage::simple(MessageRole::System, "system"),
            ChatMessage::simple(MessageRole::User, "user"),
            ChatMessage::simple(MessageRole::Assistant, "thinking 1"),
            ChatMessage::simple(MessageRole::Tool, "result 1"),
            ChatMessage::simple(MessageRole::Assistant, "thinking 2"),
            ChatMessage::simple(MessageRole::Tool, "result 2"),
        ];

        assert!(trim_messages(&mut messages));
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0].content, "system");
        assert_eq!(messages[1].content, "user");
        assert!(messages[2].content.contains("omitted"));
        assert_eq!(messages[3].content, "thinking 2");
        assert_eq!(messages[4].content, "result 2");
    }

    #[test]
    fn message_role_wire_values_match_existing_protocol_numbers() {
        assert_eq!(MessageRole::User.wire_value(), 1);
        assert_eq!(MessageRole::Assistant.wire_value(), 2);
        assert_eq!(MessageRole::Tool.wire_value(), 4);
        assert_eq!(MessageRole::System.wire_value(), 5);
    }

    #[test]
    fn build_system_prompt_contains_required_protocol_keywords() {
        let prompt = build_system_prompt(3, 6, 5);

        for keyword in [
            "/codebase",
            "restricted_exec",
            "answer",
            "ANSWER",
            "rg",
            "readfile",
            "tree",
            "ls",
            "glob",
            "6",
            "3",
        ] {
            assert!(prompt.contains(keyword), "missing keyword: {keyword}");
        }
        assert!(!prompt.contains("{max_commands}"));
        assert!(!prompt.contains("{max_turns}"));
        assert!(!prompt.contains("{max_results}"));
    }

    #[test]
    fn final_force_answer_prompt_matches_windsurf_source() {
        assert_eq!(
            FINAL_FORCE_ANSWER,
            "No search turns remain. You MUST call the answer tool now, even if the answer is incomplete."
        );
        assert!(FINAL_FORCE_ANSWER.contains("MUST"));
        assert!(FINAL_FORCE_ANSWER.contains("answer tool"));
        assert!(FINAL_FORCE_ANSWER.contains("incomplete"));
    }

    #[test]
    fn tool_definitions_expose_only_restricted_exec_and_answer() {
        let defs: serde_json::Value = serde_json::from_str(&get_tool_definitions(8)).unwrap();
        let tools = defs.as_array().unwrap();

        let names = tools
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["restricted_exec", "answer"]);

        let schema = &tools[0]["function"]["parameters"]["properties"];
        assert!(schema.get("command1").is_some());
        assert!(schema.get("command8").is_some());
        assert!(schema.get("command9").is_none());

        let serialized = serde_json::to_string(&defs).unwrap();
        assert!(serialized.contains("\"rg\""));
        assert!(serialized.contains("\"readfile\""));
        assert!(serialized.contains("\"tree\""));
        assert!(serialized.contains("\"ls\""));
        assert!(serialized.contains("\"glob\""));
    }

    #[test]
    fn parse_response_extracts_text_tool_call() {
        let mut encoder = ProtobufEncoder::new();
        encoder.write_string(1, "thinking");
        encoder.write_string(
            2,
            r#"[TOOL_CALLS]restricted_exec[ARGS]{"command1":{"type":"rg","pattern":"main","path":"/codebase/src"}}"#,
        );
        let frame = connect_frame_encode(&encoder.to_bytes(), false);

        let ParsedModelTurn::ToolCalls { thinking, calls } = parse_response(&frame) else {
            panic!("expected tool call");
        };
        assert_eq!(thinking, "thinking");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "restricted_exec");
        assert_eq!(calls[0].args["command1"]["type"], "rg");
        assert_eq!(calls[0].args["command1"]["pattern"], "main");
    }

    #[test]
    fn parse_response_repairs_restricted_exec_command_fragments() {
        let mut encoder = ProtobufEncoder::new();
        encoder.write_string(1, "thinking");
        encoder.write_string(
            2,
            r#"[TOOL_CALLS]restricted_exec[ARGS]{"command1":{"rg":{"pattern":"SearchResult","path":"/codebase/src"},"command2":{"file":"/codebase/src/core.rs","start_line":1,"end_line":20}"#,
        );
        let frame = connect_frame_encode(&encoder.to_bytes(), false);

        let ParsedModelTurn::ToolCalls { calls, .. } = parse_response(&frame) else {
            panic!("expected repaired tool call");
        };

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "restricted_exec");
        assert_eq!(calls[0].args["command1"]["rg"]["pattern"], "SearchResult");
        assert_eq!(calls[0].args["command1"]["rg"]["path"], "/codebase/src");
        assert_eq!(calls[0].args["command2"]["file"], "/codebase/src/core.rs");
        assert_eq!(calls[0].args["command2"]["start_line"], 1);
        assert_eq!(calls[0].args["command2"]["end_line"], 20);
    }

    #[test]
    fn parse_response_extracts_structured_restricted_exec() {
        let frame = connect_frame_encode(
            br#"{"output":"thinking","tool_calls":[{"id":"a","name":"restricted_exec","args":{"command1":{"type":"glob","pattern":"**/*.rs","path":"/codebase","type_filter":"file"},"command2":{"type":"readfile","file":"/codebase/src/lib.rs","start_line":1,"end_line":20}}}]}"#,
            false,
        );

        let ParsedModelTurn::ToolCalls { thinking, calls } = parse_response(&frame) else {
            panic!("expected structured tool calls");
        };

        assert_eq!(thinking, "thinking");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "a");
        assert_eq!(calls[0].name, "restricted_exec");
        assert_eq!(calls[0].args["command1"]["type"], "glob");
        assert_eq!(calls[0].args["command2"]["type"], "readfile");
        assert_eq!(calls[0].args["command2"]["file"], "/codebase/src/lib.rs");
    }

    #[test]
    fn parse_response_extracts_answer_tool_call() {
        let mut encoder = ProtobufEncoder::new();
        encoder.write_string(
            1,
            r#"[TOOL_CALLS]answer[ARGS]{"answer":"<ANSWER></ANSWER>"}"#,
        );
        let frame = connect_frame_encode(&encoder.to_bytes(), false);

        let ParsedModelTurn::ToolCalls { calls, .. } = parse_response(&frame) else {
            panic!("expected answer");
        };
        assert_eq!(calls[0].name, "answer");
        assert_eq!(calls[0].args["answer"], "<ANSWER></ANSWER>");
    }

    #[test]
    fn parse_response_handles_error_frame() {
        let frame = connect_frame_encode(
            br#"{"error":{"code":"TIMEOUT","message":"request timed out"}}"#,
            false,
        );

        assert_eq!(
            parse_response(&frame),
            ParsedModelTurn::Error("[Error] TIMEOUT: request timed out".to_string())
        );
    }

    #[test]
    fn parse_response_returns_text_without_tool_call() {
        let mut encoder = ProtobufEncoder::new();
        encoder.write_string(1, "plain model text");
        let frame = connect_frame_encode(&encoder.to_bytes(), false);

        assert_eq!(
            parse_response(&frame),
            ParsedModelTurn::Text("plain model text".to_string())
        );
    }
}
