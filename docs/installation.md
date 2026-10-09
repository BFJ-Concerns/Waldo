# Installing Waldo

Run `./install-local` from a terminal inside your Niri session; the
[README](../README.md#requirements) lists what it needs.

The installer:

1. builds and installs `waldod`, `waldo` and `waldo-mcp` with `cargo install`
   (into `~/.cargo/bin` by default);
2. installs and starts the `waldod` user service;
3. copies the plugin to `~/.local/share/waldo/plugins/waldo` and registers it
   with Claude Code through a directory marketplace there;
4. proves the installation with one real headless Claude Code turn; and
5. records what it found — outputs, scale, transforms, keyboard layout and
   capabilities — in
   `${XDG_STATE_HOME:-$HOME/.local/state}/waldo/environment.json`.

The proof turn runs Claude Code from an empty private directory with its
built-in tools removed, Waldo's read-only `query` and `observe` tools
pre-approved and permission prompts turned off, so any other tool call is
denied unless your own Claude Code settings (permission rules, hooks or
permission mode) allow it. It passes only when the transcript shows the
plugin loaded from the marketplace, its MCP server connected, and an Observe
result carrying an image; the model's reply must also report completion, but
is never taken as evidence of loading or image delivery. Nothing is recorded
as installed without that proof. This turn sends a real screenshot of one of
your outputs to Claude, so clear anything sensitive from the screen before
you run the installer.

The installed plugin starts `waldo-mcp` by its absolute path, so agent
sessions need no extra `PATH` setup. To check the controller afterwards with
the command-line client, which lives in Cargo's bin directory (`~/.cargo/bin`
unless you configured another; add it to your `PATH` or use the full path):

```sh
waldo query --include-windows
```

Run `./install-local` again to upgrade after pulling new changes.

## Rehearsal installs

Set `WALDO_INSTALL_ROOT` to a disposable directory to rehearse an installation
without touching your real one. A rehearsal installs into that tree, uses its
own controller socket, leaves the canonical service and your Claude Code
configuration alone, loads the candidate plugin for the proof turn only (with
`claude --plugin-dir`), and removes its temporary controller on exit. The
private socket is used only when `WALDO_SOCKET_PATH` is not already set: a
value inherited from your environment is used as given, so unset it first if
it names your live controller's socket.

## Codex

Codex support is best-effort. When `codex` is on `PATH`, the installer also
registers the plugin in Codex's marketplace and proves it with its own turn:
`codex exec` in a read-only sandbox must return an Observe result carrying an
image, and the MCP manifest in the plugin Codex installed must match the
candidate's. A Codex failure is reported and recorded under
`installation.agents.codex` in the environment file but never fails the
installation; when Codex is absent, it is recorded as `{"present": false}`. A
rehearsal links your existing Codex login into a private Codex home.

The installer's own settings are listed under
[Installer settings](configuration.md#installer-settings).
