//! egui frontend: selection-only canvas (freeform/rect/circle lasso).
//!
//! UX:
//! - auto-hide tool sidebar on the left edge (mouse-over to reveal)
//! - scroll = cursor-anchored zoom; Ctrl +/- = keyboard zoom; Ctrl+0 fit,
//!   Ctrl+1 = 100%
//! - pan: right-drag, Shift+drag, Shift+arrows
//! - undo/redo buttons + Ctrl+Z / Ctrl+Shift+Z; Ctrl+D deselect
//! - "?" or F1 shows the shortcut list overlay
//!
//! All pixel editing happens agent-side via the MCP tools; the GUI selects,
//! saves, and undo/redo's agent edits.

use crate::core::{ellipse_polygon_from_drag, rect_polygon};
use crate::mcp::SharedState;
use eframe::egui;
use egui::{Align2, Color32, ColorImage, FontId, Key, PointerButton, Rect, Sense, TextureHandle, Vec2};

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
    /// Sticky: the current primary-drag gesture is a pan (Space touched it).
    gesture_is_pan: bool,
    /// Last raw pointer position while panning, for delta computation.
    pan_last_pos: Option<egui::Pos2>,
    pending_polygon: Vec<[f32; 2]>,
    // ui state
    sidebar_open: bool,
    show_help: bool,
    status: String,
    seen_log_len: usize,
    mcp_note: Option<String>,
    error: Option<String>,
}

const SIDEBAR_WIDTH: f32 = 84.0;
const EDGE_HOTZONE: f32 = 10.0;
const BTN_SIZE: Vec2 = Vec2::new(64.0, 26.0);

