#!/usr/bin/env python3
"""Exercise installer resolution and generated artefacts without installing.

Build waldo-service and waldo-cli first; pass --binaries when their debug
binaries live outside target/debug. Every subprocess uses private paths.
"""

import argparse
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import time
import unittest

REPO = Path(__file__).resolve().parent.parent
BINARIES = REPO / "target/debug"
SCRATCH = Path("/tmp/waldo-installer-tests")


class InstallerTests(unittest.TestCase):
    def setUp(self):
        SCRATCH.mkdir(parents=True, exist_ok=True)
        self.directory = tempfile.TemporaryDirectory(prefix="case-", dir=SCRATCH)
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith("WALDO_") and key not in
                    ("CARGO_HOME", "CARGO_INSTALL_ROOT", "CODEX_HOME", "XDG_CONFIG_HOME")}
        self.env.update(HOME=str(self.root / "home"),
                        XDG_RUNTIME_DIR=str(self.root / "runtime"),
                        TMPDIR=str(self.root))
        # Preserve access to the session's Wayland socket while redirecting
        # the controller default to a private runtime directory.
        if self.env.get("WAYLAND_DISPLAY") and os.environ.get("XDG_RUNTIME_DIR"):
            self.env["WAYLAND_DISPLAY"] = str(Path(os.environ["XDG_RUNTIME_DIR"]) / self.env["WAYLAND_DISPLAY"])
        Path(self.env["HOME"]).mkdir()
        Path(self.env["XDG_RUNTIME_DIR"]).mkdir(mode=0o700)

    def steps(self, script, **environment):
        # Old installers execute on source: never source one and risk an install.
        if "# Installation entry point." not in (REPO / "install-local").read_text():
            raise RuntimeError("installer lacks safe sourcing; use mutation fallback")
        result = subprocess.run(
            ["bash", "-c", f"source {shlex.quote(str(REPO / 'install-local'))}\n{script}"],
            env=self.env | environment, text=True, capture_output=True, timeout=15)
        return result

    def successful_steps(self, script, **environment):
        result = self.steps(script, **environment)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def test_binary_root_and_service_use_the_same_destination(self):
        destination = self.root / 'cargo root % $place'
        binary = destination / "bin/waldod"
        binary.parent.mkdir(parents=True)
        binary.write_text("#!/bin/sh\nexit 0\n")
        binary.chmod(0o755)
        service = self.root / "units/waldod.service"
        paths = self.successful_steps(
            'resolve_installation\nwrite_service_file\n'
            'printf "%s\\n" "$waldod_bin" "$waldo_bin" "$mcp_bin"',
            WALDO_CARGO_INSTALL_ROOT=str(destination), WALDO_SERVICE_FILE=str(service))
        self.assertEqual(paths.splitlines(), [str(destination / "bin" / name)
                                             for name in ("waldod", "waldo", "waldo-mcp")])
        unit = service.read_text()
        self.assertIn('ExecStart="' + str(binary).replace('"', '\\"')
                      .replace('%', '%%') + '"', unit)
        self.assertNotIn("WALDO_SOCKET_PATH", unit)
        checked = subprocess.run(
            ["systemd-analyze", "--user", "--man=no", "--generators=no", "verify", str(service)],
            env=self.env, text=True, capture_output=True)
        self.assertEqual(checked.returncode, 0, checked.stderr)

    def test_service_carries_only_explicit_socket_and_config_overrides(self):
        service = self.root / "units/waldod.service"
        socket = str(self.root / 'socket % " $name.sock')
        config = str(self.root / "config")
        self.successful_steps('resolve_installation\nwrite_service_file',
                              WALDO_SERVICE_FILE=str(service), WALDO_SOCKET_PATH=socket,
                              XDG_CONFIG_HOME=config)
        unit = service.read_text()
        self.assertIn('Environment="WALDO_SOCKET_PATH=' + socket.replace('"', '\\"')
                      .replace('%', '%%') + '"', unit)
        self.assertIn(f'Environment="XDG_CONFIG_HOME={config}"', unit)
        self.assertNotIn("--socket", unit)

    def test_rehearsal_exports_private_default_and_binary_path_to_proof(self):
        install_root = self.root / "rehearsal"
        runtime = self.root / "private-runtime"
        output = self.successful_steps('resolve_installation\nrun_codex env',
                                       WALDO_INSTALL_ROOT=str(install_root),
                                       WALDO_RUNTIME_DIR=str(runtime))
        environment = dict(line.split("=", 1) for line in output.splitlines())
        self.assertEqual(environment["WALDO_SOCKET_PATH"], str(runtime / "waldo/controller.sock"))
        self.assertEqual(environment["HOME"], str(install_root / "home"))
        self.assertEqual(environment["XDG_CONFIG_HOME"], str(install_root / "home/.config"))
        self.assertEqual(environment["PATH"].split(":", 1)[0], str(install_root / "cargo/bin"))
        socket = str(self.root / "override.sock")
        output = self.successful_steps('resolve_installation\nrun_codex env',
                                       WALDO_INSTALL_ROOT=str(install_root), WALDO_SOCKET_PATH=socket)
        self.assertIn(f"WALDO_SOCKET_PATH={socket}\n", output)

    def test_marketplace_uses_configured_name_and_owner_for_both_writer_branches(self):
        marketplace = self.root / "marketplace.json"
        for existing in (False, True):
            with self.subTest(existing=existing):
                if existing:
                    marketplace.write_text('{"name":"old","plugins":[]}')
                self.successful_steps('resolve_installation\nmarketplace_source=./plugins/waldo\n'
                                      'write_codex_marketplace',
                                      WALDO_MARKETPLACE=str(marketplace),
                                      WALDO_MARKETPLACE_NAME="workshop",
                                      WALDO_MARKETPLACE_OWNER='Workshop "Owner"')
                manifest = json.loads(marketplace.read_text())
                self.assertEqual(manifest["name"], "workshop")
                self.assertEqual(manifest["owner"]["name"], 'Workshop "Owner"')
                self.assertEqual(manifest["plugins"][0]["source"]["path"], "./plugins/waldo")

    def test_readiness_queries_real_controller_via_environment_and_settings_file(self):
        for source in ("environment", "file"):
            with self.subTest(source=source):
                socket = self.root / f"{source}/controller.sock"
                socket.parent.mkdir(mode=0o700)
                config = self.root / f"{source}-config"
                (config / "waldo").mkdir(parents=True)
                environment = self.env | {"XDG_CONFIG_HOME": str(config)}
                if source == "environment":
                    environment["WALDO_SOCKET_PATH"] = str(socket)
                    (config / "waldo/settings").write_text(
                        f"WALDO_SOCKET_PATH={self.root}/wrong.sock\n")
                else:
                    (config / "waldo/settings").write_text(f"WALDO_SOCKET_PATH={socket}\n")
                with open(self.root / f"{source}-daemon.log", "w") as log:
                    controller = subprocess.Popen([str(BINARIES / "waldod")], env=environment,
                                                  stdout=log, stderr=log)
                    try:
                        deadline = time.monotonic() + 5
                        while not socket.exists() and controller.poll() is None and time.monotonic() < deadline:
                            time.sleep(0.01)
                        self.assertTrue(socket.is_socket(), "real controller did not bind the test socket")
                        query = self.root / f"{source}-query.json"
                        result = self.steps(
                            f'resolve_installation\nwaldo_bin={shlex.quote(str(BINARIES / "waldo"))}\n'
                            f'wait_for_controller {shlex.quote(str(query))}', **environment)
                        self.assertEqual(result.returncode, 0, result.stderr)
                        self.assertEqual(json.loads(query.read_text())["type"], "queried")
                        self.assertGreater(len(json.loads(query.read_text())["result"]["outputs"]), 0)
                        self.assertFalse((Path(self.env["XDG_RUNTIME_DIR"]) / "waldo/controller.sock").exists())
                    finally:
                        controller.terminate()
                        controller.wait(timeout=5)
                self.assertFalse(socket.exists(), "real controller left its socket behind")

    def test_backend_error_reply_is_not_readiness(self):
        socket = self.root / "error/controller.sock"
        socket.parent.mkdir(mode=0o700)
        environment = self.env | {"WALDO_SOCKET_PATH": str(socket),
                                  "WAYLAND_DISPLAY": str(self.root / "missing-wayland")}
        with open(self.root / "error-daemon.log", "w") as log:
            controller = subprocess.Popen([str(BINARIES / "waldod")], env=environment,
                                          stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 5
                while not socket.exists() and controller.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertTrue(socket.is_socket(), "real controller did not bind the test socket")
                query = self.root / "error-query.json"
                result = self.steps(
                    f'resolve_installation\nwaldo_bin={shlex.quote(str(BINARIES / "waldo"))}\n'
                    f'wait_for_controller {shlex.quote(str(query))}', **environment)
                self.assertNotEqual(result.returncode, 0, "backend failure was accepted as readiness")
                self.assertEqual(json.loads(query.read_text())["type"], "error")
            finally:
                controller.terminate()
                controller.wait(timeout=5)
        self.assertFalse(socket.exists(), "real controller left its socket behind")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--binaries", type=Path, default=BINARIES)
    options, remaining = parser.parse_known_args()
    BINARIES = options.binaries.resolve()
    try:
        unittest.main(argv=[__file__, *remaining], verbosity=2)
    finally:
        # Cases own their directories; remove the task parent when empty.
        if SCRATCH.exists():
            SCRATCH.rmdir()
