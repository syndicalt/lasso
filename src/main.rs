//! lasso — lasso-select regions of images and let your AI agent edit them.

mod app;
mod core;
mod mcp;
mod register;

use parking_lot::Mutex;
use std::sync::Arc;

fn main() {
    let mut args = std::env::args().skip(1);
    let first = args.next();

    match first.as_deref() {
        Some("--version") | Some("version") => {
            println!("lasso {}", env!("CARGO_PKG_VERSION"));
        }
        Some("register") => {
            // lasso register [--remove]
            let remove = args.next().as_deref() == Some("--remove");
            let result = if remove {
                register::unregister()
            } else {
                register::register()
            };
            match result {
                Ok(msg) => {
                    println!("{msg}");
                    println!("Agent tools go live on the next `omp` start (or /mcp reload).");
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("serve") => {
            // Headless MCP server (no GUI). For agents/CI.
            let state = mcp::SharedState::new(Arc::new(Mutex::new(core::Document::new())));
            if let Err(e) = mcp::spawn(state) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
            println!("lasso mcp listening on {}", mcp::ENDPOINT);
            std::thread::park();
        }
        Some("--help") | Some("help") | Some("-h") => {
            print_help();
        }
        Some(path) if !path.starts_with('-') => {
            launch_gui(Some(path.into()));
        }
        None => launch_gui(None),
        Some(other) => {
            eprintln!("unknown argument: {other}\n");
            print_help();
            std::process::exit(2);
        }
    }
}

fn launch_gui(open: Option<std::path::PathBuf>) {
    let document = Arc::new(Mutex::new(core::Document::new()));
    let state = mcp::SharedState::new(document.clone());

    let mcp_note = match mcp::spawn(state.clone()) {
        Ok(()) => None,
        Err(e) => Some(format!("server failed: {e}")),
    };

    let native = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 750.0])
            .with_min_inner_size([640.0, 400.0])
            .with_icon(load_icon()),
        ..Default::default()
    };

    let preload_path = open;
    let app_state = state;
    let result = eframe::run_native(
        "lasso",
        native,
        Box::new(move |_cc| {
            // Load JetBrains Mono Nerd Font so sidebar icon glyphs (PUA) resolve.
            let mut fonts = eframe::egui::FontDefinitions::default();
            if let Ok(data) = std::fs::read(
                "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
            ) {
                fonts.font_data.insert(
                    "jetbrains_nf".into(),
                    std::sync::Arc::new(eframe::egui::FontData::from_owned(data)),
                );
                for family in [eframe::egui::FontFamily::Proportional, eframe::egui::FontFamily::Monospace] {
                    if let Some(stack) = fonts.families.get_mut(&family) {
                        stack.push("jetbrains_nf".into());
                    }
                }
            }
            _cc.egui_ctx.set_fonts(fonts);

            let mut app = app::LassoApp::new(app_state, mcp_note);
            if let Some(p) = preload_path {
                app.preload(&p);
            }
            Ok(Box::new(app))
        }),
    );
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn load_icon() -> eframe::egui::IconData {
    let png = include_bytes!("../assets/icon.png");
    let img = image::load_from_memory(png).expect("icon.png");
    let rgba = img.to_rgba8();
    eframe::egui::IconData {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    }
}

fn print_help() {
    println!(
        "lasso {} — lasso-select image regions and let your AI agent edit them

Usage:
  lasso [IMAGE]        Open the editor (optionally with an image)
  lasso serve          Run the MCP server headless (127.0.0.1:{port}/mcp)
  lasso register       Register the lasso MCP server with the omp agent
  lasso register --remove
  lasso version

In the editor (selection only — all edits are made by the agent):
  drag (left)          selection: freehand / rect / circle (toolbar)
  Ctrl+drag            polygon lasso; Enter closes it
  right-drag / scroll  pan / zoom
  Ctrl+O               open an image
  Ctrl+Z / Ctrl+Shift+Z  undo / redo agent edits
  Escape               clear selection

The agent edits the same document via MCP tools (apply_op, selection_set_*,
export_region, save, ...).",
        env!("CARGO_PKG_VERSION"),
        port = mcp::PORT
    );
}
