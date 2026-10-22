#!/usr/bin/env python3
"""Read-only stream/decode check for one short finalized FREYM MKV.

This does not create a recording, repair/remux media, measure live performance,
or infer content synchronization from stream timestamps. Run after Stop.
"""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys


def run(arguments, timeout):
    result = subprocess.run(arguments, stdin=subprocess.DEVNULL, capture_output=True,
                            text=True, encoding="utf-8", errors="replace", timeout=timeout, check=False)
    if result.returncode:
        raise ValueError(f"{Path(arguments[0]).name} failed ({result.returncode}): {result.stderr[-4000:].strip()}")
    return result.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", type=Path, help="One finalized MKV segment, ideally 10–20 seconds")
    parser.add_argument("--expected-audio-tracks", type=int, help="Expected Opus tracks from the selected routing plan")
    parser.add_argument("--max-seconds", type=float, default=30, help="Refuse long files; default 30 seconds")
    args = parser.parse_args()
    if args.expected_audio_tracks is not None and args.expected_audio_tracks < 0:
        raise ValueError("Expected audio track count must be nonnegative.")
    if not 0 < args.max_seconds <= 120:
        raise ValueError("The verification duration limit must be 0–120 seconds.")
    path = args.path.resolve(strict=True)
    if not path.is_file() or path.suffix.lower() != ".mkv":
        raise ValueError("Choose a finalized MKV segment.")
    ffprobe, ffmpeg = shutil.which("ffprobe"), shutil.which("ffmpeg")
    if not ffprobe or not ffmpeg:
        raise ValueError("Install FFmpeg/ffprobe and put both on PATH to run the decoder check.")
    before = path.stat()
    probe = json.loads(run([ffprobe, "-v", "error", "-show_format", "-show_streams", "-of", "json", str(path)], 30))
    streams = probe.get("streams", [])
    video = [s for s in streams if s.get("codec_type") == "video"]
    audio = [s for s in streams if s.get("codec_type") == "audio"]
    if len(video) != 1 or video[0].get("codec_name") not in {"h264", "hevc", "av1"}:
        raise ValueError("Expected exactly one H.264, HEVC or AV1 video stream.")
    if any(s.get("codec_name") != "opus" for s in audio):
        raise ValueError("Expected Opus recording audio tracks.")
    if args.expected_audio_tracks is not None and len(audio) != args.expected_audio_tracks:
        raise ValueError(f"Expected {args.expected_audio_tracks} audio tracks; found {len(audio)}.")
    duration = float(probe.get("format", {}).get("duration", "nan"))
    if not 0 < duration <= args.max_seconds:
        raise ValueError("Container duration is absent, zero, nonfinite or exceeds this short verification limit.")
    # Decode every video/audio track. No output file is opened; failures are
    # fatal. Stream average rates are metadata and never reported as live FPS.
    run([ffmpeg, "-nostdin", "-v", "error", "-xerror", "-i", str(path),
         "-map", "0:v", "-map", "0:a?", "-f", "null", "-"], 180)
    after = path.stat()
    if (before.st_size, before.st_mtime_ns) != (after.st_size, after.st_mtime_ns):
        raise ValueError("The recording changed while being verified; stop recording before retrying.")
    print(json.dumps({
        "path": str(path), "container_duration_seconds": duration,
        "file_bytes": before.st_size, "video_codec": video[0]["codec_name"],
        "width": video[0].get("width"), "height": video[0].get("height"),
        "audio_tracks": [{"index": s["index"], "codec": s.get("codec_name"),
                          "channels": s.get("channels"), "sample_rate": s.get("sample_rate"),
                          "title": s.get("tags", {}).get("title")} for s in audio],
        "decode": "passed", "content_av_synchronization": "requires listening/playback check",
        "live_performance": "not measured by this offline workflow"
    }, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.TimeoutExpired, json.JSONDecodeError) as error:
        print(f"Recording verification failed: {error}", file=sys.stderr)
        sys.exit(1)
