"""IBus receives its config helper as a shell-parsed command line."""

from pathlib import Path
import shlex
import tempfile
import unittest
from unittest.mock import patch

import ime_fixture


class ConfigCommandTests(unittest.TestCase):
    def test_config_command_preserves_spaces_and_apostrophes(self):
        for directory in ('helper directory', "helper's directory"):
            with self.subTest(directory=directory), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                config = root / directory / 'ibus-dconf'
                config.parent.mkdir()
                config.write_text('#!/bin/sh\nexit 0\n')
                config.chmod(0o755)
                fixture = ime_fixture.IBusFixture(root / 'profile')
                launches = []

                class CapturedLaunch(Exception):
                    pass

                def capture(name, argv):
                    launches.append(argv)
                    raise CapturedLaunch()

                with patch.object(ime_fixture.virtual_display, 'verify_owned_display'), \
                     patch.object(ime_fixture.virtual_display, 'child_environment',
                                  side_effect=lambda env: {**env, 'XDG_RUNTIME_DIR': temporary}), \
                     patch.object(ime_fixture, 'LIBPINYIN', config), \
                     patch.object(ime_fixture, 'require_executable', return_value=config), \
                     patch.object(fixture, '_command'), \
                     patch.object(fixture, '_start', side_effect=capture):
                    try:
                        with self.assertRaises(CapturedLaunch):
                            fixture.start()
                    finally:
                        fixture.close()
                command = next(arg.split('=', 1)[1] for arg in launches[0] if arg.startswith('--config='))
                self.assertEqual(shlex.split(command), [str(config)])


if __name__ == '__main__':
    unittest.main()
