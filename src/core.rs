mod auth;
mod http;
mod protocol;
mod repo;

pub use auth::{
    check_auth, fetch_jwt, get_api_key, get_cached_jwt, get_config_path, get_jwt_exp,
    load_cached_api_key, save_cached_api_key,
};
pub use http::decode_unary_response;
pub use protocol::{build_system_prompt, get_tool_definitions, limit_tool_args};
pub use repo::{RepoMap, get_repo_map, parse_answer};

use crate::executor::{ToolExecutor, command_keys, valid_command_count};
use crate::path_filter::PathFilterConfig;
use protocol::{ChatMessage, FINAL_FORCE_ANSWER, build_request, parse_response, trim_messages};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::env;
use std::future::Future;
use std::path::PathBuf;
use thiserror::Error;
use uuid::Uuid;

pub const API_BASE: &str =
    "https://server.self-serve.windsurf.com/exa.api_server_pb.ApiServerService";
pub const AUTH_BASE: &str = "https://server.self-serve.windsurf.com/exa.auth_pb.AuthService";
pub const WS_APP: &str = "windsurf";
pub const DEFAULT_WS_APP_VER: &str = "1.48.2";
pub const DEFAULT_WS_LS_VER: &str = "1.9544.35";
pub const MAX_TREE_BYTES: usize = 250 * 1024;

#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct FastContextError {
    pub message: String,
    pub code: String,
    pub details: Value,
}

