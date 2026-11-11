//! The context panel's server surfaces and the Files surface.
//!
//! The rail's Files, Git and Diff buttons open a panel in place of the work
//! status card (`ContextPanel.tsx`), 640 px wide until resized, while the chat keeps 320 px.
//! Each surface is its own view, retained per runtime and directory, so a
//! session switch and a return bring back a dirty draft as it was.
//!
//! The Files surface (`FilesView.tsx`) lists the directory's tree and opens
//! one file in Kit's code editor. Saving writes one snapshot of the draft
//! (the shortcut, the Save button or the 1.5 s autosave); the server's write
//! is last-writer-wins, so a 2 s poll compares the open file's size and mtime
//! and swaps a clean file's content in place while a dirty draft is never
//! replaced (`openchamber_native_core::file_draft`). Expanded folders are
//! listed again every 8 s. Hidden surfaces poll nothing.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Render, Role, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, TestSupportExt, Window, div,
    prelude::FluentBuilder, px,
};
use openchamber_native_core::file_draft::{
    OpenFile, PollAction, PollStat, ReadFailure, SaveRefusal, SaveTrigger,
};
use openchamber_native_core::file_tree::{FileTree, ListingCause, ListingTicket};
use openchamber_native_core::ids::RuntimeId;
use openchamber_native_core::path;
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::files::{
    ContentMatch, ContentQuery, ContentSearch, FileHit, FilesError, SearchStop, StatRead,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{ButtonTone, CONTROL_RADIUS, IconButton, glyph};
use crate::context_panel::{ROW_LINE, ROW_SIZE, XS_LINE, XS_SIZE};
use crate::diff_view::DiffView;
use crate::elevation;
use crate::git_view::GitView;
use crate::knowledge::KnowledgePanel;
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::network::NetworkTask;
use crate::theme::Palette;
use crate::workspace::Workspace;

const HEADER_HEIGHT: f32 = 40.0;
/// The tree column beside the editor.
const TREE_WIDTH: f32 = 220.0;
pub(crate) const PANEL_ROW: f32 = 26.0;
const OPEN_FILE_POLL: Duration = Duration::from_secs(2);
/// Folders are listed again every fourth open-file poll.
const FOLDER_POLL_EVERY: u32 = 4;
/// A folder with more entries than this is never listed again on its own.
const FOLDER_POLL_MAX_ENTRIES: usize = 1000;
const AUTOSAVE_DELAY: Duration = Duration::from_millis(1500);
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(200);
const NAME_SEARCH_LIMIT: usize = 100;
const CONTENT_SEARCH_LIMIT: usize = 200;

gpui_kit::actions!(
    files_view,
    [
        /// Saves the open file now.
        SaveFile,
    ]
);

const KEY_CONTEXT: &str = "FilesView";

/// Marks the app whose key bindings already include the Files surface's.
struct FilesKeysBound;

impl gpui_kit::Global for FilesKeysBound {}

/// Binds the save shortcut once per app.
fn bind_keys(cx: &mut gpui_kit::App) {
    if cx.has_global::<FilesKeysBound>() {
        return;
    }
    cx.set_global(FilesKeysBound);
    cx.bind_keys([gpui_kit::KeyBinding::new(
        "secondary-s",
        SaveFile,
        Some(KEY_CONTEXT),
    )]);
}

/// One server, runtime and directory a surface works in. The network handle
/// belongs to the connection; once the connection is dropped, every request
/// started through it ends as cancelled.
#[derive(Clone)]
pub(crate) struct PanelScope {
    pub client: ServerClient,
    pub network: tokio::runtime::Handle,
    pub runtime: RuntimeId,
    pub directory: String,
    pub owner: Option<gpui_kit::WeakEntity<Workspace>>,
}

impl PanelScope {
    /// Resolve authentication and executor at request time, including for
    /// retained views that have not rendered since a reconnect.
    pub fn client(&mut self, cx: &gpui_kit::App) -> Option<ServerClient> {
        let Some(owner) = &self.owner else {
            return Some(self.client.clone());
        };
        let workspace = owner.upgrade()?;
        let workspace = workspace.read(cx);
        if workspace.runtime != self.runtime {
            return None;
        }
        let connection = workspace.connection.as_ref()?;
        let client = connection.client.clone()?;
        self.network = connection.network.handle();
        Some(client)
    }
    pub fn run<F>(&self, future: F) -> NetworkTask<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        NetworkTask::spawn_on(&self.network, future)
    }
}

/// A server surface of the context panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PanelSurface {
    Files,
    Git,
    Diff,
    Browser,
    /// An extension's panel, the one `Panels::open_guest` names.
    Guest,
    /// Project knowledge: notes, todos and plans.
    Notes,
}

impl PanelSurface {
    fn label_key(self) -> &'static str {
        match self {
            Self::Files => "contextPanel.mode.files",
            Self::Git => "layout.rightSidebar.git",
            Self::Diff => "contextPanel.mode.diff",
            Self::Browser => "contextPanel.mode.browser",
            Self::Guest => "settings.page.extensions.title",
            Self::Notes => "contextRail.surface.notes",
        }
    }
}

type ViewKey = (RuntimeId, String);

/// The open surface and the retained views.
#[derive(Default)]
pub(crate) struct Panels {
    pub(crate) open: Option<PanelSurface>,
    files: HashMap<ViewKey, (Entity<FilesView>, Rc<Cell<bool>>)>,
    git: HashMap<ViewKey, Entity<GitView>>,
    diff: HashMap<ViewKey, Entity<DiffView>>,
    /// A file a link asked for, opened once its directory's view exists.
    pending_file: Option<(ViewKey, String)>,
    #[cfg(target_os = "linux")]
    browser: HashMap<ViewKey, Entity<crate::browser_view::BrowserPanel>>,
    /// The extension panel the Guest surface shows.
    pub(crate) open_guest: Option<openchamber_native_transport::web_content::GuestPanel>,
    /// The selected runtime's extensions with a rail panel.
    pub(crate) guest_catalog: Option<(
        RuntimeId,
        Vec<openchamber_native_transport::web_content::GuestPanel>,
    )>,
    guest_catalog_task: Option<(RuntimeId, gpui_kit::Task<()>)>,
    #[cfg(target_os = "linux")]
    guests: HashMap<(RuntimeId, String, String), Entity<crate::guest_panel_view::GuestPanelView>>,
    /// Knowledge panels by runtime and owner path, with the subscription
    /// that carries their sends to the workspace.
    notes: HashMap<ViewKey, (Entity<KnowledgePanel>, gpui_kit::Subscription)>,
}

impl Panels {
    /// Every retained owner participates in the window's write barrier.
    pub(crate) fn all_knowledge_views(&self) -> Vec<Entity<KnowledgePanel>> {
        self.notes.values().map(|(view, _)| view.clone()).collect()
    }

    /// The retained knowledge panels of `runtime`.
    pub(crate) fn knowledge_views(&self, runtime: &RuntimeId) -> Vec<Entity<KnowledgePanel>> {
        self.notes
            .iter()
            .filter(|((owner, _), _)| owner == runtime)
            .map(|(_, (view, _))| view.clone())
            .collect()
    }
}

impl Workspace {
    /// The file open in the Files view of `directory`, which `@` offers
    /// before its search results.
    pub(crate) fn open_file_path(&self, directory: &str, cx: &gpui_kit::App) -> Option<String> {
        let (view, _) = self
            .panels
            .files
            .get(&(self.runtime.clone(), directory.to_owned()))?;
        view.read(cx)
            .open
            .as_ref()
            .map(|open| open.file.path().to_owned())
    }

    /// Applies Settings choices only to the runtime that dispatched them.
    pub(crate) fn apply_general_settings(
        &mut self,
        runtime: &RuntimeId,
        preferences: &crate::general_settings::GeneralPreferences,
        cx: &mut Context<Self>,
    ) {
        if &self.runtime == runtime {
            if let Some(settings) = self.send_catalog.settings.as_mut() {
                preferences.apply_to(settings);
            }
            if preferences.show_chats.is_some() || preferences.work_enabled.is_some() {
                self.rebuild_sidebar();
            }
            if let Some(panel) = &self.terminal_panel {
                panel.update(cx, |panel, cx| {
                    if &panel.runtime == runtime {
                        panel.apply_general_settings(
                            preferences.terminal_close_on_exit,
                            preferences.terminal_shell.as_deref(),
                            cx,
                        );
                    }
                });
            }
        }
        if let Some(auto_save) = preferences.auto_save_enabled {
            for ((owner, _), (view, _)) in &self.panels.files {
                if owner == runtime {
                    view.update(cx, |view, cx| view.set_auto_save(auto_save, cx));
                }
            }
        }
        cx.notify();
    }

    /// Keeps Settings choices the server has not confirmed over a newer read.
    pub(crate) fn overlay_unsettled_settings(
        &self,
        settings: &mut openchamber_native_transport::client::ServerSettings,
        cx: &App,
    ) {
        if let Some(page) = &self.settings
            && let Some(preferences) = page
                .read(cx)
                .general
                .read(cx)
                .unsettled_preferences(&self.runtime)
        {
            preferences.apply_to(settings);
        }
        // The sidebar's project order while its save is unanswered.
        if let Some(order) = self.session_management.project_sort.requested() {
            settings.project_sort = Some(order);
        }
    }

