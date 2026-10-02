# lasso 🤠

Lightweight image editor for Omarchy: lasso-select a region, then let your AI
agent do the editing. The GUI is selection-only — **all pixel edits are made
by the agent**. Built with Rust + egui.

## How it works

- **You select**: freehand lasso, rect, or circle around any region (polygon
  mode with Ctrl+drag, Enter closes).
- **The agent edits**: lasso runs a local MCP server on
  `http://127.0.0.1:8756/mcp`. The document is shared — agent edits appear
  live in the window.
- **You can undo/redo** agent edits: Ctrl+Z / Ctrl+Shift+Z.

## Install

```sh
sudo pacman -S lasso   # from the Omarchy repository
```

Then register it with the omp agent (writes `~/.omp/agent/mcp.json`):

```sh
lasso register
```

Restart `omp` (or run `/mcp reload`) and the `lasso` tools appear.

## Agent tools

| Tool | Purpose |
|---|---|
| `get_state` | path, dimensions, selection, undo state |
| `open` | load an image (replaces document) |
| `selection_set_polygon` / `selection_set_rect` | set the lasso region |
| `apply_op` | fill, blur, brightness, invert, grayscale, pixelate, delete, crop — scoped to the selection, or whole image when none |
| `export_region` | base64 PNG of `full`, `selection_bbox` or `selection` |
| `save` | write canvas to disk (png/jpeg/webp/gif/bmp) |
| `undo` / `redo` / `reset` | revert edits |
| `ops` / `clear_selection` | discover ops / drop selection |

Typical agent flow: `get_state` → `open` → `selection_set_polygon` →
`apply_op {op: "blur", sigma: 6}` → `save`.

## Keys

| Input | Action |
|---|---|
| drag (left) | selection: freeform / rect / circle (toolbar) |
| Ctrl+drag, Enter | polygon lasso, close it |
| right-drag / scroll | pan / zoom (Ctrl+0 fit) |
| Ctrl+O | open an image |
| Ctrl+Z / Ctrl+Shift+Z | undo / redo agent edits |
| Escape | clear selection |

## Building

```sh
cargo build --release   # → target/release/lasso
cargo test
```

## License

MIT
