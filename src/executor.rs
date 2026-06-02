mod commands;
mod helpers;

pub use commands::{command_keys, command_number, object_from_hashmap, valid_command_count};

use helpers::{bounded_int, normalize_path, read_int_env};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct ToolExecutor {
    root: PathBuf,
    collected_rg_patterns: Arc<Mutex<Vec<String>>>,
    result_max_lines: usize,
    line_max_chars: usize,
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
        let root = project_root
            .as_ref()
            .canonicalize()
            .unwrap_or_else(|_| normalize_path(project_root.as_ref()));

        Self {
            root,
            collected_rg_patterns: Arc::new(Mutex::new(Vec::new())),
            result_max_lines: bounded_int(
                result_max_lines,
                read_int_env("FC_RESULT_MAX_LINES", 50, 1, 500),
                1,
                500,
            ),
            line_max_chars: bounded_int(
                line_max_chars,
                read_int_env("FC_LINE_MAX_CHARS", 250, 20, 10_000),
                20,
                10_000,
            ),
        }
    }

    pub fn collected_rg_patterns(&self) -> Vec<String> {
        self.collected_rg_patterns
            .lock()
            .map(|patterns| patterns.clone())
            .unwrap_or_default()
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

    fn truncate(&self, text: &str) -> String {
        let lines: Vec<&str> = text.split('\n').collect();
        let limit = lines.len().min(self.result_max_lines);
        let mut truncated = Vec::with_capacity(limit);

        for line in lines.iter().take(limit) {
            if line.len() > self.line_max_chars {
                truncated.push(line[..self.line_max_chars].to_string());
            } else {
                truncated.push((*line).to_string());
            }
        }

        let mut result = truncated.join("\n");
        if lines.len() > self.result_max_lines {
            result.push_str("\n... (lines truncated) ...");
        }
        result
    }
}

#[cfg(test)]
mod tests;
