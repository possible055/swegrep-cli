# swegrep-cli

AI-powered Agentic Code Search CLI powered by Windsurf's SWE-grep protocol.

## Requirements

- Rust 1.96.0+
- `ripgrep` (`rg`) installed and available in `PATH`
- A logged-in Windsurf installation, or a Windsurf API key

## Install / Build

```bash
cargo build --release
./target/release/swegrep-cli --help
```

For local development:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## Windsurf API Key

The API key can be auto-discovered from your local Windsurf installation database:

| Platform | Path |
| --- | --- |
| macOS | `~/Library/Application Support/Windsurf/User/globalStorage/state.vscdb` |
| Windows | `%APPDATA%/Windsurf/User/globalStorage/state.vscdb` |
| Linux / WSL | `~/.config/Windsurf/User/globalStorage/state.vscdb` |

### Key Extraction Commands

- **Masked Key**: `swegrep-cli extract-key`
- **Save to Config**: `swegrep-cli extract-key --save` (saves to `~/.config/swegrep/config.json`)
- **Show Full Key**: `swegrep-cli extract-key --show`
- **Specify DB Path**: `swegrep-cli extract-key --db-path /path/to/state.vscdb`

### Key Resolution Priority

1. `--api-key` argument
2. `WINDSURF_API_KEY` environment variable
3. Saved config file (`config.json`)
4. Auto-discovery from the Windsurf database

## Search

```bash
# Basic usage
swegrep-cli search "where is authentication handled" --path /path/to/project

# With optional configurations
swegrep-cli search "query" --path . --api-key sk-ws-01-... --depth 4 --turns 3
```

### Path Filtering

Search uses the project root `.gitignore` by default when building the repo map and when running local `tree`, `rg`, and `glob` tools.

You can override it with global swegrep filters:

| File | Unix-like path | Windows path |
| --- | --- | --- |
| Include filter | `~/.config/swegrep/include.txt` | `~/.swegrep/include.txt` |
| Exclude filter | `~/.config/swegrep/exclude.txt` | `~/.swegrep/exclude.txt` |

Filter priority is:

```text
exclude.txt > include.txt > .gitignore
```

Patterns use gitignore-like syntax. Empty lines and `#` comments are ignored.

`rg` uses the same filter by default and receives the final visible file list from swegrep. Set `SWEGREP_PATH_FILTER=0` in `.env` to disable this global filtering policy and let `rg` traverse natively.

### Environment Variables

You can configure internal metadata or constraints through environment variables:

| Variable | Default | Description |
| --- | --- | --- |
| `WINDSURF_API_KEY` | none | Windsurf API key / JWT / Devin session token |
| `WS_APP_VER` | `1.48.2` | Windsurf app version metadata |
| `WS_LS_VER` | `1.9544.35` | Windsurf language server version metadata |
| `SWEGREP_PATH_FILTER` | `1` | Enable shared `.gitignore` / `include.txt` / `exclude.txt` filtering. Use `0`, `false`, `no`, or `off` to disable. |
| `FC_RESULT_MAX_LINES` | `50` | Max lines per local tool result |
| `FC_LINE_MAX_CHARS` | `250` | Max characters per local tool output line |
| `DEPTH` | `4` | Directory tree depth for initial repo map |
| `TURNS` | `3` | Maximum search rounds |
| `TIMEOUT` | `30000` | Streaming timeout in milliseconds |

## Caveats

- **Devin Session Tokens**: Under WSL or some environments, your Windsurf key might start with `devin-session-token$...`. Pass the **entire** string as your API key.
- **Account Limits**: Search capability and rate limits depend on your Windsurf subscription type (Free vs. Paid).
