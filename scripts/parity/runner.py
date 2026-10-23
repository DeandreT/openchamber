#!/usr/bin/env python3
"""Runs UI parity scenarios against the Electron reference and the native desktop.

    python3 scripts/parity/runner.py seed --root tmp/parity
    python3 scripts/parity/runner.py run --root tmp/parity \\
        --electron-app packages/electron/dist/linux-unpacked/openchamber

`seed` builds the synthetic seed: an isolated server's state with fixed
sessions created through the fixture provider. `run` gives each client its
own copy of that seed, launches it at the standard client size, scale, theme
and locale, performs each scenario's steps through real input, and records
screenshots, sidebar rows and geometry at every checkpoint. The Electron
capture is the reference; the comparison reports every difference and
passes only when there is none. See scripts/parity/DOCUMENTATION.md.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import compare  # noqa: E402
from electron_adapter import ElectronClient  # noqa: E402
import native_adapter  # noqa: E402
from native_adapter import NativeClient  # noqa: E402
import virtual_display  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[2]
SCENARIOS = Path(__file__).with_name("scenarios.json")
INVENTORY = Path(__file__).with_name("inventory.json")
ISOLATED_SERVER = REPO_ROOT / "scripts" / "perf" / "isolated-server.mjs"
FIXTURE_PROVIDER = REPO_ROOT / "scripts" / "perf" / "fixture-provider.mjs"
THEMES = REPO_ROOT / "native" / "crates" / "desktop" / "resources" / "themes.json"
SEED_SESSION = REPO_ROOT / "scripts" / "perf" / "seed-long-session.mjs"
DEFAULT_ELECTRON = REPO_ROOT / "packages" / "electron" / "dist" / "linux-unpacked" / "openchamber"
DEFAULT_NATIVE = REPO_ROOT / "native" / "target" / "release" / "openchamber-native"
DEFAULT_DIST = REPO_ROOT / "packages" / "electron" / "resources" / "web-dist"
DEFAULT_OPENCODE = REPO_ROOT / "packages" / "electron" / "dist" / "linux-unpacked" / "resources" / "opencode-cli" / "opencode"

# The seed: titles, turns and fixture models, created in this order.
SEED_SESSIONS = [
    ("parity: short a", 2, "perf/stream-20000cps"),
    ("parity: short b", 3, "perf/stream-20000cps"),
    ("parity: long", 12, "perf/stream-20000cps,perf/code-20000cps,perf/unicode-20000cps"),
    ("parity: tools", 1, "perf/agent-20tools-300cps"),
]


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


class IsolatedServer:
    """`scripts/perf/isolated-server.mjs` on a root this run owns."""

    def __init__(self, root: Path, args: argparse.Namespace):
        self.root = root
        self.port = free_port()
        self.fixture_port = free_port()
        command = [
            "node", str(ISOLATED_SERVER), "--root", str(root),
            "--port", str(self.port), "--fixture-port", str(self.fixture_port),
            "--dist", str(args.dist), "--opencode-binary", str(args.opencode_binary),
        ]
        self.log = open(root / "isolated-server.log", "w")
        self.process = subprocess.Popen(command, cwd=REPO_ROOT, stdout=self.log, stderr=subprocess.STDOUT, start_new_session=True)

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def wait_ready(self, timeout_s: float) -> None:
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError(f"the isolated server exited; see {self.root / 'isolated-server.log'}")
            try:
                with urllib.request.urlopen(f"{self.url}/health", timeout=2) as response:
                    if json.load(response).get("isOpenCodeReady"):
                        return
            except OSError:
                pass
            time.sleep(0.25)
        raise TimeoutError("the isolated server never became ready")

    def stop(self) -> None:
        # Ctrl-C semantics: the script stops exactly what it started.
        try:
            os.killpg(self.process.pid, 2)
            self.process.wait(timeout=30)
        except (ProcessLookupError, subprocess.TimeoutExpired):
            os.killpg(self.process.pid, 9)
            self.process.wait(timeout=10)
        self.log.close()


class FixtureProvider:
    """The fixture model provider for a client that runs its own server, with
    the OpenCode configuration the isolated server gives its OpenCode."""

    def __init__(self, root: Path):
        self.port = free_port()
        self.log = open(root / "fixture.log", "w")
        env = {key: os.environ[key] for key in ("PATH", "LANG") if key in os.environ}
        self.process = subprocess.Popen(
            ["node", str(FIXTURE_PROVIDER), str(self.port)],
            cwd=REPO_ROOT, env=env, stdout=self.log, stderr=subprocess.STDOUT, start_new_session=True,
        )
        script = (
            f"import {{ isolatedOpencodeConfig }} from {json.dumps(ISOLATED_SERVER.as_uri())};"
            f"console.log(JSON.stringify(isolatedOpencodeConfig({self.port})))"
        )
        self.opencode_config = subprocess.run(
            ["node", "--input-type=module", "-e", script], cwd=REPO_ROOT, env=env,
            check=True, capture_output=True, text=True,
        ).stdout.strip()

    def wait_ready(self, timeout_s: float) -> None:
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError(f"the fixture provider exited; see {self.log.name}")
            try:
                with socket.create_connection(("127.0.0.1", self.port), timeout=1):
                    return
            except OSError:
                time.sleep(0.2)
        raise TimeoutError("the fixture provider did not start")

    def stop(self) -> None:
        try:
            os.killpg(self.process.pid, 15)
            self.process.wait(timeout=10)
        except (ProcessLookupError, subprocess.TimeoutExpired):
            os.killpg(self.process.pid, 9)
            self.process.wait(timeout=10)
        self.log.close()


def seed(args: argparse.Namespace) -> int:
    root = args.root.resolve()
    if (root / "seed").exists() and not args.force:
        print(f"{root / 'seed'} exists; pass --force to rebuild it", file=sys.stderr)
        return 2
    shutil.rmtree(root / "state", ignore_errors=True)
    root.mkdir(parents=True, exist_ok=True)
    server = IsolatedServer(root, args)
    try:
        server.wait_ready(args.timeout)
        for title, turns, models in SEED_SESSIONS:
            subprocess.run(
                ["node", str(SEED_SESSION), "--port", str(server.port), "--dir", str(root / "project"),
                 "--turns", str(turns), "--title", title, "--models", models],
                cwd=REPO_ROOT, check=True,
            )
    finally:
        server.stop()
    shutil.rmtree(root / "seed", ignore_errors=True)
    shutil.copytree(root / "state", root / "seed", symlinks=True)
    print(f"seed: {root / 'seed'}")
    return 0


def copy_seed(root: Path, target: Path, theme: str) -> Path:
    """An independent copy of the seed with the theme pinned. The web reads
    `useSystemTheme: false` together with the derived `themeVariant`, so the
    variant and that variant's theme are pinned too."""
    state = target / "state"
    shutil.copytree(root / "seed", state, symlinks=True)
    preferences = state / "oc-data" / "preferences.json"
    document = json.loads(preferences.read_text()) if preferences.exists() else {"version": 1, "fields": {}}
    now = int(time.time() * 1000)
    variant = theme_variant(theme)
    pinned = (("useSystemTheme", False), ("themeId", theme), ("themeVariant", variant), (f"{variant}ThemeId", theme))
    for key, value in pinned:
        document["fields"][key] = {"value": value, "updatedAt": now}
    preferences.write_text(json.dumps(document, indent=2))
    # The login-shell probe runs zsh -il; an empty rc keeps it non-interactive.
    (state / "home").mkdir(parents=True, exist_ok=True)
    (state / "home" / ".zshrc").write_text("")
    return state


