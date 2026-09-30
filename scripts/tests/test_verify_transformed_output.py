"""Exercise the nested-Niri launcher's own argument parser in fresh processes."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "verify-transformed-output.py"

class LauncherDefaults(unittest.TestCase):
    def test_portable_scratch_default_and_explicit_override(self):
        with tempfile.TemporaryDirectory(prefix="waldo-parser-") as parent:
            code = """
import importlib.util
from pathlib import Path
import sys
import tempfile
spec = importlib.util.spec_from_file_location('launcher', sys.argv[1])
launcher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(launcher)
_, default = launcher.parse_options(['--bin-dir', 'bin'])
assert default.scratch_parent == Path(tempfile.gettempdir()), (default.scratch_parent, tempfile.gettempdir())
_, explicit = launcher.parse_options(['--bin-dir', 'bin', '--scratch-parent', 'operator-scratch'])
assert explicit.scratch_parent == Path('operator-scratch'), explicit.scratch_parent
print('launcher default honours TMPDIR; explicit override preserved')
"""
            result = subprocess.run([sys.executable, "-S", "-c", code, str(SCRIPT)], env={**os.environ, "TMPDIR": parent}, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

if __name__ == "__main__":
    unittest.main()
