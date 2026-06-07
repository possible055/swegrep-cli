use crate::path_filter::PathFilterConfig;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub full_path: String,
    pub ranges: Vec<(usize, usize)>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchMeta {
    pub tree_depth: Option<usize>,
    pub tree_size_kb: Option<f64>,
    pub fell_back: Option<bool>,
    pub project_root: Option<String>,
    pub error_code: Option<String>,
    pub context_trimmed: Option<bool>,
    pub instant_context: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    pub files: Vec<FileEntry>,
    pub rg_patterns: Vec<String>,
    pub raw_response: Option<String>,
    pub error: Option<SearchError>,
    pub meta: SearchMeta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchError {
    pub code: String,
    pub message: String,
}

impl SearchError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub query: String,
    pub project_root: PathBuf,
    pub api_key: Option<String>,
    pub jwt: Option<String>,
    pub app_version: Option<String>,
    pub ls_version: Option<String>,
    pub max_turns: usize,
    pub max_commands: usize,
    pub max_results: usize,
    pub tree_depth: usize,
    pub timeout_ms: u64,
    pub path_filter: PathFilterConfig,
    pub result_max_lines: Option<usize>,
    pub line_max_chars: Option<usize>,
}

impl SearchOptions {
    pub fn new(query: impl Into<String>, project_root: impl Into<PathBuf>) -> Self {
        Self {
            query: query.into(),
            project_root: project_root.into(),
            api_key: None,
            jwt: None,
            app_version: None,
            ls_version: None,
            max_turns: 4,
            max_commands: 8,
            max_results: 10,
            tree_depth: 4,
            timeout_ms: 30_000,
            path_filter: PathFilterConfig::default(),
            result_max_lines: None,
            line_max_chars: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthCheck {
    pub ok: bool,
    pub error_code: Option<String>,
    pub error: Option<String>,
    pub jwt_source: String,
    pub app_version: String,
    pub ls_version: String,
}