    /// Opens `surface`, or closes it when it is the open one.
    pub(crate) fn toggle_panel(&mut self, surface: PanelSurface, cx: &mut Context<Self>) {
        self.panels.open = (self.panels.open != Some(surface)).then_some(surface);
        if let Some(surface) = self.panels.open
            && let Some(scope) = self.panel_scope(cx)
        {
            let key = (scope.runtime.clone(), scope.directory.clone());
            // A surface shown again reads its status anew.
            match surface {
                PanelSurface::Git => {
                    if let Some(view) = self.panels.git.get(&key) {
                        let view = view.downgrade();
                        cx.defer(move |cx| {
                            let _ = view.update(cx, |view, cx| view.refresh(cx));
                        });
                    }
                }
                PanelSurface::Diff => {
                    if let Some(view) = self.panels.diff.get(&key) {
                        let view = view.downgrade();
                        cx.defer(move |cx| {
                            let _ = view.update(cx, |view, cx| view.refresh(cx));
                        });
                    }
                }
                PanelSurface::Notes => {
                    let owner = self.knowledge_owner(&scope.directory, cx);
                    if let Some(owner) = owner
                        && let Some((view, _)) =
                            self.panels.notes.get(&(scope.runtime.clone(), owner.path))
                    {
                        let view = view.downgrade();
                        cx.defer(move |cx| {
                            let _ = view.update(cx, |view, cx| view.reload(cx));
                        });
                    }
                }
                PanelSurface::Files | PanelSurface::Browser | PanelSurface::Guest => {}
            }
        }
        cx.notify();
    }

    /// Opens `path` in the Files surface of the panel's directory
    /// (`openContextFile`). A file outside it opens read-only.
    pub(crate) fn open_context_file(&mut self, path: String, cx: &mut Context<Self>) -> bool {
        let Some(scope) = self.panel_scope(cx) else {
            return false;
        };
        self.terminal_open = false;
        self.panels.open = Some(PanelSurface::Files);
        self.panels.pending_file = Some(((scope.runtime, scope.directory), path));
        cx.notify();
        true
    }

    /// The directory the panel works in: the selected session's, else the
    /// new-session draft's project, else the active project, as the web's
    /// effective directory falls back.
    fn panel_scope(&self, cx: &Context<Self>) -> Option<PanelScope> {
        let connection = self.connection.as_ref()?;
        let client = connection.client.clone()?;
        let directory = self.panel_directory()?;
        Some(PanelScope {
            client,
            network: connection.network.handle(),
            runtime: self.runtime.clone(),
            directory,
            owner: Some(cx.entity().downgrade()),
        })
    }

    /// The directory a panel surface shows.
    pub(crate) fn panel_directory(&self) -> Option<String> {
        if self.selected.is_some() {
            path::normalize_optional(self.selected_session()?.directory.as_deref())
        } else {
            let project = self
                .draft_projects
                .get(&self.runtime)
                .map(String::as_str)
                .or_else(|| {
                    self.send_catalog
                        .settings
                        .as_ref()
                        .and_then(|settings| settings.active_project_path())
                })?;
            path::normalize(project)
        }
    }

    /// The open surface for the panel's directory, created on first use.
    pub(crate) fn render_side_panel(
        &mut self,
        palette: &Palette,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        for (_, shown) in self.panels.files.values() {
            shown.set(false);
        }
        self.ensure_guest_catalog(cx);
        let surface = self.panels.open?;
        let scope = self.panel_scope(cx);
        let key = scope
            .as_ref()
            .map(|scope| (scope.runtime.clone(), scope.directory.clone()));
        for (view_key, (_, shown)) in &self.panels.files {
            shown.set(surface == PanelSurface::Files && Some(view_key) == key.as_ref());
        }
        let body = match (surface, scope, key) {
            (PanelSurface::Browser, _, _) => self.render_browser_surface(palette, window, cx),
            (PanelSurface::Guest, Some(scope), Some(_)) => {
                self.render_guest_surface(scope, palette, cx)
            }
            (surface, Some(scope), Some(key)) => match surface {
                PanelSurface::Files => {
                    let pending = self
                        .panels
                        .pending_file
                        .take_if(|(target, _)| *target == key)
                        .map(|(_, path)| path);
                    let view = self
                        .panels
                        .files
                        .entry(key)
                        .or_insert_with(|| {
                            let shown = Rc::new(Cell::new(true));
                            let flag = shown.clone();
                            (cx.new(|cx| FilesView::new(scope, flag, window, cx)), shown)
                        })
                        .0
                        .clone();
                    if let Some(path) = pending {
                        view.update(cx, |view, cx| view.open_later(path, cx));
                    }
                    view.into_any_element()
                }
                PanelSurface::Git => self
                    .panels
                    .git
                    .entry(key)
                    .or_insert_with(|| cx.new(|cx| GitView::new(scope, window, cx)))
                    .clone()
                    .into_any_element(),
                PanelSurface::Diff => self
                    .panels
                    .diff
                    .entry(key)
                    .or_insert_with(|| cx.new(|cx| DiffView::new(scope, cx)))
                    .clone()
                    .into_any_element(),
                PanelSurface::Browser => self.render_browser_surface(palette, window, cx),
                PanelSurface::Guest => empty_state(
                    locale::strings(cx).get("contextPanel.browser.loadFailed"),
                    palette,
                    cx,
                ),
                PanelSurface::Notes => match self.knowledge_owner(&scope.directory, cx) {
                    Some(owner) => {
                        let key = (scope.runtime.clone(), owner.path.clone());
                        if !self.panels.notes.contains_key(&key) {
                            let view = cx.new(|cx| KnowledgePanel::new(scope, owner, window, cx));
                            let subscription =
                                cx.subscribe_in(&view, window, |this, view, event, window, cx| {
                                    this.on_knowledge_event(view, event, window, cx)
                                });
                            self.panels.notes.insert(key.clone(), (view, subscription));
                        }
                        self.panels.notes[&key].0.clone().into_any_element()
                    }
                    None => empty_state(
                        locale::strings(cx)
                            .get("rightSidebar.contextNotesTodo.empty.selectProject"),
                        palette,
                        cx,
                    ),
                },
            },
            _ => {
                let strings = locale::strings(cx);
                let key = match surface {
                    PanelSurface::Files => "filesView.editor.pickFileFromTree",
                    PanelSurface::Git => "gitView.empty.selectSessionOrDirectory",
                    PanelSurface::Diff | PanelSurface::Browser | PanelSurface::Guest => {
                        "diffView.state.selectSessionDirectory"
                    }
                    PanelSurface::Notes => "rightSidebar.contextNotesTodo.empty.selectProject",
                };
                empty_state(strings.get(key), palette, cx)
            }
        };
        Some(self.side_panel_frame(surface, body, palette, direction, window, cx))
    }

    /// Reads the selected runtime's extension panels once per connection.
    fn ensure_guest_catalog(&mut self, cx: &mut Context<Self>) {
        let runtime = self.runtime.clone();
        let known = self
            .panels
            .guest_catalog
            .as_ref()
            .is_some_and(|(owner, _)| *owner == runtime)
            || self
                .panels
                .guest_catalog_task
                .as_ref()
                .is_some_and(|(owner, _)| *owner == runtime);
        if known {
            return;
        }
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let request = NetworkTask::spawn_on(&connection.network.handle(), async move {
            client.guest_panels().await
        });
        let owner = runtime.clone();
        let task = cx.spawn(async move |this, cx| {
            let panels = request.await;
            let _ = this.update(cx, |this, cx| {
                if this.runtime != owner {
                    return;
                }
                this.panels.guest_catalog_task = None;
                match panels {
                    Some(Ok(panels)) => {
                        this.panels.guest_catalog = Some((owner, panels));
                        cx.notify();
                    }
                    // A failed read lists nothing; the next connection reads again.
                    Some(Err(error)) => {
                        tracing::warn!(%error, "the extension catalog could not be read");
                        this.panels.guest_catalog = Some((owner, Vec::new()));
                    }
                    None => {}
                }
            });
        });
        self.panels.guest_catalog_task = Some((runtime, task));
    }

    /// The open extension's panel for the scope's directory.
    #[cfg(target_os = "linux")]
    fn render_guest_surface(
        &mut self,
        scope: PanelScope,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(panel) = self.panels.open_guest.clone() else {
            return empty_state(
                locale::strings(cx).get("contextPanel.browser.loadFailed"),
                palette,
                cx,
            );
        };
        let key = (
            scope.runtime.clone(),
            panel.id.clone(),
            scope.directory.clone(),
        );
        let owner = cx.entity().downgrade();
        self.panels
            .guests
            .entry(key)
            .or_insert_with(|| {
                cx.new(|cx| {
                    crate::guest_panel_view::GuestPanelView::new(
                        panel,
                        scope.runtime,
                        Some(scope.directory),
                        owner,
                        cx,
                    )
                })
            })
            .clone()
            .into_any_element()
    }

