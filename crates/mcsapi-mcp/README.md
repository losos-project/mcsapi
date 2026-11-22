# mcsapi-mcp

A stateless [MCP](https://modelcontextprotocol.io) server that exposes mcsapi
desktop policy as `simulate` and `arrange` tools over Streamable HTTP.

```sh
cargo run -p mcsapi-mcp   # http://127.0.0.1:8787/mcp; set MCSAPI_MCP_ADDR to change
```

`RUST_LOG` filters the log on standard error (default `info`). The server is
stateless: no `Mcp-Session-Id` is issued, nothing is stored between requests,
and `tools/list` or `tools/call` work without a prior `initialize`. Each call
carries everything it needs. `simulate` and `arrange` are also plain-Rust
functions and can be used without the transport.

`.mcp.json` (Claude Code) and `.vscode/mcp.json` (VS Code and Copilot) at the
repository root point at the default address, so start the server and the
tools appear in either.

## Tools

- `simulate` takes workspace IDs, an ordered list of operations (`insert`,
  `remove`, `focus`, `focus_next`, `focus_previous`, `promote_focused`,
  `switch_to`, `move_window`, `set_layout`), and optional output bounds. It
  replays them on a fresh `Desktop` and returns each workspace's windows in
  tiling order, focus, layout, the active workspace, and the placements. A new
  desktop uses `spiral` until `set_layout` changes it. A failing operation is
  reported by index with the library's error text.
- `arrange` computes logical window geometry for windows already in tiling
  order with `spiral`, `tall`, or `monocle`. The first window is the main pane.

## Layouts

- **Spiral** (the default on a new desktop): successive splits alternating
  horizontal and vertical. The first window takes the left half, the next
  takes the top half of the remainder, the next the left half of that, and so
  on. The last window fills whatever is left. A single window fills the output.
- **Tall**: one main window on the left (half the width, rounding down), with
  the rest stacked vertically on the right. Remainder pixels are distributed
  from the top. A single window fills the output. For more than one window, a
  width under 2 or a stack taller than the output returns `InsufficientSpace`,
  rather than emitting zero-sized tiles.
- **Monocle**: every window fills the output; the host displays only the
  focused one.
