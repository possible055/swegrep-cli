use super::helpers::{
    compare_dirs_first_case_insensitive, matches_any_pattern, matches_type, pattern_matches,
};
use super::{
    InstantContextTiming, InstantContextToolCall, InstantContextToolUpdate, ToolExecutionStatus,
    ToolExecutor, TruncationProfile,
};
use crate::rg::{resolve_rg_path, ripgrep_not_found_message};
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
    fn execute_rg(
        &self,
        pattern: &str,
        path: &str,
        include: Option<&[String]>,
        exclude: Option<&[String]>,
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
            return self.rg_native(pattern, &real_path, include, exclude);
        }

        self.rg_filtered(pattern, &real_path, include, exclude)
    }

    fn rg_filtered(
        &self,
        pattern: &str,
        real_path: &Path,
        include: Option<&[String]>,
        exclude: Option<&[String]>,
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

            let rg_path = match resolve_rg_path() {
                Ok(path) => path,
                Err(error) => return error.to_string(),
            };

            match Command::new(rg_path).args(args).output() {
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
                            TruncationProfile::General,
                        );
                    }
                    return format!("Error: exit status {code}");
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    return ripgrep_not_found_message().to_string();
                }
                Err(err) => return format!("Error: {err}"),
            }
        }

        if all_stdout.is_empty() {
            "(no matches)".to_string()
        } else {
            self.truncate_text(&self.remap(&all_stdout), TruncationProfile::General)
        }
    }

    fn rg_native(
        &self,
        pattern: &str,
        real_path: &Path,
        include: Option<&[String]>,
        exclude: Option<&[String]>,
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

        let rg_path = match resolve_rg_path() {
            Ok(path) => path,
            Err(error) => return error.to_string(),
        };

        match Command::new(rg_path).args(args).output() {
            Ok(output) => {
                let code = output.status.code().unwrap_or(-1);
                if code == 1 {
                    return "(no matches)".to_string();
                }
                if code == 0 {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    return self.truncate_text(&self.remap(&stdout), TruncationProfile::General);
                }
                if !output.stderr.is_empty() {
                    return self.truncate_text(
                        &self.remap(&String::from_utf8_lossy(&output.stderr)),
                        TruncationProfile::General,
                    );
                }
                format!("Error: exit status {code}")
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                ripgrep_not_found_message().to_string()
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

    fn execute_readfile(
        &self,
        file: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> String {
        if file.is_empty() {
            return "Error: missing or invalid file path".to_string();
        }
        let real_path = self.real(file);
        if !real_path.is_file() {
            return format!("Error: file not found: {file}");
        }
        if !self.is_visible_path(&real_path, false) {
            return format!("Error: file not visible: {file}");
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
        self.truncate_text(&out, TruncationProfile::ReadfileExpanded)
    }

    fn execute_tree(&self, path: &str, levels: Option<usize>) -> String {
        if path.is_empty() {
            return "Error: missing or invalid path".to_string();
        }
        let real_path = self.real(path);
        if !real_path.is_dir() {
            return format!("Error: dir not found: {path}");
        }

        let mut lines = vec![path.to_string()];
        lines.extend(self.generate_tree_lines(&real_path, levels, 1));
        let stdout = lines.join("\n");
        let remapped = self.remap(&stdout);
        self.truncate_text(&remapped, TruncationProfile::General)
    }

    fn generate_tree_lines(
        &self,
        dir_path: &Path,
        max_depth: Option<usize>,
        current_depth: usize,
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
                let Some(_name) = item.file_name().and_then(OsStr::to_str) else {
                    return false;
                };
                let is_dir = item.is_dir();
                if !self.is_visible_path(item, is_dir) {
                    return false;
                }
                true
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
                let sub_lines = self.generate_tree_lines(item, max_depth, current_depth + 1);
                let indent = if is_last { "    " } else { "│   " };
                for sub_line in sub_lines {
                    lines.push(format!("{indent}{sub_line}"));
                }
            }
        }

        lines
    }

    fn execute_ls(&self, path: &str, long_format: bool, all_files: bool) -> String {
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
                .filter_map(|entry| {
                    let name = entry.file_name().into_string().ok()?;
                    let path = entry.path();
                    let is_dir = entry.file_type().map(|file_type| file_type.is_dir()).ok()?;
                    if !self.is_visible_path(&path, is_dir) {
                        return None;
                    }
                    Some((name, path))
                })
                .collect::<Vec<_>>(),
            Err(err) => return format!("Error: {err}"),
        };
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));

        if !all_files {
            entries.retain(|(name, _)| !name.starts_with('.'));
        }

        if !long_format {
            let names = entries
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            return self.truncate_text(&names, TruncationProfile::General);
        }

        let mut lines = vec![format!("total {}", entries.len())];
        for (name, item_path) in entries {
            let type_char = if item_path.is_dir() { "d" } else { "-" };
            let size = item_path.metadata().map(|meta| meta.len()).unwrap_or(0);
            lines.push(format!(
                "{type_char}rwxr-xr-x  1 user  staff {size:>8} Jan 01 00:00 {name}"
            ));
        }
        self.truncate_text(&self.remap(&lines.join("\n")), TruncationProfile::General)
    }

    fn execute_glob(&self, pattern: &str, path: &str, type_filter: &str) -> String {
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
            self.truncate_text(&out, TruncationProfile::General)
        }
    }

    pub fn exec_command(&self, cmd: &Value) -> String {
        let Some(cmd) = cmd.as_object() else {
            return "Error: missing or invalid command".to_string();
        };
        let command_type = cmd.get("type").and_then(Value::as_str).unwrap_or_default();
        match command_type {
            "rg" => self.execute_rg(
                cmd.get("pattern")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                cmd.get("path").and_then(Value::as_str).unwrap_or_default(),
                string_array(cmd.get("include")).as_deref(),
                string_array(cmd.get("exclude")).as_deref(),
            ),
            "readfile" => self.execute_readfile(
                cmd.get("file").and_then(Value::as_str).unwrap_or_default(),
                cmd.get("start_line")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize),
                cmd.get("end_line")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize),
            ),
            "tree" => self.execute_tree(
                cmd.get("path").and_then(Value::as_str).unwrap_or_default(),
                cmd.get("levels")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize),
            ),
            "ls" => self.execute_ls(
                cmd.get("path").and_then(Value::as_str).unwrap_or_default(),
                cmd.get("long_format")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                cmd.get("all").and_then(Value::as_bool).unwrap_or(false),
            ),
            "glob" => self.execute_glob(
                cmd.get("pattern")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                cmd.get("path").and_then(Value::as_str).unwrap_or_default(),
                cmd.get("type_filter")
                    .and_then(Value::as_str)
                    .unwrap_or("file"),
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

        if timeout_budget_ms == 0 {
            let completed = tool_calls
                .iter()
                .map(|call| InstantContextToolUpdate {
                    step_id: step_id.to_string(),
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    command: Value::Null,
                    status: ToolExecutionStatus::TimedOut,
                    output: "Error: tool timed out".to_string(),
                    timing: InstantContextTiming { duration_ms: 0 },
                })
                .collect::<Vec<_>>();

            return pending.into_iter().chain(completed).collect();
        }

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

#[cfg(test)]
mod tests {
    use crate::executor::{InstantContextToolCall, ToolExecutionStatus, ToolExecutor};
    use crate::path_filter::PathFilterConfig;
    use crate::rg::resolve_rg_path;
    use serde_json::json;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn rg_available() -> bool {
        let Ok(path) = resolve_rg_path() else {
            return false;
        };
        Command::new(path).arg("--version").output().is_ok()
    }

    #[test]
    fn exec_command_readfile_supports_full_file_and_ranges() {
        let tmp = TempDir::new().unwrap();
        let executor = ToolExecutor::new(tmp.path());
        fs::write(
            tmp.path().join("test.txt"),
            "line1\nline2\nline3\nline4\nline5",
        )
        .unwrap();

        let res = executor.exec_command(&json!({
            "type": "readfile",
            "file": "/codebase/test.txt"
        }));
        assert!(res.contains("1:line1\n2:line2\n3:line3\n4:line4\n5:line5"));

        let res = executor.exec_command(&json!({
            "type": "readfile",
            "file": "/codebase/test.txt",
            "start_line": 2,
            "end_line": 4
        }));
        assert!(res.contains("2:line2\n3:line3\n4:line4"));
        assert!(!res.contains("1:line1"));

        assert!(
            executor
                .exec_command(&json!({"type": "readfile", "file": "/codebase/nonexistent.txt"}))
                .contains("Error: file not found")
        );
    }

    #[test]
    fn exec_command_tree_respects_depth_and_dotfile_filter() {
        let tmp = TempDir::new().unwrap();
        let executor = ToolExecutor::new(tmp.path());
        fs::create_dir(tmp.path().join("dir1")).unwrap();
        fs::write(tmp.path().join("dir1").join("file1.py"), "").unwrap();
        fs::create_dir_all(tmp.path().join("dir2").join("sub")).unwrap();
        fs::write(tmp.path().join("dir2").join("sub").join("file2.py"), "").unwrap();
        fs::write(tmp.path().join("file3.txt"), "").unwrap();
        fs::create_dir(tmp.path().join(".cache")).unwrap();

        let res = executor.exec_command(&json!({
            "type": "tree",
            "path": "/codebase",
            "levels": 2
        }));
        assert!(res.contains("dir1"));
        assert!(res.contains("file1.py"));
        assert!(res.contains("dir2"));
        assert!(res.contains("sub"));
        assert!(res.contains("file3.txt"));
        assert!(!res.contains("file2.py"));
        assert!(!res.contains(".cache"));
    }

    #[test]
    fn exec_command_ls_supports_short_and_long_formats() {
        let tmp = TempDir::new().unwrap();
        let executor = ToolExecutor::new(tmp.path());
        fs::write(tmp.path().join("file1.txt"), "").unwrap();
        fs::create_dir(tmp.path().join("dir1")).unwrap();

        let res = executor.exec_command(&json!({"type": "ls", "path": "/codebase"}));
        assert!(res.contains("dir1\nfile1.txt"));

        let res = executor.exec_command(&json!({
            "type": "ls",
            "path": "/codebase",
            "long_format": true
        }));
        assert!(res.contains("total 2"));
        assert!(res.contains("dir1"));
        assert!(res.contains("file1.txt"));
    }

    #[test]
    fn exec_command_glob_supports_recursive_patterns() {
        let tmp = TempDir::new().unwrap();
        let executor = ToolExecutor::new(tmp.path());
        fs::create_dir(tmp.path().join("dir1")).unwrap();
        fs::write(tmp.path().join("dir1").join("test1.py"), "").unwrap();
        fs::create_dir(tmp.path().join("dir2")).unwrap();
        fs::write(tmp.path().join("dir2").join("test2.py"), "").unwrap();
        fs::write(tmp.path().join("other.txt"), "").unwrap();

        let res = executor.exec_command(&json!({
            "type": "glob",
            "pattern": "**/test*.py",
            "path": "/codebase",
            "type_filter": "all"
        }));
        assert!(res.contains("/codebase/dir1/test1.py"));
        assert!(res.contains("/codebase/dir2/test2.py"));
        assert!(!res.contains("other.txt"));
    }

    #[test]
    fn path_filter_applies_to_all_local_tools() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "ignored/\n*.log\n").unwrap();
        fs::create_dir(tmp.path().join("ignored")).unwrap();
        fs::write(tmp.path().join("ignored").join("keep.txt"), "needle").unwrap();
        fs::write(tmp.path().join("ignored").join("skip.txt"), "needle").unwrap();
        fs::create_dir(tmp.path().join("logs")).unwrap();
        fs::write(tmp.path().join("logs").join("keep.log"), "needle").unwrap();
        fs::write(tmp.path().join("logs").join("drop.log"), "needle").unwrap();
        fs::create_dir(tmp.path().join(".cache")).unwrap();
        fs::write(tmp.path().join(".cache").join("visible.txt"), "needle").unwrap();
        fs::create_dir(tmp.path().join(".secret")).unwrap();
        fs::write(tmp.path().join(".secret").join("hidden.txt"), "needle").unwrap();
        fs::write(tmp.path().join("visible.txt"), "needle").unwrap();

        let config = PathFilterConfig {
            include_patterns: vec![
                "ignored/keep.txt".to_string(),
                "logs/keep.log".to_string(),
                ".cache/visible.txt".to_string(),
            ],
            exclude_patterns: vec!["logs/drop.log".to_string()],
            ..PathFilterConfig::default()
        };
        let executor = ToolExecutor::with_limits_and_filter(tmp.path(), None, None, config);

        let tree = executor.exec_command(&json!({
            "type": "tree",
            "path": "/codebase",
            "levels": 3
        }));
        assert!(tree.contains("visible.txt"));
        assert!(tree.contains("keep.txt"));
        assert!(tree.contains("keep.log"));
        assert!(!tree.contains("skip.txt"));
        assert!(!tree.contains("drop.log"));

        let glob = executor.exec_command(&json!({
            "type": "glob",
            "pattern": "**/*",
            "path": "/codebase",
            "type_filter": "file"
        }));
        assert!(glob.contains("/codebase/visible.txt"));
        assert!(glob.contains("/codebase/ignored/keep.txt"));
        assert!(glob.contains("/codebase/logs/keep.log"));
        assert!(glob.contains("/codebase/.cache/visible.txt"));
        assert!(!glob.contains("/codebase/ignored/skip.txt"));
        assert!(!glob.contains("/codebase/logs/drop.log"));

        let included_readfile = executor.exec_command(&json!({
            "type": "readfile",
            "file": "/codebase/ignored/keep.txt"
        }));
        assert!(included_readfile.contains("1:needle"));

        assert_eq!(
            executor.exec_command(&json!({
                "type": "readfile",
                "file": "/codebase/ignored/skip.txt"
            })),
            "Error: file not visible: /codebase/ignored/skip.txt"
        );
        assert_eq!(
            executor.exec_command(&json!({
                "type": "readfile",
                "file": "/codebase/logs/drop.log"
            })),
            "Error: file not visible: /codebase/logs/drop.log"
        );
        assert_eq!(
            executor.exec_command(&json!({
                "type": "readfile",
                "file": "/codebase/.secret/hidden.txt"
            })),
            "Error: file not visible: /codebase/.secret/hidden.txt"
        );

        let logs = executor.exec_command(&json!({
            "type": "ls",
            "path": "/codebase/logs"
        }));
        assert!(logs.contains("keep.log"));
        assert!(!logs.contains("drop.log"));

        let root = executor.exec_command(&json!({
            "type": "ls",
            "path": "/codebase"
        }));
        assert!(!root.contains(".cache"));
        assert!(!root.contains(".secret"));

        let root_all = executor.exec_command(&json!({
            "type": "ls",
            "path": "/codebase",
            "all": true
        }));
        assert!(root_all.contains(".cache"));
        assert!(!root_all.contains(".secret"));

        if rg_available() {
            let result = executor.exec_command(&json!({
                "type": "rg",
                "pattern": "needle",
                "path": "/codebase"
            }));
            assert!(result.contains("/codebase/visible.txt"));
            assert!(result.contains("/codebase/ignored/keep.txt"));
            assert!(result.contains("/codebase/logs/keep.log"));
            assert!(result.contains("/codebase/.cache/visible.txt"));
            assert!(!result.contains("/codebase/ignored/skip.txt"));
            assert!(!result.contains("/codebase/logs/drop.log"));
        }
    }

    #[test]
    fn exec_command_rg_includes_filename_for_single_file_results() {
        if !rg_available() {
            return;
        }

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("only.txt"), "needle").unwrap();
        let executor = ToolExecutor::new(tmp.path());

        let result = executor.exec_command(&json!({
            "type": "rg",
            "pattern": "needle",
            "path": "/codebase"
        }));

        assert!(result.contains("/codebase/only.txt:1:needle"));
    }

    #[test]
    fn disabled_path_filter_uses_native_rg_traversal() {
        if !rg_available() {
            return;
        }

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("visible.txt"), "needle").unwrap();
        let config = PathFilterConfig {
            exclude_patterns: vec!["visible.txt".to_string()],
            ..PathFilterConfig::default()
        };
        let disabled_config = PathFilterConfig {
            enabled: false,
            exclude_patterns: vec!["visible.txt".to_string()],
            ..PathFilterConfig::default()
        };

        let filtered = ToolExecutor::with_limits_and_filter(tmp.path(), None, None, config);
        let native = ToolExecutor::with_limits_and_filter(tmp.path(), None, None, disabled_config);

        assert_eq!(
            filtered.exec_command(&json!({"type": "rg", "pattern": "needle", "path": "/codebase"})),
            "(no matches)"
        );
        assert!(
            native
                .exec_command(&json!({"type": "rg", "pattern": "needle", "path": "/codebase"}))
                .contains("/codebase/visible.txt:1:needle")
        );
    }

    #[test]
    fn disabled_path_filter_allows_readfile_and_ls_entries() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("visible.txt"), "needle").unwrap();
        let config = PathFilterConfig {
            enabled: false,
            exclude_patterns: vec!["visible.txt".to_string()],
            ..PathFilterConfig::default()
        };
        let executor = ToolExecutor::with_limits_and_filter(tmp.path(), None, None, config);

        assert!(
            executor
                .exec_command(&json!({"type": "readfile", "file": "/codebase/visible.txt"}))
                .contains("1:needle")
        );
        assert!(
            executor
                .exec_command(&json!({"type": "ls", "path": "/codebase"}))
                .contains("visible.txt")
        );
    }

    #[test]
    fn exec_command_supports_all_subcommands() {
        let tmp = TempDir::new().unwrap();
        let executor = ToolExecutor::new(tmp.path());
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("test.txt"), "hello world").unwrap();
        fs::write(tmp.path().join("src").join("main.rs"), "fn main() {}").unwrap();

        assert!(
            executor
                .exec_command(&json!({"type": "readfile", "file": "/codebase/test.txt"}))
                .contains("1:hello world")
        );
        if rg_available() {
            assert!(
                executor
                    .exec_command(&json!({"type": "rg", "pattern": "hello", "path": "/codebase"}))
                    .contains("/codebase/test.txt:1:hello world")
            );
        }
        assert!(
            executor
                .exec_command(&json!({"type": "tree", "path": "/codebase", "levels": 1}))
                .contains("src")
        );
        assert!(
            executor
                .exec_command(&json!({"type": "ls", "path": "/codebase"}))
                .contains("test.txt")
        );
        assert!(
            executor
                .exec_command(&json!({"type": "glob", "pattern": "**/*.rs", "path": "/codebase", "type_filter": "file"}))
                .contains("src/main.rs")
        );
    }

    #[test]
    fn exec_restricted_exec_step_returns_pending_and_terminal_updates() {
        let tmp = TempDir::new().unwrap();
        let executor = ToolExecutor::new(tmp.path());
        fs::write(tmp.path().join("test.txt"), "hello world").unwrap();

        let calls = vec![
            InstantContextToolCall {
                id: "call-1".to_string(),
                name: "command1".to_string(),
                args: json!({"type": "readfile", "file": "/codebase/test.txt"}),
            },
            InstantContextToolCall {
                id: "call-2".to_string(),
                name: "command2".to_string(),
                args: json!({"type": "unknown"}),
            },
        ];

        let updates = executor.exec_restricted_exec_step("step-1", &calls, 1_000);

        assert_eq!(updates.len(), 4);
        assert_eq!(updates[0].step_id, "step-1");
        assert_eq!(updates[0].tool_call_id, "call-1");
        assert_eq!(updates[0].status, ToolExecutionStatus::Pending);
        assert_eq!(updates[1].tool_call_id, "call-2");
        assert_eq!(updates[1].status, ToolExecutionStatus::Pending);
        assert_eq!(updates[2].tool_call_id, "call-1");
        assert_eq!(updates[2].status, ToolExecutionStatus::Completed);
        assert!(updates[2].output.contains("1:hello world"));
        assert_eq!(updates[3].tool_call_id, "call-2");
        assert_eq!(updates[3].status, ToolExecutionStatus::Error);
    }

    #[test]
    fn exec_restricted_exec_step_marks_timed_out_when_budget_is_exceeded() {
        let tmp = TempDir::new().unwrap();
        let executor = ToolExecutor::new(tmp.path());
        fs::write(tmp.path().join("test.txt"), "hello world").unwrap();

        let calls = vec![InstantContextToolCall {
            id: "call-1".to_string(),
            name: "command1".to_string(),
            args: json!({"type": "readfile", "file": "/codebase/test.txt"}),
        }];

        let updates = executor.exec_restricted_exec_step("step-1", &calls, 0);

        assert_eq!(updates[1].status, ToolExecutionStatus::TimedOut);
    }
}
