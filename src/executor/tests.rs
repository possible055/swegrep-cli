use super::*;
use crate::path_filter::PathFilterConfig;
use serde_json::json;
use std::fs;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use tempfile::TempDir;

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn set_env_var(key: &str, value: &str) {
    // SAFETY: tests take a global env lock before mutating process environment.
    unsafe {
        std::env::set_var(key, value);
    }
}

fn remove_env_var(key: &str) {
    // SAFETY: tests take a global env lock before mutating process environment.
    unsafe {
        std::env::remove_var(key);
    }
}

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
fn path_filter_applies_to_tree_glob_and_rg() {
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

    let tree = executor.tree("/codebase", Some(3), None, false);
    assert!(tree.contains("visible.txt"));
    assert!(tree.contains("keep.txt"));
    assert!(tree.contains("keep.log"));
    assert!(!tree.contains("skip.txt"));
    assert!(!tree.contains("drop.log"));

    let glob = executor.glob("**/*", "/codebase", "file");
    assert!(glob.contains("/codebase/visible.txt"));
    assert!(glob.contains("/codebase/ignored/keep.txt"));
    assert!(glob.contains("/codebase/logs/keep.log"));
    assert!(glob.contains("/codebase/.cache/visible.txt"));
    assert!(!glob.contains("/codebase/ignored/skip.txt"));
    assert!(!glob.contains("/codebase/logs/drop.log"));

    if Command::new("rg").arg("--version").output().is_ok() {
        let result = executor.rg("needle", "/codebase", None, None);
        assert!(result.contains("/codebase/visible.txt"));
        assert!(result.contains("/codebase/ignored/keep.txt"));
        assert!(result.contains("/codebase/logs/keep.log"));
        assert!(result.contains("/codebase/.cache/visible.txt"));
        assert!(!result.contains("/codebase/ignored/skip.txt"));
        assert!(!result.contains("/codebase/logs/drop.log"));
    }
}

#[test]
fn rg_includes_filename_for_single_file_results() {
    if Command::new("rg").arg("--version").output().is_err() {
        return;
    }

    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("only.txt"), "needle").unwrap();
    let executor = ToolExecutor::new(tmp.path());

    let result = executor.rg("needle", "/codebase", None, None);

    assert!(result.contains("/codebase/only.txt:1:needle"));
}

#[test]
fn disabled_path_filter_uses_native_rg_traversal() {
    if Command::new("rg").arg("--version").output().is_err() {
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
        filtered.rg("needle", "/codebase", None, None),
        "(no matches)"
    );
    assert!(
        native
            .rg("needle", "/codebase", None, None)
            .contains("/codebase/visible.txt:1:needle")
    );
}

#[test]
fn restricted_exec_commands_execute_with_official_subcommands() {
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
    if Command::new("rg").arg("--version").output().is_ok() {
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

#[test]
fn legacy_readfile_keeps_existing_truncation_defaults() {
    let tmp = TempDir::new().unwrap();
    let executor = ToolExecutor::new(tmp.path());
    let long_line = "a".repeat(400);
    let contents = (1..=60)
        .map(|_| long_line.clone())
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(tmp.path().join("test.txt"), contents).unwrap();

    let output = executor.readfile("/codebase/test.txt", None, None);

    assert!(output.contains("50:"));
    assert!(!output.contains("51:"));
    assert!(output.ends_with("... (lines truncated) ..."));
    assert_eq!(output.lines().next().unwrap().len(), 250);
}

#[test]
fn restricted_exec_readfile_uses_dedicated_defaults() {
    let tmp = TempDir::new().unwrap();
    let executor = ToolExecutor::new(tmp.path());
    let long_line = "a".repeat(400);
    let contents = (1..=210)
        .map(|_| long_line.clone())
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(tmp.path().join("test.txt"), contents).unwrap();

    let output = executor.exec_command(&json!({"type": "readfile", "file": "/codebase/test.txt"}));

    assert!(output.contains("200:"));
    assert!(!output.contains("201:"));
    assert!(output.ends_with("... (lines truncated) ..."));
    assert_eq!(output.lines().next().unwrap().len(), 300);
}

#[test]
fn restricted_exec_ls_uses_new_shared_defaults() {
    let tmp = TempDir::new().unwrap();
    let executor = ToolExecutor::new(tmp.path());
    for idx in 1..=90 {
        fs::write(tmp.path().join(format!("file_{idx:03}.txt")), "").unwrap();
    }

    let output = executor.exec_command(&json!({"type": "ls", "path": "/codebase"}));

    assert!(output.contains("file_080.txt"));
    assert!(!output.contains("file_081.txt"));
    assert!(output.ends_with("... (lines truncated) ..."));
}

#[test]
fn fc_readfile_max_lines_only_affects_restricted_exec_readfile() {
    let _guard = env_lock().lock().unwrap();
    remove_env_var("FC_RESULT_MAX_LINES");
    remove_env_var("FC_LINE_MAX_CHARS");
    set_env_var("FC_READFILE_MAX_LINES", "120");

    let tmp = TempDir::new().unwrap();
    let executor = ToolExecutor::new(tmp.path());
    let contents = (1..=130)
        .map(|idx| format!("line-{idx}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(tmp.path().join("test.txt"), contents).unwrap();

    let tool_output =
        executor.exec_command(&json!({"type": "readfile", "file": "/codebase/test.txt"}));
    let legacy_output = executor.readfile("/codebase/test.txt", None, None);

    assert!(tool_output.contains("120:line-120"));
    assert!(!tool_output.contains("121:line-121"));
    assert!(legacy_output.contains("50:line-50"));
    assert!(!legacy_output.contains("51:line-51"));

    remove_env_var("FC_READFILE_MAX_LINES");
}

#[test]
fn fc_result_max_lines_only_affects_windsurf_non_readfile_tools() {
    let _guard = env_lock().lock().unwrap();
    set_env_var("FC_RESULT_MAX_LINES", "10");
    remove_env_var("FC_READFILE_MAX_LINES");
    remove_env_var("FC_LINE_MAX_CHARS");

    let tmp = TempDir::new().unwrap();
    let executor = ToolExecutor::new(tmp.path());
    for idx in 1..=20 {
        fs::write(tmp.path().join(format!("file_{idx:03}.txt")), "").unwrap();
    }

    let tool_output = executor.exec_command(&json!({"type": "ls", "path": "/codebase"}));
    let legacy_output = executor.ls("/codebase", false, false);

    assert!(tool_output.contains("file_010.txt"));
    assert!(!tool_output.contains("file_011.txt"));
    assert!(legacy_output.contains("file_011.txt"));

    remove_env_var("FC_RESULT_MAX_LINES");
}
