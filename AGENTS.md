# Waldo

## Development

Run workspace checks from the repository root:

- `cargo check --workspace` for normal iteration.
- `cargo test --workspace` for the full suite. Some tests connect to the active
  Niri, Wayland screencopy, and AT-SPI session, so run them inside the operator's
  graphical session.
- `cargo clippy --workspace --all-targets -- -D warnings` for the strict lint
  gate.
- `systemd-analyze --user verify packaging/waldod.service` after service-file
  changes.

Run `./install-local` only when a change is ready to replace the operator's live
installation. It requires the active Niri session and updates the installed
binaries, user service, personal plugin, marketplace entry, and discovered
environment record. `plugins/waldo/` is the plugin source; do not patch the
installed copy.

With the installed service running, use `waldo query` to inspect the current
profile, capabilities, outputs, and keyboard layout through the controller.

## Architecture

- `waldo-protocol` owns the shared wire schema and action/result types.
- `waldo-controller` owns sessions and the driving lease. `waldo-service`
  connects that state machine to the live desktop, batches, verification, and
  the private controller socket.
- `waldo-cli` and `waldo-mcp` are bindings over the same controller protocol;
  `waldo-socket` supplies their transport.
- `waldo-action` grounds ordered actions. The Niri, screencopy, accessibility,
  clipboard, virtual-pointer, and virtual-keyboard crates provide the desktop
  backends.
- `waldo-verification` contains deterministic fixtures and outside-in evidence
  for the live contract.

## Cross-machine adaptability

Build Waldo from runtime-discovered capabilities and reported desktop state so
it remains useful across machines. Derive behaviour from output geometry,
scale, transforms, keyboard layout, compositor capabilities, and application
state. Keep workstation-specific identifiers, paths, timings, applications,
and display layouts out of product logic; when a genuine machine difference
needs policy, expose it as a discovered capability or explicit configuration.
