"""Evidence interpretation tests; synthetic rows only, no GUI or capture."""
import json
from pathlib import Path
import runpy
import tempfile
import unittest

ANALYZER = runpy.run_path(str(Path(__file__).resolve().parents[1] / "scripts/analyze-cursor-preview.py"))


def row(event, at, sequence=1, **fields):
    return {"event": event, "at_ns": at, "process_id": 7,
            "frame": {"source_id": 71, "sequence": sequence, "acquired_ns": at - 10}, "fields": fields}


class CursorPreviewEvidence(unittest.TestCase):
    def test_composition_summary_does_not_double_count_one_real_draw(self):
        rows = [row("dxgi.draw_icon", 100, draw_icon_success=True, software_composition_count=1),
                row("dxgi.cursor_composition", 101, software_composition_count=1),
                row("dxgi.copy_submitted", 102, software_composition_count=1),
                row("dxgi.release", 103, software_composition_count=1)]
        source = ANALYZER["summarize"](rows, [])["sources"]["71"]
        self.assertEqual(source["successful_software_draws"], 1)
        self.assertEqual(source["frames_with_multiple_software_draws"], 0)
        rows.append(row("dxgi.draw_icon", 104, draw_icon_success=True))
        self.assertEqual(ANALYZER["summarize"](rows, [])["sources"]["71"]["frames_with_multiple_software_draws"], 1)

    def test_rates_and_stale_source_identity_are_reported_separately(self):
        rows = [row("preview_admission", 1, sink_id=4, generation=2, outcome="admitted", refresh_interval_ns=33_333_334)]
        for index, capture in enumerate([1, 2, 2, 1]):
            rows.append(row("preview_present", 1_000_000_000 + index * 50_000_000, capture,
                            sink_id=4, generation=2, preview_sequence=index + 1, status=0,
                            capture_to_present_submit_ns=10_000_000))
        sink = ANALYZER["summarize"](rows, [])["sinks"]["7:4:2"]
        self.assertAlmostEqual(sink["configured_ceiling_hz"], 30, places=5)
        self.assertEqual(sink["successful_present_submissions"]["rate_hz"], 20)
        self.assertEqual(sink["duplicate_or_reversed_sequence"], 0)
        self.assertEqual(sink["duplicate_or_reversed_capture"], 2)
        self.assertEqual(sink["capture_acquisition_to_present_submission"]["p95_ms"], 10)

    def test_positive_status_and_busy_are_not_displayed_fps(self):
        rows = [row("preview_present", 1, sink_id=4, generation=2, preview_sequence=1, status=0x087A0001),
                row("preview_present", 2, sink_id=4, generation=2, preview_sequence=2, status=-2005270518)]
        sink = ANALYZER["summarize"](rows, [])["sinks"]["7:4:2"]
        self.assertEqual(sink["successful_present_submissions"]["events"], 0)
        self.assertIsNone(sink["successful_present_submissions"]["rate_hz"])

    def test_missing_footer_or_event_loss_marks_evidence_incomplete(self):
        with tempfile.TemporaryDirectory(prefix="freym-trace-") as directory:
            path = Path(directory) / "fixture.jsonl"
            header = {"event": "trace_start", "qpc_frequency": 10_000_000}
            for footer, complete in [(None, False),
                                      ({"event": "trace_end", "dropped_events": 1, "budget_exhausted": False}, False),
                                      ({"event": "trace_end", "dropped_events": 0, "budget_exhausted": False}, True)]:
                data = [header, row("dxgi.acquire", 100, last_present_qpc=1, qpc_frequency=10_000_000)]
                if footer:
                    data.append(footer)
                path.write_text("\n".join(map(json.dumps, data)), encoding="utf-8")
                self.assertEqual(ANALYZER["load"]([path])["complete_without_loss"], complete)


if __name__ == "__main__":
    unittest.main()
