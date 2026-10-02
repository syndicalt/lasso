//! egui frontend: selection-only canvas (freeform lasso, rect, circle),
//! zoom/pan, minimal toolbar, status bar showing agent activity.
//!
//! All pixel editing happens agent-side via the MCP tools; the GUI's job is
//! drawing selections and showing the live document.

use crate::core::{ellipse_polygon_from_drag, rect_polygon};
use crate::mcp::SharedState;
use eframe::egui;
use egui::{Color32, ColorImage, Key, PointerButton, Rect, Sense, TextureHandle, Vec2};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SelMode {
    Freeform,
    Rect,
    Circle,
}

pub struct LassoApp {
    state: SharedState,
    texture: Option<TextureHandle>,
    rendered_rev: u64,
    // view transform: screen = img * zoom + base + pan
    zoom: f32,
    pan: Vec2,
    fit_needed: bool,
    last_canvas_dims: (u32, u32),
    // selection interaction
    mode: SelMode,
    freehand: Vec<[f32; 2]>,
    drag_start: Option<[f32; 2]>,
    drag_current: Option<[f32; 2]>,
    dragging: bool,
    panning: bool,
    pending_polygon: Vec<[f32; 2]>,
    // ui state
    status: String,
    seen_log_len: usize,
    mcp_note: Option<String>,
    error: Option<String>,
}

impl LassoApp {
    pub fn new(state: SharedState, mcp_note: Option<String>) -> Self {
        Self {
            state,
            texture: None,
            rendered_rev: u64::MAX,
            zoom: 1.0,
            pan: Vec2::ZERO,
            fit_needed: true,
            last_canvas_dims: (0, 0),
            mode: SelMode::Freeform,
            freehand: Vec::new(),
            drag_start: None,
            drag_current: None,
            dragging: false,
            panning: false,
            pending_polygon: Vec::new(),
            status: "Open an image, or ask your agent: the MCP endpoint is shared.".into(),
            seen_log_len: 0,
            mcp_note,
            error: None,
        }
    }