    #[cfg(not(target_os = "linux"))]
    fn render_guest_surface(
        &mut self,
        _scope: PanelScope,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        empty_state(
            locale::strings(cx).get("contextPanel.browser.loadFailed"),
            palette,
            cx,
        )
    }

    /// Opens an extension's panel, or closes it when it is the open one.
    pub(crate) fn toggle_guest_panel(
        &mut self,
        panel: openchamber_native_transport::web_content::GuestPanel,
        cx: &mut Context<Self>,
    ) {
        let open = self.panels.open == Some(PanelSurface::Guest)
            && self
                .panels
                .open_guest
                .as_ref()
                .is_some_and(|open| open.id == panel.id);
        if open {
            self.panels.open = None;
        } else {
            self.panels.open = Some(PanelSurface::Guest);
            self.panels.open_guest = Some(panel);
        }
        cx.notify();
    }

    /// The browser's tabs for the panel's directory, or for no directory
    /// before a project is chosen; the browser needs no server.
    #[cfg(target_os = "linux")]
    fn render_browser_surface(
        &mut self,
        _palette: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = (
            self.runtime.clone(),
            self.panel_directory().unwrap_or_default(),
        );
        self.panels
            .browser
            .entry(key)
            .or_insert_with(|| cx.new(|cx| crate::browser_view::BrowserPanel::new(window, cx)))
            .clone()
            .into_any_element()
    }

    #[cfg(not(target_os = "linux"))]
    fn render_browser_surface(
        &mut self,
        palette: &Palette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        empty_state(
            locale::strings(cx).get("contextPanel.browser.loadFailed"),
            palette,
            cx,
        )
    }

    /// The panel's card around a surface, with its resize handle.
    fn side_panel_frame(
        &self,
        surface: PanelSurface,
        body: AnyElement,
        palette: &Palette,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        div()
            .id("side-panel")
            .role(Role::Complementary)
            .accessibility_id("side-panel")
            .test_support()
            .aria_label(match (surface, &self.panels.open_guest) {
                (PanelSurface::Guest, Some(panel)) => SharedString::from(panel.name.clone()),
                _ => strings.get(surface.label_key()),
            })
            .relative()
            .w(px(self.fitted_panel_width(window)))
            .flex_shrink_0()
            .h_full()
            .pb(px(8.0))
            .padding_start(direction, px(8.0))
            .padding_end(direction, px(6.0))
            .child(
                div()
                    .size_full()
                    .rounded(px(12.0))
                    .overflow_hidden()
                    .bg(elevation::panel_fill(palette))
                    .shadow(elevation::panel_edge(palette))
                    .child(body),
            )
            .child(
                self.render_resize_handle(
                    crate::pane_resize::Pane::Panel,
                    strings.get("contextPanel.actions.resizePanelAria"),
                    palette,
                    direction,
                    cx,
                )
                // Straddle the card edge inside this frame's logical gutter.
                // Keep the handle outside the card's overflow clip.
                .inset_start(direction, px(4.0))
                .bottom(px(8.0)),
            )
            .into_any_element()
    }
}

/// A muted line in the middle of an empty surface.
pub(crate) fn empty_state<V>(text: SharedString, palette: &Palette, cx: &Context<V>) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p(px(16.0))
        .child(
            div()
                .css_text(ROW_SIZE, ROW_LINE, cx)
                .text_color(palette.muted_foreground)
                .child(text),
        )
        .into_any_element()
}

/// The status line every surface reserves under its header: the newest
/// outcome the user needs, read by accessibility clients as a status.
pub(crate) fn status_line<V>(
    id: &'static str,
    notice: Option<&Notice>,
    palette: &Palette,
    direction: Direction,
    cx: &Context<V>,
) -> AnyElement {
    let color = match notice.map(|notice| notice.tone) {
        Some(Tone::Error) => palette.status_error,
        _ => palette.muted_foreground,
    };
    div()
        .id(id)
        .role(Role::Status)
        .accessibility_id(id)
        .when_some(notice, |this, notice| this.aria_label(notice.text.clone()))
        .h(px(20.0))
        .px(px(12.0))
        .flex_row_from(direction)
        .items_center()
        .child(
            div()
                .min_w_0()
                .truncate()
                .css_text(XS_SIZE, XS_LINE, cx)
                .text_color(color)
                .children(notice.map(|notice| notice.text.clone())),
        )
        .into_any_element()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    Info,
    Error,
}

/// An outcome shown in a surface's status line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Notice {
    pub text: SharedString,
    pub tone: Tone,
}

impl Notice {
    pub fn info(text: SharedString) -> Self {
        Self {
            text,
            tone: Tone::Info,
        }
    }

    /// The localized title, and the server's explanation after it when it
    /// gave one.
    pub fn error(title: SharedString, detail: Option<&str>) -> Self {
        let text = match detail.map(str::trim).filter(|detail| !detail.is_empty()) {
            Some(detail) => format!("{title}: {detail}").into(),
            None => title,
        };
        Self {
            text,
            tone: Tone::Error,
        }
    }
}

/// A text control in a surface's row: a toggle when `pressed` is set.
pub(crate) fn text_button<V: 'static>(
    id: impl Into<SharedString>,
    label: SharedString,
    pressed: Option<bool>,
    palette: &Palette,
    cx: &Context<V>,
    action: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) -> AnyElement {
    let id: SharedString = id.into();
    let hover = palette.hover;
    let on = pressed == Some(true);
    let button = div()
        .id(gpui_kit::ElementId::Name(id.clone()))
        .role(Role::Button)
        .accessibility_id(id)
        .aria_label(label.clone())
        .when_some(pressed, |this, pressed| {
            this.aria_toggled(if pressed {
                gpui_kit::Toggled::True
            } else {
                gpui_kit::Toggled::False
            })
        })
        .h(px(24.0))
        .px(px(8.0))
        .flex_shrink_0()
        .rounded(px(CONTROL_RADIUS))
        .flex()
        .items_center()
        .cursor_pointer()
        .css_text(XS_SIZE, XS_LINE, cx)
        .text_color(if on {
            palette.selection_foreground
        } else {
            palette.foreground
        })
        .when(on, |this| this.bg(palette.selection))
        .when(!on, |this| this.hover(move |style| style.bg(hover)))
        .child(label);
    on_activate(button, cx, action)
        .test_support()
        .into_any_element()
}

/// Where the name or content search stands.
enum SearchState {
    Idle,
    Searching,
    Names(Vec<FileHit>),
    Contents {
        matches: Vec<ContentMatch>,
        stopped: Option<SearchStop>,
    },
    InvalidPattern,
    Unavailable,
    Failed(Option<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SearchMode {
    Names,
    Contents,
}

/// What the save control reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaveStatus {
    Idle,
    Saved,
}

/// What the Save button of the unsaved-changes dialog started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SaveStart {
    /// A write runs.
    Started,
    /// Nothing differs from the file.
    Clean,
    /// No write was allowed: one runs already, the file is read-only or not
    /// loaded, or its path is being renamed.
    Refused,
}

/// A file the user asked to open while the open one has unsaved edits
/// (`confirmDiscardOpen`): the editor is replaced only after a successful
/// save or an explicit Discard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingSwitch {
    pub(crate) path: String,
    /// The dialog's save runs.
    pub(crate) saving: bool,
}

/// The open file with its editor.
pub(crate) struct OpenEditor {
    /// Matches completions to this editor; unlike the path, a rename keeps it.
    id: u64,
    pub(crate) file: OpenFile,
    pub(crate) editor: Entity<EditorState>,
    /// Outside the workspace: read with `allowOutsideWorkspace`, never saved.
    outside: bool,
    /// Shown as a read-only preview; large files start this way.
    pub(crate) preview: bool,
    status: SaveStatus,
    /// Set while this view writes the editor's text, so the change event it
    /// raises is not an edit.
    applying: bool,
    /// The file's text changed outside the editor; the next frame shows it.
    pending_apply: bool,
    /// The file must be read again; the next frame starts the read.
    needs_load: bool,
    load: Option<Task<()>>,
    save: Option<Task<()>>,
    autosave: Option<Task<()>>,
    autosave_waiting: bool,
    /// The poll's stat read.
    read: Option<Task<()>>,
    /// The content reads of an external change.
    change: Option<Task<()>>,
    _subscription: Subscription,
}

