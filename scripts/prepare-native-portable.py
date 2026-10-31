"""Stage already-built native portable binaries; never build, sign or publish.

The launcher and application must be built from the same source/trust config.
Release notice/source gates still apply separately (docs/RELEASE.md).
"""
import argparse
import hashlib
import json
import pathlib
import os
import struct
import shutil
import subprocess
import sys

VC_LICENSE_SHA256 = "08651651a7602fc7c0e2763de0fde1ff9f868df2780597cd1775ee9d6441c783"
VC_LICENSE_URL = "https://visualstudio.microsoft.com/wp-content/uploads/2025/10/Visual-C-V14-License-Redistributable_and_Runtime_ENU.docx"


def windows_runtime(runtime, license_file):
    """Validate the imported x64 release CRT from the licensed VS redist tree.

    Never copy a system DLL or an unsigned/unreviewed runtime into the payload.
    The binary remains unmodified; its Microsoft license stays separate from GPL.
    """
    if runtime is None or license_file is None:
        raise ValueError("Native Windows payload requires --vc-runtime and --vc-runtime-license")
    if runtime.is_symlink() or license_file.is_symlink():
        raise ValueError("Runtime and license must be regular files, not symbolic links")
    runtime = runtime.resolve(strict=True)
    license_file = license_file.resolve(strict=True)
    parts = [part.lower() for part in runtime.parts]
    if (runtime.name.lower() != "vcruntime140.dll" or "redist" not in parts
            or "x64" not in parts or any("debug" in part for part in parts)
            or not runtime.is_file() or runtime.is_symlink()):
        raise ValueError("Use the unmodified x64 release VCRUNTIME140.dll from Visual Studio VC/Redist")
    data = runtime.read_bytes()
    pe = struct.unpack_from("<I", data, 0x3c)[0]
    if data[pe:pe+4] != b"PE\0\0" or struct.unpack_from("<H", data, pe+4)[0] != 0x8664:
        raise ValueError("VC runtime is not an x64 PE binary")
    if hashlib.sha256(license_file.read_bytes()).hexdigest() != VC_LICENSE_SHA256:
        raise ValueError("VC runtime license differs from the reviewed Microsoft V14 runtime terms")
    script = "$s=Get-AuthenticodeSignature -LiteralPath $env:FREYM_RUNTIME_FILE; " \
             "@{status=$s.Status.ToString();subject=$s.SignerCertificate.Subject}|ConvertTo-Json -Compress"
    result = subprocess.run(["pwsh", "-NoProfile", "-NonInteractive", "-Command", script],
                            env={**os.environ, "FREYM_RUNTIME_FILE": str(runtime)},
                            capture_output=True, text=True, check=True, timeout=15)
    signature = json.loads(result.stdout)
    if signature["status"] != "Valid" or "O=Microsoft Corporation" not in (signature["subject"] or ""):
        raise ValueError("VC runtime requires a valid Microsoft Authenticode signature")
    return {"file": "vcruntime140.dll", "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest(),
            "signature": signature, "license_sha256": VC_LICENSE_SHA256, "license_url": VC_LICENSE_URL,
            "redistribution_reference": "https://learn.microsoft.com/en-us/visualstudio/releases/2026/redistribution"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", type=pathlib.Path, required=True)
    parser.add_argument("--launcher", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--release-resources", type=pathlib.Path, required=True,
                        help="Reviewed release resources (not settings, plugins or recordings)")
    parser.add_argument("--vc-runtime", type=pathlib.Path, help="Windows x64 release VCRUNTIME140.dll from VC/Redist")
    parser.add_argument("--vc-runtime-license", type=pathlib.Path, help="Reviewed Microsoft V14 runtime license document")
    args = parser.parse_args()
    if sys.platform not in ("win32", "linux"):
        parser.error("macOS requires signed/notarized bundle packaging; portable updates are unavailable")
    package = "windows-portable-exe" if sys.platform == "win32" else "linux-portable-exe"
    app_name = "freym.exe" if sys.platform == "win32" else "freym"
    launcher_name = "freym-launcher.exe" if sys.platform == "win32" else "freym-launcher"
    for path in (args.app, args.launcher):
        if not path.is_file() or path.is_symlink():
            parser.error(f"Missing regular binary: {path}")
    if args.output.exists():
        parser.error("Output must be a new directory; existing installations are never overwritten")
    resources = args.release_resources.resolve(strict=True)
    runtime = windows_runtime(args.vc_runtime, args.vc_runtime_license) if sys.platform == "win32" else None
    for name in ("LICENSE", "NOTICE", "SOURCE.zip"):
        if not (resources / name).is_file():
            parser.error(f"Reviewed release resources must contain {name}")
    forbidden = {app_name, launcher_name, "vcruntime140.dll", "freym-portable.json", ".freym-update", "native-ui.json",
                 "recording-settings.json", "plugins", "shortcuts.json"}
    for path in resources.rglob("*"):
        if path.is_symlink() or any(part in forbidden for part in path.relative_to(resources).parts):
            parser.error(f"Release resource cannot include user/install state: {path}")
        if path.suffix.lower() in (".mkv", ".freym-session", ".freym-replay"):
            parser.error(f"Release resource cannot include recordings: {path}")
    result = subprocess.run([str(args.launcher.resolve()), "--describe-update-config"],
                            capture_output=True, text=True, check=True, timeout=10)
    trust = json.loads(result.stdout)
    if trust["package"] not in ("disabled", package):
        parser.error("The compiled launcher package does not match this operating system")
    if trust["package"] != "disabled" and (not trust["manifestUrl"] or not trust["publicKeys"]
                                           or not trust["allowedOrigins"]):
        parser.error("A production portable package requires a configured trusted feed and signing keys")
    args.output.mkdir(parents=True, exist_ok=False)
    shutil.copytree(resources, args.output, dirs_exist_ok=True)
    shutil.copy2(args.app, args.output / app_name)
    shutil.copy2(args.launcher, args.output / launcher_name)
    if runtime is not None:
        shutil.copy2(args.vc_runtime, args.output / runtime["file"])
        if hashlib.sha256((args.output / runtime["file"]).read_bytes()).hexdigest() != runtime["sha256"]:
            raise ValueError("VC runtime changed after validation; preserve this failed staging output")
        legal = args.output / "third-party/microsoft-vc-runtime"
        legal.mkdir(parents=True, exist_ok=False)
        shutil.copy2(args.vc_runtime_license, legal / "LICENSE.docx")
        (legal / "PROVENANCE.json").write_text(json.dumps(runtime, indent=2) + "\n", encoding="utf-8")
    # Only an explicitly configured portable build gets the ownership marker.
    # Disabled development and package-manager builds cannot self-update.
    if trust["package"] == package:
        (args.output / "freym-portable.json").write_text(json.dumps({
            "schemaVersion": 1, "package": package,
        }, indent=2) + "\n", encoding="utf-8")
    (args.output / "PORTABLE.md").write_text(
        f"Open {launcher_name} to run FREYM.\n"
        f"Automatic update policy: {trust['package']}.\n"
        "User settings and recordings are stored outside this directory.\n"
        "Move this portable directory as a unit. Do not install it over a package-managed app.\n",
        encoding="utf-8")
    inventory = {path.relative_to(args.output).as_posix(): hashlib.sha256(path.read_bytes()).hexdigest()
                 for path in sorted(args.output.rglob("*")) if path.is_file()}
    (args.output / "PORTABLE-MANIFEST.json").write_text(json.dumps({
        "schemaVersion": 1, "updatePackage": trust["package"], "sha256": inventory, "windowsRuntime": runtime,
    }, indent=2) + "\n", encoding="utf-8")
    print(args.output.resolve())


if __name__ == "__main__":
    main()
