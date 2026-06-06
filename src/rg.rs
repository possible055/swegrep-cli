use std::env;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

const RG_PATH_ENV: &str = "SWEGREP_RG_PATH";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RgResolutionError {
    EnvPathNotFound(PathBuf),
    NotFound,
}

impl fmt::Display for RgResolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EnvPathNotFound(path) => write!(
                f,
                "Error: ripgrep ('rg') was not found at SWEGREP_RG_PATH={}.\nHint: provide a valid SWEGREP_RG_PATH, place bundled rg next to swegrep-cli, or install rg in PATH.",
                path.display()
            ),
            Self::NotFound => write!(f, "{}", ripgrep_not_found_message()),
        }
    }
}

pub(crate) fn resolve_rg_path() -> Result<PathBuf, RgResolutionError> {
    resolve_rg_path_from(
        env::var_os(RG_PATH_ENV),
        env::current_exe().ok(),
        env::var_os("PATH"),
    )
}

pub(crate) fn ripgrep_not_found_message() -> &'static str {
    "Error: ripgrep ('rg') is required but was not found. Provide SWEGREP_RG_PATH, place bundled rg next to swegrep-cli, or install rg in PATH."
}

fn resolve_rg_path_from(
    env_path: Option<OsString>,
    current_exe: Option<PathBuf>,
    path_env: Option<OsString>,
) -> Result<PathBuf, RgResolutionError> {
    if let Some(path) = env_path.filter(|path| !path.is_empty()) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(RgResolutionError::EnvPathNotFound(path));
    }

    if let Some(path) = current_exe
        .as_deref()
        .and_then(Path::parent)
        .and_then(find_bundled_rg)
    {
        return Ok(path);
    }

    find_rg_in_path(path_env).ok_or(RgResolutionError::NotFound)
}

fn find_bundled_rg(dir: &Path) -> Option<PathBuf> {
    let candidate = dir.join(rg_binary_name());
    candidate.is_file().then_some(candidate)
}

fn find_rg_in_path(path_env: Option<OsString>) -> Option<PathBuf> {
    let paths = path_env?;
    for dir in env::split_paths(&paths) {
        let candidate = dir.join("rg");
        if candidate.is_file() {
            return Some(candidate);
        }

        if cfg!(target_os = "windows") {
            for ext in path_extensions() {
                let candidate = dir.join(format!("rg{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn rg_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "rg.exe"
    } else {
        "rg"
    }
}

fn path_extensions() -> Vec<String> {
    env::var_os("PATHEXT")
        .map(|value| {
            value
                .to_string_lossy()
                .split(';')
                .filter(|ext| !ext.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|extensions| !extensions.is_empty())
        .unwrap_or_else(|| vec![".exe".to_string(), ".bat".to_string(), ".cmd".to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn env_path_takes_priority() {
        let tmp = TempDir::new().unwrap();
        let env_rg = tmp.path().join("custom-rg");
        let bundled_dir = tmp.path().join("bundled");
        let path_dir = tmp.path().join("path");
        fs::create_dir(&bundled_dir).unwrap();
        fs::create_dir(&path_dir).unwrap();
        fs::write(&env_rg, "").unwrap();
        fs::write(bundled_dir.join(rg_binary_name()), "").unwrap();
        fs::write(path_dir.join("rg"), "").unwrap();

        let resolved = resolve_rg_path_from(
            Some(env_rg.clone().into_os_string()),
            Some(bundled_dir.join("swegrep-cli")),
            Some(path_dir.into_os_string()),
        )
        .unwrap();

        assert_eq!(resolved, env_rg);
    }

    #[test]
    fn bundled_rg_takes_priority_over_path() {
        let tmp = TempDir::new().unwrap();
        let bundled_dir = tmp.path().join("bundled");
        let path_dir = tmp.path().join("path");
        fs::create_dir(&bundled_dir).unwrap();
        fs::create_dir(&path_dir).unwrap();
        let bundled_rg = bundled_dir.join(rg_binary_name());
        fs::write(&bundled_rg, "").unwrap();
        fs::write(path_dir.join("rg"), "").unwrap();

        let resolved = resolve_rg_path_from(
            None,
            Some(bundled_dir.join("swegrep-cli")),
            Some(path_dir.into_os_string()),
        )
        .unwrap();

        assert_eq!(resolved, bundled_rg);
    }

    #[test]
    fn missing_rg_returns_clear_error() {
        let tmp = TempDir::new().unwrap();
        let error =
            resolve_rg_path_from(None, Some(tmp.path().join("swegrep-cli")), None).unwrap_err();

        assert_eq!(error, RgResolutionError::NotFound);
        assert!(error.to_string().contains("SWEGREP_RG_PATH"));
        assert!(error.to_string().contains("bundled rg"));
        assert!(error.to_string().contains("PATH"));
    }

    #[test]
    fn invalid_env_path_is_reported() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("missing-rg");
        let error =
            resolve_rg_path_from(Some(missing.clone().into_os_string()), None, None).unwrap_err();

        assert_eq!(error, RgResolutionError::EnvPathNotFound(missing));
        assert!(error.to_string().contains("SWEGREP_RG_PATH"));
    }
}
