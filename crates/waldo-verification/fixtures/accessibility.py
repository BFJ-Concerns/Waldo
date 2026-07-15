#!/usr/bin/env python3
"""Disposable GTK accessibility target with an application-level state oracle."""

import argparse
import json
import os
import time

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import GLib, Gtk  # noqa: E402


class IdentifiedSpin(Gtk.SpinButton):
    def __init__(self):
        adjustment = Gtk.Adjustment(
            value=0,
            lower=0,
            upper=100,
            step_increment=1,
            page_increment=10,
            page_size=0,
        )
        super().__init__(adjustment=adjustment, climb_rate=0, digits=0)

    def do_get_accessible_id(self):
        return "waldo-e2e016-set-value"


def arguments():
    parser = argparse.ArgumentParser()
    parser.add_argument("--control", required=True)
    parser.add_argument("--state", required=True)
    parser.add_argument("--seed", required=True, type=int)
    return parser.parse_args()


class AccessibilityFixture(Gtk.Application):
    def __init__(self, options):
        super().__init__(application_id="com.bfj.WaldoAccessibilityOracle")
        self.options = options
        self.title = f"Waldo Accessibility Oracle {options.seed:08x}"
        self.window = None
        self.duplicate = None
        self.container = None
        self.invoke_count = 0
        self.entry_generation = 0
        self.spin_generation = 0
        self.control_sequence = 0
        self.button = None
        self.entry = None
        self.spin = None
        self.selection = None

    def do_activate(self):
        self.window = Gtk.ApplicationWindow(application=self)
        self.window.set_title(self.title)
        self.window.set_default_size(720, 520)
        self.container = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=16)
        self.container.set_margin_top(24)
        self.container.set_margin_bottom(24)
        self.container.set_margin_start(24)
        self.container.set_margin_end(24)
        self.window.set_child(self.container)

        self.button = Gtk.Button(label="Invoke accessibility action")
        self.button.connect("clicked", self.invoked)
        self.container.append(self.button)

        self.install_entry()

        self.install_spin()

        self.selection = Gtk.ListBox()
        self.selection.set_selection_mode(Gtk.SelectionMode.SINGLE)
        for name in ("Alpha selection", "Beta selection", "Gamma selection"):
            self.selection.append(Gtk.Label(label=name, xalign=0))
        self.selection.connect("selected-rows-changed", self.state_changed)
        self.container.append(self.selection)

        self.window.present()
        self.button.grab_focus()
        GLib.timeout_add(50, self.poll_control)
        GLib.idle_add(self.write_state)

    def install_entry(self):
        self.entry = Gtk.Entry()
        self.entry.set_tooltip_text("Focus and SetText target")
        self.entry.connect("changed", self.state_changed)
        self.entry.connect("notify::has-focus", self.state_changed)
        self.container.insert_child_after(self.entry, self.button)

    def install_spin(self):
        self.spin = IdentifiedSpin()
        self.spin.set_tooltip_text("SetValue target")
        self.spin.connect("value-changed", self.state_changed)
        self.container.insert_child_after(self.spin, self.entry)

    def invoked(self, _button):
        self.invoke_count += 1
        self.write_state()

    def state_changed(self, *_args):
        GLib.idle_add(self.write_state)

    def poll_control(self):
        try:
            with open(self.options.control, "r", encoding="utf-8") as handle:
                command = json.load(handle)
        except (FileNotFoundError, json.JSONDecodeError):
            return GLib.SOURCE_CONTINUE
        sequence = int(command.get("sequence", 0))
        if sequence <= self.control_sequence:
            return GLib.SOURCE_CONTINUE
        self.control_sequence = sequence
        action = command.get("action")
        if action == "recreate_entry":
            self.container.remove(self.entry)
            self.entry_generation += 1
            self.install_entry()
        elif action == "refresh_value":
            self.container.remove(self.spin)
            self.spin_generation += 1
            self.install_spin()
        elif action == "open_duplicate":
            self.open_duplicate()
        elif action == "close_duplicate" and self.duplicate is not None:
            self.duplicate.close()
            self.duplicate = None
        self.write_state()
        return GLib.SOURCE_CONTINUE

    def open_duplicate(self):
        if self.duplicate is not None:
            return
        self.duplicate = Gtk.ApplicationWindow(application=self)
        self.duplicate.set_title(self.title)
        self.duplicate.set_default_size(720, 520)
        self.duplicate.set_child(Gtk.Label(label="Deliberate same-title ambiguity"))
        self.duplicate.present()

    def selected_label(self):
        row = self.selection.get_selected_row()
        if row is None:
            return None
        child = row.get_child()
        return child.get_text() if isinstance(child, Gtk.Label) else None

    def write_state(self):
        state = {
            "seed": self.options.seed,
            "title": self.title,
            "recorded_at_unix_ns": time.time_ns(),
            "invoke_count": self.invoke_count,
            "entry_text": self.entry.get_text(),
            "entry_focused": self.entry.has_focus(),
            "entry_generation": self.entry_generation,
            "spin_value": self.spin.get_value(),
            "spin_generation": self.spin_generation,
            "selected_label": self.selected_label(),
            "duplicate_open": self.duplicate is not None,
            "control_sequence": self.control_sequence,
        }
        temporary = self.options.state + ".tmp"
        with open(temporary, "w", encoding="utf-8") as handle:
            json.dump(state, handle, indent=2)
            handle.write("\n")
        os.replace(temporary, self.options.state)
        return GLib.SOURCE_REMOVE


if __name__ == "__main__":
    raise SystemExit(AccessibilityFixture(arguments()).run())
