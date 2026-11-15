"""Focused cache integrity checks using the pinned Zig, without network."""

import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("fetch_ghostty", Path(__file__).with_name("fetch-ghostty.py"))
FETCH = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FETCH)
ZIG = os.environ.get("ZIG", "/home/deandre/.local/share/tooling/zig/0.15.2/zig")


class CachedPackages(unittest.TestCase):
    def test_existing_package_is_rehashed(self):
        with tempfile.TemporaryDirectory(prefix="ghostty-integrity-") as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            (source / "payload.txt").write_text("original")
            cache = root / "cache"
            result = subprocess.run([ZIG, "fetch", "--global-cache-dir", str(cache / "zig"), str(source)], check=True, capture_output=True, text=True)
            expected = result.stdout.strip()
            packages = {expected: {"name": "fixture", "url": "https://invalid.example/never-fetch"}}
            FETCH.fetch_packages(ZIG, cache, packages)
            (cache / "zig" / "p" / expected / "payload.txt").write_text("corrupted")
            with self.assertRaisesRegex(SystemExit, "expected"):
                FETCH.fetch_packages(ZIG, cache, packages)


if __name__ == "__main__":
    unittest.main()
