mod commands;
mod helpers;

use crate::path_filter::{PathFilter, PathFilterConfig};
use helpers::{bounded_int, normalize_path, read_int_env};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolExecutionStatus {
    Pending,
    Completed,
    Error,
    TimedOut,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstantContextTiming {
    pub duration_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstantContextToolUpdate {
    pub step_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub command: Value,
    pub status: ToolExecutionStatus,
    pub output: String,
    pub timing: InstantContextTiming,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstantContextToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

#[derive(Debug, Clone)]
pub struct ToolExecutor {
    root: PathBuf,
    collected_rg_patterns: Arc<Mutex<Vec<String>>>,
    legacy_result_max_lines: usize,
    legacy_line_max_chars: usize,
    instant_context_result_max_lines: usize,
    instant_context_readfile_max_lines: usize,
    instant_context_line_max_chars: usize,
    path_filter: Arc<PathFilter>,
}

impl ToolExecutor {
    pub fn new(project_root: impl AsRef<Path>) -> Self {
        Self::with_limits(project_root, None, None)
    }

    pub fn with_limits(
        project_root: impl AsRef<Path>,
        result_max_lines: Option<usize>,
        line_max_chars: Option<usize>,
    ) -> Self {
        Self::with_limits_and_filter(
            project_root,
            result_max_lines,
            line_max_chars,
            PathFilterConfig::default(),
        )
    }

    pub fn with_limits_and_filter(
        project_root: impl AsRef<Path>,
        result_max_lines: Option<usize>,
        line_max_chars: Option<usize>,
        path_filter_config: PathFilterConfig,
    ) -> Self {
        let root = project_root
            .as_ref()
            .canonicalize()
            .unwrap_or_else(|_| normalize_path(project_root.as_ref()));
        let path_filter = Arc::new(PathFilter::new(&root, path_filter_config));

        Self {
            root,
            collected_rg_patterns: Arc::new(Mutex::new(Vec::new())),
            legacy_result_max_lines: bounded_int(result_max_lines, 50, 1, 500),
            legacy_line_max_chars: bounded_int(line_max_chars, 250, 20, 10_000),
            instant_context_result_max_lines: bounded_int(
                result_max_lines,
                read_int_env("FC_RESULT_MAX_LINES", 80, 1, 500),
                1,
                500,
            ),
            instant_context_readfile_max_lines: read_int_env(
                "FC_READFILE_MAX_LINES",
                200,
                1,
                10_000,
            ),
            instant_context_line_max_chars: bounded_int(
                line_max_chars,
                read_int_env("FC_LINE_MAX_CHARS", 300, 20, 10_000),
                20,
                10_000,
            ),
            path_filter,
        }
    }

    pub fn collected_rg_patterns(&self) -> Vec<String> {
        self.collected_rg_patterns
            .lock()
            .map(|patterns| patterns.clone())
            .unwrap_or_default()
    }

    pub fn path_filter_warnings(&self) -> &[String] {
        self.path_filter.warnings()
    }

    pub fn path_filter_enabled(&self) -> bool {
        self.path_filter.is_enabled()
    }

    pub fn real(&self, virtual_path: &str) -> PathBuf {
        if virtual_path.is_empty() {
            return self.root.clone();
        }

        let raw_path =
            if virtual_path.starts_with("/codebase") || virtual_path.starts_with("\\codebase") {
                let rel = virtual_path
                    .trim_start_matches("/codebase")
                    .trim_start_matches("\\codebase")
                    .trim_start_matches(['/', '\\']);
                self.root.join(rel)
            } else {
                let path = PathBuf::from(virtual_path);
                if path.is_absolute() {
                    path
                } else {
                    env::current_dir()
                        .unwrap_or_else(|_| self.root.clone())
                        .join(path)
                }
            };

        let resolved = normalize_path(&raw_path);
        if resolved.starts_with(&self.root) {
            resolved
        } else {
            self.root.clone()
        }
    }

    fn remap(&self, text: &str) -> String {
        let root_native = self.root.to_string_lossy();
        let root_slash = self.root.to_string_lossy().replace('\\', "/");
        let mut remapped = text.replace(root_native.as_ref(), "/codebase");
        if root_native != root_slash {
            remapped = remapped.replace(&root_slash, "/codebase");
        }
        remapped
    }

    fn truncate_legacy(&self, text: &str) -> String {
        self.truncate_with_limits(
            text,
            self.legacy_result_max_lines,
            self.legacy_line_max_chars,
        )
    }

    fn truncate_instant_context(&self, text: &str) -> String {
        self.truncate_with_limits(
            text,
            self.instant_context_result_max_lines,
            self.instant_context_line_max_chars,
        )
    }

    fn truncate_instant_context_readfile(&self, text: &str) -> String {
        self.truncate_with_limits(
            text,
            self.instant_context_readfile_max_lines,
            self.instant_context_line_max_chars,
        )
    }

    fn truncate_text(&self, text: &str, legacy: bool, readfile_tool: bool) -> String {
        if legacy {
            self.truncate_legacy(text)
        } else if readfile_tool {
            self.truncate_instant_context_readfile(text)
        } else {
            self.truncate_instant_context(text)
        }
    }

    fn truncate_with_limits(&self, text: &str, max_lines: usize, line_max_chars: usize) -> String {
        let lines: Vec<&str> = text.split('\n').collect();
        let limit = lines.len().min(max_lines);
        let mut truncated = Vec::with_capacity(limit);

        for line in lines.iter().take(limit) {
            if line.len() > line_max_chars {
                truncated.push(line[..line_max_chars].to_string());
            } else {
                truncated.push((*line).to_string());
            }
        }

        let mut result = truncated.join("\n");
        if lines.len() > max_lines {
            result.push_str("\n... (lines truncated) ...");
        }
        result
    }

    fn is_visible_path(&self, path: &Path, is_dir: bool) -> bool {
        self.path_filter.is_visible(path, is_dir)
    }

    pub(crate) fn path_visible(&self, path: &Path, is_dir: bool) -> bool {
        self.is_visible_path(path, is_dir)
    }
}

#[cfg(test)]
mod tests;