def theme_variant(theme: str) -> str:
    """`light` or `dark`, from the exported theme catalog."""
    for entry in json.loads(THEMES.read_text())["themes"]:
        if entry["id"] == theme:
            return entry["variant"]
    raise LookupError(f"no theme {theme} in {THEMES}")


def state_dirs(state: Path) -> dict[str, str]:
    """The home and XDG directories of a client's state copy. Both clients
    get them, so lookups under them (icon themes, default applications) see
    the same files."""
    return {
        "HOME": str(state / "home"),
        "XDG_CONFIG_HOME": str(state / "config"),
        "XDG_DATA_HOME": str(state / "data"),
        "XDG_CACHE_HOME": str(state / "cache"),
        "XDG_STATE_HOME": str(state / "xdg-state"),
    }


def electron_env(state: Path, opencode_config: str) -> dict[str, str]:
    keep = ("PATH", "DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS", "XDG_RUNTIME_DIR", "LANG", "USER")
    env = {key: os.environ[key] for key in keep if key in os.environ}
    env.update(state_dirs(state))
    env.update(
        SHELL="/bin/zsh",
        LANG="en_US.UTF-8",
        OPENCHAMBER_DATA_DIR=str(state / "oc-data"),
        OPENCHAMBER_DESKTOP_USER_DATA_DIR=str(state / "electron-user-data"),
        OPENCODE_CONFIG_CONTENT=opencode_config,
    )
    return env


