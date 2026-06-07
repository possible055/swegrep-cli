use crate::executor::{
    InstantContextToolCall as ExecutorToolCall, InstantContextToolUpdate, ToolExecutionStatus,
    ToolExecutor,
};
use crate::protobuf::{connect_frame_decode, extract_strings};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::env;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::auth::{get_api_key, get_cached_jwt};
use super::credentials;
use super::error::FastContextError;
use super::http;
use super::protocol::{
    ChatMessage, FINAL_FORCE_ANSWER, MessageRole, ParsedModelTurn, build_request,
    build_system_prompt, get_tool_definitions, parse_response, trim_messages,
};
use super::repo::{RepoMap, get_repo_map, parse_answer};
use super::types::{SearchError, SearchMeta, SearchOptions, SearchResult};
use super::{SearchOutputConfig, format_search_error, format_search_success};

const MAX_COMPENSATED_TURNS: usize = 2;
const DEBUG_PREVIEW_CHARS: usize = 4_000;

#[derive(Debug, Clone)]
struct DebugLogger {
    path: Option<PathBuf>,
}

impl DebugLogger {
    fn from_env() -> Self {
        if !debug_enabled() {
            return Self { path: None };
        }

        let path = env::var_os("SWEGREP_DEBUG_LOG")
            .map(PathBuf::from)
            .unwrap_or_else(default_debug_log_path);
        Self { path: Some(path) }
    }

    fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn log(&self, message: impl AsRef<str>) {
        let Some(path) = &self.path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{} {}", debug_timestamp_ms(), message.as_ref());
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct InstantContextStep {
    id: String,
    tool_calls: Vec<ExecutorToolCall>,
}

impl InstantContextStep {
    fn from_calls(id: String, calls: Vec<ExecutorToolCall>) -> Self {
        let tool_calls = calls;
        Self { id, tool_calls }
    }
}

pub async fn search(
    options: SearchOptions,
    on_progress: Option<&(dyn Fn(&str) + Sync)>,
) -> SearchResult {
    search_with_streaming(
        options,
        on_progress,
        |proto, timeout_ms, max_retries, ls_version| async move {
            http::streaming_request(&proto, timeout_ms, max_retries, ls_version.as_deref()).await
        },
    )
    .await
}

pub async fn search_with_streaming<F, Fut>(
    mut options: SearchOptions,
    on_progress: Option<&(dyn Fn(&str) + Sync)>,
    mut streaming: F,
) -> SearchResult
where
    F: FnMut(Vec<u8>, u64, u32, Option<String>) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, FastContextError>>,
{
    let project_root = options
        .project_root
        .canonicalize()
        .unwrap_or_else(|_| options.project_root.clone());
    options.project_root = project_root.clone();

    let log = |message: &str| {
        if let Some(on_progress) = on_progress {
            on_progress(message);
        }
    };
    let debug = DebugLogger::from_env();
    if let Some(path) = debug.path() {
        log(&format!("Debug log: {}", path.display()));
        debug.log(format!(
            "search_start project_root={} max_turns={} max_commands={} query={}",
            project_root.display(),
            options.max_turns,
            options.max_commands,
            debug_preview(&options.query, DEBUG_PREVIEW_CHARS)
        ));
    }

    let api_key = match get_api_key(options.api_key.as_deref(), true) {
        Ok(api_key) => api_key,
        Err(err) => {
            return SearchResult {
                error: Some(SearchError::new("API_KEY_ERROR", err)),
                meta: SearchMeta {
                    project_root: Some(project_root.to_string_lossy().into_owned()),
                    error_code: Some("API_KEY_ERROR".to_string()),
                    ..SearchMeta::default()
                },
                ..SearchResult::default()
            };
        }
    };

    let timeout_ms = options.timeout_ms;

    let jwt = match options.jwt.clone() {
        Some(jwt) => jwt,
        None => match get_cached_jwt(&api_key, timeout_ms).await {
            Ok(jwt) => jwt,
            Err(err) => {
                return SearchResult {
                    error: Some(SearchError::new(err.code.clone(), err.message)),
                    meta: SearchMeta {
                        project_root: Some(project_root.to_string_lossy().into_owned()),
                        error_code: Some(err.code),
                        ..SearchMeta::default()
                    },
                    ..SearchResult::default()
                };
            }
        },
    };

    let executor = ToolExecutor::with_limits_and_filter(
        &project_root,
        options.result_max_lines,
        options.line_max_chars,
        options.path_filter.clone(),
    );
    for warning in executor.path_filter_warnings() {
        log(&format!("Path filter warning: {warning}"));
    }
    options.max_commands = options.max_commands.clamp(1, 8);
    let tool_defs = get_tool_definitions(options.max_commands);
    let system_prompt =
        build_system_prompt(options.max_turns, options.max_commands, options.max_results);

    let repo_map = get_repo_map(&project_root, options.tree_depth, &options.path_filter);
    log(&format!(
        "Repo map: scope snapshot depth {} ({:.1}KB){}",
        repo_map.depth,
        repo_map.size_bytes as f64 / 1024.0,
        if repo_map.fell_back {
            " [fell back]"
        } else {
            ""
        }
    ));
    let user_content = build_user_content(&options.query, &repo_map);

    let mut messages = vec![
        ChatMessage::simple(MessageRole::System, system_prompt),
        ChatMessage::simple(MessageRole::User, user_content),
    ];

    let mut force_answer_injected = false;
    let mut compensated_turns = 0_usize;
    let mut turn = 0_usize;

    while turn < options.max_turns + 1 + compensated_turns {
        let progress_turn = format_search_progress_turn(
            turn,
            compensated_turns,
            options.max_turns,
            force_answer_injected,
        );
        log(&progress_turn);

        let proto = build_request(
            &api_key,
            &jwt,
            &messages,
            &tool_defs,
            options.app_version.as_deref(),
            options.ls_version.as_deref(),
        );

        let mut response = match streaming(proto, timeout_ms, 2, options.ls_version.clone()).await {
            Ok(response) => response,
            Err(err) => {
                let base_meta = search_meta(&project_root, &repo_map, Some(err.code.clone()));

                if matches!(err.code.as_str(), "PAYLOAD_TOO_LARGE" | "TIMEOUT")
                    && messages.len() > 4
                {
                    log(&format!(
                        "{} on {}: trimming context and retrying...",
                        err.code,
                        format_search_turn_ref(turn, compensated_turns, options.max_turns)
                    ));
                    trim_messages(&mut messages);
                    let retry_proto = build_request(
                        &api_key,
                        &jwt,
                        &messages,
                        &tool_defs,
                        options.app_version.as_deref(),
                        options.ls_version.as_deref(),
                    );
                    match streaming(retry_proto, timeout_ms, 2, options.ls_version.clone()).await {
                        Ok(response) => response,
                        Err(retry_err) => {
                            return SearchResult {
                                files: Vec::new(),
                                error: Some(SearchError::new(
                                    retry_err.code.clone(),
                                    format!("{} (retry failure)", retry_err.message),
                                )),
                                meta: SearchMeta {
                                    error_code: Some(retry_err.code),
                                    context_trimmed: Some(true),
                                    ..base_meta
                                },
                                ..SearchResult::default()
                            };
                        }
                    }
                } else {
                    return SearchResult {
                        files: Vec::new(),
                        error: Some(SearchError::new(err.code, err.message)),
                        meta: base_meta,
                        ..SearchResult::default()
                    };
                }
            }
        };

        let mut parsed = parse_response(&response);
        debug_model_response(&debug, &progress_turn, &response, &parsed);
        if matches!(&parsed, ParsedModelTurn::Text(text) if text.trim().is_empty()) {
            log("Empty model response on turn; retrying once...");
            let retry_proto = build_request(
                &api_key,
                &jwt,
                &messages,
                &tool_defs,
                options.app_version.as_deref(),
                options.ls_version.as_deref(),
            );
            response = match streaming(retry_proto, timeout_ms, 2, options.ls_version.clone()).await
            {
                Ok(response) => response,
                Err(err) => {
                    return SearchResult {
                        files: Vec::new(),
                        error: Some(SearchError::new(err.code.clone(), err.message)),
                        meta: search_meta(&project_root, &repo_map, Some(err.code)),
                        ..SearchResult::default()
                    };
                }
            };
            parsed = parse_response(&response);
            debug_model_response(
                &debug,
                &format!("{progress_turn} retry"),
                &response,
                &parsed,
            );
            if matches!(&parsed, ParsedModelTurn::Text(text) if text.trim().is_empty()) {
                return SearchResult {
                    files: Vec::new(),
                    error: Some(SearchError::new(
                        "MODEL_EMPTY_RESPONSE",
                        "Model returned empty response",
                    )),
                    meta: search_meta(
                        &project_root,
                        &repo_map,
                        Some("MODEL_EMPTY_RESPONSE".to_string()),
                    ),
                    ..SearchResult::default()
                };
            }
        }

        let (thinking, tool_calls) = match parsed {
            ParsedModelTurn::ToolCalls { thinking, calls } => (thinking, calls),
            ParsedModelTurn::Error(error) => {
                return SearchResult {
                    files: Vec::new(),
                    error: Some(SearchError::new("MODEL_ERROR", error)),
                    meta: search_meta(&project_root, &repo_map, Some("MODEL_ERROR".to_string())),
                    ..SearchResult::default()
                };
            }
            ParsedModelTurn::Text(text) => {
                if text.contains("<ANSWER") {
                    log("Received final answer");
                    let mut result = parse_answer(&text, &project_root);
                    result.files.truncate(options.max_results);
                    debug_answer_result(&debug, "text", &text, result.files.len());
                    result.rg_patterns = unique_patterns(executor.collected_rg_patterns());
                    result.meta = search_meta(&project_root, &repo_map, None);
                    return result;
                }

                let effective_turns_used = (turn + 1)
                    .saturating_sub(compensated_turns)
                    .min(options.max_turns);
                if !force_answer_injected {
                    messages.push(ChatMessage::simple(MessageRole::Assistant, text.clone()));
                    if effective_turns_used < options.max_turns {
                        messages.push(ChatMessage::simple(
                            MessageRole::User,
                            format_search_turn_status(effective_turns_used, options.max_turns),
                        ));
                        log("Plain-text response without answer; continuing search");
                    } else {
                        messages.push(ChatMessage::simple(MessageRole::User, FINAL_FORCE_ANSWER));
                        force_answer_injected = true;
                        log("Injected force-answer prompt after plain-text response");
                    }
                    turn += 1;
                    continue;
                }

                return SearchResult {
                    files: Vec::new(),
                    rg_patterns: unique_patterns(executor.collected_rg_patterns()),
                    raw_response: Some(text),
                    error: None,
                    meta: search_meta(&project_root, &repo_map, None),
                };
            }
        };

        if let Some(answer_call) = tool_calls.iter().find(|call| call.name == "answer") {
            let answer_xml = answer_call
                .args
                .get("answer")
                .and_then(Value::as_str)
                .unwrap_or_default();
            log("Received final answer");
            let mut result = parse_answer(answer_xml, &project_root);
            result.files.truncate(options.max_results);
            debug_answer_result(&debug, "tool_call", answer_xml, result.files.len());
            result.rg_patterns = unique_patterns(executor.collected_rg_patterns());
            result.meta = search_meta(&project_root, &repo_map, None);
            return result;
        }

        let tool_call = tool_calls
            .iter()
            .find(|call| call.name == "restricted_exec")
            .cloned()
            .or_else(|| tool_calls.into_iter().next());
        let Some(tool_call) = tool_call else {
            turn += 1;
            continue;
        };
        let (step, assistant_tool_name, assistant_tool_args, tool_ref_id) = if tool_call.name
            == "restricted_exec"
        {
            let has_valid_command =
                has_valid_restricted_exec_command(&tool_call.args, options.max_commands);
            let step = InstantContextStep::from_calls(
                format!("restricted-exec-{}", turn + 1),
                restricted_exec_commands(&tool_call.args, options.max_commands),
            );
            if !has_valid_command && compensated_turns < MAX_COMPENSATED_TURNS {
                compensated_turns += 1;
                log(&format!(
                    "No valid restricted_exec commands; scheduling compensation turn {compensated_turns}/{MAX_COMPENSATED_TURNS}"
                ));
            } else if !has_valid_command {
                log(&format!(
                    "Turn compensation skipped: max compensations ({MAX_COMPENSATED_TURNS}) reached"
                ));
            }
            (
                step,
                tool_call.name,
                tool_call.args.to_string(),
                tool_call.id,
            )
        } else {
            let command = serde_json::json!({
                "type": tool_call.name,
                "arguments": tool_call.args,
            });
            let step = InstantContextStep::from_calls(
                format!("unsupported-tool-{}", turn + 1),
                vec![ExecutorToolCall {
                    id: "command1".to_string(),
                    name: "command1".to_string(),
                    args: command,
                }],
            );
            (
                step,
                tool_call.name,
                tool_call.args.to_string(),
                tool_call.id,
            )
        };

        log(&format!(
            "Executing {} restricted_exec commands",
            step.tool_calls.len()
        ));
        let timeout_budget_ms = (timeout_ms as u128 / 2).max(1_000);
        let updates =
            executor.exec_restricted_exec_step(&step.id, &step.tool_calls, timeout_budget_ms);
        let results = format_restricted_exec_results(&updates);

        messages.push(ChatMessage {
            role: MessageRole::Assistant,
            content: thinking,
            tool_call_id: Some(tool_ref_id.clone()),
            tool_name: Some(assistant_tool_name),
            tool_args_json: Some(assistant_tool_args),
            ref_call_id: None,
        });
        messages.push(ChatMessage {
            role: MessageRole::Tool,
            content: results,
            tool_call_id: None,
            tool_name: None,
            tool_args_json: None,
            ref_call_id: Some(tool_ref_id),
        });

        let effective_turns_used = (turn + 1)
            .saturating_sub(compensated_turns)
            .min(options.max_turns);
        if effective_turns_used < options.max_turns {
            messages.push(ChatMessage::simple(
                MessageRole::User,
                format_search_turn_status(effective_turns_used, options.max_turns),
            ));
        } else if !force_answer_injected {
            messages.push(ChatMessage::simple(MessageRole::User, FINAL_FORCE_ANSWER));
            force_answer_injected = true;
            log("Injected force-answer prompt");
        }

        turn += 1;
    }

    SearchResult {
        files: Vec::new(),
        rg_patterns: unique_patterns(executor.collected_rg_patterns()),
        error: Some(SearchError::new(
            "MAX_TURNS_REACHED",
            "Max turns reached without getting an answer",
        )),
        meta: search_meta(
            &project_root,
            &repo_map,
            Some("MAX_TURNS_REACHED".to_string()),
        ),
        ..SearchResult::default()
    }
}

fn unique_patterns(patterns: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for pattern in patterns {
        if seen.insert(pattern.clone()) {
            unique.push(pattern);
        }
    }
    unique
}

fn debug_enabled() -> bool {
    env::var("SWEGREP_DEBUG")
        .ok()
        .is_some_and(|value| matches_bool_env(&value))
}

fn matches_bool_env(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn default_debug_log_path() -> PathBuf {
    credentials::get_config_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("debug.log")
}

fn debug_timestamp_ms() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("[{millis}]")
}

fn debug_model_response(
    debug: &DebugLogger,
    turn_label: &str,
    response: &[u8],
    parsed: &ParsedModelTurn,
) {
    if debug.path().is_none() {
        return;
    }
    debug.log(format!(
        "model_response turn={} bytes={} parsed={}",
        debug_preview(turn_label, 128),
        response.len(),
        debug_parsed_turn_summary(parsed)
    ));
    let strings = debug_response_strings(response);
    if !strings.is_empty() {
        debug.log(format!(
            "model_response_strings turn={} values={}",
            debug_preview(turn_label, 128),
            debug_preview(&strings.join(" | "), DEBUG_PREVIEW_CHARS)
        ));
    }
}

fn debug_response_strings(response: &[u8]) -> Vec<String> {
    connect_frame_decode(response)
        .into_iter()
        .flat_map(|frame| {
            let mut values = extract_strings(&frame);
            let raw = String::from_utf8_lossy(&frame).replace('\u{fffd}', "");
            if raw.len() > 10 && !values.iter().any(|value| value == &raw) {
                values.push(raw);
            }
            values
        })
        .map(|value| debug_preview(&value, DEBUG_PREVIEW_CHARS))
        .filter(|value| !value.trim().is_empty())
        .collect()
}

fn debug_parsed_turn_summary(parsed: &ParsedModelTurn) -> String {
    match parsed {
        ParsedModelTurn::ToolCalls { thinking, calls } => {
            let call_count = calls.len();
            let call_summaries = calls
                .iter()
                .map(|call| {
                    format!(
                        "{} args={}",
                        debug_preview(&call.name, 128),
                        debug_preview(&call.args.to_string(), DEBUG_PREVIEW_CHARS)
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            format!(
                "tool_calls count={} thinking={} calls=[{}]",
                call_count,
                debug_preview(thinking, DEBUG_PREVIEW_CHARS),
                call_summaries
            )
        }
        ParsedModelTurn::Text(text) => {
            format!("text value={}", debug_preview(text, DEBUG_PREVIEW_CHARS))
        }
        ParsedModelTurn::Error(error) => {
            format!("error value={}", debug_preview(error, DEBUG_PREVIEW_CHARS))
        }
    }
}

fn debug_answer_result(debug: &DebugLogger, source: &str, answer_xml: &str, file_count: usize) {
    debug.log(format!(
        "final_answer source={} files={} xml={}",
        source,
        file_count,
        debug_preview(answer_xml, DEBUG_PREVIEW_CHARS)
    ));
}

fn debug_preview(value: &str, max_chars: usize) -> String {
    let mut preview = value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    if preview.chars().count() > max_chars {
        preview = preview.chars().take(max_chars).collect::<String>();
        preview.push_str("...");
    }
    preview
}

fn format_search_turn_status(used: usize, max_turns: usize) -> String {
    format!(
        "Search turns used: {used}. Search turns remaining: {}.",
        max_turns.saturating_sub(used)
    )
}

fn format_search_progress_turn(
    turn: usize,
    compensated_turns: usize,
    max_turns: usize,
    force_answer_injected: bool,
) -> String {
    if force_answer_injected {
        return "Final answer turn".to_string();
    }
    if is_compensation_turn(turn, compensated_turns, max_turns) {
        return format!("Compensation turn {compensated_turns}/{MAX_COMPENSATED_TURNS}");
    }
    format!(
        "Turn {}",
        format_search_turn_ref(turn, compensated_turns, max_turns)
    )
}

fn is_compensation_turn(turn: usize, compensated_turns: usize, max_turns: usize) -> bool {
    compensated_turns > 0 && (turn + 1).saturating_sub(compensated_turns) >= max_turns
}

fn format_search_turn_ref(turn: usize, compensated_turns: usize, max_turns: usize) -> String {
    let max_turns = max_turns.max(1);
    let used = (turn + 1)
        .saturating_sub(compensated_turns)
        .max(1)
        .min(max_turns);
    format!("{used}/{max_turns}")
}

fn build_user_content(query: &str, repo_map: &RepoMap) -> String {
    format!(
        r#"Workspace scope snapshot (depth {}):
```text
{}
```

Please find the code context for the query: "({})". Constrain search to directory: "(/codebase)"."#,
        repo_map.depth, repo_map.tree, query
    )
}

fn search_meta(
    project_root: &std::path::Path,
    repo_map: &RepoMap,
    error_code: Option<String>,
) -> SearchMeta {
    SearchMeta {
        tree_depth: Some(repo_map.depth),
        tree_size_kb: Some((repo_map.size_bytes as f64 / 1024.0 * 10.0).round() / 10.0),
        fell_back: Some(repo_map.fell_back),
        project_root: Some(project_root.to_string_lossy().into_owned()),
        error_code,
        instant_context: Some(true),
        ..SearchMeta::default()
    }
}

fn restricted_exec_commands(args: &Value, max_commands: usize) -> Vec<ExecutorToolCall> {
    let Some(map) = args.as_object() else {
        return missing_restricted_exec_command();
    };

    let commands = (1..=max_commands.clamp(1, 8))
        .filter_map(|idx| {
            let key = format!("command{idx}");
            map.get(&key).map(|command| ExecutorToolCall {
                id: key.clone(),
                name: key,
                args: normalize_restricted_exec_command(command),
            })
        })
        .collect::<Vec<_>>();
    if commands.is_empty() {
        missing_restricted_exec_command()
    } else {
        commands
    }
}

fn missing_restricted_exec_command() -> Vec<ExecutorToolCall> {
    vec![ExecutorToolCall {
        id: "command1".to_string(),
        name: "command1".to_string(),
        args: serde_json::json!({"type": "", "error": "missing restricted_exec command object"}),
    }]
}

fn has_valid_restricted_exec_command(args: &Value, max_commands: usize) -> bool {
    let Some(map) = args.as_object() else {
        return false;
    };

    (1..=max_commands.clamp(1, 8)).any(|idx| {
        let key = format!("command{idx}");
        map.get(&key)
            .map(normalize_restricted_exec_command)
            .and_then(|command| {
                command
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .is_some_and(|command_type| !command_type.trim().is_empty())
    })
}

fn normalize_restricted_exec_command(command: &Value) -> Value {
    let Some(map) = command.as_object() else {
        return command.clone();
    };
    if map
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|command_type| !command_type.trim().is_empty())
    {
        return command.clone();
    }

    for command_type in ["rg", "readfile", "tree", "ls", "glob"] {
        if let Some(normalized) = normalize_shorthand_command(command_type, map) {
            return normalized;
        }
    }

    if let Some(command_type) = infer_restricted_exec_command_type(map) {
        let mut normalized = map.clone();
        normalized.insert("type".to_string(), Value::String(command_type.to_string()));
        return Value::Object(normalized);
    }

    command.clone()
}

fn infer_restricted_exec_command_type(map: &Map<String, Value>) -> Option<&'static str> {
    if map.get("file").and_then(Value::as_str).is_some() {
        return Some("readfile");
    }
    if map.get("pattern").and_then(Value::as_str).is_some()
        && map.get("path").and_then(Value::as_str).is_some()
    {
        if map.get("type_filter").and_then(Value::as_str).is_some() {
            return Some("glob");
        }
        return Some("rg");
    }
    if map.get("path").and_then(Value::as_str).is_some()
        && map.get("levels").and_then(Value::as_i64).is_some()
    {
        return Some("tree");
    }
    if map.get("path").and_then(Value::as_str).is_some()
        && (map.get("long_format").and_then(Value::as_bool).is_some()
            || map.get("all").and_then(Value::as_bool).is_some())
    {
        return Some("ls");
    }
    None
}

fn normalize_shorthand_command(command_type: &str, map: &Map<String, Value>) -> Option<Value> {
    let shorthand = map.get(command_type)?;
    match shorthand {
        Value::Object(inner) => {
            let mut normalized = inner.clone();
            normalized
                .entry("type".to_string())
                .or_insert_with(|| Value::String(command_type.to_string()));
            Some(Value::Object(normalized))
        }
        Value::String(value) => {
            let mut normalized = Map::new();
            normalized.insert("type".to_string(), Value::String(command_type.to_string()));
            let target_field = match command_type {
                "rg" | "glob" => "pattern",
                "readfile" => "file",
                "tree" | "ls" => "path",
                _ => return None,
            };
            normalized.insert(target_field.to_string(), Value::String(value.clone()));
            for (key, value) in map {
                if key != command_type {
                    normalized.insert(key.clone(), value.clone());
                }
            }
            Some(Value::Object(normalized))
        }
        _ => None,
    }
}

fn format_restricted_exec_results(updates: &[InstantContextToolUpdate]) -> String {
    let mut out = String::new();
    for update in updates {
        if update.status == ToolExecutionStatus::Pending {
            continue;
        }
        out.push_str(&format!(
            "{}_result:\n{}\n",
            update.tool_call_id, update.output
        ));
    }
    out
}

pub async fn search_with_content(options: SearchOptions) -> String {
    let max_turns = options.max_turns;
    let max_results = options.max_results;
    let max_commands = options.max_commands;
    let result = search(options.clone(), None).await;

    if result.error.is_some() {
        return format_search_error(
            &result,
            Some(SearchOutputConfig {
                max_turns,
                max_results,
                max_commands,
            }),
        );
    }

    let mut output = format_search_success(&result);
    if result.meta.tree_depth.is_some() {
        let fb_note = if result.meta.fell_back == Some(true) {
            " (fell back)"
        } else {
            ""
        };
        output.push_str(&format!(
            "\n\n[config] tree_depth={}{}{}, tree_size={}KB, max_turns={max_turns}, max_results={max_results}",
            result.meta.tree_depth.unwrap_or_default(),
            fb_note,
            "",
            result.meta.tree_size_kb.unwrap_or_default()
        ));
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::InstantContextTiming;
    use crate::protobuf::{ProtobufEncoder, connect_frame_encode};
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    #[test]
    fn build_user_content_includes_compact_scope_snapshot() {
        let repo_map = RepoMap {
            tree: "/codebase\nCargo.toml\nsrc/\nsrc/lib.rs".to_string(),
            depth: 2,
            size_bytes: 35,
            fell_back: false,
        };

        let content = build_user_content("find parser", &repo_map);

        assert!(content.contains("Workspace scope snapshot (depth 2):"));
        assert!(content.contains("/codebase"));
        assert!(content.contains("Cargo.toml"));
        assert!(content.contains("src/"));
        assert!(content.contains(r#"Please find the code context for the query: "(find parser)""#));
        assert!(content.contains(r#"Constrain search to directory: "(/codebase)""#));
    }

    #[test]
    fn restricted_exec_results_hide_internal_status_and_timing() {
        let updates = vec![
            InstantContextToolUpdate {
                step_id: "step-1".to_string(),
                tool_call_id: "command1".to_string(),
                tool_name: "command1".to_string(),
                command: serde_json::json!({"type": "rg", "pattern": "main", "path": "/codebase"}),
                status: ToolExecutionStatus::Completed,
                output: "/codebase/src/main.rs:1:fn main() {}".to_string(),
                timing: InstantContextTiming { duration_ms: 12 },
            },
            InstantContextToolUpdate {
                step_id: "step-1".to_string(),
                tool_call_id: "command2".to_string(),
                tool_name: "command2".to_string(),
                command: serde_json::json!({"type": "readfile", "file": "/codebase/src/lib.rs"}),
                status: ToolExecutionStatus::TimedOut,
                output: "Error: tool timed out".to_string(),
                timing: InstantContextTiming { duration_ms: 1_000 },
            },
        ];

        let output = format_restricted_exec_results(&updates);

        assert!(output.contains("command1_result:\n/codebase/src/main.rs:1:fn main() {}"));
        assert!(output.contains("command2_result:\nError: tool timed out"));
        assert!(!output.contains("status="));
        assert!(!output.contains("duration_ms="));
    }

    #[test]
    fn restricted_exec_commands_keep_standard_command_shape() {
        let args = serde_json::json!({
            "command1": {
                "type": "rg",
                "pattern": "needle",
                "path": "/codebase/src"
            }
        });

        assert!(has_valid_restricted_exec_command(&args, 8));
        let commands = restricted_exec_commands(&args, 8);

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].args, args["command1"]);
    }

    #[test]
    fn restricted_exec_commands_normalize_model_shorthand_commands() {
        let args = serde_json::json!({
            "command1": {
                "rg": {
                    "pattern": "auth",
                    "path": "/codebase/src",
                    "exclude": ["test"]
                }
            },
            "command2": {
                "readfile": "/codebase/src/core.rs",
                "start_line": 1,
                "end_line": 10
            },
            "command3": {
                "rg": "parse_answer",
                "path": "/codebase/src/core"
            },
            "command4": {
                "glob": "**/*.rs",
                "path": "/codebase/src"
            }
        });

        assert!(has_valid_restricted_exec_command(&args, 8));
        let commands = restricted_exec_commands(&args, 8);

        assert_eq!(commands.len(), 4);
        assert_eq!(commands[0].args["type"], "rg");
        assert_eq!(commands[0].args["pattern"], "auth");
        assert_eq!(commands[0].args["path"], "/codebase/src");
        assert_eq!(commands[1].args["type"], "readfile");
        assert_eq!(commands[1].args["file"], "/codebase/src/core.rs");
        assert_eq!(commands[1].args["start_line"], 1);
        assert_eq!(commands[2].args["type"], "rg");
        assert_eq!(commands[2].args["pattern"], "parse_answer");
        assert_eq!(commands[2].args["path"], "/codebase/src/core");
        assert_eq!(commands[3].args["type"], "glob");
        assert_eq!(commands[3].args["pattern"], "**/*.rs");
        assert_eq!(commands[3].args["path"], "/codebase/src");
    }

    #[test]
    fn restricted_exec_commands_infer_missing_command_types_from_fields() {
        let args = serde_json::json!({
            "command1": {
                "file": "/codebase/src/core.rs",
                "start_line": 1,
                "end_line": 10
            },
            "command2": {
                "pattern": "parse_answer",
                "path": "/codebase/src"
            },
            "command3": {
                "pattern": "**/*.rs",
                "path": "/codebase/src",
                "type_filter": "file"
            },
            "command4": {
                "path": "/codebase/src",
                "levels": 2
            },
            "command5": {
                "path": "/codebase/src",
                "all": false
            }
        });

        assert!(has_valid_restricted_exec_command(&args, 8));
        let commands = restricted_exec_commands(&args, 8);

        assert_eq!(commands.len(), 5);
        assert_eq!(commands[0].args["type"], "readfile");
        assert_eq!(commands[1].args["type"], "rg");
        assert_eq!(commands[2].args["type"], "glob");
        assert_eq!(commands[3].args["type"], "tree");
        assert_eq!(commands[4].args["type"], "ls");
    }

    #[test]
    fn search_progress_turn_log_uses_configured_search_turn_count() {
        assert_eq!(format_search_progress_turn(3, 0, 4, false), "Turn 4/4");
        assert_eq!(format_search_progress_turn(4, 0, 5, false), "Turn 5/5");
        assert_eq!(format_search_progress_turn(5, 0, 6, false), "Turn 6/6");
        assert_eq!(
            format_search_progress_turn(5, 1, 5, false),
            "Compensation turn 1/2"
        );
        assert_eq!(
            format_search_progress_turn(6, 2, 5, false),
            "Compensation turn 2/2"
        );
        assert_eq!(
            format_search_progress_turn(3, 0, 3, true),
            "Final answer turn"
        );
    }

    #[test]
    fn debug_parsed_turn_summary_includes_model_values() {
        let parsed = ParsedModelTurn::ToolCalls {
            thinking: "look\naround".to_string(),
            calls: vec![crate::core::protocol::ParsedToolCall {
                id: "tool-1".to_string(),
                name: "restricted_exec".to_string(),
                args: serde_json::json!({
                    "command1": {
                        "type": "rg",
                        "pattern": "format_search_success",
                        "path": "/codebase/src"
                    }
                }),
            }],
        };

        let summary = debug_parsed_turn_summary(&parsed);

        assert!(summary.contains("tool_calls count=1"));
        assert!(summary.contains("restricted_exec"));
        assert!(summary.contains("format_search_success"));
        assert!(summary.contains("look\\naround"));
    }

    #[test]
    fn debug_preview_truncates_and_escapes_control_characters() {
        assert_eq!(debug_preview("a\nb\tc", 20), "a\\nb\\tc");
        assert_eq!(debug_preview("abcdef", 3), "abc...");
    }

    #[test]
    fn debug_model_response_writes_model_values_to_log() {
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("debug.log");
        let debug = DebugLogger {
            path: Some(log_path.clone()),
        };
        let mut response = ProtobufEncoder::new();
        response.write_string(1, "model text value");
        let frame = connect_frame_encode(&response.to_bytes(), false);
        let parsed = parse_response(&frame);

        debug_model_response(&debug, "Turn 1/4", &frame, &parsed);

        let log = fs::read_to_string(log_path).unwrap();
        assert!(log.contains("model_response turn=Turn 1/4"));
        assert!(log.contains("model_response_strings"));
        assert!(log.contains("model text value"));
    }

    #[tokio::test]
    async fn search_loop_success_with_mock_streaming() {
        let mut t1_encoder = ProtobufEncoder::new();
        t1_encoder.write_string(1, "thinking about doing search");
        t1_encoder.write_string(
            2,
            r#"[TOOL_CALLS]restricted_exec[ARGS]{"command1":{"type":"readfile","file":"/codebase/test.txt"}}"#,
        );
        let t1_frame = connect_frame_encode(&t1_encoder.to_bytes(), false);

        let mut t2_encoder = ProtobufEncoder::new();
        t2_encoder.write_string(1, "found answer");
        t2_encoder.write_string(
            2,
            r#"[TOOL_CALLS]answer[ARGS]{"answer": "<ANSWER><file path=\"/codebase/test.txt\"><range>1-10</range></file></ANSWER>"}"#,
        );
        let t2_frame = connect_frame_encode(&t2_encoder.to_bytes(), false);

        let responses = Arc::new(Mutex::new(VecDeque::from([t1_frame, t2_frame])));
        let call_count = Arc::new(AtomicUsize::new(0));
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.txt"), "line1\nline2").unwrap();

        let mut options = SearchOptions::new("find main", tmp.path());
        options.api_key = Some("sk-ws-01-key".to_string());
        options.jwt = Some("mocked.jwt.token".to_string());
        options.max_turns = 2;

        let result = search_with_streaming(options, None, {
            let responses = Arc::clone(&responses);
            let call_count = Arc::clone(&call_count);
            move |_, _, _, _| {
                let responses = Arc::clone(&responses);
                let call_count = Arc::clone(&call_count);
                async move {
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(responses.lock().unwrap().pop_front().unwrap())
                }
            }
        })
        .await;

        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, "test.txt");
        assert_eq!(result.files[0].ranges, vec![(1, 2)]);
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn search_loop_supports_restricted_exec_then_answer() {
        let t1_frame = connect_frame_encode(
            br#"{"output":"find candidates","tool_calls":[{"id":"q1","name":"restricted_exec","args":{"command1":{"type":"glob","pattern":"test.txt","path":"/codebase","type_filter":"file"},"command2":{"type":"readfile","file":"/codebase/test.txt","start_line":1,"end_line":1}}}]}"#,
            false,
        );

        let mut t2_encoder = ProtobufEncoder::new();
        t2_encoder.write_string(
            1,
            r#"[TOOL_CALLS]answer[ARGS]{"answer": "<ANSWER><file path=\"/codebase/test.txt\"><range>1-1</range></file></ANSWER>"}"#,
        );
        let t2_frame = connect_frame_encode(&t2_encoder.to_bytes(), false);

        let responses = Arc::new(Mutex::new(VecDeque::from([t1_frame, t2_frame])));
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.txt"), "line1").unwrap();

        let mut options = SearchOptions::new("find test", tmp.path());
        options.api_key = Some("sk-ws-01-key".to_string());
        options.jwt = Some("mocked.jwt.token".to_string());
        options.max_turns = 2;

        let result = search_with_streaming(options, None, {
            let responses = Arc::clone(&responses);
            move |_, _, _, _| {
                let responses = Arc::clone(&responses);
                async move { Ok(responses.lock().unwrap().pop_front().unwrap()) }
            }
        })
        .await;

        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, "test.txt");
        assert_eq!(result.files[0].ranges, vec![(1, 1)]);
    }

    #[tokio::test]
    async fn search_loop_reports_remaining_search_turns_after_valid_tool_round() {
        let t1_frame = connect_frame_encode(
            br#"{"output":"read context","tool_calls":[{"id":"q1","name":"restricted_exec","args":{"command1":{"type":"readfile","file":"/codebase/test.txt","start_line":1,"end_line":1}}}]}"#,
            false,
        );
        let mut answer_encoder = ProtobufEncoder::new();
        answer_encoder.write_string(
            1,
            r#"[TOOL_CALLS]answer[ARGS]{"answer": "<ANSWER><file path=\"/codebase/test.txt\"><range>1-1</range></file></ANSWER>"}"#,
        );
        let answer_frame = connect_frame_encode(&answer_encoder.to_bytes(), false);

        let responses = Arc::new(Mutex::new(VecDeque::from([t1_frame, answer_frame])));
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.txt"), "line1").unwrap();

        let mut options = SearchOptions::new("find test", tmp.path());
        options.api_key = Some("sk-ws-01-key".to_string());
        options.jwt = Some("mocked.jwt.token".to_string());
        options.max_turns = 3;

        let result = search_with_streaming(options, None, {
            let responses = Arc::clone(&responses);
            let requests = Arc::clone(&requests);
            move |proto, _, _, _| {
                let responses = Arc::clone(&responses);
                let requests = Arc::clone(&requests);
                async move {
                    requests
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&proto).to_string());
                    Ok(responses.lock().unwrap().pop_front().unwrap())
                }
            }
        })
        .await;

        assert!(result.error.is_none());
        assert_eq!(result.files.len(), 1);

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains("Search turns used: 1. Search turns remaining: 2."));
        assert!(!requests[1].contains(FINAL_FORCE_ANSWER));
    }

    #[tokio::test]
    async fn search_loop_injects_force_answer_after_last_search_turn() {
        let t1_frame = connect_frame_encode(
            br#"{"output":"first read","tool_calls":[{"id":"q1","name":"restricted_exec","args":{"command1":{"type":"readfile","file":"/codebase/test.txt","start_line":1,"end_line":1}}}]}"#,
            false,
        );
        let t2_frame = connect_frame_encode(
            br#"{"output":"second read","tool_calls":[{"id":"q2","name":"restricted_exec","args":{"command1":{"type":"readfile","file":"/codebase/test.txt","start_line":2,"end_line":2}}}]}"#,
            false,
        );
        let mut answer_encoder = ProtobufEncoder::new();
        answer_encoder.write_string(
            1,
            r#"[TOOL_CALLS]answer[ARGS]{"answer": "<ANSWER><file path=\"/codebase/test.txt\"><range>1-2</range></file></ANSWER>"}"#,
        );
        let answer_frame = connect_frame_encode(&answer_encoder.to_bytes(), false);

        let responses = Arc::new(Mutex::new(VecDeque::from([
            t1_frame,
            t2_frame,
            answer_frame,
        ])));
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.txt"), "line1\nline2").unwrap();

        let mut options = SearchOptions::new("find test", tmp.path());
        options.api_key = Some("sk-ws-01-key".to_string());
        options.jwt = Some("mocked.jwt.token".to_string());
        options.max_turns = 2;

        let result = search_with_streaming(options, None, {
            let responses = Arc::clone(&responses);
            let requests = Arc::clone(&requests);
            move |proto, _, _, _| {
                let responses = Arc::clone(&responses);
                let requests = Arc::clone(&requests);
                async move {
                    requests
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&proto).to_string());
                    Ok(responses.lock().unwrap().pop_front().unwrap())
                }
            }
        })
        .await;

        assert!(result.error.is_none());
        assert_eq!(result.files.len(), 1);

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].contains("Search turns used: 1. Search turns remaining: 1."));
        assert!(!requests[1].contains(FINAL_FORCE_ANSWER));
        assert!(requests[2].contains(FINAL_FORCE_ANSWER));
    }

