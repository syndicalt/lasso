//! MCP server: exposes the open lasso document to the local agent over
//! Streamable HTTP on 127.0.0.1:8756/mcp.
//!
//! All tools operate on the shared `Arc<Mutex<Document>>` so agent edits and
//! GUI edits hit the same document.

use crate::core::{decode_png_b64, parse_color, Anchor, Document, Op, Region, Selection};
use base64::Engine;
use image::RgbaImage;
use parking_lot::Mutex;
use rmcp::handler::server::wrapper::Json;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ImageContent, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::tool_handler;
use rmcp::tool_router;
use rmcp::{ErrorData as McpError, ServerHandler, tool};
use schemars::JsonSchema;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

pub const PORT: u16 = 8756;
pub const ENDPOINT: &str = "http://127.0.0.1:8756/mcp";

/// Shared state: the document plus a small edit log shown in the GUI.
#[derive(Clone, Default)]
pub struct SharedState {
    pub document: Arc<Mutex<Document>>,
    pub log: Arc<Mutex<Vec<String>>>,
}

impl SharedState {
    pub fn new(document: Arc<Mutex<Document>>) -> Self {
        Self {
            document,
            log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn note(&self, msg: String) {
        let mut log = self.log.lock();
        log.push(msg);
        let len = log.len();
        if len > 200 {
            log.drain(0..len - 200);
        }
    }
}

pub struct LassoServer {
    state: SharedState,
}

#[tool_router]
impl LassoServer {
    pub fn new(state: SharedState) -> Self {
        Self { state }
    }

    fn note(&self, msg: String) {
        self.state.note(msg);
    }

    /// `lasso open <path>` — load an image into the editor.
    #[tool(description = "Open an image file in lasso. Replaces any currently open document and clears the selection and undo history.")]
    async fn open(
        &self,
        Parameters(OpenArgs { path }): Parameters<OpenArgs>,
    ) -> Result<Json<serde_json::Value>, McpError> {
        let path = PathBuf::from(shellexpand_path(&path));
        let (w, h) = {
            let mut doc = self.state.document.lock();
            doc.open(&path).map_err(|e| McpError::invalid_params(e, None))?
        };
        self.note(format!("agent opened {}", path.display()));
        Ok(Json(serde_json::json!({
            "path": path.display().to_string(),
            "width": w,
            "height": h,
        })))
    }

    #[tool(description = "List the region operations lasso supports: fill, blur, brightness, invert, grayscale, pixelate, delete, crop. All ops use the current selection when one exists, otherwise the whole image.")]
    async fn ops(&self) -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "ops": [
                {"op": "fill",       "args": "color: #RRGGBB, #RRGGBBAA or \"transparent\""},
                {"op": "blur",       "args": "sigma: f32, e.g. 4.0"},
                {"op": "brightness", "args": "factor: f32, 0.5 darker, 1.5 brighter"},
                {"op": "invert"},
                {"op": "grayscale"},
                {"op": "pixelate",   "args": "block: u32 pixels, e.g. 16"},
                {"op": "delete",     "args": "makes the region transparent"},
                {"op": "crop",       "args": "requires a selection; crops canvas to its bbox"},
            ],
            "note": "Use selection_set_polygon or selection_set_rect first to scope an op to a region."
        }))
    }

    #[tool(description = "Set the lasso selection from a polygon of image-space points, e.g. points: \"10,20 40,22 35,60\". Minimum 3 points.")]
    async fn selection_set_polygon(
        &self,
        Parameters(SelectionPolygonArgs { points }): Parameters<SelectionPolygonArgs>,
    ) -> Result<Json<serde_json::Value>, McpError> {
        let polygon = parse_points(&points).map_err(|e| McpError::invalid_params(e, None))?;
        let sel = Selection::from_polygon(polygon).map_err(|e| McpError::invalid_params(e, None))?;
        let bbox = {
            let mut doc = self.state.document.lock();
            let (w, h) = doc.canvas.dimensions();
            let bbox = sel.bbox(w, h);
            doc.selection = Some(sel);
            bbox
        };
        self.note(format!(
            "agent set polygon selection ({} points, {})",
            points.split_whitespace().count(),
            format_bbox(bbox)
        ));
        Ok(Json(serde_json::json!({
            "points": points,
            "bbox": { "x": bbox.0, "y": bbox.1, "width": bbox.2, "height": bbox.3 }
        })))
    }

    #[tool(description = "Set the lasso selection to an axis-aligned rectangle: x, y, width, height in image pixels. Negative or overflowing values are clamped to the image.")]
    async fn selection_set_rect(
        &self,
        Parameters(SelectionRectArgs { x, y, width, height }): Parameters<SelectionRectArgs>,
    ) -> Result<Json<serde_json::Value>, McpError> {
        let sel = Selection::from_rect(x as f32, y as f32, width as f32, height as f32)
            .map_err(|e| McpError::invalid_params(e, None))?;
        let bbox = {
            let mut doc = self.state.document.lock();
            let (w, h) = doc.canvas.dimensions();
            let bbox = sel.bbox(w, h);
            doc.selection = Some(sel);
            bbox
        };
        self.note(format!("agent set rect selection {}", format_bbox(bbox)));
        Ok(Json(serde_json::json!({
            "bbox": { "x": bbox.0, "y": bbox.1, "width": bbox.2, "height": bbox.3 }
        })))
    }

    #[tool(description = "Apply an operation to the current selection, or to the whole image when no selection is set. op is one of: fill, blur, brightness, invert, grayscale, pixelate, delete, crop.")]
    async fn apply_op(
        &self,
        Parameters(ApplyOpArgs { op, color, sigma, factor, block }): Parameters<ApplyOpArgs>,
    ) -> Result<Json<serde_json::Value>, McpError> {
        let op = match op.as_str() {
            "fill" => Op::Fill {
                color: color
                    .as_deref()
                    .map(parse_color)
                    .transpose()
                    .map_err(|e| McpError::invalid_params(e, None))?
                    .unwrap_or([0, 0, 0, 0]),
            },
            "blur" => Op::Blur {
                sigma: sigma.unwrap_or(4.0),
            },
            "brightness" => Op::Brightness {
                factor: factor.unwrap_or(1.2),
            },
            "invert" => Op::Invert,
            "grayscale" => Op::Grayscale,
            "pixelate" => Op::Pixelate {
                block: block.unwrap_or(16),
            },
            "delete" => Op::Delete,
            "crop" => Op::Crop,
            other => {
                return Err(McpError::invalid_params(
                    format!("unknown op {other:?}; expected fill, blur, brightness, invert, grayscale, pixelate, delete, crop"),
                    None,
                ))
            }
        };
        let (desc, dims) = {
            let mut doc = self.state.document.lock();
            let desc = doc.apply(op).map_err(|e| McpError::invalid_params(e, None))?;
            let dims = doc.canvas.dimensions();
            (desc, dims)
        };
        self.note(format!("agent: {desc}"));
        Ok(Json(serde_json::json!({ "result": desc, "canvas": { "width": dims.0, "height": dims.1 } })))
    }

    #[tool(description = "Undo the last edit (including edits made by the agent).")]
    async fn undo(&self) -> Json<serde_json::Value> {
        let undone = {
            let mut doc = self.state.document.lock();
            doc.undo()
        };
        if undone {
            self.note("agent undid last edit".into());
        }
        Json(serde_json::json!({ "undone": undone }))
    }

    #[tool(description = "Redo the last undone edit.")]
    async fn redo(&self) -> Json<serde_json::Value> {
        let redone = {
            let mut doc = self.state.document.lock();
            doc.redo()
        };
        if redone {
            self.note("agent redid edit".into());
        }
        Json(serde_json::json!({ "redone": redone }))
    }

    #[tool(description = "Revert the document to the pixels it was opened with. Undoable.")]
    async fn reset(&self) -> Json<serde_json::Value> {
        {
            let mut doc = self.state.document.lock();
            doc.reset();
        }
        self.note("agent reset document to original".into());
        Json(serde_json::json!({ "reset": true }))
    }

    #[tool(description = "Get the current document state: path, canvas size, selection polygon and bounding box, undo availability.")]
    async fn get_state(&self) -> Json<serde_json::Value> {
        let doc = self.state.document.lock();
        let (w, h) = doc.canvas.dimensions();
        let (ow, oh) = doc.original.dimensions();
        Json(serde_json::json!({
            "open": doc.is_open(),
            "path": doc.path.as_ref().map(|p| p.display().to_string()),
            "canvas": { "width": w, "height": h },
            "original": { "width": ow, "height": oh },
            "selection": doc.selection.as_ref().map(|s| {
                let bbox = s.bbox(w, h);
                serde_json::json!({
                    "polygon": s.polygon,
                    "bbox": { "x": bbox.0, "y": bbox.1, "width": bbox.2, "height": bbox.3 }
                })
            }),
            "can_undo": doc.can_undo(),
            "can_redo": doc.can_redo(),
        }))
    }

    #[tool(description = "Export pixels as base64 PNG. region: \"full\" for the whole canvas, \"selection_bbox\" for the selection's bounding box, \"selection\" for the exact lasso region with transparent outside.")]
    async fn export_region(
        &self,
        Parameters(ExportArgs { region }): Parameters<ExportArgs>,
    ) -> Result<CallToolResult, McpError> {
        let region = match region.as_str() {
            "full" => Region::Full,
            "selection_bbox" | "selection-bbox" | "bbox" => Region::SelectionBbox,
            "selection" => Region::Selection,
            other => {
                return Err(McpError::invalid_params(
                    format!("unknown region {other:?}; expected full, selection_bbox, selection"),
                    None,
                ))
            }
        };
        let img = {
            let doc = self.state.document.lock();
            doc.export(region).map_err(|e| McpError::invalid_params(e, None))?
        };
        let (data_url, b64, w, h) = encode_png(&img)?;
        self.note(format!("agent exported region ({w}x{h})"));
        let mut result = CallToolResult::success(vec![ContentBlock::Image(ImageContent::new(
            data_url,
            "image/png",
        ))]);
        result.structured_content = Some(serde_json::json!({
            "width": w,
            "height": h,
            "base64_png": b64,
        }));
        Ok(result)
    }

    #[tool(description = "Save the current canvas to disk. Without a path, saves to the file the document was opened from (png, jpeg, webp, gif, bmp by extension).")]
    async fn save(
        &self,
        Parameters(SaveArgs { path }): Parameters<SaveArgs>,
    ) -> Result<Json<serde_json::Value>, McpError> {
        let saved = {
            let doc = self.state.document.lock();
            doc.save(path.as_deref().map(PathBuf::from).as_deref())
                .map_err(|e| McpError::invalid_params(e, None))?
        };
        self.note(format!("agent saved {}", saved.0.display()));
        Ok(Json(serde_json::json!({
            "path": saved.0.display().to_string(),
            "width": saved.1,
            "height": saved.2,
        })))
    }

    #[tool(description = "Composite a base64-encoded PNG onto the canvas with its top-left corner at x,y (image pixels). blend: alpha-composite (default true) or hard replace. within_selection: restrict writes to the current lasso selection polygon. Use this for any drawing or compositing: generate the PNG however you like, then paste it. The paste is undoable.")]
    async fn paste_image(
        &self,
        Parameters(PasteImageArgs { image_b64, x, y, blend, within_selection }): Parameters<PasteImageArgs>,
    ) -> Result<Json<serde_json::Value>, McpError> {
        let img = decode_png_b64(&image_b64).map_err(|e| McpError::invalid_params(e, None))?;
        let (w, h) = {
            let mut doc = self.state.document.lock();
            doc.paste(&img, x, y, blend.unwrap_or(true), within_selection.unwrap_or(false))
                .map_err(|e| McpError::invalid_params(e, None))?
        };
        self.note(format!("agent pasted {w}x{h} image at {x},{y}"));
        Ok(Json(serde_json::json!({ "pasted": { "width": w, "height": h, "x": x, "y": y } })))
    }

    #[tool(description = "Load an image file from disk and composite it onto the canvas with its top-left corner at x,y. Same blend and within_selection semantics as paste_image. The paste is undoable.")]
    async fn paste_file(
        &self,
        Parameters(PasteFileArgs { path, x, y, blend, within_selection }): Parameters<PasteFileArgs>,
    ) -> Result<Json<serde_json::Value>, McpError> {
        let path = PathBuf::from(shellexpand_path(&path));
        let img = image::open(&path)
            .map_err(|e| McpError::invalid_params(format!("open {}: {e}", path.display()), None))?
            .to_rgba8();
        let (w, h) = {
            let mut doc = self.state.document.lock();
            doc.paste(&img, x, y, blend.unwrap_or(true), within_selection.unwrap_or(false))
                .map_err(|e| McpError::invalid_params(e, None))?
        };
        self.note(format!("agent pasted {} ({w}x{h}) at {x},{y}", path.display()));
        Ok(Json(serde_json::json!({ "pasted": { "path": path.display().to_string(), "width": w, "height": h, "x": x, "y": y } })))
    }

    #[tool(description = "Resize the canvas to width x height. The current image is re-anchored at anchor (top_left, top, top_right, left, center, right, bottom_left, bottom, bottom_right) and new areas are filled with fill color (#RRGGBB, #RRGGBBAA or \"transparent\"). Clears the selection. Undoable.")]
    async fn resize_canvas(
        &self,
        Parameters(ResizeCanvasArgs { width, height, anchor, fill }): Parameters<ResizeCanvasArgs>,
    ) -> Result<Json<serde_json::Value>, McpError> {
        let fill = match fill {
            Some(f) => parse_color(&f).map_err(|e| McpError::invalid_params(e, None))?,
            None => [0, 0, 0, 0],
        };
        let a = anchor.unwrap_or(Anchor::Center);
        let (w, h) = {
            let mut doc = self.state.document.lock();
            doc.resize_canvas(width, height, a, fill)
                .map_err(|e| McpError::invalid_params(e, None))?
        };
        self.note(format!("agent resized canvas to {w}x{h}"));
        Ok(Json(serde_json::json!({ "canvas": { "width": w, "height": h } })))
    }

    #[tool(description = "Clear the lasso selection so subsequent ops apply to the whole image.")]
    async fn clear_selection(&self) -> Json<serde_json::Value> {
        {
            let mut doc = self.state.document.lock();
            doc.selection = None;
        }
        self.note("agent cleared selection".into());
        Json(serde_json::json!({ "cleared": true }))
    }
}

