"""Compile the real bridge fault harness with the installed MSVC toolchain, no GPU.
Uses the already-cached cc crate in an isolated temporary Cargo workspace.
"""
from pathlib import Path
import subprocess
import tempfile
import sys
import json

native = Path(__file__).resolve().parent
build = Path(tempfile.mkdtemp(prefix="frame-bridge-tests-"))
(build / "src").mkdir()
(build / "Cargo.toml").write_text('[package]\nname="frame-bridge-tests"\nversion="0.0.0"\nedition="2024"\n[dependencies]\ncc="1"\n')
source = r'''use std::process::Command;
fn main() {
    let tool = cc::Build::new().target("x86_64-pc-windows-msvc").host("x86_64-pc-windows-msvc").opt_level(0).cargo_metadata(false).get_compiler();
    let status = tool.to_command().args(["/nologo", "/W4", "/WX", "/I", @INCLUDE@, @SOURCE@, "/Fe:audit.exe", "/link", "kernel32.lib"]).status().unwrap();
    if !status.success() { std::process::exit(2); }
    let status = Command::new("./audit.exe").status().unwrap();
    std::process::exit(status.code().unwrap_or(3));
}
'''
source = source.replace("@INCLUDE@", json.dumps(str(native.parent / "vendor/nv-codec-headers"))).replace("@SOURCE@", json.dumps(str(native / "nvenc_bridge_test.c")))
(build / "src/main.rs").write_text(source)
print(f"Isolated compiler workspace: {build}", flush=True)
process = subprocess.Popen(["cargo", "run", "--offline", "--quiet"], cwd=build)
try:
    sys.exit(process.wait(timeout=60))
except subprocess.TimeoutExpired:
    subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"], capture_output=True)
    process.wait()
    print("FAIL: bridge harness exceeded its external deadline", file=sys.stderr)
    sys.exit(3)
