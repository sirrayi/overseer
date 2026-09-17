"""store + taskspec — append-only store round-trip and spec validation."""

import json
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from rig import store, taskspec


class TestStore:
    def test_append_load_roundtrip(self, tmp_path):
        st = store.Store(tmp_path / "s.jsonl")
        st.append({"kind": "run", "task_id": "t1", "pass": True})
        st.append({"kind": "run", "task_id": "t2", "pass": False})
        rows = st.load()
        assert len(rows) == 2
        assert rows[0]["task_id"] == "t1"

    def test_malformed_lines_skipped_not_fatal(self, tmp_path):
        """A crashed writer can leave a truncated tail line — load must
        skip it and count it, not abort every reader of the store."""
        p = tmp_path / "s.jsonl"
        p.write_text(
            '{"kind": "run", "task_id": "ok"}\n'
            "{not json\n"
            '{"kind": "run", "task_id": "ok2"}\n'
        )
        st = store.Store(p)
        rows = st.load()
        assert [r["task_id"] for r in rows] == ["ok", "ok2"]
        assert st.skipped_lines == 1

    def test_malformed_rows_dont_crash_report(self, tmp_path):
        """Store rows with missing/dict-shaped fields must not crash
        stats or report grouping."""
        from rig import report, stats

        records = [
            {
                "kind": "run",
                "task_id": "t1",
                "pass": True,
                "seed": 0,
                "harness": "a",
                "model": "m",
                "run_set_id": "rs",
                "benchmark": "local",
            },
            {"kind": "run"},  # everything missing
            {
                "kind": "run",
                "task_id": "t1",
                "pass": False,
                "seed": 0,
                "harness": "a",
                "model": {"name": "dict"},
                "run_set_id": "rs",
                "benchmark": "local",
                "cost_usd": "lots",
                "tokens_in": -1,
            },
        ]
        s = stats.summarize(records)
        assert s["tasks"] == 1 and s["trials"] == 2
        md, _ = report.render(records, k=1)
        assert "overseer eval report" in md and "rs" in md

    def test_immutable_append_only(self, tmp_path):
        p = tmp_path / "s.jsonl"
        st = store.Store(p)
        st.append({"a": 1})
        before = p.read_text()
        st.append({"a": 2})
        assert p.read_text().startswith(before)  # never rewritten

    def test_runs_filter(self, tmp_path):
        st = store.Store(tmp_path / "s.jsonl")
        st.append({"kind": "matrix", "run_set_id": "x"})
        st.append({"kind": "run", "harness": "overseer", "model": "m1"})
        st.append({"kind": "run", "harness": "mini", "model": "m1"})
        assert len(st.runs()) == 2
        assert len(st.runs(harness="overseer")) == 1

    def test_matrix_header_shape(self, tmp_path):
        st = store.Store(tmp_path / "s.jsonl")
        h = st.matrix_header(
            benchmark="local",
            agents=["a", "b"],
            k=3,
            tasks=["t1"],
            scheduler_seed=7,
            extra={"harness_commit": "abc"},
        )
        assert h["kind"] == "matrix" and h["seeds"] == 3
        assert h["scheduler_seed"] == 7
        assert len(st.load()) == 1

    def test_load_missing(self, tmp_path):
        assert store.Store(tmp_path / "nope.jsonl").load() == []


VALID = {
    "id": "t",
    "version": 1,
    "instruction": "do it",
    "grader": {"type": "bash_assert", "script": "true"},
    "oracle": {"script": "true"},
}


class TestTaskSpec:
    def test_parse_minimal(self):
        s = taskspec.parse(VALID)
        assert s.id == "t" and s.version == 1 and s.env_digest == "local-sh"

    @pytest.mark.parametrize(
        "field", ["id", "version", "instruction", "grader", "oracle"]
    )
    def test_required_fields(self, field):
        bad = {k: v for k, v in VALID.items() if k != field}
        with pytest.raises(taskspec.SpecError):
            taskspec.parse(bad)

    def test_grader_type_whitelist(self):
        bad = dict(VALID, grader={"type": "llm_judge", "script": "x"})
        with pytest.raises(taskspec.SpecError):
            taskspec.parse(bad)

    def test_oracle_required(self):
        bad = dict(VALID, oracle={})
        with pytest.raises(taskspec.SpecError):
            taskspec.parse(bad)

    def test_version_positive_int(self):
        with pytest.raises(taskspec.SpecError):
            taskspec.parse(dict(VALID, version=0))

    def test_env_digest(self):
        s = taskspec.parse(dict(VALID, env={"image_digest": "sha256:abc"}))
        assert s.env_digest == "sha256:abc"

    def test_load_dir_dupes(self, tmp_path):
        for name in ("a.json", "b.json"):
            (tmp_path / name).write_text(json.dumps(VALID))
        with pytest.raises(taskspec.SpecError, match="duplicate"):
            taskspec.load_dir(tmp_path)

    def test_load_dir_sorted(self, tmp_path):
        for tid in ("zz", "aa"):
            (tmp_path / f"{tid}.json").write_text(json.dumps(dict(VALID, id=tid)))
        specs = taskspec.load_dir(tmp_path)
        assert [s.id for s in specs] == ["aa", "zz"]
