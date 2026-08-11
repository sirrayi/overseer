"""Vault: the canonical Obsidian-compatible memory (plan Parts 3-5).

Invariants enforced here:
- The vault is canonical; .overseer/ is derived and disposable.
- `overseer init` is idempotent and creates the full Part-4 layout.
- Note writes are atomic (temp file + rename) to avoid partial notes.
- Path containment: no note may be written outside the vault.
- Every note carries stable frontmatter (id, type, title, created, modified, status).
"""

from __future__ import annotations

import contextlib
import os
import tempfile
import uuid
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import yaml

from overseer.errors import VaultError

# Part 4 layout: numbered folders, stable ordering.
VAULT_FOLDERS: list[str] = [
    "00-Inbox",
    "05-System",
    "05-System/Templates",
    "10-Sessions",
    "20-Episodes",
    "30-Facts",
    "40-Skills",
    "50-Preferences",
    "60-Projects",
    "70-Decisions",
    "80-Corrections",
    "90-Proposals",
    "95-Archive",
    "99-Meta",
]

# Derived, disposable, gitignored.
OVERSER_DIR = ".overseer"
OVERSER_SUBDIRS = [
    "logs",
    "secrets",
    "artifacts",
    "tmp",
    "telemetry.local",
]

# Note types -> folder (Part 5).
NOTE_TYPE_FOLDERS: dict[str, str] = {
    "session": "10-Sessions",
    "episode": "20-Episodes",
    "fact": "30-Facts",
    "skill": "40-Skills",
    "preference": "50-Preferences",
    "project": "60-Projects",
    "decision": "70-Decisions",
    "correction": "80-Corrections",
    "proposal": "90-Proposals",
    "meta": "99-Meta",
}

# Stable ID prefixes (Part 5).
ID_PREFIXES: dict[str, str] = {
    "fact": "OVR-FACT",
    "skill": "OVR-SKILL",
    "preference": "OVR-PREF",
    "correction": "OVR-CORR",
    "proposal": "OVR-PROP",
    "decision": "OVR-DEC",
    "session": "OVR-SESS",
    "episode": "OVR-EPIS",
    "project": "OVR-PROJ",
    "meta": "OVR-META",
}

REQUIRED_FRONTMATTER = ("id", "type", "title", "created", "modified", "status")

SYSTEM_NOTES: dict[str, str] = {
    "05-System/Home.md": """---
id: OVR-SYS-HOME
type: system
title: Home
created: {created}
modified: {modified}
status: active
---

# Home

Overseer's main dashboard. The vault is the canonical memory; everything
else is a derived cache.

## quick links
- [[Dashboard]]
- [[Guardrails]]
- [[Ontology]]
""",
    "05-System/Dashboard.md": """---
id: OVR-SYS-DASH
type: system
title: Dashboard
created: {created}
modified: {modified}
status: active
---

# Dashboard

Operational view: recent sessions, active proposals, high-salience facts,
corrections, budget status. Populated by overseer as it runs.
""",
    "05-System/Guardrails.md": """---
id: OVR-SYS-GUARD
type: system
title: Guardrails
created: {created}
modified: {modified}
status: active
---

# Guardrails

Non-negotiable rules (plan Part 42.1, invariant 8):

1. The vault is canonical. SQLite, FTS5, embeddings, caches are derived and disposable.
2. Security is continuous. Safe defaults, approval gates, secret hygiene, path safety, injection awareness.
3. Learning is based on verified truth. No durable lessons from unverified outcomes.
4. Memory is governed: id, type, source, confidence, scope, status, dedup, conflict, staleness, supersession, archival, deletion, audit.
5. Context is compiled, not dumped. Budgeted, progressive disclosure, summarized tool output.
6. Efficiency is mandatory. Tokens, latency, CPU, RAM, disk, battery, indexing, background jobs, escalations.
7. Live learning is budgeted, reversible, visible, safe. Never per-prompt weight training.
8. **Self-modification is proposal-only and human-approved.** Overseer may propose changes to its own prompts, thresholds, code, or learning rules. It must never apply them silently. All L3 changes require explicit human approval, evidence, risk assessment, and rollback.
9. The user is the final authority. Transparent, interruptible, inspectable.
10. Simplicity beats cleverness. No overengineering, no fragile monolith.
""",
    "05-System/Ontology.md": """---
id: OVR-SYS-ONT
type: system
title: Ontology
created: {created}
modified: {modified}
status: active
---

# Ontology

Note types, frontmatter schema, tags, statuses, and memory rules (plan Part 5).

## note types
| type | folder | id prefix |
|---|---|---|
| session | 10-Sessions | OVR-SESS |
| episode | 20-Episodes | OVR-EPIS |
| fact | 30-Facts | OVR-FACT |
| skill | 40-Skills | OVR-SKILL |
| preference | 50-Preferences | OVR-PREF |
| project | 60-Projects | OVR-PROJ |
| decision | 70-Decisions | OVR-DEC |
| correction | 80-Corrections | OVR-CORR |
| proposal | 90-Proposals | OVR-PROP |
| meta | 99-Meta | OVR-META |

## common frontmatter
id, type, title, created, modified, status, tags, source, confidence,
salience, scope, expiry, superseded_by, evidence

## statuses
active, superseded, archived, draft, proposed, accepted, rejected, deprecated

## memory rules
- one fact per note (atomic notes)
- raw logs live in .overseer/logs, not the vault
- session notes are summaries, not transcripts
- large tool outputs live in .overseer/artifacts
""",
}

