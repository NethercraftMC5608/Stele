"""Stele's imported LKL entry point must not need NETHOS's package manager."""
from pathlib import Path
import runpy
import subprocess
import sys
import unittest
from unittest.mock import patch

LDK = Path(__file__).resolve().parents[1] / "ldk/ldk.py"


class Entrypoint(unittest.TestCase):
    def test_lkl_help_and_ports_without_nethos_pkg(self):
        for args, marker in ((["lkl", "--help"], "--arch"), (["ports"], "virtio-blk")):
            result = subprocess.run([sys.executable, "-B", str(LDK)] + args,
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(marker, result.stdout)

    def test_lkl_build_creates_missing_arch_image_and_checks_before_publish(self):
        module = runpy.run_path(str(LDK))
        commands = []

        def run(command, **kwargs):
            commands.append(command)
            return subprocess.CompletedProcess(command, 1 if command[1:3] == ["image", "inspect"] else 0)

        from types import SimpleNamespace
        import tempfile
        with tempfile.TemporaryDirectory() as tmp, patch.object(subprocess, "run", side_effect=run):
            module["cmd_lkl"].__globals__["BUILD"] = Path(tmp)
            module["cmd_lkl"].__globals__["ROOT"] = Path(tmp)
            module["cmd_lkl"](SimpleNamespace(arch="x86_64"))
        builds = [c for c in commands if c[1] == "build"]
        self.assertEqual(len(builds), 1)
        self.assertIn("linux/amd64", builds[0])
        self.assertIn("nethos-ldk-amd64", builds[0])
        script = commands[-1][-1]
        self.assertLess(script.index("lkl_abi.py /out/libnklkl.a.tmp"), script.index("mv /out/libnklkl.a.tmp"))


if __name__ == "__main__":
    unittest.main()
