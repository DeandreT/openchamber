//! The native OpenChamber desktop.
//!
//! `main` initializes diagnostics, assets, locale and theme, opens the window
//! and hands application lifetime to the [`Workspace`] entity. The first
//! window shows deterministic synthetic data and makes no network or
//! provider calls.

mod about;
mod activation;
mod app_menu;
mod appearance;
mod appearance_storage;
mod appearance_view;
mod archive_view;
mod assets;
mod attachment_catalog;
mod attachment_copy;
mod attachment_files;
mod attachment_images;
mod attachment_intake;
mod attachment_io;
mod attachment_part_view;
mod attachment_preparation;
mod attachment_retirement;
mod attachment_spool;
mod attachment_staging;
mod attachment_storage;
mod audio_capture;
mod authentication;
#[cfg(test)]
mod authentication_tests;
mod authentication_view;
mod baseline;
mod behavior_settings;
mod blocking_requests;
#[cfg(test)]
mod branching_tests;
mod branching_view;
#[cfg(target_os = "linux")]
mod browser_view;
mod chrome;
mod clipboard_attachment;
mod code_card;
mod code_wrap_state;
mod compatibility_view;
mod composer_attachment_input;
mod composer_attachment_view;
mod composer_autocomplete;
mod composer_autocomplete_view;
mod composer_choices;
mod composer_delivery;
mod composer_dictation;
mod composer_document;
mod composer_file_preparation;
mod composer_handoff;
mod composer_payload;
mod composer_picker;
mod composer_preparation;
mod composer_references;
mod composer_view;
mod connection;
mod connection_recovery;
mod context_panel;
mod create_attempt;
mod credential_store;
mod desktop_notifications;
mod diagnostics;
mod diff_view;
mod display_mode_view;
mod document_attachments;
mod draft_companion;
mod draft_sidebar;
mod draft_storage;
mod effort_preferences;
mod elevation;
mod enterprise;
mod file_tree_view;
mod files_view;
mod fonts;
mod form_copy;
mod form_input;
mod general_settings;
mod git_view;
#[cfg(target_os = "linux")]
mod guest_panel_view;
mod heic_worker;
mod host_manager;
mod host_profiles;
mod host_recovery;
#[cfg(target_os = "linux")]
mod html_answer_view;
#[cfg(target_os = "linux")]
mod html_preview_view;
mod html_render_document;
mod install_id;
mod journal_worker;
mod knowledge;
mod knowledge_view;
mod knowledge_writes;
mod legacy_queue_reservation;
mod locale;
mod logical;
mod managed_host;
mod managed_host_lifecycle;
mod managed_host_supervisor;
mod managed_host_view;
mod markdown_diagram;
mod markdown_links;
mod markdown_list;
mod markdown_math;
mod markdown_view;
mod message_actions;
mod message_id;
mod message_queue;
mod model_controls;
mod move_session_dialog;
mod mutation_journal;
mod network;
mod notification_voice_pages;
mod open_in_app;
mod owned_process;
mod pane_resize;
mod panel_controls;
#[cfg(test)]
mod panel_tests;
mod part_state;
mod permission_modes;
mod preference_pages;
mod project_action_run;
mod project_chooser;
mod project_registration;
mod project_section;
mod project_worktrees;
mod prompt_rail;
mod provider_logos;
mod queue_capture;
mod queue_enqueue;
mod queue_local_send;
mod queue_payload_io;
mod queue_prefix;
mod queue_storage;
mod queue_strip;
mod queue_take;
mod reasoning_view;
#[cfg(test)]
mod recovery_tests;
mod recovery_view;
mod request_dock;
mod review_flow;
mod review_view;
mod scheduled_tasks;
#[cfg(test)]
mod scheduled_tasks_tests;
mod scheduled_tasks_view;
mod sdk_envelope;
mod selection_storage;
mod send_attempt;
mod send_catalog;
mod send_recovery;
mod server_sync;
mod session_admission;
mod session_ai_rename;
mod session_branching;
mod session_creation;
mod session_defaults;
mod session_export;
mod session_folders;
mod session_goal;
mod session_goal_view;
mod session_id;
mod session_management;
mod session_menu;
#[cfg(test)]
mod session_menu_tests;
mod session_mutations;
mod session_warming;
mod settings_controls;
mod settings_navigation;
#[cfg(test)]
mod settings_test_server;
mod settings_view;
mod shortcuts;
mod shortcuts_settings;
mod sidebar_item;
mod sidebar_search;
mod sidebar_view;
mod snippets_settings;
mod source_board;
#[cfg(test)]
mod source_board_tests;
mod source_board_view;
mod starter_capture;
mod starters;
mod startup_view;
mod state_persistence;
mod terminal_emulator;
mod terminal_surface;
mod terminal_view;
mod text_part_view;
mod theme;
mod time_zones;
mod tool_part_view;
mod tracked_text;
mod transcript_clock;
mod transcript_search;
mod transcript_selection;
mod transcript_view;
mod turn_footer;
mod turn_stats_section;
mod ui_memory;
mod usage_page;
mod web_policy;
mod web_shim;
#[cfg(target_os = "linux")]
mod webview_host;
#[cfg(target_os = "linux")]
mod webview_platform;
mod workspace;
#[cfg(test)]
mod workspace_tests;
mod workspace_view;
mod worktree_actions;
mod worktree_removals;

use gpui_kit::{
    AppContext, Bounds, TitlebarOptions, WindowAppearance, WindowBounds, WindowDecorations,
    WindowOptions, point, px, size,
};
use openchamber_native_core::synthetic::WorkloadSize;

