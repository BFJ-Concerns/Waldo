#!/usr/bin/env python3
"""Disposable GTK coordinate target with an application-level event oracle."""

import argparse
import json
import os
import signal
import time

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import GLib, Gtk  # noqa: E402


def arguments():
    parser = argparse.ArgumentParser()
    parser.add_argument("--events", required=True)
    parser.add_argument("--ready", required=True)
    parser.add_argument("--seed", required=True, type=int)
    parser.add_argument(
        "--scenario",
        choices=("delayed", "continuous", "interception", "contradictory"),
        default="delayed",
    )
    parser.add_argument("--delay-ms", type=int, default=750)
    parser.add_argument("--continuous-hz", type=int, default=20)
    parser.add_argument("--columns", type=int, default=5)
    parser.add_argument("--rows", type=int, default=3)
    return parser.parse_args()


class CoordinateFixture(Gtk.Application):
    def __init__(self, options):
        super().__init__(application_id="com.bfj.WaldoCoordinateOracle")
        self.options = options
        self.phase = 0
        self.click_count = 0
        self.interceptor_count = 0
        self.frame_count = 0
        self.written_state = None
        self.continuous_source = None
        self.area = None
        self.window = None

    def do_activate(self):
        self.window = Gtk.ApplicationWindow(application=self)
        self.window.set_title("Waldo Coordinate Oracle")
        self.window.set_default_size(1000, 700)
        self.area = Gtk.DrawingArea()
        self.area.set_draw_func(self.draw)
        target_click = Gtk.GestureClick()
        target_click.connect("pressed", self.pointer_event, "press")
        target_click.connect("released", self.pointer_event, "release")
        self.area.add_controller(target_click)
        if self.options.scenario == "interception":
            overlay = Gtk.Overlay()
            overlay.set_child(self.area)
            interceptor = Gtk.Box()
            interceptor.set_hexpand(True)
            interceptor.set_vexpand(True)
            interceptor_click = Gtk.GestureClick()
            interceptor_click.connect("pressed", self.interceptor_event, "press")
            interceptor_click.connect("released", self.interceptor_event, "release")
            interceptor.add_controller(interceptor_click)
            overlay.add_overlay(interceptor)
            self.window.set_child(overlay)
        else:
            self.window.set_child(self.area)
        self.window.fullscreen()
        self.window.present()
        signal.signal(signal.SIGUSR1, self.request_change)

    def colour(self, column, row):
        cell = row * self.options.columns + column
        phase = self.phase if cell == 0 else self.click_count if cell == 1 else 0
        if self.options.scenario == "continuous" and cell == 0:
            phase = self.frame_count
        seed = self.options.seed
        return (
            32 + (seed * 17 + cell * 47 + phase * 83) % 192,
            32 + (seed * 29 + cell * 71 + phase * 59) % 192,
            32 + (seed * 43 + cell * 37 + phase * 97) % 192,
        )

    def draw(self, _area, context, width, height):
        for row in range(self.options.rows):
            top = height * row / self.options.rows
            bottom = height * (row + 1) / self.options.rows
            for column in range(self.options.columns):
                left = width * column / self.options.columns
                right = width * (column + 1) / self.options.columns
                red, green, blue = self.colour(column, row)
                context.set_source_rgb(red / 255, green / 255, blue / 255)
                context.rectangle(left, top, right - left, bottom - top)
                context.fill()
        context.set_source_rgb(1, 1, 1)
        context.set_line_width(4)
        for column in range(1, self.options.columns):
            x = width * column / self.options.columns
            context.move_to(x, 0)
            context.line_to(x, height)
        for row in range(1, self.options.rows):
            y = height * row / self.options.rows
            context.move_to(0, y)
            context.line_to(width, y)
        context.stroke()
        state = (
            self.phase,
            self.click_count,
            self.interceptor_count,
            self.frame_count,
            width,
            height,
        )
        if self.written_state != state:
            self.written_state = state
            GLib.idle_add(self.write_ready, width, height)

    def write_ready(self, width, height):
        cells = []
        for row in range(self.options.rows):
            for column in range(self.options.columns):
                cells.append(
                    {
                        "column": column,
                        "row": row,
                        "rgb": self.colour(column, row),
                    }
                )
        temporary = self.options.ready + ".tmp"
        with open(temporary, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "seed": self.options.seed,
                    "scenario": self.options.scenario,
                    "phase": self.phase,
                    "click_count": self.click_count,
                    "interceptor_count": self.interceptor_count,
                    "frame_count": self.frame_count,
                    "recorded_at_unix_ns": time.time_ns(),
                    "columns": self.options.columns,
                    "rows": self.options.rows,
                    "width": width,
                    "height": height,
                    "cells": cells,
                },
                handle,
                indent=2,
            )
            handle.write("\n")
        os.replace(temporary, self.options.ready)
        open(self.options.events, "a", encoding="utf-8").close()
        return GLib.SOURCE_REMOVE

    def record_pointer_event(self, gesture, x, y, kind, receiver):
        button = gesture.get_current_button()
        column = min(int(x * self.options.columns / self.area.get_width()), self.options.columns - 1)
        row = min(int(y * self.options.rows / self.area.get_height()), self.options.rows - 1)
        with open(self.options.events, "a", encoding="utf-8") as handle:
            handle.write(
                json.dumps(
                    {
                        "kind": f"{receiver}_{kind}",
                        "recorded_at_unix_ns": time.time_ns(),
                        "button": button,
                        "x": x,
                        "y": y,
                        "column": column,
                        "row": row,
                        "width": self.area.get_width(),
                        "height": self.area.get_height(),
                    }
                )
                + "\n"
            )
            handle.flush()

    def pointer_event(self, gesture, _count, x, y, kind):
        self.record_pointer_event(gesture, x, y, kind, "target")
        if kind == "release":
            self.click_count += 1
            if self.options.scenario == "delayed":
                GLib.timeout_add(self.options.delay_ms, self.change_first_cell)
            elif self.continuous_source is None:
                if self.options.scenario == "continuous":
                    interval = max(1, round(1000 / self.options.continuous_hz))
                    self.continuous_source = GLib.timeout_add(interval, self.advance_frame)
                elif self.options.scenario == "contradictory":
                    self.change_first_cell()
            self.area.queue_draw()

    def interceptor_event(self, gesture, _count, x, y, kind):
        self.record_pointer_event(gesture, x, y, kind, "interceptor")
        if kind == "release":
            self.interceptor_count += 1
            self.area.queue_draw()

    def request_change(self, _signum, _frame):
        GLib.idle_add(self.change_first_cell)

    def change_first_cell(self):
        self.phase += 1
        self.area.queue_draw()
        return GLib.SOURCE_REMOVE

    def advance_frame(self):
        self.frame_count += 1
        self.area.queue_draw()
        return GLib.SOURCE_CONTINUE


if __name__ == "__main__":
    options = arguments()
    raise SystemExit(CoordinateFixture(options).run())
