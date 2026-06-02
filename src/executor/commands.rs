use super::helpers::{
    compare_dirs_first_case_insensitive, matches_any_pattern, matches_type, pattern_matches,
};
use super::{
    InstantContextTiming, InstantContextToolCall, InstantContextToolUpdate, ToolExecutionStatus,
    ToolExecutor,
};
use serde_json::Value;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

const RG_FILE_CHUNK_SIZE: usize = 200;

impl ToolExecutor {
    pub fn rg(
        &self,
        pattern: &str,
        path: &str,
        include: Option<&[String]>,
        exclude: Option<&[String]>,
    ) -> String {
        self.rg_with_truncation(pattern, path, include, exclude, true)
    }

    fn rg_with_truncation(
        &self,
        pattern: &str,
        path: &str,
        include: Option<&[String]>,
        exclude: Option<&[String]>,
        legacy: bool,
    ) -> String {
        if pattern.is_empty() {
            return "Error: missing or invalid pattern".to_string();
        }
        if path.is_empty() {
            return "Error: missing or invalid path".to_string();
        }

        if let Ok(mut patterns) = self.collected_rg_patterns.lock() {
            patterns.push(pattern.to_string());
        }

        let real_path = self.real(path);
        if !real_path.exists() {
            return format!("Error: path does not exist: {path}");
        }

        if !self.path_filter_enabled() {
            return self.rg_native(pattern, &real_path, include, exclude, legacy);
        }

        self.rg_filtered(pattern, &real_path, include, exclude, legacy)
    }

    fn rg_filtered(
        &self,
        pattern: &str,
        real_path: &Path,
        include: Option<&[String]>,
        exclude: Option<&[String]>,
        legacy: bool,
    ) -> String {
        let files = self.rg_search_files(real_path, include, exclude);
        if files.is_empty() {
            return "(no matches)".to_string();
        }

        let mut all_stdout = String::new();
        for chunk in files.chunks(RG_FILE_CHUNK_SIZE) {
            let mut args = vec![
                "--no-config".to_string(),
                "--no-ignore".to_string(),
                "--hidden".to_string(),
                "--no-heading".to_string(),
                "--with-filename".to_string(),
                "-n".to_string(),
                "--max-count".to_string(),
                "50".to_string(),
                "--".to_string(),
                pattern.to_string(),
            ];
            args.extend(
                chunk
                    .iter()
                    .map(|file| file.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
            );

            match Command::new("rg").args(args).output() {
                Ok(output) => {
                    let code = output.status.code().unwrap_or(-1);
                    if code == 1 {
                        continue;
                    }
                    if code == 0 {
                        all_stdout.push_str(&String::from_utf8_lossy(&output.stdout));
                        continue;
                    }
                    if !output.stderr.is_empty() {
                        return self.truncate_text(
                            &self.remap(&String::from_utf8_lossy(&output.stderr)),
                            legacy,
                            false,
                        );
                    }
                    return format!("Error: exit status {code}");
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    return "Error: ripgrep ('rg') is not installed or not in PATH.".to_string();
                }
                Err(err) => return format!("Error: {err}"),
            }
        }

        if all_stdout.is_empty() {
            "(no matches)".to_string()
        } else {
            self.truncate_text(&self.remap(&all_stdout), legacy, false)
        }
    }

    fn rg_native(
        &self,
        pattern: &str,
        real_path: &Path,
        include: Option<&[String]>,
        exclude: Option<&[String]>,
        legacy: bool,
    ) -> String {
        let mut args = vec![
            "--no-config".to_string(),
            "--no-heading".to_string(),
            "--with-filename".to_string(),
            "-n".to_string(),
            "--max-count".to_string(),
            "50".to_string(),
        ];
        if let Some(include) = include {
            for glob in include {
                args.push("--glob".to_string());
                args.push(glob.clone());
            }
        }
        if let Some(exclude) = exclude {
            for glob in exclude {
                args.push("--glob".to_string());
                args.push(format!("!{glob}"));
            }
        }
        args.push("--".to_string());
        args.push(pattern.to_string());
        args.push(real_path.to_string_lossy().into_owned());

        match Command::new("rg").args(args).output() {
            Ok(output) => {
                let code = output.status.code().unwrap_or(-1);
                if code == 1 {
                    return "(no matches)".to_string();
                }
                if code == 0 {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    return self.truncate_text(&self.remap(&stdout), legacy, false);
                }
                if !output.stderr.is_empty() {
                    return self.truncate_text(
                        &self.remap(&String::from_utf8_lossy(&output.stderr)),
                        legacy,
                        false,
                    );
                }
                format!("Error: exit status {code}")
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                "Error: ripgrep ('rg') is not installed or not in PATH.".to_string()
            }
            Err(err) => format!("Error: {err}"),
        }
    }

    fn rg_search_files(
        &self,
        real_path: &Path,
        include: Option<&[String]>,
        exclude: Option<&[String]>,
    ) -> Vec<PathBuf> {
        if real_path.is_file() {
            return if self.is_visible_for_rg(real_path, include, exclude) {
                vec![real_path.to_path_buf()]
            } else {
                Vec::new()
            };
        }

        let mut files = WalkDir::new(real_path)
            .into_iter()
            .filter_entry(|entry| {
                entry.depth() == 0 || self.is_visible_path(entry.path(), entry.file_type().is_dir())
            })
            .flatten()
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| entry.path().to_path_buf())
            .filter(|path| self.is_visible_for_rg(path, include, exclude))
            .collect::<Vec<_>>();
        files.sort();
        files
    }

