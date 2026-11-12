"""Build the selected working-tree reference without replacing prior artifacts."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


ROOT = Path("/home/deandre/projects/rust/OpenChamber")
BUN = Path("/home/deandre/.local/share/bun-1.4.4/bun-linux-x64/bun")


def main():
    output = Path(tempfile.mkdtemp(prefix="electron-upstream-reference-", dir=ROOT / "native/target/tmp"))
    output.chmod(0o700)
    source = output / "source"
    source.mkdir(mode=0o700)
    names = subprocess.check_output([
        "git", "ls-files", "-z", "--cached", "--others", "--exclude-standard",
    ], cwd=ROOT).split(b"\0")
    records = {}
    for raw in sorted(set(names)):
        if not raw:
            continue
        relative = Path(os.fsdecode(raw))
        if relative.is_absolute() or ".." in relative.parts or relative.parts[0] in ("native", ".git"):
            continue
        original = ROOT / relative
        target = source / relative
        if original.is_symlink():
            target.parent.mkdir(parents=True, exist_ok=True)
            link = os.readlink(original)
            target.symlink_to(link)
            records[str(relative)] = {"symlink": link}
        elif original.is_file():
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(original, target)
            digest = hashlib.sha256(target.read_bytes()).hexdigest()
            if hashlib.sha256(original.read_bytes()).hexdigest() != digest:
                raise RuntimeError(f"source changed during capture: {relative}")
            records[str(relative)] = {"sha256": digest}
    (output / "source-receipt.json").write_text(json.dumps(records, indent=2) + "\n")
    environment = os.environ.copy()
    environment["BUN"] = str(BUN)
    environment["CSC_IDENTITY_AUTO_DISCOVERY"] = "false"
    commands = [
        [str(BUN), "install", "--frozen-lockfile", "--ignore-scripts"],
        [str(BUN), "run", "--cwd", "packages/electron", "ensure:electron"],
        [str(BUN), "run", "--cwd", "packages/electron", "package", "--dir"],
        [str(BUN), "run", "--cwd", "packages/electron", "verify:opencode-cli:packaged"],
    ]
    outcomes = []
    for command in commands:
        print(json.dumps({"reference": str(output), "command": command}), flush=True)
        result = subprocess.run(command, cwd=source, env=environment, stdin=subprocess.DEVNULL)
        outcomes.append({"command": command, "exitCode": result.returncode})
        (output / "build-receipt.json").write_text(json.dumps({"source": str(source), "steps": outcomes}, indent=2) + "\n")
        if result.returncode:
            raise SystemExit(result.returncode)
    binary = source / "packages/electron/dist/linux-unpacked/openchamber"
    manifest = {
        "source": str(source), "binary": str(binary),
        "binarySha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "appVersion": json.loads((source / "packages/electron/package.json").read_text())["version"],
        "electronVersion": json.loads((source / "packages/electron/node_modules/electron/package.json").read_text())["version"],
        "opencodeVersion": json.loads((source / "packages/electron/package.json").read_text())["opencodeCli"]["version"],
    }
    (output / "reference.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps(manifest, indent=2), flush=True)


if __name__ == "__main__":
    main()
