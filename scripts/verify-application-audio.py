# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 FREYM contributors
"""Independent PCM/packet/two-renderer verifier; no hardware capture or packages."""
import argparse
import array
import hashlib
import json
import math
from pathlib import Path
import sys


def require(condition, message):
    if not condition:
        raise ValueError(message)


def read_capture(directory, minimum_seconds):
    report = json.loads((directory / "capture.json").read_text(encoding="utf-8"))
    require(report["complete"] and report["cleanupJoined"], "Capture incomplete or not joined")
    require(report["sampleRate"] == 48000 and report["channels"] == 2, "Wrong PCM format")
    raw = (directory / "capture.f32le").read_bytes()
    require(len(raw) == report["frames"] * 8 == report["pcmBytes"], "PCM byte/frame mismatch")
    require(minimum_seconds * 48000 <= report["frames"] <= 31 * 48000, "PCM duration/bound mismatch")
    rows = [json.loads(line) for line in (directory / "packets.jsonl").read_text().splitlines()]
    require(len(rows) == report["blocks"] <= 30000, "Packet count/bound mismatch")
    frames = 0
    previous = None
    maximum_deviation = 0
    for index, row in enumerate(rows):
        require(row["index"] == index and row["sampleOffset"] == frames * 2, "Packet offset mismatch")
        require(0 < row["frames"] <= 4800, "Packet exceeds 100 ms")
        if previous:
            interval = row["timestampNs"] - previous["timestampNs"]
            require(interval > 0, "Timestamps not increasing")
            deviation = abs(interval - previous["frames"] * 1000000000 / 48000)
            maximum_deviation = max(maximum_deviation, deviation)
            require(deviation <= 1000000, "QPC interval beyond declared 1 ms tolerance")
        frames += row["frames"]
        previous = row
    require(frames == report["frames"], "Packet frames disagree with PCM")
    values = array.array("f")
    values.frombytes(raw)
    if sys.byteorder != "little":
        values.byteswap()
    require(all(math.isfinite(value) for value in values), "Nonfinite PCM")
    return report, values, {"pcmSha256": hashlib.sha256(raw).hexdigest(), "frames": frames,
                            "blocks": len(rows), "qpcMaximumDurationDeviationNs": maximum_deviation}


def amplitude(values, frequency, channel):
    # Seven whole steady seconds, after the first second: coherent projections at
    # integer frequencies, performed independently of native capture and its logs.
    samples = values[48000 * 2 + channel:8 * 48000 * 2:2]
    require(len(samples) == 7 * 48000, "Not enough steady PCM")
    real = imag = 0.0
    for index, value in enumerate(samples):
        phase = 2 * math.pi * frequency * index / 48000
        real += value * math.cos(phase)
        imag += value * math.sin(phase)
    return 2 * math.hypot(real, imag) / len(samples)


def verify(root):
    run = json.loads((root / "run.json").read_text())
    require(run["captureExits"] == [0, 0] and run["rendererExits"] == [0, 0], "A native run failed")
    require(run["restartExit"] == 0 and run["rootExitProbeExit"] != 0, "Restart/root-exit status mismatch")
    require(len(set(run["rendererPids"])) == 2, "Renderers must be independent processes")
    require(len(set(run["sourceIds"])) == 2, "Selected source identities must differ")
    for observation in ["sources-before.json", "sources-between.json", "sources-after.json"]:
        sources = json.loads((root / observation).read_text())
        for pid in run["rendererPids"]:
            require(any(source["processId"] == pid and source["isActive"] for source in sources),
                    f"Renderer {pid} not independently active at {observation}")
    measurements = []
    for hz, other in [(440, 997), (997, 440)]:
        events = [json.loads(line) for line in (root / f"render-{hz}.jsonl").read_text().splitlines()]
        require([event["event"] for event in events] == ["started", "finished"], "Incomplete renderer log")
        require(events[1]["framesSubmitted"] >= 20 * 48000 and events[1]["successfulWrites"] > 100,
                "Renderer did not keep submitting actual audio")
        report, samples, result = read_capture(root / f"capture-{hz}", 8)
        index = 0 if hz == 440 else 1
        pid = run["rendererPids"][index]
        require(events[0]["pid"] == pid and events[0]["toneHz"] == hz,
                "Renderer identity/frequency mismatch")
        require(report["sourceId"] == run["sourceIds"][index], "Captured wrong source identity")
        sources = json.loads((root / "sources-before.json").read_text())
        require(any(source["id"] == report["sourceId"] and source["processId"] == pid
                    for source in sources), "Source identity does not belong to renderer")
        for channel in range(2):
            selected = amplitude(samples, hz, channel)
            rejected = amplitude(samples, other, channel)
            require(selected > 0.01, "Selected signal absent; silence is inconclusive")
            rejection_db = 20 * math.log10(selected / max(rejected, 1e-15))
            require(rejection_db >= 40, "Unselected tone was not sufficiently isolated")
            result[f"channel{channel}"] = {"selectedAmplitude": selected,
                                             "unselectedAmplitude": rejected,
                                             "isolationDb": rejection_db}
        result["toneHz"] = hz
        measurements.append(result)
    _, _, restart = read_capture(root / "restart", 1)
    exit_report = json.loads((root / "root-exit" / "capture.json").read_text())
    require(not exit_report["complete"] and exit_report["cleanupJoined"], "Root-exit cleanup incomplete")
    require(exit_report["errorCode"] == "SourceLost" and "root process exited" in exit_report["error"],
            "Root exit was not the explicit source-local failure")
    return {"passed": True, "captures": measurements, "restart": restart,
            "rootExitFrames": exit_report["frames"], "scope": "two synthetic root processes; process-tree descendant inclusion not separately tested"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("evidence", type=Path)
    args = parser.parse_args()
    try:
        print(json.dumps(verify(args.evidence), indent=2))
    except (KeyError, ValueError, OSError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