def native_env(state: Path, server_url: str, theme: str) -> dict[str, str]:
    env = {key: value for key, value in os.environ.items() if not key.startswith(("OPENCHAMBER_", "OPENCODE_"))}
    env.update(state_dirs(state))
    env.update(
        OPENCHAMBER_NATIVE_SERVER=server_url,
        OPENCHAMBER_NATIVE_THEME=theme,
        OPENCHAMBER_NATIVE_LOCALE="en",
        OPENCHAMBER_NATIVE_LOG="warn",
    )
    return env


def resolve_row(rows: list[dict], target: dict) -> str:
    """A row key from a symbolic target: a zone or project prefix and a title."""
    prefix = target.get("zone", "")
    matches = [row["key"] for row in rows if row["key"].startswith(prefix) and row["label"] == target["title"]]
    if len(matches) != 1:
        raise LookupError(f"{len(matches)} rows match {target}")
    return matches[0]


# A capture waits until the client's content held still this long.
SETTLE_QUIET_S = 0.75
SETTLE_TIMEOUT_S = 15.0


def settle(client) -> bool:
    """Whether the client's fingerprint held still for the quiet period
    before the timeout. Content still loading (a panel filling in after the
    transcript) would otherwise be captured midway."""
    deadline = time.monotonic() + SETTLE_TIMEOUT_S
    previous = None
    since = time.monotonic()
    while time.monotonic() < deadline:
        current = client.fingerprint()
        if current != previous:
            previous = current
            since = time.monotonic()
        elif time.monotonic() - since >= SETTLE_QUIET_S:
            return True
        time.sleep(0.1)
    return False


def perform(client, kind: str, steps: list[dict], targets: dict[str, dict], directory: Path, args: argparse.Namespace) -> dict:
    """Runs the steps and returns the evidence of every checkpoint."""
    checkpoints: dict[str, dict] = {}
    waits: list[dict] = []
    for step in steps:
        if "wait" in step:
            condition = step["wait"]
            met = client.wait_for(condition[kind], float(condition.get("timeout", 30)))
            waits.append({"condition": condition[kind], "met": met})
        elif "click" in step:
            client.click(resolve_row(client.rows(), step["click"]))
        elif "key" in step:
            client.key(step["key"])
        elif "type" in step:
            client.type_text(step["type"])
        elif "settle" in step:
            time.sleep(float(step["settle"]))
        elif "capture" in step:
            name = step["capture"]
            image = directory / f"{name}.png"
            settled = settle(client)
            client.screenshot(image)
            checkpoints[name] = {
                "image": str(image),
                "settled": settled,
                "rows": client.rows(),
                "targets": client.targets({target: targets[target][kind] for target in step.get("targets", [])}),
                "waits": list(waits),
                "substitutions": list(getattr(client, "substitutions", [])),
                "unobservable": sorted(getattr(client, "unobservable", ())),
            }
        else:
            raise ValueError(f"unknown step {step}")
    return checkpoints


