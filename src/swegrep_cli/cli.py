import argparse
import asyncio
import shutil
import sys
from pathlib import Path
from typing import Any

from swegrep_cli.core import search
from swegrep_cli.credentials import (
    extract_key,
    mask_api_key,
    save_cached_api_key,
)


def _require_rg() -> None:
    if shutil.which("rg") is None:
        print("Error: ripgrep ('rg') is required but was not found in PATH.", file=sys.stderr)
        sys.exit(1)


class CustomArgumentParser(argparse.ArgumentParser):
    def __init__(self, *args: Any, **kwargs: Any) -> None:
        kwargs["add_help"] = False
        super().__init__(*args, **kwargs)
        self.add_argument("-h", "--help", action="help", help="")

    def format_help(self) -> str:
        raw_help = super().format_help()
        raw_help = raw_help.replace("positional arguments:", "arguments:")
        lines = raw_help.splitlines()
        new_lines = []
        for line in lines:
            stripped = line.strip()
            if stripped.startswith("{") and stripped.endswith("}"):
                continue
            if line.startswith("    ") and not line.startswith("     "):
                line = "  " + line[4:]
            new_lines.append(line.rstrip())
        return "\n".join(new_lines) + "\n"


def main() -> None:
    parser = CustomArgumentParser(prog="swegrep-cli")
    subparsers = parser.add_subparsers(
        dest="command", required=True, parser_class=CustomArgumentParser
    )

    # Subcommand: search
    search_parser = subparsers.add_parser("search", help="Execute semantic code search")
    search_parser.add_argument("query", help="Natural language search query")
    search_parser.add_argument("--api-key", help="Windsurf API key. Overrides env and config.")

    search_parser.add_argument(
        "--path",
        default=".",
        help="Absolute or relative path to project root. Default is current directory.",
    )
    search_parser.add_argument(
        "--depth",
        type=int,
        default=4,
        choices=range(3, 7),
        help="Directory tree depth for initial repo map (3-6). Default is 4.",
    )
    search_parser.add_argument(
        "--turns",
        type=int,
        default=3,
        choices=range(3, 6),
        help="Maximum search rounds. Default is 3.",
    )

    # Subcommand: extract-key
    key_parser = subparsers.add_parser("extract-key", help="Extract Windsurf API key from local database")
    key_parser.add_argument("--db-path", help="Path to Windsurf state.vscdb. Default is auto-detect.")
    key_parser.add_argument("--save", action="store_true", help="Save extracted key to swegrep config.")
    key_parser.add_argument("--show", action="store_true", help="Print the full key instead of a masked key.")

    args = parser.parse_args()
    _require_rg()

    if args.command == "extract-key":
        result = extract_key(args.db_path)
        if "error" in result:
            print(f"Error: {result['error']}", file=sys.stderr)
            if "hint" in result:
                print(f"Hint: {result['hint']}", file=sys.stderr)
            sys.exit(1)

        key = result["api_key"]
        if args.save:
            config_path = save_cached_api_key(key)
            print(f"Saved Windsurf API key to {config_path}", file=sys.stderr)
        print(f"Windsurf API Key: {key if args.show else mask_api_key(key)}")
        if "key_type" in result:
            print(f"Key type: {result['key_type']}", file=sys.stderr)
        print(f"Source DB: {result['db_path']}", file=sys.stderr)

        if args.show:
            print("\nRun the following command to set the env var:")
            print(f'  export WINDSURF_API_KEY="{key}"')
        sys.exit(0)

    elif args.command == "search":
        project_path = str(Path(args.path).resolve())
        if not Path(project_path).is_dir():
            print(f"Error: Project path does not exist: {project_path}", file=sys.stderr)
            sys.exit(1)

        # Progress reporting helper writing to stderr
        def progress_callback(msg: str) -> None:
            print(f"[fast-context] {msg}", file=sys.stderr, flush=True)



        async def run_search() -> int:
            try:
                # We call core search with on_progress callback
                res = await search(
                    query=args.query,
                    project_root=project_path,
                    api_key=args.api_key,
                    max_turns=args.turns,
                    tree_depth=args.depth,
                    on_progress=progress_callback,
                )

                if "error" in res:
                    print(f"Search failed: {res['error']}", file=sys.stderr)
                    return 1

                files = res.get("files", [])
                if not files:
                    print("No relevant files found.")
                    return 0

                print(f"\nFound {len(files)} relevant files:\n")
                for idx, entry in enumerate(files):
                    ranges_str = ", ".join(f"L{r[0]}-{r[1]}" for r in entry["ranges"])
                    print(f"  [{idx + 1}/{len(files)}] {entry['full_path']} ({ranges_str})")

                rg_patterns = res.get("rg_patterns", [])
                unique_patterns = [p for p in dict.fromkeys(rg_patterns) if len(p) >= 3]
                if unique_patterns:
                    print(f"\ngrep keywords: {', '.join(unique_patterns)}")
                return 0

            except Exception as e:
                print(f"Unexpected error: {e}", file=sys.stderr)
                return 1

        sys.exit(asyncio.run(run_search()))


if __name__ == "__main__":
    main()