impl FastContextError {
    pub fn new(message: impl Into<String>, code: impl Into<String>, details: Value) -> Self {
        Self {
            message: message.into(),
            code: code.into(),
            details,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub full_path: String,
    pub ranges: Vec<(usize, usize)>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchMeta {
    pub tree_depth: Option<usize>,
    pub tree_size_kb: Option<f64>,
    pub fell_back: Option<bool>,
    pub project_root: Option<String>,
    pub error_code: Option<String>,
    pub context_trimmed: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    pub files: Vec<FileEntry>,
    pub rg_patterns: Vec<String>,
    pub raw_response: Option<String>,
    pub error: Option<String>,
    pub meta: SearchMeta,
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub query: String,
    pub project_root: PathBuf,
    pub api_key: Option<String>,
    pub jwt: Option<String>,
    pub app_version: Option<String>,
    pub ls_version: Option<String>,
    pub max_turns: usize,
    pub max_commands: usize,
    pub max_results: usize,
    pub tree_depth: usize,
    pub timeout_ms: u64,
    pub path_filter: PathFilterConfig,
    pub result_max_lines: Option<usize>,
    pub line_max_chars: Option<usize>,
}

impl SearchOptions {
    pub fn new(query: impl Into<String>, project_root: impl Into<PathBuf>) -> Self {
        Self {
            query: query.into(),
            project_root: project_root.into(),
            api_key: None,
            jwt: None,
            app_version: None,
            ls_version: None,
            max_turns: 3,
            max_commands: 8,
            max_results: 10,
            tree_depth: 4,
            timeout_ms: 30_000,
            path_filter: PathFilterConfig::default(),
            result_max_lines: None,
            line_max_chars: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthCheck {
    pub ok: bool,
    pub error_code: Option<String>,
    pub error: Option<String>,
    pub jwt_source: String,
    pub app_version: String,
    pub ls_version: String,
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
                error: Some(err),
                meta: SearchMeta {
                    project_root: Some(project_root.to_string_lossy().into_owned()),
                    error_code: Some("API_KEY_ERROR".to_string()),
                    ..SearchMeta::default()
                },
                ..SearchResult::default()
            };
        }
    };

    let mut timeout_ms = options.timeout_ms;
    if let Ok(raw_timeout) = env::var("TIMEOUT")
        && let Ok(value) = raw_timeout.parse::<u64>()
    {
        timeout_ms = value;
    }

    let jwt = match options.jwt.clone() {
        Some(jwt) => jwt,
        None => match get_cached_jwt(&api_key, timeout_ms).await {
            Ok(jwt) => jwt,
            Err(err) => {
                return SearchResult {
                    error: Some(format!("{}: {}", err.code, err.message)),
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
    let tool_defs = get_tool_definitions(options.max_commands);
    let system_prompt =
        build_system_prompt(options.max_turns, options.max_commands, options.max_results);

    let repo_map = get_repo_map(&project_root, options.tree_depth, &options.path_filter);
    let tree_size_kb = repo_map.size_bytes as f64 / 1024.0;
    log(&format!(
        "Repo map: tree -L {} ({tree_size_kb:.1}KB){}",
        repo_map.depth,
        if repo_map.fell_back {
            " [fell back]"
        } else {
            ""
        }
    ));

    let user_content = format!(
        "Problem Statement: {}\n\nRepo Map (tree -L {} /codebase):\n```text\n{}\n```",
        options.query, repo_map.depth, repo_map.tree
    );

    let mut messages = vec![
        ChatMessage::simple(5, system_prompt),
        ChatMessage::simple(1, user_content),
    ];

    let total_api_calls = options.max_turns + 1;
    let mut compensated_turns = 0_usize;
    let max_compensations = 2_usize;
    let mut force_answer_injected = false;

    for turn in 0..(total_api_calls + max_compensations) {
        if turn >= total_api_calls + compensated_turns {
            break;
        }

        log(&format!(
            "Turn {}/{}",
            turn + 1,
            total_api_calls + compensated_turns
        ));

        let proto = build_request(
            &api_key,
            &jwt,
            &messages,
            &tool_defs,
            options.app_version.as_deref(),
            options.ls_version.as_deref(),
        );

        let response = match streaming(proto, timeout_ms, 2, options.ls_version.clone()).await {
            Ok(response) => response,
            Err(err) => {
                let base_meta = SearchMeta {
                    tree_depth: Some(repo_map.depth),
                    tree_size_kb: Some((tree_size_kb * 10.0).round() / 10.0),
                    fell_back: Some(repo_map.fell_back),
                    project_root: Some(project_root.to_string_lossy().into_owned()),
                    error_code: Some(err.code.clone()),
                    ..SearchMeta::default()
                };

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
                                error: Some(format!(
                                    "{}: {} (retry failure)",
                                    retry_err.code, retry_err.message
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
                        error: Some(format!("{}: {}", err.code, err.message)),
                        meta: base_meta,
                        ..SearchResult::default()
                    };
                }
            }
        };

        let (thinking, tool_info) = parse_response(&response);
        let Some((tool_name, tool_args)) = tool_info else {
            if thinking.starts_with("[Error]") {
                return SearchResult {
                    files: Vec::new(),
                    error: Some(thinking),
                    ..SearchResult::default()
                };
            }
            return SearchResult {
                files: Vec::new(),
                raw_response: Some(thinking),
                ..SearchResult::default()
            };
        };

        if tool_name == "answer" {
            let answer_xml = tool_args
                .get("answer")
                .and_then(Value::as_str)
                .unwrap_or_default();
            log("Received final answer");
            let mut result = parse_answer(answer_xml, &project_root);
            result.rg_patterns = unique_patterns(executor.collected_rg_patterns());
            result.meta = SearchMeta {
                tree_depth: Some(repo_map.depth),
                tree_size_kb: Some((tree_size_kb * 10.0).round() / 10.0),
                fell_back: Some(repo_map.fell_back),
                ..SearchMeta::default()
            };
            return result;
        }

        if tool_name == "restricted_exec" {
            let call_id = Uuid::new_v4().to_string();
            let limited_args = limit_tool_args(&tool_args, options.max_commands);
            let args_json = limited_args.to_string();
            let cmds = command_keys(&limited_args);
            log(&format!("Executing {} local commands", cmds.len()));

            let results = executor.exec_tool_call(&limited_args);
            if valid_command_count(&limited_args) == 0 && compensated_turns < max_compensations {
                compensated_turns += 1;
                log(&format!(
                    "Turn compensation: no valid commands, extending search ({compensated_turns}/{max_compensations})"
                ));
            } else if valid_command_count(&limited_args) == 0 {
                log("Turn compensation skipped: limit reached");
            }

            messages.push(ChatMessage {
                role: 2,
                content: thinking,
                tool_call_id: Some(call_id.clone()),
                tool_name: Some("restricted_exec".to_string()),
                tool_args_json: Some(args_json),
                ref_call_id: None,
            });
            messages.push(ChatMessage {
                role: 4,
                content: results,
                tool_call_id: None,
                tool_name: None,
                tool_args_json: None,
                ref_call_id: Some(call_id),
            });

            let effective_turn = turn.saturating_sub(compensated_turns);
            if effective_turn >= options.max_turns.saturating_sub(1) && !force_answer_injected {
                messages.push(ChatMessage::simple(1, FINAL_FORCE_ANSWER));
                force_answer_injected = true;
                log("Injected force-answer prompt");
            }
        }
    }

    SearchResult {
        files: Vec::new(),
        rg_patterns: unique_patterns(executor.collected_rg_patterns()),
        error: Some("Max turns reached without getting an answer".to_string()),
        meta: SearchMeta {
            tree_depth: Some(repo_map.depth),
            tree_size_kb: Some((tree_size_kb * 10.0).round() / 10.0),
            fell_back: Some(repo_map.fell_back),
            project_root: Some(project_root.to_string_lossy().into_owned()),
            ..SearchMeta::default()
        },
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

pub async fn search_with_content(options: SearchOptions) -> String {
    let max_turns = options.max_turns;
    let max_results = options.max_results;
    let result = search(options.clone(), None).await;

    if let Some(error) = result.error {
        let mut err_msg = format!("Error: {error}");
        if result.meta.tree_depth.is_some() || result.meta.error_code.is_some() {
            err_msg.push_str(&format!(
                "\n\n[diagnostic] error_type={}, tree_depth_used={:?}, tree_size={:?}KB",
                result.meta.error_code.as_deref().unwrap_or("unknown"),
                result.meta.tree_depth,
                result.meta.tree_size_kb
            ));
            if result.meta.fell_back == Some(true) {
                err_msg.push_str(" (auto fell back)");
            }
            if result.meta.context_trimmed == Some(true) {
                err_msg.push_str(", context_trimmed=true");
            }
            if let Some(project_root) = result.meta.project_root {
                err_msg.push_str(&format!("\n[diagnostic] project_path={project_root}"));
            }
            err_msg.push_str(&format!(
                "\n[config] max_turns={max_turns}, max_results={max_results}, max_commands={}",
                options.max_commands
            ));

            match result.meta.error_code.as_deref() {
                Some("PAYLOAD_TOO_LARGE" | "TIMEOUT") => {
                    err_msg.push_str(
                        "\n[hint] Try: reduce tree_depth, add exclude.txt entries, or narrow project_path.",
                    );
                }
                Some("AUTH_ERROR") => {
                    err_msg.push_str(
                        "\n[hint] Authentication error. Ensure a fresh WINDSURF_API_KEY is configured.",
                    );
                }
                Some("RATE_LIMITED") => {
                    err_msg.push_str("\n[hint] Rate limited. Wait a moment and retry.");
                }
                _ => {}
            }
        }
        return err_msg;
    }

    let unique_patterns = result
        .rg_patterns
        .iter()
        .filter(|pattern| pattern.len() >= 3)
        .cloned()
        .collect::<Vec<_>>();

    if result.files.is_empty() && unique_patterns.is_empty() {
        return result
            .raw_response
            .map(|raw| format!("No relevant files found.\n\nRaw response:\n{raw}"))
            .unwrap_or_else(|| "No relevant files found.".to_string());
    }

    let mut parts = Vec::new();
    if result.files.is_empty() {
        parts.push("No files found.".to_string());
    } else {
        parts.push(format!("Found {} relevant files.\n", result.files.len()));
        for (idx, entry) in result.files.iter().enumerate() {
            let ranges = entry
                .ranges
                .iter()
                .map(|(start, end)| format!("L{start}-{end}"))
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!(
                "  [{}/{}] {} ({ranges})",
                idx + 1,
                result.files.len(),
                entry.full_path
            ));
        }
    }

    if !unique_patterns.is_empty() {
        parts.push(String::new());
        parts.push(format!("grep keywords: {}", unique_patterns.join(", ")));
    }

    if result.meta.tree_depth.is_some() {
        parts.push(String::new());
        let fb_note = if result.meta.fell_back == Some(true) {
            " (fell back)"
        } else {
            ""
        };
        parts.push(format!(
            "[config] tree_depth={}{}{}, tree_size={}KB, max_turns={max_turns}, max_results={max_results}",
            result.meta.tree_depth.unwrap_or_default(),
            fb_note,
            "",
            result.meta.tree_size_kb.unwrap_or_default()
        ));
    }

    parts.join("\n")
}

#[cfg(test)]
mod tests;
