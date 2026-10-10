#!/usr/bin/env python3
"""Deterministic Qt or XWayland interaction fixture with a consumer oracle."""

import argparse
import os
import sys


def arguments():
    parser = argparse.ArgumentParser()
    parser.add_argument("--state")
    parser.add_argument("--events")
    parser.add_argument("--control")
    parser.add_argument("--seed", required=True, type=int)
    parser.add_argument(
        "--display-route",
        choices=("native_wayland", "xwayland"),
        default="native_wayland",
    )
    parser.add_argument("--probe-api", action="store_true")
    return parser.parse_args()


OPTIONS = arguments()
TOOLKIT = "xwayland" if OPTIONS.display_route == "xwayland" else "qt"
# The requested route decides the platform; an inherited QT_QPA_PLATFORM
# must not run the native cell through XWayland or the reverse.
os.environ["QT_QPA_PLATFORM"] = "xcb" if OPTIONS.display_route == "xwayland" else "wayland"

from PySide6.QtCore import QEvent, QObject, QPointF, Qt, QTimer  # noqa: E402
from PySide6.QtGui import QColor, QKeyEvent, QMouseEvent, QPalette, QWheelEvent  # noqa: E402
from PySide6.QtWidgets import (  # noqa: E402
    QApplication,
    QComboBox,
    QFrame,
    QLabel,
    QLineEdit,
    QPushButton,
    QVBoxLayout,
    QWidget,
)

from interaction_common import InteractionOracle, probe_json  # noqa: E402


class ConsumerFilter(QObject):
    def __init__(self, fixture):
        super().__init__()
        self.fixture = fixture

    def eventFilter(self, watched, event):
        if watched is self.fixture.surface:
            if event.type() == QEvent.Type.MouseMove:
                self.fixture.pointer_move(event)
            elif event.type() == QEvent.Type.MouseButtonPress:
                self.fixture.pointer_press(event)
            elif event.type() == QEvent.Type.MouseButtonRelease:
                self.fixture.pointer_release(event)
            elif event.type() == QEvent.Type.Wheel:
                self.fixture.scroll(event)
        if watched is self.fixture.entry:
            if event.type() == QEvent.Type.KeyPress:
                self.fixture.key_event(event, "key_pressed")
            elif event.type() == QEvent.Type.KeyRelease:
                self.fixture.key_event(event, "key_released")
            elif event.type() == QEvent.Type.MouseButtonRelease:
                self.fixture.oracle.count("entry_clicks")
                self.fixture.oracle.record("pointer_released", "text-entry")
            elif event.type() == QEvent.Type.FocusIn:
                self.fixture.oracle.focused_control = "text-entry"
                self.fixture.oracle.record("focus_changed", "text-entry")
            elif event.type() == QEvent.Type.FocusOut:
                self.fixture.oracle.focused_control = None
                self.fixture.oracle.record("focus_changed", "text-entry")
        return False


