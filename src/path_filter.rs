use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
    gitignores: Mutex<HashMap<PathBuf, Gitignore>>,
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
        let gitignores = Mutex::new(HashMap::from([(root.clone(), gitignore)]));
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
            gitignores,
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
        if self.matches_gitignore(path, is_dir) {
            return false;
        }
        true
    }

    fn relative_path(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.root).unwrap_or(path).to_path_buf()
    }

    fn matches_gitignore(&self, path: &Path, is_dir: bool) -> bool {
        let mut ignored = false;
        for dir in self.gitignore_dirs(path) {
            match self
                .gitignore_for_dir(&dir)
                .matched_path_or_any_parents(path, is_dir)
            {
                Match::Ignore(_) => ignored = true,
                Match::Whitelist(_) => ignored = false,
                Match::None => {}
            }
        }
        ignored
    }

    fn gitignore_dirs(&self, path: &Path) -> Vec<PathBuf> {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return vec![self.root.clone()];
        };
        let mut dirs = vec![self.root.clone()];
        let Some(parent) = rel.parent() else {
            return dirs;
        };

        let mut current = self.root.clone();
        for component in parent.components() {
            current.push(component);
            dirs.push(current.clone());
        }
        dirs
    }

    fn gitignore_for_dir(&self, dir: &Path) -> Gitignore {
        if let Ok(cache) = self.gitignores.lock()
            && let Some(matcher) = cache.get(dir)
        {
            return matcher.clone();
        }

        let matcher = build_gitignore_file_quiet(dir);
        let Ok(mut cache) = self.gitignores.lock() else {
            return matcher;
        };
        cache
            .entry(dir.to_path_buf())
            .or_insert_with(|| matcher.clone())
            .clone()
    }
}

fn build_gitignore_file(root: &Path, warnings: &mut Vec<String>) -> Gitignore {
    build_gitignore_file_with_warning(root, |warning| warnings.push(warning))
}

fn build_gitignore_file_quiet(root: &Path) -> Gitignore {
    build_gitignore_file_with_warning(root, |_| {})
}

fn build_gitignore_file_with_warning(root: &Path, mut warn: impl FnMut(String)) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    let gitignore_path = root.join(".gitignore");
    if gitignore_path.exists()
        && let Some(err) = builder.add(&gitignore_path)
    {
        warn(format!(
            "Could not load {}: {err}",
            gitignore_path.display()
        ));
    }
    match builder.build() {
        Ok(gitignore) => gitignore,
        Err(err) => {
            warn(format!("Could not build .gitignore matcher: {err}"));
            empty_gitignore(root)
        }
    }
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
            empty_gitignore(root)
        }
    }
}

fn empty_gitignore(root: &Path) -> Gitignore {
    GitignoreBuilder::new(root)
        .build()
        .expect("empty gitignore matcher should build")
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
    fn nested_gitignore_excludes_only_its_descendants() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("pkg")).unwrap();
        fs::write(tmp.path().join("pkg").join(".gitignore"), "*.tmp\n").unwrap();
        let filter = PathFilter::new(tmp.path(), PathFilterConfig::default());

        assert!(filter.is_visible(&tmp.path().join("root.tmp"), false));
        assert!(filter.is_visible(&tmp.path().join("pkg"), true));
        assert!(!filter.is_visible(&tmp.path().join("pkg").join("drop.tmp"), false));
        assert!(filter.is_visible(&tmp.path().join("pkg").join("keep.rs"), false));
    }

    #[test]
    fn nested_gitignore_can_override_parent_file_pattern() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "*.log\n").unwrap();
        fs::create_dir_all(tmp.path().join("pkg")).unwrap();
        fs::write(tmp.path().join("pkg").join(".gitignore"), "!keep.log\n").unwrap();
        let filter = PathFilter::new(tmp.path(), PathFilterConfig::default());

        assert!(!filter.is_visible(&tmp.path().join("root.log"), false));
        assert!(filter.is_visible(&tmp.path().join("pkg").join("keep.log"), false));
        assert!(!filter.is_visible(&tmp.path().join("pkg").join("drop.log"), false));
    }

    #[test]
    fn include_overrides_nested_gitignore() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("pkg")).unwrap();
        fs::write(tmp.path().join("pkg").join(".gitignore"), "*.txt\n").unwrap();
        let config = PathFilterConfig {
            include_patterns: vec!["pkg/keep.txt".to_string()],
            ..PathFilterConfig::default()
        };
        let filter = PathFilter::new(tmp.path(), config);

        assert!(filter.is_visible(&tmp.path().join("pkg").join("keep.txt"), false));
        assert!(!filter.is_visible(&tmp.path().join("pkg").join("drop.txt"), false));
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
