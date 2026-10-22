//! The native OpenChamber desktop.
//!
//! `main` initializes diagnostics, assets, locale and theme, opens the window
//! and hands application lifetime to the [`Workspace`] entity. The first
//! window shows deterministic synthetic data and makes no network or
//! provider calls.

mod activation;
mod assets;
mod composer_view;
mod connection;
mod diagnostics;
mod install_id;
mod locale;
mod network;
mod server_sync;
mod sidebar_item;
mod sidebar_view;
mod theme;
mod transcript_view;
mod workspace;
#[cfg(test)]
mod workspace_tests;
mod workspace_view;

use gpui_kit::{
    AppContext, Bounds, TitlebarOptions, WindowAppearance, WindowBounds, WindowOptions, px, size,
};
use openchamber_native_core::synthetic::WorkloadSize;

use tracing_subscriber::EnvFilter;

use crate::assets::AppAssets;
use crate::connection::{Connection, ServerConfig};
use crate::diagnostics::Marker;
use crate::locale::Strings;
use crate::theme::{Appearance, Palette};
use crate::workspace::{Source, Workspace};

/// The fixture client size of the parity method.
const WINDOW_WIDTH: f32 = 1280.0;
const WINDOW_HEIGHT: f32 = 800.0;

fn main() {
    diagnostics::start();
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
            let strings = Strings::load(&locale::requested_locale());
            tracing::info!(locale = strings.locale(), "locale selected");
            cx.set_global(strings);
            let palette = requested_palette(cx.window_appearance());
            theme::apply(palette, cx);
            workspace_view::bind_keys(cx);

            let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(title.into()),
                    ..TitlebarOptions::default()
                }),
                app_id: Some("ai.openchamber.native.development".to_owned()),
                ..WindowOptions::default()
            };
            let opened = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Workspace::new(source, window, cx))
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