/// The Files surface.
pub(crate) struct FilesView {
    auto_save: bool,
    pub(crate) scope: PanelScope,
    pub(crate) tree: FileTree,
    listings: HashMap<String, Task<()>>,
    pub(crate) tree_focus: FocusHandle,
    pub(crate) tree_scroll: gpui_kit::UniformListScrollHandle,
    search: Entity<InputState>,
    search_mode: SearchMode,
    regex: bool,
    search_state: SearchState,
    search_task: Option<Task<()>>,
    pub(crate) open: Option<OpenEditor>,
    next_editor: u64,
    pub(crate) switch: Option<PendingSwitch>,
    /// A file to open on the next frame, which has the window it needs.
    open_next: Option<String>,
    pub(crate) rename: Option<crate::file_tree_view::TreeRename>,
    /// Paths whose rename runs; a save under one is refused, as a write would
    /// recreate the old path.
    pub(crate) renaming: Vec<String>,
    pub(crate) uncertain_renames: HashMap<String, String>,
    pub(crate) rename_revisions: HashMap<String, u64>,
    pub(crate) next_rename_revision: u64,
    pub(crate) notice: Option<Notice>,
    shown: Rc<Cell<bool>>,
    /// The open HTML file as a page, while its preview is on.
    #[cfg(target_os = "linux")]
    html_preview: Option<Entity<crate::html_preview_view::HtmlPreviewView>>,
    poll_ticks: u32,
    polling: bool,
    _poll: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl FilesView {
    pub(crate) fn new(
        scope: PanelScope,
        shown: Rc<Cell<bool>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        bind_keys(cx);
        let placeholder = locale::strings(cx).get("filesView.tree.search.placeholder");
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let subscription = cx.subscribe_in(&search, window, |this, _, event, _, cx| {
            if matches!(event, InputEvent::Change) {
                this.search_changed(cx);
            }
        });
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(OPEN_FILE_POLL).await;
                if this.update(cx, |this, cx| this.poll_tick(cx)).is_err() {
                    return;
                }
            }
        });
        let mut subscriptions = vec![subscription];
        if let Some(owner) = scope.owner.as_ref().and_then(|owner| owner.upgrade()) {
            subscriptions.push(cx.observe(&owner, |this, _, cx| {
                this.sync_auto_save(cx);
                if this.open.as_ref().is_some_and(|open| open.autosave_waiting)
                    && this.scope.client(cx).is_some()
                {
                    this.arm_autosave(cx);
                }
            }));
        }
        let view = Self {
            auto_save: true,
            tree: FileTree::new(scope.directory.clone()),
            scope,
            listings: HashMap::new(),
            tree_focus: cx.focus_handle().tab_stop(true),
            tree_scroll: gpui_kit::UniformListScrollHandle::new(),
            search,
            search_mode: SearchMode::Names,
            regex: false,
            search_state: SearchState::Idle,
            search_task: None,
            open: None,
            next_editor: 0,
            switch: None,
            open_next: None,
            rename: None,
            renaming: Vec::new(),
            uncertain_renames: HashMap::new(),
            rename_revisions: HashMap::new(),
            next_rename_revision: 0,
            notice: None,
            shown,
            #[cfg(target_os = "linux")]
            html_preview: None,
            poll_ticks: 0,
            polling: false,
            _poll: poll,
            _subscriptions: subscriptions,
        };
        let entity = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = entity.update(cx, |view, cx| {
                view.sync_auto_save(cx);
                let root = view.tree.root().to_owned();
                if let Some(ticket) = view.tree.request(&root, ListingCause::Reveal) {
                    view.list(ticket, cx);
                }
                // The tree as it was left, across restarts (`useFilesViewTabsStore`).
                let remembered = cx
                    .try_global::<crate::ui_memory::UiMemory>()
                    .and_then(|memory| memory.tree(view.scope.runtime.as_str(), &root))
                    .cloned();
                if let Some(remembered) = remembered {
                    for ticket in view.tree.restore(&remembered.expanded) {
                        view.list(ticket, cx);
                    }
                    if view.open_next.is_none() {
                        view.open_next = remembered.selected;
                    }
                    cx.notify();
                }
            });
        });
        view
    }

    /// Performs one listing and hands it to the tree.
    pub(crate) fn list(&mut self, ticket: ListingTicket, cx: &mut Context<Self>) {
        let Some(client) = self.scope.client(cx) else {
            self.tree.complete(
                ticket,
                Err(openchamber_native_core::file_tree::ListingFailure::Failed(
                    None,
                )),
            );
            cx.notify();
            return;
        };
        let directory = self.scope.directory.clone();
        let path = ticket.path.clone();
        let respect = ticket.respect_gitignore;
        let task = self.scope.run(async move {
            client
                .list_directory(&directory, &path, respect)
                .await
                .map_err(FilesError::into_listing_failure)
        });
        let key = ticket.path.clone();
        self.listings.insert(
            key.clone(),
            cx.spawn(async move |this, cx| {
                let result = task.await.unwrap_or(Err(
                    openchamber_native_core::file_tree::ListingFailure::Failed(None),
                ));
                let _ = this.update(cx, |this, cx| {
                    this.listings.remove(&key);
                    if this.tree.complete(ticket, result) {
                        cx.notify();
                    }
                });
            }),
        );
    }

    pub(crate) fn toggle_folder(&mut self, path: &str, cx: &mut Context<Self>) {
        if let Some(ticket) = self.tree.toggle(path) {
            self.list(ticket, cx);
        }
        self.tree.select(Some(path.to_owned()));
        self.remember_tree(cx);
        cx.notify();
    }

    /// Keeps the expanded folders and the open file for the next start.
    pub(crate) fn remember_tree(&self, cx: &mut Context<Self>) {
        let selected = self.open.as_ref().map(|open| open.file.path().to_owned());
        crate::ui_memory::remember_tree(
            self.scope.runtime.as_str(),
            self.tree.root(),
            self.tree.expanded_paths(),
            selected,
            cx,
        );
    }

    /// Opens `path` on the next frame, as a link from the transcript asks.
    pub(crate) fn open_later(&mut self, path: String, cx: &mut Context<Self>) {
        self.open_next = Some(path);
        cx.notify();
    }

    pub(crate) fn refresh_tree(&mut self, cx: &mut Context<Self>) {
        for (old, new) in self.uncertain_renames.clone() {
            self.check_rename(old, new, cx);
        }
        for ticket in self.tree.refresh_all(ListingCause::Mutation) {
            self.list(ticket, cx);
        }
    }

    fn set_show_gitignored(&mut self, show: bool, cx: &mut Context<Self>) {
        self.listings.clear();
        for ticket in self.tree.set_show_gitignored(show) {
            self.list(ticket, cx);
        }
        if !self.search_text(cx).is_empty() {
            self.search_changed(cx);
        }
        cx.notify();
    }

    fn search_text(&self, cx: &Context<Self>) -> String {
        self.search.read(cx).value().trim().to_owned()
    }

    /// Runs the search after the typing settles. Replacing the task drops the
    /// previous request, which ends its ripgrep on the server.
    fn search_changed(&mut self, cx: &mut Context<Self>) {
        let query = self.search_text(cx);
        if query.is_empty() {
            self.search_task = None;
            self.search_state = SearchState::Idle;
            cx.notify();
            return;
        }
        self.search_state = SearchState::Searching;
        let directory = self.scope.directory.clone();
        let mode = self.search_mode;
        let content = ContentQuery {
            query: query.clone(),
            regex: self.regex,
            limit: CONTENT_SEARCH_LIMIT,
            include_hidden: true,
            respect_gitignore: !self.tree.show_gitignored(),
        };
        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let source = this.update(cx, |this, cx| {
                let client = this.scope.client(cx);
                if client.is_none() {
                    this.search_state = SearchState::Failed(None);
                    cx.notify();
                }
                client.map(|client| (client, this.scope.network.clone()))
            });
            let Ok(Some((client, network))) = source else {
                return;
            };
            let task =
                NetworkTask::spawn_on(&network, async move {
                    match mode {
                        SearchMode::Names => client
                            .find_files(&directory, &query, NAME_SEARCH_LIMIT)
                            .await
                            .map(SearchState::Names),
                        SearchMode::Contents => client
                            .search_contents(&directory, &content)
                            .await
                            .map(|outcome| match outcome {
                                ContentSearch::Matches { matches, stopped } => {
                                    SearchState::Contents { matches, stopped }
                                }
                                ContentSearch::InvalidPattern(_) => SearchState::InvalidPattern,
                                ContentSearch::Unavailable => SearchState::Unavailable,
                            }),
                    }
                });
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.search_state = result
                    .map(|result| {
                        result.unwrap_or_else(|error| {
                            SearchState::Failed(error.message().map(str::to_owned))
                        })
                    })
                    .unwrap_or(SearchState::Failed(None));
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn set_search_mode(&mut self, mode: SearchMode, cx: &mut Context<Self>) {
        if self.search_mode != mode {
            self.search_mode = mode;
            self.search_changed(cx);
        }
    }

    fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.set_value("", window, cx));
        self.search_changed(cx);
    }

    /// Opens `path` in the editor. A file outside the workspace opens
    /// read-only. While the open file has unsaved edits, the unsaved-changes
    /// dialog asks first.
    pub(crate) fn open_file(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .open
            .as_ref()
            .is_some_and(|open| open.file.path() == path)
        {
            self.switch = None;
            return;
        }
        if self.open.as_ref().is_some_and(|open| open.file.is_dirty()) {
            self.switch = Some(PendingSwitch {
                path: path.to_owned(),
                saving: false,
            });
            cx.notify();
            return;
        }
        self.replace_editor(path, window, cx);
    }

    /// The unsaved-changes dialog's Save: the switch follows once the write
    /// succeeded and nothing is left unsaved.
    pub(crate) fn save_and_switch(&mut self, cx: &mut Context<Self>) {
        let Some(switch) = self.switch.as_mut().filter(|switch| !switch.saving) else {
            return;
        };
        let path = switch.path.clone();
        match self.save(SaveTrigger::Explicit, cx) {
            SaveStart::Started => {
                if let Some(switch) = self.switch.as_mut() {
                    switch.saving = true;
                }
            }
            SaveStart::Clean => self.open_next = Some(path),
            SaveStart::Refused => {
                let strings = locale::strings(cx);
                self.notice = Some(Notice::error(
                    strings.get("filesView.toast.saveFailed"),
                    None,
                ));
            }
        }
        cx.notify();
    }

    /// The dialog's Discard drops the draft and opens the asked-for file.
    pub(crate) fn discard_and_switch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(switch) = self.switch.take() {
            self.replace_editor(&switch.path, window, cx);
        }
    }

    pub(crate) fn cancel_switch(&mut self, cx: &mut Context<Self>) {
        if self.switch.take().is_some() {
            cx.notify();
        }
    }

    fn replace_editor(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.switch = None;
        let outside = !path::is_within(path, &self.scope.directory);
        let language = path
            .rsplit('/')
            .next()
            .and_then(|name| name.rsplit_once('.'))
            .map(|(_, extension)| extension.to_lowercase())
            .unwrap_or_else(|| "text".to_owned());
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(language)
                .line_number(true)
        });
        let subscription = cx.subscribe_in(&editor, window, |this, editor, event, _, cx| {
            if matches!(event, InputEvent::Change) {
                let text = editor.read(cx).value();
                this.edited(&text, cx);
            }
        });
        self.next_editor += 1;
        self.open = Some(OpenEditor {
            id: self.next_editor,
            file: OpenFile::new(path, outside),
            editor,
            outside,
            preview: false,
            status: SaveStatus::Idle,
            applying: false,
            pending_apply: false,
            needs_load: false,
            load: None,
            save: None,
            autosave: None,
            autosave_waiting: false,
            read: None,
            change: None,
            _subscription: subscription,
        });
        self.polling = false;
        self.tree.select(Some(path.to_owned()));
        if !outside {
            self.remember_tree(cx);
        }
        self.load_open_file(window, cx);
        cx.notify();
    }

    /// The open editor, when it is still the one with `id`.
    fn editor_mut(&mut self, id: u64) -> Option<&mut OpenEditor> {
        self.open.as_mut().filter(|open| open.id == id)
    }

    /// Whether `path` or a folder above it is being renamed.
    fn is_renaming(&self, path: &str) -> bool {
        self.renaming
            .iter()
            .chain(self.uncertain_renames.keys())
            .chain(self.uncertain_renames.values())
            .any(|renamed| path == renamed || path.starts_with(&format!("{renamed}/")))
    }

    pub(crate) fn save_blocks_rename(&self, path: &str) -> bool {
        self.renaming
            .iter()
            .chain(self.uncertain_renames.keys())
            .chain(self.uncertain_renames.values())
            .any(|pending| path::is_within(path, pending) || path::is_within(pending, path))
            || self.open.as_ref().is_some_and(|open| {
                open.file.is_saving()
                    && (open.file.path() == path
                        || open.file.path().starts_with(&format!("{path}/")))
            })
    }

    /// Restarts the 1.5 s autosave debounce while the draft is unsaved.
    fn arm_autosave(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        let id = open.id;
        open.autosave_waiting = false;
        open.autosave =
            (self.auto_save && open.file.is_dirty() && !open.file.read_only()).then(|| {
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(AUTOSAVE_DELAY).await;
                    let _ = this.update(cx, |this, cx| {
                        let Some(open) = this.editor_mut(id) else {
                            return;
                        };
                        open.autosave = None;
                        let _ = this.save(SaveTrigger::Auto, cx);
                    });
                })
            });
    }

    fn set_auto_save(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.auto_save == enabled {
            return;
        }
        self.auto_save = enabled;
        self.arm_autosave(cx);
        cx.notify();
    }

    fn sync_auto_save(&mut self, cx: &mut Context<Self>) {
        let value = self
            .scope
            .owner
            .as_ref()
            .and_then(|owner| owner.upgrade())
            .and_then(|owner| {
                let owner = owner.read(cx);
                owner
                    .settings
                    .as_ref()
                    .and_then(|settings| {
                        settings
                            .read(cx)
                            .general_preferences(&self.scope.runtime, cx)
                    })
                    .and_then(|preferences| preferences.auto_save_enabled)
                    .or_else(|| {
                        (owner.runtime == self.scope.runtime)
                            .then(|| {
                                owner
                                    .send_catalog
                                    .settings
                                    .as_ref()
                                    .and_then(|settings| settings.auto_save_enabled)
                            })
                            .flatten()
                    })
            });
        if let Some(value) = value {
            self.set_auto_save(value, cx);
        }
    }

    /// A rename of `old` to `new` finished: the open file and a pending
    /// switch under it follow, as `moveWorkspacePath` moves open paths.
    pub(crate) fn path_moved(&mut self, old: &str, new: &str, cx: &mut Context<Self>) {
        let moved = |path: &str| {
            if path == old {
                Some(new.to_owned())
            } else {
                path.strip_prefix(&format!("{old}/"))
                    .map(|rest| format!("{new}/{rest}"))
            }
        };
        if let Some(open) = self.open.as_mut()
            && let Some(path) = moved(open.file.path())
        {
            open.file.moved(path);
        }
        if let Some(switch) = self.switch.as_mut()
            && let Some(path) = moved(&switch.path)
        {
            switch.path = path;
        }
        if let Some(path) = self.tree.selected().and_then(moved) {
            self.tree.select(Some(path));
        }
        cx.notify();
    }

    /// A rename of `old` ended: saves under it are allowed again, and an
    /// unsaved draft gets its autosave back.
    pub(crate) fn rename_finished(&mut self, old: &str, cx: &mut Context<Self>) {
        self.rename_revisions.remove(old);
        self.uncertain_renames.remove(old);
        self.renaming.retain(|renamed| renamed != old);
        self.arm_autosave(cx);
    }

    /// Reads the open file's content and stat from scratch.
    fn load_open_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.scope.client(cx) else {
            if let Some(open) = self.open.as_mut() {
                open.file.load_failed(ReadFailure::Failed(None));
            }
            cx.notify();
            return;
        };
        let Some(open) = self.open.as_mut() else {
            return;
        };
        let directory = self.scope.directory.clone();
        let path = open.file.path().to_owned();
        let requested_path = path.clone();
        let outside = open.outside;
        let task = self.scope.run(async move {
            let content = client.read_file(&directory, &path, outside).await;
            let stat = client.stat_file(&directory, &path, outside).await;
            (content, stat)
        });
        let id = open.id;
        open.load = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(open) = this.editor_mut(id) else {
                    return;
                };
                open.load = None;
                if open.file.path() != requested_path {
                    this.load_open_file(window, cx);
                    return;
                }
                let Some((content, stat)) = outcome else {
                    open.file.load_failed(ReadFailure::Failed(None));
                    cx.notify();
                    return;
                };
                match content {
                    Ok(content) => {
                        let text = open.file.loaded(&content).to_owned();
                        open.preview = open.file.is_large();
                        open.applying = true;
                        open.editor
                            .update(cx, |editor, cx| editor.set_value(text, window, cx));
                        open.applying = false;
                        if let Ok(StatRead::Found { stat, .. }) = stat {
                            let revision = open.file.revision();
                            open.file.record_stat(revision, stat);
                        }
                    }
                    Err(error) => open.file.load_failed(read_failure(error)),
                }
                cx.notify();
            });
        }));
    }

    pub(crate) fn retry_open_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(open) = self.open.as_mut() {
            open.file = OpenFile::new(open.file.path().to_owned(), open.outside);
        }
        self.load_open_file(window, cx);
        cx.notify();
    }

    fn edited(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        if open.applying {
            return;
        }
        let was_dirty = open.file.is_dirty();
        open.file.edit(text);
        open.status = SaveStatus::Idle;
        let now_dirty = open.file.is_dirty();
        // Every edit restarts the debounce.
        self.arm_autosave(cx);
        if was_dirty != now_dirty {
            cx.notify();
        }
    }

    /// Writes one snapshot of the draft. A save of a path being renamed is
    /// refused, since the write would recreate the old path; the rename's
    /// end arms autosave again.
    pub(crate) fn save(&mut self, trigger: SaveTrigger, cx: &mut Context<Self>) -> SaveStart {
        if trigger == SaveTrigger::Auto && !self.auto_save {
            return SaveStart::Refused;
        }
        let Some(client) = self.scope.client(cx) else {
            if trigger == SaveTrigger::Auto
                && let Some(open) = self.open.as_mut()
            {
                open.autosave_waiting = true;
            }
            return SaveStart::Refused;
        };
        let renaming = self
            .open
            .as_ref()
            .is_some_and(|open| self.is_renaming(open.file.path()));
        let Some(open) = self.open.as_mut() else {
            return SaveStart::Refused;
        };
        if renaming {
            return SaveStart::Refused;
        }
        let request = match open.file.begin_save(trigger) {
            Ok(request) => request,
            Err(SaveRefusal::Clean) => {
                if trigger == SaveTrigger::Explicit {
                    open.status = SaveStatus::Saved;
                    cx.notify();
                }
                return SaveStart::Clean;
            }
            Err(_) => return SaveStart::Refused,
        };
        open.autosave = None;
        let directory = self.scope.directory.clone();
        let path = open.file.path().to_owned();
        let content = request.content.clone();
        let task = self.scope.run(async move {
            let written = client.write_file(&directory, &path, &content).await;
            // The stat after the write, so the poll does not read our own
            // write as an external change.
            let stat = match &written {
                Ok(()) => client.stat_file(&directory, &path, false).await.ok(),
                Err(_) => None,
            };
            (written, stat)
        });
        let id = open.id;
        open.save = Some(cx.spawn(async move |this, cx| {
            let outcome = task.await;
            let _ = this.update(cx, |this, cx| {
                let Some(open) = this.editor_mut(id) else {
                    return;
                };
                open.save = None;
                let strings = locale::strings(cx);
                let success = matches!(&outcome, Some((Ok(()), _)));
                let mut notice = None;
                match outcome {
                    Some((Ok(()), stat)) => {
                        open.file.save_finished(request, true);
                        if let Some(StatRead::Found { stat, .. }) = stat {
                            let revision = open.file.revision();
                            open.file.record_stat(revision, stat);
                        }
                        open.status = SaveStatus::Saved;
                    }
                    Some((Err(error), _)) => {
                        open.file.save_finished(request, false);
                        notice = Some(Notice::error(
                            strings.get("filesView.toast.saveFailed"),
                            error.message(),
                        ));
                    }
                    None => open.file.save_finished(request, false),
                }
                let saved = success && !open.file.is_dirty() && open.file.failure().is_none();
                this.notice = notice;
                // The dialog's switch follows only a save that left nothing
                // unsaved; otherwise the dialog stays with the draft.
                if let Some(switch) = this.switch.as_mut().filter(|switch| switch.saving) {
                    switch.saving = false;
                    if saved {
                        this.open_next = this.switch.take().map(|switch| switch.path);
                    }
                }
                // Edits made during the write, or a failed write, keep the
                // draft unsaved; autosave tries again after its delay.
                this.arm_autosave(cx);
                cx.notify();
            });
        }));
        cx.notify();
        SaveStart::Started
    }

    fn toggle_preview(&mut self, cx: &mut Context<Self>) {
        if let Some(open) = self.open.as_mut() {
            open.preview = !open.preview;
            cx.notify();
        }
    }

    /// One 2 s tick: the open file's stat, and every fourth tick the
    /// expanded folders. Nothing runs while the surface is hidden.
    fn poll_tick(&mut self, cx: &mut Context<Self>) {
        if !self.shown.get() {
            return;
        }
        self.poll_ticks = self.poll_ticks.wrapping_add(1);
        if self.poll_ticks.is_multiple_of(FOLDER_POLL_EVERY) {
            for ticket in self.tree.refresh_all(ListingCause::Poll) {
                if self
                    .tree
                    .entry_count(&ticket.path)
                    .is_some_and(|count| count > FOLDER_POLL_MAX_ENTRIES)
                {
                    continue;
                }
                self.list(ticket, cx);
            }
        }
        self.poll_open_file(cx);
    }

    pub(crate) fn poll_open_file(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        if self.polling {
            return;
        }
        let Some(open) = self.open.as_mut() else {
            return;
        };
        if open.load.is_some() || open.read.is_some() || open.change.is_some() {
            return;
        }
        self.polling = true;
        let directory = self.scope.directory.clone();
        let path = open.file.path().to_owned();
        let requested_path = path.clone();
        let outside = open.outside;
        let task = self.scope.run(async move {
            match client.stat_file(&directory, &path, outside).await {
                Ok(StatRead::Found { stat, .. }) => (PollStat::Found(stat), Some(stat)),
                Ok(StatRead::Missing) => (PollStat::Missing, None),
                Err(_) => (PollStat::Failed, None),
            }
        });
        let id = open.id;
        open.read = Some(cx.spawn(async move |this, cx| {
            let outcome = task.await;
            let _ = this.update(cx, |this, cx| {
                this.polling = false;
                let Some(open) = this.editor_mut(id) else {
                    return;
                };
                open.read = None;
                if open.file.path() != requested_path {
                    return;
                }
                let Some((stat, found)) = outcome else {
                    return;
                };
                let failed_before = open.file.failure().is_some();
                match open.file.poll(stat) {
                    PollAction::Nothing => {
                        if failed_before != open.file.failure().is_some() {
                            cx.notify();
                        }
                    }
                    PollAction::Reload => this.reload_open_file(cx),
                    PollAction::ReadContent { revision } => {
                        if let Some(stat) = found {
                            this.read_change(revision, stat, cx);
                        }
                    }
                }
            });
        }));
    }

    fn reload_open_file(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        open.file = OpenFile::new(open.file.path().to_owned(), open.outside);
        // Setting the editor's text needs the window; the next frame has it.
        open.needs_load = true;
        cx.notify();
    }

    /// Reads an external change; a second read must agree before it shows.
    fn read_change(
        &mut self,
        revision: u64,
        stat: openchamber_native_core::file_draft::FileStat,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let Some(open) = self.open.as_mut() else {
            return;
        };
        let directory = self.scope.directory.clone();
        let path = open.file.path().to_owned();
        let requested_path = path.clone();
        let outside = open.outside;
        let task = self.scope.run(async move {
            let first = client.read_file(&directory, &path, outside).await;
            let second = client.read_file(&directory, &path, outside).await;
            (first, second)
        });
        let id = open.id;
        open.change = Some(cx.spawn(async move |this, cx| {
            let outcome = task.await;
            let Some((Ok(first), Ok(second))) = outcome else {
                let _ = this.update(cx, |this, _| {
                    if let Some(open) = this.editor_mut(id) {
                        open.change = None;
                    }
                });
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let Some(open) = this.editor_mut(id) else {
                    return;
                };
                open.change = None;
                if open.file.path() != requested_path {
                    return;
                }
                let _ = open.file.observe_content(revision, &first, stat);
                if open.file.observe_content(revision, &second, stat)
                    == Some(openchamber_native_core::file_draft::ContentStep::Applied)
                {
                    open.pending_apply = true;
                    cx.notify();
                }
            });
        }));
    }
}

