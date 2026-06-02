use globset::Glob;
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use walkdir::WalkDir;

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

    pub fn rg(
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

        let mut args = vec![
            "--no-heading".to_string(),
            "-n".to_string(),
            "--max-count".to_string(),
            "50".to_string(),
            pattern.to_string(),
            real_path.to_string_lossy().into_owned(),
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

        match Command::new("rg")
            .args(args)
            .env("RIPGREP_CONFIG_PATH", "")
            .output()
        {
            Ok(output) => {
                let code = output.status.code().unwrap_or(-1);
                if code == 1 {
                    return "(no matches)".to_string();
                }
                if code == 0 {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let stdout = if stdout.is_empty() {
                        "(no matches)"
                    } else {
                        &stdout
                    };
                    return self.truncate(&self.remap(stdout));
                }
                if !output.stderr.is_empty() {
                    return self.truncate(&self.remap(&String::from_utf8_lossy(&output.stderr)));
                }
                format!("Error: exit status {code}")
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                "Error: ripgrep ('rg') is not installed or not in PATH.".to_string()
            }
            Err(err) => format!("Error: {err}"),
        }
    }

    pub fn readfile(
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
        self.truncate(&out)
    }

    pub fn tree(
        &self,
        path: &str,
        levels: Option<usize>,
        exclude_paths: Option<&[String]>,
        truncate: bool,
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
            self.truncate(&remapped)
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
                if name.starts_with('.') {
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
            return self.truncate(&entries.join("\n"));
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
        self.truncate(&self.remap(&lines.join("\n")))
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
            for entry in WalkDir::new(&real_path).into_iter().flatten() {
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
            "rg" => self.rg(
                cmd.get("pattern")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                cmd.get("path").and_then(Value::as_str).unwrap_or_default(),
                string_array(cmd.get("include")).as_deref(),
                string_array(cmd.get("exclude")).as_deref(),
            ),
            "readfile" => self.readfile(
                cmd.get("file").and_then(Value::as_str).unwrap_or_default(),
                cmd.get("start_line")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize),
                cmd.get("end_line")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize),
            ),
            "tree" => self.tree(
                cmd.get("path").and_then(Value::as_str).unwrap_or_default(),
                cmd.get("levels")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize),
                None,
                true,
            ),
            "ls" => self.ls(
                cmd.get("path").and_then(Value::as_str).unwrap_or_default(),
                cmd.get("long_format")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                cmd.get("all").and_then(Value::as_bool).unwrap_or(false),
            ),
            "glob" => self.glob(
                cmd.get("pattern")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                cmd.get("path").and_then(Value::as_str).unwrap_or_default(),
                cmd.get("type_filter")
                    .and_then(Value::as_str)
                    .unwrap_or("all"),
            ),
            _ => format!("Error: unknown command type '{command_type}'"),
        }
    }

    pub fn exec_tool_call(&self, args: &Value) -> String {
        let Some(args) = args.as_object() else {
            return "Error: missing or invalid tool args".to_string();
        };

        let mut keys = args
            .keys()
            .filter(|key| key.starts_with("command"))
            .cloned()
            .collect::<Vec<_>>();
        keys.sort_by_key(|key| command_number(key));
        if keys.is_empty() {
            return String::new();
        }

        let outputs = thread::scope(|scope| {
            let handles = keys
                .iter()
                .map(|key| {
                    let cmd = args.get(key).cloned().unwrap_or(Value::Null);
                    scope.spawn(move || self.exec_command(&cmd))
                })
                .collect::<Vec<_>>();

            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .unwrap_or_else(|_| "Error: command panicked".to_string())
                })
                .collect::<Vec<_>>()
        });

        keys.iter()
            .zip(outputs)
            .map(|(key, output)| format!("<{key}_result>\n{output}\n</{key}_result>"))
            .collect::<Vec<_>>()
            .join("")
    }
}

fn bounded_int(value: Option<usize>, default: usize, min: usize, max: usize) -> usize {
    value.unwrap_or(default).clamp(min, max)
}

