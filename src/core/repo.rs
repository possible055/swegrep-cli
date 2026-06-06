use crate::executor::ToolExecutor;
use crate::path_filter::PathFilterConfig;
use regex::Regex;
use std::ffi::OsStr;
use std::path::{Component, Path};

use super::{FileEntry, MAX_TREE_BYTES, SearchResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoMap {
    pub tree: String,
    pub depth: usize,
    pub size_bytes: usize,
    pub fell_back: bool,
}

pub fn get_repo_map(
    project_root: &Path,
    target_depth: usize,
    path_filter_config: &PathFilterConfig,
) -> RepoMap {
    let executor =
        ToolExecutor::with_limits_and_filter(project_root, None, None, path_filter_config.clone());
    for depth in (1..=target_depth).rev() {
        let Ok(tree) = build_scope_manifest(project_root, &executor, depth) else {
            break;
        };
        let size_bytes = tree.len();
        if size_bytes <= MAX_TREE_BYTES {
            return RepoMap {
                tree,
                depth,
                size_bytes,
                fell_back: depth < target_depth,
            };
        }
    }

    match visible_entries(project_root, &executor) {
        Ok(entries) => {
            let tree = capped_root_manifest(project_root, entries);
            RepoMap {
                size_bytes: tree.len(),
                tree,
                depth: 0,
                fell_back: true,
            }
        }
        Err(_) => {
            let tree = "/codebase\n(empty or inaccessible)".to_string();
            RepoMap {
                size_bytes: tree.len(),
                tree,
                depth: 0,
                fell_back: true,
            }
        }
    }
}

fn build_scope_manifest(
    project_root: &Path,
    executor: &ToolExecutor,
    max_depth: usize,
) -> std::io::Result<String> {
    let mut lines = vec!["/codebase".to_string()];
    let entries = visible_entries(project_root, executor)?;
    append_scope_manifest_entries(&mut lines, project_root, executor, max_depth, 1, entries);
    Ok(lines.join("\n"))
}

fn append_scope_manifest_entries(
    lines: &mut Vec<String>,
    project_root: &Path,
    executor: &ToolExecutor,
    max_depth: usize,
    current_depth: usize,
    entries: Vec<std::path::PathBuf>,
) {
    if current_depth > max_depth {
        return;
    }

    for path in entries {
        let is_dir = path.is_dir();
        lines.push(manifest_line(project_root, &path, is_dir));
        if is_dir && current_depth < max_depth {
            let Ok(entries) = visible_entries(&path, executor) else {
                continue;
            };
            append_scope_manifest_entries(
                lines,
                project_root,
                executor,
                max_depth,
                current_depth + 1,
                entries,
            );
        }
    }
}

fn visible_entries(
    dir_path: &Path,
    executor: &ToolExecutor,
) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut paths = std::fs::read_dir(dir_path)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| executor.path_visible(path, path.is_dir()))
        .collect::<Vec<_>>();
    paths.sort_by(|a, b| compare_scope_paths(a, b));
    Ok(paths)
}

