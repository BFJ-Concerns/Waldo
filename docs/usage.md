# Using Waldo

This page is the reference for Waldo's agent tools, its command-line
client and the results they return. The
[README](../README.md#usage) covers everyday use.

## MCP tools

| Tool | Request | Result |
| --- | --- | --- |
| `query` | `include_windows` | Outputs, workspaces, focus, keyboard layout, capabilities and lease state |
| `observe` | `source`, `include_accessibility`, optional `continue_from` | An observation identifier, coordinates, a PNG and any available accessibility state |
| `act` | `observation_id`, ordered `actions`, `verification` predicates | Per-action receipts, the application outcome and, when available, a resulting observation |
| `stop` | Empty object | Cancels the client's batch and releases its driving lease and any held input |

An observation source is a whole output, a window, or a region of an output.
Actions are tagged by `type`, for example:

```json
{"type":"pointer_click","position":{"x":226,"y":156},"button":"primary","count":1}
```

MCP `tools/list` gives each tool's input schema. `waldo schema` prints the
complete versioned protocol schema without connecting to a controller, and is
the authority on every action and predicate variant.

## Command-line client

`waldo` speaks the same protocol as the MCP server, with subcommands `schema`,
`query`, `observe`, `act`, `stop`, `handover` and `send`; run
`waldo <command> --help` for a command's options. `waldod` serves
`$XDG_RUNTIME_DIR/waldo/controller.sock` unless the `WALDO_SOCKET_PATH`
setting names another socket, and both `waldod` and `waldo` accept
`--socket PATH`.

An observation and its identifier belong to the controller connection that
requested it, so separate `waldo` invocations do not share one. To replay an
act document, give `waldo act` an observe document too; it captures fresh
grounding on the same connection and substitutes that observation's
identifier before dispatch. With `observe.json` containing:

```json
{"source":{"type":"output","output_id":"OUTPUT_NAME"},"include_accessibility":false}
```

where `OUTPUT_NAME` is an output identifier from `waldo query`, and `act.json`
holding `observation_id`, `actions` and `verification`, run:

```sh
waldo act act.json --observe observe.json
```

Explicit lease handover to another connection is available through the CLI
only; MCP clients finish their driving session with Stop.

## Image coordinates and results

Full-output, region and supported foreground-window captures are oriented to
the logical desktop. Buffer row inversion and output rotation or reflection
are applied before PNG encoding. `coordinates.transform` describes the output;
it has already been accounted for in the image.

`source_pixels` gives the desktop-oriented capture dimensions.
`model_image_pixels` gives the delivered image dimensions. MCP preserves aspect
ratio and limits the longest image edge to `WALDO_MODEL_IMAGE_MAX_EDGE`
(default 2048 pixels), leaving smaller images unscaled; the CLI returns the
controller image without that presentation resize. An image point maps into
`logical_bounds` as follows:

```text
logical_x = origin.x + image_x * width / model_image_pixels.width
logical_y = origin.y + image_y * height / model_image_pixels.height
```

Positions in actions are desktop-logical coordinates; output and window
identifiers come from Query. MCP returns the PNG as an image content block and
marks it `delivered_as_content` in the structured observation.
Geometry-dependent actions cite the observation used for their coordinates,
and `continue_from` must name an observation of exactly the same source.
Changed output layout, geometry or relevant focus can make that grounding
stale.

An `executed` receipt confirms dispatch, not application completion.
`application_outcome` and `completion_evidence` report what post-action evidence
supports; an inconclusive result can accompany a dispatched action. Batches
stop at a failed or unverifiable execution boundary and do not roll back
earlier actions. An indeterminate receipt means an action may have executed.

## Scope and limits

Waldo drives the Niri session its environment names: the compositor reached
through `NIRI_SOCKET` and `WAYLAND_DISPLAY`, and the controller socket under
`XDG_RUNTIME_DIR`. Query reports which backends are available and, for each
unavailable one, its `limitation`.

- There is no nested, headless or private desktop. No command creates,
  selects or tears down a compositor session, and no MCP tool takes a session
  argument. A second controller can serve another session only by starting
  `waldod` under that session's environment and pointing clients at its socket
  with `--socket` or `WALDO_SOCKET_PATH`.
  `scripts/verify-transformed-output.py` sets up a disposable nested Niri this
  way for one regression test; it is a test harness, not a supported session
  lifecycle.
- Operation is foreground-only and single-driver. One connection holds the
  driving lease, and pointer, keyboard and clipboard input land on the
  foreground desktop; only window and workspace operations can target an
  unfocused window by its ID. Background application input, private desktops,
  independent seats and simultaneous user and agent work are not supported.
- Niri is the only supported compositor, and Claude Code the only fully
  supported agent.
- Waldo is a supervised development tool for your own machine. It is not fit
  for unattended operation or a shared workstation, and no packaged releases
  exist.
