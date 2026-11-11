//! The application menu behind the title row's menu button: the template of
//! Electron's `buildAutoHiddenMenu` (`packages/electron/main.mjs`), which the
//! frameless window opens from the same button on Windows and Linux.
//!
//! Labels come from Electron's own menu dictionaries
//! (`resources/menu-locales.json`) and accelerators are shown as Electron
//! writes them; a popup menu's accelerators are hints, so the keys stay with
//! the shortcuts. Items whose feature this client lacks stay in place,
//! disabled, so the menu keeps the reference's shape.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::{
    Anchor, AnyElement, App, Context, DismissEvent, Entity, FocusHandle, IntoElement,
    ParentElement, SharedString, Styled, Subscription, Window, anchored, deferred, div, point, px,
};

use crate::appearance::ThemeMode;
use crate::appearance_view::AppearanceOwner;
use crate::chrome;
use crate::files_view::PanelSurface;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

const MENU_LOCALES_JSON: &str = include_str!("../resources/menu-locales.json");
const BUG_REPORT_URL: &str =
    "https://github.com/openchamber/openchamber/issues/new?template=bug_report.yml";
const FEATURE_REQUEST_URL: &str =
    "https://github.com/openchamber/openchamber/issues/new?template=feature_request.yml";
const DISCORD_INVITE_URL: &str = "https://discord.gg/ZYRSdnwwKA";
/// The menu opens below the title row's menu button, 12 px from the edge.
const MENU_INSET: f32 = 12.0;

/// What a menu item does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    About,
    CheckForUpdates,
    Settings,
    ReloadWebview,
    Restart,
    OpenCommandPalette,
    Quit,
    NewWindow,
    NewSession,
    NewWorktree,
    AddWorkspace,
    Undo,
    Redo,
    Cut,
    Copy,
    AddSelectionToChat,
    Paste,
    SelectAll,
    Reload,
    ForceReload,
    DeveloperTools,
    ToggleRightSidebar,
    OpenGit,
    OpenFiles,
    ToggleTerminal,
    ToggleContextPanelExpanded,
    Theme(ThemeMode),
    ToggleSessionSidebar,
    ToggleMemoryDebug,
    FullScreen,
    Back,
    Forward,
    PreviousSession,
    NextSession,
    PreviousProject,
    NextProject,
    Minimize,
    ZoomIn,
    ZoomOut,
    ResetZoom,
    Close,
    KeyboardShortcuts,
    ShowDiagnostics,
    ClearCache,
    ReportBug,
    SuggestFeature,
    JoinDiscord,
}

impl Command {
    /// Whether this client has the feature the item runs.
    pub(crate) fn available(self) -> bool {
        !matches!(
            self,
            Self::CheckForUpdates
                | Self::ReloadWebview
                | Self::Restart
                | Self::NewWindow
                | Self::NewWorktree
                | Self::AddWorkspace
                | Self::AddSelectionToChat
                | Self::Reload
                | Self::ForceReload
                | Self::DeveloperTools
                | Self::ToggleContextPanelExpanded
                | Self::ToggleMemoryDebug
                | Self::Back
                | Self::Forward
                | Self::PreviousProject
                | Self::NextProject
                | Self::ZoomIn
                | Self::ZoomOut
                | Self::ResetZoom
                | Self::KeyboardShortcuts
                | Self::ShowDiagnostics
                | Self::ClearCache
        )
    }
}

/// One row of a submenu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    Item {
        label: &'static str,
        accelerator: Option<&'static str>,
        command: Command,
    },
    Separator,
}

const fn item(label: &'static str, accelerator: Option<&'static str>, command: Command) -> Entry {
    Entry::Item {
        label,
        accelerator,
        command,
    }
}

const SEPARATOR: Entry = Entry::Separator;

