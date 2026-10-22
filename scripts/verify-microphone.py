#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 FREYM contributors
"""Transparent PCM verifier for FRAME-0105 microphone proofs.

Checks one microphone proof directory produced by ``microphone_probe``: a raw
``capture.f32le`` file (interleaved little-endian f32) plus a ``capture.meta.json`` with
``format`` / ``timestamp`` / ``count`` metadata. It is deliberately simple and readable:

* full byte/frame counts (PCM length == frames * channels * 4 == totals, block offsets
  contiguous),
* every sample is finite (no NaN/Inf), without clipping valid float samples,
* per-block timestamps strictly increase and advance by the block's frame duration,
* duration agrees with the requested seconds within 0.25 seconds,
* RMS and peak are measured and reported. Silence is valid PCM; signal energy alone
  cannot establish its origin or prove that a live capture took place.

A ``--expect-failure`` proof (the bounded-queue pressure run) is checked the other way: it
must report a queue overflow, discarded data, completed cleanup and no completion.

``prove`` is the bounded parent: it spawns the proof command with a finite timeout and then
verifies what that run wrote. ``verify`` re-checks an existing proof under a finite
analysis deadline. Exit codes: 0 pass, 1 fail, 2 timeout/usage error.
"""

from __future__ import annotations

import argparse
import json
import math
import re
import struct
import subprocess
import sys
import time
from pathlib import Path

DEFAULT_TIMEOUT_SECONDS = 120.0
DURATION_TOLERANCE_SECONDS = 0.25


class Timeout(Exception):
    pass


class Checker:
    def __init__(self, deadline: float) -> None:
        self.deadline = deadline
        self.failures: list[str] = []
        self.lines: list[str] = []

    def check(self, ok: bool, message: str) -> bool:
        tag = "PASS" if ok else "FAIL"
        self.lines.append(f"[{tag}] {message}")
        if not ok:
            self.failures.append(message)
        return ok

    def note(self, message: str) -> None:
        self.lines.append(f"[info] {message}")

    def tick(self) -> None:
        if time.monotonic() > self.deadline:
            raise Timeout()


def diagnostic_int(diagnostics: str, name: str) -> int | None:
    match = re.search(rf"\b{re.escape(name)}=(\d+)\b", diagnostics)
    return int(match.group(1)) if match else None


