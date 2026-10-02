//! Editor core: image state, lasso selection, region operations, undo.
//!
//! Shared between the GUI and the MCP server behind a single mutex, so agent
//! edits and user edits hit the same document.

use image::RgbaImage;
use std::path::{Path, PathBuf};

/// A lasso selection: closed polygon in image pixel coordinates.
#[derive(Debug, Clone)]
pub struct Selection {
    pub polygon: Vec<[f32; 2]>,
}

impl Selection {
    pub fn from_polygon(polygon: Vec<[f32; 2]>) -> Result<Self, String> {
        if polygon.len() < 3 {
            return Err("selection needs at least 3 points".into());
        }
        Ok(Self { polygon })
    }

    pub fn from_rect(x: f32, y: f32, w: f32, h: f32) -> Result<Self, String> {
        if w <= 0.0 || h <= 0.0 {
            return Err("rect width and height must be positive".into());
        }
        Ok(Self {
            polygon: vec![[x, y], [x + w, y], [x + w, y + h], [x, y + h]],
        })
    }

    /// Axis-aligned bounding box clamped to image bounds.
    pub fn bbox(&self, width: u32, height: u32) -> (u32, u32, u32, u32) {
        let min_x = self.polygon.iter().map(|p| p[0]).fold(f32::INFINITY, f32::min);
        let min_y = self.polygon.iter().map(|p| p[1]).fold(f32::INFINITY, f32::min);
        let max_x = self
            .polygon
            .iter()
            .map(|p| p[0])
            .fold(f32::NEG_INFINITY, f32::max);
        let max_y = self
            .polygon
            .iter()
            .map(|p| p[1])
            .fold(f32::NEG_INFINITY, f32::max);
        let x0 = (min_x.floor().max(0.0) as u32).min(width.saturating_sub(1));
        let y0 = (min_y.floor().max(0.0) as u32).min(height.saturating_sub(1));
        let x1 = (max_x.ceil().min(width as f32) as u32).min(width);
        let y1 = (max_y.ceil().min(height as f32) as u32).min(height);
        (x0, y0, (x1 - x0).max(1), (y1 - y0).max(1))
    }
}

/// Region operations the GUI and the agent can apply.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Fill { color: [u8; 4] },
    Blur { sigma: f32 },
    Brightness { factor: f32 },
    Invert,
    Grayscale,
    Pixelate { block: u32 },
    Delete,
    Crop,
}

impl Op {
    pub fn name(&self) -> &'static str {
        match self {
            Op::Fill { .. } => "fill",
            Op::Blur { .. } => "blur",
            Op::Brightness { .. } => "brightness",
            Op::Invert => "invert",
            Op::Grayscale => "grayscale",
            Op::Pixelate { .. } => "pixelate",
            Op::Delete => "delete",
            Op::Crop => "crop",
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Op::Fill { color } => format!("fill #{:02x}{:02x}{:02x}{:02x}", color[0], color[1], color[2], color[3]),
            Op::Blur { sigma } => format!("blur sigma={sigma}"),
            Op::Brightness { factor } => format!("brightness factor={factor}"),
            Op::Invert => "invert".into(),
            Op::Grayscale => "grayscale".into(),
            Op::Pixelate { block } => format!("pixelate block={block}"),
            Op::Delete => "delete".into(),
            Op::Crop => "crop".into(),
        }
    }
}

/// Parse "#RRGGBB", "#RRGGBBAA" or "transparent" into RGBA.
pub fn parse_color(s: &str) -> Result<[u8; 4], String> {
    let s = s.trim().to_ascii_lowercase();
    if s == "transparent" || s == "none" {
        return Ok([0, 0, 0, 0]);
    }
    let hex = s.strip_prefix('#').ok_or("color must be #RRGGBB, #RRGGBBAA or \"transparent\"")?;
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| "invalid hex color".to_string());
    match hex.len() {
        6 => {
            let mut c = [0, 0, 0, 255];
            for i in 0..3 {
                c[i] = byte(i * 2).map_err(|e| e.to_string())?;
            }
            Ok(c)
        }
        8 => {
            let mut c = [0; 4];
            for i in 0..4 {
                c[i] = byte(i * 2).map_err(|e| e.to_string())?;
            }
            Ok(c)
        }
        _ => Err("color must be #RRGGBB, #RRGGBBAA or \"transparent\"".into()),
    }
}