/// The submenus in order, each with its label key and rows.
pub(crate) const TEMPLATE: &[(&str, &[Entry])] = &[
    (
        "app.name",
        &[
            item("about", None, Command::About),
            item("checkForUpdates", None, Command::CheckForUpdates),
            SEPARATOR,
            item("settings", Some("Ctrl+,"), Command::Settings),
            item("reloadWebview", None, Command::ReloadWebview),
            item("restart", None, Command::Restart),
            item(
                "commandPalette",
                Some("Ctrl+P"),
                Command::OpenCommandPalette,
            ),
            SEPARATOR,
            item("quit", Some("Ctrl+Q"), Command::Quit),
        ],
    ),
    (
        "file",
        &[
            item("newWindow", Some("Ctrl+Shift+Alt+N"), Command::NewWindow),
            SEPARATOR,
            item("newSession", Some("Ctrl+N"), Command::NewSession),
            item("newWorktree", Some("Ctrl+Shift+N"), Command::NewWorktree),
            SEPARATOR,
            item("addWorkspace", None, Command::AddWorkspace),
            SEPARATOR,
            item("quit", Some("Ctrl+Q"), Command::Quit),
        ],
    ),
    (
        "edit",
        &[
            item("undo", Some("Ctrl+Z"), Command::Undo),
            item("redo", Some("Ctrl+Shift+Z"), Command::Redo),
            SEPARATOR,
            item("cut", Some("Ctrl+X"), Command::Cut),
            item("copy", Some("Ctrl+C"), Command::Copy),
            item(
                "addSelectionToChat",
                Some("Ctrl+L"),
                Command::AddSelectionToChat,
            ),
            item("paste", Some("Ctrl+V"), Command::Paste),
            item("selectAll", Some("Ctrl+A"), Command::SelectAll),
        ],
    ),
    (
        "view",
        &[
            item("reload", Some("Ctrl+R"), Command::Reload),
            item("forceReload", Some("Ctrl+Shift+R"), Command::ForceReload),
            item(
                "toggleDeveloperTools",
                Some("Ctrl+Alt+I"),
                Command::DeveloperTools,
            ),
            SEPARATOR,
            item(
                "toggleRightSidebar",
                Some("Ctrl+B"),
                Command::ToggleRightSidebar,
            ),
            item("openGitSidebar", Some("Ctrl+Shift+G"), Command::OpenGit),
            item("openFilesSidebar", None, Command::OpenFiles),
            SEPARATOR,
            item(
                "toggleTerminalDock",
                Some("Ctrl+J"),
                Command::ToggleTerminal,
            ),
            item(
                "toggleContextPanelExpanded",
                Some("Ctrl+Shift+F"),
                Command::ToggleContextPanelExpanded,
            ),
            SEPARATOR,
            item("lightTheme", None, Command::Theme(ThemeMode::Light)),
            item("darkTheme", None, Command::Theme(ThemeMode::Dark)),
            item("systemTheme", None, Command::Theme(ThemeMode::System)),
            SEPARATOR,
            item(
                "toggleSessionSidebar",
                Some("Ctrl+Alt+L"),
                Command::ToggleSessionSidebar,
            ),
            item(
                "toggleMemoryDebug",
                Some("Ctrl+Shift+D"),
                Command::ToggleMemoryDebug,
            ),
            SEPARATOR,
            item("toggleFullScreen", Some("F11"), Command::FullScreen),
        ],
    ),
    (
        "go",
        &[
            item("back", Some("Ctrl+["), Command::Back),
            item("forward", Some("Ctrl+]"), Command::Forward),
            SEPARATOR,
            item("previousSession", None, Command::PreviousSession),
            item("nextSession", None, Command::NextSession),
            SEPARATOR,
            item(
                "previousProject",
                Some("Ctrl+Alt+Up"),
                Command::PreviousProject,
            ),
            item("nextProject", Some("Ctrl+Alt+Down"), Command::NextProject),
        ],
    ),
    (
        "window",
        &[
            item("minimize", Some("Ctrl+M"), Command::Minimize),
            item("zoomIn", Some("Ctrl+="), Command::ZoomIn),
            item("zoomOut", Some("Ctrl+-"), Command::ZoomOut),
            item("resetZoom", Some("Ctrl+0"), Command::ResetZoom),
            item("toggleFullScreen", Some("F11"), Command::FullScreen),
            SEPARATOR,
            item("close", Some("Ctrl+W"), Command::Close),
        ],
    ),
    (
        "help",
        &[
            item(
                "keyboardShortcuts",
                Some("Ctrl+."),
                Command::KeyboardShortcuts,
            ),
            item(
                "showDiagnostics",
                Some("Ctrl+Shift+L"),
                Command::ShowDiagnostics,
            ),
            SEPARATOR,
            item("clearCache", None, Command::ClearCache),
            SEPARATOR,
            item("reportABug", None, Command::ReportBug),
            item("suggestFeature", None, Command::SuggestFeature),
            SEPARATOR,
            item("joinDiscord", None, Command::JoinDiscord),
        ],
    ),
];