/// Uniform-width icon button with a hover tooltip.
fn icon_button(ui: &mut egui::Ui, icon: &str, selected: bool, tooltip: &str) -> egui::Response {
    let text = egui::RichText::new(icon).size(14.0);
    let response = if selected {
        ui.add_sized(BTN_SIZE, egui::Button::new(text).fill(Color32::from_rgb(52, 86, 128)))
    } else {
        ui.add_sized(BTN_SIZE, egui::Button::new(text))
    };
    response.on_hover_text(tooltip)
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
            gesture_is_pan: false,
            pan_last_pos: None,
            pending_polygon: Vec::new(),
            sidebar_open: false,
            show_help: false,
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
        if factor < 1.0 {
            // Zooming out always recenters the image in the workspace.
            self.pan = Vec2::ZERO;
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

    fn set_zoom_percent(&mut self, target: f32, pointer: Option<egui::Pos2>, panel_min: egui::Pos2, panel_size: Vec2, img: Vec2) {
        let target = target.clamp(2.0, 6400.0) / 100.0;
        let factor = target / self.zoom;
        self.zoom_at(factor, pointer, panel_min, panel_size, img);
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
        let mut zoom_100 = false;
        let mut zoom_in = false;
        let mut zoom_out = false;
        let mut close_polygon = false;
        let mut pan_delta = Vec2::ZERO;
        let mut toggle_help = false;

        ctx.input(|i| {
            let cmd = i.modifiers.command;
            if cmd && i.key_pressed(Key::O) {
                open_path = rfd::FileDialog::new()
                    .add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif", "bmp"])
                    .pick_file();
            }
            if cmd && i.key_pressed(Key::S) {
                save = true;
            }
            if cmd && i.key_pressed(Key::Z) {
                if i.modifiers.shift {
                    do_redo = true;
                } else {
                    do_undo = true;
                }
            }
            if cmd && i.key_pressed(Key::D) {
                clear_sel = true;
            }
            if cmd && i.key_pressed(Key::Num0) {
                fit_view = true;
            }
            if cmd && i.key_pressed(Key::Num1) {
                zoom_100 = true;
            }
            if cmd && (i.key_pressed(Key::Equals)) {
                zoom_in = true;
            }
            if cmd && (i.key_pressed(Key::Minus)) {
                zoom_out = true;
            }
            if i.key_pressed(Key::L) && !cmd {
                self.mode = SelMode::Freeform;
            }
            if i.key_pressed(Key::R) && !cmd {
                self.mode = SelMode::Rect;
            }
            if i.key_pressed(Key::C) && !cmd {
                self.mode = SelMode::Circle;
            }
            if i.key_pressed(Key::Enter) {
                close_polygon = true;
            }
            if i.key_pressed(Key::F1) || (i.modifiers.shift && i.key_pressed(Key::Slash)) {
                toggle_help = true;
            }
            if i.modifiers.shift && !cmd {
                if i.key_pressed(Key::ArrowLeft) {
                    pan_delta.x -= 60.0;
                }
                if i.key_pressed(Key::ArrowRight) {
                    pan_delta.x += 60.0;
                }
                if i.key_pressed(Key::ArrowUp) {
                    pan_delta.y -= 60.0;
                }
                if i.key_pressed(Key::ArrowDown) {
                    pan_delta.y += 60.0;
                }
            }
            for f in &i.raw.dropped_files {
                open_path = Some(f.path().to_path_buf());
            }
        });

        if toggle_help {
            self.show_help = !self.show_help;
        }

        if do_undo {
            if self.state.document.lock().undo() {
                self.set_status("Undo");
            } else {
                self.set_status("Nothing to undo");
            }
        }
        if do_redo {
            if self.state.document.lock().redo() {
                self.set_status("Redo");
            } else {
                self.set_status("Nothing to redo");
            }
        }
        if clear_sel {
            self.state.document.lock().selection = None;
            self.pending_polygon.clear();
            self.set_status("Selection cleared (Ctrl+D)");
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

        // Compute canvas geometry once for the frame; the sidebar is an overlay
        // so the canvas keeps the full window area.
        self.sync_texture(&ctx);
        let dims = self.last_canvas_dims;
        let img_size = Vec2::new(dims.0 as f32, dims.1 as f32);
        let screen_rect = ctx.viewport_rect();
        let panel_min = screen_rect.min;
        let panel_size = screen_rect.size();
        let panel_size_inner = Vec2::new(panel_size.x, panel_size.y - 26.0); // reserve status bar

        if self.fit_needed && img_size.x > 1.0 {
            self.fit(panel_size_inner, img_size);
        }
        if fit_view {
            self.fit(panel_size_inner, img_size);
        }
        if zoom_100 {
            self.set_zoom_percent(100.0, ctx.input(|i| i.pointer.hover_pos()), panel_min, panel_size_inner, img_size);
            self.set_status("Zoom 100%");
        }
        if zoom_in {
            self.set_zoom_percent(self.zoom * 100.0 * 1.25, ctx.input(|i| i.pointer.hover_pos()), panel_min, panel_size_inner, img_size);
        }
        if zoom_out {
            self.set_zoom_percent(self.zoom * 100.0 / 1.25, ctx.input(|i| i.pointer.hover_pos()), panel_min, panel_size_inner, img_size);
        }
        self.pan += pan_delta;
        self.pan.x = self.pan.x.clamp(-panel_size.x * 2.0, panel_size.x * 2.0);
        self.pan.y = self.pan.y.clamp(-panel_size.y * 2.0, panel_size.y * 2.0);

        // --- canvas: full-window painter + interaction -------------------------
        if img_size.x > 1.0 {
            let rect = self.image_rect(panel_min, panel_size_inner, img_size);
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

            // --- pan vs select ---
            // Pan is driven by RAW pointer state (button_down + press_origin +
            // latest_pos), not by egui's widget drag classification, so it is
            // immune to click-vs-drag thresholds and frame-order quirks:
            // - Space held + primary drag => pan, ANYWHERE in the window
            //   (Photoshop-style; sticky for the whole gesture)
            // - right button drag => pan
            let pointer = ctx.input(|i| i.pointer.clone());
            let space_down = ctx.input(|i| i.key_down(Key::Space));
            let primary_down = pointer.button_down(PointerButton::Primary);
            let right_down = pointer.button_down(PointerButton::Secondary);

            if pointer.press_origin().is_none() {
                // No button held: clear per-gesture state.
                self.gesture_is_pan = false;
                self.pan_last_pos = None;
            } else if self.pan_last_pos.is_none() {
                self.pan_last_pos = pointer.press_origin();
            }

            if space_down && primary_down && !self.gesture_is_pan {
                // Pan takes over this gesture: discard any partial selection.
                self.gesture_is_pan = true;
                self.dragging = false;
                self.freehand.clear();
                self.drag_start = None;
                self.drag_current = None;
            }
            let panning = right_down || (self.gesture_is_pan && primary_down);
            self.panning = panning;

            if space_down {
                // Grabbing (closed hand) renders in every theme; "grab" may not.
                ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
            }

            if panning {
                if let (Some(last), Some(cur)) = (self.pan_last_pos, pointer.latest_pos()) {
                    self.pan += cur - last;
                }
                self.pan_last_pos = pointer.latest_pos();
                ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
            } else {
                // selection gestures
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
            }

            // scroll = cursor-anchored zoom (smooth multiplicative steps)
            let scroll = ctx.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 && response.hovered() {
                let factor = if scroll > 0.0 { 1.1 } else { 1.0 / 1.1 };
                let pointer = response.hover_pos();
                self.zoom_at(factor, pointer, panel_min, panel_size_inner, img_size);
            }
        }

        // --- auto-hide sidebar (left edge hotzone) -------------------------------
        let hover = ctx.input(|i| i.pointer.hover_pos());
        let over_hotzone = hover
            .map(|p| p.x - screen_rect.min.x < EDGE_HOTZONE)
            .unwrap_or(false);
        if over_hotzone {
            self.sidebar_open = true;
        } else if let Some(p) = hover {
            if p.x > screen_rect.min.x + SIDEBAR_WIDTH + 12.0 {
                self.sidebar_open = false;
            }
        }

        if self.sidebar_open || std::env::var_os("LASSO_KEEP_SIDEBAR").is_some() {
            let (can_undo, can_redo) = {
                let doc = self.state.document.lock();
                (doc.can_undo(), doc.can_redo())
            };
            let sidebar = egui::Area::new(egui::Id::new("sidebar"))
                .anchor(egui::Align2::LEFT_TOP, [0.0, 0.0])
                .order(egui::Order::Middle);
            let (mut open_clicked, mut save_clicked) = (false, false);
            let (mut fit_clicked, mut z100_clicked, mut zin_clicked, mut zout_clicked) =
                (false, false, false, false);
            let (mut help_clicked, mut deselect_clicked) = (false, false);
            let mut mode_clicked: Option<SelMode> = None;
            sidebar.show(&ctx, |ui| {
                egui::Frame::default()
                    .fill(Color32::from_rgba_unmultiplied(24, 26, 34, 242))
                    .inner_margin(10.0)
                    .show(ui, |ui| {
                        ui.vertical(|ui| {
                            ui.set_min_height(screen_rect.height() - 20.0);
                            ui.heading("lasso");
                            ui.add_space(8.0);

                            ui.label(egui::RichText::new("SELECT").weak().small());
                            for (mode, icon, tip) in [
                                (SelMode::Freeform, "\u{f0f03}", "Freeform lasso (L)"), // nf lasso
                                (SelMode::Rect, "\u{f096}", "Rectangle select (R)"),     // nf square_o
                                (SelMode::Circle, "\u{f4aa}", "Circle select (C)"),      // nf circle
                            ] {
                                if icon_button(ui, icon, self.mode == mode, tip).clicked() {
                                    mode_clicked = Some(mode);
                                }
                            }
                            if icon_button(ui, "\u{f12d}", false, "Deselect (Ctrl+D)").clicked() {
                                deselect_clicked = true;
                            }

                            ui.add_space(10.0);
                            ui.label(egui::RichText::new("HISTORY").weak().small());
                            if icon_button(ui, "\u{f0e2}", false, "Undo (Ctrl+Z)").clicked() && can_undo {
                                if self.state.document.lock().undo() {
                                    self.set_status("Undo");
                                } else {
                                    self.set_status("Nothing to undo");
                                }
                            }
                            if icon_button(ui, "\u{f01e}", false, "Redo (Ctrl+Shift+Z)").clicked() && can_redo {
                                if self.state.document.lock().redo() {
                                    self.set_status("Redo");
                                } else {
                                    self.set_status("Nothing to redo");
                                }
                            }

                            ui.add_space(10.0);
                            ui.label(egui::RichText::new("VIEW").weak().small());
                            if icon_button(ui, "\u{f065}", false, "Fit to window (Ctrl+0)").clicked() {
                                fit_clicked = true;
                            }
                            if icon_button(ui, "\u{f06f}", false, "Zoom 100% (Ctrl+1)").clicked() {
                                z100_clicked = true;
                            }
                            if icon_button(ui, "\u{f00e}", false, "Zoom in (Ctrl+=)").clicked() {
                                zin_clicked = true;
                            }
                            if icon_button(ui, "\u{f010}", false, "Zoom out (Ctrl+-)").clicked() {
                                zout_clicked = true;
                            }

                            ui.add_space(10.0);
                            ui.label(egui::RichText::new("FILE").weak().small());
                            if icon_button(ui, "\u{f07b}", false, "Open (Ctrl+O)").clicked() {
                                open_clicked = true;
                            }
                            if icon_button(ui, "\u{f0c7}", false, "Save (Ctrl+S)").clicked() {
                                save_clicked = true;
                            }

                            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                                if icon_button(ui, "?", false, "Keyboard shortcuts (?)").clicked() {
                                    help_clicked = true;
                                }
                            });
                        });
                    });
            });
            if let Some(mode) = mode_clicked {
                self.mode = mode;
            }
            if fit_clicked {
                self.fit(panel_size_inner, img_size);
            }
            if z100_clicked {
                self.set_zoom_percent(100.0, hover, panel_min, panel_size_inner, img_size);
            }
            if zin_clicked {
                self.set_zoom_percent(self.zoom * 100.0 * 1.25, hover, panel_min, panel_size_inner, img_size);
            }
            if zout_clicked {
                self.set_zoom_percent(self.zoom * 100.0 / 1.25, hover, panel_min, panel_size_inner, img_size);
            }
            if open_clicked {
                open_path = rfd::FileDialog::new()
                    .add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif", "bmp"])
                    .pick_file();
            }
            if save_clicked {
                save = true;
            }
            if deselect_clicked {
                self.state.document.lock().selection = None;
                self.pending_polygon.clear();
                self.set_status("Selection cleared (Ctrl+D)");
            }
            if help_clicked {
                self.show_help = !self.show_help;
            }
        }

        // --- help overlay (? / F1) -------------------------------------------------
        if self.show_help {
            egui::Area::new(egui::Id::new("help"))
                .order(egui::Order::Foreground)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&ctx, |ui| {
                    egui::Frame::default()
                        .fill(Color32::from_rgba_unmultiplied(18, 20, 26, 250))
                        .stroke(egui::Stroke::new(1.0, Color32::from_gray(80)))
                        .inner_margin(16.0)
                        .show(ui, |ui| {
                            ui.set_width(430.0);
                            ui.heading("Keyboard shortcuts");
                            ui.add_space(6.0);
                            let rows: &[(&str, &str)] = &[
                                ("L / R / C", "Freeform / Rect / Circle select"),
                                ("drag", "Draw selection"),
                                ("Ctrl+drag, Enter", "Polygon select, close it"),
                                ("Space+drag / right-drag", "Pan"),
                                ("Shift+arrows", "Pan by steps"),
                                ("scroll", "Zoom at cursor"),
                                ("Ctrl+= / Ctrl+-", "Zoom in / out"),
                                ("Ctrl+0", "Fit to window"),
                                ("Ctrl+1", "Zoom 100%"),
                                ("Ctrl+O / Ctrl+S", "Open / save"),
                                ("Ctrl+Z / Ctrl+Shift+Z", "Undo / redo"),
                                ("Ctrl+D", "Deselect"),
                                ("? or F1", "Toggle this help"),
                            ];
                            egui::Grid::new("help-grid").striped(true).show(ui, |ui| {
                                for (k, v) in rows {
                                    ui.label(egui::RichText::new(*k).monospace().strong());
                                    ui.label(*v);
                                    ui.end_row();
                                }
                            });
                            ui.add_space(6.0);
                            ui.weak("All pixel edits are made by your agent via MCP.");
                            if ui.button("Close (? / F1)").clicked() {
                                self.show_help = false;
                            }
                        });
                });
            if ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.show_help = false;
            }
        }

        // --- status bar (bottom overlay strip) --------------------------------------
        let status_h = 26.0;
        let status_rect = Rect::from_min_size(
            egui::pos2(screen_rect.min.x, screen_rect.max.y - status_h),
            Vec2::new(screen_rect.width(), status_h),
        );
        ui.painter().rect_filled(status_rect, 0, Color32::from_rgba_unmultiplied(20, 22, 28, 235));
        ui.painter().line_segment(
            [status_rect.min, egui::pos2(status_rect.max.x, status_rect.min.y)],
            egui::Stroke::new(1.0, Color32::from_gray(60)),
        );
        let painter = ui.painter();
        let mut cursor = status_rect.min + egui::vec2(8.0, (status_h - 16.0) / 2.0 + 8.0);
        let font = FontId::monospace(12.0);
        let fg = Color32::from_gray(180);
        let mut put = |text: &str, color: Color32, painter: &egui::Painter| {
            painter.text(cursor, Align2::LEFT_CENTER, text, font.clone(), color);
            let galley = painter.layout_no_wrap(text.to_string(), font.clone(), color);
            cursor.x += galley.size().x + 14.0;
        };
        {
            let doc = self.state.document.lock();
            let path_label = doc
                .path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "no file".into());
            put(&path_label, Color32::from_gray(210), painter);
            put(&format!("{}×{}", dims.0, dims.1), fg, painter);
            put(&format!("{:.0}%", self.zoom * 100.0), fg, painter);
            if let Some(sel) = &doc.selection {
                let (x, y, w, h) = sel.bbox(dims.0, dims.1);
                put(&format!("sel: {}×{} @ {},{}", w, h, x, y), fg, painter);
            }
        }
        if let Some(note) = &self.mcp_note {
            put(&format!("mcp: {note}"), Color32::YELLOW, painter);
        } else {
            put(crate::mcp::ENDPOINT, Color32::from_gray(120), painter);
        }
        put(&self.status, Color32::from_rgb(255, 214, 120), painter);
        if let Some(err) = &self.error {
            put(err, Color32::RED, painter);
        }

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