fn read_int_env(name: &str, default: usize, min: usize, max: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .map(|value| value.clamp(min, max))
        .unwrap_or(default)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn compare_dirs_first_case_insensitive(a: &Path, b: &Path) -> Ordering {
    let a_is_file = !a.is_dir();
    let b_is_file = !b.is_dir();
    a_is_file.cmp(&b_is_file).then_with(|| {
        let a_name = a
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_lowercase();
        let b_name = b
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_lowercase();
        a_name.cmp(&b_name)
    })
}

fn matches_any_pattern(patterns: &[String], name: &str, rel: &str) -> bool {
    patterns.iter().any(|pattern| {
        pattern_matches(&pattern.replace('\\', "/"), name) || pattern_matches(pattern, rel)
    })
}

fn pattern_matches(pattern: &str, text: &str) -> bool {
    Glob::new(pattern)
        .map(|glob| glob.compile_matcher().is_match(text))
        .unwrap_or(false)
}

fn matches_type(path: &Path, type_filter: &str) -> bool {
    match type_filter {
        "file" => path.is_file(),
        "directory" => path.is_dir(),
        _ => true,
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

pub fn command_number(key: &str) -> usize {
    key.strip_prefix("command")
        .and_then(|suffix| suffix.parse().ok())
        .unwrap_or(9999)
}

pub fn command_keys(args: &Value) -> Vec<String> {
    let Some(map) = args.as_object() else {
        return Vec::new();
    };
    let mut keys = map
        .keys()
        .filter(|key| key.starts_with("command"))
        .cloned()
        .collect::<Vec<_>>();
    keys.sort_by_key(|key| command_number(key));
    keys
}

pub fn valid_command_count(args: &Value) -> usize {
    let Some(map) = args.as_object() else {
        return 0;
    };
    command_keys(args)
        .into_iter()
        .filter(|key| {
            map.get(key)
                .and_then(Value::as_object)
                .and_then(|cmd| cmd.get("type"))
                .is_some()
        })
        .count()
}

pub fn object_from_hashmap(map: HashMap<String, Value>) -> Value {
    Value::Object(map.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    #[test]
    fn executor_paths_are_clamped_to_root() {
        let tmp = TempDir::new().unwrap();
        let executor = ToolExecutor::new(tmp.path());

        assert_eq!(
            executor.real("/codebase"),
            tmp.path().canonicalize().unwrap()
        );
        assert_eq!(
            executor.real("/codebase/sub/file.py"),
            tmp.path().join("sub").join("file.py")
        );
        assert_eq!(
            executor.real("/codebase/../../etc/passwd"),
            tmp.path().canonicalize().unwrap()
        );
        assert_eq!(
            executor.real("/codebase/sub/../../../etc/passwd"),
            tmp.path().canonicalize().unwrap()
        );
        assert_eq!(
            executor.real("/etc/passwd"),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn readfile_supports_full_file_and_ranges() {
        let tmp = TempDir::new().unwrap();
        let executor = ToolExecutor::new(tmp.path());
        fs::write(
            tmp.path().join("test.txt"),
            "line1\nline2\nline3\nline4\nline5",
        )
        .unwrap();

        let res = executor.readfile("/codebase/test.txt", None, None);
        assert!(res.contains("1:line1\n2:line2\n3:line3\n4:line4\n5:line5"));

        let res = executor.readfile("/codebase/test.txt", Some(2), Some(4));
        assert!(res.contains("2:line2\n3:line3\n4:line4"));
        assert!(!res.contains("1:line1"));

        assert!(
            executor
                .readfile("/codebase/nonexistent.txt", None, None)
                .contains("Error: file not found")
        );
    }

    #[test]
    fn tree_respects_depth_and_dotfile_filter() {
        let tmp = TempDir::new().unwrap();
        let executor = ToolExecutor::new(tmp.path());
        fs::create_dir(tmp.path().join("dir1")).unwrap();
        fs::write(tmp.path().join("dir1").join("file1.py"), "").unwrap();
        fs::create_dir_all(tmp.path().join("dir2").join("sub")).unwrap();
        fs::write(tmp.path().join("dir2").join("sub").join("file2.py"), "").unwrap();
        fs::write(tmp.path().join("file3.txt"), "").unwrap();
        fs::create_dir(tmp.path().join(".cache")).unwrap();

        let res = executor.tree("/codebase", Some(2), Some(&["dist".to_string()]), true);
        assert!(res.contains("dir1"));
        assert!(res.contains("file1.py"));
        assert!(res.contains("dir2"));
        assert!(res.contains("sub"));
        assert!(res.contains("file3.txt"));
        assert!(!res.contains("file2.py"));
        assert!(!res.contains(".cache"));
    }

    #[test]
    fn ls_supports_short_and_long_formats() {
        let tmp = TempDir::new().unwrap();
        let executor = ToolExecutor::new(tmp.path());
        fs::write(tmp.path().join("file1.txt"), "").unwrap();
        fs::create_dir(tmp.path().join("dir1")).unwrap();

        let res = executor.ls("/codebase", false, false);
        assert!(res.contains("dir1\nfile1.txt"));

        let res = executor.ls("/codebase", true, false);
        assert!(res.contains("total 2"));
        assert!(res.contains("dir1"));
        assert!(res.contains("file1.txt"));
    }

    #[test]
    fn glob_supports_recursive_patterns() {
        let tmp = TempDir::new().unwrap();
        let executor = ToolExecutor::new(tmp.path());
        fs::create_dir(tmp.path().join("dir1")).unwrap();
        fs::write(tmp.path().join("dir1").join("test1.py"), "").unwrap();
        fs::create_dir(tmp.path().join("dir2")).unwrap();
        fs::write(tmp.path().join("dir2").join("test2.py"), "").unwrap();
        fs::write(tmp.path().join("other.txt"), "").unwrap();

        let res = executor.glob("**/test*.py", "/codebase", "all");
        assert!(res.contains("/codebase/dir1/test1.py"));
        assert!(res.contains("/codebase/dir2/test2.py"));
        assert!(!res.contains("other.txt"));
    }

    #[test]
    fn exec_tool_call_formats_results() {
        let tmp = TempDir::new().unwrap();
        let executor = ToolExecutor::new(tmp.path());
        fs::write(tmp.path().join("test.txt"), "hello world").unwrap();

        let args = json!({"command1": {"type": "readfile", "file": "/codebase/test.txt"}});
        let res = executor.exec_tool_call(&args);
        assert!(res.contains("<command1_result>"));
        assert!(res.contains("1:hello world"));
        assert!(res.contains("</command1_result>"));
    }

    #[test]
    fn exec_tool_call_preserves_output_order_when_parallel() {
        struct SlowExecutor {
            inner: ToolExecutor,
        }

        impl SlowExecutor {
            fn exec_tool_call(&self, args: &Value) -> String {
                let keys = command_keys(args);
                let outputs = thread::scope(|scope| {
                    let handles = keys
                        .iter()
                        .map(|key| {
                            let value = args[key]["value"].as_i64().unwrap();
                            scope.spawn(move || {
                                thread::sleep(Duration::from_millis(50));
                                value.to_string()
                            })
                        })
                        .collect::<Vec<_>>();
                    handles
                        .into_iter()
                        .map(|handle| handle.join().unwrap())
                        .collect::<Vec<_>>()
                });
                keys.iter()
                    .zip(outputs)
                    .map(|(key, output)| format!("<{key}_result>\n{output}\n</{key}_result>"))
                    .collect::<Vec<_>>()
                    .join("")
            }
        }

        let tmp = TempDir::new().unwrap();
        let executor = SlowExecutor {
            inner: ToolExecutor::new(tmp.path()),
        };
        assert!(executor.inner.real("/codebase").exists());
        let args = json!({
            "command1": {"value": 1},
            "command2": {"value": 2},
            "command3": {"value": 3},
            "command4": {"value": 4}
        });

        let started = Instant::now();
        let res = executor.exec_tool_call(&args);
        assert!(started.elapsed() < Duration::from_millis(150));
        assert!(
            res.find("<command1_result>\n1\n").unwrap()
                < res.find("<command2_result>\n2\n").unwrap()
        );
        assert!(
            res.find("<command2_result>\n2\n").unwrap()
                < res.find("<command3_result>\n3\n").unwrap()
        );
        assert!(
            res.find("<command3_result>\n3\n").unwrap()
                < res.find("<command4_result>\n4\n").unwrap()
        );
    }
}
