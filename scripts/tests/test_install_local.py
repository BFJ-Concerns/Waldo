#!/usr/bin/env python3
"""Exercise installer resolution, generated artefacts and proof judgement
without installing.

The installer's steps are sourced into a private environment and run one at
a time; nothing here registers a plugin, starts the user service or runs an
agent. The readiness cases start the real controller from target/debug, so
build waldo-service and waldo-cli first (pass --binaries when the debug
binaries live elsewhere); they need the Niri session and skip without it.
"""

import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import time
import unittest

REPO = Path(__file__).resolve().parents[2]
BINARIES = REPO / "target/debug"
# Resolved once so a case may hand the installer a PATH of stub commands.
BASH = shutil.which("bash")
SESSION = all(os.environ.get(name) for name in ("XDG_RUNTIME_DIR", "WAYLAND_DISPLAY", "NIRI_SOCKET"))

OBSERVE_TOOL = "mcp__plugin_waldo_waldo__observe"
QUERY_TOOL = "mcp__plugin_waldo_waldo__query"


def init_event(plugin_source, plugin_path, server_status="connected"):
    return {"type": "system", "subtype": "init",
            "plugins": [{"name": "waldo", "source": plugin_source, "path": plugin_path, "version": "0.1.0"}],
            "mcp_servers": [{"name": "plugin:waldo:waldo", "status": server_status, "source": "plugin"}],
            "tools": [QUERY_TOOL, OBSERVE_TOOL]}


def tool_use(tool_id, name):
    return {"type": "assistant", "message": {"role": "assistant", "content": [
        {"type": "tool_use", "id": tool_id, "name": name, "input": {}}]}}


def tool_result(tool_id, content, structured=None):
    """A tool result as Claude Code streams it: the content blocks under the
    message and, beside it, the raw result with its structured content."""
    result = {"content": content}
    if structured is not None:
        result["structuredContent"] = structured
    return {"type": "user", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": tool_id, "content": content}]},
        "tool_use_result": result}