    fn is_visible_for_rg(
        &self,
        path: &Path,
        include: Option<&[String]>,
        exclude: Option<&[String]>,
    ) -> bool {
        if !self.is_visible_path(path, false) {
            return false;
        }

        let rel = path
            .strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();

        if let Some(include) = include
            && !include.is_empty()
            && !matches_any_pattern(include, name, &rel)
        {
            return false;
        }
        if let Some(exclude) = exclude
            && matches_any_pattern(exclude, name, &rel)
        {
            return false;
        }
        true
    }

    pub fn readfile(
        &self,
        file: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> String {
        self.readfile_with_mode(file, start_line, end_line, true)
    }

    fn readfile_with_mode(
        &self,
        file: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
        legacy: bool,
    ) -> String {
        if file.is_empty() {
            return "Error: missing or invalid file path".to_string();
        }
        let real_path = self.real(file);
        if !real_path.is_file() {
            return format!("Error: file not found: {file}");
        }

        let content = match fs::read(&real_path) {
            Ok(content) => String::from_utf8_lossy(&content).into_owned(),
            Err(err) => return format!("Error: {err}"),
        };

        let all_lines: Vec<&str> = content.split('\n').collect();
        let start = start_line.unwrap_or(1).saturating_sub(1);
        let end = end_line.unwrap_or(all_lines.len()).min(all_lines.len());
        let selected = if start < end && start < all_lines.len() {
            &all_lines[start..end]
        } else {
            &[]
        };

        let out = selected
            .iter()
            .enumerate()
            .map(|(idx, line)| format!("{}:{line}", start + idx + 1))
            .collect::<Vec<_>>()
            .join("\n");
        self.truncate_text(&out, legacy, !legacy)
    }

    pub fn tree(
        &self,
        path: &str,
        levels: Option<usize>,
        exclude_paths: Option<&[String]>,
        truncate: bool,
    ) -> String {
        self.tree_with_mode(path, levels, exclude_paths, truncate, true)
    }

    fn tree_with_mode(
        &self,
        path: &str,
        levels: Option<usize>,
        exclude_paths: Option<&[String]>,
        truncate: bool,
        legacy: bool,
    ) -> String {
        if path.is_empty() {
            return "Error: missing or invalid path".to_string();
        }
        let real_path = self.real(path);
        if !real_path.is_dir() {
            return format!("Error: dir not found: {path}");
        }

        let mut lines = vec![path.to_string()];
        lines.extend(self.generate_tree_lines(&real_path, levels, 1, exclude_paths));
        let stdout = lines.join("\n");
        let remapped = self.remap(&stdout);
        if truncate {
            self.truncate_text(&remapped, legacy, false)
        } else {
            remapped
        }
    }

    fn generate_tree_lines(
        &self,
        dir_path: &Path,
        max_depth: Option<usize>,
        current_depth: usize,
        exclude_patterns: Option<&[String]>,
    ) -> Vec<String> {
        if max_depth.is_some_and(|depth| current_depth > depth) {
            return Vec::new();
        }

        let Ok(entries) = fs::read_dir(dir_path) else {
            return Vec::new();
        };
        let mut items = entries
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        items.sort_by(|a, b| compare_dirs_first_case_insensitive(a, b));

        let filtered = items
            .into_iter()
            .filter(|item| {
                let Some(name) = item.file_name().and_then(OsStr::to_str) else {
                    return false;
                };
                let is_dir = item.is_dir();
                if !self.is_visible_path(item, is_dir) {
                    return false;
                }
                if let Some(patterns) = exclude_patterns {
                    let rel = item
                        .strip_prefix(&self.root)
                        .unwrap_or(item)
                        .to_string_lossy()
                        .replace('\\', "/");
                    !matches_any_pattern(patterns, name, &rel)
                } else {
                    true
                }
            })
            .collect::<Vec<_>>();

        let mut lines = Vec::new();
        let count = filtered.len();
        for (index, item) in filtered.iter().enumerate() {
            let is_last = index == count - 1;
            let prefix = if is_last { "└── " } else { "├── " };
            let name = item
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_string();
            lines.push(format!("{prefix}{name}"));

            if item.is_dir() {
                let sub_lines =
                    self.generate_tree_lines(item, max_depth, current_depth + 1, exclude_patterns);
                let indent = if is_last { "    " } else { "│   " };
                for sub_line in sub_lines {
                    lines.push(format!("{indent}{sub_line}"));
                }
            }
        }

        lines
    }

    pub fn ls(&self, path: &str, long_format: bool, all_files: bool) -> String {
        self.ls_with_mode(path, long_format, all_files, true)
    }

    fn ls_with_mode(&self, path: &str, long_format: bool, all_files: bool, legacy: bool) -> String {
        if path.is_empty() {
            return "Error: missing or invalid path".to_string();
        }
        let real_path = self.real(path);
        if !real_path.is_dir() {
            return format!("Error: dir not found: {path}");
        }

        let mut entries = match fs::read_dir(real_path) {
            Ok(entries) => entries
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect::<Vec<_>>(),
            Err(err) => return format!("Error: {err}"),
        };
        entries.sort();

        if !all_files {
            entries.retain(|entry| !entry.starts_with('.'));
        }

        if !long_format {
            return self.truncate_text(&entries.join("\n"), legacy, false);
        }

        let mut lines = vec![format!("total {}", entries.len())];
        for name in entries {
            let fp = self.real(&format!("{path}/{name}"));
            let type_char = if fp.is_dir() { "d" } else { "-" };
            let size = fp.metadata().map(|meta| meta.len()).unwrap_or(0);
            lines.push(format!(
                "{type_char}rwxr-xr-x  1 user  staff {size:>8} Jan 01 00:00 {name}"
            ));
        }
        self.truncate_text(&self.remap(&lines.join("\n")), legacy, false)
    }

    pub fn glob(&self, pattern: &str, path: &str, type_filter: &str) -> String {
        if pattern.is_empty() {
            return "Error: missing or invalid pattern".to_string();
        }
        if path.is_empty() {
            return "Error: missing or invalid path".to_string();
        }

        let real_path = self.real(path);
        if !real_path.is_dir() {
            return format!("Error: dir not found: {path}");
        }

        let mut matches = Vec::new();
        if pattern.contains("**") {
            let clean_pattern = pattern.strip_prefix("**/").unwrap_or(pattern);
            for entry in WalkDir::new(&real_path)
                .into_iter()
                .filter_entry(|entry| {
                    entry.depth() == 0
                        || self.is_visible_path(entry.path(), entry.file_type().is_dir())
                })
                .flatten()
            {
                let item = entry.path();
                if !matches_type(item, type_filter) {
                    continue;
                }
                let rel = item
                    .strip_prefix(&real_path)
                    .unwrap_or(item)
                    .to_string_lossy()
                    .replace('\\', "/");
                let name = item.file_name().and_then(OsStr::to_str).unwrap_or_default();
                if pattern_matches(pattern, &rel)
                    || pattern_matches(clean_pattern, &rel)
                    || pattern_matches(clean_pattern, name)
                {
                    matches.push(item.to_path_buf());
                }
            }
        } else if let Ok(entries) = fs::read_dir(&real_path) {
            for entry in entries.flatten() {
                let item = entry.path();
                if !matches_type(&item, type_filter) {
                    continue;
                }
                if !self.is_visible_path(&item, item.is_dir()) {
                    continue;
                }
                let name = item.file_name().and_then(OsStr::to_str).unwrap_or_default();
                if pattern_matches(pattern, name) {
                    matches.push(item);
                }
            }
        }

        matches.sort();
        matches.truncate(100);
        let out = matches
            .iter()
            .map(|path| self.remap(&path.to_string_lossy()))
            .collect::<Vec<_>>()
            .join("\n");
        if out.is_empty() {
            "(no matches)".to_string()
        } else {
            out
        }
    }

    pub fn exec_command(&self, cmd: &Value) -> String {
        let Some(cmd) = cmd.as_object() else {
            return "Error: missing or invalid command".to_string();
        };
        let command_type = cmd.get("type").and_then(Value::as_str).unwrap_or_default();
        match command_type {
            "rg" => self.rg_with_truncation(
                cmd.get("pattern")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                cmd.get("path").and_then(Value::as_str).unwrap_or_default(),
                string_array(cmd.get("include")).as_deref(),
                string_array(cmd.get("exclude")).as_deref(),
                false,
            ),
            "readfile" => self.readfile_with_mode(
                cmd.get("file").and_then(Value::as_str).unwrap_or_default(),
                cmd.get("start_line")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize),
                cmd.get("end_line")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize),
                false,
            ),
            "tree" => self.tree_with_mode(
                cmd.get("path").and_then(Value::as_str).unwrap_or_default(),
                cmd.get("levels")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize),
                None,
                true,
                false,
            ),
            "ls" => self.ls_with_mode(
                cmd.get("path").and_then(Value::as_str).unwrap_or_default(),
                cmd.get("long_format")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                cmd.get("all").and_then(Value::as_bool).unwrap_or(false),
                false,
            ),
            "glob" => self.truncate_text(
                &self.glob(
                    cmd.get("pattern")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    cmd.get("path").and_then(Value::as_str).unwrap_or_default(),
                    cmd.get("type_filter")
                        .and_then(Value::as_str)
                        .unwrap_or("file"),
                ),
                false,
                false,
            ),
            _ => format!("Error: unknown command type '{command_type}'"),
        }
    }

    pub fn exec_restricted_exec_step(
        &self,
        step_id: &str,
        tool_calls: &[InstantContextToolCall],
        timeout_budget_ms: u128,
    ) -> Vec<InstantContextToolUpdate> {
        if tool_calls.is_empty() {
            return Vec::new();
        }

        let pending = tool_calls
            .iter()
            .map(|call| InstantContextToolUpdate {
                step_id: step_id.to_string(),
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                command: call.args.clone(),
                status: ToolExecutionStatus::Pending,
                output: String::new(),
                timing: InstantContextTiming::default(),
            })
            .collect::<Vec<_>>();

        let timeout = Duration::from_millis(timeout_budget_ms.min(u64::MAX as u128) as u64);
        let receivers = tool_calls
            .iter()
            .map(|call| {
                let executor = self.clone();
                let call_name = call.name.clone();
                let mut cmd = call.args.clone();
                if let Some(map) = cmd.as_object_mut() {
                    map.entry("type".to_string())
                        .or_insert_with(|| Value::String(call_name));
                }
                let (tx, rx) = mpsc::channel();
                thread::spawn(move || {
                    let started = Instant::now();
                    let output = executor.exec_command(&cmd);
                    let duration_ms = started.elapsed().as_millis();
                    let status = if output.trim_start().starts_with("Error:") {
                        ToolExecutionStatus::Error
                    } else {
                        ToolExecutionStatus::Completed
                    };
                    let _ = tx.send((cmd, status, output, duration_ms));
                });
                rx
            })
            .collect::<Vec<_>>();

        let outputs = receivers
            .into_iter()
            .map(|rx| match rx.recv_timeout(timeout) {
                Ok(result) => result,
                Err(mpsc::RecvTimeoutError::Timeout) => (
                    Value::Null,
                    ToolExecutionStatus::TimedOut,
                    "Error: tool timed out".to_string(),
                    timeout_budget_ms,
                ),
                Err(mpsc::RecvTimeoutError::Disconnected) => (
                    Value::Null,
                    ToolExecutionStatus::Error,
                    "Error: command panicked".to_string(),
                    0,
                ),
            })
            .collect::<Vec<_>>();

        let completed = tool_calls
            .iter()
            .zip(outputs)
            .map(
                |(call, (command, status, output, duration_ms))| InstantContextToolUpdate {
                    step_id: step_id.to_string(),
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    command,
                    status,
                    output,
                    timing: InstantContextTiming { duration_ms },
                },
            )
            .collect::<Vec<_>>();

        pending.into_iter().chain(completed).collect()
    }
}

fn string_array(value: Option<&Value>) -> Option<Vec<String>> {
    value.and_then(Value::as_array).map(|values| {
        values
            .iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    })
}
