"""Project detection and repo map generation (plan B4).

detect_project() reads the standard manifest files (pyproject.toml,
package.json, Cargo.toml, go.mod, Makefile) and produces a ProjectContext
with the detected language, framework, package manager, test runner,
linter, typechecker, and build system, plus the concrete commands to run
them.

repo_map() produces a lightweight map of the repository: directory tree,
file extensions, and key files. It never reads file contents. The map is
cached under .overseer/cache/ keyed by a hash of the root's mtime+size so
it is not regenerated every turn.
"""

from __future__ import annotations

import hashlib
import json
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

# Directories never walked for the repo map (noise + secrets).
SKIP_DIRS = {
    ".git",
    ".hg",
    ".svn",
    ".overseer",
    ".venv",
    "venv",
    "env",
    "node_modules",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    "dist",
    "build",
    "target",
    ".next",
    ".nuxt",
    ".cache",
    "coverage",
    ".tox",
    ".eggs",
    "site-packages",
    ".idea",
    ".vscode",
}

# Extensions that mark a file as source (everything else is "other").
SOURCE_EXTS = {
    ".py": "python",
    ".js": "javascript",
    ".ts": "typescript",
    ".tsx": "typescript",
    ".jsx": "javascript",
    ".rs": "rust",
    ".go": "go",
    ".java": "java",
    ".c": "c",
    ".h": "c",
    ".cpp": "cpp",
    ".hpp": "cpp",
    ".rb": "ruby",
    ".php": "php",
    ".swift": "swift",
    ".kt": "kotlin",
    ".scala": "scala",
    ".sh": "shell",
    ".zsh": "shell",
    ".bash": "shell",
    ".sql": "sql",
    ".html": "html",
    ".css": "css",
    ".scss": "scss",
    ".vue": "vue",
    ".svelte": "svelte",
    ".md": "markdown",
    ".toml": "toml",
    ".yaml": "yaml",
    ".yml": "yaml",
    ".json": "json",
}

# Key files that matter for orientation (entry points, docs, config).
KEY_FILES = {
    "README.md",
    "README",
    "LICENSE",
    "pyproject.toml",
    "package.json",
    "Cargo.toml",
    "go.mod",
    "Makefile",
    "setup.py",
    "setup.cfg",
    "requirements.txt",
    "Pipfile",
    "poetry.lock",
    "uv.lock",
    "yarn.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "main.py",
    "app.py",
    "cli.py",
    "index.js",
    "index.ts",
    "main.rs",
    "main.go",
    "manage.py",
    "wsgi.py",
    "asgi.py",
    "Dockerfile",
    "docker-compose.yml",
    ".env.example",
    "tsconfig.json",
    "eslint.config.js",
    "eslint.config.mjs",
    ".eslintrc",
    ".eslintrc.json",
    "vitest.config.ts",
    "vitest.config.js",
    "jest.config.js",
    "jest.config.ts",
    "pytest.ini",
    "tox.ini",
    ".flake8",
    "mypy.ini",
    "ruff.toml",
    ".ruff.toml",
    "Cargo.lock",
    "go.sum",
    "justfile",
    "Taskfile.yml",
    "CMakeLists.txt",
    "meson.build",
    "BUILD",
    "WORKSPACE",
}


@dataclass
class ProjectContext:
    """Detected environment of a repository (plan B4)."""

    root: Path
    name: str = ""
    language: str = "unknown"
    framework: str = "unknown"
    package_manager: str = "unknown"
    test_runner: str = "unknown"
    linter: str = "unknown"
    typechecker: str = "unknown"
    build_system: str = "unknown"
    commands: dict[str, str] = field(default_factory=dict)  # test/lint/typecheck/build
    is_git: bool = False

    def summary(self) -> str:
        """One-line orientation for the model."""
        label = self.name or self.root.name
        return (
            f"project {label}: {self.language}/{self.framework} "
            f"({self.package_manager}, test: {self.test_runner}, "
            f"lint: {self.linter}, typecheck: {self.typechecker})"
        )


