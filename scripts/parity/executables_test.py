"""Executable discovery across Linux packaging layouts."""

import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from executables import executable_path, require_executable


class ExecutablePathTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.legacy = self.root / 'lib' / 'fixture-helper'
        self.modern = self.root / 'libexec' / 'fixture-helper'
        self.path = self.root / 'bin' / 'fixture-helper'
        self.environment = patch.dict(os.environ, {'PATH': str(self.path.parent)})
        self.environment.start()
        self.addCleanup(self.environment.stop)

    def install(self, path, executable=True):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text('#!/bin/sh\nexit 0\n')
        path.chmod(0o755 if executable else 0o644)

    def resolve(self):
        return executable_path('fixture-helper', str(self.legacy), str(self.modern))

    def test_path_selects_the_exact_helper_before_distro_candidates(self):
        for path in (self.path, self.legacy, self.modern):
            self.install(path)
        self.assertEqual(self.resolve(), self.path)

    def test_legacy_and_libexec_layouts(self):
        self.install(self.modern)
        self.assertEqual(self.resolve(), self.modern)
        self.install(self.legacy)
        self.assertEqual(self.resolve(), self.legacy)

    def test_non_executable_files_and_directories_are_skipped(self):
        self.install(self.path, executable=False)
        self.legacy.parent.mkdir(parents=True)
        self.legacy.mkdir()
        self.install(self.modern)
        self.assertEqual(self.resolve(), self.modern)

    def test_missing_helper_keeps_diagnostic_path_and_fails_before_launch(self):
        self.install(self.path.with_name('different-helper'))
        self.assertEqual(self.resolve(), self.legacy)
        with self.assertRaisesRegex(RuntimeError, 'fixture-helper is unavailable; checked PATH'):
            require_executable('fixture-helper', str(self.legacy), str(self.modern))

    def test_required_helper_returns_verified_path(self):
        self.install(self.modern)
        self.assertEqual(require_executable('fixture-helper', str(self.legacy), str(self.modern)), self.modern)


if __name__ == '__main__':
    unittest.main()
