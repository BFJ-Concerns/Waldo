#!/usr/bin/env python3
"""Check capture orientation and wx button input on a disposable nested Niri."""

import argparse
import base64
import io
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time


def wait_for(predicate, description):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.05)
    raise RuntimeError(f"timed out waiting for {description}")


def verify(root, binaries, processes):
    from PIL import Image, ImageChops

    runtime = root / "runtime"
    runtime.mkdir(mode=0o700)
    config_home = root / "config"
    config_home.mkdir(mode=0o700)
    # The nested session must not reach the operator's desktop or read the
    # operator's Waldo settings: every Waldo override is dropped, the
    # configuration home is private and empty, and the socket is named.
    env = {key: value for key, value in os.environ.items() if not key.startswith("WALDO_")}
    for key in ("WAYLAND_DISPLAY", "WAYLAND_SOCKET", "NIRI_SOCKET"):
        env.pop(key, None)
    socket = runtime / "waldo/controller.sock"
    env.update(XDG_RUNTIME_DIR=str(runtime), XDG_CONFIG_HOME=str(config_home),
               WALDO_SOCKET_PATH=str(socket), WINIT_UNIX_BACKEND="x11", GDK_BACKEND="wayland")

    def launch(name, command, **kwargs):
        with (root / f"{name}.log").open("w") as log:
            process = subprocess.Popen(command, env=env, stdout=log, stderr=subprocess.STDOUT,
                                       start_new_session=True, **kwargs)
        processes.append(process)
        return process

    display_file = root / "display"
    with display_file.open("w") as display:
        launch("xvfb", ["Xvfb", "-displayfd", str(display.fileno()), "-screen", "0",
                        "1280x800x24", "-nolisten", "tcp"], pass_fds=[display.fileno()])
        env["DISPLAY"] = ":" + wait_for(lambda: display_file.read_text().strip(), "Xvfb")
    config = root / "niri.kdl"
    config.write_text("hotkey-overlay { skip-at-startup; }\nprefer-no-csd\nanimations { off; }\n")
    launch("niri", ["niri", "--config", str(config)])
    env["NIRI_SOCKET"] = str(wait_for(lambda: next(runtime.glob("niri.*.sock"), None), "Niri IPC"))
    env["WAYLAND_DISPLAY"] = wait_for(
        lambda: next((p.name for p in runtime.glob("wayland-*") if p.is_socket()), None), "Wayland")
    outputs = json.loads(subprocess.check_output(["niri", "msg", "-j", "outputs"], env=env))
    output = next(iter(outputs.values()))
    if output["logical"]["transform"] != "Flipped180":
        raise RuntimeError("this regression requires the nested backend's Flipped180 output")
    output_name = output["name"]
    launch("waldod", [str(binaries / "waldod")])
    wait_for(lambda: socket.is_socket(), "Waldo controller")
    fixture = Path(__file__).resolve().parents[1] / "crates/waldo-verification/fixtures/wx_button.py"
    events = root / "events.jsonl"
    launch("fixture", ["python3", str(fixture), "--events", str(events)])

    def reference(region=None):
        command = ["grim", "-c"] + (["-g", region] if region else ["-o", output_name])
        return Image.open(io.BytesIO(subprocess.check_output(command + ["-"], env=env))).convert("RGB")

    def blue_bounds():
        image = reference()
        red, green, blue = image.split()
        mask = ImageChops.multiply(red.point(lambda x: 255 if x == 0 else 0),
                                  green.point(lambda x: 255 if x == 0 else 0))
        mask = ImageChops.multiply(mask, blue.point(lambda x: 255 if x == 255 else 0))
        return mask.getbbox()

    bounds = wait_for(blue_bounds, "the wx button")
    source = {"type": "output", "output_id": output_name}
    observe_path = root / "observe.json"

    def observe(selector):
        observe_path.write_text(json.dumps({"source": selector, "include_accessibility": False}))
        result = json.loads(subprocess.check_output(
            [str(binaries / "waldo"), "observe", str(observe_path)], env=env))
        return result["observation"]

    def compare(observation, expected):
        actual = Image.open(io.BytesIO(base64.b64decode(observation["image"]["data_base64"]))).convert("RGB")
        if actual.size != expected.size or ImageChops.difference(actual, expected).getbbox():
            raise AssertionError("Waldo capture differs from Grim's desktop-oriented pixels")
        if observation["coordinates"]["model_image_pixels"] != dict(zip(("width", "height"), actual.size)):
            raise AssertionError("delivered image dimensions disagree with coordinate metadata")

    compare(observe(source), reference())
    region = {"type": "region", "output_id": output_name,
              "region": {"origin": {"x": 70, "y": 80}, "width": 320, "height": 180}}
    compare(observe(region), reference("70,80 320x180"))
    observation = observe(source)
    point = {"x": (bounds[0] + bounds[2]) / 2, "y": (bounds[1] + bounds[3]) / 2}
    # The independently located pixel centre and the delivered image coincide.
    # Scale through the public mapping, even on this scale-one fixture.
    mapping = observation["coordinates"]
    for axis, size in (("x", "width"), ("y", "height")):
        point[axis] = mapping["logical_bounds"]["origin"][axis] + point[axis] * mapping["logical_bounds"][size] / mapping["model_image_pixels"][size]
    action_path = root / "act.json"
    action_path.write_text(json.dumps({"observation_id": observation["id"], "actions": [
        {"type": "pointer_click", "position": point, "button": "primary", "count": 1}], "verification": []}))
    result = json.loads(subprocess.check_output([
        str(binaries / "waldo"), "act", str(action_path), "--observe", str(observe_path)], env=env))
    if result["result"]["receipts"][0]["status"] != "executed":
        raise AssertionError("click was not dispatched")
    wait_for(lambda: events.exists() and '"activated"' in events.read_text(), "wx activation")
    records = [json.loads(line) for line in events.read_text().splitlines()]
    if [item["kind"] for item in records] != ["down", "up", "activated"]:
        raise AssertionError(f"unexpected application event sequence: {records}")
    if records[-1]["position"] != [110, 40]:
        raise AssertionError(f"click missed the button centre: {records[-1]}")
    print(json.dumps({"transform": output["logical"]["transform"], "capture": "matches grim",
                      "region": "matches grim", "button_events": records}))


def parse_options(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--scratch-parent", type=Path, default=Path(tempfile.gettempdir()))
    return parser, parser.parse_args(argv)


def main():
    parser, options = parse_options()
    for command in ("Xvfb", "niri", "grim", "python3"):
        if not shutil.which(command):
            parser.error(f"missing prerequisite: {command}")
    binaries = options.bin_dir.resolve()
    for name in ("waldo", "waldod"):
        if not os.access(binaries / name, os.X_OK):
            parser.error(f"missing executable: {binaries / name}")
    if importlib.util.find_spec("wx") is None:
        parser.error("missing prerequisite: wxPython")
    processes = []
    with tempfile.TemporaryDirectory(prefix="waldo-transform-", dir=options.scratch_parent) as directory:
        try:
            verify(Path(directory), binaries, processes)
        finally:
            # These groups contain only processes launched by this invocation.
            for process in reversed(processes):
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()


if __name__ == "__main__":
    main()
