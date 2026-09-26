# Waldo

Waldo lets an agent observe and operate applications on a Niri Wayland
desktop. A local controller serves the command-line client and the personal
plugin's MCP server. The desktop-control skill supplies the agent's driving
guidance.

Waldo is a supervised development tool. Actions affect the foreground desktop;
one client holds the driving lease at a time. Screenshots and accessibility
content returned through MCP enter the agent's model conversation.

## Local installation

From the active Niri graphical session:

```sh
./install-local
waldo query --include-windows
```

The installer builds `waldod`, `waldo` and `waldo-mcp`, enables the user service,
installs the personal plugin and records the discovered environment at
`${XDG_STATE_HOME:-$HOME/.local/state}/waldo/environment.json`.
It currently requires Cargo, Codex, Git, jq, realpath, ripgrep, rsync, sha256sum and
systemctl on `PATH`, plus an authenticated Codex session for its installation
proof. When Claude Code is present, canonical installation also registers the
plugin there. The installer still uses Codex to verify skill loading and MCP
image delivery; a Claude-Code-only installation is not yet supported.

`WALDO_INSTALL_ROOT` selects a disposable installation tree and a separate
controller socket. This mode leaves the canonical service alone and removes
its temporary controller on exit; it still requires access to an existing
Codex login for verification.

## Controller and tools

`waldod` serves a private Unix socket at
`$XDG_RUNTIME_DIR/waldo/controller.sock`. The CLI accepts `--socket PATH`;
`WALDO_SOCKET_PATH` overrides the default for both client bindings.
`waldo-mcp` serves MCP over standard input and output.

| MCP tool | Request | Result |
| --- | --- | --- |
| `query` | `include_windows` | Outputs, workspaces, focus, keyboard layout, capabilities and lease state |
| `observe` | `source`, `include_accessibility`, optional `continue_from` | An observation identifier, coordinates, PNG and available accessibility state |
| `act` | `observation_id`, ordered `actions`, `verification` predicates | Per-action receipts, application outcome and a resulting observation when available |
| `stop` | Empty object | Cancellation and release of the client's driving lease and held input |

The CLI publishes the complete shared schema without connecting to a controller:

```sh
waldo schema
```

MCP `tools/list` supplies the input schema for each tool. Actions use a `type`
tag; the schema is the authoritative list of variants and fields. For example:

```json
{"type":"pointer_click","position":{"x":226,"y":156},"button":"primary","count":1}
```

Positions are desktop-logical coordinates. Output and window identifiers come
from Query; capability results disclose which capture and action routes are
available. An observation and its identifier belong to the controller
connection that requested it. Separate CLI invocations do not share one.
`continue_from` must also name an observation of exactly the same source.

For a CLI reproduction, `observe.json` can contain:

```json
{"source":{"type":"output","output_id":"OUTPUT_NAME"},"include_accessibility":false}
```

Replace `OUTPUT_NAME` with the output identifier returned by Query. Given an
`act.json` with `observation_id`, `actions` and `verification`, this command
captures fresh grounding on the same connection and replaces the supplied
observation identifier before dispatch:

```sh
waldo act act.json --observe observe.json
```

Explicit lease handover is available through the CLI only. MCP clients finish
their driving session with Stop.

## Image coordinates and results

Full-output, region and supported foreground-window captures are oriented to
the logical desktop. Buffer row inversion and output rotation or reflection
are applied before PNG encoding. `coordinates.transform` describes the output;
it has already been accounted for in the image.

`source_pixels` gives the desktop-oriented capture dimensions.
`model_image_pixels` gives the delivered image dimensions. MCP preserves aspect
ratio and limits the longest image edge to 2048 pixels; the CLI returns the
controller image without that presentation resize. An image point maps into
`logical_bounds` as follows:

```text
logical_x = origin.x + image_x * width / model_image_pixels.width
logical_y = origin.y + image_y * height / model_image_pixels.height
```

MCP returns the PNG as an image content block and marks it
`delivered_as_content` in the structured observation. Geometry-dependent actions
cite the observation used for their coordinates. Changed output layout,
geometry or relevant focus can make that grounding stale.

An `executed` receipt confirms dispatch, not application completion.
`application_outcome` and `completion_evidence` report what post-action evidence
supports; an inconclusive result can accompany a dispatched action. Batches
stop at a failed or unverifiable execution boundary and do not roll back
earlier actions. An indeterminate receipt means an action may have executed.
