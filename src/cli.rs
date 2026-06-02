use crate::core::{SearchOptions, search};
use crate::credentials::{self, extract_key, mask_api_key, save_cached_api_key};
use clap::{Args, Parser, Subcommand};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

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

    #[arg(
        long,
        default_value = ".",
        help = "Absolute or relative path to project root. Default is current directory."
    )]
    path: PathBuf,

    #[arg(
        long,
        value_parser = parse_depth,
        help = "Directory tree depth for initial repo map (3-6). Default is from DEPTH or 4."
    )]
    depth: Option<usize>,

    #[arg(
        long,
        value_parser = parse_turns,
        help = "Maximum search rounds. Default is from TURNS or 3."
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
    let default_depth = read_env_range("DEPTH", 4, 3..=6);
    let default_turns = read_env_range("TURNS", 3, 3..=5);

    let cli = Cli::parse();
    if !command_exists("rg") {
        eprintln!("Error: ripgrep ('rg') is required but was not found in PATH.");
        return 1;
    }

    match cli.command {
        Commands::ExtractKey(args) => run_extract_key(args),
        Commands::Search(args) => run_search(args, default_depth, default_turns),
    }
}

pub fn load_skill_env() {
    let env_path = credentials::get_config_path()
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
        } else if key == "TIMEOUT" {
            if let Ok(seconds) = value.parse::<f64>() {
                set_env_var("TIMEOUT", &(seconds * 1000.0).trunc().to_string());
            }
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
    env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| range.contains(value))
        .unwrap_or(default)
}

fn parse_depth(value: &str) -> Result<usize, String> {
    parse_range(value, 3..=6, "depth")
}

fn parse_turns(value: &str) -> Result<usize, String> {
    parse_range(value, 3..=5, "turns")
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

fn run_search(args: SearchArgs, default_depth: usize, default_turns: usize) -> i32 {
    let project_path = absolute_path(&args.path);
    if !project_path.is_dir() {
        eprintln!(
            "Error: Project path does not exist: {}",
            project_path.display()
        );
        return 1;
    }

    let exclude_patterns = read_exclude_patterns();
    let mut options = SearchOptions::new(args.query, project_path);
    options.api_key = args.api_key;
    options.max_turns = args.turns.unwrap_or(default_turns);
    options.tree_depth = args.depth.unwrap_or(default_depth);
    options.exclude_paths = exclude_patterns;

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

    if result.files.is_empty() {
        println!("No relevant files found.");
        return 0;
    }

    println!("\nFound {} relevant files:\n", result.files.len());
    for (idx, entry) in result.files.iter().enumerate() {
        let ranges = entry
            .ranges
            .iter()
            .map(|(start, end)| format!("L{start}-{end}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "  [{}/{}] {} ({ranges})",
            idx + 1,
            result.files.len(),
            entry.full_path
        );
    }

    let mut seen = HashSet::new();
    let patterns = result
        .rg_patterns
        .into_iter()
        .filter(|pattern| pattern.len() >= 3)
        .filter(|pattern| seen.insert(pattern.clone()))
        .collect::<Vec<_>>();
    if !patterns.is_empty() {
        println!("\ngrep keywords: {}", patterns.join(", "));
    }

    0
}

fn read_exclude_patterns() -> Vec<String> {
    let Some(config_dir) = credentials::get_config_path()
        .parent()
        .map(Path::to_path_buf)
    else {
        return Vec::new();
    };
    let exclude_file = config_dir.join("exclude.txt");
    let Ok(text) = fs::read_to_string(exclude_file) else {
        return Vec::new();
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

fn command_exists(command: &str) -> bool {
    let Some(paths) = env::var_os("PATH") else {
        return false;
    };

    for dir in env::split_paths(&paths) {
        let candidate = dir.join(command);
        if candidate.is_file() {
            return true;
        }
        if cfg!(target_os = "windows") {
            let extensions = env::var_os("PATHEXT")
                .map(|value| {
                    value
                        .to_string_lossy()
                        .split(';')
                        .map(ToOwned::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| {
                    vec![".exe".to_string(), ".bat".to_string(), ".cmd".to_string()]
                });
            for ext in extensions {
                if dir.join(format!("{command}{ext}")).is_file() {
                    return true;
                }
            }
        }
    }
    false
}
