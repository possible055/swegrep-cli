use super::{SearchError, SearchMeta, SearchResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchOutputConfig {
    pub max_turns: usize,
    pub max_results: usize,
    pub max_commands: usize,
}

pub fn format_search_output(result: &SearchResult) -> String {
    if result.error.is_some() {
        return format_search_error(result, None);
    }
    format_search_success(result)
}

pub fn format_search_success(result: &SearchResult) -> String {
    let patterns = unique_patterns(&result.rg_patterns);
    if result.files.is_empty() && patterns.is_empty() {
        return format_no_relevant_files(result.raw_response.as_deref());
    }

    let mut parts = Vec::new();
    if result.files.is_empty() {
        parts.push("No files found.".to_string());
    } else {
        parts.push(format!("Found {} relevant files.\n", result.files.len()));
        for (idx, entry) in result.files.iter().enumerate() {
            let ranges = entry
                .ranges
                .iter()
                .map(|(start, end)| format!("L{start}-{end}"))
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!(
                "  [{}/{}] {} ({ranges})",
                idx + 1,
                result.files.len(),
                entry.full_path
            ));
        }
    }

    if !patterns.is_empty() {
        parts.push(String::new());
        parts.push(format!("grep keywords: {}", patterns.join(", ")));
    }

    parts.join("\n")
}

pub fn format_search_error(result: &SearchResult, config: Option<SearchOutputConfig>) -> String {
    let Some(error) = result.error.as_ref() else {
        return String::new();
    };

    let mut message = format_error_header(error);
    append_diagnostics(&mut message, &result.meta, config);
    message
}

fn format_error_header(error: &SearchError) -> String {
    format!("Error: {error}")
}

fn append_diagnostics(message: &mut String, meta: &SearchMeta, config: Option<SearchOutputConfig>) {
    if meta.tree_depth.is_none() && meta.error_code.is_none() && config.is_none() {
        return;
    }

    message.push_str(&format!(
        "\n\n[diagnostic] error_type={}, tree_depth_used={:?}, tree_size={:?}KB",
        meta.error_code.as_deref().unwrap_or("unknown"),
        meta.tree_depth,
        meta.tree_size_kb
    ));
    if meta.fell_back == Some(true) {
        message.push_str(" (auto fell back)");
    }
    if meta.context_trimmed == Some(true) {
        message.push_str(", context_trimmed=true");
    }
    if let Some(project_root) = &meta.project_root {
        message.push_str(&format!("\n[diagnostic] project_path={project_root}"));
    }
    if let Some(config) = config {
        message.push_str(&format!(
            "\n[config] max_turns={}, max_results={}, max_commands={}",
            config.max_turns, config.max_results, config.max_commands
        ));
    }

    match meta.error_code.as_deref() {
        Some("PAYLOAD_TOO_LARGE" | "TIMEOUT") => {
            message.push_str(
                "\n[hint] Try: reduce scope snapshot depth, add exclude.txt entries, or narrow project_path.",
            );
        }
        Some("AUTH_ERROR") => {
            message.push_str(
                "\n[hint] Authentication error. Ensure a fresh WINDSURF_API_KEY is configured.",
            );
        }
        Some("RATE_LIMITED") => {
            message.push_str("\n[hint] Rate limited. Wait a moment and retry.");
        }
        _ => {}
    }
}

fn format_no_relevant_files(raw_response: Option<&str>) -> String {
    match raw_response {
        Some(raw_response) => format!("No relevant files found.\n\nRaw response:\n{raw_response}"),
        None => "No relevant files found.".to_string(),
    }
}

fn unique_patterns(patterns: &[String]) -> Vec<String> {
    let mut unique = Vec::new();
    for pattern in patterns {
        if pattern.len() >= 3 && !unique.contains(pattern) {
            unique.push(pattern.clone());
        }
    }
    unique
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{FileEntry, SearchMeta};

    #[test]
    fn formats_file_results_and_keywords_as_plain_text() {
        let result = SearchResult {
            files: vec![FileEntry {
                path: "src/lib.rs".to_string(),
                full_path: "/repo/src/lib.rs".to_string(),
                ranges: vec![(1, 10), (20, 30)],
            }],
            rg_patterns: vec!["auth".to_string(), "auth".to_string(), "id".to_string()],
            ..SearchResult::default()
        };

        let output = format_search_success(&result);

        assert!(output.contains("Found 1 relevant files."));
        assert!(output.contains("[1/1] /repo/src/lib.rs (L1-10, L20-30)"));
        assert!(output.contains("grep keywords: auth"));
        assert!(!output.contains("id"));
        assert!(!output.trim_start().starts_with('{'));
    }

    #[test]
    fn formats_only_keywords_without_files() {
        let result = SearchResult {
            rg_patterns: vec!["needle".to_string()],
            ..SearchResult::default()
        };

        assert_eq!(
            format_search_success(&result),
            "No files found.\n\ngrep keywords: needle"
        );
    }

    #[test]
    fn formats_no_files_with_raw_response() {
        let result = SearchResult {
            raw_response: Some("model text".to_string()),
            ..SearchResult::default()
        };

        assert_eq!(
            format_search_success(&result),
            "No relevant files found.\n\nRaw response:\nmodel text"
        );
    }

    #[test]
    fn formats_error_with_diagnostics() {
        let result = SearchResult {
            error: Some(SearchError::new("TIMEOUT", "request timed out")),
            meta: SearchMeta {
                tree_depth: Some(2),
                tree_size_kb: Some(12.5),
                error_code: Some("TIMEOUT".to_string()),
                project_root: Some("/repo".to_string()),
                ..SearchMeta::default()
            },
            ..SearchResult::default()
        };

        let output = format_search_error(
            &result,
            Some(SearchOutputConfig {
                max_turns: 4,
                max_results: 10,
                max_commands: 8,
            }),
        );

        assert!(output.contains("Error: TIMEOUT: request timed out"));
        assert!(output.contains("[diagnostic] error_type=TIMEOUT"));
        assert!(output.contains("[diagnostic] project_path=/repo"));
        assert!(output.contains("[config] max_turns=4, max_results=10, max_commands=8"));
        assert!(output.contains("[hint] Try: reduce scope snapshot depth"));
    }
}