// ---- args schemas -----------------------------------------------------------

#[derive(Debug, serde::Deserialize, JsonSchema)]
struct OpenArgs {
    /// Absolute path to the image file.
    path: String,
}

#[derive(Debug, serde::Deserialize, JsonSchema)]
struct SelectionPolygonArgs {
    /// Space-separated "x,y" pairs in image pixels, e.g. "10,20 40,22 35,60".
    points: String,
}

#[derive(Debug, serde::Deserialize, JsonSchema)]
struct SelectionRectArgs {
    x: i64,
    y: i64,
    width: u32,
    height: u32,
}

#[derive(Debug, serde::Deserialize, JsonSchema)]
struct ApplyOpArgs {
    /// One of: fill, blur, brightness, invert, grayscale, pixelate, delete, crop.
    op: String,
    /// For fill: "#RRGGBB", "#RRGGBBAA" or "transparent".
    #[serde(default)]
    color: Option<String>,
    /// For blur: gaussian sigma (default 4.0).
    #[serde(default)]
    sigma: Option<f32>,
    /// For brightness: multiplier (default 1.2).
    #[serde(default)]
    factor: Option<f32>,
    /// For pixelate: block size in pixels (default 16).
    #[serde(default)]
    block: Option<u32>,
}

#[derive(Debug, serde::Deserialize, JsonSchema)]
struct ExportArgs {
    /// One of: full, selection_bbox, selection.
    region: String,
}

