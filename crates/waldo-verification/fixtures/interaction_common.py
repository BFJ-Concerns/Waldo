"""Shared state and event contract for deterministic interaction fixtures."""

import hashlib
import json
import os
import time


SCHEMA_VERSION = 1
FIXTURE_VERSION = "1"
CAPABILITIES = {
    "pointer": True,
    "keyboard": True,
    "text": True,
    "scroll": True,
    "drag": True,
    "delayed_state": True,
    "final_consumer_log": True,
}


def sha256_text(value):
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def descriptor(toolkit, display_route, seed, title):
    return {
        "schema_version": SCHEMA_VERSION,
        "fixture_version": FIXTURE_VERSION,
        "toolkit": toolkit,
        "display_route": display_route,
        "seed": seed,
        "title": title,
        "capabilities": CAPABILITIES,
    }


class InteractionOracle:
    def __init__(self, *, toolkit, display_route, seed, title, state, events, control, runtime_backend="pending"):
        self.descriptor = descriptor(toolkit, display_route, seed, title)
        self.runtime_backend = runtime_backend
        self.state_path = state
        self.events_path = events
        self.control_path = control
        self.sequence = 0
        self.control_sequence = 0
        self.ready = False
        self.phase = "starting"
        self.focused_control = None
        self.text_length = 0
        self.text_sha256 = None
        self.selection = None
        self.counters = {
            "pointer_moves": 0,
            "pointer_presses": 0,
            "pointer_releases": 0,
            "clicks": 0,
            "entry_clicks": 0,
            "drags": 0,
            "horizontal_scrolls": 0,
            "vertical_scrolls": 0,
            "key_presses": 0,
            "key_releases": 0,
            "text_changes": 0,
            "delayed_completions": 0,
        }
        os.makedirs(os.path.dirname(os.path.abspath(state)), mode=0o700, exist_ok=True)
        self._touch_private(events)

    def _touch_private(self, path):
        descriptor_fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
        os.close(descriptor_fd)
        os.chmod(path, 0o600)

    def write_state(self):
        value = {
            "descriptor": self.descriptor,
            "runtime_backend": self.runtime_backend,
            "recorded_at_unix_ns": time.time_ns(),
            "sequence": self.sequence,
            "ready": self.ready,
            "phase": self.phase,
            "focused_control": self.focused_control,
            "text_length": self.text_length,
            "text_sha256": self.text_sha256,
            "selection": self.selection,
            "counters": self.counters,
        }
        temporary = self.state_path + ".tmp"
        descriptor_fd = os.open(
            temporary,
            os.O_WRONLY | os.O_CREAT | os.O_TRUNC,
            0o600,
        )
        with os.fdopen(descriptor_fd, "w", encoding="utf-8") as handle:
            json.dump(value, handle, indent=2)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, self.state_path)
        os.chmod(self.state_path, 0o600)

    def record(self, kind, control, **values):
        self.sequence += 1
        event = {
            "schema_version": SCHEMA_VERSION,
            "fixture_version": FIXTURE_VERSION,
            "toolkit": self.descriptor["toolkit"],
            "display_route": self.descriptor["display_route"],
            "seed": self.descriptor["seed"],
            "sequence": self.sequence,
            "recorded_at_unix_ns": time.time_ns(),
            "kind": kind,
            "control": control,
            "x": values.get("x"),
            "y": values.get("y"),
            "button": values.get("button"),
            "horizontal_delta": values.get("horizontal_delta"),
            "vertical_delta": values.get("vertical_delta"),
            "physical_key": values.get("physical_key"),
            "logical_key_sha256": values.get("logical_key_sha256"),
            "modifiers": sorted(values.get("modifiers", [])),
            "text_length": values.get("text_length"),
            "text_sha256": values.get("text_sha256"),
        }
        with open(self.events_path, "a", encoding="utf-8") as handle:
            handle.write(json.dumps(event, separators=(",", ":")) + "\n")
            handle.flush()
        self.write_state()

    def count(self, name, amount=1):
        self.counters[name] += amount

    def update_text(self, value):
        self.text_length = len(value)
        self.text_sha256 = sha256_text(value) if value else None
        self.count("text_changes")
        self.record(
            "text_changed",
            "text-entry",
            text_length=self.text_length,
            text_sha256=self.text_sha256,
        )

    def record_key(self, kind, physical_key, logical_key, modifiers):
        counter = "key_presses" if kind == "key_pressed" else "key_releases"
        self.count(counter)
        self.record(
            kind,
            "text-entry",
            physical_key=physical_key,
            logical_key_sha256=sha256_text(logical_key),
            modifiers=modifiers,
        )

    def set_ready(self):
        self.ready = True
        self.phase = "ready"
        self.write_state()

    def complete_delayed_state(self):
        self.phase = "complete"
        self.count("delayed_completions")
        self.record("delayed_state_completed", "delayed-action")

    def read_control(self):
        try:
            with open(self.control_path, "r", encoding="utf-8") as handle:
                command = json.load(handle)
        except (FileNotFoundError, json.JSONDecodeError):
            return None
        sequence = int(command.get("sequence", 0))
        if sequence <= self.control_sequence:
            return None
        self.control_sequence = sequence
        return command

    def reset(self):
        self.phase = "ready"
        self.focused_control = None
        self.text_length = 0
        self.text_sha256 = None
        self.selection = None
        for name in self.counters:
            self.counters[name] = 0
        self.record("fixture_reset", "fixture")


def probe_json(toolkit, display_route, seed, title):
    return json.dumps(descriptor(toolkit, display_route, seed, title), indent=2) + "\n"
