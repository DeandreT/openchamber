"""Locate the same Linux helper across distro packaging layouts."""

import os
from pathlib import Path
import shutil


def executable_path(name: str, *candidates: str) -> Path:
    """Return an executable on PATH or a known path; retain the first missing
    candidate for the caller's prerequisite error when none is installed.

    Resolution never runs a helper or substitutes a different executable name.
    Callers must check the result before launching it.
    """
    found = shutil.which(name)
    for value in ([found] if found else []) + list(candidates):
        path = Path(value)
        if path.is_file() and os.access(path, os.X_OK):
            return path.absolute()
    return Path(candidates[0])


def require_executable(name: str, *candidates: str) -> Path:
    path = executable_path(name, *candidates)
    if not path.is_file() or not os.access(path, os.X_OK):
        raise RuntimeError(f"required executable {name} is unavailable; checked PATH and {', '.join(candidates)}")
    return path