TEMPLATE_NOTES: dict[str, str] = {
    "05-System/Templates/session.md": """---
id: OVR-SESS-{id}
type: session
title: {title}
created: {created}
modified: {modified}
status: active
tags: [session]
---

# {title}

## goal
- 

## plan
- 

## actions
- 

## outcomes
- 

## corrections
- 

## files touched
- 

## commands run
- 

## evidence
- 

## extracted memories
- 
""",
    "05-System/Templates/fact.md": """---
id: OVR-FACT-{id}
type: fact
title: {title}
created: {created}
modified: {modified}
status: active
tags: [fact]
scope: global
confidence: 0.5
source: 
evidence: []
expiry: 
---

# {title}

{body}
""",
    "05-System/Templates/skill.md": """---
id: OVR-SKILL-{id}
type: skill
title: {title}
created: {created}
modified: {modified}
status: draft
tags: [skill]
trigger: 
confidence: 0.5
use_count: 0
success_count: 0
failure_count: 0
risk: low
source: 
---

# {title}

## when to use
{trigger}

## steps
1. 

## pitfalls
- 
""",
    "05-System/Templates/preference.md": """---
id: OVR-PREF-{id}
type: preference
title: {title}
created: {created}
modified: {modified}
status: active
tags: [preference]
scope: global
strength: 0.5
source: explicit
---

# {title}

{body}
""",
    "05-System/Templates/correction.md": """---
id: OVR-CORR-{id}
type: correction
title: {title}
created: {created}
modified: {modified}
status: active
tags: [correction]
trigger: 
mistake: 
correction: 
rule: 
severity: medium
---

# {title}

## trigger
{trigger}

## mistake
{mistake}

## correction
{correction}

## rule
{rule}
""",
    "05-System/Templates/decision.md": """---
id: OVR-DEC-{id}
type: decision
title: {title}
created: {created}
modified: {modified}
status: accepted
tags: [decision]
context: 
alternatives: []
consequences: []
---

# {title}

## context
{context}

## decision
{decision}

## alternatives
- 

## consequences
- 
""",
    "05-System/Templates/proposal.md": """---
id: OVR-PROP-{id}
type: proposal
title: {title}
created: {created}
modified: {modified}
status: draft
tags: [proposal]
proposal_type: skill
risk: low
expected_benefit: 
evidence: []
approval: required
---

# {title}

## proposal
{body}

## expected benefit
{expected_benefit}

## evidence
- 

## approval
Required. Never applied silently (guardrail 8).
""",
    "05-System/Templates/project.md": """---
id: OVR-PROJ-{id}
type: project
title: {title}
created: {created}
modified: {modified}
status: active
tags: [project]
languages: []
commands: []
conventions: []
risks: []
---

# {title}

## languages
- 

## commands
- 

## conventions
- 

## risks
- 

## important paths
- 

## architecture notes
- 
""",
}


def _now() -> str:
    return datetime.now(UTC).isoformat(timespec="seconds")