#[derive(Debug, serde::Deserialize, JsonSchema)]
struct SaveArgs {
    /// Optional output path; defaults to the opened file.
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, serde::Deserialize, JsonSchema)]
struct PasteImageArgs {
    /// Base64-encoded PNG to composite onto the canvas.
    image_b64: String,
    /// Top-left x in image pixels (may be negative to clip).
    x: i64,
    /// Top-left y in image pixels.
    y: i64,
    /// Alpha-composite (true, default) or hard replace (false).
    #[serde(default)]
    blend: Option<bool>,
    /// Restrict writes to the current selection polygon.
    #[serde(default)]
    within_selection: Option<bool>,
}

#[derive(Debug, serde::Deserialize, JsonSchema)]
struct PasteFileArgs {
    /// Path of the image file to paste (png, jpeg, webp, gif, bmp).
    path: String,
    /// Top-left x in image pixels (may be negative to clip).
    x: i64,
    /// Top-left y in image pixels.
    y: i64,
    /// Alpha-composite (true, default) or hard replace (false).
    #[serde(default)]
    blend: Option<bool>,
    /// Restrict writes to the current selection polygon.
    #[serde(default)]
    within_selection: Option<bool>,
}

#[derive(Debug, serde::Deserialize, JsonSchema)]
struct ResizeCanvasArgs {
    width: u32,
    height: u32,
    /// Where the old canvas lands: top_left, top, top_right, left, center, right, bottom_left, bottom, bottom_right.
    #[serde(default)]
    anchor: Option<Anchor>,
    /// Fill for new areas: "#RRGGBB", "#RRGGBBAA" or "transparent". Default transparent.
    #[serde(default)]
    fill: Option<String>,
}

