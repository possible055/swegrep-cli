use crate::executor::{
    InstantContextToolCall as ExecutorToolCall, InstantContextToolUpdate, ToolExecutionStatus,
    ToolExecutor,
};
use serde_json::Value;
use std::collections::HashSet;
use std::future::Future;

use super::auth::{get_api_key, get_cached_jwt};
use super::error::FastContextError;
use super::http;
use super::protocol::{
    ChatMessage, FINAL_FORCE_ANSWER, MessageRole, ParsedModelTurn, build_request,
    build_system_prompt, get_tool_definitions, parse_response, trim_messages,
};
use super::repo::{RepoMap, get_repo_map, parse_answer};
use super::types::{SearchError, SearchMeta, SearchOptions, SearchResult};
use super::{SearchOutputConfig, format_search_error, format_search_success};

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

    let total_api_calls = options.max_turns + 1;
    const MAX_COMPENSATED_TURNS: usize = 2;
    let mut force_answer_injected = false;
    let mut compensated_turns = 0_usize;
    let mut turn = 0_usize;

    while turn < total_api_calls + compensated_turns {
        log(&format!("Turn {}/{}", turn + 1, total_api_calls));

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
                        "{} on turn {}: trimming context and retrying...",
                        err.code,
                        turn + 1
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
                    result.rg_patterns = unique_patterns(executor.collected_rg_patterns());
                    result.meta = search_meta(&project_root, &repo_map, None);
                    return result;
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
                    "Turn compensation: no valid restricted_exec commands, extending search by 1 turn ({compensated_turns}/{MAX_COMPENSATED_TURNS})"
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

fn format_search_turn_status(used: usize, max_turns: usize) -> String {
    format!(
        "Search turns used: {used}. Search turns remaining: {}.",
        max_turns.saturating_sub(used)
    )
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
                args: command.clone(),
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
            .and_then(Value::as_object)
            .and_then(|command| command.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|command_type| !command_type.trim().is_empty())
    })
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
    async fn search_loop_returns_raw_response_for_plain_text_without_answer() {
        let mut t1_encoder = ProtobufEncoder::new();
        t1_encoder.write_string(1, "I could not find a matching implementation.");
        let t1_frame = connect_frame_encode(&t1_encoder.to_bytes(), false);

        let responses = Arc::new(Mutex::new(VecDeque::from([t1_frame])));
        let tmp = TempDir::new().unwrap();

        let mut options = SearchOptions::new("find missing thing", tmp.path());
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

        assert!(result.error.is_none());
        assert_eq!(
            result.raw_response.as_deref(),
            Some("I could not find a matching implementation.")
        );
        assert!(result.files.is_empty());
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
