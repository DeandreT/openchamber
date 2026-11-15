#!/usr/bin/env python3
"""Fetch the inputs for the offline libghostty-vt build.

Downloads the pinned Ghostty source tarball and fills Zig's package cache for
it, so `native/vendor/libghostty-vt-sys/build.rs` can build without a network.
Run it once per cache directory; it skips anything already present and valid.

The cache is `native/.cache/ghostty`, or the directory named by
`OPENCHAMBER_GHOSTTY_CACHE`. Zig 0.15.2 comes from `ZIG` or `PATH`.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
from pathlib import Path

GHOSTTY_COMMIT = "9f62873bf195e4d8a762d768a1405a5f2f7b1697"
GHOSTTY_TARBALL_SHA256 = "16c78c876b07cdf7c88e6dcd622ce88b9723329b07e82a04a7b8c00d1586d226"
GHOSTTY_TARBALL_URL = f"https://github.com/ghostty-org/ghostty/archive/{GHOSTTY_COMMIT}.tar.gz"
ZIG_VERSION = "0.15.2"
NATIVE_DIR = Path(__file__).resolve().parent.parent


def cache_dir() -> Path:
    configured = os.environ.get("OPENCHAMBER_GHOSTTY_CACHE")
    return Path(configured) if configured else NATIVE_DIR / ".cache" / "ghostty"


def locate_zig() -> str:
    zig = os.environ.get("ZIG") or shutil.which("zig")
    if not zig:
        sys.exit(f"Zig {ZIG_VERSION} is required: set ZIG to its zig binary or put it on PATH.")
    version = subprocess.run([zig, "version"], capture_output=True, text=True, check=True).stdout.strip()
    if version != ZIG_VERSION:
        sys.exit(f"{zig} is Zig {version}; Ghostty {GHOSTTY_COMMIT[:8]} needs exactly {ZIG_VERSION}.")
    return zig


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def fetch_tarball(cache: Path) -> Path:
    tarball = cache / f"ghostty-{GHOSTTY_COMMIT}.tar.gz"
    if tarball.is_file() and sha256(tarball) == GHOSTTY_TARBALL_SHA256:
        print(f"source: {tarball} (present)")
        return tarball
    partial = tarball.with_suffix(".partial")
    print(f"source: downloading {GHOSTTY_TARBALL_URL}")
    with urllib.request.urlopen(GHOSTTY_TARBALL_URL, timeout=120) as response, partial.open("wb") as out:
        shutil.copyfileobj(response, out)
    actual = sha256(partial)
    if actual != GHOSTTY_TARBALL_SHA256:
        partial.unlink()
        sys.exit(f"source: SHA-256 {actual} does not match {GHOSTTY_TARBALL_SHA256}")
    partial.replace(tarball)
    return tarball


def read_packages(tarball: Path) -> dict[str, dict[str, str]]:
    """Ghostty's build.zig.zon.json maps each Zig package hash to its URL."""
    member = f"ghostty-{GHOSTTY_COMMIT}/build.zig.zon.json"
    with tarfile.open(tarball, "r:gz") as archive:
        handle = archive.extractfile(member)
        if handle is None:
            sys.exit(f"source: {member} is missing from the tarball")
        return json.load(handle)


def fetch_packages(zig: str, cache: Path, packages: dict[str, dict[str, str]]) -> None:
    store = cache / "zig"
    env = dict(os.environ, ZIG_GLOBAL_CACHE_DIR=str(store))
    for expected, package in sorted(packages.items(), key=lambda item: item[1]["name"]):
        existing = store / "p" / expected
        if existing.is_dir():
            # Hash the contents again. A directory named by a digest can be
            # incomplete or edited after the original download.
            with tempfile.TemporaryDirectory() as verification:
                result = subprocess.run(
                    [zig, "fetch", "--global-cache-dir", verification, str(existing.resolve())],
                    capture_output=True, text=True, env=env,
                )
            actual = result.stdout.strip()
            if result.returncode != 0 or actual != expected:
                sys.exit(f"zig: {package['name']} cached as {actual or result.stderr.strip()}, expected {expected}; remove {existing} and rerun this fetch script")
            continue
        print(f"zig: fetching {package['name']} from {package['url']}")
        # Zig names a fetched package by the hash of its contents, so the
        # printed hash proves the download matches Ghostty's manifest.
        with tempfile.TemporaryDirectory() as work:
            result = subprocess.run(
                [zig, "fetch", package["url"]], capture_output=True, text=True, env=env, cwd=work
            )
        actual = result.stdout.strip()
        if result.returncode != 0 or actual != expected:
            sys.exit(f"zig: {package['name']} fetched as {actual or result.stderr.strip()}, expected {expected}")
    missing = [key for key in packages if not (store / "p" / key).is_dir()]
    if missing:
        sys.exit(f"zig: packages missing after fetch: {', '.join(missing)}")
    print(f"zig: {len(packages)} packages in {store / 'p'}")


def main() -> None:
    argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter).parse_args()
    cache = cache_dir()
    cache.mkdir(parents=True, exist_ok=True)
    zig = locate_zig()
    tarball = fetch_tarball(cache)
    fetch_packages(zig, cache, read_packages(tarball))


if __name__ == "__main__":
    main()
