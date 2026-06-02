# swegrep-cli

`swegrep-cli` is a Rust CLI for Windsurf Context subagent-compatible code search. It sends a natural-language query to `GetDevstralStream`, executes `restricted_exec` local search commands against your working tree, and returns the most relevant files and line ranges.

## Scope

- Implements a Windsurf Context subagent-compatible `restricted_exec` fast-context loop.
- Executes local equivalents of the official fast-context subcommands: `rg`, `readfile`, `tree`, `ls`, and `glob`.
- Returns compact file/range results for follow-up inspection.
- Does not aim to be a full Windsurf Cascade or ACP runtime clone.

## Requirements

- Rust 1.96.0 or newer
- `ripgrep` (`rg`) available in `PATH`
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

### Search a Repository

```bash
swegrep-cli search "where is authentication handled" --path /path/to/project
```

Optional flags:

```bash
swegrep-cli search "query" \
  --path . \
  --api-key sk-ws-01-... \
  --turns 3
```

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

## Path Filtering

Path filtering is shared by local search tools.

By default, visibility is determined by:

1. `exclude.txt`
2. `include.txt`
3. Project-root `.gitignore`

Additional behavior:

- Patterns use gitignore-style syntax.
- Empty lines and lines starting with `#` are ignored.
- Hidden dot-paths such as `.cache/` are excluded unless explicitly included.

Global filter files:

| File | macOS / Linux | Windows |
| --- | --- | --- |
| `include.txt` | `~/.config/swegrep/include.txt` | `~/.swegrep/include.txt` |
| `exclude.txt` | `~/.config/swegrep/exclude.txt` | `~/.swegrep/exclude.txt` |

Disable this shared filtering policy with:

```bash
export SWEGREP_PATH_FILTER=0
```

## Environment Variables

At startup, the CLI also loads an optional `.env` file from the same config directory as `config.json`:

- macOS / Linux: `~/.config/swegrep/.env`
- Windows: `~/.swegrep/.env`

The repo also includes [.env.example](/home/debian/projects/swegrep-cli/.env.example) as a tracked example. It is documentation only; the CLI does not read the repo-root `.env.example` or a repo-root `.env`.

Supported variables:

| Variable | Default | Description |
| --- | --- | --- |
| `WINDSURF_API_KEY` | none | Windsurf API key used by `search` |
| `WS_APP_VER` | `1.48.2` | Windsurf app version metadata |
| `WS_LS_VER` | `1.9544.35` | Windsurf language server version metadata |
| `SWEGREP_PATH_FILTER` | `1` | Enable shared path filtering; use `0`, `false`, `no`, or `off` to disable |
| `FC_RESULT_MAX_LINES` | `80` | Max lines for Windsurf-style non-`readfile` local tool results |
| `FC_READFILE_MAX_LINES` | `200` | Max lines for Windsurf-style `readfile` tool output |
| `FC_LINE_MAX_CHARS` | `300` | Max characters kept per line for Windsurf-style tool output |
| `TURNS` | `3` | Default maximum search rounds for `search` |
| `TIMEOUT` | `30000` | Streaming timeout in milliseconds |

Notes:

- In the config `.env`, `API_KEY` is accepted as an alias for `WINDSURF_API_KEY`.
- In the config `.env`, `TIMEOUT` is interpreted in seconds and converted to milliseconds before use.
- These three truncation variables apply to the Windsurf Context subagent-compatible tool path used by `search`.
- Legacy direct executor helpers keep their existing defaults: `readfile()` uses `50` lines and `250` chars per line unless explicitly overridden in code.

## Caveats

- Search quality, availability, and rate limits depend on Windsurf account access and server behavior.
- Some environments expose credentials as `devin-session-token$...`; pass the full string when using that format.
- This project does not implement ACP sessions, permission UI, diff zones, MCP, terminal streaming, embeddings, or workspace indexing.
