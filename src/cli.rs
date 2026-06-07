use crate::core::{
    self, SearchOptions, extract_key, format_search_success, mask_api_key, save_cached_api_key,
    search,
};
use crate::path_filter::PathFilterConfig;
use crate::rg::resolve_rg_path;
use clap::{Args, Parser, Subcommand};
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::io::Write;
use std::path::{Path, PathBuf};

const SCOPE_SNAPSHOT_TREE_DEPTH_ENV: &str = "SCOPE_SNAPSHOT_TREE_DEPTH";
const FC_MAX_TURNS_ENV: &str = "FC_MAX_TURNS";
const FC_MAX_COMMANDS_ENV: &str = "FC_MAX_COMMANDS";
const FC_TIMEOUT_MS_ENV: &str = "FC_TIMEOUT_MS";

#[derive(Debug, Parser)]
#[command(name = "swegrep-cli")]
#[command(disable_help_subcommand = true)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    #[command(about = "Execute semantic code search")]
    Search(SearchArgs),
    #[command(
        name = "extract-key",
        about = "Extract Windsurf API key from local database"
    )]
    ExtractKey(ExtractKeyArgs),
}

#[derive(Debug, Args)]
struct SearchArgs {
    #[arg(help = "Natural language search query")]
    query: String,

    #[arg(long, help = "Windsurf API key. Overrides env and config.")]
    api_key: Option<String>,

    #[arg(long, help = "Absolute or relative path to project root.")]
    path: PathBuf,

    #[arg(
        long,
        value_parser = parse_turns,
        help = "Maximum search rounds. Default is from FC_MAX_TURNS or 4."
    )]
    turns: Option<usize>,
}

#[derive(Debug, Args)]
struct ExtractKeyArgs {
    #[arg(long, help = "Path to Windsurf state.vscdb. Default is auto-detect.")]
    db_path: Option<PathBuf>,

    #[arg(long, help = "Save extracted key to swegrep config.")]
    save: bool,

    #[arg(long, help = "Print the full key instead of a masked key.")]
    show: bool,
}

pub fn run() -> i32 {
    load_skill_env();
    let default_turns = read_env_range(FC_MAX_TURNS_ENV, 4, 4..=6);
    let scope_snapshot_tree_depth = read_env_range(SCOPE_SNAPSHOT_TREE_DEPTH_ENV, 4, 0..=8);
    let max_commands = read_env_range(FC_MAX_COMMANDS_ENV, 8, 1..=8);
    let timeout_ms = read_timeout_ms_env();

    let cli = Cli::parse();
    match cli.command {
        Commands::ExtractKey(args) => run_extract_key(args),
        Commands::Search(args) => run_search(
            args,
            default_turns,
            scope_snapshot_tree_depth,
            max_commands,
            timeout_ms,
        ),
    }
}

pub fn load_skill_env() {
    let env_path = core::get_config_path()
        .parent()
        .map(Path::to_path_buf)
        .map(|dir| dir.join(".env"));
    let Some(env_path) = env_path else {
        return;
    };
    let Ok(text) = fs::read_to_string(env_path) else {
        return;
    };

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let mut value = value.trim().to_string();
        if (value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\''))
        {
            value = value[1..value.len() - 1].to_string();
        }

        if key == "API_KEY" {
            set_env_var("WINDSURF_API_KEY", &value);
        } else {
            set_env_var(key, &value);
        }
    }
}

fn set_env_var(key: &str, value: &str) {
    // SAFETY: this CLI loads its .env file before creating the Tokio runtime or spawning threads.
    unsafe {
        env::set_var(key, value);
    }
}

fn read_env_range(name: &str, default: usize, range: std::ops::RangeInclusive<usize>) -> usize {
    read_env_range_value(name, range).unwrap_or(default)
}

fn read_env_range_value(name: &str, range: std::ops::RangeInclusive<usize>) -> Option<usize> {
    env::var(name)
        .ok()
        .and_then(|raw| parse_env_range_value(&raw, range))
}

fn parse_env_range_value(raw: &str, range: std::ops::RangeInclusive<usize>) -> Option<usize> {
    raw.parse::<usize>()
        .ok()
        .filter(|value| range.contains(value))
}

fn read_timeout_ms_env() -> u64 {
    read_env_u64_range(FC_TIMEOUT_MS_ENV, 1_000..=300_000).unwrap_or(30_000)
}

fn read_env_u64_range(name: &str, range: std::ops::RangeInclusive<u64>) -> Option<u64> {
    env::var(name)
        .ok()
        .and_then(|raw| parse_env_u64_range_value(&raw, range))
}

fn parse_env_u64_range_value(raw: &str, range: std::ops::RangeInclusive<u64>) -> Option<u64> {
    raw.parse::<u64>()
        .ok()
        .filter(|value| range.contains(value))
}

fn parse_turns(value: &str) -> Result<usize, String> {
    parse_range(value, 4..=6, "turns")
}

fn parse_range(
    value: &str,
    range: std::ops::RangeInclusive<usize>,
    label: &str,
) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("{label} must be an integer"))?;
    if range.contains(&parsed) {
        Ok(parsed)
    } else {
        Err(format!(
            "{label} must be between {} and {}",
            range.start(),
            range.end()
        ))
    }
}