class QtInteractionFixture:
    def __init__(self, options):
        self.options = options
        route_label = "XWayland" if options.display_route == "xwayland" else "Qt"
        self.title = f"Waldo {route_label} Interaction Oracle {options.seed:08x}"
        self.oracle = InteractionOracle(
            toolkit=TOOLKIT,
            display_route=options.display_route,
            seed=options.seed,
            title=self.title,
            state=options.state,
            events=options.events,
            control=options.control,
            runtime_backend=QApplication.platformName(),
        )
        self.pointer_origin = None
        self.dragged = False
        self.window = QWidget()
        self.window.setWindowTitle(self.title)
        self.window.resize(920, 700)
        layout = QVBoxLayout(self.window)
        marker = QLabel(f"{route_label.upper()}-{options.seed:08x}")
        marker.setAlignment(Qt.AlignmentFlag.AlignCenter)
        layout.addWidget(marker)

        self.surface = QFrame()
        self.surface.setObjectName("interaction-surface")
        self.surface.setMinimumSize(760, 340)
        self.surface.setMouseTracking(True)
        colour = QColor(
            32 + options.seed % 192 * 17 % 192,
            32 + options.seed % 192 * 29 % 192,
            32 + options.seed % 192 * 43 % 192,
        )
        palette = self.surface.palette()
        palette.setColor(QPalette.ColorRole.Window, colour)
        self.surface.setAutoFillBackground(True)
        self.surface.setPalette(palette)
        layout.addWidget(self.surface)

        self.entry = QLineEdit()
        self.entry.setPlaceholderText("Deterministic text and keyboard consumer")
        self.entry.textChanged.connect(self.oracle.update_text)
        layout.addWidget(self.entry)

        self.selection = QComboBox()
        self.selection.addItems(["Alpha", "Beta", "Gamma"])
        self.selection.currentTextChanged.connect(self.selection_changed)
        layout.addWidget(self.selection)

        delayed = QPushButton("Complete after a deterministic delay")
        delayed.clicked.connect(self.delayed_clicked)
        layout.addWidget(delayed)

        self.consumer_filter = ConsumerFilter(self)
        self.surface.installEventFilter(self.consumer_filter)
        self.entry.installEventFilter(self.consumer_filter)
        self.control_timer = QTimer()
        self.control_timer.setInterval(50)
        self.control_timer.timeout.connect(self.poll_control)

    def show(self):
        self.window.show()
        self.control_timer.start()
        QTimer.singleShot(0, self.oracle.set_ready)

    def pointer_move(self, event: QMouseEvent):
        position = event.position()
        self.oracle.count("pointer_moves")
        if self.pointer_origin is not None:
            distance = abs(position.x() - self.pointer_origin.x()) + abs(
                position.y() - self.pointer_origin.y()
            )
            self.dragged = self.dragged or distance >= 12
        self.oracle.record(
            "pointer_moved",
            "interaction-surface",
            x=position.x(),
            y=position.y(),
        )

    def pointer_press(self, event: QMouseEvent):
        position = event.position()
        self.pointer_origin = QPointF(position)
        self.dragged = False
        self.oracle.count("pointer_presses")
        self.oracle.record(
            "pointer_pressed",
            "interaction-surface",
            x=position.x(),
            y=position.y(),
            button=int(event.button().value),
        )

    def pointer_release(self, event: QMouseEvent):
        position = event.position()
        self.oracle.count("pointer_releases")
        # A release ends either a drag or a click, never both.
        if self.dragged:
            self.oracle.count("drags")
        else:
            self.oracle.count("clicks")
        self.oracle.record(
            "pointer_released",
            "interaction-surface",
            x=position.x(),
            y=position.y(),
            button=int(event.button().value),
        )
        self.pointer_origin = None

    def scroll(self, event: QWheelEvent):
        delta = event.angleDelta()
        if delta.x():
            self.oracle.count("horizontal_scrolls")
        if delta.y():
            self.oracle.count("vertical_scrolls")
        self.oracle.record(
            "scrolled",
            "interaction-surface",
            horizontal_delta=delta.x(),
            vertical_delta=delta.y(),
        )

    def key_event(self, event: QKeyEvent, kind):
        logical_key = event.text() or f"qt-key:{event.key()}"
        modifiers = []
        for flag, name in (
            (Qt.KeyboardModifier.ShiftModifier, "shift"),
            (Qt.KeyboardModifier.ControlModifier, "control"),
            (Qt.KeyboardModifier.AltModifier, "alt"),
            (Qt.KeyboardModifier.MetaModifier, "super"),
        ):
            if event.modifiers() & flag:
                modifiers.append(name)
        self.oracle.record_key(
            kind,
            f"scan:{event.nativeScanCode()}",
            logical_key,
            modifiers,
        )

    def selection_changed(self, value):
        self.oracle.selection = value
        self.oracle.record("selection_changed", "selection")

    def delayed_clicked(self):
        self.oracle.phase = "pending"
        self.oracle.record("delayed_state_started", "delayed-action")
        QTimer.singleShot(500, self.oracle.complete_delayed_state)

    def poll_control(self):
        command = self.oracle.read_control()
        if command is None:
            return
        if command.get("action") == "reset":
            self.entry.clear()
            self.selection.setCurrentIndex(0)
            self.oracle.reset()
        elif command.get("action") == "complete_delayed_state":
            self.oracle.complete_delayed_state()
        elif command.get("action") == "quit":
            QApplication.quit()


def main():
    route_label = "XWayland" if OPTIONS.display_route == "xwayland" else "Qt"
    title = f"Waldo {route_label} Interaction Oracle {OPTIONS.seed:08x}"
    if OPTIONS.probe_api:
        print(probe_json(TOOLKIT, OPTIONS.display_route, OPTIONS.seed, title), end="")
        return 0
    if not all((OPTIONS.state, OPTIONS.events, OPTIONS.control)):
        raise SystemExit("--state, --events and --control are required outside --probe-api")
    application = QApplication(sys.argv[:1])
    fixture = QtInteractionFixture(OPTIONS)
    fixture.show()
    return application.exec()


if __name__ == "__main__":
    raise SystemExit(main())