def verify_proof(meta_path: Path, expect_failure: bool, timeout_seconds: float) -> int:
    deadline = time.monotonic() + timeout_seconds
    c = Checker(deadline)

    try:
        meta = json.loads(meta_path.read_text(encoding="utf-8"))
    except Exception as error:  # noqa: BLE001 - report any load problem as a failure
        print(f"[FAIL] could not read metadata {meta_path}: {error}")
        return 1

    c.note(f"metadata: {meta_path}")
    status = meta.get("status")
    complete = bool(meta.get("complete"))
    error_text = meta.get("error")
    totals = meta.get("totals", {})
    blocks = meta.get("blocks", [])
    fmt = meta.get("format", {}) or {}
    diagnostics = meta.get("diagnostics", "")
    if not isinstance(diagnostics, str):
        diagnostics = ""

    if expect_failure:
        # Queue pressure must name the actual bounded-queue failure and account for
        # discarded PCM after a joined cleanup.
        c.check(status == "failed", f"status is 'failed' (got {status!r})")
        c.check(not complete, "complete is false")
        c.check(
            isinstance(error_text, str) and error_text.strip() != "",
            "an explicit non-empty failure reason is recorded",
        )
        if isinstance(error_text, str):
            c.note(f"failure: {error_text}")
        c.check(isinstance(error_text, str) and "queue" in error_text.lower(),
                "failure identifies the microphone queue")
        overflows = diagnostic_int(diagnostics, "queue_overflows")
        discarded = diagnostic_int(diagnostics, "discarded_bytes")
        c.check(overflows is not None and overflows > 0,
                f"queue_overflows > 0 (got {overflows})")
        c.check(discarded is not None and discarded > 0,
                f"discarded_bytes > 0 after cleanup (got {discarded})")
        c.check("released=true worker_finished=true" in diagnostics,
                "session was released and worker finished")
        c.check("closed=true" in diagnostics, "queue was closed")
        c.check(diagnostic_int(diagnostics, "queued_blocks") == 0
                and diagnostic_int(diagnostics, "held_bytes") == 0,
                "cleanup left no queued blocks or held bytes")
        _report_result(c)
        return 1 if c.failures else 0

    # Recording proof: complete and fully self-consistent.
    c.check(status == "complete", f"status is 'complete' (got {status!r})")
    c.check(complete, "complete is true")
    c.check(error_text in (None, ""), f"no failure recorded (got {error_text!r})")

    encoding = fmt.get("encoding")
    sample_rate = fmt.get("sample_rate")
    channels = fmt.get("channels")
    bytes_per_sample = fmt.get("bytes_per_sample", 4)
    requested_seconds = meta.get("requested_seconds")
    c.check(encoding == "f32le", f"encoding is f32le (got {encoding!r})")
    c.check(
        type(sample_rate) is int and sample_rate > 0,
        f"sample_rate is a positive integer (got {sample_rate!r})",
    )
    c.check(
        type(channels) is int and channels > 0,
        f"channels is a positive integer (got {channels!r})",
    )
    c.check(bytes_per_sample == 4, f"bytes_per_sample is 4 (got {bytes_per_sample!r})")
    c.check(type(requested_seconds) is int and 1 <= requested_seconds <= 300,
            f"requested_seconds is in 1..=300 (got {requested_seconds!r})")
    if not (type(sample_rate) is int and sample_rate > 0
            and type(channels) is int and channels > 0
            and bytes_per_sample == 4
            and type(requested_seconds) is int and 1 <= requested_seconds <= 300):
        _report_result(c)
        return 1
    frame_align = channels * bytes_per_sample

    pcm_path = meta_path.parent / meta.get("pcm_file", "capture.f32le")
    if not c.check(pcm_path.is_file(), f"PCM file exists: {pcm_path}"):
        _report_result(c)
        return 1
    data = pcm_path.read_bytes()
    c.tick()

    total_bytes = totals.get("bytes")
    total_frames = totals.get("frames")
    total_blocks = totals.get("blocks")
    if not c.check(type(total_frames) is int and total_frames > 0
                   and type(total_blocks) is int and total_blocks > 0
                   and isinstance(blocks, list), "positive frame/block totals and block list"):
        _report_result(c)
        return 1
    c.check(total_bytes == len(data), f"totals.bytes == PCM length ({total_bytes} == {len(data)})")
    c.check(
        len(data) == total_frames * frame_align,
        f"PCM length == frames * channels * bytes ({len(data)} == {total_frames} * {frame_align})",
    )
    c.check(total_blocks == len(blocks), f"totals.blocks == len(blocks) ({total_blocks} == {len(blocks)})")
    duration_seconds = total_frames / sample_rate
    c.check(abs(duration_seconds - requested_seconds) <= DURATION_TOLERANCE_SECONDS,
            f"PCM duration {duration_seconds:.6f}s is within {DURATION_TOLERANCE_SECONDS}s of requested {requested_seconds}s")

    # Block layout: contiguous offsets, exact frame counts, monotonic timestamps that
    # advance by each block's frame duration.
    expected_offset = 0
    sum_frames = 0
    prev_ts = None
    ts_ok = True
    off_ok = True
    for i, block in enumerate(blocks):
        c.tick()
        if not isinstance(block, dict):
            c.check(False, f"block {i} is an object")
            _report_result(c)
            return 1
        frames = block.get("frames")
        offset = block.get("byte_offset")
        ts = block.get("timestamp_ns")
        if not (type(frames) is int and frames > 0 and type(offset) is int
                and offset >= 0 and type(ts) is int and ts > 0):
            c.check(False, f"block {i} has positive integral frames/timestamp and nonnegative offset")
            _report_result(c)
            return 1
        if offset != expected_offset:
            off_ok = False
        expected_offset += frames * frame_align
        sum_frames += frames
        if prev_ts is not None:
            delta_ns = ts - prev_ts
            if delta_ns <= 0:
                ts_ok = False
            # Timestamps are first-sample times: the gap equals the PREVIOUS block's
            # duration. Tolerate QPC-anchor jitter (2 ms or 50% of a block) but still
            # catch a missing/overlapping block (a whole block off) and non-increase.
            if abs(delta_ns - prev_frames_ns) > max(2_000_000, 0.5 * prev_frames_ns):
                ts_ok = False
        prev_frames_ns = frames * 1_000_000_000 / sample_rate
        prev_ts = ts
    c.check(off_ok, "block byte offsets are contiguous from 0")
    c.check(expected_offset == len(data), "block frame/byte sizes cover the whole PCM file")
    c.check(sum_frames == total_frames, f"sum(block.frames) == totals.frames ({sum_frames} == {total_frames})")
    c.check(ts_ok, "block timestamps are strictly increasing and frame-consistent")
    c.check(
        all(b.get("timestamp_ns", 0) > 0 for b in blocks) or total_blocks == 0,
        "block timestamps are positive media times",
    )

    # Sample scan: finite, peak, RMS. Signal level does not authenticate a source.
    count = len(data) // 4
    peak = 0.0
    square_sum = 0.0
    all_finite = True
    for start in range(0, count, 4096):
        c.tick()
        chunk = data[start * 4 : (start + 4096) * 4]
        n = len(chunk) // 4
        if n == 0:
            break
        values = struct.unpack(f"<{n}f", chunk[: n * 4])
        for v in values:
            if not math.isfinite(v):
                all_finite = False
                continue
            a = abs(v)
            if a > peak:
                peak = a
            square_sum += v * v
    rms = math.sqrt(square_sum / count) if count else 0.0
    c.check(all_finite, "every sample is finite (no NaN/Inf)")
    c.check(count > 0, f"the PCM file carries samples (got {count})")
    c.note(f"RMS = {rms:.6f}  peak = {peak:.6f}  samples = {count}")
    c.note("RMS/peak describe sample energy only; silence is valid PCM and origin is not established")

    _report_result(c)
    return 1 if c.failures else 0


