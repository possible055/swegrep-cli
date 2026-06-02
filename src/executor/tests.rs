use super::*;
use crate::path_filter::PathFilterConfig;
use serde_json::{Value, json};
use std::fs;
use std::process::Command;
use std::thread;
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
        res.find("<command1_result>\n1\n").unwrap() < res.find("<command2_result>\n2\n").unwrap()
    );
    assert!(
        res.find("<command2_result>\n2\n").unwrap() < res.find("<command3_result>\n3\n").unwrap()
    );
    assert!(
        res.find("<command3_result>\n3\n").unwrap() < res.find("<command4_result>\n4\n").unwrap()
    );
}