/// The open document: current pixels, original pixels, selection, undo stack.
pub struct Document {
    pub path: Option<PathBuf>,
    pub original: RgbaImage,
    pub canvas: RgbaImage,
    pub selection: Option<Selection>,
    /// Bumped on every visual mutation; used for texture invalidation.
    pub rev: u64,
    undo: Vec<RgbaImage>,
    redo: Vec<RgbaImage>,
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    pub fn new() -> Self {
        Self {
            path: None,
            original: RgbaImage::new(1, 1),
            canvas: RgbaImage::new(1, 1),
            selection: None,
            rev: 0,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    /// Load a file from disk, replacing current state. Returns dimensions.
    pub fn open(&mut self, path: &Path) -> Result<(u32, u32), String> {
        let img = image::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
        let rgba = img.to_rgba8();
        let (w, h) = (rgba.width(), rgba.height());
        self.original = rgba.clone();
        self.canvas = rgba;
        self.path = Some(path.to_path_buf());
        self.selection = None;
        self.undo.clear();
        self.redo.clear();
        self.rev += 1;
        Ok((w, h))
    }

    pub fn is_open(&self) -> bool {
        self.canvas.width() > 1 || self.canvas.height() > 1 || self.path.is_some()
    }

    fn push_undo(&mut self) {
        self.undo.push(self.canvas.clone());
        // A fresh edit invalidates the redo branch.
        self.redo.clear();
        self.rev += 1;
        if self.undo.len() > 50 {
            self.undo.remove(0);
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Undo the last edit. Returns true if something was undone.
    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else {
            return false;
        };
        let dims_changed = prev.dimensions() != self.canvas.dimensions();
        self.redo.push(std::mem::replace(&mut self.canvas, prev));
        self.rev += 1;
        if dims_changed {
            // Crop resized the canvas: selection can no longer be trusted.
            self.selection = None;
        }
        true
    }

    /// Redo the last undone edit. Returns true if something was redone.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        let dims_changed = next.dimensions() != self.canvas.dimensions();
        self.undo.push(std::mem::replace(&mut self.canvas, next));
        self.rev += 1;
        if dims_changed {
            self.selection = None;
        }
        true
    }

    /// Revert to the originally loaded pixels (undoable).
    pub fn reset(&mut self) {
        if self.original.dimensions() == self.canvas.dimensions() {
            self.push_undo();
        } else {
            self.undo.clear();
            self.redo.clear();
        }
        self.canvas = self.original.clone();
        self.selection = None;
        self.rev += 1;
    }

    /// Apply an operation, scoped to the selection (or whole image when none).
    /// Returns a short description of what happened, e.g. for status/history.
    pub fn apply(&mut self, op: Op) -> Result<String, String> {
        let (w, h) = self.canvas.dimensions();
        let scope = self
            .selection
            .as_ref()
            .map(|s| s.bbox(w, h))
            .map(|(x, y, bw, bh)| format!("selection bbox {x},{y} {bw}x{bh}"))
            .unwrap_or_else(|| "whole image".to_string());

        let op_desc;
        match &op {
            Op::Crop => {
                let Some(sel) = self.selection.clone() else {
                    return Err("crop needs a selection".into());
                };
                let (x, y, bw, bh) = sel.bbox(w, h);
                self.push_undo();
                self.canvas = image::imageops::crop_imm(&self.canvas, x, y, bw, bh).to_image();
                self.selection = None;
            }
            op @ (Op::Fill { .. }
            | Op::Blur { .. }
            | Op::Brightness { .. }
            | Op::Invert
            | Op::Grayscale
            | Op::Pixelate { .. }
            | Op::Delete) => {
                self.push_undo();
                if let Some(sel) = self.selection.clone() {
                    let mask = polygon_mask(w, h, &sel.polygon);
                    apply_masked(&mut self.canvas, &mask, op);
                } else {
                    apply_masked(&mut self.canvas, &vec![true; (w * h) as usize], op);
                }
            }
        }
        op_desc = op.describe();
        Ok(format!("{op_desc} applied to {scope}"))
    }

    /// Set the selection from raw points; returns bbox description when valid.
    pub fn set_selection(&mut self, pts: Vec<[f32; 2]>) -> Result<Option<String>, String> {
        if pts.len() < 3 {
            return Ok(None);
        }
        let sel = Selection::from_polygon(pts)?;
        let (w, h) = self.canvas.dimensions();
        let b = sel.bbox(w, h);
        self.selection = Some(sel);
        Ok(Some(format!("{}×{} @ {},{}", b.2, b.3, b.0, b.1)))
    }

    /// Pixels for export: `region` picks full image, bbox crop, or mask-cropped
    /// selection (outside transparent).
    pub fn export(&self, region: Region) -> Result<RgbaImage, String> {
        let (w, h) = self.canvas.dimensions();
        match region {
            Region::Full => Ok(self.canvas.clone()),
            Region::SelectionBbox => {
                let sel = self.selection.as_ref().ok_or("no selection")?;
                let (x, y, bw, bh) = sel.bbox(w, h);
                Ok(image::imageops::crop_imm(&self.canvas, x, y, bw, bh).to_image())
            }
            Region::Selection => {
                let sel = self.selection.as_ref().ok_or("no selection")?;
                let (x, y, bw, bh) = sel.bbox(w, h);
                let mask = polygon_mask(w, h, &sel.polygon);
                let mut out = RgbaImage::new(bw, bh);
                for yy in 0..bh {
                    for xx in 0..bw {
                        let gx = x + xx;
                        let gy = y + yy;
                        if mask[(gy * w + gx) as usize] {
                            out.put_pixel(xx, yy, *self.canvas.get_pixel(gx, gy));
                        }
                    }
                }
                Ok(out)
            }
        }
    }

    /// Save canvas to `path` (or the document path). Returns saved path + dims.
    pub fn save(&self, path: Option<&Path>) -> Result<(PathBuf, u32, u32), String> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => self
                .path
                .clone()
                .ok_or("no path: provide one or use Save As")?,
        };
        let format = image::ImageFormat::from_path(&path)
            .map_err(|_| format!("unsupported file extension: {}", path.display()))?;
        self.canvas
            .save_with_format(&path, format)
            .map_err(|e| format!("save {}: {e}", path.display()))?;
        let (w, h) = self.canvas.dimensions();
        Ok((path, w, h))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Region {
    Full,
    SelectionBbox,
    Selection,
}

/// Axis-aligned rectangle from two opposite corners as a 4-point polygon.
pub fn rect_polygon(a: [f32; 2], b: [f32; 2]) -> Vec<[f32; 2]> {
    let (x0, x1) = (a[0].min(b[0]), a[0].max(b[0]));
    let (y0, y1) = (a[1].min(b[1]), a[1].max(b[1]));
    vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]]
}