def _atomic_write(path: Path, content: str) -> None:
    """Write content atomically: temp file in same dir, then rename."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(dir=str(path.parent), prefix=".tmp-", suffix=".md")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fh.write(content)
        os.replace(tmp_name, path)
    except BaseException:
        with contextlib.suppress(OSError):
            os.unlink(tmp_name)
        raise


def _contained(root: Path, candidate: Path) -> Path:
    """Resolve candidate under root; raise VaultError on traversal."""
    resolved_root = root.resolve()
    resolved = candidate.resolve()
    if resolved != resolved_root and resolved_root not in resolved.parents:
        raise VaultError(f"path escapes vault: {candidate}")
    return resolved


def _new_note_id(note_type: str) -> str:
    """Generate a collision-safe stable note ID: OVR-<TYPE>-<uuid4 hex[:8]>."""
    prefix = ID_PREFIXES[note_type]
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


def _validate_frontmatter(note_type: str, meta: dict[str, Any]) -> None:
    """Enforce type-specific frontmatter governance (plan Part 5, MAJOR-16)."""
    for field in REQUIRED_FRONTMATTER:
        if field not in meta:
            raise VaultError(f"note missing required frontmatter field: {field}")

    statuses = {
        "active",
        "superseded",
        "archived",
        "draft",
        "proposed",
        "accepted",
        "rejected",
        "deprecated",
    }
    if meta.get("status") not in statuses:
        raise VaultError(f"invalid status {meta.get('status')!r} (see Ontology)")

    # Type-specific required fields (plan: governed memory).
    type_required: dict[str, tuple[str, ...]] = {
        "fact": ("scope", "confidence", "source"),
        "correction": ("trigger", "mistake", "correction", "rule", "severity"),
        "proposal": ("proposal_type", "risk", "approval"),
        "skill": ("trigger", "risk", "use_count", "success_count", "failure_count"),
        "preference": ("scope", "strength", "source"),
        "decision": ("context", "alternatives", "consequences"),
        "project": ("languages", "commands", "conventions", "risks"),
    }
    for field in type_required.get(note_type, ()):
        if field not in meta:
            raise VaultError(f"{note_type} note missing required frontmatter field: {field}")


class Vault:
    """A canonical overseer vault."""

    def __init__(self, root: str | Path) -> None:
        self.root = Path(root).expanduser().resolve()
        self.overseer_dir = self.root / OVERSER_DIR

    # -- lifecycle ---------------------------------------------------------

    def init(self) -> list[Path]:
        """Create the full Part-4 layout. Idempotent: safe to run twice.

        Returns the list of files created (empty on a second run).
        """
        created: list[Path] = []
        for folder in VAULT_FOLDERS:
            (self.root / folder).mkdir(parents=True, exist_ok=True)
        for sub in OVERSER_SUBDIRS:
            (self.overseer_dir / sub).mkdir(parents=True, exist_ok=True)

        # .overseer is derived/disposable and may hold secrets — never track it.
        ov_gitignore = self.overseer_dir / ".gitignore"
        if not ov_gitignore.exists():
            _atomic_write(ov_gitignore, "*\n")
            created.append(ov_gitignore)

        stamp = _now()
        defaults = {
            "id": "000000",
            "title": "template",
            "created": stamp,
            "modified": stamp,
            "body": "",
            "trigger": "",
            "mistake": "",
            "correction": "",
            "rule": "",
            "context": "",
            "decision": "",
            "expected_benefit": "",
        }
        for rel, template in SYSTEM_NOTES.items():
            path = self.root / rel
            if not path.exists():
                _atomic_write(path, template.format(**defaults))
                created.append(path)

        for rel, template in TEMPLATE_NOTES.items():
            path = self.root / rel
            if not path.exists():
                _atomic_write(path, template.format(**defaults))
                created.append(path)

        return created

    def is_vault(self) -> bool:
        """True if this looks like an overseer vault (system notes present)."""
        return (self.root / "05-System" / "Guardrails.md").is_file() and (
            self.root / "05-System" / "Ontology.md"
        ).is_file()

    # -- note writing ------------------------------------------------------

    def write_note(self, note_type: str, title: str, body: str = "", **frontmatter: Any) -> Path:
        """Write an atomic note with stable frontmatter into the right folder.

        Generates a collision-safe stable ID (OVR-<TYPE>-<hex>) and timestamps.
        Filename includes the ID, so duplicate titles never overwrite each other.
        """
        folder = NOTE_TYPE_FOLDERS.get(note_type)
        if folder is None:
            raise VaultError(f"unknown note type: {note_type!r} (see Ontology)")

        note_id = _new_note_id(note_type)
        stamp = _now()

        meta: dict[str, Any] = {
            "id": note_id,
            "type": note_type,
            "title": title,
            "created": stamp,
            "modified": stamp,
            "status": "active",
            "tags": [note_type],
        }
        meta.update(frontmatter)

        _validate_frontmatter(note_type, meta)

        slug = _slugify(title)
        rel = Path(folder) / f"{note_id}-{slug}.md"
        path = _contained(self.root, self.root / rel)

        header = yaml.safe_dump(meta, sort_keys=False, allow_unicode=True).strip()
        content = f"---\n{header}\n---\n\n# {title}\n\n{body}".rstrip() + "\n"
        _atomic_write(path, content)
        return path

    # -- inspection --------------------------------------------------------

    def list_notes(self, note_type: str | None = None) -> list[Path]:
        """List note files, optionally filtered by type folder."""
        if note_type is not None:
            folder = NOTE_TYPE_FOLDERS.get(note_type)
            if folder is None:
                raise VaultError(f"unknown note type: {note_type!r}")
            return sorted((self.root / folder).glob("*.md"))
        return sorted(self.root.glob("**/*.md"))


def _slugify(title: str) -> str:
    """Turn a title into a safe filename slug (lowercase, hyphens, unicode-safe)."""
    keep = []
    for ch in title.lower():
        if ch.isalnum():
            keep.append(ch)
        elif ch in (" ", "-", "_"):
            keep.append("-")
    slug = "".join(keep)
    # Collapse consecutive hyphens, strip leading/trailing.
    slug = "-".join(part for part in slug.split("-") if part)
    return slug or "note"