// ---- server plumbing ---------------------------------------------------------

#[tool_handler]
impl ServerHandler for LassoServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::LATEST;
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        let mut impl_ = Implementation::default();
        impl_.name = "lasso".into();
        impl_.version = env!("CARGO_PKG_VERSION").into();
        impl_.title = Some("Lasso image editor".into());
        info.server_info = impl_;
        info.instructions = Some(
            "Lasso is an image editor with a lasso selection tool that is open on the user's \
             desktop. The document is shared: anything you edit appears live in the window. \
             Typical flow: get_state, open if needed, selection_set_polygon or \
             selection_set_rect, then apply_op (fill/blur/brightness/invert/grayscale/\
             pixelate/delete/crop), then save. export_region returns a PNG you can view to \
             inspect the result. The user may edit or undo between your calls."
                .into(),
        );
        info
    }
}

fn format_bbox(bbox: (u32, u32, u32, u32)) -> String {
    format!("{}x{} at {},{}", bbox.2, bbox.3, bbox.0, bbox.1)
}

fn parse_points(s: &str) -> Result<Vec<[f32; 2]>, String> {
    s.split_whitespace()
        .map(|tok| {
            let (x, y) = tok
                .split_once(',')
                .ok_or_else(|| format!("point {tok:?} must be x,y"))?;
            let x: f32 = x.trim().parse().map_err(|_| format!("bad x in {tok:?}"))?;
            let y: f32 = y.trim().parse().map_err(|_| format!("bad y in {tok:?}"))?;
            Ok([x, y])
        })
        .collect()
}

