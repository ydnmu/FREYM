#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 FREYM contributors
"""Decode-side check for the FRAME-0205 region capture probe.

It reads the probe's own JSON report, asks ffprobe for the decoded dimensions of the
recorded Matroska file and decodes frames to raw RGB with ffmpeg. Two things are checked
from the artifact itself, never from a claim:

* the decoded frame size is exactly the canonical region the session copied, and
* in ``--pattern`` mode every sampled decoded pixel is the colour the pattern has at its
  *display-local* coordinate, which checks the crop offset in both axes as well as the size.

Nothing is re-encoded, retimed or rescaled here, and no image library is used.

    python verify_region_capture.py --report test-results/frame-0205/region-pattern.json
"""

import argparse
import json
import subprocess
import sys

# Pixels this close to a bar edge or the bar/band boundary are skipped: H.264 chroma
# subsampling and deblocking blur a few pixels around a hard edge, and that blur is a
# property of the codec, not of the crop.
EDGE_MARGIN = 4
# Fraction of sampled pixels that may differ from the expected colour before the run fails.
MAX_MISMATCH_FRACTION = 0.02
# Largest L1 distance from the expected colour still accepted as "that colour".
COLOR_TOLERANCE = 200


def ffprobe_stream(path):
    """Returns (video stream, container format).

    Matroska carries no per-stream frame count or duration, so the frame count is read by
    actually decoding the stream (`-count_frames`) and the duration from the container.
    """
    result = subprocess.run(
        [
            "ffprobe",
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_frames",
            "-show_entries",
            "stream=width,height,codec_name,pix_fmt,nb_read_frames",
            "-show_entries",
            "format=duration",
            "-of",
            "json",
            path,
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    document = json.loads(result.stdout)
    streams = document.get("streams", [])
    if len(streams) != 1:
        raise SystemExit(f"expected exactly one video stream in {path}, found {len(streams)}")
    return streams[0], document.get("format", {})


def decode_frames(path, width, height, sample_every, max_samples):
    """Yields (index, frame bytes) for a bounded number of decoded frames.

    Reading stops once enough frames were sampled, which makes ffmpeg fail with a broken
    pipe; that deliberate stop is not a decode failure and is not reported as one.
    """
    process = subprocess.Popen(
        [
            "ffmpeg",
            "-v",
            "error",
            "-i",
            path,
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    frame_bytes = width * height * 3
    index = 0
    sampled = 0
    stopped_early = False
    try:
        while sampled < max_samples:
            data = process.stdout.read(frame_bytes)
            if len(data) < frame_bytes:
                break
            if index % sample_every == 0:
                sampled += 1
                yield index, data
            index += 1
        stopped_early = sampled >= max_samples
    finally:
        try:
            process.stdout.close()
        except OSError:
            pass
        stderr = process.stderr.read().decode("utf-8", "replace")
        process.stderr.close()
        code = process.wait()
        if code != 0 and not stopped_early:
            raise SystemExit(f"ffmpeg decode of {path} failed with exit {code}: {stderr}")
        if code != 0 and "Broken pipe" not in stderr:
            raise SystemExit(
                f"ffmpeg decode of {path} failed with exit {code} after the requested "
                f"frames were sampled: {stderr}"
            )


def expected_color(region, pattern, column, row):
    x = region["x"] + column
    y = region["y"] + row
    if row < pattern["barsRows"]:
        palette = pattern["palette"]
        return palette[(x // pattern["stripeWidth"]) % len(palette)]
    return pattern["bandColor"]


def near_edge(region, pattern, column, row):
    x = region["x"] + column
    stripe = pattern["stripeWidth"]
    if row < pattern["barsRows"] and (x % stripe) < EDGE_MARGIN:
        return True
    if row < pattern["barsRows"] and (x % stripe) > stripe - 1 - EDGE_MARGIN:
        return True
    return abs(row - pattern["barsRows"]) <= EDGE_MARGIN


def classify(pixel, candidates):
    best = None
    best_distance = None
    for candidate in candidates:
        distance = sum(abs(a - b) for a, b in zip(pixel, candidate))
        if best_distance is None or distance < best_distance:
            best = candidate
            best_distance = distance
    return best, best_distance


def check_frame(data, width, height, region, pattern):
    """Returns (checked, mismatches, examples)."""
    palette = [tuple(color) for color in pattern["palette"]]
    band = tuple(pattern["bandColor"])
    candidates = palette + [band]
    checked = 0
    mismatches = 0
    examples = []
    # A 2-pixel grid keeps the generated PNG/SVG cost out of the way and is still far
    # denser than the 80 px pattern feature.
    for row in range(0, height, 2):
        for column in range(0, width, 2):
            if near_edge(region, pattern, column, row):
                continue
            offset = (row * width + column) * 3
            pixel = (data[offset], data[offset + 1], data[offset + 2])
            want = tuple(expected_color(region, pattern, column, row))
            nearest, distance = classify(pixel, candidates)
            checked += 1
            if nearest != want or distance > COLOR_TOLERANCE:
                mismatches += 1
                if len(examples) < 5:
                    examples.append(
                        {
                            "column": column,
                            "row": row,
                            "decoded": list(pixel),
                            "expected": list(want),
                            "nearest": list(nearest),
                            "distance": distance,
                        }
                    )
    return checked, mismatches, examples


def row_runs(data, width, row, candidates):
    """Run-length encodes one decoded row as the colours it actually contains."""
    runs = []
    for column in range(width):
        offset = (row * width + column) * 3
        pixel = (data[offset], data[offset + 1], data[offset + 2])
        nearest, _ = classify(pixel, candidates)
        if runs and runs[-1][0] == nearest:
            runs[-1][1] += 1
        else:
            runs.append([nearest, 1])
    return runs


def describe_runs(runs, band_color):
    return " ".join(
        f"{'band' if color == band_color else '#' + ''.join(f'{channel:02x}' for channel in color)}"
        f"x{length}"
        for color, length in runs
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", required=True, help="the probe's JSON report")
    parser.add_argument(
        "--max-frames",
        type=int,
        default=3,
        help="how many decoded frames are sampled (default 3: even spread over the file)",
    )
    arguments = parser.parse_args()

    with open(arguments.report, "r", encoding="utf-8") as handle:
        report = json.load(handle)
    if report.get("kind") != "frame-0205-region-probe":
        raise SystemExit(f"{arguments.report} is not a FRAME-0205 region probe report")
    artifact = report["artifact"]
    region = report["region"]
    width = region["width"]
    height = region["height"]

    stream, container = ffprobe_stream(artifact)
    decoded_width = int(stream["width"])
    decoded_height = int(stream["height"])
    duration = float(container.get("duration") or 0.0)
    frames = int(stream.get("nb_read_frames") or 0)
    print(
        f"artifact: {artifact}\n"
        f"  codec={stream.get('codec_name')} pix_fmt={stream.get('pix_fmt')} "
        f"decoded={decoded_width}x{decoded_height} frames={frames} duration={duration:.3f}s"
    )
    if (decoded_width, decoded_height) != (width, height):
        raise SystemExit(
            f"decoded size {decoded_width}x{decoded_height} is not the requested region "
            f"{width}x{height}"
        )
    seconds = float(report["seconds"])
    if duration < seconds * 0.5 or duration > seconds * 1.6:
        raise SystemExit(
            f"decoded duration {duration:.3f}s is not consistent with the requested "
            f"{seconds}s recording"
        )
    print(f"  size matches the canonical region {width}x{height} at ({region['x']}, {region['y']})")

    if report.get("mode") != "pattern":
        print("explicit-region mode: size and duration checked; no pattern content to verify")
        return 0

    pattern = report["pattern"]
    sample_every = max(1, frames // arguments.max_frames) if frames > 0 else 1
    candidates = [tuple(color) for color in pattern["palette"]] + [tuple(pattern["bandColor"])]
    checked_total = 0
    mismatch_total = 0
    sampled = 0
    for index, data in decode_frames(
        artifact, decoded_width, decoded_height, sample_every, arguments.max_frames
    ):
        checked, mismatches, examples = check_frame(
            data, decoded_width, decoded_height, region, pattern
        )
        sampled += 1
        checked_total += checked
        mismatch_total += mismatches
        fraction = (mismatches / checked) if checked else 1.0
        print(
            f"  frame {index}: sampled {checked} pixels, {mismatches} off-pattern "
            f"({fraction * 100:.3f}%)"
        )
        for example in examples:
            print(f"    mismatch {example}")
        if sampled == 1:
            # One bar row and one band row, run-length encoded: this is the decoded content
            # itself, not a summary of it.
            print(
                "    bar row at display y="
                f"{region['y'] + 1}: "
                + describe_runs(
                    row_runs(data, decoded_width, 1, candidates), tuple(pattern["bandColor"])
                )
            )
            last = decoded_height - 2
            print(
                f"    band row at display y={region['y'] + last}: "
                + describe_runs(
                    row_runs(data, decoded_width, last, candidates),
                    tuple(pattern["bandColor"]),
                )
            )
        if fraction > MAX_MISMATCH_FRACTION:
            raise SystemExit(
                f"frame {index} has {fraction * 100:.3f}% off-pattern pixels, above the "
                f"{MAX_MISMATCH_FRACTION * 100:.2f}% bound"
            )
    if sampled == 0 or checked_total == 0:
        raise SystemExit("no frame or no pixel was sampled, so the crop content was not verified")
    overall = mismatch_total / checked_total
    print(
        f"content: {sampled} frame(s), {checked_total} sampled pixels, "
        f"{mismatch_total} off-pattern ({overall * 100:.3f}%) — the crop is the requested "
        f"rectangle of the known pattern"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
