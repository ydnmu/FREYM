#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 FREYM contributors
"""Create fresh native source/legal resources; fail closed on notice gaps.

No build, install, publication or source mutation. The existing notice collector
owns coverage policy. Sources include the dirty repository and the resolved
Windows dependency sources, not just a list of hashes or dependency URLs.
"""
import argparse
import hashlib
import importlib.util
import json
import pathlib
import shutil
import subprocess
import zipfile


ROOTS = {".github", "assets", "crates", "docs", "fixtures", "scripts", "vendor"}
ROOT_FILES = {".gitattributes", ".gitignore", "AGENTS.md", "Cargo.lock", "Cargo.toml",
              "LICENSE", "NOTICE", "rust-toolchain.toml", "TRADEMARKS.md", "SECURITY.md"}
FORBIDDEN = {"node_modules", "target", "dist", ".vite", "coverage", "test-results", "playwright-report",
             ".git", ".idea", ".vscode", "cache", "__pycache__", ".openai"}
FORBIDDEN_SUFFIX = {".exe", ".dll", ".pdb", ".zip", ".log", ".db", ".sqlite", ".profraw", ".user", ".suo"}


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_snapshot(repo):
    raw = subprocess.check_output(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], cwd=repo)
    deleted = set(subprocess.check_output(["git", "ls-files", "--deleted", "-z"], cwd=repo).decode("utf-8").split("\0"))
    entries = []
    for name in sorted(set(raw.decode("utf-8").split("\0")) - {""}):
        path = pathlib.PurePosixPath(name)
        if (path.is_absolute() or any(part in ("", ".", "..") or ":" in part for part in path.parts)
                or any(part in FORBIDDEN or part.startswith(".env") for part in path.parts)
                or path.suffix.lower() in FORBIDDEN_SUFFIX):
            raise ValueError(f"Local/generated or unsafe source path: {name}")
        file = repo / name
        if name in deleted and not file.exists():
            continue  # Preserve an intentional tracked deletion in the dirty overlay.
        if ((len(path.parts) == 1 and name not in ROOT_FILES)
                or (len(path.parts) > 1 and path.parts[0] not in ROOTS)):
            raise ValueError(f"Local/generated or unsafe source path: {name}")
        if not file.resolve().is_relative_to(repo.resolve()):
            raise ValueError(f"Source path resolves outside the repository: {name}")
        if not file.is_file() or file.is_symlink():
            raise ValueError(f"Source is not a regular file: {name}")
        entries.append({"path": name, "bytes": file.stat().st_size, "sha256": digest(file)})
    return entries


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path)
    parser.add_argument("--verify", type=pathlib.Path, help="Verify the exact Git-visible source inventory without writing")
    args = parser.parse_args()
    repo = args.repo_root.resolve(strict=True)
    if args.verify:
        manifest = json.loads(args.verify.read_text(encoding="utf-8-sig"))
        if manifest["files"] != source_snapshot(repo):
            raise ValueError("Source inventory changed after the candidate snapshot")
        return 0
    if args.output is None:
        parser.error("--output is required unless --verify is used")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    before = source_snapshot(repo)
    spec = importlib.util.spec_from_file_location("notices", repo / "scripts/collect-third-party-notices.py")
    notices = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(notices)
    inventory = notices.collect(repo, output / "third-party/inventory.json", None, native=True)
    if not inventory["summary"]["distribution_complete"]:
        print(json.dumps({"distribution_complete": False, "unresolved": inventory["unresolved"]}, indent=2))
        return 2
    for name in ("LICENSE", "NOTICE", "TRADEMARKS.md"):
        shutil.copy2(repo / name, output / name)
    shutil.copytree(repo / "crates/freym-ui/third_party", output / "third-party/ui")
    fonts = output / "third-party/fonts"
    fonts.mkdir()
    shutil.copy2(repo / "crates/freym-ui/assets/fonts/ui-lab/Geist-LICENSE.txt", fonts / "Geist-LICENSE.txt")
    vendor = output / "third-party/nv-codec-headers"
    vendor.mkdir()
    for name in ("nvEncodeAPI.h", "README.md"):
        shutil.copy2(repo / "crates/frame-windows/vendor/nv-codec-headers" / name, vendor / name)
    metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--locked", "--offline", "--format-version", "1",
                                                  "--filter-platform", notices.TARGET], cwd=repo))
    dependencies = [package for package in notices.cargo_packages(metadata, ("freym-app", "freym-update")) if package["source"]]
    dependency_entries = []
    with zipfile.ZipFile(output / "SOURCE.zip", "x", compression=zipfile.ZIP_DEFLATED, compresslevel=6) as archive:
        for entry in before:
            archive.write(repo / entry["path"], entry["path"])
        copied_git = set()
        for package in dependencies:
            source = pathlib.Path(package["manifest_path"]).parent
            if package["source"].startswith("git+"):
                source = pathlib.Path(subprocess.check_output(["git", "-C", str(source), "rev-parse", "--show-toplevel"], text=True).strip())
                if source in copied_git:
                    continue
                copied_git.add(source)
                prefix = f"dependency-sources/git/{source.parent.name}/{package['source'].rsplit('#',1)[-1]}"
                names = subprocess.check_output(["git", "-C", str(source), "ls-files", "-z"]).decode("utf-8").split("\0")
                files = [source / name for name in names if name]
            else:
                prefix = f"dependency-sources/registry/{package['name']}-{package['version']}"
                files = sorted(file for file in source.rglob("*") if file.is_file())
            for file in files:
                relative = file.relative_to(source).as_posix()
                # Git link entries are preserved as their object bytes in the
                # archive, rather than following arbitrary checkout symlinks.
                if file.is_symlink():
                    data = subprocess.check_output(["git", "-C", str(source), "show",
                                                   f"{package['source'].rsplit('#',1)[-1]}:{relative}"])
                else:
                    data = file.read_bytes()
                path = f"{prefix}/{relative}"
                archive.writestr(path, data)
                dependency_entries.append({"path": path, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()})
        identities = [{key: package.get(key) for key in ("name", "version", "source", "license", "repository")}
                      for package in dependencies]
        archive.writestr("DEPENDENCY-SOURCES.json", json.dumps({"packages": identities, "files": dependency_entries}, indent=2))
        archive.writestr("SOURCE-README.txt", "FREYM native Windows source, including resolved dependency sources.\n"
                         "Build with the pinned Rust toolchain, MSVC and Windows SDK: cargo build --locked --release -p freym-app --bin freym -p freym-update --bin freym-launcher -j2\n"
                         "Cargo.lock pins registry checksums and Git revisions. Dependency source copies are retained under dependency-sources; a normal Cargo build resolves the locked upstream packages.\n"
                         "No Node, WebView2 or FFmpeg is required for the native recorder.\n")
    if before != source_snapshot(repo):
        raise ValueError("Source changed while preparing native resources; preserve this failed output and retry after fixing the cause")
    (output / "SOURCE-MANIFEST.json").write_text(json.dumps({"schemaVersion": 1,
        "head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip(),
        "dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=repo)),
        "files": before, "source_sha256": digest(output / "SOURCE.zip")}, indent=2) + "\n", encoding="utf-8")
    print(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