def _read_manifest(root: Path, name: str) -> dict[str, Any] | None:
    """Read a JSON manifest (package.json) or return None."""
    p = root / name
    if not p.is_file():
        return None
    try:
        data: dict[str, Any] = json.loads(p.read_text(encoding="utf-8"))
        return data
    except (json.JSONDecodeError, OSError):
        return None


def _read_toml(root: Path, name: str) -> dict[str, Any] | None:
    """Read a TOML manifest (pyproject.toml, Cargo.toml) or return None."""
    p = root / name
    if not p.is_file():
        return None
    try:
        import tomllib
    except ImportError:  # pragma: no cover - py3.11 always has tomllib
        return None
    try:
        with p.open("rb") as fh:
            data: dict[str, Any] = tomllib.load(fh)
            return data
    except (tomllib.TOMLDecodeError, OSError):
        return None


def _detect_python(ctx: ProjectContext, pyproject: dict[str, Any] | None) -> None:
    """Fill Python-specific fields from pyproject.toml."""
    ctx.language = "python"
    if pyproject is None:
        ctx.package_manager = "pip"
        ctx.test_runner = "pytest"
        ctx.linter = "ruff"
        ctx.typechecker = "mypy"
        ctx.commands = {
            "test": "pytest -q",
            "lint": "ruff check .",
            "typecheck": "mypy src",
        }
        return
    # Package manager: uv / poetry / pip from the tool sections.
    if "tool" in pyproject:
        tools = pyproject["tool"]
        if "uv" in tools:
            ctx.package_manager = "uv"
        elif "poetry" in tools:
            ctx.package_manager = "poetry"
        else:
            ctx.package_manager = "pip"
        if "pytest" in tools:
            ctx.test_runner = "pytest"
        if "ruff" in tools:
            ctx.linter = "ruff"
        elif "flake8" in tools:
            ctx.linter = "flake8"
        if "mypy" in tools:
            ctx.typechecker = "mypy"
        elif "pyright" in tools:
            ctx.typechecker = "pyright"
    # Framework detection from dependencies.
    deps = set()
    for section in ("dependencies", "project"):
        if section in pyproject and isinstance(pyproject[section], dict):
            deps.update(pyproject[section].keys())
    if "project" in pyproject and isinstance(pyproject["project"], dict):
        pd = pyproject["project"]
        if "dependencies" in pd and isinstance(pd["dependencies"], list):
            for dep in pd["dependencies"]:
                deps.add(re.split(r"[<>=!~\[(]", str(dep))[0].strip().lower())
    if "django" in deps:
        ctx.framework = "django"
    elif "flask" in deps:
        ctx.framework = "flask"
    elif "fastapi" in deps:
        ctx.framework = "fastapi"
    elif "torch" in deps or "tensorflow" in deps:
        ctx.framework = "ml"
    # Commands.
    pm = ctx.package_manager
    ctx.commands = {
        "test": f"{pm} run pytest -q" if pm in ("uv", "poetry") else "pytest -q",
        "lint": f"{pm} run ruff check ." if pm in ("uv", "poetry") else "ruff check .",
        "typecheck": f"{pm} run mypy src" if pm in ("uv", "poetry") else "mypy src",
    }


