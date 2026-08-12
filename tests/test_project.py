"""Project detection and repo map tests (plan B4)."""

from __future__ import annotations

import json
from pathlib import Path

from overseer.project import detect_project, repo_map


def _write(tmp_path: Path, rel: str, content: str = "") -> Path:
    p = tmp_path / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(content, encoding="utf-8")
    return p


def test_detect_python_uv(tmp_path):
    _write(
        tmp_path,
        "pyproject.toml",
        '[project]\nname = "demo"\ndependencies = ["fastapi>=0.100"]\n'
        "[tool.uv]\n[tool.pytest.ini_options]\n[tool.ruff]\n[tool.mypy]\n",
    )
    ctx = detect_project(tmp_path)
    assert ctx.language == "python"
    assert ctx.package_manager == "uv"
    assert ctx.framework == "fastapi"
    assert ctx.test_runner == "pytest"
    assert ctx.linter == "ruff"
    assert ctx.typechecker == "mypy"
    assert "uv run pytest -q" in ctx.commands["test"]


def test_detect_python_plain(tmp_path):
    _write(tmp_path, "requirements.txt", "flask\n")
    ctx = detect_project(tmp_path)
    assert ctx.language == "python"
    assert ctx.package_manager == "pip"
    assert ctx.test_runner == "pytest"
    assert ctx.commands["test"] == "pytest -q"


def test_detect_node_vitest(tmp_path):
    pkg = {
        "name": "demo",
        "scripts": {"test": "vitest run", "lint": "eslint ."},
        "devDependencies": {"vitest": "^1.0", "eslint": "^8.0", "typescript": "^5.0"},
        "dependencies": {"next": "^14.0"},
    }
    _write(tmp_path, "package.json", json.dumps(pkg))
    _write(tmp_path, "pnpm-lock.yaml", "")
    ctx = detect_project(tmp_path)
    assert ctx.language == "javascript"
    assert ctx.package_manager == "pnpm"
    assert ctx.framework == "next"
    assert ctx.test_runner == "vitest"
    assert ctx.linter == "eslint"
    assert ctx.typechecker == "tsc"


def test_detect_rust(tmp_path):
    _write(tmp_path, "Cargo.toml", '[package]\nname = "demo"\n')
    ctx = detect_project(tmp_path)
    assert ctx.language == "rust"
    assert ctx.test_runner == "cargo test"
    assert ctx.commands["test"] == "cargo test"


def test_detect_go(tmp_path):
    _write(tmp_path, "go.mod", "module demo\n")
    ctx = detect_project(tmp_path)
    assert ctx.language == "go"
    assert ctx.commands["test"] == "go test ./..."


def test_detect_unknown(tmp_path):
    _write(tmp_path, "notes.txt", "hello")
    ctx = detect_project(tmp_path)
    assert ctx.language == "unknown"
    assert ctx.commands == {}


def test_repo_map_basic(tmp_path):
    _write(tmp_path, "src/overseer/agent.py", "x = 1\n")
    _write(tmp_path, "src/overseer/cli.py", "y = 2\n")
    _write(tmp_path, "README.md", "# demo\n")
    _write(tmp_path, "tests/test_agent.py", "def test_x(): pass\n")
    _write(tmp_path, ".venv/lib/python3.11/site-packages/pkg/mod.py", "z\n")
    _write(tmp_path, "node_modules/pkg/index.js", "q\n")
    m = repo_map(tmp_path, use_cache=False)
    assert m.total_files == 4  # .venv and node_modules pruned
    assert "README.md" in m.key_files
    assert "src/overseer/agent.py" in m.files
    assert ".py" in m.extensions
    assert "src/overseer" in m.dirs


def test_repo_map_cache(tmp_path):
    _write(tmp_path, "a.py", "x\n")
    m1 = repo_map(tmp_path, use_cache=True)
    m2 = repo_map(tmp_path, use_cache=True)
    assert m1.total_files == m2.total_files
    # Cache file exists and is reused (same signature).
    cache = tmp_path / ".overseer" / "cache" / "repo_map.json"
    assert cache.is_file()
    # Adding a file invalidates the cache.
    _write(tmp_path, "b.py", "y\n")
    m3 = repo_map(tmp_path, use_cache=True)
    assert m3.total_files == 2


def test_repo_map_skips_secrets(tmp_path):
    _write(tmp_path, ".env", "API_KEY=sk-ant-secret1234567890abcdefghijklmnop\n")
    _write(tmp_path, "main.py", "print(1)\n")
    m = repo_map(tmp_path, use_cache=False)
    # .env is not a KEY_FILE and its content is never read anyway.
    assert ".env" not in m.key_files
    assert m.total_files == 2


def test_project_summary(tmp_path):
    _write(tmp_path, "pyproject.toml", '[project]\nname = "demo"\n')
    ctx = detect_project(tmp_path)
    s = ctx.summary()
    assert "python" in s
    assert "demo" in s
