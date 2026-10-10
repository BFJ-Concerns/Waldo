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
# The binaries Cargo just built: under CARGO_TARGET_DIR when it is set, as
# Cargo resolves it, else the checkout's target directory. --binaries
# overrides this when the file is run directly.
BINARIES = Path(os.environ.get("CARGO_TARGET_DIR") or REPO / "target").resolve() / "debug"
# Resolved once so a case may hand the installer a PATH of stub commands.
BASH = shutil.which("bash")
SESSION = all(os.environ.get(name) for name in ("XDG_RUNTIME_DIR", "WAYLAND_DISPLAY", "NIRI_SOCKET"))

OBSERVE_TOOL = "mcp__plugin_waldo_waldo__observe"
QUERY_TOOL = "mcp__plugin_waldo_waldo__query"


def init_event(plugin_source, plugin_path, server_status="connected", version="0.1.0"):
    return {"type": "system", "subtype": "init",
            "plugins": [{"name": "waldo", "source": plugin_source, "path": plugin_path, "version": version}],
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
                    ("CARGO_HOME", "CARGO_INSTALL_ROOT", "CODEX_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME")}
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

    def rehearsal_entry(self, failure="", runtime=None, socket_parent=None, service_file=None, profile_file=None):
        """Run the real entry point; external builds and agent turns are isolated."""
        root = self.root / "entry"
        if root.exists():
            shutil.rmtree(root)
        root.mkdir()
        commands = root / "commands"
        commands.mkdir(exist_ok=True)
        for name in ("bash", "python3", "jq", "realpath", "rsync", "systemctl", "id",
                     "dirname", "basename", "mktemp", "chmod", "rm", "rmdir",
                     "stat", "cut", "date", "cmp", "ls", "env", "sleep"):
            (commands / name).symlink_to(shutil.which(name))
        daemon = root / "daemon"
        daemon.write_text("#!/usr/bin/env python3\nimport signal\nsignal.signal(signal.SIGTERM, lambda *_: exit(0))\nsignal.pause()\n")
        daemon.chmod(0o755)
        query = root / "query"
        query.write_text("#!/bin/sh\nprintf '%s\\n' '{\"type\":\"queried\",\"result\":{\"outputs\":[{\"name\":\"fixture\"}]}}'\n")
        query.chmod(0o755)
        real_git, real_sha = shutil.which("git"), shutil.which("sha256sum")
        installer = commands / "install"
        installer.write_text(
            '#!/usr/bin/env python3\nimport os, pathlib, subprocess, sys\n'
            'paths = [pathlib.Path(arg) for arg in sys.argv[1:]]\n'
            'before = {p: p.stat().st_mode for p in paths if p.is_dir()}\n'
            f'result = subprocess.run([{shutil.which("install")!r}, *sys.argv[1:]])\n'
            'with open(pathlib.Path(os.environ["ENTRY"]) / "mode-changes", "a") as log:\n'
            '    for p, mode in before.items():\n'
            '        if p.is_dir() and p.stat().st_mode != mode: print(p, file=log)\n'
            'sys.exit(result.returncode)\n')
        installer.chmod(0o755)
        scripts = {
            "cargo": '#!/bin/bash\nroot=""\nwhile (($#)); do if [[ "$1" == --root ]]; then root="$2"; fi; shift; done\n'
                     'install -d "$root/bin"\ninstall -m 0755 "$ENTRY/daemon" "$root/bin/waldod"\n'
                     'install -m 0755 "$ENTRY/query" "$root/bin/waldo"\n'
                     'printf binary > "$root/bin/waldo-mcp"\n',
            "claude": '#!/bin/bash\nif [[ "$1" == --version ]]; then [[ "$FAIL_PROBE" != claude ]] || exit 41; echo fixture-version; exit; fi\n'
                      'while (($#)); do if [[ "$1" == --plugin-dir ]]; then plugin="$2"; fi; shift; done\n'
                      'python3 "$ENTRY/emit.py" "$plugin"\n',
            "git": f'#!/bin/bash\nif [[ "$FAIL_PROBE" == git-status && "$*" == *status* ]] || [[ "$FAIL_PROBE" == git-head && "$*" == *rev-parse* ]]; then exit 42; fi\nexec {shlex.quote(real_git)} "$@"\n',
            "sha256sum": f'#!/bin/bash\nif [[ "$FAIL_PROBE" == checksum && "$*" == *waldo-mcp* ]]; then rm -f -- "$1"; fi\nexec {shlex.quote(real_sha)} "$@"\n',
        }
        for name, content in scripts.items():
            command = commands / name
            command.write_text(content)
            command.chmod(0o755)
        (root / "emit.py").write_text(
            'import json, pathlib, sys\np = pathlib.Path(sys.argv[1])\n'
            'v = json.loads((p / ".claude-plugin/plugin.json").read_text())["version"]\n'
            'events = ' + repr([
                init_event("waldo@inline", "PLACEHOLDER"), tool_use("o1", OBSERVE_TOOL),
                tool_result("o1", [image_block()], observation(True)), result_event("MCP_IMAGE_PROOF_OK")]) + '\n'
            'events[0]["plugins"][0].update(path=str(p), version=v)\n'
            'for event in events: print(json.dumps(event))\n')
        install_root = root / "installation"
        runtime = runtime or root / "runtime"
        socket_parent = socket_parent or runtime / "waldo"
        profile_file = profile_file or install_root / "state/waldo/environment.json"
        result = subprocess.run([BASH, str(REPO / "install-local")], text=True,
                                capture_output=True, timeout=30, env=self.env | {
                                    "PATH": str(commands), "ENTRY": str(root), "FAIL_PROBE": failure,
                                    "WALDO_INSTALL_ROOT": str(install_root),
                                    "WALDO_PROFILE_FILE": str(profile_file),
                                    "WALDO_RUNTIME_DIR": str(runtime),
                                    "WALDO_SERVICE_FILE": str(service_file or install_root / "home/.config/systemd/user/waldod.service"),
                                    "WALDO_SOCKET_PATH": str(socket_parent / "controller.sock")})
        profile = json.loads(profile_file.read_text())
        return result, profile, install_root

    def test_rehearsal_refuses_unsuitable_directories_without_installing_service(self):
        for target in ("runtime", "socket", "profile"):
            with self.subTest(target=target):
                directory = self.root / ("existing-" + target)
                directory.mkdir(mode=0o755)
                (directory / "keep").write_text("operator-owned")
                result, profile, install_root = self.rehearsal_entry(
                    runtime=directory if target == "runtime" else None,
                    socket_parent=directory if target in ("socket", "profile") else None,
                    profile_file=directory / "environment.json" if target == "profile" else None)
                self.assertEqual(directory.stat().st_mode & 0o777, 0o755)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn(str(directory), result.stderr)
                self.assertFalse((install_root / "home/.config/systemd/user/waldod.service").exists())
                self.assertFalse(profile["installation"]["available"])
                self.assertIn("controller service installation", profile["installation"]["diagnostic"])
                shutil.rmtree(self.root / "entry")

    def test_rehearsal_adopts_private_directories_and_removes_only_created_ones(self):
        for target in ("runtime", "socket", "service"):
            with self.subTest(target=target):
                directory = self.root / ("existing-" + target)
                directory.mkdir(mode=0o700)
                (directory / "keep").write_text("operator-owned")
                result, profile, _ = self.rehearsal_entry(
                    runtime=directory if target == "runtime" else None,
                    socket_parent=directory if target in ("socket", "service") else None,
                    service_file=directory / "waldod.service" if target == "service" else None)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertTrue(profile["installation"]["available"])
                self.assertEqual(directory.stat().st_mode & 0o777, 0o700)
                changed = (self.root / "entry/mode-changes").read_text().splitlines()
                self.assertNotIn(str(directory), changed)
                self.assertEqual((directory / "keep").read_text(), "operator-owned")
                if target == "runtime":
                    self.assertFalse((directory / "waldo").exists())
                else:
                    self.assertFalse((self.root / "entry/runtime").exists())
                shutil.rmtree(self.root / "entry")

    def test_required_metadata_failure_records_unavailable_at_recording_stage(self):
        for probe in ("checksum", "claude", "git-status", "git-head"):
            with self.subTest(probe=probe):
                result, profile, _ = self.rehearsal_entry(failure=probe)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertFalse(profile["installation"]["available"])
                self.assertIn("verified environment recording", profile["installation"]["diagnostic"])
                self.assertNotIn("source_tree_clean", profile["installation"])
                shutil.rmtree(self.root / "entry")

    def test_paths_every_binding_must_agree_on_are_made_absolute_or_refused(self):
        relative_socket = self.steps("resolve_installation", WALDO_SOCKET_PATH="relative/controller.sock")
        self.assertNotEqual(relative_socket.returncode, 0)
        self.assertIn("WALDO_SOCKET_PATH must be an absolute path", relative_socket.stderr)

        other_unit = self.steps("resolve_installation",
                                WALDO_SERVICE_FILE=str(self.root / "units/other.service"))
        self.assertNotEqual(other_unit.returncode, 0)
        self.assertIn("waldod.service", other_unit.stderr)

        plugin_home = self.successful_steps('resolve_installation\nprintf %s "$plugin_home"',
                                            WALDO_PLUGIN_HOME="relative/plugin")
        self.assertTrue(Path(plugin_home).is_absolute(), plugin_home)

    def test_the_canonical_unit_follows_an_absolute_config_home(self):
        config_home = self.root / "config"
        service = self.successful_steps('resolve_installation\nprintf %s "$service_file"',
                                        XDG_CONFIG_HOME=str(config_home))
        self.assertEqual(Path(service), config_home / "systemd/user/waldod.service")

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

    def candidate_plugin(self, name="candidate"):
        candidate = self.root / name
        for file, content in {
            ".claude-plugin/plugin.json": '{"name":"waldo","version":"0.1.0"}',
            ".codex-plugin/plugin.json": '{"name":"waldo","version":"0.1.0+codex.20261010120000"}',
            ".mcp.json": '{"mcpServers":{"waldo":{"command":"/candidate/waldo-mcp"}}}',
            ".mcp.claude.json": '{"mcpServers":{"waldo":{"command":"/candidate/waldo-mcp"}}}',
            "skills/desktop-control/SKILL.md": "Candidate guidance.\n",
        }.items():
            path = candidate / file
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)
        return candidate

    def claude_summary(self, transcript, plugin_source="waldo@personal", candidate=None):
        """The summary the installer derives from a transcript, and whether it
        counts as proof."""
        output = self.successful_steps(
            f'summary="$(summarise_claude_turn {shlex.quote(str(transcript))} {shlex.quote(plugin_source)} '
            f'{shlex.quote(str(candidate or self.root / "candidate"))})"\n'
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

    def test_codex_marketplace_takes_configured_identity_only_when_created(self):
        marketplace = self.root / "marketplace.json"
        configured = dict(WALDO_MARKETPLACE=str(marketplace), WALDO_MARKETPLACE_NAME="workshop",
                          WALDO_MARKETPLACE_OWNER='Workshop "Owner"')
        self.successful_steps('resolve_installation\nwrite_codex_marketplace', **configured)
        manifest = json.loads(marketplace.read_text())
        self.assertEqual(manifest["name"], "workshop")
        self.assertEqual(manifest["owner"], {"name": 'Workshop "Owner"'})
        self.assertEqual(manifest["interface"], {"displayName": "Personal"})
        self.assertEqual(manifest["plugins"][0]["source"]["path"], "./.local/share/waldo/plugins/waldo")

        marketplace.write_text('{"name":"old","owner":{"name":"Old Owner","email":"old@example.org"},"plugins":[]}')
        output = self.successful_steps('resolve_installation\nwrite_codex_marketplace\n'
                                       'printf "%s\\n" "$codex_marketplace_name"', **configured)
        self.assertEqual(output.strip(), "old")
        manifest = json.loads(marketplace.read_text())
        self.assertEqual(manifest["name"], "old")
        self.assertEqual(manifest["owner"], {"name": "Old Owner", "email": "old@example.org"})

    def test_created_marketplace_defaults_to_waldo_owned_by_the_installing_user(self):
        marketplace = self.root / "marketplace.json"
        self.successful_steps('resolve_installation\nwrite_codex_marketplace',
                              WALDO_MARKETPLACE=str(marketplace))
        manifest = json.loads(marketplace.read_text())
        self.assertEqual(manifest["name"], "waldo")
        self.assertEqual(manifest["owner"], {"name": subprocess.run(
            ["id", "-un"], text=True, capture_output=True, check=True).stdout.strip()})

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
        self.assertEqual(manifest["name"], "old")
        self.assertEqual(manifest["owner"], {"name": "Old Owner"})
        self.assertEqual(manifest["metadata"], {"description": "kept"})
        self.assertEqual(manifest["plugins"], [other, {"name": "waldo", "source": "./waldo"}])

    def test_marketplace_writer_fails_without_touching_a_malformed_manifest(self):
        # The writers may run where errexit is suspended, so a failed merge
        # must report failure itself and leave the manifest as it was.
        marketplace = self.root / "plugins/.claude-plugin/marketplace.json"
        marketplace.parent.mkdir(parents=True)
        marketplace.write_text("{not json")
        result = self.steps(
            f'claude_marketplace={shlex.quote(str(marketplace))}\n'
            'marketplace_owner=Operator\nclaude_marketplace_name=waldo\n'
            f'plugin_home={shlex.quote(str(self.root / "plugins/waldo"))}\n'
            'if write_claude_marketplace; then echo wrote; else echo refused; fi')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "refused")
        self.assertEqual(marketplace.read_text(), "{not json")
        self.assertEqual(list(marketplace.parent.glob(".waldo-marketplace.*")), [])

    def test_claude_marketplace_is_created_beside_the_plugin_home(self):
        plugin_home = self.root / "elsewhere/waldo-candidate"
        self.successful_steps('resolve_installation\nwrite_claude_marketplace\n'
                              'printf "%s\\n" "$claude_marketplace_root"',
                              WALDO_PLUGIN_HOME=str(plugin_home))
        manifest = json.loads((self.root / "elsewhere/.claude-plugin/marketplace.json").read_text())
        self.assertEqual(manifest["name"], "waldo")
        self.assertEqual(manifest["plugins"], [{"name": "waldo", "source": "./waldo-candidate"}])

    def test_plugin_home_must_be_waldos_alone(self):
        home = Path(self.env["HOME"])
        unrelated = self.root / "unrelated"
        unrelated.mkdir()
        (unrelated / "keep.txt").write_text("not Waldo's")
        for label, destination in (("root", "/"), ("home", home), ("above home", home.parent),
                                   ("checkout", REPO), ("inside checkout", REPO / "plugins/waldo"),
                                   ("above checkout", REPO.parent), ("unrelated files", unrelated)):
            with self.subTest(destination=label):
                result = self.steps('resolve_installation\ninstall_plugin_copy',
                                    WALDO_PLUGIN_HOME=str(destination))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("WALDO_PLUGIN_HOME", result.stderr)
        self.assertEqual((unrelated / "keep.txt").read_text(), "not Waldo's")
        self.assertEqual(sorted(path.name for path in unrelated.iterdir()), ["keep.txt"])

    def test_plugin_copy_replaces_an_earlier_copy(self):
        plugin_home = self.root / "plugins/waldo"
        self.successful_steps('resolve_installation\ninstall_plugin_copy',
                              WALDO_PLUGIN_HOME=str(plugin_home))
        (plugin_home / "stale.txt").write_text("from an older copy")
        self.successful_steps('resolve_installation\ninstall_plugin_copy',
                              WALDO_PLUGIN_HOME=str(plugin_home))
        self.assertFalse((plugin_home / "stale.txt").exists())

    def test_canonical_plugin_copy_lives_in_the_data_directory_without_overrides(self):
        plugin_home = Path(self.env["HOME"]) / ".local/share/waldo/plugins/waldo"
        self.successful_steps('resolve_installation\ninstall_plugin_copy')
        for name in (".mcp.json", ".mcp.claude.json"):
            with self.subTest(manifest=name):
                server = json.loads((plugin_home / name).read_text())["mcpServers"]["waldo"]
                self.assertNotIn("env", server)

    def test_plugin_copy_names_the_resolved_binary_in_both_mcp_manifests(self):
        sources = {name: (REPO / "plugins/waldo" / name).read_bytes() for name in (".mcp.json", ".mcp.claude.json")}
        destination = self.root / 'cargo root " $place'
        install_root = self.root / "rehearsal"
        plugin_home = install_root / "home/.local/share/waldo/plugins/waldo"
        socket = str(self.root / "custom/controller.sock")
        self.successful_steps('resolve_installation\ninstall_plugin_copy',
                              WALDO_INSTALL_ROOT=str(install_root),
                              WALDO_CARGO_INSTALL_ROOT=str(destination),
                              WALDO_SOCKET_PATH=socket)
        for name, original in sources.items():
            with self.subTest(manifest=name):
                installed = json.loads((plugin_home / name).read_text())
                expected = json.loads(original)
                expected["mcpServers"]["waldo"]["command"] = str(destination / "bin/waldo-mcp")
                # The overrides the service receives travel with the server.
                expected["mcpServers"]["waldo"]["env"] = {
                    "WALDO_SOCKET_PATH": socket, "XDG_CONFIG_HOME": str(install_root / "home/.config")}
                self.assertEqual(installed, expected)
                self.assertEqual((REPO / "plugins/waldo" / name).read_bytes(), original)
        for client in ("claude", "codex"):
            version = json.loads((plugin_home / f".{client}-plugin/plugin.json").read_text())["version"]
            self.assertRegex(version, rf"^0\.1\.0\+{client}\.\d{{14}}$")
        self.assertTrue((plugin_home / "skills/desktop-control/SKILL.md").is_file())

    def test_claude_turn_proof_needs_an_observe_result_with_a_delivered_image(self):
        candidate = self.candidate_plugin()
        loaded = self.root / "cached-candidate"
        shutil.copytree(candidate, loaded)
        events = [init_event("waldo@personal", str(loaded)),
                  tool_use("q1", QUERY_TOOL),
                  tool_result("q1", [{"type": "text", "text": "{\"type\":\"queried\"}"}]),
                  tool_use("o1", OBSERVE_TOOL),
                  tool_result("o1", [image_block(), {"type": "text", "text": "{}"}], observation(True)),
                  result_event("MCP_IMAGE_PROOF_OK")]
        summary, proves = self.claude_summary(self.write_transcript("proof.jsonl", events))
        self.assertTrue(proves, summary)
        self.assertEqual(summary, {"plugin_source": "waldo@personal", "plugins_loaded": 1,
                                   "plugin_paths": [str(loaded)], "plugin_versions": ["0.1.0"],
                                   "candidate_matches": True, "servers_connected": 1,
                                   "observe_calls": 1, "image_results": 1, "mutating_calls": 0,
                                   "completion_markers": 1})

    def test_claude_turn_proof_refuses_a_reply_without_the_tool_result(self):
        # The model's words are never the proof: a turn that claims success
        # with no Observe tool result, or with one carrying no image, or with
        # the plugin loaded from somewhere else, is refused.
        candidate = self.candidate_plugin()
        loaded = init_event("waldo@personal", str(candidate))
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

    def codex_proof_steps(self, candidate, cache_home, auth_source=None, probe=""):
        proof = self.root / "proof"
        (proof / "cwd").mkdir(parents=True, exist_ok=True)
        transcript = self.write_transcript("codex-events.jsonl", [
            {"type": "item.completed", "item": {"type": "mcp_tool_call", "server": "waldo",
                "tool": "observe", "result": {"content": [
                    {"type": "image", "mimeType": "image/png", "data": "iVBORw0KGgo="}],
                    "structuredContent": observation(True)}}},
            {"type": "item.completed", "item": {"type": "agent_message", "text": "MCP_IMAGE_PROOF_OK"}},
        ])
        script = (
            'resolve_installation\n'
            'codex() { echo fixture-version; }\n'
            + ('codex() { return 43; }\n' if probe == 'version' else '')
            + ('sha256sum() { return 44; }\n' if probe == 'checksum' else '')
            + f'proof_dir={shlex.quote(str(proof))}\n'
            'run_codex() { if [[ "$2" == plugin ]]; then return 0; fi; '
            f'cat {shlex.quote(str(transcript))}; }}\n'
            'if codex_registration_and_proof; then verdict=proved; else verdict=refused; fi\n'
            'printf "%s\n%s\n%s\n" "$verdict" "$installed_codex_manifest" "$codex_diagnostic"'
        )
        return self.steps(script, WALDO_INSTALL_ROOT=str(self.root / "install"),
                          WALDO_PLUGIN_HOME=str(candidate), WALDO_CODEX_HOME=str(cache_home),
                          WALDO_CODEX_AUTH_SOURCE=str(auth_source or self.root / "auth.json"))

    def codex_cache(self, candidate, label="codex"):
        cache_home = self.root / label
        cache_home.mkdir()
        version = json.loads((candidate / ".codex-plugin/plugin.json").read_text())["version"]
        selected = cache_home / "plugins/cache/waldo/waldo" / version
        shutil.copytree(candidate, selected)
        (self.root / "auth.json").write_text('{"login":"fixture"}')
        return cache_home, selected

    def test_codex_metadata_failure_is_a_best_effort_refusal(self):
        candidate = self.candidate_plugin()
        for probe in ("version", "checksum"):
            with self.subTest(probe=probe):
                cache_home, _ = self.codex_cache(candidate, "codex-" + probe)
                result = self.codex_proof_steps(candidate, cache_home, probe=probe)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.splitlines()[0], "refused", result.stdout)
                self.assertIn("could not", result.stdout.splitlines()[2])

    def test_relative_codex_auth_source_links_to_the_validated_file(self):
        candidate = self.candidate_plugin()
        cache_home, _ = self.codex_cache(candidate)
        # Relative to the installer invocation, not to the redirected home.
        source = os.path.relpath(self.root / "auth.json", REPO)
        result = self.codex_proof_steps(candidate, cache_home, source)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines()[0], "proved", result.stdout)
        link = cache_home / "auth.json"
        self.assertTrue(link.is_file(), f"validated login became a dangling link: {link.readlink()}")
        self.assertTrue(link.readlink().is_absolute(), link.readlink())
        self.assertEqual(link.read_bytes(), (self.root / "auth.json").read_bytes())

    def test_codex_cache_selects_the_candidate_even_when_it_sorts_first(self):
        candidate = self.candidate_plugin()
        cache_home, selected = self.codex_cache(candidate)
        stale = selected.parent / "0.1.0"
        shutil.copytree(candidate, stale)
        (stale / ".codex-plugin/plugin.json").write_text('{"name":"waldo","version":"0.1.0"}')
        result = self.codex_proof_steps(candidate, cache_home)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines()[0], "proved", result.stdout)
        self.assertEqual(Path(result.stdout.splitlines()[1]), selected / ".codex-plugin/plugin.json")

    def test_codex_cache_refuses_missing_or_altered_candidate(self):
        for change in ("missing", "altered"):
            with self.subTest(change=change):
                candidate = self.candidate_plugin()
                cache_home, selected = self.codex_cache(candidate, f"codex-{change}")
                manifest = selected / ".codex-plugin/plugin.json"
                if change == "missing":
                    manifest.unlink()
                else:
                    manifest.write_text('{"name":"waldo","version":"0.1.0"}')
                result = self.codex_proof_steps(candidate, cache_home)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.splitlines()[0], "refused", result.stdout)
                self.assertTrue(result.stdout.splitlines()[2], result.stdout)
                shutil.rmtree(cache_home)

    def test_claude_proof_refuses_stale_version_or_candidate_bytes(self):
        candidate = self.candidate_plugin()
        for change in ("version", ".claude-plugin/plugin.json", ".mcp.claude.json", ".mcp.json",
                       "skills/desktop-control/SKILL.md", "missing path"):
            with self.subTest(change=change):
                loaded = self.root / ("loaded-" + change.replace("/", "-"))
                shutil.copytree(candidate, loaded)
                if change not in ("version", "missing path"):
                    (loaded / change).write_text("stale bytes")
                events = [init_event("waldo@personal", str(loaded),
                                     version="0.0.1" if change == "version" else "0.1.0"),
                          tool_use("o1", OBSERVE_TOOL),
                          tool_result("o1", [image_block()], observation(True)),
                          result_event("MCP_IMAGE_PROOF_OK")]
                if change == "missing path":
                    shutil.rmtree(loaded)
                summary, proves = self.claude_summary(self.write_transcript("stale.jsonl", events))
                self.assertFalse(proves, summary)
                if loaded.exists():
                    shutil.rmtree(loaded)

    def test_corrupt_claude_marketplace_is_refused_during_resolution(self):
        plugin_home = self.root / "plugins/waldo"
        manifest = plugin_home.parent / ".claude-plugin/marketplace.json"
        manifest.parent.mkdir(parents=True)
        manifest.write_text("{not json")
        result = self.steps('if resolve_installation; then echo resolved; else echo refused; fi',
                            WALDO_PLUGIN_HOME=str(plugin_home))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "refused")
        self.assertIn("Could not read marketplace manifest", result.stderr)
        self.assertIn(str(manifest), result.stderr)
        self.assertEqual(manifest.read_text(), "{not json")

    def test_corrupt_codex_marketplace_fails_only_codex_registration(self):
        candidate = self.candidate_plugin()
        cache_home, _ = self.codex_cache(candidate)
        manifest = self.root / "install/home/.agents/plugins/marketplace.json"
        manifest.parent.mkdir(parents=True)
        manifest.write_text("{not json")
        result = self.codex_proof_steps(candidate, cache_home)
        # resolve_installation runs first in the script; reaching the Codex
        # verdict means resolution, and so the Claude installation, went on.
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines()[0], "refused", result.stdout)
        self.assertIn(f"could not read or write the Codex marketplace {manifest}", result.stdout)
        self.assertEqual(manifest.read_text(), "{not json")

    def test_interrupted_marketplace_write_leaves_original_bytes(self):
        manifest = self.root / "marketplace.json"
        original = '{"name":"shared","plugins":[{"name":"sentinel","source":"./sentinel"}]}\n'
        manifest.write_text(original)
        stubs = self.root / "write-failure"
        stubs.mkdir()
        # Interrupt a destination write through the OS file-size limit. A
        # rename is interrupted before it starts; no stub invents damage.
        install = stubs / "install"
        install.write_text(
            '#!/usr/bin/env python3\nimport os, resource, sys\n'
            f'if sys.argv[-1] == {str(manifest)!r}:\n'
            '    resource.setrlimit(resource.RLIMIT_FSIZE, (10, 10))\n'
            f'os.execv({shutil.which("install")!r}, ["install", *sys.argv[1:]])\n')
        install.chmod(0o755)
        move = stubs / "mv"
        move.write_text('#!/bin/sh\nkill -TERM "$PPID"\nexit 1\n')
        move.chmod(0o755)
        result = self.steps('resolve_installation\nwrite_codex_marketplace',
                            WALDO_MARKETPLACE=str(manifest), PATH=f"{stubs}:{self.env['PATH']}")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(manifest.read_text(), original)
        temporary = list(manifest.parent.glob(".waldo-marketplace.*"))
        self.assertEqual(len(temporary), 1)
        merged = json.loads(temporary[0].read_text())
        self.assertEqual(merged["plugins"][0], {"name": "sentinel", "source": "./sentinel"})
        self.assertEqual(merged["plugins"][1]["name"], "waldo")

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