pub(crate) fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

fn read_failure(error: FilesError) -> ReadFailure {
    match error {
        FilesError::NotFound => ReadFailure::NotFound,
        FilesError::PermissionDenied => ReadFailure::PermissionDenied,
        other => ReadFailure::Failed(other.message().map(str::to_owned)),
    }
}

fn read_failure_text(failure: &ReadFailure, strings: &Strings) -> SharedString {
    let title = strings.get("filesView.error.readFileFailed");
    match failure {
        ReadFailure::Failed(Some(message)) => format!("{title}: {message}").into(),
        _ => title,
    }
}

impl FilesView {
    /// The unsaved-changes dialog (`confirmDiscardOpen`) over the surface:
    /// Save changes, Discard and Cancel. Save waits for the write; a failed
    /// or refused save keeps the dialog and the draft.
    fn render_switch_dialog(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let switch = self.switch.as_ref()?;
        let strings = locale::strings(cx);
        let save_label = if switch.saving {
            strings.get("filesView.editor.saving")
        } else {
            strings.get("filesView.unsaved.saveChanges")
        };
        Some(
            div()
                .id("files-unsaved-backdrop")
                .absolute()
                .inset_0()
                .bg(palette.background.opacity(0.6))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .id("files-unsaved")
                        .role(Role::Dialog)
                        .accessibility_id("files-unsaved")
                        .aria_label(strings.get("filesView.unsaved.title"))
                        .aria_description(strings.get("filesView.unsaved.description"))
                        .test_support()
                        .w(px(320.0))
                        .p(px(16.0))
                        .rounded(px(12.0))
                        .bg(palette.elevated)
                        .shadow(elevation::floating(palette))
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(
                            div()
                                .css_text(ROW_SIZE, ROW_LINE, cx)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(palette.foreground)
                                .child(strings.get("filesView.unsaved.title")),
                        )
                        .child(
                            div()
                                .css_text(ROW_SIZE, ROW_LINE, cx)
                                .text_color(palette.muted_foreground)
                                .child(strings.get("filesView.unsaved.description")),
                        )
                        .child(
                            div()
                                .pt(px(4.0))
                                .flex_row_from(direction)
                                .justify_end()
                                .gap(px(6.0))
                                .child(text_button(
                                    "files-unsaved-cancel",
                                    strings.get("filesView.dialog.cancel"),
                                    None,
                                    palette,
                                    cx,
                                    |this: &mut Self, _, cx| this.cancel_switch(cx),
                                ))
                                .child(text_button(
                                    "files-unsaved-discard",
                                    strings.get("filesView.unsaved.discard"),
                                    None,
                                    palette,
                                    cx,
                                    |this: &mut Self, window, cx| {
                                        this.discard_and_switch(window, cx)
                                    },
                                ))
                                .child(text_button(
                                    "files-unsaved-save",
                                    save_label,
                                    None,
                                    palette,
                                    cx,
                                    |this: &mut Self, _, cx| this.save_and_switch(cx),
                                )),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Applies what changed outside the editor, now that a window exists.
    fn sync_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.open_next.take() {
            self.replace_editor(&path, window, cx);
            return;
        }
        let Some(open) = self.open.as_mut() else {
            return;
        };
        if open.needs_load {
            open.needs_load = false;
            self.load_open_file(window, cx);
            return;
        }
        if open.pending_apply {
            open.pending_apply = false;
            open.applying = true;
            let text = open.file.draft().to_owned();
            open.editor
                .update(cx, |editor, cx| editor.set_value(text, window, cx));
            open.applying = false;
        }
    }

    fn render_header(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let searching = !self.search.read(cx).value().trim().is_empty();
        div()
            .h(px(HEADER_HEIGHT))
            .px(px(8.0))
            .flex_row_from(direction)
            .items_center()
            .gap(px(4.0))
            .child(
                div()
                    .id("files-search")
                    .role(Role::Search)
                    .accessibility_id("files-search")
                    .aria_label(strings.get("filesView.tree.search.placeholder"))
                    .flex_1()
                    .min_w_0()
                    .h(px(28.0))
                    .px(px(8.0))
                    .rounded(px(CONTROL_RADIUS))
                    .bg(palette.background)
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(6.0))
                    .child(glyph("search", 14.0, palette.muted_foreground))
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.search)
                                .appearance(false)
                                .bordered(false)
                                .text_size(gpui_kit::rems(ROW_SIZE)),
                        ),
                    )
                    .when(searching, |this| {
                        this.child(
                            IconButton::new(
                                "files-search-clear",
                                "close",
                                strings.get("filesView.tree.search.clearAria"),
                            )
                            .size(20.0, 14.0)
                            .render(
                                palette,
                                cx,
                                |this: &mut Self, window, cx| this.clear_search(window, cx),
                            ),
                        )
                    }),
            )
            .child(text_button(
                "files-search-names",
                strings.get("sidebarFilesTree.search.mode.names"),
                Some(self.search_mode == SearchMode::Names),
                palette,
                cx,
                |this: &mut Self, _, cx| this.set_search_mode(SearchMode::Names, cx),
            ))
            .child(text_button(
                "files-search-contents",
                strings.get("sidebarFilesTree.search.mode.contents"),
                Some(self.search_mode == SearchMode::Contents),
                palette,
                cx,
                |this: &mut Self, _, cx| this.set_search_mode(SearchMode::Contents, cx),
            ))
            .when(self.search_mode == SearchMode::Contents, |this| {
                this.child(text_button(
                    "files-search-regex",
                    ".*".into(),
                    Some(self.regex),
                    palette,
                    cx,
                    |this: &mut Self, _, cx| {
                        this.regex = !this.regex;
                        this.search_changed(cx);
                    },
                ))
            })
            .child(
                IconButton::new(
                    "files-show-gitignored",
                    if self.tree.show_gitignored() {
                        "eye"
                    } else {
                        "eye-off"
                    },
                    strings.get("filesView.tree.actions.showGitignored"),
                )
                .tone(if self.tree.show_gitignored() {
                    ButtonTone::Active
                } else {
                    ButtonTone::Quiet
                })
                .render(palette, cx, |this: &mut Self, _, cx| {
                    let show = !this.tree.show_gitignored();
                    this.set_show_gitignored(show, cx);
                }),
            )
            .child(
                IconButton::new(
                    "files-refresh",
                    "refresh",
                    strings.get("filesView.tree.actions.refreshTitle"),
                )
                .render(palette, cx, |this: &mut Self, _, cx| this.refresh_tree(cx)),
            )
            .into_any_element()
    }

    fn render_results(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let line = |text: SharedString, color| {
            div()
                .id("files-search-state")
                .role(Role::Status)
                .accessibility_id("files-search-state")
                .aria_label(text.clone())
                .px(px(12.0))
                .py(px(4.0))
                .css_text(ROW_SIZE, ROW_LINE, cx)
                .text_color(color)
                .child(text)
                .into_any_element()
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        let result_row = |id: String, label: SharedString, path: String, cx: &Context<Self>| {
            let hover = palette.hover;
            let element = div()
                .id(gpui_kit::ElementId::Name(id.clone().into()))
                .role(Role::ListItem)
                .accessibility_id(SharedString::from(id))
                .aria_label(label.clone())
                .h(px(PANEL_ROW))
                .mx(px(4.0))
                .px(px(8.0))
                .rounded(px(CONTROL_RADIUS))
                .flex_row_from(direction)
                .items_center()
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .css_text(ROW_SIZE, ROW_LINE, cx)
                        .text_color(palette.foreground)
                        .child(label),
                );
            on_activate(element, cx, move |this: &mut Self, window, cx| {
                this.open_file(&path, window, cx)
            })
            .test_support()
            .into_any_element()
        };
        match &self.search_state {
            SearchState::Idle => {}
            SearchState::Searching => rows.push(line(
                strings.get("filesView.tree.search.searching"),
                palette.muted_foreground,
            )),
            SearchState::Names(hits) if hits.is_empty() => rows.push(line(
                strings.get("sidebarFilesTree.contentSearch.empty"),
                palette.muted_foreground,
            )),
            SearchState::Names(hits) => {
                for hit in hits {
                    rows.push(result_row(
                        format!("files-result:{}", hit.path),
                        hit.relative_path.clone().into(),
                        hit.path.clone(),
                        cx,
                    ));
                }
            }
            SearchState::Contents { matches, stopped } => {
                if matches.is_empty() {
                    rows.push(line(
                        strings.get("sidebarFilesTree.contentSearch.empty"),
                        palette.muted_foreground,
                    ));
                }
                for found in matches {
                    rows.push(result_row(
                        format!("files-match:{}:{}", found.path, found.line),
                        format!(
                            "{}:{}  {}",
                            found.relative_path,
                            found.line,
                            found.text.trim()
                        )
                        .into(),
                        found.path.clone(),
                        cx,
                    ));
                }
                match stopped {
                    Some(SearchStop::Limit) => rows.push(line(
                        strings.format(
                            "sidebarFilesTree.contentSearch.limit",
                            &[("count", &matches.len().to_string())],
                        ),
                        palette.muted_foreground,
                    )),
                    Some(SearchStop::Deadline) => rows.push(line(
                        strings.get("sidebarFilesTree.contentSearch.deadline"),
                        palette.muted_foreground,
                    )),
                    None => {}
                }
            }
            SearchState::InvalidPattern => rows.push(line(
                strings.get("sidebarFilesTree.contentSearch.invalidPattern"),
                palette.status_error,
            )),
            SearchState::Unavailable => rows.push(line(
                strings.get("sidebarFilesTree.contentSearch.unavailable"),
                palette.status_error,
            )),
            SearchState::Failed(message) => rows.push(line(
                strings.format(
                    "sidebarFilesTree.contentSearch.failed",
                    &[("message", message.as_deref().unwrap_or_default())],
                ),
                palette.status_error,
            )),
        }
        div()
            .id("files-results")
            .role(Role::List)
            .accessibility_id("files-results")
            .size_full()
            .py(px(4.0))
            .overflow_y_scroll()
            .children(rows)
            .into_any_element()
    }

    fn render_editor_column(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let Some(open) = self.open.as_ref() else {
            return empty_state(
                strings.get("filesView.editor.pickFileFromTree"),
                palette,
                cx,
            );
        };
        let name: SharedString = file_name(open.file.path()).into();
        let dirty = open.file.is_dirty();
        let state = if open.file.is_saving() {
            Some(strings.get("filesView.editor.saving"))
        } else if dirty {
            Some(strings.get("filesView.unsaved.title"))
        } else if open.status == SaveStatus::Saved {
            Some(strings.get("filesView.editor.saved"))
        } else {
            None
        };
        let can_save =
            !open.file.read_only() && open.file.failure().is_none() && !open.file.is_loading();
        let header = div()
            .h(px(32.0))
            .px(px(8.0))
            .flex_row_from(direction)
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .css_text(ROW_SIZE, ROW_LINE, cx)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(palette.foreground)
                    .child(name.clone()),
            )
            .when(dirty, |this| {
                this.child(div().size(px(6.0)).rounded_full().bg(palette.foreground))
            })
            .child(
                div()
                    .id("files-save-status")
                    .role(Role::Status)
                    .accessibility_id("files-save-status")
                    .test_support()
                    .when_some(state.clone(), |this, state| this.aria_label(state))
                    .min_w_0()
                    .truncate()
                    .css_text(XS_SIZE, XS_LINE, cx)
                    .text_color(palette.muted_foreground)
                    .children(state),
            )
            .child(div().flex_1())
            .when(
                open.file.failure().is_none() && !open.file.is_loading(),
                |this| {
                    this.child(text_button(
                        "files-preview",
                        strings.get("documentToolbar.preview"),
                        Some(open.preview),
                        palette,
                        cx,
                        |this: &mut Self, _, cx| this.toggle_preview(cx),
                    ))
                },
            )
            .when(can_save, |this| {
                this.child(
                    IconButton::new(
                        "files-save",
                        "save-3",
                        strings.get("filesView.editor.saveFile"),
                    )
                    .render(palette, cx, |this: &mut Self, _, cx| {
                        let _ = this.save(SaveTrigger::Explicit, cx);
                    }),
                )
            });
        let body: AnyElement = if let Some(failure) = open.file.failure() {
            div()
                .id("files-error")
                .role(Role::Alert)
                .accessibility_id("files-error")
                .test_support()
                .aria_label(read_failure_text(failure, strings))
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .child(
                    div()
                        .css_text(ROW_SIZE, ROW_LINE, cx)
                        .text_color(palette.status_error)
                        .child(read_failure_text(failure, strings)),
                )
                .child(text_button(
                    "files-retry",
                    strings.get("diffView.actions.retry"),
                    None,
                    palette,
                    cx,
                    |this: &mut Self, window, cx| this.retry_open_file(window, cx),
                ))
                .into_any_element()
        } else if open.file.is_loading() {
            empty_state(strings.get("filesView.state.loading"), palette, cx)
        } else if let Some(page) = self.html_preview_element() {
            page
        } else {
            div()
                .id("files-editor")
                .accessibility_id("files-editor")
                .test_support()
                .flex_1()
                .min_h_0()
                .size_full()
                .child(
                    Editor::new(&open.editor)
                        .readonly(open.preview || open.file.read_only())
                        .bordered(false)
                        .aria_label(name)
                        .h_full(),
                )
                .into_any_element()
        };
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(header)
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }
}

