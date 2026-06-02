use globset::Glob;
use std::cmp::Ordering;
use std::env;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

pub(super) fn bounded_int(value: Option<usize>, default: usize, min: usize, max: usize) -> usize {
    value.unwrap_or(default).clamp(min, max)
}

pub(super) fn read_int_env(name: &str, default: usize, min: usize, max: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .map(|value| value.clamp(min, max))
        .unwrap_or(default)
}

pub(super) fn normalize_path(path: &Path) -> PathBuf {
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

pub(super) fn compare_dirs_first_case_insensitive(a: &Path, b: &Path) -> Ordering {
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

pub(super) fn matches_any_pattern(patterns: &[String], name: &str, rel: &str) -> bool {
    patterns.iter().any(|pattern| {
        pattern_matches(&pattern.replace('\\', "/"), name) || pattern_matches(pattern, rel)
    })
}

pub(super) fn pattern_matches(pattern: &str, text: &str) -> bool {
    Glob::new(pattern)
        .map(|glob| glob.compile_matcher().is_match(text))
        .unwrap_or(false)
}

pub(super) fn matches_type(path: &Path, type_filter: &str) -> bool {
    match type_filter {
        "file" => path.is_file(),
        "directory" => path.is_dir(),
        _ => true,
    }
}
