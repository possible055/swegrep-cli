use crate::executor::ToolExecutor;
use crate::path_filter::PathFilterConfig;
use regex::Regex;
use std::collections::BTreeMap;
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RangeMap {
    ranges: BTreeMap<String, Vec<(usize, usize)>>,
}

impl RangeMap {
    pub(crate) fn add_range(&mut self, path: &str, start: usize, end: usize) {
        let Some(rel) = safe_codebase_rel(path) else {
            return;
        };
        if start == 0 || end == 0 {
            return;
        }
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        self.ranges.entry(rel).or_default().push((start, end));
    }

    pub(crate) fn merge_search_result(&mut self, result: &SearchResult) {
        for file in &result.files {
            for (start, end) in &file.ranges {
                self.add_range(&file.path, *start, *end);
            }
        }
    }

    pub(crate) fn merge_tool_output(&mut self, command: &serde_json::Value, output: &str) {
        match command
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
        {
            "rg" => self.merge_rg_output(output),
            "readfile" => self.merge_readfile_output(command, output),
            _ => {}
        }
    }

    pub(crate) fn to_result(&self, project_root: &Path, max_results: usize) -> SearchResult {
        let resolved_root = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
        let mut files = Vec::new();
        for (path, ranges) in &self.ranges {
            if files.len() >= max_results {
                break;
            }
            let ranges = merged_ranges(ranges);
            if ranges.is_empty() {
                continue;
            }
            files.push(FileEntry {
                path: path.clone(),
                full_path: resolved_root.join(path).to_string_lossy().into_owned(),
                ranges,
            });
        }
        SearchResult {
            files,
            ..SearchResult::default()
        }
    }

    fn merge_rg_output(&mut self, output: &str) {
        let regex = Regex::new(r"(?m)^/codebase/([^:\n]+):(\d+):").unwrap();
        for capture in regex.captures_iter(output) {
            let Some(path) = capture.get(1).map(|m| m.as_str()) else {
                continue;
            };
            let Some(line) = capture
                .get(2)
                .and_then(|m| m.as_str().parse::<usize>().ok())
            else {
                continue;
            };
            self.add_range(path, line, line);
        }
    }

    fn merge_readfile_output(&mut self, command: &serde_json::Value, output: &str) {
        let Some(file) = command.get("file").and_then(serde_json::Value::as_str) else {
            return;
        };
        let lines = output
            .lines()
            .filter_map(|line| line.split_once(':')?.0.parse::<usize>().ok())
            .collect::<Vec<_>>();
        if let (Some(start), Some(end)) = (lines.iter().min(), lines.iter().max()) {
            self.add_range(file, *start, *end);
            return;
        }

        let start = command
            .get("start_line")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(1);
        let end = command
            .get("end_line")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(start);
        self.add_range(file, start, end);
    }
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

        let full_path = resolved_root.join(&rel);
        let ranges = range_regex
            .captures_iter(body)
            .filter_map(|range| {
                let start = range.get(1)?.as_str().parse::<usize>().ok()?;
                let end = range.get(2)?.as_str().parse::<usize>().ok()?;
                Some((start, end))
            })
            .collect::<Vec<_>>();

        files.push(FileEntry {
            path: rel,
            full_path: full_path.to_string_lossy().into_owned(),
            ranges,
        });
    }

    SearchResult {
        files,
        ..SearchResult::default()
    }
}

pub(crate) fn parse_range_map_answer(
    xml_text: &str,
    project_root: &Path,
    range_map: &RangeMap,
    max_results: usize,
) -> SearchResult {
    let mut merged = range_map.clone();
    let parsed = parse_answer(xml_text, project_root);
    merged.merge_search_result(&parsed);
    merged.to_result(project_root, max_results)
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

fn merged_ranges(ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut ranges = ranges
        .iter()
        .copied()
        .filter(|(start, end)| *start > 0 && *end > 0)
        .map(|(start, end)| {
            if start <= end {
                (start, end)
            } else {
                (end, start)
            }
        })
        .collect::<Vec<_>>();
    ranges.sort_unstable();

    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        if let Some((_, last_end)) = merged.last_mut()
            && start <= *last_end + 1
        {
            *last_end = (*last_end).max(end);
            continue;
        }
        merged.push((start, end));
    }
    merged
}
