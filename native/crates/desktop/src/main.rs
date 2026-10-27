//! The native OpenChamber desktop.
//!
//! `main` initializes diagnostics, assets, locale and theme, opens the window
//! and hands application lifetime to the [`Workspace`] entity. The first
//! window shows deterministic synthetic data and makes no network or
//! provider calls.

mod activation;
mod assets;
mod baseline;
mod chrome;
mod code_card;
mod code_wrap_state;
mod composer_view;
mod connection;
mod context_panel;
mod diagnostics;
mod draft_sidebar;
mod elevation;
mod fonts;
mod install_id;
mod locale;
mod logical;
mod markdown_list;
mod markdown_view;
mod message_id;
mod model_controls;
mod network;
mod open_in_app;
mod project_chooser;
mod project_section;
mod prompt_rail;
mod provider_logos;
mod send_attempt;
mod send_catalog;
mod server_sync;
mod session_mutations;
mod sidebar_item;
mod sidebar_view;
mod startup_view;
mod theme;
mod tracked_text;
mod transcript_selection;
mod transcript_view;
mod turn_footer;
mod turn_stats_section;
mod workspace;
#[cfg(test)]
mod workspace_tests;
mod workspace_view;

use gpui_kit::{
    AppContext, Bounds, TitlebarOptions, WindowAppearance, WindowBounds, WindowDecorations,
    WindowOptions, point, px, size,
};
use openchamber_native_core::synthetic::WorkloadSize;

use tracing_subscriber::EnvFilter;

use crate::assets::AppAssets;
use crate::connection::{Connection, ServerConfig};
use crate::diagnostics::Marker;
use crate::locale::Strings;
use crate::theme::{Appearance, Palette};
use crate::workspace::{Source, Workspace};

/// The default client size, which is also the parity method's fixture size.
const WINDOW_WIDTH: f32 = 1280.0;
const WINDOW_HEIGHT: f32 = 800.0;
/// The smallest client size the Electron window allows.
const MIN_WINDOW_WIDTH: f32 = 800.0;
const MIN_WINDOW_HEIGHT: f32 = 520.0;

fn main() {
    diagnostics::start();
    // Read off the startup path; joined just before the first frame.
    let generic_fonts = fonts::spawn_generic_lookup();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("OPENCHAMBER_NATIVE_LOG")
                .unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let Some(source) = requested_source() else {
        std::process::exit(2);
    };
    let title = match &source {
        Source::Synthetic(_) => "OpenChamber Native (synthetic data)".to_owned(),
        Source::Server(connection) => {
            format!(
                "OpenChamber Native ({})",
                connection.config.endpoint.authority()
            )
        }
    };

    gpui_kit::application()
        .with_assets(AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            // Chromium's Linux reference renders glyphs with grayscale antialiasing.
            #[cfg(target_os = "linux")]
            cx.set_text_rendering_mode(gpui_kit::TextRenderingMode::Grayscale);
            let strings = Strings::load(&locale::requested_locale());
            tracing::info!(locale = strings.locale(), "locale selected");
            cx.set_global(strings);
            let palette = requested_palette(cx.window_appearance());
            theme::apply(palette, cx);
            let generic_fonts = generic_fonts.join().unwrap_or_else(|_| {
                tracing::warn!("the font lookup failed; the default fonts stay");
                fonts::GenericFamilies::default()
            });
            fonts::apply(&generic_fonts, cx);
            workspace_view::bind_keys(cx);

            // The window grows by the frame line, so the content keeps the
            // sizes above.
            let framed = |width: f32, height: f32| {
                size(
                    px(width + 2.0 * chrome::FRAME_LINE),
                    px(height + 2.0 * chrome::FRAME_LINE),
                )
            };
            let bounds = Bounds::centered(None, framed(WINDOW_WIDTH, WINDOW_HEIGHT), cx);
            // Frameless like the Electron window on Windows and Linux: the
            // header draws the window controls and moves the window.
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(title.into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(12.0), px(16.0))),
                }),
                app_owns_titlebar_drag: true,
                window_decorations: Some(WindowDecorations::Client),
                window_min_size: Some(framed(MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT)),
                app_id: Some("ai.openchamber.native.development".to_owned()),
                ..WindowOptions::default()
            };
            let opened = gpui_kit::open_window(options, cx, |window, cx| {
                window.set_layout_mode(gpui_kit::LayoutMode::Logical);
                cx.new(|cx| {
                    let mut workspace = Workspace::new(source, window, cx);
                    workspace.initial_size = Some(chrome::InitialSize::new(size(
                        px(WINDOW_WIDTH),
                        px(WINDOW_HEIGHT),
                    )));
                    workspace
                })
            });
            if let Err(error) = opened {
                tracing::error!(%error, "the main window could not be opened");
                cx.quit();
                return;
            }
            diagnostics::mark(Marker::WindowCreated, None, None);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        });
}

/// A configured server, else synthetic data. `None` after reporting a
/// configuration the client cannot use.
fn requested_source() -> Option<Source> {
    let Some(config) = ServerConfig::from_env() else {
        return Some(Source::Synthetic(requested_workload()));
    };
    let config = match config {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(%error, "OPENCHAMBER_NATIVE_SERVER is not a usable server URL");
            return None;
        }
    };
    match Connection::new(config) {
        Ok(connection) => Some(Source::Server(Box::new(connection))),
        Err(error) => {
            tracing::error!(%error, "the network runtime could not start");
            None
        }
    }
}

/// `OPENCHAMBER_NATIVE_WORKLOAD`: `representative` (default), `stress` or `small`.
fn requested_workload() -> WorkloadSize {
    match std::env::var("OPENCHAMBER_NATIVE_WORKLOAD").as_deref() {
        Ok("stress") => WorkloadSize::Stress,
        Ok("small") => WorkloadSize::Small,
        _ => WorkloadSize::Representative,
    }
}

/// `OPENCHAMBER_NATIVE_THEME` names a built-in theme; otherwise the system
/// appearance picks the default light or dark palette.
fn requested_palette(appearance: WindowAppearance) -> Palette {
    if let Ok(id) = std::env::var("OPENCHAMBER_NATIVE_THEME")
        && let Some(palette) = Palette::by_id(&id)
    {
        return palette;
    }
    match appearance {
        WindowAppearance::Dark | WindowAppearance::VibrantDark => {
            Palette::default_for(Appearance::Dark)
        }
        WindowAppearance::Light | WindowAppearance::VibrantLight => {
            Palette::default_for(Appearance::Light)
        }
    }
}