    pub fn preload(&mut self, path: &std::path::Path) {
        match self.state.document.lock().open(path) {
            Ok(_) => {
                self.fit_needed = true;
                self.status = format!("Opened {}", path.display());
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn set_status(&mut self, s: impl Into<String>) {
        self.status = s.into();
    }

    /// Screen-space rect where the image is drawn.
    fn image_rect(&self, panel_min: egui::Pos2, panel_size: Vec2, img: Vec2) -> Rect {
        let scaled = Vec2::new(img.x * self.zoom, img.y * self.zoom);
        let base = panel_min + (panel_size - scaled) * 0.5 + self.pan;
        Rect::from_min_size(base, scaled)
    }

    fn zoom_at(&mut self, factor: f32, pointer: Option<egui::Pos2>, panel_min: egui::Pos2, panel_size: Vec2, img: Vec2) {
        let old = self.zoom;
        self.zoom = (self.zoom * factor).clamp(0.02, 64.0);
        if self.zoom == old {
            return;
        }
        if let Some(p) = pointer {
            // keep the image point under the cursor fixed
            let rect = self.image_rect(panel_min, panel_size, img);
            let ix = (p.x - rect.min.x) / old;
            let iy = (p.y - rect.min.y) / old;
            let new_rect = self.image_rect(panel_min, panel_size, img);
            self.pan += Vec2::new(new_rect.min.x + ix * self.zoom, new_rect.min.y + iy * self.zoom) - p.to_vec2();
        }
    }

    fn fit(&mut self, panel_size: Vec2, img: Vec2) {
        if img.x <= 0.0 || img.y <= 0.0 {
            return;
        }
        let zx = (panel_size.x / img.x).min(panel_size.y / img.y);
        self.zoom = zx.clamp(0.02, 64.0);
        self.pan = Vec2::ZERO;
        self.fit_needed = false;
    }

    fn sync_texture(&mut self, ctx: &egui::Context) {
        let (rev, canvas) = {
            let doc = self.state.document.lock();
            (doc.rev, doc.canvas.clone())
        };
        let dims = canvas.dimensions();
        if self.texture.is_none() || rev != self.rendered_rev || dims != self.last_canvas_dims {
            let size = [dims.0 as usize, dims.1 as usize];
            let pixels = canvas.into_raw();
            let image = ColorImage::from_rgba_unmultiplied(size, &pixels);
            if let Some(tex) = self.texture.as_mut() {
                tex.set(image, Default::default());
            } else {
                self.texture = Some(ctx.load_texture("canvas", image, Default::default()));
            }
            self.rendered_rev = rev;
            if dims != self.last_canvas_dims {
                self.fit_needed = true;
                self.last_canvas_dims = dims;
            }
        }
    }

    /// Commit a finished selection (any mode). Empty/invalid input is ignored.
    fn commit_selection(&mut self, pts: Vec<[f32; 2]>) {
        let result = self.state.document.lock().set_selection(pts);
        match result {
            Ok(Some(b)) => self.set_status(format!("Selection: {b}")),
            Ok(None) => {}
            Err(e) => self.error = Some(e),
        }
    }
}

impl eframe::App for LassoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // --- global shortcuts & dnd -----------------------------------------
        // The GUI never edits pixels: humans select, save, and undo/redo
        // agent edits. All pixel edits belong to the agent via MCP.
        let mut open_path: Option<std::path::PathBuf> = None;
        let mut save = false;
        let mut do_undo = false;
        let mut do_redo = false;
        let mut clear_sel = false;
        let mut fit_view = false;
        let mut close_polygon = false;

        ctx.input(|i| {
            if i.modifiers.command && i.key_pressed(Key::O) {
                open_path = rfd::FileDialog::new()
                    .add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif", "bmp"])
                    .pick_file();
            }
            if i.modifiers.command && i.key_pressed(Key::S) {
                save = true;
            }
            if i.modifiers.command && i.key_pressed(Key::Z) {
                if i.modifiers.shift {
                    do_redo = true;
                } else {
                    do_undo = true;
                }
            }
            if i.key_pressed(Key::Escape) {
                clear_sel = true;
            }
            if i.modifiers.command && i.key_pressed(Key::Num0) {
                fit_view = true;
            }
            if i.key_pressed(Key::Enter) {
                close_polygon = true;
            }
            for f in &i.raw.dropped_files {
                open_path = Some(f.path().to_path_buf());
            }
        });

        if do_undo {
            if self.state.document.lock().undo() {
                self.set_status("Undo");
            }
        }
        if do_redo {
            if self.state.document.lock().redo() {
                self.set_status("Redo");
            }
        }
        if clear_sel {
            self.state.document.lock().selection = None;
            self.pending_polygon.clear();
            self.set_status("Selection cleared");
        }

        // --- poll agent log ---------------------------------------------------
        {
            let log = self.state.log.lock();
            let changed = log.len() != self.seen_log_len;
            let latest = log.last().cloned();
            drop(log);
            if changed {
                if let Some(entry) = latest {
                    self.set_status(format!("agent · {entry}"));
                }
                self.seen_log_len = self.state.log.lock().len();
            }
        }

        // --- top toolbar: selection mode + file ops (editing is the agent's job) --
        egui::Panel::top("toolbar").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Open").clicked() {
                    open_path = rfd::FileDialog::new()
                        .add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif", "bmp"])
                        .pick_file();
                }
                if ui.button("Save").clicked() {
                    save = true;
                }
                ui.separator();
                ui.selectable_value(&mut self.mode, SelMode::Freeform, "Freeform");
                ui.selectable_value(&mut self.mode, SelMode::Rect, "Rect");
                ui.selectable_value(&mut self.mode, SelMode::Circle, "Circle");
            });
        });

        // --- status bar ----------------------------------------------------------
        egui::Panel::bottom("status").show_inside(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let doc = self.state.document.lock();
                let dims = doc.canvas.dimensions();
                if let Some(path) = &doc.path {
                    ui.label(path.display().to_string());
                } else {
                    ui.weak("no file");
                }
                ui.separator();
                ui.weak(format!("{}×{}", dims.0, dims.1));
                ui.weak(format!("{:.0}%", self.zoom * 100.0));
                if let Some(sel) = &doc.selection {
                    let (x, y, w, h) = sel.bbox(dims.0, dims.1);
                    ui.weak(format!("sel: {} pts, {}×{} @ {},{}", sel.polygon.len(), w, h, x, y));
                }
                drop(doc);
                ui.separator();
                if let Some(note) = &self.mcp_note {
                    ui.colored_label(Color32::YELLOW, format!("mcp: {note}"));
                } else {
                    ui.weak(format!("mcp: {}", crate::mcp::ENDPOINT));
                }
                ui.separator();
                ui.label(&self.status);
                if let Some(err) = &self.error {
                    ui.colored_label(Color32::RED, err);
                }
            });
        });

        // --- canvas ---------------------------------------------------------------
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show_inside(ui, |ui| {
                let (panel_min, panel_size) = (ui.cursor().min, ui.available_size());
                self.sync_texture(&ctx);
                let dims = self.last_canvas_dims;
                let img_size = Vec2::new(dims.0 as f32, dims.1 as f32);

                if self.fit_needed && img_size.x > 1.0 {
                    self.fit(panel_size, img_size);
                }
                if fit_view {
                    self.fit(panel_size, img_size);
                }

                if img_size.x > 1.0 {
                    let rect = self.image_rect(panel_min, panel_size, img_size);
                    let response = ui.allocate_rect(rect, Sense::click_and_drag());

                    // backdrop for transparency
                    ui.painter().rect_filled(rect, 0, Color32::from_gray(40));

                    if let Some(tex) = &self.texture {
                        ui.painter().image(
                            tex.id(),
                            rect,
                            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    }

                    // overlays: committed selection, in-progress drag, polygon
                    let to_screen = |p: [f32; 2]| rect.min + Vec2::new(p[0], p[1]) * self.zoom;
                    let sel_stroke = egui::Stroke::new(1.5, Color32::from_rgb(80, 220, 255));
                    let wip_stroke = egui::Stroke::new(1.5, Color32::YELLOW);
                    {
                        let doc = self.state.document.lock();
                        if let Some(sel) = &doc.selection {
                            let mut pts: Vec<egui::Pos2> = sel.polygon.iter().map(|&p| to_screen(p)).collect();
                            if pts.len() > 2 {
                                if let Some(first) = pts.first() {
                                    pts.push(*first);
                                }
                                ui.painter().add(egui::Shape::line(pts, sel_stroke));
                            }
                        }
                    }
                    if self.freehand.len() > 1 {
                        let pts: Vec<egui::Pos2> = self.freehand.iter().map(|&p| to_screen(p)).collect();
                        ui.painter().add(egui::Shape::line(pts, wip_stroke));
                    }
                    if let (Some(a), Some(b)) = (self.drag_start, self.drag_current) {
                        let poly = match self.mode {
                            SelMode::Rect => rect_polygon(a, b),
                            SelMode::Circle => ellipse_polygon_from_drag(a, b),
                            SelMode::Freeform => Vec::new(),
                        };
                        if !poly.is_empty() {
                            let pts: Vec<egui::Pos2> = poly.iter().map(|&p| to_screen(p)).collect();
                            ui.painter().add(egui::Shape::line(pts, wip_stroke));
                        }
                    }
                    if self.pending_polygon.len() > 1 {
                        let pts: Vec<egui::Pos2> = self.pending_polygon.iter().map(|&p| to_screen(p)).collect();
                        ui.painter().add(egui::Shape::line(pts, wip_stroke));
                    }

                    // ---- interactions ----
                    let to_img = |p: egui::Pos2| -> [f32; 2] {
                        [(p.x - rect.min.x) / self.zoom, (p.y - rect.min.y) / self.zoom]
                    };

                    if response.drag_started_by(PointerButton::Secondary) {
                        self.panning = true;
                    }
                    if response.drag_stopped_by(PointerButton::Secondary) {
                        self.panning = false;
                    }
                    if self.panning {
                        self.pan += response.drag_delta();
                    }

                    let ctrl = ctx.input(|i| i.modifiers.ctrl);
                    if ctrl {
                        // polygon mode: click adds vertices, Enter closes
                        if response.clicked() {
                            if let Some(p) = response.interact_pointer_pos() {
                                self.pending_polygon.push(to_img(p));
                            }
                        }
                        if close_polygon {
                            let poly = std::mem::take(&mut self.pending_polygon);
                            self.commit_selection(poly);
                        }
                    } else {
                        match self.mode {
                            SelMode::Freeform => {
                                if response.drag_started_by(PointerButton::Primary) {
                                    self.dragging = true;
                                    self.freehand.clear();
                                    if let Some(p) = response.interact_pointer_pos() {
                                        self.freehand.push(to_img(p));
                                    }
                                }
                                if self.dragging && response.dragged_by(PointerButton::Primary) {
                                    if let Some(p) = response.interact_pointer_pos() {
                                        let img_pt = to_img(p);
                                        if self.freehand.last() != Some(&img_pt) {
                                            self.freehand.push(img_pt);
                                        }
                                    }
                                }
                                if response.drag_stopped_by(PointerButton::Primary) && self.dragging {
                                    self.dragging = false;
                                    let pts = std::mem::take(&mut self.freehand);
                                    self.commit_selection(pts);
                                }
                            }
                            SelMode::Rect | SelMode::Circle => {
                                if response.drag_started_by(PointerButton::Primary) {
                                    self.dragging = true;
                                    if let Some(p) = response.interact_pointer_pos() {
                                        self.drag_start = Some(to_img(p));
                                    }
                                }
                                if self.dragging && response.dragged_by(PointerButton::Primary) {
                                    if let Some(p) = response.interact_pointer_pos() {
                                        self.drag_current = Some(to_img(p));
                                    }
                                }
                                if response.drag_stopped_by(PointerButton::Primary) && self.dragging {
                                    self.dragging = false;
                                    if let (Some(a), Some(b)) = (self.drag_start, self.drag_current) {
                                        let poly = match self.mode {
                                            SelMode::Rect => rect_polygon(a, b),
                                            _ => ellipse_polygon_from_drag(a, b),
                                        };
                                        self.commit_selection(poly);
                                    }
                                    self.drag_start = None;
                                    self.drag_current = None;
                                }
                            }
                        }
                    }

                    // zoom with scroll, anchored at pointer
                    let scroll = ctx.input(|i| i.smooth_scroll_delta.y);
                    if scroll != 0.0 && response.hovered() {
                        let factor = if scroll > 0.0 { 1.1 } else { 1.0 / 1.1 };
                        let pointer = response.hover_pos();
                        self.zoom_at(factor, pointer, panel_min, panel_size, img_size);
                    }
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label("Drag & drop an image, press Ctrl+O, or ask your agent to open one.");
                        ui.weak(format!("MCP endpoint: {}", crate::mcp::ENDPOINT));
                    });
                }
            });

        // --- save (writes the agent-edited document to disk) -----------------------
        if save {
            let has_path = self.state.document.lock().path.is_some();
            let target = if has_path {
                None
            } else {
                rfd::FileDialog::new().set_file_name("out.png").save_file()
            };
            if has_path || target.is_some() {
                let result = self.state.document.lock().save(target.as_deref());
                match result {
                    Ok((p, w, h)) => self.set_status(format!("Saved {} ({}×{})", p.display(), w, h)),
                    Err(e) => self.error = Some(e),
                }
            }
        }

        if let Some(path) = open_path {
            self.preload(&path);
        }

        // repaint while dragging to keep overlays smooth
        if self.dragging || self.panning {
            ctx.request_repaint();
        }
    }
}
