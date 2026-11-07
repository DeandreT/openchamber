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
                | Self::OpenCommandPalette
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
            Command::Quit | Command::Close => self.request_close(window, cx),
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

fn menu_item(
    text: SharedString,
    accelerator: Option<&'static str>,
    command: Command,
    workspace: gpui_kit::WeakEntity<Workspace>,
    target: Option<FocusHandle>,
) -> PopupMenuItem {
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
}
