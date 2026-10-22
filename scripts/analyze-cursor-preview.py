"""Summarize opt-in JSONL evidence; no capture, application or GUI operation."""
import argparse
from collections import Counter, defaultdict
import json
import math
from pathlib import Path


def distribution(values):
    values = sorted(values)
    if not values:
        return None
    return {"samples": len(values), "p50_ms": values[math.ceil(len(values) * .50) - 1] / 1e6,
            "p95_ms": values[math.ceil(len(values) * .95) - 1] / 1e6,
            "p99_ms": values[math.ceil(len(values) * .99) - 1] / 1e6,
            "max_ms": values[-1] / 1e6}


def cadence(times):
    times = sorted(times)
    seconds = (times[-1] - times[0]) / 1e9 if len(times) > 1 else 0
    return {"events": len(times), "observed_seconds": seconds,
            "rate_hz": (len(times) - 1) / seconds if seconds > 0 else None,
            "intervals": distribution([b - a for a, b in zip(times, times[1:])])}


def summarize(rows, completeness):
    counts = Counter()
    sources = defaultdict(lambda: {"acquired": [], "completed": [], "native_times": set(),
                                  "compositions": Counter(), "drops": Counter(),
                                  "frame_times": {}, "cpu_spans": defaultdict(list), "last_counters": None})
    sinks = defaultdict(lambda: {"presents": [], "claims": [], "capture_age": [],
                                "last_sequence": None, "last_capture": {},
                                "duplicate_or_reversed_sequence": 0,
                                "duplicate_or_reversed_capture": 0, "outcomes": Counter(),
                                "cpu_spans": defaultdict(list), "refresh_interval_ns": None,
                                "native_age": [], "gpu_timing": None})
    for row in sorted(rows, key=lambda row: row.get("at_ns", 0)):
        event = row["event"]
        counts[event] += 1
        fields = row.get("fields", {})
        frame = row.get("frame")
        if frame:
            source = sources[str(frame["source_id"])]
            if event in ("dxgi.acquire", "wgc.frame_received"):
                source["acquired"].append(row["at_ns"])
            if event in ("dxgi.acquire", "wgc.source_time"):
                native = fields.get("last_present_qpc", fields.get("frame_qpc"))
                if native and native > 0:
                    source["native_times"].add((native, fields.get("qpc_frequency")))
                visual = max(native or 0, fields.get("last_mouse_update_qpc", 0) if fields.get("show_cursor") else 0)
                hz = fields.get("qpc_frequency")
                if visual > 0 and hz and hz > 0:
                    source["frame_times"][frame["sequence"]] = visual * 1_000_000_000 // hz
            if event == "capture_complete":
                source["completed"].append(row["at_ns"])
                source["last_counters"] = fields.get("counters")
            for name in ("copy_and_cursor_commands_cpu_ns", "slot_copy_commands_cpu_ns", "copy_commands_cpu_ns",
                         "flush_commands_cpu_ns", "draw_surface_cpu_ns", "copy_completion_ns"):
                value = fields.get(name)
                if isinstance(value, int):
                    source["cpu_spans"][f"{event}.{name}"].append(value)
            # Count actual successful DrawIconEx calls only, not summaries at
            # composition/copy/release which describe the same draw again.
            if event == "dxgi.draw_icon" and fields.get("draw_icon_success"):
                source["compositions"][frame["sequence"]] += 1
            if event == "capture_drop":
                source["drops"][fields["reason"]] += 1
        if "sink_id" not in fields:
            continue
        key = f'{row["process_id"]}:{fields["sink_id"]}:{fields.get("generation")}'
        sink = sinks[key]
        if event == "preview_admission":
            sink["refresh_interval_ns"] = fields.get("refresh_interval_ns")
            sink["outcomes"][f'admission_{fields.get("outcome")}'] += 1
        if event == "preview_claim":
            sink["claims"].append(row["at_ns"])
        if event in ("preview_consumer_busy", "preview_producer_busy", "preview_hidden_discard"):
            sink["outcomes"][event] += 1
        if event == "preview_publish" and fields.get("replaced_pending"):
            sink["outcomes"]["replaced_pending"] += 1
        for name in ("copy_submit_ns", "flush_ns", "present_cpu_ns"):
            value = fields.get(name)
            if isinstance(value, int):
                sink["cpu_spans"][f"{event}.{name}"].append(value)
        if event != "preview_present":
            continue
        # Positive DXGI status (e.g. OCCLUDED) is not counted as displayed.
        status = fields.get("status")
        if "presenter_gpu_span" in fields:
            sink["gpu_timing"] = {name: fields.get(name) for name in (
                "presenter_gpu_span", "gpu_timing_unavailable", "gpu_timing_invalid",
                "gpu_timing_busy", "gpu_timing_pending", "gpu_span_scope")}
        sink["outcomes"][f"present_hresult_{status}"] += 1
        if status != 0:
            continue
        sink["presents"].append(row["at_ns"])
        sequence = fields["preview_sequence"]
        if sink["last_sequence"] is not None and sequence <= sink["last_sequence"]:
            sink["duplicate_or_reversed_sequence"] += 1
        sink["last_sequence"] = sequence
        if frame:
            previous = sink["last_capture"].get(frame["source_id"])
            if previous is not None and frame["sequence"] <= previous:
                sink["duplicate_or_reversed_capture"] += 1
            sink["last_capture"][frame["source_id"]] = frame["sequence"]
            native_ns = sources[str(frame["source_id"])]["frame_times"].get(frame["sequence"])
            if native_ns is not None:
                sink["native_age"].append(row["at_ns"] - native_ns)
        age = fields.get("capture_to_present_submit_ns")
        if isinstance(age, int):
            sink["capture_age"].append(age)
    source_result = {}
    for key, source in sources.items():
        native_ns = [ticks * 1_000_000_000 // hz for ticks, hz in source["native_times"] if hz and hz > 0]
        source_result[key] = {
            "acquired_visual_updates": cadence(source["acquired"]),
            "completed_visual_copies": cadence(source["completed"]),
            "distinct_native_desktop_timestamps": cadence(native_ns),
            "successful_software_draws": sum(source["compositions"].values()),
            "frames_with_multiple_software_draws": sum(count > 1 for count in source["compositions"].values()),
            "drop_reasons": dict(source["drops"]),
            "cpu_command_and_completion_spans": {name: distribution(values) for name, values in source["cpu_spans"].items()},
            "last_capture_counters": source["last_counters"],
        }
    sink_result = {}
    for key, sink in sinks.items():
        sink_result[key] = {
            "configured_ceiling_hz": 1e9 / sink["refresh_interval_ns"] if sink["refresh_interval_ns"] else None,
            "successful_present_submissions": cadence(sink["presents"]),
            "mailbox_claims": cadence(sink["claims"]),
            "capture_acquisition_to_present_submission": distribution(sink["capture_age"]),
            "native_visual_timestamp_to_present_event": distribution(sink["native_age"]),
            "duplicate_or_reversed_sequence": sink["duplicate_or_reversed_sequence"],
            "duplicate_or_reversed_capture": sink["duplicate_or_reversed_capture"],
            "outcomes": dict(sink["outcomes"]),
            "cpu_command_spans": {name: distribution(values) for name, values in sink["cpu_spans"].items()},
            "last_cumulative_gpu_timing": sink["gpu_timing"],
        }
    return {"files": completeness, "complete_without_loss": all(item["complete_without_loss"] for item in completeness),
            "event_counts": dict(counts), "sources": source_result, "sinks": sink_result,
            "limits": ["No pixel embedding or cursor ghost detection; embedded_in_pixels remains unknown.",
                       "Present success measures submission, not physical display scanout.",
                       "Opt-in logging/timing has overhead; rates are observed spans, not a benchmark.",
                       "Missing footer, truncation or dropped events invalidate absence-of-defect conclusions."]}


def load(paths):
    rows, completeness = [], []
    for path in paths:
        data = [json.loads(line) for line in Path(path).read_text(encoding="utf-8").splitlines() if line.strip()]
        footer = next((row for row in reversed(data) if row["event"] == "trace_end"), None)
        header = next((row for row in data if row["event"] == "trace_start"), None)
        completeness.append({"path": str(path), "complete_without_loss": bool(header and header.get("qpc_frequency", 0) > 0 and footer
                             and footer.get("dropped_events") == 0 and not footer.get("budget_exhausted")), "footer": footer})
        rows.extend(row for row in data if row["event"] not in ("trace_start", "trace_end"))
    return summarize(rows, completeness)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("traces", nargs="+")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    summary = json.dumps(load(args.traces), indent=2) + "\n"
    if args.output:
        with args.output.open("x", encoding="utf-8") as output:
            output.write(summary)
    else:
        print(summary, end="")
