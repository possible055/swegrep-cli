import fnmatch
import os
import subprocess
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any


class ToolExecutor:
    def __init__(
        self,
        project_root: str,
        result_max_lines: int | None = None,
        line_max_chars: int | None = None,
    ) -> None:
        self.root = Path(project_root).resolve()
        self.collected_rg_patterns: list[str] = []

        self.result_max_lines = self._bounded_int(
            result_max_lines, self._read_int_env("FC_RESULT_MAX_LINES", 50, 1, 500), 1, 500
        )
        self.line_max_chars = self._bounded_int(
            line_max_chars, self._read_int_env("FC_LINE_MAX_CHARS", 250, 20, 10000), 20, 10000
        )

    def _bounded_int(
        self, value: int | None, default: int, min_val: int, max_val: int
    ) -> int:
        if value is None:
            return default
        return max(min_val, min(max_val, value))

    def _read_int_env(self, name: str, default: int, min_val: int, max_val: int) -> int:
        raw = os.environ.get(name)
        if raw is None:
            return default
        try:
            val = int(raw)
            return max(min_val, min(max_val, val))
        except ValueError:
            return default

    def _real(self, virtual: str) -> Path:
        if not isinstance(virtual, str) or not virtual:
            return self.root

        # Normalize virtual codebase paths
        if virtual.startswith("/codebase") or virtual.startswith("\\codebase"):
            rel = virtual[len("/codebase") :].lstrip("/\\")
            full_path = Path(self.root / rel)
        else:
            full_path = Path(virtual)

        try:
            # Resolve to absolute path to check path traversal
            resolved = full_path.resolve()
            resolved.relative_to(self.root)
            return resolved
        except (ValueError, RuntimeError):
            # Safe fallback: clamp to root if outside
            return self.root

    def _remap(self, text: str) -> str:
        # Map real path back to /codebase
        root_str = str(self.root)
        # Handle backward slash windows mapping if needed
        root_slash = self.root.as_posix()
        text = text.replace(root_str, "/codebase")
        if root_str != root_slash:
            text = text.replace(root_slash, "/codebase")
        return text

    def _truncate(self, text: str) -> str:
        lines = text.split("\n")
        limit = min(len(lines), self.result_max_lines)
        truncated_lines: list[str] = []

        for i in range(limit):
            line = lines[i]
            if len(line) > self.line_max_chars:
                truncated_lines.append(line[: self.line_max_chars])
            else:
                truncated_lines.append(line)

        result = "\n".join(truncated_lines)
        if len(lines) > self.result_max_lines:
            result += "\n... (lines truncated) ..."
        return result

    def rg(
        self,
        pattern: str,
        path: str,
        include: list[str] | None = None,
        exclude: list[str] | None = None,
    ) -> str:
        if not pattern or not isinstance(pattern, str):
            return "Error: missing or invalid pattern"
        if not path or not isinstance(path, str):
            return "Error: missing or invalid path"

        self.collected_rg_patterns.append(pattern)
        rp = self._real(path)
        if not rp.exists():
            return f"Error: path does not exist: {path}"

        args = ["rg", "--no-heading", "-n", "--max-count", "50", pattern, str(rp)]
        if include:
            for g in include:
                args.extend(["--glob", g])
        if exclude:
            for g in exclude:
                args.extend(["--glob", f"!{g}"])

        try:
            res = subprocess.run(
                args,
                capture_output=True,
                text=True,
                timeout=30,
                env={**os.environ, "RIPGREP_CONFIG_PATH": ""},
            )
            if res.returncode == 1:
                return "(no matches)"
            if res.returncode == 0:
                return self._truncate(self._remap(res.stdout or "(no matches)"))
            if res.stderr:
                return self._truncate(self._remap(res.stderr))
            return f"Error: exit status {res.returncode}"
        except FileNotFoundError:
            return "Error: ripgrep ('rg') is not installed or not in PATH."
        except subprocess.TimeoutExpired:
            return "Error: ripgrep command timed out"
        except Exception as e:
            return f"Error: {e}"

    def readfile(
        self, file: str, start_line: int | None = None, end_line: int | None = None
    ) -> str:
        if not file or not isinstance(file, str):
            return "Error: missing or invalid file path"
        rp = self._real(file)
        if not rp.is_file():
            return f"Error: file not found: {file}"

        try:
            content = rp.read_text(encoding="utf-8", errors="ignore")
        except Exception as e:
            return f"Error: {e}"

        all_lines = content.split("\n")
        s = (start_line or 1) - 1
        end = end_line or len(all_lines)

        selected = all_lines[s:end]
        out_lines = [f"{s + idx + 1}:{line}" for idx, line in enumerate(selected)]
        out = "\n".join(out_lines)
        return self._truncate(out)

    def tree(
        self,
        path: str,
        levels: int | None = None,
        exclude_paths: list[str] | None = None,
        *,
        truncate: bool = True,
    ) -> str:
        if not path or not isinstance(path, str):
            return "Error: missing or invalid path"
        rp = self._real(path)
        if not rp.is_dir():
            return f"Error: dir not found: {path}"

        lines = [path]
        tree_lines = self._generate_tree_lines(rp, max_depth=levels, exclude_patterns=exclude_paths)
        lines.extend(tree_lines)
        stdout = "\n".join(lines)
        remapped = self._remap(stdout)
        return self._truncate(remapped) if truncate else remapped

    def _generate_tree_lines(
        self,
        dir_path: Path,
        max_depth: int | None = None,
        current_depth: int = 1,
        exclude_patterns: list[str] | None = None,
    ) -> list[str]:
        if max_depth is not None and current_depth > max_depth:
            return []

        lines: list[str] = []
        try:
            items = sorted(dir_path.iterdir(), key=lambda x: (not x.is_dir(), x.name.lower()))
        except Exception:
            return []

        filtered_items: list[Path] = []
        for item in items:
            if item.name.startswith("."):
                continue

            is_excluded = False
            if exclude_patterns:
                # Match path name or relative path
                rel_path = item.relative_to(self.root).as_posix()
                for pat in exclude_patterns:
                    if fnmatch.fnmatch(item.name, pat) or fnmatch.fnmatch(rel_path, pat):
                        is_excluded = True
                        break
            if not is_excluded:
                filtered_items.append(item)

        num_items = len(filtered_items)
        for index, item in enumerate(filtered_items):
            is_last = index == num_items - 1
            prefix = "└── " if is_last else "├── "
            lines.append(f"{prefix}{item.name}")

            if item.is_dir():
                sub_lines = self._generate_tree_lines(
                    item, max_depth, current_depth + 1, exclude_patterns
                )
                indent = "    " if is_last else "│   "
                for sub_line in sub_lines:
                    lines.append(f"{indent}{sub_line}")

        return lines

    def ls(self, path: str, long_format: bool = False, all_files: bool = False) -> str:
        if not path or not isinstance(path, str):
            return "Error: missing or invalid path"
        rp = self._real(path)
        if not rp.is_dir():
            return f"Error: dir not found: {path}"

        try:
            entries = sorted(os.listdir(rp))
        except Exception as e:
            return f"Error: {e}"

        if not all_files:
            entries = [e for e in entries if not e.startswith(".")]

        if not long_format:
            return self._truncate("\n".join(entries))

        lines = [f"total {len(entries)}"]
        for name in entries:
            fp = rp / name
            try:
                st = fp.stat()
                is_dir = fp.is_dir()
                type_char = "d" if is_dir else "-"
                perm = "rwxr-xr-x"
                size = str(st.st_size).rjust(8)

                # Emulate ls -l datetime
                from datetime import datetime

                mtime = datetime.fromtimestamp(st.st_mtime)
                date_str = mtime.strftime("%b %d %H:%M")

                lines.append(f"{type_char}{perm}  1 user  staff {size} {date_str} {name}")
            except Exception:
                lines.append(f"?---------  ? ?     ?        ? ? ?     ? {name}")

        return self._truncate(self._remap("\n".join(lines)))

    def glob(self, pattern: str, path: str, type_filter: str = "all") -> str:
        if not pattern or not isinstance(pattern, str):
            return "Error: missing or invalid pattern"
        if not path or not isinstance(path, str):
            return "Error: missing or invalid path"

        rp = self._real(path)
        if not rp.is_dir():
            return f"Error: dir not found: {path}"

        matches: list[str] = []
        try:
            if "**" in pattern:
                # Strip leading **/ if present
                clean_pat = pattern[3:] if pattern.startswith("**/") else pattern
                items = rp.rglob(clean_pat)
            else:
                items = rp.glob(pattern)

            for item in items:
                if type_filter == "file" and not item.is_file():
                    continue
                if type_filter == "directory" and not item.is_dir():
                    continue
                matches.append(str(item))
        except Exception:
            # Fallback simple listdir matching
            try:
                for entry in os.listdir(rp):
                    if fnmatch.fnmatch(entry, pattern):
                        fp = rp / entry
                        if type_filter == "file" and not fp.is_file():
                            continue
                        if type_filter == "directory" and not fp.is_dir():
                            continue
                        matches.append(str(fp))
            except Exception:
                pass

        matches.sort()
        matches = matches[:100]
        out = "\n".join(self._remap(m) for m in matches)
        return out or "(no matches)"

    def exec_command(self, cmd: dict[str, Any]) -> str:
        if not cmd or not isinstance(cmd, dict):
            return "Error: missing or invalid command"
        t = cmd.get("type", "")
        if t == "rg":
            return self.rg(
                cmd.get("pattern", ""), cmd.get("path", ""), cmd.get("include"), cmd.get("exclude")
            )
        elif t == "readfile":
            return self.readfile(cmd.get("file", ""), cmd.get("start_line"), cmd.get("end_line"))
        elif t == "tree":
            return self.tree(cmd.get("path", ""), cmd.get("levels"))
        elif t == "ls":
            return self.ls(
                cmd.get("path", ""), cmd.get("long_format", False), cmd.get("all", False)
            )
        elif t == "glob":
            return self.glob(
                cmd.get("pattern", ""), cmd.get("path", ""), cmd.get("type_filter", "all")
            )
        else:
            return f"Error: unknown command type '{t}'"

    def exec_tool_call(self, args: dict[str, Any]) -> str:
        if not args or not isinstance(args, dict):
            return "Error: missing or invalid tool args"
        keys = sorted(
            [k for k in args.keys() if k.startswith("command")],
            key=lambda key: int(key.removeprefix("command"))
            if key.removeprefix("command").isdigit()
            else 9999,
        )
        if not keys:
            return ""
        max_workers = min(len(keys), 8)
        with ThreadPoolExecutor(max_workers=max_workers) as pool:
            outputs = list(pool.map(lambda key: self.exec_command(args[key]), keys))
        return "".join(
            f"<{key}_result>\n{output}\n</{key}_result>"
            for key, output in zip(keys, outputs, strict=True)
        )
