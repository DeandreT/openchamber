"""Compile only the XIM callback module using existing Cargo dependency artifacts."""
from pathlib import Path
import subprocess
import tempfile

vendor = Path(__file__).resolve().parents[1]
native = vendor.parents[1]
deps = native / "target/release/deps"
externs = []
for alias, crate in (("xim", "zed_xim"), ("x11rb", "x11rb"), ("log", "log")):
    candidates = list(deps.glob(f"lib{crate}-*.rlib"))
    if not candidates:
        raise SystemExit(f"Build the native release dependencies first: missing {crate}")
    artifact = max(candidates, key=lambda path: path.stat().st_mtime_ns)
    externs.extend(("--extern", f"{alias}={artifact}"))

scratch = native / "target/tmp"
scratch.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix="xim-handler-test-", dir=scratch) as output:
    binary = Path(output) / "xim-handler-tests"
    subprocess.run([
        "rustc", "--edition=2024", "--test", str(vendor / "src/linux/x11/xim_handler.rs"),
        "-L", f"dependency={deps}", *externs, "-o", str(binary),
    ], cwd=native, check=True)
    subprocess.run([str(binary)], cwd=native, check=True)
