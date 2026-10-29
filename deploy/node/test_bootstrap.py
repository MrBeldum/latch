from pathlib import Path
import os
import tempfile
import tomllib
import unittest
from bootstrap import configure


class BootstrapTest(unittest.TestCase):
    def test_restart_preserves_preferences_and_prep(self):
        with tempfile.TemporaryDirectory() as tmp:
            host = Path(tmp) / 'host.toml'
            configure(host)
            self.assertEqual(os.stat(host).st_mode & 0o777, 0o600)
            host.write_text('power_allowed = false\nprep_do = "resize"\n')
            configure(host)
            self.assertEqual(tomllib.loads(host.read_text()), {'power_allowed': False, 'prep_do': 'resize'})

    def test_corruption_is_not_overwritten(self):
        with tempfile.TemporaryDirectory() as tmp:
            host = Path(tmp) / 'host.toml'
            host.write_text('bad = [')
            with self.assertRaises(tomllib.TOMLDecodeError):
                configure(host)
            self.assertEqual(host.read_text(), 'bad = [')


if __name__ == '__main__':
    unittest.main()
