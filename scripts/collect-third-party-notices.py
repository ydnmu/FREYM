#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 FREYM contributors
"""Collect notices from the locked Windows desktop build's resolved sources."""

import argparse
import hashlib
import json
import re
import subprocess
import sys
import tomllib
import urllib.error
import urllib.request
from pathlib import Path


TARGET = "x86_64-pc-windows-msvc"
NOTICE_NAME = re.compile(
    r"^(license|licence|copying|copyright|notice|authors|unlicense)([._-]|$)", re.I
)
LICENSE_NAME = re.compile(r"^(license|licence|copying|unlicense)([._-]|$)", re.I)

# Standard texts explicitly referenced by these exact package sources. Preserve
# the declaring source too; never infer or invent a copyright holder.
REFERENCED_TEXTS = {
    ("selectors", "0.36.1"): {
        "declaration": "lib.rs",
        "reference": b"https://mozilla.org/MPL/2.0/",
        "license": "MPL-2.0",
        "url": "https://www.mozilla.org/media/MPL/2.0/index.815ca599c9df.txt",
        "sha256": "fab3dd6bdab226f1c08630b1dd917e11fcb4ec5e1e020e2c16f83a0a13863e85",
    },
    ("siphasher", "1.0.3"): {
        "declaration": "COPYING",
        "reference": b"http://www.apache.org/licenses/LICENSE-2.0",
        "license": "Apache-2.0",
        "url": "https://www.apache.org/licenses/LICENSE-2.0.txt",
        "sha256": "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30",
    },
}

# The exact legal filenames observed at the package source commits. An empty
# tuple means that exact upstream tree has no standalone legal text to fetch.
UPSTREAM_LEGAL_FILES = {
    "etemesi254/zune-image": ("LICENSE.md", "LICENSE-ZLIB"),
    "AccessKit/accesskit": ("LICENSE-MIT",),
    "rustaudio/sample": ("LICENSE-MIT",),
    "nical/lyon": ("LICENSE-MIT",),
    "aclysma/profiling": ("LICENSE-MIT",),
    "sarah-quinones/pulp": ("LICENSE",),
    "nical/rust_debug": ("LICENSE",),
    "DioxusLabs/taffy": ("LICENSE",),
    "dropbox/rust-alloc-no-stdlib": ("LICENSE",),
    "knurling-rs/defmt": ("LICENSE-APACHE", "LICENSE-MIT"),
    "servo/stylo": (),
    "Aleph-Alpha/ts-rs": ("LICENSE",),
    "open-i18n/rust-unic": ("AUTHORS", "COPYRIGHT.md", "LICENSE-APACHE", "LICENSE-MIT"),
    "wravery/webview2-rs": ("LICENSE",),
    "jedisct1/rust-siphash": ("COPYING",),
    "rolldown/rolldown": ("LICENSE", "THIRD-PARTY-LICENSE"),
    "Rich-Harris/is-reference": (),
    "Rich-Harris/locate-character": (),
    "defunctzombie/node-stackback": (),
    "HEnquist/realfft": (),
}

# The exact published releases predate the license text their project later
# added in its own canonical repository. Each pin is the project's remediation
# commit, verified by content hash; any substitute fails the review.
FOLLOW_UP_LICENSES = {
    ("simd_helpers", "0.1.0"): {
        "repository": "https://github.com/lu-zero/simd_helpers",
        "published_commit": "ca1a2f84aa386d758e98f8a609d990263932fb85",
        "license_commit": "82040194cd05affb060bf94d6f19f82a771d07fb",
        "file": "LICENSE",
        "sha256": "d69f24ad84ec2ade64c0b68bdb31b41170e997b158370342056918329cc9af1e",
    },
    ("seahash", "4.1.0"): {
        "repository": "https://gitlab.redox-os.org/redox-os/seahash",
        "published_commit": "94b632aeac099031c373599313d5b5f0acbbaec0",
        "license_commit": "3088c5c912b70b586d27bf553fbe964e025a2c89",
        "file": "LICENSE",
        "sha256": "23f18e03dc49df91622fe2a76176497404e46ced8a715d9d2b67a7446571cca3",
    },
}


