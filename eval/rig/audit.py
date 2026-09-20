"""Canary audit (playbook §4.4).

Held-out tasks carry a unique `OVR-CANARY-<hex16>` token planted in their
workspace/instruction. The token is inert — the grader never reads it —
so its only function is traceability:

- canary of task A found in task B's trajectory/artifacts → content leaked
  across runs (shared-state bug or memorization carried between trials)
- canary found in a non-held-out run → held-out material leaked
- held-out task file without a canary → suite hygiene failure

Echo presence inside the owning run is expected (the agent legitimately
reads the planted file); we record it as `echo` for visibility, not a
finding.
"""

from __future__ import annotations

import json
import re
from pathlib import Path

CANARY_RE = re.compile(r"OVR-CANARY-[0-9a-f]{16}")

def _word_5grams(text: str) -> set[tuple[str, ...]]:
    toks = re.findall(r"[a-z0-9_]+", text.lower())
    if len(toks) < 5:
        return set()
    return {tuple(toks[i:i + 5]) for i in range(len(toks) - 4)}


def overlap_5gram(a: str, b: str) -> float:
    """5-gram Jaccard-ish overlap over normalized word tokens.

    |intersection| / |union|; 0.0 when either side yields no 5-grams.
    """
    ga, gb = _word_5grams(a or ""), _word_5grams(b or "")
    if not ga or not gb:
        return 0.0
    return len(ga & gb) / len(ga | gb)


_PATH_RE = re.compile(
    r"(?:^|[\s\"'`(\[])"
    r"((?:[\w.\-]+/)+[\w.\-]+(?:\.[\w]+)?|[\w.\-]+\.(?:py|rs|ts|tsx|js|jsx|go|java|rb|sh|md|toml|yaml|yml|json))"
)


def localization_probe(issue: str, patch: str) -> bool:
    """True when a file path named in the issue text appears in the patch."""
    issue = issue or ""
    patch = patch or ""
    if not issue.strip() or not patch.strip():
        return False
    paths = {m.group(1).strip().strip(",;:") for m in _PATH_RE.finditer(issue)}
    paths = {p for p in paths if p}
    if not paths:
        return False
    pl = patch.lower()
    return any(p.lower() in pl for p in paths)


def _scan_text(path: Path) -> set[str]:
    try:
        return set(CANARY_RE.findall(path.read_text(errors="replace")))
    except OSError:
        return set()


def _scan_streamed(path: Path) -> set[str]:
    """Chunked scan so >5 MB trajectories/events don't get skipped — a
    canary can span a chunk boundary, hence the overlap tail."""
    found: set[str] = set()
    tail = ""
    try:
        with open(path, errors="replace") as f:
            while chunk := f.read(1 << 20):
                found |= set(CANARY_RE.findall(tail + chunk))
                tail = chunk[-64:]
    except OSError:
        pass
    return found


def audit_canaries(
    tasks,
    records: list[dict],
    *,
    public_dirs: list[Path] | None = None,
) -> dict:
    """tasks: held-out TaskSpecs (canary owners). records: stored runs.

    Returns {findings, scanned_runs, per_task} — every finding is
    {type, canary, owner_task, found_in_run, found_in_task}."""
    owner = {t.canary: t.id for t in tasks if t.canary}
    heldout_ids = {t.id for t in tasks}
    findings = []
    per_task = {t.id: {"echo": 0, "foreign_sightings": 0} for t in tasks if t.canary}

    # hygiene: every held-out task must carry a canary
    for t in tasks:
        if not t.canary:
            findings.append(
                {
                    "type": "missing_canary",
                    "task": t.id,
                    "detail": "held-out task has no canary token",
                }
            )
        elif not CANARY_RE.fullmatch(t.canary):
            findings.append(
                {
                    "type": "malformed_canary",
                    "task": t.id,
                    "detail": f"{t.canary!r} doesn't match {CANARY_RE.pattern}",
                }
            )

    # leak check: no held-out canary may sit in the public corpus
    for d in public_dirs or []:
        for f in Path(d).glob("*.json"):
            for c in _scan_text(f) & set(owner):
                findings.append(
                    {
                        "type": "leak_to_public",
                        "canary": c,
                        "owner_task": owner[c],
                        "found_in": str(f),
                        "detail": "held-out canary present in public task file",
                    }
                )

    # cross-run check: canaries found in trajectories/artifacts
    scanned = 0
    for r in records:
        if r.get("kind") != "run":
            continue
        blobs = set()
        files_read = 0
        for key in ("trajectory", "session_dir"):
            p = r.get(key)
            if not p:
                continue
            path = Path(p)
            targets = (
                [path]
                if path.is_file()
                else (
                    [f for f in path.rglob("*") if f.is_file()] if path.is_dir() else []
                )
            )
            for f in targets:
                found = _scan_streamed(f)
                files_read += 1
                blobs |= found
        if files_read == 0:
            continue
        scanned += 1  # a run counts as scanned iff we actually read a file
        run_task = r.get("task_id", "")
        for c in blobs:
            own = owner.get(c)
            if own is None:
                continue  # canary from an unregistered task — not ours
            if own == run_task or run_task.endswith(own):
                per_task[own]["echo"] += 1
            else:
                per_task[own]["foreign_sightings"] += 1
                findings.append(
                    {
                        "type": "foreign_canary",
                        "canary": c,
                        "owner_task": own,
                        "found_in_run": r.get("run_id"),
                        "found_in_task": run_task,
                        "detail": "canary appeared in a run that doesn't own it",
                    }
                )
            if run_task and not any(
                run_task == h or run_task.endswith(h) for h in heldout_ids
            ):
                findings.append(
                    {
                        "type": "canary_in_public_run",
                        "canary": c,
                        "owner_task": own,
                        "found_in_run": r.get("run_id"),
                        "found_in_task": run_task,
                        "detail": "held-out canary surfaced in a public-suite run",
                    }
                )

    return {
        "findings": findings,
        "scanned_runs": scanned,
        "per_task": per_task,
        "n_heldout_tasks": len(tasks),
        "n_canaries": len(owner),
    }


def render_md(audit: dict) -> str:
    lines = [
        "# Canary audit",
        "",
        f"- held-out tasks: {audit['n_heldout_tasks']} "
        f"({audit['n_canaries']} canaries)",
        f"- runs scanned: {audit['scanned_runs']}",
        f"- findings: {len(audit['findings'])}",
        "",
    ]
    for f in audit["findings"]:
        lines.append(
            f"- **{f['type']}**: {f['detail']} "
            f"({json.dumps({k: v for k, v in f.items() if k not in ('type', 'detail')})})"
        )
    if audit["per_task"]:
        lines += ["", "| task | echo | foreign sightings |", "|---|---|---|"]
        for t, s in sorted(audit["per_task"].items()):
            lines.append(f"| {t} | {s['echo']} | {s['foreign_sightings']} |")
    return "\n".join(lines) + "\n"
