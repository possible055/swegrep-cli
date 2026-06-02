use crate::executor::ToolExecutor;
use crate::path_filter::PathFilterConfig;
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

pub fn get_repo_map(
    project_root: &Path,
    target_depth: usize,
    path_filter_config: &PathFilterConfig,
) -> RepoMap {
    let executor =
        ToolExecutor::with_limits_and_filter(project_root, None, None, path_filter_config.clone());
    for depth in (1..=target_depth).rev() {
        let tree = executor.tree("/codebase", Some(depth), None, false);
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
                .filter(|entry| {
                    let path = entry.path();
                    executor.path_visible(&path, path.is_dir())
                })
                .filter_map(|entry| entry.file_name().into_string().ok())
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
