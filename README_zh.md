# swegrep-cli

[English](README.md) | 简体中文

`swegrep-cli` 是一个用于 Windsurf Context 子代理（subagent）兼容代码搜索的 Rust 命令行工具（CLI）。它向 `GetDevstralStream` 发送自然语言查询，在您的工作树（working tree）中执行 `restricted_exec` 本地搜索命令，并返回最相关的 文件 和 行范围。

## 范围

- 实现了兼容 Windsurf Context 子代理的 `restricted_exec` fast-context 循环。
- 执行与官方 fast-context 子命令等效的本地命令：`rg`、`readfile`、`tree`、`ls` 和 `glob`。
- 返回紧凑的文件/范围结果，以便后续检查。

## 要求

- 已登录的 Windsurf 客户端或有效的 Windsurf API 密钥

## 构建

```bash
cargo build --release
./target/release/swegrep-cli --help
```

## 开发检查

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## 使用方法

### 搜索代码

```bash
swegrep-cli search "where is authentication handled" --path /path/to/project
```

可选参数：

```bash
swegrep-cli search "query" \
  --path /path/to/project \
  --api-key sk-ws-01-... \
  --turns 4
```

- `--path` 是必填项，应指向项目根目录。
- `--turns` 接受 `4` 到 `6` 之间的值。

### 提取 Windsurf API 密钥

```bash
swegrep-cli extract-key
swegrep-cli extract-key --save
swegrep-cli extract-key --show
swegrep-cli extract-key --db-path /path/to/state.vscdb
```

`--save` 将提取的密钥写入：

- macOS / Linux: `~/.config/swegrep/config.json`
- Windows: `~/.swegrep/config.json`

## 身份验证

`search` 按以下顺序解析凭据：

1. `--api-key`
2. `WINDSURF_API_KEY`
3. 保存的配置文件
4. 自动发现的 Windsurf 数据库

自动发现会在以下位置寻找 `state.vscdb`：

| 平台 | 路径 |
| --- | --- |
| macOS | `~/Library/Application Support/Windsurf/User/globalStorage/state.vscdb` |
| Windows | `%APPDATA%/Windsurf/User/globalStorage/state.vscdb` |
| Linux | `$XDG_CONFIG_HOME/Windsurf/User/globalStorage/state.vscdb` 或 `~/.config/Windsurf/User/globalStorage/state.vscdb` |
| WSL | 首先匹配 `/mnt/c/Users/*/AppData/Roaming/Windsurf/User/globalStorage/state.vscdb`，然后是上述 Linux 备选路径 |

最近的 Devin 构建版本在 `devin/User/globalStorage` 下使用相同的 `state.vscdb` 格式。
自动发现会在检查 Windsurf 路径后检查匹配 of `devin` 路径。

## 路径过滤

本地搜索工具共享相同的路径过滤策略。

默认情况下，可见性按以下顺序决定：

1. `exclude.txt`
2. `include.txt`
3. 项目根目录的 `.gitignore`

附加规则：

- 模式使用 gitignore 风格的语法。
- 忽略空行和以 `#` 开头的行。
- 排除隐藏的点路径（例如 `.cache/`），除非显式包含。

全局过滤文件：

| 文件 | Linux | Windows |
| --- | --- | --- |
| `include.txt` | `~/.config/swegrep/include.txt` | `~/.swegrep/include.txt` |
| `exclude.txt` | `~/.config/swegrep/exclude.txt` | `~/.swegrep/exclude.txt` |

通过以下方式禁用共享路径过滤：

```bash
export SWEGREP_PATH_FILTER=0
```

## 环境变量

启动时，CLI 可以选择从与 `config.json` 相同的配置目录中加载 `.env` 文件：

- macOS / Linux: `~/.config/swegrep/.env`
- Windows: `~/.swegrep/.env`

支持的变量：

| 变量 | 默认值 | 描述 |
| --- | --- | --- |
| `WINDSURF_API_KEY` | 无 | `search` 使用的 Windsurf API 密钥 |
| `SWEGREP_RG_PATH` | 无 | 自定义 `rg` 二进制文件路径；覆盖打包的 `rg` 和 `PATH` |
| `WS_APP_VER` | `1.48.2` | Windsurf 应用版本元数据 |
| `WS_LS_VER` | `1.9544.35` | Windsurf 语言 server 版本元数据 |
| `SWEGREP_PATH_FILTER` | `1` | 启用共享路径过滤；使用 `0`、`false`、`no` 或 `off` 禁用 |
| `FC_MAX_COMMANDS` | `8` | 每一轮搜索的最大并行受限命令数 |
| `FC_RESULT_MAX_LINES` | `80` | Windsurf 风格非 `readfile` 本地工具结果的最大行数 |
| `FC_READFILE_MAX_LINES` | `200` | Windsurf 风格 `readfile` 工具输出的最大行数 |
| `FC_LINE_MAX_CHARS` | `300` | Windsurf 风格工具输出中每行保留的最大字符数 |
| `FC_MAX_TURNS` | `4` | `search` 的默认最大搜索轮数 |
| `FC_TIMEOUT_MS` | `30000` | 流式传输超时（毫秒） |
| `SWEGREP_DEBUG` | `0` | 设为 `1`、`true`、`yes` 或 `on` 时启用模型回应诊断 |
| `SWEGREP_DEBUG_LOG` | `~/.config/swegrep/debug.log` | 已解析模型回应与 final answer XML 的 debug log 路径 |

## 致谢

本项目受到以下项目的启发并基于其理念构建：

- [fast-context-mcp](https://github.com/SammySnake-d/fast-context-mcp)
- [fast-context-skill](https://github.com/oulkurt/fast-context-skill)