# Reviewed third-party material vendored inside a published package. A package's
# own license text cannot cover material it did not author, so each entry pins
# the packaged declaration, proves the vendored copy is the unmodified upstream
# file and retains the upstream license text at one immutable revision.
VENDORED_SOURCES = {
    ("mkwriter", "0.1.0"): {
        "declaration": "spec/README.md",
        "references": (
            b"https://github.com/ietf-wg-cellar/matroska-specification",
            b"Creative Commons Attribution 4.0 International (CC BY 4.0)",
        ),
        "vendored": "spec/ebml_matroska.xml",
        "license": "CC-BY-4.0",
        "repo": "ietf-wg-cellar/matroska-specification",
        "commit": "7611d1e7f37f098f536267b0894bf46ed7e5442d",
        "vendored_sha256": (
            "a42a9c6ea47dfd83589ce04a508b6a7f5d038560cee11dee1e4cbe2e746d0939"
        ),
        "license_files": {
            "LICENSE.md": (
                "46b8f59c52378b971fc0b4250e8d3d1f2b53e0d761cddbe0d8601f361007e092"
            ),
        },
    },
}

# The exact realfft release declares MIT, but neither its published crate nor
# its pinned upstream commit contains the full text. Retain the declaration as
# evidence while keeping the distribution gate closed. A sibling project or a
# generic MIT template is not a substitute for a text tied to this release.
REALFFT_DECLARATION = {
    "version": "3.5.0",
    "file": "Cargo.toml.orig",
    "sha256": "d72ddbadf9bb55ed21ae973ac97f0bb4e8df2064af628c54b802b2c7d764c8de",
    "upstream_commit": "d0d4eee0525fd27c96c8a046d6d107acd5ed84a6",
    "license_expression": "MIT",
}


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def request(url):
    req = urllib.request.Request(url, headers={"User-Agent": "frame-notices/1"})
    with urllib.request.urlopen(req, timeout=25) as response:
        return response.read()


def upstream_files(repo, commit):
    """Read legal files at an immutable Git commit, preserving source bytes."""
    if repo not in UPSTREAM_LEGAL_FILES:
        raise ValueError(f"no verified legal file list for {repo}")
    files = []
    for name in UPSTREAM_LEGAL_FILES[repo]:
        url = f"https://raw.githubusercontent.com/{repo}/{commit}/{name}"
        files.append((name, request(url), url))
    return files


def follow_up_url(spec):
    repository, commit, file = spec["repository"], spec["license_commit"], spec["file"]
    if repository.startswith("https://github.com/"):
        return (
            "https://raw.githubusercontent.com/"
            f"{repository.removeprefix('https://github.com/')}/{commit}/{file}"
        )
    return f"{repository}/-/raw/{commit}/{file}"


def follow_up_license_text(spec):
    """Read the reviewed follow-up license text; byte equality is required."""
    data = request(follow_up_url(spec))
    if sha256(data) != spec["sha256"]:
        raise ValueError("follow-up license hash changed; review required")
    return data


def notice_files(source, license_file=None):
    if not source.is_dir():
        return []
    explicit = Path(license_file) if license_file else None
    found = []
    for file in source.rglob("*"):
        if not file.is_file() or file.is_symlink():
            continue
        relative = file.relative_to(source)
        if "node_modules" in relative.parts:
            continue  # nested npm packages have their own lock entries
        if NOTICE_NAME.match(file.name) or relative == explicit:
            found.append((relative.as_posix(), file.read_bytes(), "published package"))
    return sorted(found, key=lambda item: item[0])


def has_license(files, license_file=None):
    """Conservative text-presence screening, not a compatibility verdict.

    A long pointer or unrelated vendored license cannot cover its parent.
    Unknown text formats remain unresolved for review.
    """
    clauses = (
        (
            "unicode license v3",
            "permission is hereby granted",
            "the data files and software are provided",
            "without prior written authorization",
        ),
        (
            "permission is hereby granted, free of charge",
            "copies or substantial portions",
            "the software is provided",
        ),
        ("apache license", "version 2.0", "end of terms and conditions"),
        ("mozilla public license", "2.0", "exhibit a"),
        (
            "redistribution and use in source and binary forms",
            "this software is provided",
        ),
        ("permission to use, copy, modify", "the software is provided"),
        (
            "permission is granted to anyone",
            "origin of this software",
            "this software is provided",
        ),
        (
            "this is free and unencumbered software",
            "anyone is free",
            "the software is provided",
        ),
        ("creative commons", "1.0 universal", "waiver", "fallback"),
        ("community data license agreement - permissive - version 2.0",
         "provision of the data", "no data provider shall have any liability", "definitions"),
    )
    for name, data, _ in files:
        parts = Path(name).parts
        explicit = name == license_file
        own_text = len(parts) == 1 or (
            len(parts) == 2 and parts[0] in ("upstream", "referenced")
        )
        if not (explicit or (own_text and LICENSE_NAME.match(Path(name).name))):
            continue
        text = " ".join(data.decode("utf-8", errors="replace").lower().split())
        if any(all(clause in text for clause in group) for group in clauses):
            return True
    return False