def _detect_node(ctx: ProjectContext, pkg: dict[str, Any] | None) -> None:
    """Fill Node-specific fields from package.json."""
    ctx.language = "javascript"
    if pkg is None:
        ctx.package_manager = "npm"
        ctx.test_runner = "jest"
        ctx.linter = "eslint"
        ctx.typechecker = "tsc"
        ctx.commands = {
            "test": "npm test",
            "lint": "npx eslint .",
            "typecheck": "npx tsc --noEmit",
        }
        return
    # Package manager from lockfiles is handled by detect_project; here we
    # infer from scripts + devDependencies.
    scripts = pkg.get("scripts", {}) or {}
    dev = set((pkg.get("devDependencies", {}) or {}).keys())
    deps = set((pkg.get("dependencies", {}) or {}).keys())
    all_deps = dev | deps
    if "next" in all_deps:
        ctx.framework = "next"
    elif "nuxt" in all_deps:
        ctx.framework = "nuxt"
    elif "react" in all_deps:
        ctx.framework = "react"
    elif "vue" in all_deps:
        ctx.framework = "vue"
    elif "express" in all_deps:
        ctx.framework = "express"
    if "vitest" in all_deps or "vitest" in str(scripts):
        ctx.test_runner = "vitest"
    elif "jest" in all_deps:
        ctx.test_runner = "jest"
    elif "mocha" in all_deps:
        ctx.test_runner = "mocha"
    elif "test" in scripts:
        ctx.test_runner = "npm-script"
    if "eslint" in all_deps:
        ctx.linter = "eslint"
    if "typescript" in all_deps or "tsc" in str(scripts):
        ctx.typechecker = "tsc"
    pm = ctx.package_manager
    ctx.commands = {
        "test": f"{pm} test" if "test" in scripts else f"{pm} run test",
        "lint": f"{pm} run lint" if "lint" in scripts else "npx eslint .",
        "typecheck": "npx tsc --noEmit" if ctx.typechecker == "tsc" else "",
    }


def _detect_rust(ctx: ProjectContext, cargo: dict[str, Any] | None) -> None:
    ctx.language = "rust"
    ctx.package_manager = "cargo"
    ctx.test_runner = "cargo test"
    ctx.linter = "clippy"
    ctx.typechecker = "cargo check"
    ctx.build_system = "cargo"
    ctx.commands = {
        "test": "cargo test",
        "lint": "cargo clippy -- -D warnings",
        "typecheck": "cargo check",
        "build": "cargo build",
    }


def _detect_go(ctx: ProjectContext, go_mod: bool) -> None:
    ctx.language = "go"
    ctx.package_manager = "go modules"
    ctx.test_runner = "go test"
    ctx.linter = "golangci-lint"
    ctx.typechecker = "go vet"
    ctx.build_system = "go build"
    ctx.commands = {
        "test": "go test ./...",
        "lint": "golangci-lint run",
        "typecheck": "go vet ./...",
        "build": "go build ./...",
    }


def detect_project(root: str | Path) -> ProjectContext:
    """Detect the environment of a repository (plan B4)."""
    root = Path(root).expanduser().resolve()
    ctx = ProjectContext(root=root, is_git=(root / ".git").exists())

    pyproject = _read_toml(root, "pyproject.toml")
    pkg = _read_manifest(root, "package.json")
    cargo = _read_toml(root, "Cargo.toml")
    go_mod = (root / "go.mod").is_file()
    makefile = (root / "Makefile").is_file()

    if pyproject is not None:
        _detect_python(ctx, pyproject)
        proj = pyproject.get("project", {})
        if isinstance(proj, dict):
            ctx.name = str(proj.get("name", ""))
    elif pkg is not None:
        # Lockfile determines the package manager.
        if (root / "pnpm-lock.yaml").exists():
            ctx.package_manager = "pnpm"
        elif (root / "yarn.lock").exists():
            ctx.package_manager = "yarn"
        elif (root / "package-lock.json").exists():
            ctx.package_manager = "npm"
        else:
            ctx.package_manager = "npm"
        ctx.name = str(pkg.get("name", ""))
        _detect_node(ctx, pkg)
    elif cargo is not None:
        _detect_rust(ctx, cargo)
    elif go_mod:
        _detect_go(ctx, True)
    elif (root / "requirements.txt").is_file() or (root / "setup.py").is_file():
        # Bare Python project: no pyproject.toml, but clearly Python.
        _detect_python(ctx, None)
    elif makefile:
        ctx.build_system = "make"
        ctx.commands = {"test": "make test", "build": "make build"}

    return ctx


