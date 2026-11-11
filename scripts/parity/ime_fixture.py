"""A private IBus with libpinyin for the runner's native `ime` steps.

The configuration `native_platform.py` qualifies, owned by one scenario run:
its profile, keyfile settings and logs live under the run's directory, the
daemon serves XIM on the owned display, and nothing reaches the user's input
method. The native client joins it through `XMODIFIERS`.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import threading
import time
from pathlib import Path

import virtual_display

ENGINE = "libpinyin"
LATIN = "xkb:us::eng"
LIBPINYIN = Path("/usr/lib/ibus-libpinyin/ibus-engine-libpinyin")
CLIENT_ENV = {"XMODIFIERS": "@im=ibus", "GTK_IM_MODULE": "ibus", "QT_IM_MODULE": "ibus"}
# A Unix socket path holds 107 bytes. The daemon's default socket sits under
# the profile's cache directory, and a longer path is cut short: scenarios
# then share one socket file, and the one left behind refuses the next bind.
MAX_SOCKET_BYTES = 107


class IBusFixture:
    def __init__(self, root: Path):
        self.root = root
        self.processes: list[subprocess.Popen] = []
        self.logs = []
        self.env: dict[str, str] = {}
        self.socket_dir: Path | None = None
        self.traffic: list[str] = []

    def _command(self, *argv: str) -> str:
        return subprocess.run(argv, env=self.env, check=True, capture_output=True, text=True, timeout=15).stdout

    def _start(self, name: str, argv: list[str]) -> None:
        log = (self.root / f"{name}.log").open("w")
        self.logs.append(log)
        self.processes.append(subprocess.Popen(argv, env=self.env, stdout=log, stderr=log, start_new_session=True))

    def _until(self, problem: str, check, timeout_s: float = 15) -> None:
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            try:
                if check():
                    return
            except subprocess.SubprocessError:
                pass
            time.sleep(0.1)
        raise TimeoutError(problem)

    def start(self) -> dict[str, str]:
        """Starts the daemon and engine; returns the client's environment."""
        virtual_display.verify_owned_display()
        if not LIBPINYIN.is_file():
            raise RuntimeError(f"IME steps need {LIBPINYIN}")
        self.root.mkdir(parents=True)
        env = {key: os.environ[key] for key in ("PATH", "USER", "LANG") if key in os.environ}
        env.update(HOME=str(self.root), LANG="en_US.UTF-8", GSETTINGS_BACKEND="keyfile")
        for variable, folder in (("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache"), ("XDG_STATE_HOME", "state")):
            (self.root / folder / "ibus" / "libpinyin").mkdir(parents=True)
            env[variable] = str(self.root / folder)
        self.env = virtual_display.child_environment(env)
        # The display's runtime directory is private, short and owned by this run.
        self.socket_dir = Path(tempfile.mkdtemp(prefix="ibus-", dir=self.env["XDG_RUNTIME_DIR"]))
        bus = self.socket_dir / "bus"
        if len(os.fsencode(bus)) > MAX_SOCKET_BYTES:
            raise RuntimeError(f"the IBus socket path {bus} is longer than {MAX_SOCKET_BYTES} bytes")
        for key, value in (("preload-engines", f"['{ENGINE}', '{LATIN}']"), ("engines-order", f"['{ENGINE}', '{LATIN}']"), ("enable-by-default", "true")):
            self._command("gsettings", "set", "org.freedesktop.ibus.general", key, value)
        self._command("gsettings", "set", "com.github.libpinyin.ibus-libpinyin.libpinyin", "lua-extension", "false")
        self._start("ibus", ["ibus-daemon", "--xim", "--panel=disable", "--config=/usr/lib/ibus/ibus-dconf", "--emoji-extension=disable", "--cache=none", f"--address=unix:path={bus}"])
        self._until("IBus did not register libpinyin", lambda: ENGINE in self._command("ibus", "list-engine"))
        self.env["IBUS_ADDRESS"] = self._command("ibus", "address").strip()
        self._start("libpinyin", [str(LIBPINYIN), "--ibus"])
        self.select(LATIN)
        self._watch()
        return dict(CLIENT_ENV)

    def _watch(self) -> None:
        """Records the private bus's messages, so a step can wait for the
        engine's commit. It is watching once it has seen a call made here."""
        monitor = subprocess.Popen(["dbus-monitor", "--address", self.env["IBUS_ADDRESS"]], env=self.env, text=True,
                                   stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, start_new_session=True)
        self.processes.append(monitor)

        def record() -> None:
            for line in monitor.stdout:
                self.traffic.append(line)

        threading.Thread(target=record, daemon=True).start()

        def seen() -> bool:
            self._call("GetGlobalEngine")
            return any("member=GetGlobalEngine" in line for line in list(self.traffic))

        self._until("the IBus bus could not be watched", seen)

    def commit_mark(self) -> int:
        """A position in the bus record, taken before the key that commits."""
        return len(self.traffic)

    def wait_commit(self, mark: int, timeout_s: float = 10) -> None:
        """Returns once the engine has committed text since `mark`; how long
        the client took to deliver the key goes to `commits.log`."""
        started = time.monotonic()
        self._until("IBus never committed the composition",
                    lambda: any("member=CommitText" in line for line in list(self.traffic)[mark:]), timeout_s)
        with (self.root / "commits.log").open("a") as log:
            log.write(f"{time.monotonic() - started:.2f}s from the key to the commit\n")

    def _call(self, method: str, *arguments: str) -> str:
        return self._command("gdbus", "call", "--address", self.env["IBUS_ADDRESS"], "--dest", "org.freedesktop.IBus",
                             "--object-path", "/org/freedesktop/IBus", "--method", f"org.freedesktop.IBus.{method}", *arguments)

    def select(self, engine: str) -> None:
        self._call("SetGlobalEngine", engine)
        self._until(f"IBus did not select {engine}", lambda: engine in self._call("GetGlobalEngine"))

    def close(self) -> None:
        for process in reversed(self.processes):
            virtual_display.stop_process(process)
        for log in self.logs:
            log.close()
        if self.socket_dir is not None:
            shutil.rmtree(self.socket_dir, ignore_errors=True)
