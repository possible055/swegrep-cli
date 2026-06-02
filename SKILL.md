# swegrep-cli Skill Configuration Guide

This Rust CLI is designed to be packaged and executed as an agentic AI skill. Build it with `cargo build --release`, then expose `target/release/swegrep-cli` to the caller. To customize behavior when called by tools or systems, place environment and path filtering configuration files inside the native `swegrep` configuration directory.

## Configuration Paths

Depending on your platform, config files are read from:

- **Linux / macOS / WSL**: `~/.config/swegrep/`
- **Windows**: `~/.swegrep/`

---

## 1. Environment Variables Configuration (`.env`)

Before using the tool as a skill, create an environment configuration file:

### Path
- Unix-like: `~/.config/swegrep/.env`
- Windows: `~/.swegrep/.env`

### Example Content
```ini
# Windsurf API Key or Devin session token
API_KEY=devin-session-token$eyJhbGciOiJIUzI1...

# Streaming connection timeout in seconds (default is 30)
TIMEOUT=120

# Directory tree depth limit for initial repo map (3-6, default is 4)
DEPTH=4

# Maximum search execution rounds/turns (3-5, default is 3)
TURNS=3
```

---

## 2. Global Path Filtering

The project root `.gitignore` is read by default. Global swegrep filters can override it for the repo map and local `tree`, `rg`, and `glob` tools.

Priority:

```text
exclude.txt > include.txt > .gitignore
```

### Include Path
- Unix-like: `~/.config/swegrep/include.txt`
- Windows: `~/.swegrep/include.txt`

### Exclude Path
- Unix-like: `~/.config/swegrep/exclude.txt`
- Windows: `~/.swegrep/exclude.txt`

### Example `include.txt`
```text
# Re-include specific ignored files or directories
target/generated-schema.json
.config/visible.txt
```

### Example `exclude.txt`
```text
# Exclude build directories and cache
dist
build
target

# Exclude packages
node_modules
.git
```

Each non-empty line that does not start with `#` is parsed as a gitignore-like pattern.

`rg` uses this shared filter by default. Set `SWEGREP_PATH_FILTER=0` in `.env` to disable the global filtering policy and let `rg` traverse paths natively when comparing behavior or diagnosing filter issues.
