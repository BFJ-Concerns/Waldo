# Development

[`AGENTS.md`](../AGENTS.md) describes the crate layout and the conventions
contributors follow.

## Building

```sh
cargo build --workspace
```

This needs Rust 1.88 or later and the libxkbcommon development headers.

## The gate

`scripts/gate` runs the checks CI runs on every push and pull request:

- `cargo fmt --check`
- `cargo check`
- Clippy with warnings denied
- `systemd-analyze --user verify` on the service file
- `cargo test --workspace`
- the Python tests under `scripts/tests`

`scripts/gate --check` runs every stage except the tests. CI runs
`scripts/gate --all`, which reports every failing stage instead of stopping
at the first.

## Tests that need a live desktop

Most tests that need a live desktop check for the session variables they use
— `NIRI_SOCKET`, `WAYLAND_DISPLAY` or the D-Bus session bus — and return
early, counting as passed, when those are missing. A run without them, such
as CI's, has not exercised those tests, so run the gate inside a Niri
session.

Seven clipboard tests are ignored by default instead, and fail rather than
pass without a session. Run them inside the session with an empty
clipboard:

```sh
cargo test --workspace -- --ignored
```

These include reads against test owners that temporarily take the clipboard.
Some publish to the clipboard and replace its owner. The
ordered-publication test returns early, and passes, unless the clipboard is
empty, and one launches a GTK fixture that needs Python 3 with PyGObject and
GTK 4.

## Outside-in verification

`waldo-verification` runs the real Waldo binaries against disposable
application fixtures and checks what each application actually received.
Its fixtures need GTK 4, Qt through PySide6, wxPython and Electron; the
clipboard circuit also needs wl-clipboard (`wl-copy` and `wl-paste`, with
`--watch` and `CLIPBOARD_STATE` support) and a POSIX shell (`sh`).
`scripts/verify-transformed-output.py` starts a disposable nested Niri to
check captures and pointer input on a transformed output.