def run_client(kind: str, scenario: dict, work: Path, args: argparse.Namespace) -> dict:
    directory = work / kind
    directory.mkdir(parents=True)
    state = copy_seed(args.root.resolve(), directory, args.theme)
    server = None
    fixture = None
    started = time.monotonic()
    try:
        if kind == "native":
            server = IsolatedServer(directory, args)
            server.wait_ready(args.timeout)
            client = NativeClient(args.native_binary, native_env(state, server.url, args.theme), args.scale, args.width, args.height)
        else:
            fixture = FixtureProvider(directory)
            fixture.wait_ready(args.timeout)
            client = ElectronClient(args.electron_app, electron_env(state, fixture.opencode_config), args.scale, args.width, args.height)
        launched = client.launch()
        try:
            client.wait_ready(args.timeout, connected=True)
            checkpoints = perform(client, kind, scenario["steps"], args.targets, directory, args)
        finally:
            closed = client.close()
        return {"launch": launched, "checkpoints": checkpoints, "close": closed, "seconds": round(time.monotonic() - started, 1)}
    except (RuntimeError, TimeoutError, LookupError, OSError, subprocess.CalledProcessError) as error:
        return {"error": str(error)}
    finally:
        if server is not None:
            server.stop()
        if fixture is not None:
            fixture.stop()


def compare_rows(reference: list[dict], candidate: list[dict], unobservable: frozenset[str] = frozenset(), unobserved: list[dict] | None = None) -> list[dict]:
    """Differences in row identity, order, label and state. A field the
    candidate's platform cannot report is listed in ``unobserved`` instead:
    neither a difference nor a match."""
    differences = []
    reference_keys = [row["key"] for row in reference]
    candidate_keys = [row["key"] for row in candidate]
    if reference_keys != candidate_keys:
        differences.append({
            "kind": "rows",
            "missing": [key for key in reference_keys if key not in candidate_keys],
            "extra": [key for key in candidate_keys if key not in reference_keys],
            "orderMatches": [key for key in reference_keys if key in candidate_keys] == [key for key in candidate_keys if key in reference_keys],
        })
    by_key = {row["key"]: row for row in candidate}
    for row in reference:
        other = by_key.get(row["key"])
        if other is None:
            continue
        for field in ("label", "selected", "expanded"):
            if field in unobservable:
                if row[field] is not None and unobserved is not None:
                    unobserved.append({"kind": field, "key": row["key"], "reference": row[field]})
                continue
            if row[field] != other[field]:
                differences.append({"kind": field, "key": row["key"], "reference": row[field], "candidate": other[field]})
    return differences


def run(args: argparse.Namespace) -> int:
    if not (args.root / "seed").exists():
        print(f"no seed under {args.root}; run the seed command first", file=sys.stderr)
        return 2
    definitions = json.loads(SCENARIOS.read_text())
    scenarios = definitions["scenarios"]
    args.targets = definitions.get("targets", {})
    features = json.loads(INVENTORY.read_text())["features"]
    problems = check_contracts(scenarios, features, args.targets)
    if problems:
        for problem in problems:
            print(problem, file=sys.stderr)
        return 2
    wanted = set(args.scenarios.split(",")) if args.scenarios else None
    output = (args.output or REPO_ROOT / "artifacts" / f"parity-{time.strftime('%Y%m%dT%H%M%S')}").resolve()
    output.mkdir(parents=True, exist_ok=True)
    report = {"generatedAt": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "configuration": {key: str(value) for key, value in vars(args).items()}, "scenarios": {}}
    failed = 0
    previous_accessibility = native_adapter.accessibility_enabled()
    native_adapter.set_accessibility(True)
    try:
        failed = run_scenarios(scenarios, wanted, output, report, args)
    finally:
        native_adapter.set_accessibility(previous_accessibility)
        report["accessibilityRestoredTo"] = previous_accessibility
    report["inventory"] = inventory_coverage(features, report["scenarios"])
    (output / "report.json").write_text(json.dumps(report, indent=2))
    coverage = report["inventory"]
    print(f"inventory: {coverage['passed']} of {coverage['features']} features passed; "
          f"{coverage['failed']} failed, {coverage['incomplete']} incomplete, {coverage['notRun']} not run, "
          f"{coverage['manual']} manual only")
    print(f"report: {output / 'report.json'}")
    return 1 if failed else 0


