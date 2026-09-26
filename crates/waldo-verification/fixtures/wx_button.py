#!/usr/bin/env python3
"""Disposable wxWidgets capture-on-press button with an activation oracle."""

import argparse
import json

import wx


class CaptureButton(wx.Panel):
    def __init__(self, parent, events):
        super().__init__(parent, pos=(100, 100), size=(220, 80))
        self.events = events
        self.pressed = False
        self.SetBackgroundColour("blue")
        self.Bind(wx.EVT_LEFT_DOWN, self.down)
        self.Bind(wx.EVT_LEFT_UP, self.up)
        self.Bind(wx.EVT_MOUSE_CAPTURE_LOST, self.lost)

    def record(self, kind, event):
        with open(self.events, "a", encoding="utf-8") as stream:
            stream.write(json.dumps({"kind": kind, "position": list(event.GetPosition())}) + "\n")

    def down(self, event):
        event.Skip()
        self.record("down", event)
        self.pressed = True
        self.SetFocus()
        if not self.HasCapture():
            self.CaptureMouse()

    def up(self, event):
        event.Skip()
        self.record("up", event)
        if self.pressed:
            self.pressed = False
            if self.HasCapture():
                self.ReleaseMouse()
            if wx.Rect((0, 0), self.GetSize()).Contains(event.GetPosition()):
                self.record("activated", event)

    def lost(self, _event):
        self.record("capture_lost", wx.MouseEvent())
        self.up(wx.MouseEvent())


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--events", required=True)
options = parser.parse_args()
app = wx.App()
frame = wx.Frame(None, title="Waldo wx button oracle", size=(700, 500))
panel = wx.Panel(frame)
panel.SetBackgroundColour("white")
button = CaptureButton(panel, options.events)
frame.Show()
app.MainLoop()