def image_block(data="iVBORw0KGgo="):
    return {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": data}}


def observation(delivered):
    return {"type": "observed", "observation": {"id": 1, "image": {"delivered_as_content": delivered}}}


def result_event(text):
    return {"type": "result", "subtype": "success", "is_error": False, "result": text}


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="waldo-installer-case-")
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
            [BASH, "-c", f"source {shlex.quote(str(REPO / 'install-local'))}\n{script}"],
            env=self.env | environment, text=True, capture_output=True, timeout=15)
        return result

    def successful_steps(self, script, **environment):
        result = self.steps(script, **environment)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def stub_commands(self, label, *names):
        """A PATH holding only executables of the given names."""
        stubs = self.root / f"stubs-{label}"
        stubs.mkdir()
        for name in names:
            stub = stubs / name
            stub.write_text("#!/bin/sh\nexit 0\n")
            stub.chmod(0o755)
        return str(stubs)

    def write_transcript(self, name, events):
        transcript = self.root / name
        transcript.write_text("".join(json.dumps(event) + "\n" for event in events))
        return transcript

    def claude_summary(self, transcript, plugin_source="waldo@personal"):
        """The summary the installer derives from a transcript, and whether it
        counts as proof."""
        output = self.successful_steps(
            f'summary="$(summarise_claude_turn {shlex.quote(str(transcript))} {shlex.quote(plugin_source)})"\n'
            'printf "%s\\n" "$summary"\n'
            'if claude_turn_proves "$summary"; then echo proves; else echo refuses; fi')
        summary_text, verdict = output.rsplit("\n", 2)[:2]
        return json.loads(summary_text), verdict == "proves"

    def test_required_commands_name_claude_and_not_codex(self):
        required = ("cargo", "claude", "git", "jq", "realpath", "rsync", "sha256sum", "systemctl")
        complete = self.stub_commands("complete", *required)
        self.successful_steps(f"PATH={shlex.quote(complete)} require_commands")
        codex_only = self.stub_commands("codex-only", *(name for name in required if name != "claude"), "codex")
        without_claude = self.steps(f"PATH={shlex.quote(codex_only)} require_commands")
        self.assertNotEqual(without_claude.returncode, 0)
        self.assertIn("requires claude", without_claude.stderr)

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

    def test_rehearsal_runs_claude_as_the_operator_with_the_candidate_binaries(self):
        install_root = self.root / "rehearsal"
        output = self.successful_steps('resolve_installation\nrun_claude env',
                                       WALDO_INSTALL_ROOT=str(install_root))
        environment = dict(line.split("=", 1) for line in output.splitlines())
        self.assertEqual(environment["HOME"], self.env["HOME"])
        self.assertEqual(environment["PATH"].split(":", 1)[0], str(install_root / "cargo/bin"))
        socket = Path(environment["WALDO_SOCKET_PATH"])
        self.assertEqual(socket.parent.parent.parent, Path(self.env["XDG_RUNTIME_DIR"]))
        self.assertTrue(socket.parent.parent.name.startswith("waldo-install-"), socket)
        self.assertEqual(socket.relative_to(socket.parent.parent), Path("waldo/controller.sock"))

    def test_codex_marketplace_uses_configured_name_and_owner_for_both_writer_branches(self):
        marketplace = self.root / "marketplace.json"
        for existing in (False, True):
            with self.subTest(existing=existing):
                if existing:
                    marketplace.write_text('{"name":"old","plugins":[]}')
                self.successful_steps('resolve_installation\nwrite_codex_marketplace',
                                      WALDO_MARKETPLACE=str(marketplace),
                                      WALDO_MARKETPLACE_NAME="workshop",
                                      WALDO_MARKETPLACE_OWNER='Workshop "Owner"')
                manifest = json.loads(marketplace.read_text())
                self.assertEqual(manifest["name"], "workshop")
                self.assertEqual(manifest["owner"]["name"], 'Workshop "Owner"')
                self.assertEqual(manifest["interface"], {"displayName": "Personal"})
                self.assertEqual(manifest["plugins"][0]["source"]["path"], "./plugins/waldo")

    def test_claude_marketplace_keeps_other_plugins_and_replaces_waldo(self):
        plugin_home = self.root / "plugins/waldo"
        marketplace = self.root / "plugins/.claude-plugin/marketplace.json"
        marketplace.parent.mkdir(parents=True)
        other = {"name": "sentinel", "source": "./sentinel", "description": "unrelated"}
        marketplace.write_text(json.dumps({
            "name": "old", "owner": {"name": "Old Owner"}, "metadata": {"description": "kept"},
            "plugins": [other, {"name": "waldo", "source": "./stale", "stale": True}]}))
        self.successful_steps('resolve_installation\nwrite_claude_marketplace',
                              WALDO_PLUGIN_HOME=str(plugin_home),
                              WALDO_MARKETPLACE_NAME="workshop", WALDO_MARKETPLACE_OWNER="Workshop Owner")
        manifest = json.loads(marketplace.read_text())
        self.assertEqual(manifest["name"], "workshop")
        self.assertEqual(manifest["owner"], {"name": "Workshop Owner"})
        self.assertEqual(manifest["metadata"], {"description": "kept"})
        self.assertEqual(manifest["plugins"], [other, {"name": "waldo", "source": "./waldo"}])

    def test_claude_marketplace_is_created_beside_the_plugin_home(self):
        plugin_home = self.root / "elsewhere/waldo-candidate"
        self.successful_steps('resolve_installation\nwrite_claude_marketplace\n'
                              'printf "%s\\n" "$claude_marketplace_root"',
                              WALDO_PLUGIN_HOME=str(plugin_home))
        manifest = json.loads((self.root / "elsewhere/.claude-plugin/marketplace.json").read_text())
        self.assertEqual(manifest["name"], "personal")
        self.assertEqual(manifest["plugins"], [{"name": "waldo", "source": "./waldo-candidate"}])

    def test_plugin_copy_names_the_resolved_binary_in_both_mcp_manifests(self):
        sources = {name: (REPO / "plugins/waldo" / name).read_bytes() for name in (".mcp.json", ".mcp.claude.json")}
        destination = self.root / 'cargo root " $place'
        install_root = self.root / "rehearsal"
        plugin_home = install_root / "home/plugins/waldo"
        self.successful_steps('resolve_installation\ninstall_plugin_copy',
                              WALDO_INSTALL_ROOT=str(install_root),
                              WALDO_CARGO_INSTALL_ROOT=str(destination))
        for name, original in sources.items():
            with self.subTest(manifest=name):
                installed = json.loads((plugin_home / name).read_text())
                expected = json.loads(original)
                expected["mcpServers"]["waldo"]["command"] = str(destination / "bin/waldo-mcp")
                self.assertEqual(installed, expected)
                self.assertEqual((REPO / "plugins/waldo" / name).read_bytes(), original)
        for client in ("claude", "codex"):
            version = json.loads((plugin_home / f".{client}-plugin/plugin.json").read_text())["version"]
            self.assertRegex(version, rf"^0\.1\.0\+{client}\.\d{{14}}$")
        self.assertTrue((plugin_home / "skills/desktop-control/SKILL.md").is_file())

    def test_claude_turn_proof_needs_an_observe_result_with_a_delivered_image(self):
        events = [init_event("waldo@personal", "/home/operator/plugins/waldo"),
                  tool_use("q1", QUERY_TOOL),
                  tool_result("q1", [{"type": "text", "text": "{\"type\":\"queried\"}"}]),
                  tool_use("o1", OBSERVE_TOOL),
                  tool_result("o1", [image_block(), {"type": "text", "text": "{}"}], observation(True)),
                  result_event("MCP_IMAGE_PROOF_OK")]
        summary, proves = self.claude_summary(self.write_transcript("proof.jsonl", events))
        self.assertTrue(proves, summary)
        self.assertEqual(summary, {"plugin_source": "waldo@personal", "plugins_loaded": 1,
                                   "plugin_paths": ["/home/operator/plugins/waldo"], "servers_connected": 1,
                                   "observe_calls": 1, "image_results": 1, "mutating_calls": 0,
                                   "completion_markers": 1})

    def test_claude_turn_proof_refuses_a_reply_without_the_tool_result(self):
        # The model's words are never the proof: a turn that claims success
        # with no Observe tool result, or with one carrying no image, or with
        # the plugin loaded from somewhere else, is refused.
        loaded = init_event("waldo@personal", "/home/operator/plugins/waldo")
        cases = {
            "no observe call": [loaded, result_event("MCP_IMAGE_PROOF_OK")],
            "observe result without an image": [
                loaded, tool_use("o1", OBSERVE_TOOL),
                tool_result("o1", [{"type": "text", "text": "{}"}], observation(True)),
                result_event("MCP_IMAGE_PROOF_OK")],
            "image the observation did not deliver in-turn": [
                loaded, tool_use("o1", OBSERVE_TOOL),
                tool_result("o1", [image_block()], observation(False)),
                result_event("MCP_IMAGE_PROOF_OK")],
            "plugin loaded from another source": [
                init_event("waldo@inline", "/tmp/candidate"), tool_use("o1", OBSERVE_TOOL),
                tool_result("o1", [image_block()], observation(True)),
                result_event("MCP_IMAGE_PROOF_OK")],
            "mutating call": [
                loaded, tool_use("o1", OBSERVE_TOOL),
                tool_result("o1", [image_block()], observation(True)),
                tool_use("a1", "mcp__plugin_waldo_waldo__act"),
                tool_result("a1", [{"type": "text", "text": "denied"}]),
                result_event("MCP_IMAGE_PROOF_OK")],
        }
        for name, events in cases.items():
            with self.subTest(case=name):
                summary, proves = self.claude_summary(self.write_transcript("refused.jsonl", events))
                self.assertFalse(proves, summary)

    @unittest.skipUnless(SESSION, "needs the Niri session to answer the readiness query")
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
        self.assertTrue((BINARIES / "waldod").is_file(), f"build waldo-service first ({BINARIES})")
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
    unittest.main(argv=[__file__, *remaining], verbosity=2)
