"""Run one native proof under a parent watchdog, retain every attempt and full decode.
Requires a previously built encode_probe.exe, ffplay, ffmpeg and ffprobe on PATH.
All artifacts stay in the ignored test-results directory, including failed attempts.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import re
from pathlib import Path
import subprocess
import time
import uuid

root = Path(__file__).resolve().parents[3]
parser = argparse.ArgumentParser()
parser.add_argument("--display-id", required=True)
parser.add_argument("--seconds", type=int, default=30)
parser.add_argument("--rate-num", type=int, required=True)
parser.add_argument("--rate-den", type=int, default=1)
parser.add_argument("--stall-consumer-ms", type=int, default=0)
args = parser.parse_args()
folder = root / "test-results" / ("frame-0104-integration-" + datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S") + "-" + uuid.uuid4().hex[:8])
folder.mkdir(parents=True, exist_ok=False)

def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def capture(command):
    return subprocess.run(command, cwd=root, capture_output=True, text=True, timeout=20).stdout.strip()

def fingerprint():
    paths = subprocess.check_output(["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], cwd=root).decode().split("\0")
    return {name: digest(root / name) for name in sorted(set(paths)) if name and (root / name).is_file() and not name.startswith("docs/")}

binary = root / "target/debug/examples/encode_probe.exe"
stream = folder / "stream.h264"
command = [str(binary), "--display-id", args.display_id, "--seconds", str(args.seconds), "--rate-num", str(args.rate_num), "--rate-den", str(args.rate_den), "--output", str(stream), "--stall-consumer-ms", str(args.stall_consumer_ms)]
visual = ["ffplay", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc2=size=960x540:rate=60", "-an", "-autoexit", "-t", str(args.seconds + 25), "-window_title", "Frame controlled encode proof", "-alwaysontop", "-left", "80", "-top", "100"]
manifest = {"status": "running", "head": capture(["git", "rev-parse", "HEAD"]), "diff_sha256": hashlib.sha256(subprocess.check_output(["git", "diff", "--binary"], cwd=root)).hexdigest(), "source_sha256": fingerprint(), "binary_sha256": digest(binary), "command": command, "controlled_source_command": visual, "driver": capture(["nvidia-smi", "--query-gpu=name,driver_version", "--format=csv,noheader"]), "requested_seconds": args.seconds, "watchdog_seconds": args.seconds + 35, "started_utc": datetime.now(timezone.utc).isoformat()}
manifest_path = folder / "manifest.json"
manifest_path.write_text(json.dumps(manifest, indent=2))
print(folder, flush=True)
started = time.monotonic()
producer = None
try:
    with (folder / "source.stderr.txt").open("w") as source_log, (folder / "stdout.txt").open("w") as out, (folder / "stderr.txt").open("w") as err:
        producer = subprocess.Popen(visual, cwd=root, stdout=subprocess.DEVNULL, stderr=source_log, creationflags=subprocess.CREATE_NO_WINDOW)
        time.sleep(1)
        if producer.poll() is not None:
            raise RuntimeError("Controlled changing source exited before the probe")
        process = subprocess.Popen(command, cwd=root, stdout=out, stderr=err, creationflags=subprocess.CREATE_NO_WINDOW)
        try:
            manifest["exit_code"] = process.wait(timeout=args.seconds + 35)
            manifest["watchdog_terminated"] = False
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
            manifest["exit_code"] = process.returncode
            manifest["watchdog_terminated"] = True
        manifest["controlled_source_alive_through_probe"] = producer.poll() is None
    manifest["elapsed_seconds"] = time.monotonic() - started
    manifest["source_unchanged"] = fingerprint() == manifest["source_sha256"]
    sidecar = Path(str(stream) + ".jsonl")
    for path in [stream, sidecar, folder / "stdout.txt", folder / "stderr.txt"]:
        if path.exists(): manifest.setdefault("artifacts", {})[path.name] = {"sha256": digest(path), "bytes": path.stat().st_size}
    rows = [json.loads(line) for line in sidecar.read_text().splitlines()] if sidecar.exists() else []
    packets = [row for row in rows if row.get("kind") == "packet"]
    summaries = [row for row in rows if row.get("kind") == "summary"]
    manifest["settings"] = next((row for row in rows if row.get("kind") == "settings"), None)
    manifest["summary"] = summaries[-1] if summaries else None
    manifest["packet_rows"] = len(packets)
    report = (folder / "stdout.txt").read_text()
    manifest["resource_metrics"] = {label: next((line for line in report.splitlines() if line.startswith(label + ":")), None) for label in ["delivery", "drain", "pressure", "ownership", "accounting"]}
    manifest["teardown_lines"] = [line.strip() for line in report.splitlines() if "teardown" in line]
    manifest["pressure_valid"] = args.stall_consumer_ms == 0 or bool(re.search(r"pressure:.*backpressure_returns=[1-9][0-9]*.*credits_reached=true", report))
    manifest["duration_valid"] = bool(manifest["summary"]) and manifest["summary"].get("duration_s", 0) >= args.seconds

    manifest["timestamps_and_offsets_valid"] = bool(packets) and all(p["index"] == i and p["pts_ns"] == p["dts_ns"] and p["duration_ns"] > 0 and (i == 0 or (p["pts_ns"] > packets[i-1]["pts_ns"] and p["offset"] == packets[i-1]["offset"] + packets[i-1]["bytes"])) for i,p in enumerate(packets)) and packets[-1]["offset"] + packets[-1]["bytes"] == stream.stat().st_size
    if stream.exists() and stream.stat().st_size:
        decode = ["ffmpeg", "-v", "error", "-xerror", "-err_detect", "explode", "-c:v", "h264", "-i", str(stream), "-map", "0:v:0", "-f", "null", "-"]
        with (folder / "decode.stderr.txt").open("w") as err:
            result = subprocess.run(decode, stdout=subprocess.DEVNULL, stderr=err, timeout=90)
        manifest["decode_command"] = decode
        manifest["decode_exit_code"] = result.returncode
        probe = subprocess.run(["ffprobe", "-v", "error", "-count_frames", "-select_streams", "v:0", "-show_entries", "stream=nb_read_frames,codec_name,profile,width,height,pix_fmt", "-of", "json", str(stream)], capture_output=True, text=True, timeout=90)
        (folder / "ffprobe.json").write_text(probe.stdout)
        manifest["ffprobe_exit_code"] = probe.returncode
        manifest["decoded_stream"] = json.loads(probe.stdout)["streams"][0]
        summary = manifest["summary"] or {}
        count = int(manifest["decoded_stream"].get("nb_read_frames", -1))
        manifest["counts_reconcile"] = count == len(packets) and all(summary.get(k) == count for k in ["accepted_inputs", "packets_emitted", "packets_dequeued", "packets_written"])
    manifest["status"] = "passed" if all([manifest.get("exit_code") == 0, not manifest.get("watchdog_terminated", True), manifest.get("decode_exit_code") == 0, manifest.get("ffprobe_exit_code") == 0, manifest.get("counts_reconcile"), manifest.get("timestamps_and_offsets_valid"), manifest.get("source_unchanged"), manifest.get("controlled_source_alive_through_probe"), manifest.get("pressure_valid"), manifest.get("duration_valid")]) else "failed"
except Exception as error:
    manifest["status"] = "failed"
    manifest["error"] = repr(error)
finally:
    if producer is not None and producer.poll() is None:
        producer.terminate()
        producer.wait(timeout=10)
    manifest_path.write_text(json.dumps(manifest, indent=2))
print(json.dumps({"manifest": str(manifest_path), "status": manifest["status"], "packet_rows": manifest.get("packet_rows"), "watchdog_terminated": manifest.get("watchdog_terminated"), "error": manifest.get("error")}))
raise SystemExit(0 if manifest["status"] == "passed" else 1)
