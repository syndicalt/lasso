---
name: lasso
description: >
  REQUIRED for editing the image open in the lasso editor (lasso photo editor
  for Omarchy). Use when the user asks to edit, retouch, annotate, composite,
  crop, resize, fill, blur, pixelate or otherwise modify the image currently
  shown in lasso, or to make selections in it. The lasso MCP tools are the
  ONLY way to edit the document shown in the lasso window - never use Pillow,
  ImageMagick or similar to modify the file behind lasso's back: the window
  holds the live pixels and external edits force a reload and break undo/redo.
---

# Lasso Skill

Lasso is a selection-only image editor: the human draws lasso selections
(freeform / rect / circle), the agent does ALL pixel editing through the MCP
tools below. The document is shared and live — every tool call instantly
appears in the window.

## Tool inventory

| Tool | Purpose |
|---|---|
| `get_state` | path, canvas size, selection, can_undo/can_redo. Call FIRST. |
| `open` | Load an image file. Same-size reloads keep undo history. |
| `selection_set_polygon` | Set the lasso region from points, e.g. `"10,20 40,22 35,60"` |
| `selection_set_rect` | Set the lasso region from x, y, width, height |
| `clear_selection` | Drop the selection so ops hit the whole image |
| `apply_op` | Edit pixels (ops below); scopes to selection when set |
| `paste_image` | Composite a base64 PNG at x,y — the general drawing tool |
| `paste_file` | Composite an image file at x,y (e.g. a render you made) |
| `export_region` | Base64 PNG of `full`, `selection_bbox` or `selection` — LOOK at your result |
| `save` | Write canvas to disk (png/jpeg/webp/gif/bmp by extension) |
| `undo` / `redo` | Step history (also Ctrl+Z / Ctrl+Shift+Z in the window) |
| `reset` | Revert to the originally opened pixels (undoable) |
| `resize_canvas` | Grow/shrink canvas with anchor + fill |
| `ops` | Machine-readable op list |

## apply_op operations

| op | args | notes |
|---|---|---|
| `fill` | `color`: `#RRGGBB`, `#RRGGBBAA`, `"transparent"` | flat color |
| `blur` | `sigma` (default 4.0) | gaussian |
| `brightness` | `factor` (default 1.2; <1 darker, >1 brighter) | |
| `invert` | — | |
| `grayscale` | — | |
| `pixelate` | `block` px (default 16) | |
| `delete` | — | makes the region transparent |
| `crop` | — | canvas becomes the selection bbox; requires selection |

All ops use the current selection polygon when one is set, else the whole
image. Selection state is NOT cleared by ops (crop is the exception).

## The golden workflow

1. `get_state` — what's open, how big, what's selected.
2. `selection_set_polygon` or `selection_set_rect` — scope the edit. The user
   may have drawn one already: get_state tells you.
3. Edit (`apply_op`, `paste_image`, `paste_file`, ...).
4. `export_region {region: "selection_bbox"}` — view the PNG to check the
   result before saving. Iterate if wrong; `undo` is cheap.
5. `save` (in place) — only when the result is right.

## Recipes

- **Remove an object**: select it, `apply_op {op: "fill", color: "#22242a"}`
  with the sampled background (export a nearby strip first to sample), or
  paste a patch you generated: `paste_image {x, y, within_selection: true}`.
- **Annotate**: draw the arrow/text/sticker as a PNG (any renderer), then
  `paste_image {x, y}` at the right spot. Keep text large and high-contrast.
- **Composite a generated image**: `paste_file {path, x, y, blend: true}`;
  use `within_selection: true` to confine it to the lasso region. The image
  must be transparent outside the subject (see Rules); if the background under
  the subject needs to change, inpaint it in a separate paste first.
- **Extend canvas**: `resize_canvas {width, height, anchor: "center", fill: "transparent"}`
  then paste new content into the new area.
- **Vignette / soft effects**: `selection_set_polygon` with a coarse polygon
  over the region, `apply_op {op: "blur", sigma: 8}` — mask edges follow the
  polygon exactly (no feather). For feathering, paste a pre-feathered patch
  with `blend: true` instead.

## Rules

- **No backing plates.** When compositing generated content (`paste_image` /
  `paste_file`), the pasted PNG must be **transparent outside the subject** —
  never an opaque rectangle, circle, or "staged background" behind it. If the
  original pixels under the subject must change (removed background, new
  scenery), inpaint those pixels in a separate paste matched to the surrounding
  photo, and keep the subject on its own transparent layer. A visible box/halo
  in the shape of the selection after an edit is a bug.
- NEVER edit the file behind lasso's back (Pillow/ImageMagick on the open
  path) — the window shows stale pixels and undo/redo desync. If an external
  tool already modified it, `open` the same path to reload; same-size reloads
  preserve history.
- `paste_image`/`paste_file`/`apply_op`/`resize_canvas`/`reset` all push undo
  states — prefer iterating with undo over generating from scratch.
- `export_region` is your eyes: region `"selection"` returns exact-polygon
  pixels with transparent outside; `"selection_bbox"` the bounding box.
- Coordinates are image pixels, y-down, top-left origin. Negative/overflowing
  paste coordinates are clipped, not errors.
- `save` with no path writes to the opened file. Use a new path to produce a
  derived image and leave the original untouched.
- The user can undo/redo/escape at any time; re-run `get_state` after any
  human interaction before continuing.