def check_contracts(scenarios: list[dict], features: list[dict], targets: dict[str, dict]) -> list[str]:
    """Every scenario names a known feature, every listed scenario exists and
    every captured target is defined for both clients."""
    feature_ids = {feature["id"] for feature in features}
    scenario_ids = {scenario["id"] for scenario in scenarios}
    problems = [f"scenario {scenario['id']} names unknown feature {scenario['feature']}" for scenario in scenarios if scenario["feature"] not in feature_ids]
    for scenario in scenarios:
        for step in scenario["steps"]:
            if "wait" in step and set(step["wait"]) - {"timeout"} != {"electron", "native"}:
                problems.append(f"scenario {scenario['id']} has a wait without a condition for each client")
            problems.extend(
                f"scenario {scenario['id']} captures undefined target {target}"
                for target in step.get("targets", [])
                if set(targets.get(target, {})) != {"electron", "native"}
            )
    for feature in features:
        problems.extend(f"feature {feature['id']} lists unknown scenario {scenario}" for scenario in feature["scenarios"] if scenario not in scenario_ids)
    return problems


def inventory_coverage(features: list[dict], results: dict[str, dict]) -> dict:
    """Coverage against the whole inventory: a feature passes only when every
    scenario it lists ran and passed; skipped or manual features never count."""
    counts = {"features": len(features), "passed": 0, "failed": 0, "incomplete": 0, "notRun": 0, "manual": 0, "byFeature": {}}
    for feature in features:
        statuses = [results.get(scenario, {}).get("status", "not run") for scenario in feature["scenarios"]]
        if not feature["scenarios"]:
            status = "manual"
        elif any(status in ("failed", "blocked") for status in statuses):
            status = "failed"
        elif statuses and all(status == "passed" for status in statuses):
            status = "passed"
        elif any(status == "incomplete" for status in statuses):
            status = "incomplete"
        else:
            status = "not run"
        counts["byFeature"][feature["id"]] = status
        counts[{"passed": "passed", "failed": "failed", "incomplete": "incomplete", "not run": "notRun", "manual": "manual"}[status]] += 1
    return counts


