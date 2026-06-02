use crate::executor::ToolExecutor;
use regex::Regex;
use std::path::{Component, Path};

use super::{FileEntry, MAX_TREE_BYTES, SearchResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoMap {
    pub tree: String,
    pub depth: usize,
    pub size_bytes: usize,
    pub fell_back: bool,
}

pub fn get_repo_map(project_root: &Path, target_depth: usize, exclude_paths: &[String]) -> RepoMap {
    let executor = ToolExecutor::new(project_root);
    for depth in (1..=target_depth).rev() {
        let tree = executor.tree("/codebase", Some(depth), Some(exclude_paths), false);
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

    match std::fs::read_dir(project_root) {
        Ok(entries) => {
            let mut names = entries
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| !exclude_paths.iter().any(|pat| glob_match(pat, name)))
                .collect::<Vec<_>>();
            names.sort();
            let tree = std::iter::once("/codebase".to_string())
                .chain(names.into_iter().map(|name| format!("├── {name}")))
                .collect::<Vec<_>>()
                .join("\n");
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

fn glob_match(pattern: &str, text: &str) -> bool {
    globset::Glob::new(pattern)
        .map(|glob| glob.compile_matcher().is_match(text))
        .unwrap_or(false)
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
        let rel = vpath
            .strip_prefix("/codebase")
            .unwrap_or(vpath)
            .trim_start_matches(['/', '\\'])
            .replace('\\', "/");

        let rel_path = Path::new(&rel);
        if rel_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        }) {
            continue;
        }

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