type MenuLocales = HashMap<String, HashMap<String, String>>;

fn menu_locales() -> &'static MenuLocales {
    static LOCALES: OnceLock<MenuLocales> = OnceLock::new();
    LOCALES.get_or_init(|| {
        serde_json::from_str(MENU_LOCALES_JSON)
            .expect("resources/menu-locales.json is generated JSON")
    })
}

/// A menu label in `locale`, English when the locale has no menu dictionary.
pub(crate) fn label(locale: &str, key: &str) -> SharedString {
    let locales = menu_locales();
    locales
        .get(locale)
        .or_else(|| locales.get("en"))
        .and_then(|labels| labels.get(key))
        .map_or_else(
            || SharedString::from(key.to_owned()),
            |label| label.clone().into(),
        )
}

/// A press on the menu button within this long of an outside-press
/// dismissal belongs to that dismissal: the press that closed the menu must
/// not open it again.
const REOPEN_GUARD: Duration = Duration::from_millis(400);

/// The open menu and the focus it returns to.
struct AppMenu {
    menu: Entity<PopupMenu>,
    restore: Option<FocusHandle>,
    _dismiss: Subscription,
}

#[derive(Default)]
pub(crate) struct AppMenuState {
    open: Option<AppMenu>,
    dismissed_at: Option<Instant>,
}

impl AppMenuState {
    #[cfg(test)]
    pub(crate) fn is_open(&self) -> bool {
        self.open.is_some()
    }
}