def referenced_text(source, name, version):
    spec = REFERENCED_TEXTS.get((name, version))
    if spec is None:
        return [], None
    declaration = (source / spec["declaration"]).read_bytes()
    if spec["reference"] not in declaration:
        raise ValueError(
            "exact package no longer contains the reviewed license reference"
        )
    data = request(spec["url"])
    if sha256(data) != spec["sha256"]:
        raise ValueError("referenced standard license hash changed; review required")
    return [
        (
            f"referenced/NOTICE-{spec['declaration']}.txt",
            declaration,
            f"published package: {spec['declaration']}",
        ),
        (f"referenced/LICENSE-{spec['license']}.txt", data, spec["url"]),
    ], spec["license"]


def vendored_sources(source, name, version):
    """Retain reviewed third-party material vendored inside a published crate.

    The packaged copy must still be byte-identical to the pinned upstream file,
    and the declaration must still name that upstream source. The vendored file
    itself is provenance, not a notice, so only the declaration and the upstream
    license text are retained.
    """
    spec = VENDORED_SOURCES.get((name, version))
    if spec is None or not source.is_dir():
        return [], None
    declaration = (source / spec["declaration"]).read_bytes()
    if any(reference not in declaration for reference in spec["references"]):
        raise ValueError(
            "exact package no longer contains the reviewed vendored-source reference"
        )
    vendored = (source / spec["vendored"]).read_bytes()
    if sha256(vendored) != spec["vendored_sha256"]:
        raise ValueError("vendored third-party file changed; review required")
    base = f"https://raw.githubusercontent.com/{spec['repo']}/{spec['commit']}"
    files = [
        (
            f"referenced/NOTICE-{spec['declaration'].replace('/', '-')}",
            declaration,
            f"published package: {spec['declaration']}",
        )
    ]
    for filename, digest in spec["license_files"].items():
        data = request(f"{base}/{filename}")
        if sha256(data) != digest:
            raise ValueError(
                "upstream third-party license hash changed; review required"
            )
        files.append((f"upstream/{filename}", data, f"{base}/{filename}"))
    provenance = {
        "vendored_path": spec["vendored"],
        "vendored_sha256": spec["vendored_sha256"],
        "upstream_repository": f"https://github.com/{spec['repo']}",
        "upstream_commit": spec["commit"],
        "upstream_ref_provenance": (
            "immutable upstream commit containing the byte-identical vendored file"
        ),
        "license": spec["license"],
    }
    return files, provenance


def matches_platform(locked):
    def matches(values, current):
        return (
            not values
            or (current in values and f"!{current}" not in values)
            or (
                all(value.startswith("!") for value in values)
                and f"!{current}" not in values
            )
        )

    return matches(locked.get("os"), "win32") and matches(locked.get("cpu"), "x64")


def add_files(output, package_dir, files):
    notices = []
    for name, data, provenance in files:
        relative = f"third-party/{package_dir}/{name}"
        destination = output.parent.parent / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        with destination.open("xb") as file:
            file.write(data)
        notices.append(
            {
                "path": relative,
                "bytes": len(data),
                "sha256": sha256(data),
                "source": provenance,
            }
        )
    return notices


def cargo_packages(metadata, root_names=("freym-app", "freym-update")):
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    roots = [
        package["id"]
        for package in metadata["packages"]
        if package["name"] in root_names and package["source"] is None
    ]
    if len(roots) != len(root_names):
        raise ValueError(f"expected one local package for each root: {root_names}")
    seen = set(roots)
    todo = roots[:]
    while todo:
        for dependency in nodes[todo.pop()]["deps"]:
            if any(kind["kind"] in (None, "build") for kind in dependency["dep_kinds"]):
                dep_id = dependency["pkg"]
                if dep_id not in seen:
                    seen.add(dep_id)
                    todo.append(dep_id)
    return sorted(
        (packages[key] for key in seen),
        key=lambda p: (p["name"], p["version"], p["source"] or ""),
    )