    #[tokio::test]
    async fn search_loop_empty_answer_does_not_fallback_to_tool_history() {
        let t1_frame = connect_frame_encode(
            br#"{"output":"read context","tool_calls":[{"id":"q1","name":"restricted_exec","args":{"command1":{"type":"readfile","file":"/codebase/test.txt","start_line":1,"end_line":2}}}]}"#,
            false,
        );

        let mut t2_encoder = ProtobufEncoder::new();
        t2_encoder.write_string(
            1,
            r#"[TOOL_CALLS]answer[ARGS]{"answer": "<ANSWER></ANSWER>"}"#,
        );
        let t2_frame = connect_frame_encode(&t2_encoder.to_bytes(), false);

        let responses = Arc::new(Mutex::new(VecDeque::from([t1_frame, t2_frame])));
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.txt"), "line1\nline2\nline3").unwrap();

        let mut options = SearchOptions::new("find test", tmp.path());
        options.api_key = Some("sk-ws-01-key".to_string());
        options.jwt = Some("mocked.jwt.token".to_string());
        options.max_turns = 2;

        let result = search_with_streaming(options, None, {
            let responses = Arc::clone(&responses);
            let requests = Arc::clone(&requests);
            move |proto, _, _, _| {
                let responses = Arc::clone(&responses);
                let requests = Arc::clone(&requests);
                async move {
                    requests
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&proto).to_string());
                    Ok(responses.lock().unwrap().pop_front().unwrap())
                }
            }
        })
        .await;

        assert!(result.files.is_empty());

        let requests = requests.lock().unwrap();
        assert!(requests[0].contains("Workspace scope snapshot (depth "));
        assert!(requests[0].contains("/codebase"));
        assert!(requests[0].contains("test.txt"));
        assert!(requests[1].contains("command1_result:"));
        assert!(!requests[1].contains("status="));
        assert!(!requests[1].contains("duration_ms="));
    }

    #[tokio::test]
    async fn search_loop_uses_final_answer_without_tool_history_ranges() {
        let t1_frame = connect_frame_encode(
            br#"{"output":"read context","tool_calls":[{"id":"q1","name":"restricted_exec","args":{"command1":{"type":"readfile","file":"/codebase/test.txt","start_line":1,"end_line":2}}}]}"#,
            false,
        );

        let mut t2_encoder = ProtobufEncoder::new();
        t2_encoder.write_string(
            1,
            r#"[TOOL_CALLS]answer[ARGS]{"answer": "<ANSWER><file path=\"/codebase/test.txt\"><range>3-3</range></file></ANSWER>"}"#,
        );
        let t2_frame = connect_frame_encode(&t2_encoder.to_bytes(), false);

        let responses = Arc::new(Mutex::new(VecDeque::from([t1_frame, t2_frame])));
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.txt"), "line1\nline2\nline3").unwrap();

        let mut options = SearchOptions::new("find test", tmp.path());
        options.api_key = Some("sk-ws-01-key".to_string());
        options.jwt = Some("mocked.jwt.token".to_string());
        options.max_turns = 2;

        let result = search_with_streaming(options, None, {
            let responses = Arc::clone(&responses);
            move |_, _, _, _| {
                let responses = Arc::clone(&responses);
                async move { Ok(responses.lock().unwrap().pop_front().unwrap()) }
            }
        })
        .await;

        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, "test.txt");
        assert_eq!(result.files[0].ranges, vec![(3, 3)]);
    }

    #[tokio::test]
    async fn search_loop_accepts_plain_text_answer() {
        let mut t1_encoder = ProtobufEncoder::new();
        t1_encoder.write_string(
            1,
            r#"<ANSWER><file path="/codebase/test.txt"><range>1-1</range></file></ANSWER>"#,
        );
        let t1_frame = connect_frame_encode(&t1_encoder.to_bytes(), false);

        let responses = Arc::new(Mutex::new(VecDeque::from([t1_frame])));
        let call_count = Arc::new(AtomicUsize::new(0));
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.txt"), "line1").unwrap();

        let mut options = SearchOptions::new("find main", tmp.path());
        options.api_key = Some("sk-ws-01-key".to_string());
        options.jwt = Some("mocked.jwt.token".to_string());
        options.max_turns = 2;

        let result = search_with_streaming(options, None, {
            let responses = Arc::clone(&responses);
            let call_count = Arc::clone(&call_count);
            move |_, _, _, _| {
                let responses = Arc::clone(&responses);
                let call_count = Arc::clone(&call_count);
                async move {
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(responses.lock().unwrap().pop_front().unwrap())
                }
            }
        })
        .await;

        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, "test.txt");
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn search_loop_returns_raw_response_for_plain_text_after_force_answer() {
        let mut t1_encoder = ProtobufEncoder::new();
        t1_encoder.write_string(1, "I could not find a matching implementation.");
        let t1_frame = connect_frame_encode(&t1_encoder.to_bytes(), false);

        let mut t2_encoder = ProtobufEncoder::new();
        t2_encoder.write_string(1, "Still no structured answer.");
        let t2_frame = connect_frame_encode(&t2_encoder.to_bytes(), false);

        let responses = Arc::new(Mutex::new(VecDeque::from([t1_frame, t2_frame])));
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let tmp = TempDir::new().unwrap();

        let mut options = SearchOptions::new("find missing thing", tmp.path());
        options.api_key = Some("sk-ws-01-key".to_string());
        options.jwt = Some("mocked.jwt.token".to_string());
        options.max_turns = 1;

        let result = search_with_streaming(options, None, {
            let responses = Arc::clone(&responses);
            let requests = Arc::clone(&requests);
            move |proto, _, _, _| {
                let responses = Arc::clone(&responses);
                let requests = Arc::clone(&requests);
                async move {
                    requests
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&proto).to_string());
                    Ok(responses.lock().unwrap().pop_front().unwrap())
                }
            }
        })
        .await;

        assert!(result.error.is_none());
        assert_eq!(
            result.raw_response.as_deref(),
            Some("Still no structured answer.")
        );
        assert!(result.files.is_empty());

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains(FINAL_FORCE_ANSWER));
    }

    #[tokio::test]
    async fn search_loop_injects_force_answer_after_plain_text_on_last_search_turn() {
        let t1_frame = connect_frame_encode(
            br#"{"output":"read context","tool_calls":[{"id":"q1","name":"restricted_exec","args":{"command1":{"type":"readfile","file":"/codebase/test.txt","start_line":1,"end_line":1}}}]}"#,
            false,
        );

        let mut t2_encoder = ProtobufEncoder::new();
        t2_encoder.write_string(1, "I could not find a matching implementation.");
        let t2_frame = connect_frame_encode(&t2_encoder.to_bytes(), false);

        let mut answer_encoder = ProtobufEncoder::new();
        answer_encoder.write_string(
            1,
            r#"[TOOL_CALLS]answer[ARGS]{"answer": "<ANSWER><file path=\"/codebase/test.txt\"><range>1-1</range></file></ANSWER>"}"#,
        );
        let answer_frame = connect_frame_encode(&answer_encoder.to_bytes(), false);

        let responses = Arc::new(Mutex::new(VecDeque::from([
            t1_frame,
            t2_frame,
            answer_frame,
        ])));
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.txt"), "line1").unwrap();

        let mut options = SearchOptions::new("find test", tmp.path());
        options.api_key = Some("sk-ws-01-key".to_string());
        options.jwt = Some("mocked.jwt.token".to_string());
        options.max_turns = 2;

        let result = search_with_streaming(options, None, {
            let responses = Arc::clone(&responses);
            let requests = Arc::clone(&requests);
            move |proto, _, _, _| {
                let responses = Arc::clone(&responses);
                let requests = Arc::clone(&requests);
                async move {
                    requests
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&proto).to_string());
                    Ok(responses.lock().unwrap().pop_front().unwrap())
                }
            }
        })
        .await;

        assert!(result.error.is_none());
        assert_eq!(result.files.len(), 1);

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[2].contains(FINAL_FORCE_ANSWER));
        assert!(requests[2].contains("I could not find a matching implementation."));
    }

    #[tokio::test]
    async fn search_loop_retries_empty_model_response_once() {
        let mut t2_encoder = ProtobufEncoder::new();
        t2_encoder.write_string(
            1,
            r#"<ANSWER><file path="/codebase/test.txt"><range>1-1</range></file></ANSWER>"#,
        );
        let t2_frame = connect_frame_encode(&t2_encoder.to_bytes(), false);

        let responses = Arc::new(Mutex::new(VecDeque::from([Vec::new(), t2_frame])));
        let call_count = Arc::new(AtomicUsize::new(0));
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.txt"), "line1").unwrap();

        let mut options = SearchOptions::new("find test", tmp.path());
        options.api_key = Some("sk-ws-01-key".to_string());
        options.jwt = Some("mocked.jwt.token".to_string());
        options.max_turns = 2;

        let result = search_with_streaming(options, None, {
            let responses = Arc::clone(&responses);
            let call_count = Arc::clone(&call_count);
            move |_, _, _, _| {
                let responses = Arc::clone(&responses);
                let call_count = Arc::clone(&call_count);
                async move {
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(responses.lock().unwrap().pop_front().unwrap())
                }
            }
        })
        .await;

        assert!(result.error.is_none());
        assert_eq!(result.files.len(), 1);
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn search_loop_errors_after_empty_response_retry_exhausted() {
        let responses = Arc::new(Mutex::new(VecDeque::from([Vec::new(), Vec::new()])));
        let call_count = Arc::new(AtomicUsize::new(0));
        let tmp = TempDir::new().unwrap();

        let mut options = SearchOptions::new("find test", tmp.path());
        options.api_key = Some("sk-ws-01-key".to_string());
        options.jwt = Some("mocked.jwt.token".to_string());
        options.max_turns = 2;

        let result = search_with_streaming(options, None, {
            let responses = Arc::clone(&responses);
            let call_count = Arc::clone(&call_count);
            move |_, _, _, _| {
                let responses = Arc::clone(&responses);
                let call_count = Arc::clone(&call_count);
                async move {
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(responses.lock().unwrap().pop_front().unwrap())
                }
            }
        })
        .await;

        let error = result.error.as_ref().unwrap();
        assert_eq!(error.code, "MODEL_EMPTY_RESPONSE");
        assert_eq!(error.message, "Model returned empty response");
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn search_loop_compensates_invalid_restricted_exec_once() {
        let invalid_frame = connect_frame_encode(
            br#"{"output":"bad command","tool_calls":[{"id":"q1","name":"restricted_exec","args":{}}]}"#,
            false,
        );
        let valid_frame = connect_frame_encode(
            br#"{"output":"read context","tool_calls":[{"id":"q2","name":"restricted_exec","args":{"command1":{"type":"readfile","file":"/codebase/test.txt","start_line":1,"end_line":1}}}]}"#,
            false,
        );
        let mut answer_encoder = ProtobufEncoder::new();
        answer_encoder.write_string(
            1,
            r#"[TOOL_CALLS]answer[ARGS]{"answer": "<ANSWER><file path=\"/codebase/test.txt\"><range>1-1</range></file></ANSWER>"}"#,
        );
        let answer_frame = connect_frame_encode(&answer_encoder.to_bytes(), false);

        let responses = Arc::new(Mutex::new(VecDeque::from([
            invalid_frame,
            valid_frame,
            answer_frame,
        ])));
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let call_count = Arc::new(AtomicUsize::new(0));
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.txt"), "line1").unwrap();

        let mut options = SearchOptions::new("find test", tmp.path());
        options.api_key = Some("sk-ws-01-key".to_string());
        options.jwt = Some("mocked.jwt.token".to_string());
        options.max_turns = 2;

        let result = search_with_streaming(options, None, {
            let responses = Arc::clone(&responses);
            let requests = Arc::clone(&requests);
            let call_count = Arc::clone(&call_count);
            move |proto, _, _, _| {
                let responses = Arc::clone(&responses);
                let requests = Arc::clone(&requests);
                let call_count = Arc::clone(&call_count);
                async move {
                    call_count.fetch_add(1, Ordering::SeqCst);
                    requests
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&proto).to_string());
                    Ok(responses.lock().unwrap().pop_front().unwrap())
                }
            }
        })
        .await;

        assert!(result.error.is_none());
        assert_eq!(result.files.len(), 1);
        assert_eq!(call_count.load(Ordering::SeqCst), 3);

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].contains("Search turns used: 0. Search turns remaining: 2."));
        assert!(requests[2].contains("Search turns used: 1. Search turns remaining: 1."));
        assert!(!requests[2].contains(FINAL_FORCE_ANSWER));
    }
}