impl Workspace {
    /// Opens the application menu, or closes it when it is open.
    pub(crate) fn toggle_app_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.app_menu.open.is_some() {
            self.close_app_menu(window, cx);
            return;
        }
        if self
            .app_menu
            .dismissed_at
            .take()
            .is_some_and(|at| at.elapsed() < REOPEN_GUARD)
        {
            return;
        }
        let target = window.focused(cx);
        let target_for_restore = target.clone();
        let workspace = cx.weak_entity();
        let locale = locale::strings(cx).locale().to_owned();
        let menu = PopupMenu::build(window, cx, move |mut menu, window, cx| {
            for (title, entries) in TEMPLATE {
                let workspace = workspace.clone();
                let target = target.clone();
                let locale = locale.clone();
                menu = menu.submenu(
                    label(&locale, title),
                    window,
                    cx,
                    move |mut submenu, _, _| {
                        for entry in *entries {
                            submenu = match *entry {
                                Entry::Separator => submenu.separator(),
                                Entry::Item {
                                    label: key,
                                    accelerator,
                                    command,
                                } => submenu.item(menu_item(
                                    key,
                                    label(&locale, key),
                                    accelerator,
                                    command,
                                    workspace.clone(),
                                    target.clone(),
                                )),
                            };
                        }
                        submenu
                    },
                );
            }
            menu.min_w(px(220.0))
        });
        let dismiss = cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, window, cx| {
            this.app_menu.dismissed_at = Some(Instant::now());
            this.close_app_menu(window, cx);
        });
        menu.update(cx, |_, cx| cx.focus_self(window));
        self.app_menu.open = Some(AppMenu {
            menu,
            restore: target_for_restore,
            _dismiss: dismiss,
        });
        cx.notify();
    }

    /// Closes the menu and gives focus back to what had it, or to the
    /// workspace, so its shortcuts keep working.
    pub(crate) fn close_app_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.app_menu.open.take() else {
            return;
        };
        match menu.restore {
            Some(handle) => handle.focus(window, cx),
            None => self.focus_handle.focus(window, cx),
        }
        cx.notify();
    }

    /// The open menu, below the menu button at the title row's start.
    pub(crate) fn render_app_menu(
        &self,
        window: &Window,
        direction: Direction,
    ) -> Option<AnyElement> {
        let menu = self.app_menu.open.as_ref()?.menu.clone();
        let top = px(chrome::TITLE_ROW_HEIGHT);
        let (position, corner) = match direction {
            Direction::LeftToRight => (point(px(MENU_INSET), top), Anchor::TopLeft),
            Direction::RightToLeft => (
                point(chrome::content_size(window).width - px(MENU_INSET), top),
                Anchor::TopRight,
            ),
        };
        Some(
            deferred(
                anchored()
                    .position(position)
                    .anchor(corner)
                    .snap_to_window_with_margin(px(8.0))
                    .child(menu),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    fn run_menu_command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            Command::About => self.open_about(window, cx),
            Command::Settings => self.open_settings(window, cx),
            Command::OpenCommandPalette => self.toggle_command_palette(window, cx),
            Command::Quit => cx.quit(),
            Command::Close => self.request_close(window, cx),
            Command::NewSession => self.show_new_session(window, cx),
            Command::ToggleRightSidebar => {
                let surface = self.panels.open.unwrap_or(PanelSurface::Git);
                self.toggle_panel(surface, cx);
            }
            Command::OpenGit => self.open_panel(PanelSurface::Git, cx),
            Command::OpenFiles => self.open_panel(PanelSurface::Files, cx),
            Command::ToggleTerminal => {
                if !self.terminal_open {
                    self.toggle_terminal(window, cx);
                }
            }
            Command::Theme(mode) => set_theme_mode(mode, cx),
            Command::ToggleSessionSidebar => self.toggle_sidebar(cx),
            Command::FullScreen => window.toggle_fullscreen(),
            Command::PreviousSession => self.select_adjacent(-1, window, cx),
            Command::NextSession => self.select_adjacent(1, window, cx),
            Command::Minimize => window.minimize_window(),
            Command::ReportBug => cx.open_url(BUG_REPORT_URL),
            Command::SuggestFeature => cx.open_url(FEATURE_REQUEST_URL),
            Command::JoinDiscord => cx.open_url(DISCORD_INVITE_URL),
            _ => {}
        }
        cx.notify();
    }

    fn open_panel(&mut self, surface: PanelSurface, cx: &mut Context<Self>) {
        if self.panels.open != Some(surface) {
            self.toggle_panel(surface, cx);
        }
    }
}

fn set_theme_mode(mode: ThemeMode, cx: &mut App) {
    let Some(owner) = cx
        .try_global::<AppearanceOwner>()
        .map(|owner| owner.0.clone())
    else {
        return;
    };
    owner.update(cx, |appearance, cx| {
        if let Err(error) = appearance.set_mode(mode, cx) {
            tracing::warn!(?error, "the theme mode could not be saved");
        }
    });
}

/// Edit items act on what had focus before the menu opened, as Electron's
/// roles act on the focused web contents.
fn edit_action(command: Command) -> Option<Box<dyn gpui_kit::Action>> {
    use gpui_kit::base::input;
    Some(match command {
        Command::Undo => Box::new(input::Undo),
        Command::Redo => Box::new(input::Redo),
        Command::Cut => Box::new(input::Cut),
        Command::Copy => Box::new(input::Copy),
        Command::Paste => Box::new(input::Paste),
        Command::SelectAll => Box::new(input::SelectAll),
        _ => return None,
    })
}

