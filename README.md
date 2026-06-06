# swegrep-cli

English | [简体中文](README_zh.md)

`swegrep-cli` is a Rust CLI for Windsurf Context subagent-compatible code search. It sends a natural-language query to `GetDevstralStream`, executes `restricted_exec` local search commands against your working tree, and returns the most relevant files and line ranges.

## Scope

- Implements a Windsurf Context subagent-compatible `restricted_exec` fast-context loop.
- Executes local equivalents of the official fast-context subcommands: `rg`, `readfile`, `tree`, `ls`, and `glob`.
- Returns compact file/range results for follow-up inspection.

## Requirements

- A logged-in Windsurf installation or a valid Windsurf API key

## Build

```bash
cargo build --release
./target/release/swegrep-cli --help
```

## Development Checks

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## Usage

### Search Code

```bash
swegrep-cli search "where is authentication handled" --path /path/to/project
```

Optional flags:

```bash
swegrep-cli search "query" \
  --path /path/to/project \
  --api-key sk-ws-01-... \
  --turns 3
```

- `--path` is required and should point to the project root.
- `--turns` accepts values from `3` to `5`.

### Extract a Windsurf API Key

```bash
swegrep-cli extract-key
swegrep-cli extract-key --save
swegrep-cli extract-key --show
swegrep-cli extract-key --db-path /path/to/state.vscdb
```

`--save` writes the extracted key to:

- macOS / Linux: `~/.config/swegrep/config.json`
- Windows: `~/.swegrep/config.json`

## Authentication

`search` resolves credentials in this order:

1. `--api-key`
2. `WINDSURF_API_KEY`
3. Saved config file
4. Auto-discovered Windsurf database

Auto-discovery looks for `state.vscdb` in these locations:

| Platform | Path |
| --- | --- |
| macOS | `~/Library/Application Support/Windsurf/User/globalStorage/state.vscdb` |
| Windows | `%APPDATA%/Windsurf/User/globalStorage/state.vscdb` |
| Linux | `$XDG_CONFIG_HOME/Windsurf/User/globalStorage/state.vscdb` or `~/.config/Windsurf/User/globalStorage/state.vscdb` |
| WSL | First matching `/mnt/c/Users/*/AppData/Roaming/Windsurf/User/globalStorage/state.vscdb`, then the Linux fallback above |

Recent Devin builds use the same `state.vscdb` format under `devin/User/globalStorage`.
Auto-discovery checks the matching `devin` paths after the Windsurf paths.

## Path Filtering

Local search tools share the same path filtering policy.

By default, visibility is determined in this order:

1. `exclude.txt`
2. `include.txt`
3. Project-root `.gitignore`

Additional rules:

- Patterns use gitignore-style syntax.
- Empty lines and lines starting with `#` are ignored.
- Hidden dot-paths such as `.cache/` are excluded unless explicitly included.

Global filter files:

| File | Linux | Windows |
| --- | --- | --- |
| `include.txt` | `~/.config/swegrep/include.txt` | `~/.swegrep/include.txt` |
| `exclude.txt` | `~/.config/swegrep/exclude.txt` | `~/.swegrep/exclude.txt` |

Disable shared path filtering with:

```bash
export SWEGREP_PATH_FILTER=0
```

## Environment Variables

At startup, the CLI optionally loads a `.env` file from the same config directory as `config.json`:

- macOS / Linux: `~/.config/swegrep/.env`
- Windows: `~/.swegrep/.env`

Supported variables:

| Variable | Default | Description |
| --- | --- | --- |
| `WINDSURF_API_KEY` | none | Windsurf API key used by `search` |
| `SWEGREP_RG_PATH` | none | Custom `rg` binary path; overrides bundled `rg` and `PATH` |
| `WS_APP_VER` | `1.48.2` | Windsurf app version metadata |
| `WS_LS_VER` | `1.9544.35` | Windsurf language server version metadata |
| `SWEGREP_PATH_FILTER` | `1` | Enable shared path filtering; use `0`, `false`, `no`, or `off` to disable |
| `FC_MAX_COMMANDS` | `8` | Max parallel restricted commands per search round |
| `FC_RESULT_MAX_LINES` | `80` | Max lines for Windsurf-style non-`readfile` local tool results |
| `FC_READFILE_MAX_LINES` | `200` | Max lines for Windsurf-style `readfile` tool output |
| `FC_LINE_MAX_CHARS` | `300` | Max characters kept per line for Windsurf-style tool output |
| `FC_MAX_TURNS` | `3` | Default maximum search rounds for `search` |
| `FC_TIMEOUT_MS` | `30000` | Streaming timeout in milliseconds |

## Acknowledgements

This project was inspired by and built upon ideas from the following projects:

- [fast-context-mcp](https://github.com/SammySnake-d/fast-context-mcp)
- [fast-context-skill](https://github.com/oulkurt/fast-context-skill)