fn encode_png(img: &RgbaImage) -> Result<(String, String, u32, u32), McpError> {
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png)
        .map_err(|e| McpError::internal_error(format!("png encode: {e}"), None))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(buf.get_ref());
    Ok((
        format!("data:image/png;base64,{b64}"),
        b64,
        img.width(),
        img.height(),
    ))
}

fn shellexpand_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(rest)
                .to_string_lossy()
                .into_owned();
        }
    }
    path.to_string()
}

/// Process-wide tokio runtime for the MCP server (the GUI does not drive tokio).
static TOKIO_RT: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
});

/// Spawn the MCP HTTP server. Non-fatal on failure: the GUI works without it,
/// but the error is reported to the caller for the status bar.
pub fn spawn(state: SharedState) -> Result<(), String> {
    let _guard = TOKIO_RT.enter();
    let session_manager =
        Arc::new(rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default());
    let mut config = rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default();
    config.json_response = true;
    let svc = rmcp::transport::streamable_http_server::tower::StreamableHttpService::new(
        move || Ok(LassoServer::new(state.clone())),
        session_manager,
        config,
    );
    let app = axum::Router::new().fallback_service(svc);
    let listener = TOKIO_RT
        .block_on(async { tokio::net::TcpListener::bind(("127.0.0.1", PORT)).await })
        .map_err(|e| format!("bind 127.0.0.1:{PORT}: {e}"))?;
    TOKIO_RT.spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("lasso mcp server stopped: {e}");
        }
    });
    Ok(())
}
