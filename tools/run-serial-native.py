"""Run one owned native qualification command with the shared-host capacity stop.

This lease coordinates this session's invocations only. It establishes no global
writer clearance and grants no authority over other processes or retained files.
"""

import fcntl
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time


ROOT = Path("/home/deandre/projects/rust/OpenChamber")
NATIVE = ROOT / "native"
STOP_BYTES = 17 * 1024**3


def available():
    info = os.statvfs(NATIVE)
    return info.f_bavail * info.f_frsize


def identity(pid):
    path = Path(f"/proc/{pid}")
    try:
        if path.stat().st_uid != os.getuid():
            return None
        tail = (path / "stat").read_text().rsplit(") ", 1)[1].split()
        return {"pid": pid, "pgrp": int(tail[2]), "startTicks": int(tail[19])}
    except (OSError, ValueError, IndexError):
        return None


def main():
    label, separator, *command = sys.argv[1:]
    if separator != "--" or not command:
        raise SystemExit("Expected LABEL -- COMMAND [ARGUMENTS]")
    lease_path = Path("/tmp/opencode/openchamber-native-serial.lock")
    with lease_path.open("a") as lease:
        # Parallel agents queue here; one native workload runs at a time.
        fcntl.flock(lease, fcntl.LOCK_EX)
        before = available()
        if before < STOP_BYTES:
            raise SystemExit(f"Capacity stop before dispatch: {before} available bytes")
        run = Path(tempfile.mkdtemp(prefix=f"{label}-", dir=NATIVE / "target" / "tmp"))
        log = run / "command.log"
        environment = os.environ.copy()
        environment.update({
            "CARGO_INCREMENTAL": "0",
            "RUSTUP_AUTO_INSTALL": "0",
            "ZIG": "/home/deandre/.local/share/tooling/zig/0.15.2/zig",
            "PATH": "/home/deandre/.local/share/bun-1.4.4/bun-linux-x64:" + environment.get("PATH", ""),
        })
        signals = []
        samples = [before]
        finished = threading.Event()
        descriptor = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "wb") as output:
            child = subprocess.Popen(command, cwd=NATIVE, env=environment,
                stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT,
                start_new_session=True)
            owner = identity(child.pid)
            if owner is not None and owner["pgrp"] != child.pid:
                raise RuntimeError("Qualification child did not create its own process group")

            def monitor():
                stop_at = None
                while not finished.wait(0.5):
                    free = available()
                    samples.append(free)
                    if free >= STOP_BYTES and stop_at is None:
                        continue
                    current = identity(child.pid)
                    if owner is None or current != owner or child.poll() is not None:
                        continue
                    if stop_at is None:
                        os.killpg(child.pid, signal.SIGTERM)
                        signals.append({"signal": "SIGTERM", "owner": owner, "availableBytes": free})
                        stop_at = time.monotonic()
                    elif time.monotonic() - stop_at >= 10 and not any(s["signal"] == "SIGKILL" for s in signals):
                        os.killpg(child.pid, signal.SIGKILL)
                        signals.append({"signal": "SIGKILL", "owner": owner, "availableBytes": free})

            watcher = threading.Thread(target=monitor, daemon=True)
            watcher.start()
            exit_code = child.wait()
            finished.set()
            watcher.join()
        after = available()
        samples.append(after)
        result = {"command": command, "exitCode": exit_code, "owner": owner,
            "log": str(log), "availableBytesBefore": before, "availableBytesAfter": after,
            "minimumSampledAvailableBytes": min(samples), "stopThresholdBytes": STOP_BYTES,
            "signals": signals, "cleanupPerformed": False}
        receipt = run / "receipt.json"
        receipt.write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result, indent=2), flush=True)
        print("Receipt:", receipt, flush=True)
        raise SystemExit(exit_code if exit_code >= 0 else 128 - exit_code)


if __name__ == "__main__":
    main()
