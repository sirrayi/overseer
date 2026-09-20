"""Harbor wall-clock parsing — Z/offset/space/naive/None (zero spend)."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from rig.benchmarks import harbor


class TestParseWall:
    def test_z_suffix(self):
        assert (
            harbor.parse_wall(
                "2026-09-16T22:07:14.633575Z", "2026-09-16T22:08:20.244405Z"
            )
            == 65.6
        )

    def test_offset_colon(self):
        assert (
            harbor.parse_wall(
                "2026-09-16T22:07:14+00:00", "2026-09-16T22:08:14+00:00"
            )
            == 60.0
        )

    def test_offset_equivalent_zones(self):
        # same instant in different zones → 0.0
        assert (
            harbor.parse_wall("2026-09-16T22:07:14+02:00", "2026-09-16T20:07:14Z")
            == 0.0
        )

    def test_offset_no_colon(self):
        assert (
            harbor.parse_wall(
                "2026-09-16T22:07:14+0000", "2026-09-16T22:08:14+0000"
            )
            == 60.0
        )

    def test_space_separator(self):
        assert (
            harbor.parse_wall("2026-09-16 22:07:14", "2026-09-16 22:08:14") == 60.0
        )

    def test_naive_assumed_utc(self):
        assert harbor.parse_wall("2026-09-16T22:07:14", "2026-09-16T22:07:14Z") == 0.0

    def test_none_inputs(self):
        assert harbor.parse_wall(None, "2026-09-16T22:07:14Z") is None
        assert harbor.parse_wall("2026-09-16T22:07:14Z", None) is None
        assert harbor.parse_wall(None, None) is None

    def test_garbage_is_none(self):
        assert harbor.parse_wall("garbage", "also garbage") is None
        assert harbor.parse_wall("", "") is None
