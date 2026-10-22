# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 FREYM contributors
"""Focused verifier regressions; fixtures are synthetic and never hardware proof."""

import contextlib
import importlib.util
import io
import json
import struct
import tempfile
import unittest
from argparse import Namespace
from pathlib import Path


SPEC = importlib.util.spec_from_file_location(
    "verify_microphone", Path(__file__).with_name("verify-microphone.py")
)
verifier = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(verifier)


class VerifierTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.meta_path = self.directory / "capture.meta.json"
        self.meta = {
            "status": "complete", "complete": True, "error": None,
            "requested_seconds": 1, "pcm_file": "capture.f32le",
            "format": {"encoding": "f32le", "sample_rate": 48_000,
                       "channels": 1, "bytes_per_sample": 4},
            "totals": {"bytes": 192_000, "frames": 48_000, "blocks": 1},
            "blocks": [{"byte_offset": 0, "frames": 48_000,
                        "timestamp_ns": 1_000_000_000}],
        }
        (self.directory / "capture.f32le").write_bytes(struct.pack("<f", 0.0) * 48_000)
        self.save()

    def save(self):
        self.meta_path.write_text(json.dumps(self.meta), encoding="utf-8")

    def verify(self, expect_failure=False):
        with contextlib.redirect_stdout(io.StringIO()):
            return verifier.verify_proof(self.meta_path, expect_failure, 10)

    def test_silent_pcm_with_correct_duration_is_valid(self):
        self.assertEqual(self.verify(), 0)

    def test_duplicate_timestamps_are_rejected(self):
        self.meta["blocks"] = [
            {"byte_offset": 0, "frames": 24_000, "timestamp_ns": 1_000_000_000},
            {"byte_offset": 96_000, "frames": 24_000, "timestamp_ns": 1_000_000_000},
        ]
        self.meta["totals"]["blocks"] = 2
        self.save()
        self.assertEqual(self.verify(), 1)

    def test_short_capture_is_rejected(self):
        self.meta["totals"] = {"bytes": 19_200, "frames": 4_800, "blocks": 1}
        self.meta["blocks"][0]["frames"] = 4_800
        (self.directory / "capture.f32le").write_bytes(struct.pack("<f", 0.0) * 4_800)
        self.save()
        self.assertEqual(self.verify(), 1)

    def test_pressure_requires_overflow_and_cleanup_accounting(self):
        self.meta.update(status="failed", complete=False, error="source lost",
                         diagnostics="queue_overflows=0 discarded_bytes=0 closed=false")
        self.save()
        self.assertEqual(self.verify(True), 1)
        self.meta["error"] = "bounded queue is full"
        self.meta["diagnostics"] = (
            "released=true worker_finished=true\n"
            "queue_overflows=1 discarded_bytes=122880\n"
            "queued_blocks=0 held_bytes=0 closed=true"
        )
        self.save()
        self.assertEqual(self.verify(True), 0)

    def test_prove_expected_failure_needs_nonzero_child_exit(self):
        self.meta.update(status="failed", complete=False, error="bounded queue is full",
                         diagnostics="released=true worker_finished=true\n"
                                     "queue_overflows=1 discarded_bytes=1\n"
                                     "queued_blocks=0 held_bytes=0 closed=true")
        self.save()
        import sys
        args = Namespace(output_dir=str(self.directory), expect_failure=True,
                         timeout_seconds=10, command=[sys.executable, "-c", "pass"])
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(verifier.run_prove(args), 1)


if __name__ == "__main__":
    unittest.main()
