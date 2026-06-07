---
name: swe-grep
description: Use swegrep-cli for semantic codebase search in local repositories. Best for broad natural-language questions about implementation paths, feature flow, and relevant files. Use ripgrep for exact keyword lookup.
metadata:
  short-description: Semantic codebase search with swegrep-cli
---

# SWE Grep

## When To Use

Use this skill when the user asks a codebase question that benefits from semantic or natural-language search, such as:
- Where a behavior, API, feature, command, or configuration is implemented.
- Which files are most relevant before making a change.
- How an authentication, request, parsing, execution, or persistence flow is wired.
- Where to continue investigation when exact identifiers are unknown.

## Binary

The packaged executable is located next to this skill file under:
- Linux: `./bin/swegrep-cli`
- Windows: `.\bin\swegrep-cli.exe`

## Command

### Codebase Search

Run codebase search from the target repository root:
```sh
./bin/swegrep-cli search "<natural language query>" --path <repo-path> --turns 4
```

**Parameters:**
* `<natural language query>`: A broad semantic question about the codebase.
* `--path`: Target repository root to search. Use an absolute path when possible.
* `--turns <4-6>`: Maximum search rounds. Use `4` by default for normal questions. Increase to `5` or `6` only when the task is broad, ambiguous, or likely to require deeper codebase traversal.

### Extract Key

When credentials are missing, try extracting and saving them:

```sh
./bin/swegrep-cli extract-key --save
```

**Parameters:**
* `--save`: Save the extracted key to the swegrep config.
* `--show`: Print the full key instead of a masked key.
* `--db-path <path>`: Read a specific Windsurf `state.vscdb` file instead of auto-detecting it.
