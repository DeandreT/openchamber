"""A private IBus with libpinyin for the runner's native `ime` steps.

The configuration `native_platform.py` qualifies, owned by one scenario run:
its profile, keyfile settings and logs live under the run's directory, the
daemon serves XIM on the owned display, and nothing reaches the user's input
method. The native client joins it through `XMODIFIERS`.
"""

from __future__ import annotations

import os
import subprocess
import time
from pathlib import Path

import virtual_display

ENGINE = "libpinyin"
LATIN = "xkb:us::eng"
LIBPINYIN = Path("/usr/lib/ibus-libpinyin/ibus-engine-libpinyin")
CLIENT_ENV = {"XMODIFIERS": "@im=ibus", "GTK_IM_MODULE": "ibus", "QT_IM_MODULE": "ibus"}


class IBusFixture:
    def __init__(self, root: Path):
        self.root = root
        self.processes: list[subprocess.Popen] = []
        self.logs = []
        self.env: dict[str, str] = {}

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
        for key, value in (("preload-engines", f"['{ENGINE}', '{LATIN}']"), ("engines-order", f"['{ENGINE}', '{LATIN}']"), ("enable-by-default", "true")):
            self._command("gsettings", "set", "org.freedesktop.ibus.general", key, value)
        self._command("gsettings", "set", "com.github.libpinyin.ibus-libpinyin.libpinyin", "lua-extension", "false")
        self._start("ibus", ["ibus-daemon", "--xim", "--panel=disable", "--config=/usr/lib/ibus/ibus-dconf", "--emoji-extension=disable", "--cache=none"])
        self._until("IBus did not register libpinyin", lambda: ENGINE in self._command("ibus", "list-engine"))
        self.env["IBUS_ADDRESS"] = self._command("ibus", "address").strip()
        self._start("libpinyin", [str(LIBPINYIN), "--ibus"])
        self.select(LATIN)
        return dict(CLIENT_ENV)

    def select(self, engine: str) -> None:
        def call(method: str, *arguments: str) -> str:
            return self._command("gdbus", "call", "--address", self.env["IBUS_ADDRESS"], "--dest", "org.freedesktop.IBus",
                                 "--object-path", "/org/freedesktop/IBus", "--method", f"org.freedesktop.IBus.{method}", *arguments)

        call("SetGlobalEngine", engine)
        self._until(f"IBus did not select {engine}", lambda: engine in call("GetGlobalEngine"))

    def close(self) -> None:
        for process in reversed(self.processes):
            virtual_display.stop_process(process)
        for log in self.logs:
            log.close()