/// A row of a submenu. `key` is its label key in the template; the row's
/// accessibility ID is `app-menu-<key>`.
fn menu_item(
    key: &'static str,
    text: SharedString,
    accelerator: Option<&'static str>,
    command: Command,
    workspace: gpui_kit::WeakEntity<Workspace>,
    target: Option<FocusHandle>,
) -> PopupMenuItem {
    let accessible_label = text.clone();
    PopupMenuItem::element(move |_, cx| {
        let palette = cx.global::<Palette>();
        let direction = locale::strings(cx).direction();
        div()
            .w_full()
            .flex_row_from(direction)
            .items_center()
            .gap(px(24.0))
            .child(div().flex_1().child(text.clone()))
            .children(accelerator.map(|accelerator| {
                // Accelerators read left to right in every language.
                div()
                    .text_color(palette.muted_foreground)
                    .child(accelerator)
            }))
    })
    .aria_label(accessible_label)
    .accessibility_id(format!("app-menu-{key}"))
    .disabled(!command.available())
    .on_click(move |_, window, cx| {
        if let Some(action) = edit_action(command) {
            if let Some(target) = &target {
                target.focus(window, cx);
            }
            window.dispatch_action(action, cx);
            return;
        }
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.close_app_menu(window, cx);
            workspace.run_menu_command(command, window, cx);
        });
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::AppContext;
    use gpui_kit::test::TestWindowExt;

    #[gpui_kit::test]
    fn custom_menu_rows_expose_the_localized_name_without_the_shortcut(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::test::TestWindowExt;

        let (_, workspace) = crate::workspace_tests::open(cx);
        let owner = workspace.downgrade();
        let quit = label("de", "quit");
        let unavailable = label("de", "newWindow");
        let (menu_window, _) = cx.update(|cx| {
            gpui_kit::open_window(gpui_kit::WindowOptions::default(), cx, |window, cx| {
                PopupMenu::build(window, cx, |menu, _, _| {
                    menu.item(menu_item(
                        "quit",
                        quit.clone(),
                        Some("Ctrl+Q"),
                        Command::Quit,
                        owner.clone(),
                        None,
                    ))
                    .item(menu_item(
                        "newWindow",
                        unavailable.clone(),
                        Some("Ctrl+Shift+Alt+N"),
                        Command::NewWindow,
                        owner,
                        None,
                    ))
                })
            })
            .expect("a standalone menu window opens")
        });
        cx.update_window(menu_window, |_, window, cx| {
            window.render_frame(cx);
            for (index, expected) in [(0usize, quit.as_ref()), (1usize, unavailable.as_ref())] {
                let row = window.find(index);
                assert_eq!(row.role(), Some(gpui_kit::Role::MenuItem));
                assert_eq!(
                    row.label(),
                    Some(expected),
                    "the native menu row owns its translated name"
                );
            }
        })
        .expect("the real menu rows render with accessible names");
    }

    #[gpui_kit::test]
    fn the_command_palette_item_opens_the_palette(cx: &mut gpui_kit::TestAppContext) {
        assert!(Command::OpenCommandPalette.available());
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_app_menu(window, cx);
                assert!(workspace.app_menu.is_open());
                // A click on the item closes the menu, then runs its command.
                workspace.close_app_menu(window, cx);
                workspace.run_menu_command(Command::OpenCommandPalette, window, cx);
                assert!(
                    workspace.command_palette.is_some(),
                    "the menu item opens the palette"
                );
            });
        })
        .expect("the workspace window is open");
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            workspace.update(cx, |workspace, cx| {
                workspace.run_menu_command(Command::OpenCommandPalette, window, cx);
                assert!(
                    workspace.command_palette.is_none(),
                    "the item closes an open palette, as the web toggles it"
                );
            });
        })
        .expect("the workspace window is open");
    }

    #[test]
    fn every_label_exists_in_every_locale() {
        let locales = menu_locales();
        assert!(locales.contains_key("en"));
        for labels in locales.values() {
            for (title, entries) in TEMPLATE {
                assert!(labels.contains_key(*title), "{title}");
                for entry in *entries {
                    if let Entry::Item { label, .. } = entry {
                        assert!(labels.contains_key(*label), "{label}");
                    }
                }
            }
        }
        assert_eq!(label("de", "file"), "Datei");
        assert_eq!(label("xx", "file"), "File", "unknown locales read English");
    }

    #[test]
    fn template_matches_the_reference_shape() {
        let titles: Vec<&str> = TEMPLATE.iter().map(|(title, _)| *title).collect();
        assert_eq!(
            titles,
            ["app.name", "file", "edit", "view", "go", "window", "help"]
        );
        let available = |command: Command| command.available();
        assert!(available(Command::NewSession));
        assert!(available(Command::Settings));
        assert!(!available(Command::NewWindow), "no second window yet");
        assert!(edit_action(Command::Copy).is_some());
        assert!(edit_action(Command::Settings).is_none());
    }

    #[gpui_kit::test]
    fn close_keeps_the_other_workspace_open(cx: &mut gpui_kit::TestAppContext) {
        let (first, workspace) = crate::workspace_tests::open(cx);
        let (second, _) = crate::workspace_tests::open(cx);
        cx.update_window(first, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.run_menu_command(Command::Close, window, cx);
            });
        })
        .expect("the closing workspace is still live during its menu action");
        cx.run_until_parked();
        assert!(
            first.update(cx, |_, _, _| ()).is_err(),
            "Close removes its own window"
        );
        assert!(
            second.update(cx, |_, _, _| ()).is_ok(),
            "Close preserves the other window"
        );
        assert_eq!(
            cx.platform_quit_count(),
            0,
            "Close never asks the platform to quit with another window open"
        );
    }

    fn quit_after_last_window(cx: &mut gpui_kit::TestAppContext) -> Subscription {
        cx.update(|cx| {
            cx.set_quit_mode(gpui_kit::QuitMode::Explicit);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
        })
    }

    #[gpui_kit::test]
    fn quit_closes_every_workspace(cx: &mut gpui_kit::TestAppContext) {
        let (first, workspace) = crate::workspace_tests::open(cx);
        let (second, _) = crate::workspace_tests::open(cx);
        let _closed = quit_after_last_window(cx);
        cx.update_window(first, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.run_menu_command(Command::Quit, window, cx);
            });
        })
        .expect("application quit can start inside the owning workspace update");
        cx.run_until_parked();
        assert!(
            first.update(cx, |_, _, _| ()).is_err(),
            "Quit closes the initiating window"
        );
        assert!(
            second.update(cx, |_, _, _| ()).is_err(),
            "Quit reaches the other workspace's close guard"
        );
        assert_eq!(
            cx.platform_quit_count(),
            1,
            "the application quits after the last workspace closes"
        );
        assert!(
            cx.simulate_platform_quit(),
            "the final platform callback accepts the completed preflight"
        );
    }

    struct JournalFixture(std::path::PathBuf);

    impl JournalFixture {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let directory = std::env::temp_dir().join(format!(
                "openchamber-shell-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::create_dir(&directory).expect("unique private journal fixture");
            Self(directory)
        }

        fn journal(&self) -> std::path::PathBuf {
            self.0.join("drafts-and-sends.json")
        }
    }

    impl Drop for JournalFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn journal_window(
        cx: &mut gpui_kit::TestAppContext,
        fixture: &JournalFixture,
        text: &str,
    ) -> (gpui_kit::AnyWindowHandle, Entity<Workspace>) {
        use crate::connection::{Connection, ServerConfig};
        use openchamber_native_transport::endpoint::Endpoint;

        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                // Only local journal I/O runs. No server request is started by this fixture.
                workspace.connection = Some(
                    Connection::new(ServerConfig::for_endpoint(
                        Endpoint::parse("http://127.0.0.1:9").expect("unused test endpoint"),
                    ))
                    .expect("the test's network runtime starts"),
                );
                workspace.open_persistence(fixture.journal(), window, cx);
            });
        })
        .expect("the journal opens in its workspace's window");
        crate::recovery_tests::ready(cx, &workspace);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace
                    .composer
                    .update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
            });
        })
        .expect("the live composer accepts its private fixture text");
        (window, workspace)
    }

    fn wait_for(
        cx: &mut gpui_kit::TestAppContext,
        description: &str,
        mut condition: impl FnMut(&mut gpui_kit::TestAppContext) -> bool,
    ) {
        let started = Instant::now();
        loop {
            cx.run_until_parked();
            if condition(cx) {
                return;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "{description}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[gpui_kit::test]
    fn quit_waits_for_independent_workspace_journal_drains(cx: &mut gpui_kit::TestAppContext) {
        cx.executor().allow_parking();
        // Independent private journals qualify the window-close orchestration. Sharing a
        // profile journal between editable windows requires its own mutation owner.
        let first_fixture = JournalFixture::new();
        let second_fixture = JournalFixture::new();
        let (first, first_workspace) = journal_window(cx, &first_fixture, "first window draft");
        let (second, second_workspace) = journal_window(cx, &second_fixture, "second window draft");
        let _closed = quit_after_last_window(cx);
        let first_gate = cx.read(|cx| {
            first_workspace
                .read(cx)
                .attachment_worker()
                .expect("first journal owner")
                .hold_finish_reply_for_test()
        });
        let second_gate = cx.read(|cx| {
            second_workspace
                .read(cx)
                .attachment_worker()
                .expect("second journal owner")
                .hold_finish_reply_for_test()
        });

        cx.update_window(first, |_, window, cx| {
            first_workspace.update(cx, |workspace, cx| {
                workspace.run_menu_command(Command::Quit, window, cx);
            });
        })
        .expect("Quit starts from the first window's actual menu command");
        wait_for(cx, "the initiating window starts its journal drain", |_| {
            first_gate.has_reached()
        });
        assert!(
            cx.read(|cx| second_workspace.read(cx).close_in_progress()),
            "application Quit must also start the independent window's real close barrier"
        );
        wait_for(
            cx,
            "both independent journal finish receipts reach their gates",
            |_| second_gate.has_reached(),
        );
        assert!(first.update(cx, |_, _, _| ()).is_ok());
        assert!(second.update(cx, |_, _, _| ()).is_ok());
        assert_eq!(
            cx.platform_quit_count(),
            0,
            "held foreground drains veto platform shutdown"
        );
        // The gates hold replies from completed real I/O, not a timer or a mock save.
        // Let the second window finish first while the initiating window stays held.
        second_gate.release();
        wait_for(cx, "the second window closes independently", |cx| {
            second.update(cx, |_, _, _| ()).is_err()
        });
        assert!(first.update(cx, |_, _, _| ()).is_ok());
        assert_eq!(
            cx.platform_quit_count(),
            0,
            "one held drain still vetoes final shutdown"
        );
        cx.update(|cx| cx.quit());
        cx.run_until_parked();
        assert_eq!(
            cx.platform_quit_count(),
            0,
            "repeated Quit cannot bypass the held first receipt"
        );
        first_gate.release();
        wait_for(
            cx,
            "the initiating window closes after its own receipt",
            |cx| first.update(cx, |_, _, _| ()).is_err(),
        );
        assert_eq!(cx.platform_quit_count(), 1);
        assert!(cx.simulate_platform_quit());

        for (fixture, expected) in [
            (&first_fixture, "first window draft"),
            (&second_fixture, "second window draft"),
        ] {
            let (_, loaded) = crate::mutation_journal::MutationJournal::open(&fixture.journal())
                .expect("normal close released the actual journal lock");
            let crate::mutation_journal::LoadedJournal::Present(snapshot) = loaded else {
                panic!("each window saved its journal");
            };
            assert!(
                snapshot
                    .drafts
                    .iter()
                    .any(|(_, draft)| draft.text == expected),
                "each window's own draft is durable after application Quit"
            );
        }
    }
}
