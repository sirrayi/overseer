"""Audit probes — 5-gram overlap + localization (zero spend)."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from rig import audit


class TestOverlap5gram:
    def test_identical_is_one(self):
        assert audit.overlap_5gram(
            "the quick brown fox jumps over", "the quick brown fox jumps over"
        ) == 1.0

    def test_disjoint_is_zero(self):
        assert (
            audit.overlap_5gram(
                "alpha beta gamma delta epsilon zeta",
                "one two three four five six",
            )
            == 0.0
        )

    def test_empty_is_zero(self):
        assert audit.overlap_5gram("", "the quick brown fox jumps over") == 0.0
        assert audit.overlap_5gram("short text", "short text") == 0.0

    def test_partial_between_zero_and_one(self):
        v = audit.overlap_5gram(
            "the quick brown fox jumps over the lazy dog",
            "the quick brown fox jumps over a high wall",
        )
        assert 0.0 < v < 1.0


class TestLocalizationProbe:
    def test_hit(self):
        issue = "Traceback points at src/foo.py line 12, null deref on load."
        patch = "diff --git a/src/foo.py b/src/foo.py\n+fix null guard\n"
        assert audit.localization_probe(issue, patch) is True

    def test_miss(self):
        issue = "Traceback points at src/foo.py line 12, null deref on load."
        patch = "diff --git a/src/bar.py b/src/bar.py\n+unrelated change\n"
        assert audit.localization_probe(issue, patch) is False

    def test_empty_is_false(self):
        assert audit.localization_probe("", "diff --git a/src/foo.py") is False
        assert audit.localization_probe("see src/foo.py", "") is False

    def test_no_path_in_issue_is_false(self):
        assert (
            audit.localization_probe(
                "it crashes sometimes, please fix", "diff --git a/src/foo.py"
            )
            is False
        )
