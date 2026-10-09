# Waldo

Waldo lets a Claude Code session observe and operate applications on a live
Niri Wayland desktop: a local controller (`waldod`) serves a private socket,
and two bindings speak its versioned protocol — the `waldo` CLI for
full-fidelity reproduction and diagnostics, and `waldo-mcp`, the plugin's MCP
server, for the agent's normal loop. `README.md` is the operator's guide and
is kept truthful in the same change as any behaviour it describes.

## The gate

`scripts/gate` is the repository's quality gate: formatting, `cargo check`,
Clippy with warnings denied, `systemd-analyze --user verify` on the service
file, `cargo test --workspace`, and the Python launcher tests under
`scripts/tests`. CI runs exactly that command on every push
and pull request, and every pull request — documentation included — is
expected to pass it. Run it inside the Niri session: the live-session
tests (screencopy, Niri IPC, virtual input, AT-SPI) return early when
`NIRI_SOCKET`, `WAYLAND_DISPLAY` or the session bus is absent, and cargo
counts that early return as a pass, so a run outside the session — CI's
included — has not exercised them. Four clipboard tests are `#[ignore]`d
because they take over the live clipboard; run them inside the session with
`cargo test --workspace -- --ignored` (their fixture needs Python 3 with
PyGObject and GTK 4). Live tests start their own
controllers on private sockets and open their own fixture windows on the
live desktop; they do not touch an installed service or its lease.
For fast iteration between gate runs, `scripts/gate --check` runs every stage
but the test suite, and `cargo check --workspace` alone is faster still.

Run `./install-local` only when a change is ready to replace the operator's
live installation: it builds and installs the binaries, enables the user
service, installs the plugin, proves plugin loading and image delivery, and
records the environment that proof exercised — outputs, scale, transforms,
keyboard layout, capabilities — as the installation's profile. It needs the
active Niri session. `plugins/waldo/` is the plugin source; never patch the
installed copy. With the service running, `waldo query` shows the current
profile, capabilities, outputs and keyboard layout through the controller.

## Architecture

- `waldo-protocol` owns the shared wire schema and action/result types. An
  incompatible request change needs a new schema major; additive result
  fields stay ignorable.
- `waldo-controller` owns sessions and the driving lease. `waldo-service`
  connects that state machine to the live desktop, batches, verification and
  the private controller socket.
- `waldo-cli` and `waldo-mcp` are bindings over the same controller protocol;
  `waldo-socket` supplies their transport.
- `waldo-settings` declares every runtime setting once — each value that
  differs between machines or people — while `install-local` reads its own
  installer settings from its environment. The README's Configuration
  section names every one of both kinds; a test enforces that.
- `waldo-action` grounds ordered actions. The Niri, screencopy, accessibility,
  clipboard, virtual-pointer and virtual-keyboard crates provide the desktop
  backends.
- `waldo-verification` holds deterministic fixtures and outside-in evidence
  for the live contract.

## Cross-machine adaptability

Derive behaviour from runtime-discovered desktop state — output geometry,
scale, transforms, the compositor's keyboard layout, compositor capabilities,
application state — never from workstation-specific identifiers. Keep paths,
timings, applications and display layouts out of product logic. When a
genuine machine difference needs policy, expose it as a discovered capability
or as a setting declared in `waldo-settings` and documented in the README;
tuning internals (poll intervals, quiet periods, deadlines, resource caps)
stay internal.

## The desktop-control skill

`plugins/waldo/skills/desktop-control/SKILL.md` is the driving guidance the
plugin ships and the MCP server's instructions tell the model to load. A
change the runtime needs the skill to carry — a new action variant, a
changed result field, a grounding rule learnt from an agent going wrong —
updates the skill's literal examples and stated facts here, in the same
change, and is described in the README's "Guidance for driving agents"
section. Keep such changes to the examples and facts; propose restructuring
or rewording the skill in an issue rather than a direct edit. The protocol
tests decode every JSON example in the skill through the protocol
crate's wire types and require every action and predicate variant to have
one, so a runtime change that outgrows the skill fails the gate until the
skill catches up.

Defects in Waldo are reported on the public issue tracker named in the
README.