fn run_extract_key(args: ExtractKeyArgs) -> i32 {
    let result = extract_key(args.db_path.as_deref());
    if let Some(error) = result.error {
        eprintln!("Error: {error}");
        if let Some(hint) = result.hint {
            eprintln!("Hint: {hint}");
        }
        return 1;
    }

    let Some(key) = result.api_key else {
        eprintln!("Error: apiKey field is empty");
        return 1;
    };

    if args.save {
        match save_cached_api_key(&key, None) {
            Ok(config_path) => eprintln!("Saved Windsurf API key to {}", config_path.display()),
            Err(error) => {
                eprintln!("Error: {error}");
                return 1;
            }
        }
    }

    println!(
        "Windsurf API Key: {}",
        if args.show {
            key.clone()
        } else {
            mask_api_key(&key)
        }
    );
    if let Some(key_type) = result.key_type {
        eprintln!("Key type: {key_type}");
    }
    eprintln!("Source DB: {}", result.db_path);

    if args.show {
        println!("\nRun the following command to set the env var:");
        println!("  export WINDSURF_API_KEY=\"{key}\"");
    }

    0
}

fn run_search(
    args: SearchArgs,
    default_turns: usize,
    scope_snapshot_tree_depth: usize,
    max_commands: usize,
    timeout_ms: u64,
) -> i32 {
    if let Err(error) = resolve_rg_path() {
        eprintln!("{error}");
        return 1;
    }

    let project_path = absolute_path(&args.path);
    if !project_path.is_dir() {
        eprintln!(
            "Error: Project path does not exist: {}",
            project_path.display()
        );
        return 1;
    }

    let path_filter = read_path_filter_config();
    let mut options = SearchOptions::new(args.query, project_path);
    options.api_key = args.api_key;
    options.max_turns = args.turns.unwrap_or(default_turns);
    options.max_commands = max_commands;
    options.timeout_ms = timeout_ms;
    options.tree_depth = scope_snapshot_tree_depth;
    options.path_filter = path_filter;

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Unexpected error: {error}");
            return 1;
        }
    };

    let progress = |message: &str| {
        eprintln!("[fast-context] {message}");
        let _ = std::io::stderr().flush();
    };

    let result = runtime.block_on(async { search(options, Some(&progress)).await });
    if let Some(error) = result.error {
        eprintln!("Search failed: {error}");
        return 1;
    }

    println!("{}", format_search_success(&result));
    0
}

fn read_path_filter_config() -> PathFilterConfig {
    let path_filter_enabled = read_bool_env("SWEGREP_PATH_FILTER");
    if path_filter_enabled == Some(false) {
        return PathFilterConfig::disabled();
    }

    let Some(config_dir) = core::get_config_path().parent().map(Path::to_path_buf) else {
        return PathFilterConfig::default();
    };

    let mut warnings = Vec::new();
    if env::var("SWEGREP_PATH_FILTER").is_ok() && path_filter_enabled.is_none() {
        warnings.push(
            "Invalid SWEGREP_PATH_FILTER value; expected 1/0, true/false, yes/no, or on/off"
                .to_string(),
        );
    }
    let include_patterns = read_filter_patterns(&config_dir.join("include.txt"), &mut warnings);
    let exclude_patterns = read_filter_patterns(&config_dir.join("exclude.txt"), &mut warnings);

    PathFilterConfig {
        enabled: true,
        include_patterns,
        exclude_patterns,
        warnings,
    }
}

fn read_bool_env(name: &str) -> Option<bool> {
    let value = env::var(name).ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn read_filter_patterns(path: &Path, warnings: &mut Vec<String>) -> Vec<String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            warnings.push(format!("Could not read {}: {err}", path.display()));
            return Vec::new();
        }
    };

    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToOwned::to_owned)
        .collect()
}

fn absolute_path(path: &Path) -> PathBuf {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    candidate.canonicalize().unwrap_or(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_snapshot_tree_depth_env_accepts_expected_range() {
        assert_eq!(parse_env_range_value("0", 0..=8), Some(0));
        assert_eq!(parse_env_range_value("4", 0..=8), Some(4));
        assert_eq!(parse_env_range_value("8", 0..=8), Some(8));
        assert_eq!(parse_env_range_value("9", 0..=8), None);
        assert_eq!(parse_env_range_value("not-a-number", 0..=8), None);
    }

    #[test]
    fn fc_max_commands_env_accepts_expected_range() {
        assert_eq!(parse_env_range_value("1", 1..=8), Some(1));
        assert_eq!(parse_env_range_value("8", 1..=8), Some(8));
        assert_eq!(parse_env_range_value("0", 1..=8), None);
        assert_eq!(parse_env_range_value("9", 1..=8), None);
        assert_eq!(parse_env_range_value("not-a-number", 1..=8), None);
    }

    #[test]
    fn fc_max_turns_env_accepts_expected_range() {
        assert_eq!(parse_env_range_value("4", 4..=6), Some(4));
        assert_eq!(parse_env_range_value("6", 4..=6), Some(6));
        assert_eq!(parse_env_range_value("3", 4..=6), None);
        assert_eq!(parse_env_range_value("7", 4..=6), None);
        assert_eq!(parse_env_range_value("not-a-number", 4..=6), None);
    }

    #[test]
    fn fc_timeout_ms_env_accepts_expected_range() {
        assert_eq!(
            parse_env_u64_range_value("1000", 1_000..=300_000),
            Some(1_000)
        );
        assert_eq!(
            parse_env_u64_range_value("300000", 1_000..=300_000),
            Some(300_000)
        );
        assert_eq!(parse_env_u64_range_value("999", 1_000..=300_000), None);
        assert_eq!(parse_env_u64_range_value("300001", 1_000..=300_000), None);
        assert_eq!(
            parse_env_u64_range_value("not-a-number", 1_000..=300_000),
            None
        );
    }
}
