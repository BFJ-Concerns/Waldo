#!/usr/bin/env python3
"""Deterministic GTK interaction fixture with a final-consumer oracle."""

import argparse

import gi

gi.require_version("Gdk", "4.0")
gi.require_version("Gtk", "4.0")
from gi.repository import Gdk, GLib, Gtk  # noqa: E402

from interaction_common import InteractionOracle, probe_json  # noqa: E402


def arguments():
    parser = argparse.ArgumentParser()
    parser.add_argument("--state")
    parser.add_argument("--events")
    parser.add_argument("--control")
    parser.add_argument("--seed", required=True, type=int)
    parser.add_argument("--probe-api", action="store_true")
    return parser.parse_args()


class GtkInteractionFixture(Gtk.Application):
    def __init__(self, options):
        super().__init__(application_id="com.bfj.WaldoGtkInteractionOracle")
        self.options = options
        self.title = f"Waldo GTK Interaction Oracle {options.seed:08x}"
        self.oracle = InteractionOracle(
            toolkit="gtk",
            display_route="native_wayland",
            seed=options.seed,
            title=self.title,
            state=options.state,
            events=options.events,
            control=options.control,
        )
        self.pointer_origin = None
        self.dragged = False

    def do_activate(self):
        display = Gdk.Display.get_default()
        self.oracle.runtime_backend = type(display).__name__ if display is not None else "unknown"
        window = Gtk.ApplicationWindow(application=self)
        window.set_title(self.title)
        window.set_default_size(920, 700)
        content = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        content.set_margin_top(18)
        content.set_margin_bottom(18)
        content.set_margin_start(18)
        content.set_margin_end(18)

        marker = Gtk.Label(label=f"GTK-{self.options.seed:08x}")
        marker.add_css_class("title-1")
        content.append(marker)

        surface = Gtk.DrawingArea()
        surface.set_content_width(760)
        surface.set_content_height(340)
        surface.set_hexpand(True)
        surface.set_vexpand(True)
        surface.set_draw_func(self.draw_surface)
        motion = Gtk.EventControllerMotion()
        motion.connect("motion", self.pointer_move)
        surface.add_controller(motion)
        click = Gtk.GestureClick()
        click.connect("pressed", self.pointer_press)
        click.connect("released", self.pointer_release)
        surface.add_controller(click)
        scroll = Gtk.EventControllerScroll.new(Gtk.EventControllerScrollFlags.BOTH_AXES)
        scroll.connect("scroll", self.scroll)
        surface.add_controller(scroll)
        content.append(surface)

        entry = Gtk.Entry()
        self.entry = entry
        entry.set_placeholder_text("Deterministic text and keyboard consumer")
        entry.connect("changed", self.text_changed)
        entry.connect("notify::has-focus", self.entry_focus)
        keys = Gtk.EventControllerKey()
        keys.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        keys.connect("key-pressed", self.key_pressed)
        keys.connect("key-released", self.key_released)
        entry.add_controller(keys)
        entry_clicks = Gtk.GestureClick()
        entry_clicks.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        entry_clicks.connect("released", self.entry_clicked)
        entry.add_controller(entry_clicks)
        content.append(entry)

        selection = Gtk.DropDown.new_from_strings(["Alpha", "Beta", "Gamma"])
        self.selection_control = selection
        selection.connect("notify::selected", self.selection_changed)
        content.append(selection)

        delayed = Gtk.Button(label="Complete after a deterministic delay")
        delayed.connect("clicked", self.delayed_clicked)
        content.append(delayed)

        window.set_child(content)
        window.present()
        GLib.timeout_add(50, self.poll_control)
        GLib.idle_add(self.mark_ready)

    def draw_surface(self, _area, context, width, height):
        seed = self.options.seed
        context.set_source_rgb(
            (32 + seed % 192 * 17 % 192) / 255,
            (32 + seed % 192 * 29 % 192) / 255,
            (32 + seed % 192 * 43 % 192) / 255,
        )
        context.paint()
        context.set_source_rgb(1, 1, 1)
        context.set_line_width(5)
        context.rectangle(4, 4, width - 8, height - 8)
        context.stroke()

    def mark_ready(self):
        self.oracle.set_ready()
        return GLib.SOURCE_REMOVE

    def pointer_move(self, _controller, x, y):
        self.oracle.count("pointer_moves")
        if self.pointer_origin is not None:
            distance = abs(x - self.pointer_origin[0]) + abs(y - self.pointer_origin[1])
            self.dragged = self.dragged or distance >= 12
        self.oracle.record("pointer_moved", "interaction-surface", x=x, y=y)

    def pointer_press(self, gesture, _count, x, y):
        self.pointer_origin = (x, y)
        self.dragged = False
        self.oracle.count("pointer_presses")
        self.oracle.record(
            "pointer_pressed",
            "interaction-surface",
            x=x,
            y=y,
            button=gesture.get_current_button(),
        )

    def pointer_release(self, gesture, _count, x, y):
        self.oracle.count("pointer_releases")
        # A release ends either a drag or a click, never both.
        if self.dragged:
            self.oracle.count("drags")
        else:
            self.oracle.count("clicks")
        self.oracle.record(
            "pointer_released",
            "interaction-surface",
            x=x,
            y=y,
            button=gesture.get_current_button(),
        )
        self.pointer_origin = None

    def scroll(self, _controller, horizontal, vertical):
        if horizontal:
            self.oracle.count("horizontal_scrolls")
        if vertical:
            self.oracle.count("vertical_scrolls")
        self.oracle.record(
            "scrolled",
            "interaction-surface",
            horizontal_delta=horizontal,
            vertical_delta=vertical,
        )
        return True

    def key_pressed(self, _controller, keyval, keycode, state):
        self.oracle.record_key(
            "key_pressed",
            f"keycode:{keycode}",
            Gdk.keyval_name(keyval) or f"keyval:{keyval}",
            self.modifiers(state),
        )
        return False

    def key_released(self, _controller, keyval, keycode, state):
        self.oracle.record_key(
            "key_released",
            f"keycode:{keycode}",
            Gdk.keyval_name(keyval) or f"keyval:{keyval}",
            self.modifiers(state),
        )

    def modifiers(self, state):
        names = []
        for flag, name in (
            (Gdk.ModifierType.SHIFT_MASK, "shift"),
            (Gdk.ModifierType.CONTROL_MASK, "control"),
            (Gdk.ModifierType.ALT_MASK, "alt"),
            (Gdk.ModifierType.SUPER_MASK, "super"),
        ):
            if state & flag:
                names.append(name)
        return names

    def entry_clicked(self, _gesture, _count, _x, _y):
        self.oracle.count("entry_clicks")
        self.oracle.record("pointer_released", "text-entry")

    def text_changed(self, entry):
        self.oracle.update_text(entry.get_text())

    def entry_focus(self, entry, _parameter):
        self.oracle.focused_control = "text-entry" if entry.has_focus() else None
        self.oracle.record("focus_changed", "text-entry")

    def selection_changed(self, selection, _parameter):
        self.oracle.selection = ("Alpha", "Beta", "Gamma")[selection.get_selected()]
        self.oracle.record("selection_changed", "selection")

    def delayed_clicked(self, _button):
        self.oracle.phase = "pending"
        self.oracle.record("delayed_state_started", "delayed-action")
        GLib.timeout_add(500, self.delayed_complete)

    def delayed_complete(self):
        self.oracle.complete_delayed_state()
        return GLib.SOURCE_REMOVE

    def poll_control(self):
        command = self.oracle.read_control()
        if command is not None:
            if command.get("action") == "reset":
                self.entry.set_text("")
                self.selection_control.set_selected(0)
                self.oracle.reset()
            elif command.get("action") == "complete_delayed_state":
                self.oracle.complete_delayed_state()
            elif command.get("action") == "quit":
                self.quit()
        return GLib.SOURCE_CONTINUE


def main():
    options = arguments()
    title = f"Waldo GTK Interaction Oracle {options.seed:08x}"
    if options.probe_api:
        print(probe_json("gtk", "native_wayland", options.seed, title), end="")
        return 0
    if not all((options.state, options.events, options.control)):
        raise SystemExit("--state, --events and --control are required outside --probe-api")
    return GtkInteractionFixture(options).run()


if __name__ == "__main__":
    raise SystemExit(main())