fn compare_scope_paths(a: &Path, b: &Path) -> std::cmp::Ordering {
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

fn manifest_line(project_root: &Path, path: &Path, is_dir: bool) -> String {
    let mut rel = path
        .strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    if is_dir {
        rel.push('/');
    }
    rel
}

fn capped_root_manifest(project_root: &Path, entries: Vec<std::path::PathBuf>) -> String {
    let suffix = "... (scope snapshot truncated) ...";
    let mut tree = String::from("/codebase");
    let mut truncated = false;

    for path in entries {
        let line = format!("\n{}", manifest_line(project_root, &path, path.is_dir()));
        let suffix_len = if truncated { 0 } else { suffix.len() + 1 };
        if tree.len() + line.len() + suffix_len > MAX_TREE_BYTES {
            truncated = true;
            break;
        }
        tree.push_str(&line);
    }

    if truncated {
        tree.push('\n');
        tree.push_str(suffix);
    }
    tree
}

pub fn parse_answer(xml_text: &str, project_root: &Path) -> SearchResult {
    let mut files = Vec::new();
    let resolved_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let file_regex = Regex::new(r#"<file\s+path=["']([^"']+)["']>([\s\S]*?)</file>"#).unwrap();
    let range_regex = Regex::new(r"<range>(\d+)-(\d+)</range>").unwrap();

    for captures in file_regex.captures_iter(xml_text) {
        let Some(vpath) = captures.get(1).map(|m| m.as_str()) else {
            continue;
        };
        let body = captures.get(2).map(|m| m.as_str()).unwrap_or_default();
        let Some(rel) = safe_codebase_rel(vpath) else {
            continue;
        };
        if let Some(file) = parse_answer_file(&resolved_root, rel, body, &range_regex) {
            files.push(file);
        }
    }

    SearchResult {
        files,
        ..SearchResult::default()
    }
}

fn parse_answer_file(
    resolved_root: &Path,
    rel: String,
    body: &str,
    range_regex: &Regex,
) -> Option<FileEntry> {
    let full_path = resolved_root.join(&rel);
    let full_path = full_path.canonicalize().ok()?;
    if !full_path.is_file() || !full_path.starts_with(resolved_root) {
        return None;
    }

    let line_count = file_line_count(&full_path).ok()?;
    if line_count == 0 {
        return None;
    }

    let ranges = range_regex
        .captures_iter(body)
        .filter_map(|range| {
            let start = range.get(1)?.as_str().parse::<usize>().ok()?;
            let end = range.get(2)?.as_str().parse::<usize>().ok()?;
            normalize_answer_range(start, end, line_count)
        })
        .collect::<Vec<_>>();
    if ranges.is_empty() {
        return None;
    }

    Some(FileEntry {
        path: rel,
        full_path: full_path.to_string_lossy().into_owned(),
        ranges,
    })
}

fn file_line_count(path: &Path) -> std::io::Result<usize> {
    let content = std::fs::read(path)?;
    if content.is_empty() {
        return Ok(0);
    }
    let newline_count = content.iter().filter(|byte| **byte == b'\n').count();
    if content.last() == Some(&b'\n') {
        Ok(newline_count)
    } else {
        Ok(newline_count + 1)
    }
}

fn normalize_answer_range(start: usize, end: usize, line_count: usize) -> Option<(usize, usize)> {
    if start == 0 || end == 0 {
        return None;
    }
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    if start > line_count {
        return None;
    }
    Some((start, end.min(line_count)))
}

fn safe_codebase_rel(path: &str) -> Option<String> {
    let rel = path
        .strip_prefix("/codebase")
        .unwrap_or(path)
        .trim_start_matches(['/', '\\'])
        .replace('\\', "/");
    if rel.is_empty() {
        return None;
    }
    let rel_path = Path::new(&rel);
    if rel_path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::Prefix(_) | Component::RootDir
        )
    }) {
        return None;
    }
    Some(rel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path_filter::PathFilterConfig;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn parse_answer_filters_path_traversal() {
        let xml = r#"
        Some thoughts first.
        <ANSWER>
          <file path="/codebase/src/main.py">
            <range>10-20</range>
            <range>30-40</range>
          </file>
          <file path="/codebase/tests/test_main.py">
            <range>1-5</range>
          </file>
          <file path="/codebase/../../etc/passwd">
            <range>1-2</range>
          </file>
        </ANSWER>
        "#;
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::create_dir_all(tmp.path().join("tests")).unwrap();
        fs::write(tmp.path().join("src/main.py"), numbered_lines(40)).unwrap();
        fs::write(tmp.path().join("tests/test_main.py"), numbered_lines(5)).unwrap();

        let result = parse_answer(xml, tmp.path());
        assert_eq!(result.files.len(), 2);
        assert_eq!(result.files[0].path, "src/main.py");
        assert_eq!(result.files[0].ranges, vec![(10, 20), (30, 40)]);
        assert_eq!(result.files[1].path, "tests/test_main.py");
        assert_eq!(result.files[1].ranges, vec![(1, 5)]);
    }

    #[test]
    fn parse_answer_validates_files_and_ranges() {
        let xml = r#"
        <ANSWER>
          <file path="/codebase/src/lib.rs">
            <range>4-2</range>
            <range>3-20</range>
            <range>0-2</range>
            <range>9-10</range>
          </file>
          <file path="/codebase/src/missing.rs">
            <range>1-1</range>
          </file>
          <file path="/codebase/src/empty.rs">
            <range>1-1</range>
          </file>
          <file path="/codebase/src/no_ranges.rs">
          </file>
        </ANSWER>
        "#;
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/lib.rs"), numbered_lines(6)).unwrap();
        fs::write(tmp.path().join("src/empty.rs"), "").unwrap();
        fs::write(tmp.path().join("src/no_ranges.rs"), numbered_lines(1)).unwrap();

        let result = parse_answer(xml, tmp.path());

        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, "src/lib.rs");
        assert_eq!(result.files[0].ranges, vec![(2, 4), (3, 6)]);
    }

    #[test]
    fn get_repo_map_uses_untruncated_manifest() {
        let tmp = TempDir::new().unwrap();
        for i in 0..60 {
            fs::write(tmp.path().join(format!("file_{i:03}.txt")), "").unwrap();
        }

        let result = get_repo_map(tmp.path(), 1, &PathFilterConfig::default());
        assert!(!result.tree.contains("... (lines truncated) ..."));
        assert!(result.tree.contains("file_059.txt"));
        assert_eq!(result.size_bytes, result.tree.len());
    }

    fn numbered_lines(count: usize) -> String {
        (1..=count)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn get_repo_map_falls_back_to_compact_scope_snapshot() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        for i in 0..6_000 {
            fs::write(
                tmp.path().join(format!(
                    "very_long_file_name_for_scope_snapshot_padding_{i:05}.txt"
                )),
                "",
            )
            .unwrap();
        }
        fs::write(tmp.path().join("src").join("lib.rs"), "").unwrap();

        let result = get_repo_map(tmp.path(), 4, &PathFilterConfig::default());

        assert!(result.size_bytes <= MAX_TREE_BYTES);
        assert!(result.fell_back);
        assert!(result.tree.starts_with("/codebase"));
        assert!(result.tree.lines().any(|line| line == "src/"));
    }

    #[test]
    fn get_repo_map_uses_manifest_path_format() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        fs::write(tmp.path().join("src").join("main.rs"), "").unwrap();

        let result = get_repo_map(tmp.path(), 2, &PathFilterConfig::default());
        let lines = result.tree.lines().collect::<Vec<_>>();

        assert_eq!(lines[0], "/codebase");
        assert!(lines.contains(&"Cargo.toml"));
        assert!(lines.contains(&"src/"));
        assert!(lines.contains(&"src/main.rs"));
        assert!(!lines.contains(&"/codebase/src/main.rs"));
    }
}