def git_referenced_notices(source, identity):
    """Resolve Git license links from the exact locked object, including Windows
    checkouts that materialize symlinks as a short text file. Never follow a link
    outside that checkout or substitute the repository's current upstream HEAD.
    """
    revision = identity.rsplit("#", 1)[-1]
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("Git source has no locked full revision")
    root = Path(subprocess.check_output(
        ["git", "-C", str(source), "rev-parse", "--show-toplevel"], text=True
    ).strip()).resolve()
    actual = subprocess.check_output(
        ["git", "-C", str(root), "rev-parse", "HEAD"], text=True
    ).strip()
    if actual != revision:
        raise ValueError("Git checkout does not match Cargo.lock revision")
    result = []
    for path in source.rglob("*"):
        if not NOTICE_NAME.match(path.name) or not (path.is_file() or path.is_symlink()):
            continue
        relative = path.relative_to(root).as_posix()
        data = subprocess.check_output(["git", "-C", str(root), "show", f"{revision}:{relative}"])
        if len(data) > 4096:
            continue
        pointer = data.decode("utf-8", errors="replace").strip()
        if "\n" in pointer or not pointer.startswith("../"):
            continue
        target = (path.parent / pointer).resolve()
        target_relative = target.relative_to(root).as_posix()  # fails on escape
        data = subprocess.check_output(["git", "-C", str(root), "show", f"{revision}:{target_relative}"])
        result.append((f"referenced/{path.relative_to(source).as_posix()}", data,
                       f"{identity}:{target_relative}, linked by {relative}"))
    return result


