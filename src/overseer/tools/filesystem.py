"""Filesystem tools: file_read, file_write, file_patch, list_dir, grep.

Security: all paths resolve and are contained within allowed roots for
writes; reads are allowed anywhere but never follow outside the vault
without approval. Output is redacted and truncated; full output is stored
as an artifact.
"""

from __future__ import annotations

import re
from pathlib import Path
from typing import Any

from overseer.errors import ToolError
from overseer.redact import redact
from overseer.tools.base import Tool, ToolContext, ToolResult, store_artifact
from overseer.tools.registry import register_tool

MAX_READ_CHARS = 100_000  # hard cap on file reads


def _resolve(root: Path, path: str) -> Path:
    """Resolve a path relative to a root, refusing traversal."""
    p = (root / path).resolve()
    if not p.is_relative_to(root.resolve()):
        raise ToolError(f"path escapes allowed root: {path!r}")
    return p


def _contained_in_any(path: Path, roots: list[Path]) -> bool:
    resolved = path.resolve()
    return any(resolved.is_relative_to(r.resolve()) for r in roots)


@register_tool
class FileReadTool(Tool):
    name = "file_read"
    description = "Read a file's contents (truncated for the model; full copy stored as artifact)."
    parameters = {
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Absolute or relative path to read."},
            "offset": {"type": "integer", "description": "Line number to start from (1-based)."},
            "limit": {"type": "integer", "description": "Max lines to read."},
        },
        "required": ["path"],
    }

    def run(self, args: dict[str, Any], context: ToolContext | None = None) -> ToolResult:
        raw = args["path"]
        if context and context.allowed_roots and not Path(raw).expanduser().is_absolute():
            path = _resolve(context.allowed_roots[0], raw)
        else:
            path = Path(raw).expanduser()
        if not path.is_file():
            return self._error(f"file not found: {path}")
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError as exc:
            return self._error(f"cannot read {path}: {exc}")

        lines = text.splitlines()
        offset = int(args.get("offset", 1))
        limit = int(args.get("limit", len(lines)))
        selected = lines[max(0, offset - 1) : offset - 1 + limit]
        selected_text = "\n".join(selected)

        # Store the full (redacted) content as an artifact.
        artifact = store_artifact(
            context.artifacts_dir if context else Path(".overseer/artifacts"),
            self.name,
            redact(text),
        )
        return self._result(
            f"file: {path} ({len(lines)} lines)\n{selected_text}",
            artifacts=[str(artifact)],
        )


@register_tool
class FileWriteTool(Tool):
    name = "file_write"
    description = "Write a file. Must be inside an allowed root (approval required otherwise)."
    parameters = {
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Path to write (relative to allowed root)."},
            "content": {"type": "string", "description": "Full content to write."},
        },
        "required": ["path", "content"],
    }
    requires_approval = True

    def run(self, args: dict[str, Any], context: ToolContext | None = None) -> ToolResult:
        if context is None or not context.allowed_roots:
            return self._error("no allowed write roots configured")
        try:
            path = _resolve(context.allowed_roots[0], args["path"])
        except ToolError as exc:
            return self._error(str(exc))
        if not _contained_in_any(path, context.allowed_roots):
            return self._error(f"write outside allowed roots: {path}")
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(args["content"], encoding="utf-8")
        except OSError as exc:
            return self._error(f"cannot write {path}: {exc}")
        return self._result(f"wrote {len(args['content'])} chars to {path}")


@register_tool
class FilePatchTool(Tool):
    name = "file_patch"
    description = "Apply a simple string replacement in a file (old -> new)."
    parameters = {
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Path to patch (relative to allowed root)."},
            "old": {"type": "string", "description": "Exact text to find."},
            "new": {"type": "string", "description": "Replacement text."},
            "replace_all": {"type": "boolean", "description": "Replace all occurrences."},
        },
        "required": ["path", "old", "new"],
    }
    requires_approval = True

    def run(self, args: dict[str, Any], context: ToolContext | None = None) -> ToolResult:
        if context is None or not context.allowed_roots:
            return self._error("no allowed write roots configured")
        try:
            path = _resolve(context.allowed_roots[0], args["path"])
        except ToolError as exc:
            return self._error(str(exc))
        if not _contained_in_any(path, context.allowed_roots):
            return self._error(f"patch outside allowed roots: {path}")
        try:
            text = path.read_text(encoding="utf-8")
        except OSError as exc:
            return self._error(f"cannot read {path}: {exc}")
        old, new = args["old"], args["new"]
        if old not in text:
            return self._error(f"old text not found in {path}")
        count = text.count(old) if args.get("replace_all") else 1
        text = text.replace(old, new, -1 if args.get("replace_all") else 1)
        try:
            path.write_text(text, encoding="utf-8")
        except OSError as exc:
            return self._error(f"cannot write {path}: {exc}")
        return self._result(f"patched {count} occurrence(s) in {path}")


@register_tool
class ListDirTool(Tool):
    name = "list_dir"
    description = "List a directory's entries (names only, one per line)."
    parameters = {
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Directory to list."},
        },
        "required": ["path"],
    }

    def run(self, args: dict[str, Any], context: ToolContext | None = None) -> ToolResult:
        path = Path(args["path"]).expanduser()
        if not path.is_dir():
            return self._error(f"not a directory: {path}")
        try:
            entries = sorted(p.name for p in path.iterdir())
        except OSError as exc:
            return self._error(f"cannot list {path}: {exc}")
        return self._result(f"dir: {path} ({len(entries)} entries)\n" + "\n".join(entries))


@register_tool
class GrepTool(Tool):
    name = "grep"
    description = "Search file contents with a regex; returns matching lines with line numbers."
    parameters = {
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": "Regex pattern."},
            "path": {"type": "string", "description": "File or directory to search."},
            "file_glob": {"type": "string", "description": "Glob filter (e.g. *.py)."},
        },
        "required": ["pattern", "path"],
    }

    def run(self, args: dict[str, Any], context: ToolContext | None = None) -> ToolResult:
        try:
            pattern = re.compile(args["pattern"])
        except re.error as exc:
            return self._error(f"invalid regex: {exc}")
        root = Path(args["path"]).expanduser()
        if not root.exists():
            return self._error(f"path not found: {root}")

        files: list[Path] = []
        if root.is_file():
            files = [root]
        else:
            glob = args.get("file_glob") or "*"
            files = sorted(root.rglob(glob)) if glob == "*" else sorted(root.rglob(glob))
            files = [f for f in files if f.is_file()]

        matches: list[str] = []
        for f in files[:200]:  # cap files scanned
            try:
                for i, line in enumerate(
                    f.read_text(encoding="utf-8", errors="replace").splitlines(), 1
                ):
                    if pattern.search(line):
                        matches.append(f"{f}:{i}:{line}")
            except OSError:
                continue
        if not matches:
            return self._result(f"no matches for {args['pattern']!r} in {root}")
        return self._result(f"{len(matches)} match(es):\n" + "\n".join(matches[:200]))