impl FilesView {
    /// Keeps the HTML preview in step with the open file: a page while the
    /// preview of an HTML file is on, granted again after each load or save.
    #[cfg(target_os = "linux")]
    fn sync_html_preview(&mut self, cx: &mut Context<Self>) {
        let wanted = self.open.as_ref().filter(|open| {
            open.preview
                && crate::html_preview_view::is_html(open.file.path())
                && !open.file.is_loading()
                && open.file.failure().is_none()
        });
        let Some(open) = wanted else {
            self.html_preview = None;
            return;
        };
        let (path, revision, outside) = (
            open.file.path().to_owned(),
            open.file.revision(),
            open.outside,
        );
        if self
            .html_preview
            .as_ref()
            .is_some_and(|view| view.read(cx).shows(&path, revision))
        {
            return;
        }
        let mut scope = self.scope.clone();
        self.html_preview = Some(cx.new(|cx| {
            crate::html_preview_view::HtmlPreviewView::new(&mut scope, path, revision, outside, cx)
        }));
    }

    #[cfg(target_os = "linux")]
    fn html_preview_element(&self) -> Option<AnyElement> {
        self.html_preview.clone().map(|view| {
            div()
                .id("files-html-preview")
                .accessibility_id("files-html-preview")
                .size_full()
                .child(view)
                .into_any_element()
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn html_preview_element(&self) -> Option<AnyElement> {
        None
    }
}

impl Focusable for FilesView {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.tree_focus.clone()
    }
}

impl Render for FilesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let placeholder = locale::strings(cx).get("filesView.tree.search.placeholder");
        if self.search.read(cx).presentation().placeholder() != &placeholder {
            self.search.update(cx, |input, cx| {
                input.set_placeholder(placeholder, window, cx)
            });
        }
        self.sync_editor(window, cx);
        #[cfg(target_os = "linux")]
        self.sync_html_preview(cx);
        let palette = cx.global::<Palette>().clone();
        let direction = locale::strings(cx).direction();
        let searching = !self.search.read(cx).value().trim().is_empty();
        let tree = if searching {
            self.render_results(&palette, direction, cx)
        } else {
            self.render_tree(&palette, direction, cx)
        };
        let label = locale::strings(cx).get("contextPanel.mode.files");
        div()
            .id("files-panel")
            .role(Role::Group)
            .accessibility_id("files-panel")
            .aria_label(label)
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(|this, _: &SaveFile, _, cx| {
                this.save(SaveTrigger::Explicit, cx);
            }))
            .on_action(cx.listener(Self::on_rename_escape))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_header(&palette, direction, cx))
            .child(status_line(
                "files-notice",
                self.notice.as_ref(),
                &palette,
                direction,
                cx,
            ))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex_row_from(direction)
                    .child(div().w(px(TREE_WIDTH)).flex_shrink_0().h_full().child(tree))
                    .child(div().w(px(1.0)).h_full().bg(palette.divider))
                    .child(self.render_editor_column(&palette, direction, cx)),
            )
            .children(self.render_switch_dialog(&palette, direction, cx))
    }
}