def collect_cargo(package, lock_entries, output):
    name, version = package["name"], package["version"]
    key = (name, version, package["source"])
    lock = lock_entries.get(key)
    entry = {
        "ecosystem": "cargo",
        "name": name,
        "version": version,
        "license_expression": package.get("license"),
        "source": {
            "registry": package["source"],
            "checksum": lock.get("checksum") if lock else None,
        },
        "notices": [],
        "unresolved": [],
    }
    git_source = (package["source"] or "").startswith("git+")
    if lock is None or (not git_source and not lock.get("checksum")):
        entry["unresolved"].append("missing source/checksum in Cargo.lock")
    source = Path(package["manifest_path"]).parent
    files = notice_files(source, package.get("license_file"))
    if git_source:
        revision = package["source"].rsplit("#", 1)[-1]
        if not re.fullmatch(r"[0-9a-f]{40}", revision):
            entry["unresolved"].append("Git source has no locked full revision")
        else:
            entry["source"]["git_revision"] = revision
            entry["source"]["checksum_note"] = "Cargo pins Git sources by revision, not registry checksum"
            if source.is_dir():
                try:
                    files += git_referenced_notices(source, package["source"])
                except (OSError, ValueError, subprocess.CalledProcessError) as error:
                    entry["unresolved"].append(f"Git notice provenance failed: {type(error).__name__}")
    if not source.is_dir():
        entry["unresolved"].append("resolved crate source is unavailable")
    if source.is_dir():
        if name == "realfft" and version == REALFFT_DECLARATION["version"]:
            declaration = source / REALFFT_DECLARATION["file"]
            vcs_file = source / ".cargo_vcs_info.json"
            try:
                data = declaration.read_bytes()
                vcs = json.loads(vcs_file.read_text(encoding="utf-8"))
                if (
                    sha256(data) != REALFFT_DECLARATION["sha256"]
                    or vcs["git"]["sha1"] != REALFFT_DECLARATION["upstream_commit"]
                    or package.get("license")
                    != REALFFT_DECLARATION["license_expression"]
                ):
                    raise ValueError("realfft declaration or upstream commit differs")
                files.append(
                    (
                        "referenced/NOTICE-Cargo.toml.orig",
                        data,
                        "exact published realfft 3.5.0 crate: Cargo.toml.orig",
                    )
                )
                entry["source"]["license_declaration"] = {
                    "path": REALFFT_DECLARATION["file"],
                    "sha256": REALFFT_DECLARATION["sha256"],
                    "upstream_commit": REALFFT_DECLARATION["upstream_commit"],
                    "full_text_available": False,
                }
            except (OSError, ValueError, KeyError) as error:
                entry["unresolved"].append(
                    f"realfft declaration provenance failed: {type(error).__name__}"
                )
        try:
            supplements, vendored = vendored_sources(source, name, version)
            files += supplements
            if vendored:
                entry["source"]["vendored_third_party"] = vendored
        except (OSError, TimeoutError, ValueError, KeyError) as error:
            entry["unresolved"].append(
                f"vendored third-party notice lookup failed: {type(error).__name__}"
            )
    if not has_license(files, package.get("license_file")) and source.is_dir():
        try:
            supplements, selected = referenced_text(source, name, version)
            files += supplements
            if selected:
                entry["source"]["referenced_license_text"] = selected
        except (OSError, ValueError) as error:
            entry["unresolved"].append(
                f"referenced license lookup failed: {type(error).__name__}"
            )
    if not has_license(files, package.get("license_file")) and source.is_dir():
        vcs_file = source / ".cargo_vcs_info.json"
        repository = package.get("repository") or ""
        # These exact releases predate their project adding the license text,
        # so evidence is the published VCS commit and the text comes from the
        # project's own follow-up commit, verified by bytes.
        spec = FOLLOW_UP_LICENSES.get((name, version))
        if spec is not None:
            try:
                published = json.loads(vcs_file.read_text(encoding="utf-8"))
                if published.get("git", {}).get("sha1") != spec["published_commit"]:
                    raise ValueError(
                        "published crate VCS commit differs from reviewed revision"
                    )
                files.append(
                    (f"upstream/{spec['file']}", follow_up_license_text(spec),
                     follow_up_url(spec))
                )
                entry["source"]["upstream_repository"] = spec["repository"]
                entry["source"]["published_commit"] = spec["published_commit"]
                entry["source"]["license_commit"] = spec["license_commit"]
                entry["source"]["upstream_ref_provenance"] = (
                    "published crate: .cargo_vcs_info.json commit; project's own "
                    "follow-up license commit in its reviewed repository"
                )
            except (
                OSError, urllib.error.URLError, TimeoutError, ValueError, KeyError
            ) as error:
                entry["unresolved"].append(
                    f"follow-up license lookup failed: {type(error).__name__}"
                )
        # This exact published crate omits repository but declares its project
        # homepage. The reviewed upstream revision and subdirectory must match.
        if name == "zune-inflate" and version == "0.2.54":
            declared = (source / "Cargo.toml.orig").read_bytes()
            if b'https://github.com/etemesi254/zune-image/tree/main/zune-inflate' in declared:
                vcs = json.loads(vcs_file.read_text(encoding="utf-8"))
                if vcs["git"]["sha1"] == "69502ce83fdfecdd0beefd677e2abb3781b29d98" and vcs.get("path_in_vcs") == "zune-inflate":
                    repository = "https://github.com/etemesi254/zune-image"
        if (
            not has_license(files, package.get("license_file"))
            and vcs_file.is_file()
            and repository.startswith("https://github.com/")
        ):
            vcs = json.loads(vcs_file.read_text(encoding="utf-8"))
            commit = vcs["git"]["sha1"]
            repo = (
                repository
                .removeprefix("https://github.com/")
                .removesuffix(".git")
                .rstrip("/")
            )
            try:
                files += [
                    (f"upstream/{filename}", data, url)
                    for filename, data, url in upstream_files(repo, commit)
                ]
                entry["source"]["upstream_commit"] = commit
                entry["source"]["upstream_repository"] = f"https://github.com/{repo}"
                entry["source"]["upstream_ref_provenance"] = (
                    ".cargo_vcs_info.json in published crate"
                )
            except (urllib.error.URLError, TimeoutError, ValueError, KeyError) as error:
                entry["unresolved"].append(
                    f"upstream notice lookup failed: {type(error).__name__}"
                )
        if not has_license(files, package.get("license_file")):
            entry["unresolved"].append(
                "full license text absent from exact published and upstream source"
            )
    entry["notices"] = add_files(output, f"cargo/{name}-{version}", files)
    return entry