def _report_result(c: Checker) -> None:
    for line in c.lines:
        print(line)
    if c.failures:
        print(f"RESULT: FAIL ({len(c.failures)} check(s) failed)")
    else:
        print("RESULT: PASS")


def run_prove(args: argparse.Namespace) -> int:
    """Bounded parent: run the proof command with a finite timeout, then verify it."""
    command = args.command
    if not command:
        print("[FAIL] prove needs a command after '--'")
        return 2
    output_dir = Path(args.output_dir)
    meta_path = output_dir / "capture.meta.json"
    print(f"[info] running (finite parent timeout {args.timeout_seconds}s): {command}")
    try:
        proc = subprocess.run(
            command,
            timeout=args.timeout_seconds,
            capture_output=True,
            text=True,
        )
    except subprocess.TimeoutExpired:
        print(f"[FAIL] proof command exceeded the finite parent timeout ({args.timeout_seconds}s)")
        return 2
    print(f"[info] proof command exit={proc.returncode}")
    if proc.stdout.strip():
        print("[info] stdout tail:")
        for line in proc.stdout.strip().splitlines()[-20:]:
            print(f"  {line}")
    if proc.stderr.strip():
        print("[info] stderr tail:")
        for line in proc.stderr.strip().splitlines()[-20:]:
            print(f"  {line}")
    if not args.expect_failure and proc.returncode != 0:
        print("[FAIL] the proof command failed but a clean recording was expected")
        return 1
    if args.expect_failure and proc.returncode == 0:
        print("[FAIL] the proof command exited successfully but a queue-pressure failure was expected")
        return 1
    return verify_proof(meta_path, args.expect_failure, args.timeout_seconds)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, add_help=True)
    sub = parser.add_subparsers(dest="mode", required=True)

    v = sub.add_parser("verify", help="verify an existing proof directory")
    v.add_argument("--meta", required=True, help="path to capture.meta.json")
    v.add_argument("--expect-failure", action="store_true")
    v.add_argument("--timeout-seconds", type=float, default=DEFAULT_TIMEOUT_SECONDS)

    p = sub.add_parser("prove", help="run a proof command with a finite timeout, then verify")
    p.add_argument("--output-dir", required=True)
    p.add_argument("--expect-failure", action="store_true")
    p.add_argument("--timeout-seconds", type=float, default=DEFAULT_TIMEOUT_SECONDS)
    p.add_argument("command", nargs=argparse.REMAINDER)

    args = parser.parse_args(argv)
    if args.mode == "verify":
        return verify_proof(Path(args.meta), args.expect_failure, args.timeout_seconds)
    if args.command and args.command[0] == "--":
        args.command = args.command[1:]
    return run_prove(args)


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except Timeout:
        print("[FAIL] verification exceeded its finite timeout")
        sys.exit(2)