#[cfg(test)]
mod resize_tests {
    use super::*;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{
        MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, PlatformInput, Point,
        TestAppContext, point, size,
    };

    fn drag(window: &mut Window, start: Point<Pixels>, delta: Pixels, cx: &mut gpui_kit::App) {
        window.dispatch_event(
            PlatformInput::MouseMove(MouseMoveEvent {
                position: start,
                ..Default::default()
            }),
            cx,
        );
        window.dispatch_event(
            PlatformInput::MouseDown(MouseDownEvent {
                position: start,
                button: MouseButton::Left,
                click_count: 1,
                ..Default::default()
            }),
            cx,
        );
        let end = point(start.x + delta, start.y);
        window.dispatch_event(
            PlatformInput::MouseMove(MouseMoveEvent {
                position: end,
                pressed_button: Some(MouseButton::Left),
                ..Default::default()
            }),
            cx,
        );
        window.dispatch_event(
            PlatformInput::MouseUp(MouseUpEvent {
                position: end,
                button: MouseButton::Left,
                click_count: 1,
                ..Default::default()
            }),
            cx,
        );
        window.render_frame(cx);
    }

    fn check_handles(language: &str, cx: &mut TestAppContext) {
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.simulate_window_resize(window, size(px(1440.0), px(900.0)));
        cx.update_window(window, |_, window, cx| {
            cx.set_global(Strings::load(language));
            workspace.update(cx, |workspace, cx| {
                workspace.panels.open = Some(PanelSurface::Files);
                workspace.pane_sizes.panel_width = Some(640.0);
                cx.notify();
            });
            window.render_frame(cx);
            let direction = locale::strings(cx).direction();
            let rtl = direction == Direction::RightToLeft;
            for change in [80.0, -80.0] {
                let slot = window.find("side-panel").bounds();
                let handle = window.find("panel-resize").bounds();
                let edge = if rtl {
                    slot.right() - px(8.0)
                } else {
                    slot.left() + px(8.0)
                };
                assert_eq!(
                    handle.left(),
                    edge - px(4.0),
                    "handle straddles the card edge"
                );
                assert_eq!(handle.size.width, px(8.0));
                assert_eq!(handle.top(), slot.top());
                assert_eq!(handle.bottom(), slot.bottom() - px(8.0));
                let start = point(edge, handle.center().y);
                drag(window, start, px(if rtl { change } else { -change }), cx);
                assert_eq!(
                    workspace.read(cx).fitted_panel_width(window),
                    f32::from(slot.size.width) + change,
                    "real dispatch from the card-facing gutter resizes in {language}"
                );
                assert!(workspace.read(cx).pane_sizes.dragging().is_none());
            }

            workspace.update(cx, |workspace, cx| {
                workspace.panels.open = None;
                workspace.terminal_open = true;
                cx.notify();
            });
            window.render_frame(cx);
            let terminal = window.find("terminal-panel").bounds();
            let handle = window.find("panel-resize").bounds();
            let start = if rtl {
                assert_eq!(handle.right(), terminal.right() + px(6.0));
                terminal.right() - px(1.0)
            } else {
                assert_eq!(handle.left(), terminal.left() - px(6.0));
                terminal.left() + px(1.0)
            };
            assert_eq!(handle.size.width, px(8.0));
            assert_eq!(handle.top(), terminal.top());
            assert_eq!(handle.bottom(), terminal.bottom());
            drag(
                window,
                point(start, handle.center().y),
                px(if rtl { -80.0 } else { 80.0 }),
                cx,
            );
            assert_eq!(
                workspace.read(cx).fitted_panel_width(window),
                f32::from(terminal.size.width) - 80.0,
                "the terminal retains its separate handle placement and dispatch"
            );
        })
        .expect("resize test window remains open");
    }

    #[gpui_kit::test]
    fn card_edge_and_terminal_handles_dispatch_in_ltr(cx: &mut TestAppContext) {
        check_handles("en", cx);
    }

    #[gpui_kit::test]
    fn card_edge_and_terminal_handles_dispatch_in_rtl(cx: &mut TestAppContext) {
        check_handles("ar", cx);
    }
}