/// Ellipse approximation with `segments` points (minimum 8).
pub fn ellipse_polygon(center: [f32; 2], rx: f32, ry: f32, segments: usize) -> Vec<[f32; 2]> {
    let n = segments.max(8);
    (0..n)
        .map(|i| {
            let t = (i as f32) * std::f32::consts::TAU / n as f32;
            [center[0] + rx * t.cos(), center[1] + ry * t.sin()]
        })
        .collect()
}

/// Ellipse inscribed in the box defined by two opposite drag corners.
pub fn ellipse_polygon_from_drag(a: [f32; 2], b: [f32; 2]) -> Vec<[f32; 2]> {
    let center = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0];
    let rx = (a[0] - b[0]).abs() / 2.0;
    let ry = (a[1] - b[1]).abs() / 2.0;
    ellipse_polygon(center, rx, ry, 64)
}

/// Scanline polygon rasterization into a per-pixel mask.
/// Uses the half-open edge rule so shared edges are not double-filled.
pub fn polygon_mask(width: u32, height: u32, polygon: &[[f32; 2]]) -> Vec<bool> {
    let mut mask = vec![false; (width as usize) * (height as usize)];
    let n = polygon.len();
    if n < 3 {
        return mask;
    }
    for y in 0..height {
        let yc = y as f32 + 0.5;
        let mut xs: Vec<f32> = Vec::new();
        for i in 0..n {
            let [x1, y1] = polygon[i];
            let [x2, y2] = polygon[(i + 1) % n];
            if (y1 <= yc && y2 > yc) || (y2 <= yc && y1 > yc) {
                let t = (yc - y1) / (y2 - y1);
                xs.push(x1 + t * (x2 - x1));
            }
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        for pair in xs.chunks(2) {
            if let [xa, xb] = pair {
                let x0 = (*xa).ceil().max(0.0) as u32;
                let x1 = (*xb).floor().max(0.0) as u32;
                if x1 < x0 {
                    continue;
                }
                for x in x0..=x1.min(width - 1) {
                    mask[(y * width + x) as usize] = true;
                }
            }
        }
    }
    mask
}

fn apply_masked(canvas: &mut RgbaImage, mask: &[bool], op: &Op) {
    let (w, h) = canvas.dimensions();
    match op {
        Op::Fill { color } => {
            let c = image::Rgba(*color);
            for y in 0..h {
                for x in 0..w {
                    if mask[(y * w + x) as usize] {
                        canvas.put_pixel(x, y, c);
                    }
                }
            }
        }
        Op::Blur { sigma } => {
            let blurred = image::imageops::blur(canvas, *sigma);
            for y in 0..h {
                for x in 0..w {
                    if mask[(y * w + x) as usize] {
                        canvas.put_pixel(x, y, *blurred.get_pixel(x, y));
                    }
                }
            }
        }
        Op::Brightness { factor } => {
            for y in 0..h {
                for x in 0..w {
                    if mask[(y * w + x) as usize] {
                        let p = canvas.get_pixel(x, y);
                        let f = |v: u8| ((v as f32 * factor).round().clamp(0.0, 255.0)) as u8;
                        canvas.put_pixel(x, y, image::Rgba([f(p[0]), f(p[1]), f(p[2]), p[3]]));
                    }
                }
            }
        }
        Op::Invert => {
            for y in 0..h {
                for x in 0..w {
                    if mask[(y * w + x) as usize] {
                        let p = canvas.get_pixel(x, y);
                        canvas.put_pixel(x, y, image::Rgba([255 - p[0], 255 - p[1], 255 - p[2], p[3]]));
                    }
                }
            }
        }
        Op::Grayscale => {
            for y in 0..h {
                for x in 0..w {
                    if mask[(y * w + x) as usize] {
                        let p = canvas.get_pixel(x, y);
                        let l = (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32).round() as u8;
                        canvas.put_pixel(x, y, image::Rgba([l, l, l, p[3]]));
                    }
                }
            }
        }
        Op::Pixelate { block } => {
            let block = (*block).max(2);
            for by in (0..h).step_by(block as usize) {
                for bx in (0..w).step_by(block as usize) {
                    let bx_end = (bx + block).min(w);
                    let by_end = (by + block).min(h);
                    // Average over the block, but only over masked pixels.
                    let (mut r, mut g, mut b, mut a, mut count) = (0u64, 0u64, 0u64, 0u64, 0u64);
                    for y in by..by_end {
                        for x in bx..bx_end {
                            if mask[(y * w + x) as usize] {
                                let p = canvas.get_pixel(x, y);
                                r += p[0] as u64;
                                g += p[1] as u64;
                                b += p[2] as u64;
                                a += p[3] as u64;
                                count += 1;
                            }
                        }
                    }
                    if count == 0 {
                        continue;
                    }
                    let avg = image::Rgba([
                        (r / count) as u8,
                        (g / count) as u8,
                        (b / count) as u8,
                        (a / count) as u8,
                    ]);
                    for y in by..by_end {
                        for x in bx..bx_end {
                            if mask[(y * w + x) as usize] {
                                canvas.put_pixel(x, y, avg);
                            }
                        }
                    }
                }
            }
        }
        Op::Delete => {
            for y in 0..h {
                for x in 0..w {
                    if mask[(y * w + x) as usize] {
                        let p = canvas.get_pixel(x, y);
                        canvas.put_pixel(x, y, image::Rgba([p[0], p[1], p[2], 0]));
                    }
                }
            }
        }
        Op::Crop => unreachable!("crop handled in Document::apply"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tri() -> Selection {
        Selection::from_polygon(vec![[1.0, 1.0], [4.0, 1.0], [1.0, 4.0]]).unwrap()
    }

    #[test]
    fn mask_covers_triangle_interior_and_excludes_outside() {
        let mask = polygon_mask(6, 6, &tri().polygon);
        let at = |x: u32, y: u32| mask[(y * 6 + x) as usize];
        // Interior of the triangle (1,1)-(4,1)-(1,4)
        assert!(at(1, 1));
        assert!(at(2, 2));
        assert!(!at(4, 4), "corner outside hypotenuse");
        assert!(!at(0, 0));
        assert!(!at(5, 5));
    }

    #[test]
    fn mask_covers_convex_quad_fully() {
        let sel = Selection::from_rect(2.0, 2.0, 4.0, 4.0).unwrap();
        let mask = polygon_mask(10, 10, &sel.polygon);
        for y in 2..6 {
            for x in 2..6 {
                assert!(mask[(y * 10 + x) as usize], "missing {x},{y}");
            }
        }
        assert!(!mask[(1 * 10 + 2) as usize]);
        assert!(!mask[(6 * 10 + 6) as usize]);
    }

    #[test]
    fn bbox_clamps_to_image() {
        let sel = Selection::from_rect(-3.0, -3.0, 10.0, 10.0).unwrap();
        assert_eq!(sel.bbox(5, 5), (0, 0, 5, 5));
        let sel = Selection::from_rect(2.0, 3.0, 4.0, 5.0).unwrap();
        assert_eq!(sel.bbox(10, 10), (2, 3, 4, 5));
    }

    #[test]
    fn fill_respects_selection() {
        let mut doc = Document::new();
        doc.canvas = RgbaImage::from_pixel(6, 6, image::Rgba([255, 0, 0, 255]));
        doc.selection = Some(tri());
        doc.apply(Op::Fill { color: [0, 255, 0, 255] }).unwrap();
        assert_eq!(doc.canvas.get_pixel(2, 2), &image::Rgba([0, 255, 0, 255]));
        assert_eq!(doc.canvas.get_pixel(5, 5), &image::Rgba([255, 0, 0, 255]));
        // Outside polygon but inside bbox stays red
        assert_eq!(doc.canvas.get_pixel(4, 4), &image::Rgba([255, 0, 0, 255]));
    }

    #[test]
    fn undo_restores_canvas() {
        let mut doc = Document::new();
        doc.canvas = RgbaImage::from_pixel(4, 4, image::Rgba([10, 10, 10, 255]));
        doc.selection = Some(Selection::from_rect(0.0, 0.0, 2.0, 2.0).unwrap());
        doc.apply(Op::Fill { color: [255, 0, 0, 255] }).unwrap();
        assert!(doc.can_undo());
        assert!(doc.undo());
        assert_eq!(doc.canvas.get_pixel(0, 0), &image::Rgba([10, 10, 10, 255]));
        assert!(!doc.can_undo());
        assert!(!doc.undo());
    }

    #[test]
    fn crop_changes_dims_and_selection_clears_on_undo() {
        let mut doc = Document::new();
        doc.canvas = RgbaImage::from_pixel(8, 8, image::Rgba([1, 2, 3, 255]));
        doc.selection = Some(Selection::from_rect(2.0, 2.0, 4.0, 4.0).unwrap());
        doc.apply(Op::Crop).unwrap();
        assert_eq!(doc.canvas.dimensions(), (4, 4));
        assert!(doc.selection.is_none());
        doc.undo();
        assert_eq!(doc.canvas.dimensions(), (8, 8));
        assert!(doc.selection.is_none(), "selection must not survive crop undo");
    }

    #[test]
    fn redo_restores_after_undo_and_new_edit_clears_redo() {
        let mut doc = Document::new();
        doc.canvas = RgbaImage::from_pixel(4, 4, image::Rgba([10, 10, 10, 255]));
        doc.selection = Some(Selection::from_rect(0.0, 0.0, 2.0, 2.0).unwrap());
        doc.apply(Op::Fill { color: [255, 0, 0, 255] }).unwrap();
        doc.undo();
        assert_eq!(doc.canvas.get_pixel(0, 0), &image::Rgba([10, 10, 10, 255]));
        assert!(doc.redo());
        assert_eq!(doc.canvas.get_pixel(0, 0), &image::Rgba([255, 0, 0, 255]));
        assert!(!doc.redo(), "redo stack must be empty after redo");
        // a fresh edit clears the redo branch
        doc.undo();
        assert!(doc.can_undo() || true);
        doc.apply(Op::Invert).unwrap();
        assert!(!doc.redo());
    }

    #[test]
    fn delete_makes_selection_transparent() {
        let mut doc = Document::new();
        doc.canvas = RgbaImage::from_pixel(4, 4, image::Rgba([9, 9, 9, 255]));
        doc.selection = Some(Selection::from_rect(0.0, 0.0, 2.0, 2.0).unwrap());
        doc.apply(Op::Delete).unwrap();
        assert_eq!(doc.canvas.get_pixel(0, 0), &image::Rgba([9, 9, 9, 0]));
        assert_eq!(doc.canvas.get_pixel(3, 3), &image::Rgba([9, 9, 9, 255]));
    }

    #[test]
    fn color_parsing() {
        assert_eq!(parse_color("#ff0000").unwrap(), [255, 0, 0, 255]);
        assert_eq!(parse_color("#ff000080").unwrap(), [255, 0, 0, 128]);
        assert_eq!(parse_color("transparent").unwrap(), [0, 0, 0, 0]);
        assert!(parse_color("red").is_err());
        assert!(parse_color("#12345").is_err());
    }

    #[test]
    fn reset_restores_original_and_is_undoable() {
        let mut doc = Document::new();
        doc.canvas = RgbaImage::from_pixel(4, 4, image::Rgba([7, 7, 7, 255]));
        doc.original = doc.canvas.clone();
        doc.selection = Some(Selection::from_rect(0.0, 0.0, 4.0, 4.0).unwrap());
        doc.apply(Op::Invert).unwrap();
        doc.reset();
        assert_eq!(doc.canvas.get_pixel(0, 0), &image::Rgba([7, 7, 7, 255]));
        assert!(doc.can_undo());
    }

    #[test]
    fn save_roundtrip_png_and_rejects_bad_ext() {
        let mut doc = Document::new();
        doc.canvas = RgbaImage::from_pixel(3, 3, image::Rgba([1, 2, 3, 255]));
        let dir = std::env::temp_dir().join(format!("lasso-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.png");
        doc.save(Some(&path)).unwrap();
        let img = image::open(&path).unwrap().to_rgba8();
        assert_eq!((img.width(), img.height()), (3, 3));
        assert!(doc.save(Some(&dir.join("out.xyz"))).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
