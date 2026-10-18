#!/usr/bin/env python3
"""Pure-logic checks for scripts/shellx/perf/perf_mac.py (runs on Linux CI).

Only the comparison gate and the output parsers are exercised here; the
measuring commands drive a real macOS app and are run by hand (see the
harness README).
"""

from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "shellx" / "perf" / "perf_mac.py"
spec = importlib.util.spec_from_file_location("shellx_perf_mac", SCRIPT)
assert spec is not None and spec.loader is not None
perf_mac = importlib.util.module_from_spec(spec)
spec.loader.exec_module(perf_mac)
compare = perf_mac.compare


class CompareTests(unittest.TestCase):
    def test_compare_flags_regression_over_ten_percent(self):
        base = {"metrics": {"cold_start_ms": 1000, "cat_100mb_ms": 500, "idle_rss_mb_4panes": 200, "idle_cpu_pct": 1.0, "keystroke_ms": 10}}
        cand = dict(metrics=dict(base["metrics"], cold_start_ms=1101))
        self.assertEqual(compare(base, cand, max_regression=0.10), ["cold_start_ms"])

    def test_compare_passes_at_exactly_ten_percent(self):
        base = {"metrics": {"cold_start_ms": 1000}}
        self.assertEqual(compare(base, {"metrics": {"cold_start_ms": 1100}}, 0.10), [])

    def test_compare_skips_metric_null_or_missing_on_either_side(self):
        base = {"metrics": {"keystroke_ms": None, "cat_100mb_ms": 500, "idle_cpu_pct": 1.0}}
        cand = {"metrics": {"keystroke_ms": 99.0, "cat_100mb_ms": None, "max_hitch_ms": 500.0}}
        self.assertEqual(compare(base, cand, 0.10), [])

    def test_compare_treats_every_metric_as_lower_is_better(self):
        base = {"metrics": {"idle_rss_mb_4panes": 200, "idle_cpu_pct": 1.0}}
        cand = {"metrics": {"idle_rss_mb_4panes": 100, "idle_cpu_pct": 1.2}}
        self.assertEqual(compare(base, cand, 0.10), ["idle_cpu_pct"])

    def test_compare_subcommand_exits_one_on_regression(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp) / "base.json"
            ok = Path(tmp) / "ok.json"
            bad = Path(tmp) / "bad.json"
            base.write_text(json.dumps({"metrics": {"cold_start_ms": 1000}}))
            ok.write_text(json.dumps({"metrics": {"cold_start_ms": 1100}}))
            bad.write_text(json.dumps({"metrics": {"cold_start_ms": 1101}}))

            def run(cand: Path) -> subprocess.CompletedProcess:
                return subprocess.run(
                    [sys.executable, str(SCRIPT), "compare", str(base), str(cand), "--max-regression", "0.10"],
                    capture_output=True,
                    text=True,
                )

            self.assertEqual(run(ok).returncode, 0)
            failed = run(bad)
            self.assertEqual(failed.returncode, 1)
            self.assertIn("cold_start_ms", failed.stdout + failed.stderr)


class ParserTests(unittest.TestCase):
    def test_parse_real_seconds_from_time_reports(self):
        self.assertAlmostEqual(perf_mac.parse_real_seconds("real 1.234\n"), 1.234)
        self.assertAlmostEqual(perf_mac.parse_real_seconds("real 1:02.500\n"), 62.5)
        self.assertAlmostEqual(perf_mac.parse_real_seconds("\nreal\t0m3.250s\nuser\t0m0.010s\n"), 3.25)
        self.assertIsNone(perf_mac.parse_real_seconds("no timing here"))

    def test_hitch_durations_resolve_refs_and_filter_by_pid(self):
        xml = """<?xml version="1.0"?>
<trace-query-result><node>
  <schema name="hitches"/>
  <row>
    <duration id="1" fmt="33.33 ms">33333333</duration>
    <process id="2" fmt="cmux DEV perf (4242)"><pid id="3" fmt="4242">4242</pid></process>
  </row>
  <row>
    <duration ref="1"/>
    <process ref="2"/>
  </row>
  <row>
    <duration id="4" fmt="50 ms">50000000</duration>
    <process id="5" fmt="WindowServer (99)"><pid id="6" fmt="99">99</pid></process>
  </row>
</node></trace-query-result>
"""
        durations = perf_mac.hitch_durations_ms(xml, {4242})
        self.assertEqual(len(durations), 2)
        self.assertAlmostEqual(sum(durations), 66.666666, places=4)


if __name__ == "__main__":
    unittest.main()
