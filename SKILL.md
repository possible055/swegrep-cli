# swegrep-cli Skill Configuration Guide

This CLI is designed to be packaged and executed as an agentic AI skill. To customize behavior when called by tools or systems, you can place environment and exclusion configuration files inside the native `swegrep` configuration directory.

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

# Optional: Model selection identifier
MODEL=gpt-5.5
```

---

## 2. Global Path Exclusions (`exclude.txt`)

You can globally exclude certain file names or directory paths from the initial repository map traversal.

### Path
- Unix-like: `~/.config/swegrep/exclude.txt`
- Windows: `~/.swegrep/exclude.txt`

### Example Content
```text
# Exclude Python build directories and cache
dist
build
__pycache__
*.pyc

# Exclude packages
node_modules
.venv
.git
```

Each non-empty line (that does not start with `#`) will be parsed as a glob pattern and filtered out during the directory tree generation phase.
