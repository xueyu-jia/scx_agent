from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]


class MixedClassStagingTest(unittest.TestCase):
    def test_staged_script_imports_bench_from_guest_workdir(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            staged = Path(temporary) / "mixed_class.py"
            shutil.copy2(REPO_ROOT / "bench/benchmarks/mixed_class.py", staged)
            env = {**os.environ, "SCX_BENCH_WORKDIR": str(REPO_ROOT)}
            result = subprocess.run(
                [sys.executable, str(staged), "--help"],
                cwd="/",
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )

        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