def classify_distribution(entries, evidence, evidence_hash):
    for entry in entries:
        if entry["ecosystem"] != "cargo":
            raise ValueError("unexpected non-Cargo dependency in native package")
        entry["distribution"] = {
            "classification": "required",
            "reason": "conservative Windows desktop Cargo normal/build closure",
            "evidence": "third-party/native-build-provenance.json",
            "evidence_sha256": evidence_hash,
        }


def collect(repo_root, output, frontend_evidence=None, build_id=None, native=True):
    if output.exists():
        raise FileExistsError("refusing to overwrite an existing notice inventory")
    if not native or frontend_evidence is not None:
        raise ValueError("The web desktop was retired; only native collection is supported")
    evidence = {"native": True, "cargo_roots": ["freym-app", "freym-update"],
                "cargo_lock_sha256": sha256((repo_root / "Cargo.lock").read_bytes())}
    evidence_bytes = (json.dumps(evidence, indent=2) + "\n").encode()
    metadata = json.loads(
        subprocess.check_output(
            [
                "cargo",
                "metadata",
                "--locked",
                "--format-version",
                "1",
                "--filter-platform",
                TARGET,
            ],
            cwd=repo_root,
        )
    )
    cargo_lock = tomllib.loads((repo_root / "Cargo.lock").read_text(encoding="utf-8"))
    lock_entries = {
        (p["name"], p["version"], p.get("source")): p for p in cargo_lock["package"]
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    entries = []
    local = []
    roots = ("freym-app", "freym-update")
    for package in cargo_packages(metadata, roots):
        if package["source"] is None:
            local.append(
                {
                    "ecosystem": "cargo",
                    "name": package["name"],
                    "version": package["version"],
                }
            )
        else:
            entries.append(collect_cargo(package, lock_entries, output))
    entries.sort(
        key=lambda p: (p["ecosystem"], p["name"], p["version"], p.get("lock_path", ""))
    )
    classify_distribution(entries, evidence, sha256(evidence_bytes))
    unresolved = [
        {
            "ecosystem": e["ecosystem"],
            "name": e["name"],
            "version": e["version"],
            "gaps": e["unresolved"],
            "distribution": e["distribution"],
        }
        for e in entries
        if e["unresolved"]
    ]
    inventory = {
        "schema": "frame-third-party-notices/1",
        "coverage_check": "Recognized full-text clauses in package-root, explicitly declared or reviewed upstream/referenced license files; not a license-expression or compatibility audit.",
        "scope": "Conservative Windows Cargo normal/build closure of freym-app and freym-update; no web frontend is bundled. Local workspace packages use root project notices.",
        "local_project_packages": local,
        "packages": entries,
        "summary": {
            "cargo_registry": sum(e["ecosystem"] == "cargo" for e in entries),
            "notice_files": sum(len(e["notices"]) for e in entries),
            "unresolved_packages": len(unresolved),
            "complete": not unresolved,
            "distribution_complete": not any(
                g["distribution"]["classification"] == "required" for g in unresolved
            ),
            "build_only_unresolved": sum(
                g["distribution"]["classification"] == "build_only" for g in unresolved
            ),
        },
        "unresolved": unresolved,
    }
    # Invalid/missing installed identities are not excused by build-only status.
    allowed_gap = "full license text absent from exact published and upstream source"
    if any(gap != allowed_gap for entry in entries for gap in entry["unresolved"]):
        inventory["summary"]["distribution_complete"] = False
    with (output.parent / ("native-build-provenance.json" if native else "frontend-bundle-provenance.json")).open("xb") as file:
        file.write(evidence_bytes)
    output.write_text(
        json.dumps(inventory, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    return inventory


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--native", action="store_true", default=True, help="Compatibility flag; collection always targets the native app and launcher")
    parser.add_argument("--build-id")
    args = parser.parse_args()
    inventory = collect(
        args.repo_root.resolve(),
        args.output.resolve(),
        None,
        args.build_id,
        args.native,
    )
    print(json.dumps(inventory["summary"], sort_keys=True))
    for gap in inventory["unresolved"]:
        print(
            f"UNRESOLVED {gap['ecosystem']} {gap['name']} {gap['version']}: {', '.join(gap['gaps'])}",
            file=sys.stderr,
        )
    return 0 if inventory["summary"]["distribution_complete"] else 2


if __name__ == "__main__":
    sys.exit(main())