use tracing_subscriber::EnvFilter;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use openchamber_native_transport::endpoint::Endpoint;

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
    if let Some(status) = heic_worker::run_if_requested() {
        std::process::exit(status);
    }
    connection::take_password_from_environment();
    diagnostics::start();
    // Read off the startup path; joined just before the first frame.
    let generic_fonts = fonts::spawn_generic_lookup();
    let layout = install_id::config_directory().map(|directory| {
        let store = pane_resize::LayoutStore::new(&directory);
        let layout = store.load();
        (store, layout)
    });
    let ui_memory = install_id::config_directory()
        .map(|directory| ui_memory::UiMemory::load(&directory))
        .unwrap_or_default();
    let appearance = install_id::config_directory().map(|directory| {
        let store = appearance_storage::AppearanceStore::new(&directory);
        let reader = store.clone();
        (store, std::thread::spawn(move || reader.load()))
    });
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("OPENCHAMBER_NATIVE_LOG")
                .unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let relaunch = Arc::new(AtomicBool::new(false));
    let Some((source, mut managed)) = requested_source(&relaunch) else {
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
        Source::Managed { .. } | Source::HostRecovery { .. } => "OpenChamber Native".to_owned(),
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
            cx.set_global(ui_memory);
            let palette = requested_palette(cx.window_appearance());
            theme::apply(palette, cx);
            let generic_fonts = generic_fonts.join().unwrap_or_else(|_| {
                tracing::warn!("the font lookup failed; the default fonts stay");
                fonts::GenericFamilies::default()
            });
            fonts::apply(&generic_fonts, cx);
            let appearance = appearance.map(|(store, reader)| {
                let loaded = reader
                    .join()
                    .unwrap_or(Err(appearance_storage::AppearanceError::Unavailable));
                let state =
                    cx.new(|cx| appearance::AppearanceState::new(store, loaded, generic_fonts, cx));
                cx.set_global(appearance_view::AppearanceOwner(state.clone()));
                state
            });
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
                if let Some(appearance) = &appearance {
                    appearance.update(cx, |appearance, cx| appearance.attach_window(window, cx));
                }
                cx.new(|cx| {
                    let mut workspace = Workspace::new(source, window, cx);
                    if let Some((store, layout)) = layout {
                        workspace.restore_layout(store, layout);
                    }
                    workspace.start_policy_monitor(cx);
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

    // The window is gone; the owned server stops before the process exits.
    // An external server is never stopped: no host owns it.
    if let Some(host) = managed.as_mut() {
        let outcome = host.shutdown();
        tracing::info!(?outcome, "the managed host stopped");
    }
    if relaunch.load(Ordering::Acquire) {
        relaunch_app();
    }
}

/// Starts this binary again with the same arguments, after the managed host
/// stopped, as the desktop relaunches from its recovery screen.
fn relaunch_app() {
    let Ok(executable) = std::env::current_exe() else {
        tracing::error!("the app could not find itself to relaunch");
        return;
    };
    let spawned = std::process::Command::new(executable)
        .args(std::env::args_os().skip(1))
        .stdin(std::process::Stdio::null())
        .spawn();
    if let Err(error) = spawned {
        tracing::error!(%error, "the app could not relaunch");
    }
}

/// A configured server, else the managed local server, else synthetic data
/// when a workload is requested. `None` after reporting a configuration the
/// client cannot use.
fn requested_source(
    relaunch: &Arc<AtomicBool>,
) -> Option<(Source, Option<managed_host::ManagedHost>)> {
    let mut profile_label = None;
    let mut recovery = None;
    let config = ServerConfig::from_env().or_else(|| {
        if std::env::var_os("OPENCHAMBER_NATIVE_WORKLOAD").is_some() {
            return None;
        }
        let directory = install_id::config_directory()?;
        let store = host_profiles::ProfileStore::new(&directory);
        match store.load() {
            Ok(profiles) => profiles.default_profile().map(|host| {
                profile_label = Some(host.label.clone());
                Ok(ServerConfig::for_endpoint(host.endpoint.clone()))
            }),
            Err(error) => {
                recovery = Some(Source::HostRecovery { store, error });
                None
            }
        }
    });
    if let Some(recovery) = recovery {
        return Some((recovery, None));
    }
    let config = match config {
        Some(config) => config,
        None if std::env::var_os("OPENCHAMBER_NATIVE_WORKLOAD").is_some() => {
            return Some((Source::Synthetic(requested_workload()), None));
        }
        None => return managed_source(relaunch),
    };
    let config = match config {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(%error, "OPENCHAMBER_NATIVE_SERVER is not a usable server URL");
            return None;
        }
    };
    match Connection::new(config) {
        Ok(mut connection) => {
            connection.display_label = profile_label;
            Some((Source::Server(Box::new(connection)), None))
        }
        Err(error) => {
            tracing::error!(%error, "the network runtime could not start");
            None
        }
    }
}

/// Starts the managed local server, which the window connects to once it is
/// ready. A host that cannot start still opens the window, on its failure.
fn managed_source(
    relaunch: &Arc<AtomicBool>,
) -> Option<(Source, Option<managed_host::ManagedHost>)> {
    let host = managed_host::ManagedHost::start(install_id::config_directory().as_deref());
    let endpoint = Endpoint::parse(&format!("http://127.0.0.1:{}", host.port()))
        .expect("a loopback URL parses");
    let connection = match Connection::new(ServerConfig::for_endpoint(endpoint)) {
        Ok(connection) => connection,
        Err(error) => {
            tracing::error!(%error, "the network runtime could not start");
            return None;
        }
    };
    let link = managed_host_view::ManagedLink {
        state: host.subscribe(),
        relaunch: relaunch.clone(),
    };
    let source = Source::Managed {
        connection: Box::new(connection),
        link,
    };
    Some((source, Some(host)))
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