def run_scenarios(scenarios: list[dict], wanted: set[str] | None, output: Path, report: dict, args: argparse.Namespace) -> int:
    failed = 0
    for scenario in scenarios:
        if wanted and scenario["id"] not in wanted:
            continue
        work = output / scenario["id"]
        shutil.rmtree(work, ignore_errors=True)
        result = {"feature": scenario["feature"], "electron": run_client("electron", scenario, work, args), "native": run_client("native", scenario, work, args)}
        if "error" in result["electron"] or "error" in result["native"]:
            result["status"] = "blocked" if "error" in result["electron"] else "failed"
        else:
            comparisons = {}
            for name, reference in result["electron"]["checkpoints"].items():
                candidate = result["native"]["checkpoints"].get(name)
                if candidate is None:
                    comparisons[name] = {"missing": True}
                    continue
                image = compare.compare_images(Path(reference["image"]), Path(candidate["image"]), work / name, args.tolerance)
                rects_reference = {row["key"]: row["rect"] for row in reference["rows"]}
                rects_candidate = {row["key"]: row["rect"] for row in candidate["rows"]}
                for target, rect in reference.get("targets", {}).items():
                    rects_reference[f"target:{target}"] = rect
                for target, rect in candidate.get("targets", {}).items():
                    rects_candidate[f"target:{target}"] = rect
                unobserved: list[dict] = []
                rows = compare_rows(reference["rows"], candidate["rows"], frozenset(candidate.get("unobservable", ())), unobserved)
                # An input the host could not deliver was replaced; its path is unverified.
                unobserved.extend(candidate.get("substitutions", []))
                comparisons[name] = {
                    "image": image.as_dict(),
                    "rows": rows,
                    "unobserved": unobserved,
                    "geometry": compare.compare_geometry(rects_reference, rects_candidate, args.scale),
                }
            for name, comparison in comparisons.items():
                if not comparison.get("missing"):
                    candidate = result["native"]["checkpoints"][name]
                    comparison["unmetWaits"] = [wait for wait in candidate.get("waits", []) if not wait["met"]]
                    comparison["nativeUnsettled"] = not candidate.get("settled", True)
            result["comparisons"] = comparisons
            # A reference that never reached its state, or never held still,
            # is no reference.
            reference_unmet = any(
                not wait["met"] for checkpoint in result["electron"]["checkpoints"].values() for wait in checkpoint.get("waits", [])
            ) or any(not checkpoint.get("settled", True) for checkpoint in result["electron"]["checkpoints"].values())
            passed = all(
                not comparison.get("missing")
                and comparison["image"]["sizeMatches"]
                and comparison["image"]["differingPixels"] == 0
                and not comparison["rows"]
                and not comparison["geometry"]
                and not comparison["unmetWaits"]
                and not comparison["nativeUnsettled"]
                for comparison in comparisons.values()
            )
            # Checks the candidate's platform cannot observe leave a clean
            # scenario incomplete: unverified is never passed.
            unverified = any(comparison.get("unobserved") for comparison in comparisons.values())
            if reference_unmet:
                result["status"] = "blocked"
            else:
                result["status"] = "failed" if not passed else "incomplete" if unverified else "passed"
        failed += result["status"] != "passed"
        report["scenarios"][scenario["id"]] = result
        print(f"{scenario['id']}: {result['status']}", flush=True)
        for name, comparison in result.get("comparisons", {}).items():
            if comparison.get("missing"):
                print(f"  {name}: no native checkpoint")
                continue
            image = comparison["image"]
            print(f"  {name}: {image['differingPixels']} differing pixels ({image['differingShare']:.1%}), "
                  f"{len(comparison['rows'])} row differences, {len(comparison['geometry'])} geometry differences, "
                  f"{len(comparison['unobserved'])} checks not observable natively, "
                  f"{len(comparison['unmetWaits'])} native waits not met"
                  + (", native never settled" if comparison["nativeUnsettled"] else ""))
        for kind in ("electron", "native"):
            if "error" in result[kind]:
                print(f"  {kind}: {result[kind]['error']}")
    return failed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("seed", "run"):
        command = commands.add_parser(name)
        command.add_argument("--root", type=Path, default=REPO_ROOT / "tmp" / "parity", help="seed and project directory")
        command.add_argument("--dist", type=Path, default=DEFAULT_DIST, help="built web UI the isolated server serves")
        command.add_argument("--opencode-binary", type=Path, default=DEFAULT_OPENCODE)
        command.add_argument("--timeout", type=float, default=120.0)
    commands.choices["seed"].add_argument("--force", action="store_true", help="rebuild an existing seed")
    runner = commands.choices["run"]
    runner.add_argument("--scenarios", help="comma-separated scenario IDs; all by default")
    runner.add_argument("--electron-app", type=Path, default=DEFAULT_ELECTRON)
    runner.add_argument("--native-binary", type=Path, default=DEFAULT_NATIVE)
    runner.add_argument("--theme", default="openchamber-dark")
    runner.add_argument("--width", type=int, default=1280)
    runner.add_argument("--height", type=int, default=800)
    runner.add_argument("--scale", type=float, default=1.0)
    runner.add_argument("--tolerance", type=int, default=0, help="largest per-channel difference ignored; reviewed noise only")
    runner.add_argument("--output", type=Path, help="report directory, outside the repository's sources")
    runner.add_argument(
        "--display", choices=("private", "session"), default="private",
        help="private: a headless compositor nothing else reaches (default); session: the desktop's own display",
    )
    args = parser.parse_args()
    if args.command == "seed":
        return seed(args)
    if args.display == "private" and not virtual_display.inside():
        problem = virtual_display.available()
        if problem:
            print(f"no private display: {problem}; pass --display session to use the desktop's", file=sys.stderr)
            return 1
        command = [sys.executable, "-u", str(Path(__file__).resolve()), *sys.argv[1:]]
        return virtual_display.run_inside(command, args.width, args.height)
    return run(args)


if __name__ == "__main__":
    sys.exit(main())
