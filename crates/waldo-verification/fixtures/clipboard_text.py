#!/usr/bin/env python3
"""Disposable two-window clipboard source and masked final-consumer oracle."""

import argparse
import hashlib
import json
import os
import secrets

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Gdk, GLib, Gtk  # noqa: E402


def arguments():
    parser = argparse.ArgumentParser()
    parser.add_argument("--probe-api", action="store_true")
    parser.add_argument("--state")
    parser.add_argument("--control")
    parser.add_argument("--secret-handoff")
    parser.add_argument("--seed")
    options = parser.parse_args()
    if not options.probe_api:
        missing = [
            name
            for name in ("state", "control", "secret_handoff", "seed")
            if getattr(options, name) is None
        ]
        if missing:
            parser.error("runtime mode requires: " + ", ".join(missing))
    return options


def probe_api():
    content = Gdk.ContentProvider.new_for_bytes(
        "text/plain;charset=utf-8",
        GLib.Bytes.new(b"waldo-clipboard-api-probe"),
    )
    if not isinstance(content, Gdk.ContentProvider):
        return 1
    entry = Gtk.Entry()
    entry.set_text("probe")
    entry.set_text("")
    return 0


class ClipboardFixture(Gtk.Application):
    def __init__(self, options):
        super().__init__(application_id="com.bfj.WaldoClipboardOracle")
        self.options = options
        self.secret = secrets.token_urlsafe(36)
        self.secret_digest = digest(self.secret)
        self.expected_digest = self.secret_digest
        self.scenario = "accept"
        self.control_sequence = 0
        self.copy_count = 0
        self.key_chords = 0
        self.insert_attempts = 0
        self.submission_count = 0
        self.accepted = False
        self.clearing = False
        self.entry = None
        self.digest_status = None
        self.submission_status = None
        self.write_secret_handoff()

    def write_secret_handoff(self):
        descriptor = os.open(
            self.options.secret_handoff,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL,
            0o600,
        )
        with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
            handle.write(self.secret)
            handle.flush()
            os.fsync(handle.fileno())

    def do_activate(self):
        source = Gtk.ApplicationWindow(application=self)
        source.set_title(f"Waldo Clipboard Source {self.options.seed}")
        source.set_default_size(520, 260)
        source_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=18)
        source_box.set_margin_top(40)
        source_box.set_margin_bottom(40)
        source_box.set_margin_start(40)
        source_box.set_margin_end(40)
        source_label = Gtk.Label(label="Generated value is retained only inside this fixture")
        copy_button = Gtk.Button(label="Copy generated value")
        copy_button.connect("clicked", self.copy_secret)
        source_box.append(source_label)
        source_box.append(copy_button)
        source.set_child(source_box)

        destination = Gtk.ApplicationWindow(application=self)
        destination.set_title(f"Waldo Clipboard Destination {self.options.seed}")
        destination.set_default_size(620, 340)
        destination_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=14)
        destination_box.set_margin_top(36)
        destination_box.set_margin_bottom(36)
        destination_box.set_margin_start(36)
        destination_box.set_margin_end(36)
        self.entry = Gtk.Entry()
        self.entry.set_visibility(False)
        self.entry.set_placeholder_text("Masked destination")
        self.entry.connect("changed", self.destination_changed)
        key_controller = Gtk.EventControllerKey()
        key_controller.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        key_controller.connect("key-pressed", self.key_pressed)
        self.entry.add_controller(key_controller)
        self.digest_status = Gtk.CheckButton(label="Destination digest matched")
        self.digest_status.set_sensitive(False)
        self.submission_status = Gtk.CheckButton(label="Submission accepted")
        self.submission_status.set_sensitive(False)
        submit = Gtk.Button(label="Submit masked destination")
        submit.connect("clicked", self.submit)
        destination_box.append(self.entry)
        destination_box.append(self.digest_status)
        destination_box.append(self.submission_status)
        destination_box.append(submit)
        destination.set_child(destination_box)

        source.present()
        destination.present()
        self.entry.grab_focus()
        GLib.timeout_add(25, self.read_control)
        self.write_state()

    def copy_secret(self, _button):
        content = Gdk.ContentProvider.new_for_bytes(
            "text/plain;charset=utf-8",
            GLib.Bytes.new(self.secret.encode("utf-8")),
        )
        if not Gdk.Display.get_default().get_clipboard().set_content(content):
            raise RuntimeError("GTK refused the generated clipboard content provider")
        self.copy_count += 1
        self.write_state()

    def key_pressed(self, _controller, keyval, _keycode, modifiers):
        control = bool(modifiers & Gdk.ModifierType.CONTROL_MASK)
        if control and keyval in (Gdk.KEY_v, Gdk.KEY_V):
            self.key_chords += 1
            self.write_state()
            return self.scenario == "blocked"
        return False

    def destination_changed(self, _entry):
        if self.clearing:
            return
        self.insert_attempts += 1
        if self.scenario == "unchanged" and self.entry.get_text():
            self.clearing = True
            self.entry.set_text("")
            self.clearing = False
        self.refresh_status()

    def refresh_status(self):
        matches = digest(self.entry.get_text()) == self.expected_digest
        self.digest_status.set_active(matches)
        self.write_state()

    def submit(self, _button):
        self.submission_count += 1
        self.accepted = digest(self.entry.get_text()) == self.expected_digest
        self.submission_status.set_active(self.accepted)
        self.clearing = True
        self.entry.set_text("")
        self.clearing = False
        self.write_state()

    def read_control(self):
        try:
            with open(self.options.control, "r", encoding="utf-8") as handle:
                control = json.load(handle)
        except (FileNotFoundError, json.JSONDecodeError):
            return GLib.SOURCE_CONTINUE
        sequence = int(control.get("sequence", 0))
        if sequence <= self.control_sequence:
            return GLib.SOURCE_CONTINUE
        self.control_sequence = sequence
        self.scenario = control["scenario"]
        self.entry.set_editable(self.scenario != "rejected")
        self.expected_digest = control["expected_digest"]
        self.accepted = False
        self.clearing = True
        self.entry.set_text("")
        self.clearing = False
        self.digest_status.set_active(False)
        self.submission_status.set_active(False)
        self.write_state()
        return GLib.SOURCE_CONTINUE

    def write_state(self):
        if self.entry is None:
            return
        text = self.entry.get_text()
        state = {
            "seed": self.options.seed,
            "source_title": f"Waldo Clipboard Source {self.options.seed}",
            "destination_title": f"Waldo Clipboard Destination {self.options.seed}",
            "source_digest": self.secret_digest,
            "scenario": self.scenario,
            "control_sequence": self.control_sequence,
            "copy_count": self.copy_count,
            "key_chords": self.key_chords,
            "insert_attempts": self.insert_attempts,
            "destination_mask": "•" * len(text),
            "destination_digest": digest(text),
            "digest_matches": digest(text) == self.expected_digest,
            "submission_count": self.submission_count,
            "accepted": self.accepted,
        }
        temporary = self.options.state + ".tmp"
        with open(temporary, "w", encoding="utf-8") as handle:
            json.dump(state, handle, indent=2)
            handle.write("\n")
        os.replace(temporary, self.options.state)


def digest(value):
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


if __name__ == "__main__":
    options = arguments()
    raise SystemExit(probe_api() if options.probe_api else ClipboardFixture(options).run())
