import tempfile
import time
from pathlib import Path
from typing import Any

from swegrep_cli.executor import ToolExecutor


def test_executor_paths() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        executor = ToolExecutor(tmpdir)

        # Test basic mapping
        assert executor._real("/codebase") == Path(tmpdir).resolve()
        assert (
            executor._real("/codebase/sub/file.py") == (Path(tmpdir) / "sub" / "file.py").resolve()
        )

        # Test traversal protection
        assert executor._real("/codebase/../../etc/passwd") == Path(tmpdir).resolve()
        assert executor._real("/codebase/sub/../../../etc/passwd") == Path(tmpdir).resolve()
        assert executor._real("/etc/passwd") == Path(tmpdir).resolve()


def test_readfile() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        executor = ToolExecutor(tmpdir)

        # Create a test file
        fp = Path(tmpdir) / "test.txt"
        lines = ["line1", "line2", "line3", "line4", "line5"]
        fp.write_text("\n".join(lines), encoding="utf-8")

        # Test reading full file
        res = executor.readfile("/codebase/test.txt")
        assert "1:line1\n2:line2\n3:line3\n4:line4\n5:line5" in res

        # Test range
        res_range = executor.readfile("/codebase/test.txt", start_line=2, end_line=4)
        assert "2:line2\n3:line3\n4:line4" in res_range
        assert "1:line1" not in res_range

        # Test missing file
        assert "Error: file not found" in executor.readfile("/codebase/nonexistent.txt")


def test_tree() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        executor = ToolExecutor(tmpdir)

        # Create a directory structure
        (Path(tmpdir) / "dir1").mkdir()
        (Path(tmpdir) / "dir1" / "file1.py").touch()
        (Path(tmpdir) / "dir2").mkdir()
        (Path(tmpdir) / "dir2" / "sub").mkdir()
        (Path(tmpdir) / "dir2" / "sub" / "file2.py").touch()
        (Path(tmpdir) / "file3.txt").touch()

        # Test tree generate
        res = executor.tree("/codebase", levels=2)
        assert "dir1" in res
        assert "file1.py" in res
        assert "dir2" in res
        assert "sub" in res
        assert "file3.txt" in res
        # Level 2 should hide sub/file2.py
        assert "file2.py" not in res


def test_tree_keeps_dotfiles_hidden_with_excludes() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        executor = ToolExecutor(tmpdir)

        (Path(tmpdir) / ".cache").mkdir()
        (Path(tmpdir) / "src").mkdir()

        res = executor.tree("/codebase", levels=1, exclude_paths=["dist"])

        assert ".cache" not in res
        assert "src" in res


def test_ls() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        executor = ToolExecutor(tmpdir)

        (Path(tmpdir) / "file1.txt").touch()
        (Path(tmpdir) / "dir1").mkdir()

        res = executor.ls("/codebase")
        assert "dir1\nfile1.txt" in res

        res_long = executor.ls("/codebase", long_format=True)
        assert "total 2" in res_long
        assert "dir1" in res_long
        assert "file1.txt" in res_long


def test_glob() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        executor = ToolExecutor(tmpdir)

        (Path(tmpdir) / "dir1").mkdir()
        (Path(tmpdir) / "dir1" / "test1.py").touch()
        (Path(tmpdir) / "dir2").mkdir()
        (Path(tmpdir) / "dir2" / "test2.py").touch()
        (Path(tmpdir) / "other.txt").touch()

        # Test recursive glob
        res = executor.glob("**/test*.py", "/codebase")
        assert "/codebase/dir1/test1.py" in res
        assert "/codebase/dir2/test2.py" in res
        assert "other.txt" not in res


def test_exec_tool_call() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        executor = ToolExecutor(tmpdir)

        fp = Path(tmpdir) / "test.txt"
        fp.write_text("hello world", encoding="utf-8")

        args = {"command1": {"type": "readfile", "file": "/codebase/test.txt"}}
        res = executor.exec_tool_call(args)
        assert "<command1_result>" in res
        assert "1:hello world" in res
        assert "</command1_result>" in res


def test_exec_tool_call_runs_commands_in_parallel() -> None:
    class SlowExecutor(ToolExecutor):
        def exec_command(self, cmd: dict[str, Any]) -> str:
            time.sleep(0.05)
            return str(cmd["value"])

    with tempfile.TemporaryDirectory() as tmpdir:
        executor = SlowExecutor(tmpdir)
        args = {f"command{i}": {"value": i} for i in range(1, 5)}

        started = time.perf_counter()
        res = executor.exec_tool_call(args)
        elapsed = time.perf_counter() - started

        assert elapsed < 0.15
        assert (
            res.index("<command1_result>\n1\n")
            < res.index("<command2_result>\n2\n")
            < res.index("<command3_result>\n3\n")
            < res.index("<command4_result>\n4\n")
        )