# ---------------------------------------------------------------------------
# Repo map
# ---------------------------------------------------------------------------


@dataclass
class RepoMap:
    """Lightweight repository map (plan B4). Never reads file contents."""

    root: Path
    dirs: list[str] = field(default_factory=list)
    files: list[str] = field(default_factory=list)  # relative paths
    extensions: dict[str, int] = field(default_factory=dict)
    key_files: list[str] = field(default_factory=list)
    entry_points: list[str] = field(default_factory=list)
    total_files: int = 0

    def summary(self, limit: int = 60) -> str:
        """Compact text form for the model (truncated)."""
        lines = [f"repo map: {self.root.name} ({self.total_files} files)"]
        if self.key_files:
            lines.append("key files: " + ", ".join(self.key_files[:12]))
        if self.entry_points:
            lines.append("entry points: " + ", ".join(self.entry_points[:8]))
        exts = ", ".join(f"{e}:{n}" for e, n in sorted(self.extensions.items())[:10])
        if exts:
            lines.append("extensions: " + exts)
        lines.append("dirs: " + ", ".join(self.dirs[:limit]))
        return "\n".join(lines)


def _cache_path(root: Path) -> Path:
    return root / ".overseer" / "cache" / "repo_map.json"


def _root_signature(root: Path) -> str:
    """Hash of root mtime + file count — cheap cache invalidation."""
    try:
        entries = list(root.iterdir())
    except OSError:
        entries = []
    sig = hashlib.sha256()
    sig.update(str(root).encode())
    for e in sorted(entries, key=lambda p: p.name):
        try:
            st = e.stat()
            sig.update(f"{e.name}:{st.st_mtime_ns}:{st.st_size}".encode())
        except OSError:
            continue
    return sig.hexdigest()[:16]


def repo_map(root: str | Path, use_cache: bool = True) -> RepoMap:
    """Generate (or load cached) a lightweight map of the repository."""
    root = Path(root).expanduser().resolve()
    cache = _cache_path(root)
    if use_cache and cache.is_file():
        try:
            data = json.loads(cache.read_text(encoding="utf-8"))
            if data.get("signature") == _root_signature(root):
                return RepoMap(root=root, **{k: v for k, v in data.items() if k != "signature"})
        except (json.JSONDecodeError, OSError):
            pass  # stale/corrupt cache -> regenerate

    m = RepoMap(root=root)
    for dirpath, dirnames, filenames in os_walk(root):
        dirpath = Path(dirpath)  # os.walk yields str
        rel_dir = dirpath.relative_to(root)
        # Prune noise dirs in place.
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        if rel_dir != Path("."):
            m.dirs.append(str(rel_dir))
        for fn in sorted(filenames):
            rel = rel_dir / fn
            rel_s = str(rel)
            m.files.append(rel_s)
            m.total_files += 1
            ext = rel.suffix.lower()
            m.extensions[ext] = m.extensions.get(ext, 0) + 1
            if fn in KEY_FILES:
                m.key_files.append(rel_s)
            if fn in ("main.py", "app.py", "cli.py", "index.js", "index.ts", "main.rs", "main.go"):
                m.entry_points.append(rel_s)

    # Cap the file list (the map is for orientation, not exhaustive listing).
    m.files = m.files[:500]
    m.dirs = m.dirs[:200]

    if use_cache:
        try:
            cache.parent.mkdir(parents=True, exist_ok=True)
            data = {
                "signature": _root_signature(root),
                "dirs": m.dirs,
                "files": m.files,
                "extensions": m.extensions,
                "key_files": m.key_files,
                "entry_points": m.entry_points,
                "total_files": m.total_files,
            }
            cache.write_text(json.dumps(data), encoding="utf-8")
        except OSError:
            pass  # cache is best-effort

    return m


def os_walk(root: Path) -> Any:
    """os.walk wrapper (imported lazily to keep module import light)."""
    import os

    return os.walk(root)
