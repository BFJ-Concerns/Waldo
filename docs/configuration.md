# Configuration

Anything that differs between machines or people is a setting; nothing else
needs a source edit.

## Runtime settings

Waldo's binaries read their settings from
`${XDG_CONFIG_HOME:-$HOME/.config}/waldo/settings`, one `NAME=VALUE` per line.
As the XDG base directory specification requires, a relative or empty
`XDG_CONFIG_HOME` is ignored, and when neither it nor `HOME` is an absolute
path no settings file is read:

```sh
# Paste with Ctrl+Shift+V, as terminals expect
WALDO_CLIPBOARD_PASTE_CHORD=ctrl+shift+v
```

Blank lines and lines starting with `#` are ignored, values are not quoted,
and a name that is not a setting is an error. A setting can also come from
the process environment, and some have a command-line flag; the flag wins,
then the environment, then the settings file, then the default. Every Waldo
binary reads the file whenever it resolves a setting, however it was
launched. Restart `waldod` after changing a value it reads.

| Setting | Default | Effect |
| --- | --- | --- |
| `WALDO_SOCKET_PATH` | `$XDG_RUNTIME_DIR/waldo/controller.sock` | The private controller socket `waldod` serves and both clients connect to. An absolute path; a relative one is refused, and a relative `XDG_RUNTIME_DIR` counts as unset. |
| `WALDO_LEASE_IDLE_TIMEOUT_SECONDS` | `30` | Seconds after the driver's last action before a driving lease lapses and held input is released. Query and Observe do not count as activity, and a lease is never lapsed while a batch is running. Whole seconds, at least 1; the timeout cannot be disabled. |
| `WALDO_CLIPBOARD_PASTE_CHORD` | `ctrl+v` | Clipboard-paste key names separated by `+`; whitespace around each name is trimmed and empty names are refused at start-up. Key names use the virtual keyboard vocabulary and are checked against the active layout at paste time; an unavailable key fails without a default-chord fallback. |
| `WALDO_MODEL_IMAGE_MAX_EDGE` | `2048` | Maximum longest edge in pixels for MCP images. A positive whole number; smaller images are delivered unscaled. Restart the MCP process after changing it. |

## Keyboard layout

Keyboard input uses the keymap the compositor reports, so the layout needs
no setting. Text includes characters produced by one dead key followed by a
base key, using the Compose data for `waldod`'s locale (`LC_ALL`,
`LC_CTYPE`, then `LANG`). Direct keys take precedence. If Compose data is
unavailable, direct typing remains available; characters without a
supported sequence are refused with the active layout named. Only when the
session exposes no keymap does Waldo fall back to the standard XKB
environment — `XKB_DEFAULT_LAYOUT`, and optionally `XKB_DEFAULT_VARIANT`,
`XKB_DEFAULT_MODEL`, `XKB_DEFAULT_OPTIONS` and `XKB_DEFAULT_RULES` — in
`waldod`'s environment; with neither, Query reports the keyboard route
unavailable and says why.

Waldo presses the dead key and then the base key; the character arrives
only where the receiving application composes dead keys. GTK 4 on Wayland
composes them only with a running input method or
`GTK_IM_MODULE=gtk-im-context-simple`. Where exact text matters and
composition is uncertain, use the clipboard-paste route, which delivers
the string exactly. The automatic route tries accessibility text
replacement on the focused element first, then clipboard paste, then the
keyboard, moving on when a route is unavailable, so its receipt names the
route that actually carried the text.

## Installer settings

Installer settings are read from the environment of the `./install-local`
invocation only. The installer passes an explicit Cargo install root: the
canonical default is `CARGO_INSTALL_ROOT`, then `CARGO_HOME`, then `~/.cargo`
(the usual Cargo location); Cargo's `install.root` configuration is not used.
Binaries land in `<root>/bin`, and that directory supplies the service and
proof clients regardless of the caller's `PATH`.

The controller, readiness query and plugin proof honour `WALDO_SOCKET_PATH`,
then the runtime settings file, then the runtime default. An environment
socket override is written into the installed service and the installed
plugin's MCP server entries; without one, both resolve the socket through the
settings file and default. A custom `XDG_CONFIG_HOME` is carried into the
service and the MCP server entries too. Rehearsals
use `<install root>/home/.config` as their config home and, when no socket
override is set, export a private socket beneath `WALDO_RUNTIME_DIR` to every
child so they cannot connect to the live controller.

| Setting | Default | Effect |
| --- | --- | --- |
| `WALDO_INSTALL_ROOT` | unset | Names a disposable installation tree for a rehearsal install that leaves the canonical service alone; the settings below default beneath it. |
| `WALDO_CARGO_INSTALL_ROOT` | `${CARGO_INSTALL_ROOT:-${CARGO_HOME:-~/.cargo}}` | Cargo install root; binaries land in `<root>/bin`. Rehearsal default: `<install root>/cargo`. Relative paths are resolved from the invocation directory. |
| `WALDO_MARKETPLACE_NAME` | `waldo` | Name given to a marketplace manifest the installer creates. An existing manifest keeps its own name and owner, and the plugin is registered under that name. Registration refuses a Claude Code marketplace of the same name that lives elsewhere. |
| `WALDO_MARKETPLACE_OWNER` | your login name | Owner name given to a marketplace manifest the installer creates. |
| `WALDO_PLUGIN_HOME` | `${XDG_DATA_HOME:-~/.local/share}/waldo/plugins/waldo` | Where the plugin source is copied for the agents' marketplaces. Each install replaces the directory's contents, so it must be absent, empty or an earlier Waldo plugin copy, and outside the checkout. Its parent directory is the Claude Code marketplace, whose manifest `<parent>/.claude-plugin/marketplace.json` keeps any other plugins it lists. |
| `WALDO_MARKETPLACE` | `~/.agents/plugins/marketplace.json` | The Codex marketplace file the plugin is registered in; other plugins it lists are kept. |
| `WALDO_SERVICE_FILE` | `${XDG_CONFIG_HOME:-~/.config}/systemd/user/waldod.service` | Where the user service file is written. It must be named `waldod.service`, since the installer enables and restarts the unit by that name, and be the file the user manager resolves that name to; the installer checks both and stops otherwise. |
| `WALDO_PROFILE_FILE` | `${XDG_STATE_HOME:-~/.local/state}/waldo/environment.json` | Where the recorded installation environment is written. |
| `WALDO_CODEX_HOME` | `<install root>/codex` | Rehearsal installs with `codex` present only: the Codex home used for the best-effort Codex proof. |
| `WALDO_CODEX_AUTH_SOURCE` | `${CODEX_HOME:-~/.codex}/auth.json` | Rehearsal installs with `codex` present only: the existing Codex login linked into that home. |
| `WALDO_RUNTIME_DIR` | `$XDG_RUNTIME_DIR/waldo-install-<hash>` | Rehearsal installs only: the runtime directory holding the temporary controller socket. |
