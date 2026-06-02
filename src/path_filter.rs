use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PathFilterConfig {
    pub enabled: bool,
    pub include_patterns: Vec<String>,
    pub exclude_patterns: Vec<String>,
    pub warnings: Vec<String>,
}

impl Default for PathFilterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            include_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

impl PathFilterConfig {
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    pub fn with_excludes(exclude_patterns: Vec<String>) -> Self {
        Self {
            enabled: true,
            exclude_patterns,
            ..Self::default()
        }
    }
}

#[derive(Debug)]
pub struct PathFilter {
    enabled: bool,
    root: PathBuf,
    gitignore: Gitignore,
    include: Gitignore,
    exclude: Gitignore,
    include_patterns: Vec<String>,
    warnings: Vec<String>,
}

impl PathFilter {
    pub fn new(project_root: impl AsRef<Path>, config: PathFilterConfig) -> Self {
        let root = project_root.as_ref().to_path_buf();
        let mut warnings = config.warnings;

        let gitignore = build_gitignore_file(&root, &mut warnings);
        let include = build_pattern_set(
            &root,
            "include.txt",
            &config.include_patterns,
            &mut warnings,
        );
        let exclude = build_pattern_set(
            &root,
            "exclude.txt",
            &config.exclude_patterns,
            &mut warnings,
        );

        Self {
            enabled: config.enabled,
            root,
            gitignore,
            include,
            exclude,
            include_patterns: config.include_patterns,
            warnings,
        }
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn is_visible(&self, path: &Path, is_dir: bool) -> bool {
        if !self.enabled {
            return true;
        }

        let rel_path = self.relative_path(path);
        let rel = rel_path.as_path();

        if matches_ignore(&self.exclude, rel, is_dir) {
            return false;
        }
        if matches_ignore(&self.include, rel, is_dir) {
            return true;
        }
        if is_dir && include_may_match_descendant(rel, &self.include_patterns) {
            return true;
        }
        if has_dot_component(rel) {
            return false;
        }
        if matches_ignore(&self.gitignore, rel, is_dir) {
            return false;
        }
        true
    }

    fn relative_path(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.root).unwrap_or(path).to_path_buf()
    }
}

fn build_gitignore_file(root: &Path, warnings: &mut Vec<String>) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    let gitignore_path = root.join(".gitignore");
    if gitignore_path.exists()
        && let Some(err) = builder.add(&gitignore_path)
    {
        warnings.push(format!(
            "Could not load {}: {err}",
            gitignore_path.display()
        ));
    }
    build_or_empty(builder, root, ".gitignore", warnings)
}

fn build_pattern_set(
    root: &Path,
    source_name: &str,
    patterns: &[String],
    warnings: &mut Vec<String>,
) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    for pattern in patterns {
        if let Err(err) = builder.add_line(Some(PathBuf::from(source_name)), pattern) {
            warnings.push(format!(
                "Ignored invalid {source_name} pattern '{pattern}': {err}"
            ));
        }
    }
    build_or_empty(builder, root, source_name, warnings)
}

fn build_or_empty(
    builder: GitignoreBuilder,
    root: &Path,
    source_name: &str,
    warnings: &mut Vec<String>,
) -> Gitignore {
    match builder.build() {
        Ok(gitignore) => gitignore,
        Err(err) => {
            warnings.push(format!("Could not build {source_name} matcher: {err}"));
            GitignoreBuilder::new(root)
                .build()
                .expect("empty gitignore matcher should build")
        }
    }
}

fn matches_ignore(matcher: &Gitignore, path: &Path, is_dir: bool) -> bool {
    match matcher.matched_path_or_any_parents(path, is_dir) {
        Match::Ignore(_) => true,
        Match::Whitelist(_) | Match::None => false,
    }
}

fn has_dot_component(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|part| part.starts_with('.') && part != "." && part != "..")
    })
}

fn include_may_match_descendant(dir: &Path, patterns: &[String]) -> bool {
    let dir = dir.to_string_lossy().replace('\\', "/");
    if dir.is_empty() {
        return true;
    }
    let prefix = format!("{}/", dir.trim_matches('/'));

    patterns.iter().any(|pattern| {
        let pattern = pattern
            .trim()
            .trim_start_matches('!')
            .trim_start_matches('/')
            .trim_end_matches("/**")
            .trim_end_matches('/');
        pattern.starts_with(&prefix)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn gitignore_excludes_by_default() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "target/\n*.log\n").unwrap();
        let filter = PathFilter::new(tmp.path(), PathFilterConfig::default());

        assert!(!filter.is_visible(&tmp.path().join("target"), true));
        assert!(!filter.is_visible(&tmp.path().join("debug.log"), false));
        assert!(filter.is_visible(&tmp.path().join("src/main.rs"), false));
    }

    #[test]
    fn include_overrides_gitignore_and_dot_paths() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "target/\n").unwrap();
        let config = PathFilterConfig {
            include_patterns: vec!["target/keep.rs".to_string(), ".config/".to_string()],
            ..PathFilterConfig::default()
        };
        let filter = PathFilter::new(tmp.path(), config);

        assert!(filter.is_visible(&tmp.path().join("target/keep.rs"), false));
        assert!(filter.is_visible(&tmp.path().join(".config"), true));
        assert!(!filter.is_visible(&tmp.path().join("target/skip.rs"), false));
    }

    #[test]
    fn exclude_overrides_include() {
        let tmp = TempDir::new().unwrap();
        let config = PathFilterConfig {
            include_patterns: vec!["target/keep.rs".to_string()],
            exclude_patterns: vec!["target/".to_string()],
            ..PathFilterConfig::default()
        };
        let filter = PathFilter::new(tmp.path(), config);

        assert!(!filter.is_visible(&tmp.path().join("target/keep.rs"), false));
    }

    #[test]
    fn disabled_filter_allows_paths() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "target/\n").unwrap();
        let config = PathFilterConfig {
            enabled: false,
            exclude_patterns: vec!["target/".to_string()],
            ..PathFilterConfig::default()
        };
        let filter = PathFilter::new(tmp.path(), config);

        assert!(filter.is_visible(&tmp.path().join("target/skip.rs"), false));
        assert!(filter.is_visible(&tmp.path().join(".cache"), true));
    }

    #[test]
    fn invalid_patterns_are_reported() {
        let tmp = TempDir::new().unwrap();
        let config = PathFilterConfig {
            exclude_patterns: vec!["bad\\".to_string()],
            ..PathFilterConfig::default()
        };
        let filter = PathFilter::new(tmp.path(), config);

        assert!(!filter.warnings().is_empty());
    }
}
