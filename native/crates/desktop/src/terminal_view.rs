//! The terminal panel: tabs, their server terminals and the socket.
//!
//! Mirrors `packages/ui/src/components/views/TerminalView.tsx`. The panel
//! owns one [`TerminalSocket`] for the runtime; every tab with a server
//! terminal stays attached while the window lives, so a tab that is not shown
//! keeps parsing its output. Closing the panel only hides it, and dropping it
//! (window close, runtime switch) detaches without stopping any terminal:
//! the server reaps an unattached terminal after 30 idle minutes.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Duration;

use gpui_kit::component::Theme;
use gpui_kit::{
    Anchor, AnchoredPositionMode, AnyElement, App, AppContext, Context, Edges, Entity, FocusHandle,
    InteractiveElement, IntoElement, KeyBinding, KeyContext, Keymap, NoAction, ParentElement,
    Pixels, Point, Render, Role, SharedString, StatefulInteractiveElement, Styled, Subscription,
    Task, TestSupportExt, Window, anchored, deferred, div, font, is_no_action,
    prelude::FluentBuilder, px,
};
use openchamber_native_core::ids::RuntimeId;
use openchamber_native_core::terminal_tabs::{
    ActionRun, Lifecycle, ListedTerminal, TabId, TerminalTabs,
};
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::terminal::{
    ActionCommand, CreateTerminal, CreatedTerminal, TerminalApi, TerminalCommands, TerminalEvent,
    TerminalPurpose, TerminalRequestError, TerminalSettings, TerminalSocket, TerminalStatus,
    ThemeMode,
};

use crate::activation::on_activate;
use crate::chrome::{CONTROL_RADIUS, IconButton, glyph};
use crate::elevation;
use crate::locale::{self, Strings};
use crate::network::NetworkTask;
use crate::terminal_emulator::TerminalColors;
use crate::terminal_surface::{self, SurfaceEvent, TerminalSurface};
use crate::theme::{Appearance, Palette};

const HEADER_HEIGHT: f32 = 36.0;
const TAB_HEIGHT: f32 = 28.0;
/// Open clients keep their terminals from the idle sweep
/// (`useTerminalSessionKeepalive.ts`).
const KEEPALIVE: Duration = Duration::from_secs(10 * 60);
/// The size a terminal starts with before its grid is measured.
const PROVISIONAL: (u16, u16) = (80, 24);
/// How long a stopping run has to exit after Ctrl+C before it is killed
/// (`stopProjectActionTerminalSession`).
const STOP_GRACE: Duration = Duration::from_secs(1);

/// The key contexts above a focused terminal, outermost first: Kit's window
/// root, the workspace and the surface.
pub(crate) fn terminal_context_stack() -> Vec<KeyContext> {
    [
        "Root",
        crate::workspace_view::KEY_CONTEXT,
        terminal_surface::KEY_CONTEXT,
    ]
    .into_iter()
    .map(|context| KeyContext::parse(context).expect("a valid key context"))
    .collect()
}

/// Gives the focused shell every chord a binding above it claims, except
/// `app_chords`: the terminal toggle, as `handleTerminalShortcutCapture` lets
/// it through. The surface's own key handler keeps the copy, paste and zoom
/// chords. Runs after every other binding is registered: a later `NoAction`
/// outranks earlier bindings and cancels their pending chords. The bindings
/// belong to `shortcuts::bind`, which rebuilds them with the shortcuts.
pub(crate) fn bind_keys(app_chords: &[String], cx: &mut App) {
    let chords = shadowed_chords(&cx.key_bindings().borrow(), app_chords);
    cx.bind_keys(chords.iter().map(|keys| {
        KeyBinding::new(keys, NoAction, Some(terminal_surface::KEY_CONTEXT))
            .with_meta(crate::shortcuts::OWNED)
    }));
}

/// The chords of every live binding enabled above a focused terminal. A
/// multi-key chord brings its first key too, which would otherwise wait for
/// the rest of the chord.
fn shadowed_chords(keymap: &Keymap, app_chords: &[String]) -> BTreeSet<String> {
    let stack = terminal_context_stack();
    let mut chords = BTreeSet::new();
    for binding in keymap.bindings() {
        if is_no_action(binding.action()) {
            continue;
        }
        if binding
            .predicate()
            .is_some_and(|predicate| predicate.depth_of(&stack).is_none())
        {
            continue;
        }
        let keys: Vec<String> = binding
            .keystrokes()
            .iter()
            .map(|keystroke| keystroke.unparse())
            .collect();
        let Some(first) = keys.first() else {
            continue;
        };
        if app_chords.contains(first) {
            continue;
        }
        if keys.len() > 1 {
            chords.insert(first.clone());
        }
        chords.insert(keys.join(" "));
    }
    chords
}

/// Why a tab shows no live terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Notice {
    StartFailed { cwd_missing: bool },
    SessionEnded,
    ConnectionFailed(String),
    SendFailed,
    ClipboardFailed { paste: bool },
}

/// The open context menu: where, and for which tab.
#[derive(Clone, Copy, Debug)]
struct Menu {
    position: Point<Pixels>,
    tab: TabId,
}

/// What the panel reaches the server through.
struct Server {
    api: TerminalApi,
    runtime: tokio::runtime::Handle,
    commands: TerminalCommands,
    _events: Task<()>,
}

pub(crate) struct TerminalPanel {
    pub(crate) runtime: RuntimeId,
    generation: Option<u64>,
    server: Option<Server>,
    settings: TerminalSettings,
    general_revision: [u64; 2],
    tabs: TerminalTabs,
    surfaces: HashMap<TabId, (Entity<TerminalSurface>, Subscription)>,
    colors: TerminalColors,
    mono_font: SharedString,
    /// Directories whose server terminals were adopted since the socket last
    /// connected.
    adopted: HashSet<String>,
    directory: Option<String>,
    notices: HashMap<TabId, Notice>,
    reconnecting: bool,
    menu: Option<Menu>,
    focus: FocusHandle,
    pending_focus: Option<(String, gpui_kit::AnyWindowHandle)>,
    /// Directories whose action runs the header shows; their terminals are
    /// listed again when the server says its terminals changed.
    observed: BTreeSet<String>,
    /// Each action's glyph, for its tab.
    action_glyphs: HashMap<String, &'static str>,
    _keepalive: Option<Task<()>>,
    _settings: Option<Task<()>>,
    _appearance: [Subscription; 2],
}

impl std::fmt::Debug for TerminalPanel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TerminalPanel")
            .field("runtime", &self.runtime)
            .field("tabs", &self.tabs)
            .finish_non_exhaustive()
    }
}

impl TerminalPanel {
    /// General's current choice wins over a settings read started earlier.
    pub(crate) fn apply_general_settings(
        &mut self,
        close_on_exit: Option<bool>,
        shell: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        if let Some(value) = close_on_exit {
            self.general_revision[0] += 1;
            self.settings.close_on_exit = value;
        }
        if let Some(value) = shell {
            self.general_revision[1] += 1;
            self.settings.shell = value.to_owned();
        }
        cx.notify();
    }

    /// A panel for `runtime`. Without a client (synthetic data, or a server
    /// not yet authenticated) it shows why no terminal can start.
    pub fn new(
        runtime: RuntimeId,
        client: Option<(ServerClient, tokio::runtime::Handle)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let server = client.map(|(client, handle)| {
            let socket = {
                let _entered = handle.enter();
                TerminalSocket::start(client.clone())
            };
            let commands = socket.commands();
            let events = cx.spawn(async move |this, cx| {
                let mut socket = socket;
                while let Some(event) = socket.next().await {
                    if this
                        .update(cx, |this, cx| this.on_socket_event(event, cx))
                        .is_err()
                    {
                        return;
                    }
                }
            });
            Server {
                api: TerminalApi::new(client),
                runtime: handle,
                commands,
                _events: events,
            }
        });
        let mut panel = Self {
            runtime,
            generation: None,
            server,
            settings: TerminalSettings::default(),
            general_revision: [0; 2],
            tabs: TerminalTabs::new(),
            surfaces: HashMap::new(),
            colors: terminal_surface::terminal_colors(cx.global::<Palette>()),
            mono_font: Theme::global(cx).mono_font_family.clone(),
            adopted: HashSet::new(),
            directory: None,
            notices: HashMap::new(),
            reconnecting: false,
            menu: None,
            focus: cx.focus_handle(),
            pending_focus: None,
            observed: BTreeSet::new(),
            action_glyphs: HashMap::new(),
            _keepalive: None,
            _settings: None,
            _appearance: [
                cx.observe_global::<Palette>(Self::refresh_appearance),
                cx.observe_global::<Theme>(Self::refresh_appearance),
            ],
        };
        panel.load_settings(cx);
        panel._keepalive = panel.server.is_some().then(|| {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(KEEPALIVE).await;
                    if this.update(cx, |this, _| this.touch()).is_err() {
                        return;
                    }
                }
            })
        });
        panel
    }

    /// Global changes reach hidden tabs too. Compare once per notification,
    /// before visiting surfaces; unrelated interface settings change neither.
    fn refresh_appearance(&mut self, cx: &mut Context<Self>) {
        let colors = terminal_surface::terminal_colors(cx.global::<Palette>());
        let mono_font = Theme::global(cx).mono_font_family.clone();
        let colors_changed = self.colors != colors;
        let font_changed = self.mono_font != mono_font;
        if !colors_changed && !font_changed {
            return;
        }
        for (surface, _) in self.surfaces.values() {
            surface.update(cx, |surface, cx| {
                if colors_changed {
                    surface.set_colors(colors.clone(), cx);
                }
                if font_changed {
                    surface.set_font(font(mono_font.clone()), cx);
                }
            });
        }
        self.colors = colors;
        self.mono_font = mono_font;
        cx.notify();
    }

    /// Fire-and-forget work, such as a resize or a close: it completes even
    /// when the panel goes away first.
    fn spawn_detached(&self, work: impl std::future::Future<Output = ()> + Send + 'static) {
        if let Some(server) = &self.server {
            drop(server.runtime.spawn(work));
        }
    }

    fn run<T: Send + 'static>(
        &self,
        work: impl std::future::Future<Output = T> + Send + 'static,
    ) -> Option<NetworkTask<T>> {
        let server = self.server.as_ref()?;
        Some(NetworkTask::spawn_on(&server.runtime, work))
    }

    fn load_settings(&mut self, cx: &mut Context<Self>) {
        let Some(server) = &self.server else {
            return;
        };
        let api = server.api.clone();
        let general_revision = self.general_revision;
        let Some(read) = self.run(async move { api.settings().await }) else {
            return;
        };
        self._settings = Some(cx.spawn(async move |this, cx| {
            if let Some(Ok(mut settings)) = read.await {
                let _ = this.update(cx, |this, cx| {
                    if this.general_revision[0] != general_revision[0] {
                        settings.close_on_exit = this.settings.close_on_exit;
                    }
                    if this.general_revision[1] != general_revision[1] {
                        settings.shell = this.settings.shell.clone();
                    }
                    this.settings = settings;
                    cx.notify();
                });
            }
        }));
    }

    fn touch(&self) {
        let Some(server) = &self.server else {
            return;
        };
        let live: Vec<String> = self
            .tabs
            .sessions()
            .filter(|session| {
                self.tabs
                    .find_session(session)
                    .is_some_and(|(_, tab)| tab.lifecycle == Lifecycle::Running)
            })
            .map(str::to_owned)
            .collect();
        let Some(directory) = self.directory.clone() else {
            return;
        };
        if live.is_empty() {
            return;
        }
        let api = server.api.clone();
        self.spawn_detached(async move {
            if let Err(error) = api.touch(&live, &directory).await {
                tracing::debug!(%error, "the terminals could not be kept alive");
            }
        });
    }

    /// Shows `directory`'s terminals, adopting the server's and starting one
    /// when it has none.
    pub fn has_server(&self) -> bool {
        self.server.is_some()
    }

    pub fn show_directory(
        &mut self,
        directory: Option<String>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let changed = self.directory != directory;
        if changed {
            self.pending_focus = None;
            self.directory = directory.clone();
            self.menu = None;
            cx.notify();
        }
        let Some(directory) = directory else {
            return;
        };
        if self.server.is_some() && !self.adopted.contains(&directory) {
            self.adopted.insert(directory.clone());
            self.adopt(&directory, true, cx);
        }
        if focus {
            self.focus_active(window, cx);
            self.pending_focus = self
                .tabs
                .active(&directory)
                .filter(|tab| self.surfaces.contains_key(&tab.id))
                .is_none()
                .then(|| (directory, window.window_handle()));
        }
    }

    fn adopt(&mut self, directory: &str, open_if_empty: bool, cx: &mut Context<Self>) {
        let Some(server) = &self.server else {
            return;
        };
        let api = server.api.clone();
        let read_directory = directory.to_owned();
        let Some(read) = self.run(async move { api.sessions(&read_directory).await }) else {
            return;
        };
        let directory = directory.to_owned();
        cx.spawn(async move |this, cx| {
            let outcome = read.await;
            let _ = this.update(cx, |this, cx| {
                match outcome {
                    Some(Ok(listed)) => {
                        let listed: Vec<ListedTerminal> = listed
                            .into_iter()
                            .map(|terminal| ListedTerminal {
                                running: terminal.status == TerminalStatus::Running,
                                is_terminal: terminal.interactive
                                    && terminal.purpose == TerminalPurpose::Terminal,
                                action: match terminal.purpose {
                                    TerminalPurpose::ProjectAction {
                                        action_id,
                                        execution_id,
                                    } => Some((action_id, execution_id)),
                                    TerminalPurpose::Terminal => None,
                                },
                                session: terminal.session_id,
                            })
                            .collect();
                        for session in this.tabs.adopt(&directory, &listed) {
                            this.attach_adopted(&session, cx);
                        }
                    }
                    // A failed read changes nothing; the tabs keep their state.
                    Some(Err(error)) => {
                        tracing::debug!(%error, "the terminal list could not be read");
                        this.adopted.remove(&directory);
                    }
                    None => {}
                }
                if open_if_empty && this.tabs.tabs(&directory).is_empty() {
                    this.new_tab_in(&directory, cx);
                }
                this.focus_requested_surface(&directory, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn attach_adopted(&mut self, session: &str, cx: &mut Context<Self>) {
        let Some((_, tab)) = self.tabs.find_session(session) else {
            return;
        };
        let id = tab.id;
        let (cols, rows) = self.provisional_size(cx);
        let surface = self.new_surface(id, cols, rows, cx);
        if let Some(server) = &self.server {
            surface.update(cx, |surface, _| {
                surface.set_writer(server.commands.clone(), session.to_owned());
            });
            server.commands.attach(session);
        }
    }

    /// The size a new terminal starts with: the shown surface's grid when
    /// one was measured, else 80 by 24.
    fn provisional_size(&self, cx: &App) -> (u16, u16) {
        self.surfaces
            .values()
            .map(|(surface, _)| surface.read(cx))
            .find(|surface| surface.measured())
            .map_or(PROVISIONAL, TerminalSurface::grid_size)
    }

    fn new_surface(
        &mut self,
        id: TabId,
        cols: u16,
        rows: u16,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalSurface> {
        let palette = cx.global::<Palette>().clone();
        let family = Theme::global(cx).mono_font_family.clone();
        let font_size = self.settings.font_size;
        let surface =
            cx.new(|cx| TerminalSurface::new(cols, rows, font(family), font_size, &palette, cx));
        let subscription = cx.subscribe(&surface, move |this, _, event, cx| {
            this.on_surface_event(id, event, cx);
        });
        self.surfaces.insert(id, (surface.clone(), subscription));
        surface
    }

    pub fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(directory) = self.directory.clone() {
            self.new_tab_in(&directory, cx);
            self.focus_active(window, cx);
        }
    }

    fn new_tab_in(&mut self, directory: &str, cx: &mut Context<Self>) {
        let Some(api) = self.server.as_ref().map(|server| server.api.clone()) else {
            return;
        };
        let id = self.tabs.open(directory);
        let (cols, rows) = self.provisional_size(cx);
        self.new_surface(id, cols, rows, cx);
        let request = self.create_request(directory, cols, rows, cx);
        let Some(create) = self.run(async move { api.create(&request).await }) else {
            return;
        };
        let directory = directory.to_owned();
        cx.spawn(async move |this, cx| {
            let outcome = create.await;
            let _ = this.update(cx, |this, cx| this.created(&directory, id, outcome, cx));
        })
        .detach();
        cx.notify();
    }

    /// What a new terminal starts with: the size, the shell and the colors
    /// its color queries are answered with.
    fn create_request(&self, directory: &str, cols: u16, rows: u16, cx: &App) -> CreateTerminal {
        let palette = cx.global::<Palette>();
        let hex = |color: gpui_kit::Hsla| {
            let color = color.to_rgb();
            format!(
                "#{:02x}{:02x}{:02x}",
                (color.r * 255.0).round() as u8,
                (color.g * 255.0).round() as u8,
                (color.b * 255.0).round() as u8
            )
        };
        let shell = self.settings.shell.clone();
        CreateTerminal {
            cwd: directory.to_owned(),
            cols,
            rows,
            theme_mode: match palette.appearance {
                Appearance::Light => ThemeMode::Light,
                Appearance::Dark => ThemeMode::Dark,
            },
            background: Some(hex(palette.background)),
            foreground: Some(hex(palette.syntax_foreground)),
            login_shell: self.settings.login_shells.contains(&shell),
            shell,
        }
    }

    fn created(
        &mut self,
        directory: &str,
        id: TabId,
        outcome: Option<Result<CreatedTerminal, TerminalRequestError>>,
        cx: &mut Context<Self>,
    ) {
        let created = match outcome {
            Some(Ok(created)) => created,
            Some(Err(error)) => {
                tracing::debug!(%error, "the terminal did not start");
                let cwd_missing = error.is_cwd_missing();
                self.tabs.start_failed(directory, id, cwd_missing);
                self.notices.insert(id, Notice::StartFailed { cwd_missing });
                cx.notify();
                return;
            }
            None => return,
        };
        if !self.tabs.started(directory, id, &created.session_id) {
            // The tab closed while the terminal started; nobody shows it.
            self.close_server_terminal(&created.session_id, directory);
            return;
        }
        let Some(server) = &self.server else {
            return;
        };
        let commands = server.commands.clone();
        commands.attach(&created.session_id);
        if let Some((surface, _)) = self.surfaces.get(&id) {
            let session = created.session_id.clone();
            surface.update(cx, |surface, cx| {
                surface.set_writer(commands, session);
                // A grid that settled while the terminal started reports its size.
                surface.set_pty_size((created.cols, created.rows), cx);
            });
        }
        self.focus_requested_surface(directory, cx);
        cx.notify();
    }

    fn resize_server(
        &self,
        session: &str,
        directory: &str,
        cols: u16,
        rows: u16,
        cx: &mut Context<Self>,
    ) {
        let Some(server) = &self.server else {
            return;
        };
        let api = server.api.clone();
        let session = session.to_owned();
        let directory = directory.to_owned();
        let Some((_, surface)) = self.surface_for_session(&session) else {
            return;
        };
        let Some(request) = surface.read(cx).pending_resize() else {
            return;
        };
        let weak_surface = surface.downgrade();
        let Some(task) =
            self.run(async move { api.resize(&session, cols, rows, &directory).await })
        else {
            return;
        };
        let task = cx.spawn(async move |_, cx| {
            let outcome = task.await;
            let succeeded = matches!(outcome, Some(Ok(_)));
            if let Some(Err(error)) = outcome {
                tracing::debug!(%error, "the terminal size was not applied");
            }
            let _ = weak_surface.update(cx, |surface, cx| {
                surface.resize_finished(request, succeeded, cx)
            });
        });
        surface.update(cx, |surface, _| surface.retain_resize_http(task));
    }

    fn focus_requested_surface(&mut self, directory: &str, cx: &mut Context<Self>) {
        let Some((requested, window)) = self.pending_focus.as_ref() else {
            return;
        };
        if requested != directory || self.directory.as_deref() != Some(directory) {
            return;
        }
        let Some((surface, _)) = self
            .tabs
            .active(directory)
            .filter(|tab| tab.lifecycle != Lifecycle::Starting)
            .and_then(|tab| self.surfaces.get(&tab.id))
        else {
            return;
        };
        let focus = surface.read(cx).focus();
        let placeholder = self.focus.clone();
        let window = *window;
        self.pending_focus = None;
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                // A later click elsewhere cancels the request implicitly.
                if placeholder.is_focused(window) {
                    window.focus(&focus, cx);
                }
            });
        });
    }

    fn close_server_terminal(&self, session: &str, directory: &str) {
        let Some(server) = &self.server else {
            return;
        };
        server.commands.detach(session);
        let api = server.api.clone();
        let session = session.to_owned();
        let directory = directory.to_owned();
        self.spawn_detached(async move {
            if let Err(error) = api.close(&session, &directory).await {
                tracing::debug!(%error, "the terminal did not close");
            }
        });
    }

    /// Closes a tab and stops its server terminal.
    pub fn close_tab(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(directory) = self.directory.clone() else {
            return;
        };
        if let Some(session) = self.tabs.close(&directory, id) {
            self.close_server_terminal(&session, &directory);
        }
        if let Some((surface, _)) = self.surfaces.remove(&id) {
            surface.update(cx, |surface, _| surface.stop_resizing());
        }
        self.notices.remove(&id);
        self.menu = None;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// The live run of `action` in `directory`.
    pub(crate) fn action_run(&self, directory: &str, action: &str) -> Option<ActionRun> {
        self.tabs.action_run(directory, action)
    }

    /// Keeps `directory`'s action runs current: its terminals are listed now
    /// and again whenever the server says its terminals changed, so a run
    /// another client started shows as running.
    pub(crate) fn observe(&mut self, directory: &str, cx: &mut Context<Self>) {
        if self.server.is_none() || !self.observed.insert(directory.to_owned()) {
            return;
        }
        self.adopt(directory, false, cx);
    }

    /// The server's terminals changed (`openchamber:terminal-sessions-changed`).
    pub(crate) fn refresh_observed(&mut self, cx: &mut Context<Self>) {
        let directories: BTreeSet<String> = self
            .observed
            .iter()
            .chain(self.directory.iter())
            .cloned()
            .collect();
        for directory in directories {
            self.adopt(&directory, false, cx);
        }
    }

    /// Names the action tabs of `directory` after the actions they run.
    pub(crate) fn name_actions<'a>(
        &mut self,
        directory: &str,
        actions: impl IntoIterator<Item = (&'a str, &'a str, &'static str)>,
    ) {
        for (id, name, glyph) in actions {
            self.tabs.name_action(directory, id, name);
            self.action_glyphs.insert(id.to_owned(), glyph);
        }
    }

    /// Runs a project action's command in its own tab of `directory`
    /// (`runAction`). A run already live is left alone; a run another client
    /// started is joined, since the server answers a duplicate create with it.
    pub(crate) fn run_action(
        &mut self,
        directory: &str,
        action: ActionCommand,
        label: &str,
        glyph: &'static str,
        cx: &mut Context<Self>,
    ) {
        let Some(api) = self.server.as_ref().map(|server| server.api.clone()) else {
            return;
        };
        if self.tabs.action_run(directory, &action.action_id).is_some() {
            return;
        }
        self.action_glyphs.insert(action.action_id.clone(), glyph);
        let id = self.tabs.open_action(directory, &action.action_id, label);
        if let Some(previous) = self.tabs.begin_run(directory, id, &action.execution_id)
            && let Some(server) = &self.server
        {
            server.commands.detach(&previous);
        }
        self.notices.remove(&id);
        // Each run starts on a clean screen.
        let (cols, rows) = self.provisional_size(cx);
        if let Some((surface, _)) = self.surfaces.remove(&id) {
            surface.update(cx, |surface, _| surface.stop_resizing());
        }
        self.new_surface(id, cols, rows, cx);
        let request = self.create_request(directory, cols, rows, cx);
        let command = action.clone();
        let Some(create) = self.run(async move { api.create_command(&request, &command).await })
        else {
            return;
        };
        let directory = directory.to_owned();
        cx.spawn(async move |this, cx| {
            let outcome = create.await;
            let _ = this.update(cx, |this, cx| {
                this.action_created(&directory, id, &action, outcome, cx)
            });
        })
        .detach();
        cx.notify();
    }

    fn action_created(
        &mut self,
        directory: &str,
        id: TabId,
        action: &ActionCommand,
        outcome: Option<Result<CreatedTerminal, TerminalRequestError>>,
        cx: &mut Context<Self>,
    ) {
        let requested = action.execution_id.as_str();
        let created = match outcome {
            Some(Ok(created)) => created,
            Some(Err(error)) => {
                tracing::debug!(%error, "the project action did not start");
                self.tabs.run_failed(directory, id, requested);
                self.notices.insert(
                    id,
                    Notice::StartFailed {
                        cwd_missing: error.is_cwd_missing(),
                    },
                );
                cx.notify();
                return;
            }
            None => return,
        };
        // A deduplicated answer is the run another client started; only a
        // terminal this request created is closed when it is refused.
        let own = created.session_id == requested;
        let joined = match &created.purpose {
            Some(TerminalPurpose::ProjectAction {
                action_id,
                execution_id,
            }) if created.command_mode && *action_id == action.action_id => execution_id.clone(),
            _ => {
                if own {
                    self.close_server_terminal(&created.session_id, directory);
                }
                self.tabs.run_failed(directory, id, requested);
                self.notices
                    .insert(id, Notice::StartFailed { cwd_missing: false });
                cx.notify();
                return;
            }
        };
        if !self
            .tabs
            .action_started(directory, id, requested, &joined, &created.session_id)
        {
            // Stopped or rerun while it started; nobody shows this run.
            if own {
                self.close_server_terminal(&created.session_id, directory);
            }
            return;
        }
        let Some(server) = &self.server else {
            return;
        };
        let commands = server.commands.clone();
        commands.attach(&created.session_id);
        if let Some((surface, _)) = self.surfaces.get(&id) {
            let session = created.session_id.clone();
            surface.update(cx, |surface, cx| {
                surface.set_writer(commands, session);
                surface.set_pty_size((created.cols, created.rows), cx);
            });
        }
        cx.notify();
    }

    /// Stops a live run (`stopProjectActionTerminalSession`): Ctrl+C, then,
    /// when it has not exited within a second, a kill. A failed kill shows
    /// the run as running again.
    pub(crate) fn stop_action(&mut self, directory: &str, action: &str, cx: &mut Context<Self>) {
        let Some(run) = self.tabs.action_run(directory, action) else {
            return;
        };
        if run.stopping {
            return;
        }
        let Some(server) = &self.server else {
            return;
        };
        self.tabs.set_stopping(&run.session, true);
        server.commands.write(&run.session, "\u{3}".to_owned());
        let api = server.api.clone();
        let directory = directory.to_owned();
        let action = action.to_owned();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(STOP_GRACE).await;
            let Ok(kill) = this.update(cx, |this, _| {
                // Exited, ended or rerun meanwhile: nothing left to stop.
                let still = this
                    .tabs
                    .action_run(&directory, &action)
                    .is_some_and(|current| current.execution == run.execution);
                still.then(|| {
                    let session = run.session.clone();
                    let directory = directory.clone();
                    this.run(async move { api.force_kill(&session, &directory).await })
                })
            }) else {
                return;
            };
            let Some(Some(kill)) = kill else {
                return;
            };
            let outcome = kill.await;
            let _ = this.update(cx, |this, cx| {
                if !this
                    .tabs
                    .is_current_run(&directory, run.tab, &run.execution)
                {
                    return;
                }
                match outcome {
                    Some(Ok(())) => this.exited(&run.session, None, None, false, cx),
                    _ => {
                        tracing::debug!("the project action could not be stopped");
                        this.tabs.set_stopping(&run.session, false);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn select_tab(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(directory) = self.directory.clone() {
            self.tabs.select(&directory, id);
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        let surface = self
            .directory
            .as_deref()
            .and_then(|directory| self.tabs.active(directory))
            .and_then(|tab| self.surfaces.get(&tab.id));
        match surface {
            Some((surface, _)) => {
                let handle = surface.read(cx).focus();
                window.focus(&handle, cx);
            }
            None => window.focus(&self.focus, cx),
        }
    }

    fn surface_for_session(&self, session: &str) -> Option<(TabId, Entity<TerminalSurface>)> {
        let (_, tab) = self.tabs.find_session(session)?;
        self.surfaces
            .get(&tab.id)
            .map(|(surface, _)| (tab.id, surface.clone()))
    }

    fn on_socket_event(&mut self, event: TerminalEvent, cx: &mut Context<Self>) {
        match event {
            TerminalEvent::Connected => {
                let was_reconnecting = std::mem::take(&mut self.reconnecting);
                if was_reconnecting {
                    // Terminals started elsewhere while away are adopted again.
                    self.adopted.clear();
                    if let Some(directory) = self.directory.clone() {
                        self.adopted.insert(directory);
                    }
                    self.refresh_observed(cx);
                }
            }
            TerminalEvent::Reconnecting { .. } => self.reconnecting = true,
            TerminalEvent::Snapshot(snapshot) => {
                let Some((id, surface)) = self.surface_for_session(&snapshot.session) else {
                    return;
                };
                self.notices.remove(&id);
                let recorded = snapshot.cols.zip(snapshot.rows);
                surface.update(cx, |surface, cx| {
                    surface.replay(&snapshot.history, recorded, cx)
                });
                if snapshot.status == TerminalStatus::Exited {
                    self.exited(
                        &snapshot.session,
                        snapshot.exit_code,
                        snapshot.signal,
                        false,
                        cx,
                    );
                } else {
                    self.tabs.restarted(&snapshot.session);
                    surface.update(cx, |surface, cx| surface.resume_resizing(cx));
                }
            }
            TerminalEvent::Output { session, data, .. } => {
                if let Some((_, surface)) = self.surface_for_session(&session) {
                    surface.update(cx, |surface, cx| surface.feed(&data, cx));
                }
            }
            TerminalEvent::Exit {
                session,
                exit_code,
                signal,
            } => self.exited(&session, exit_code, signal, true, cx),
            TerminalEvent::Restarted { session, history } => {
                if let Some((id, surface)) = self.surface_for_session(&session) {
                    self.notices.remove(&id);
                    surface.update(cx, |surface, cx| surface.replay(&history, None, cx));
                    self.tabs.restarted(&session);
                    surface.update(cx, |surface, cx| surface.resume_resizing(cx));
                }
            }
            TerminalEvent::Error {
                session: Some(session),
                code,
                fatal: true,
            } => {
                let Some((_, tab)) = self.tabs.find_session(&session) else {
                    return;
                };
                let id = tab.id;
                self.tabs.ended(&session);
                let notice = match code.as_deref() {
                    Some("SESSION_NOT_FOUND" | "CLOSED" | "KILLED" | "IDLE_TIMEOUT") | None => {
                        Notice::SessionEnded
                    }
                    Some(other) => Notice::ConnectionFailed(other.to_owned()),
                };
                self.notices.insert(id, notice);
            }
            TerminalEvent::Error { .. } => {}
            TerminalEvent::WriteFailed { session } => {
                if let Some((_, tab)) = self.tabs.find_session(&session) {
                    self.notices.insert(tab.id, Notice::SendFailed);
                }
            }
        }
        cx.notify();
    }

    /// A process ended. With `terminalCloseOnExit` the tab closes; otherwise
    /// the screen says how it ended and the tab stays readable.
    fn exited(
        &mut self,
        session: &str,
        exit_code: Option<i64>,
        signal: Option<i64>,
        live: bool,
        cx: &mut Context<Self>,
    ) {
        let Some((directory, id)) = self.tabs.exited(session, exit_code, signal) else {
            return;
        };
        if self.settings.close_on_exit {
            if let Some(session) = self.tabs.close(&directory, id) {
                self.close_server_terminal(&session, &directory);
            }
            if let Some((surface, _)) = self.surfaces.remove(&id) {
                surface.update(cx, |surface, _| surface.stop_resizing());
            }
            self.notices.remove(&id);
            return;
        }
        if live && let Some((surface, _)) = self.surfaces.get(&id) {
            let strings = locale::strings(cx);
            let code = exit_code.map(|code| code.to_string());
            let signal = signal.map(|signal| signal.to_string());
            let code_segment = code.as_deref().map_or(SharedString::default(), |code| {
                strings.format(
                    "terminalView.stream.processExitedWithCode",
                    &[("exitCode", code)],
                )
            });
            let signal_segment = signal.as_deref().map_or(SharedString::default(), |signal| {
                strings.format(
                    "terminalView.stream.processExitedWithSignal",
                    &[("signal", signal)],
                )
            });
            let message = strings.format(
                "terminalView.stream.processExitedMessage",
                &[
                    ("exitCodeSegment", code_segment.as_ref()),
                    ("signalSegment", signal_segment.as_ref()),
                ],
            );
            surface.update(cx, |surface, cx| surface.feed(&message, cx));
        }
        self.notices.insert(id, Notice::SessionEnded);
    }

    fn on_surface_event(&mut self, id: TabId, event: &SurfaceEvent, cx: &mut Context<Self>) {
        match event {
            SurfaceEvent::Resized { cols, rows } => {
                let owner = self.tabs.sessions().find_map(|session| {
                    let (directory, tab) = self.tabs.find_session(session)?;
                    (tab.id == id && tab.lifecycle == Lifecycle::Running)
                        .then(|| (directory.to_owned(), session.to_owned()))
                });
                if let Some((directory, session)) = owner {
                    self.resize_server(&session, &directory, *cols, *rows, cx);
                } else if let Some((surface, _)) = self.surfaces.get(&id) {
                    surface.update(cx, |surface, _| surface.decline_resize());
                }
            }
            SurfaceEvent::ContextMenu { position } => {
                self.menu = Some(Menu {
                    position: *position,
                    tab: id,
                });
                cx.notify();
            }
            SurfaceEvent::ClipboardFailed { paste } => {
                self.notices
                    .insert(id, Notice::ClipboardFailed { paste: *paste });
                cx.notify();
            }
        }
    }

    fn notice_text(&self, notice: &Notice, strings: &Strings) -> SharedString {
        match notice {
            Notice::StartFailed { .. } => strings.get("terminalView.error.startSessionFailed"),
            Notice::SessionEnded => strings.get("terminalView.error.sessionEnded"),
            Notice::ConnectionFailed(message) => strings.format(
                "terminalView.error.connectionFailed",
                &[("message", message)],
            ),
            Notice::SendFailed => strings.get("terminalView.error.sendInputFailed"),
            Notice::ClipboardFailed { paste: true } => {
                strings.get("terminalView.toast.pasteFailed")
            }
            Notice::ClipboardFailed { paste: false } => {
                strings.get("terminalView.toast.copyFailed")
            }
        }
    }

    /// The tab strip and the new-tab button: one header row.
    fn render_header(
        &self,
        palette: &Palette,
        strings: &Strings,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let directory = self.directory.clone().unwrap_or_default();
        let active = self.tabs.active(&directory).map(|tab| tab.id);
        let tabs = self.tabs.tabs(&directory).iter().map(|tab| {
            let id = tab.id;
            let selected = active == Some(id);
            let label = SharedString::from(tab.label.clone());
            let close = IconButton::new(
                format!("terminal-tab-close-{}", id.get()),
                "close",
                strings.get("terminalView.tabs.closeTabTitle"),
            )
            .size(20.0, 14.0)
            .render(palette, cx, move |this: &mut Self, window, cx| {
                this.close_tab(id, window, cx)
            });
            let tab_id = SharedString::from(format!("terminal-tab-{}", id.get()));
            let row = div()
                .id(tab_id.clone())
                .role(Role::Tab)
                .accessibility_id(tab_id)
                .aria_label(label.clone())
                .aria_selected(selected)
                .h(px(TAB_HEIGHT))
                .px(px(8.0))
                .gap(px(4.0))
                .flex()
                .items_center()
                .rounded(px(CONTROL_RADIUS))
                .cursor_pointer()
                .text_size(px(13.0))
                .text_color(if selected {
                    palette.foreground
                } else {
                    palette.muted_foreground
                })
                .when(selected, |this| this.bg(palette.selection.opacity(0.5)))
                .when(!selected, |this| {
                    this.hover(|style| style.bg(palette.hover.opacity(0.5)))
                })
                .child(glyph(
                    tab.action()
                        .map(|action| self.action_glyphs.get(action).copied().unwrap_or("play"))
                        .unwrap_or("terminal-box"),
                    14.0,
                    palette.muted_foreground,
                ))
                .child(div().whitespace_nowrap().child(label))
                .child(close);
            on_activate(row, cx, move |this: &mut Self, window, cx| {
                this.select_tab(id, window, cx)
            })
            .test_support()
            .into_any_element()
        });
        div()
            .id("terminal-header")
            .h(px(HEADER_HEIGHT))
            .flex_shrink_0()
            .px(px(6.0))
            .gap(px(4.0))
            .flex()
            .items_center()
            .child(
                div()
                    .id("terminal-tabs")
                    .role(Role::TabList)
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .min_w_0()
                    .overflow_x_scroll()
                    .children(tabs),
            )
            .when(self.server.is_some() && self.directory.is_some(), |this| {
                this.child(
                    IconButton::new(
                        "terminal-new-tab",
                        "add",
                        strings.get("terminalView.tabs.newTabTitle"),
                    )
                    .size(28.0, 16.0)
                    .render(palette, cx, |this: &mut Self, window, cx| {
                        this.new_tab(window, cx)
                    }),
                )
            })
            .into_any_element()
    }

    fn render_menu(
        &self,
        palette: &Palette,
        strings: &Strings,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.menu?;
        let (surface, _) = self.surfaces.get(&menu.tab)?;
        let has_selection = surface.read(cx).has_selection();
        let item = |id: &'static str, label: SharedString, enabled: bool| {
            div()
                .id(id)
                .role(Role::MenuItem)
                .accessibility_id(id)
                .aria_label(label.clone())
                .h(px(28.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .rounded(px(8.0))
                .text_size(px(13.0))
                .text_color(if enabled {
                    palette.foreground
                } else {
                    palette.muted_foreground
                })
                .when(enabled, |this| {
                    this.cursor_pointer().hover(|style| style.bg(palette.hover))
                })
                .child(label)
        };
        let copy_surface = surface.clone();
        let copy = on_activate(
            item(
                "terminal-menu-copy",
                strings.get("terminalView.actions.copy"),
                has_selection,
            ),
            cx,
            move |this: &mut Self, _, cx| {
                if copy_surface.read(cx).has_selection() {
                    copy_surface.update(cx, |surface, cx| surface.copy(cx));
                }
                this.menu = None;
                cx.notify();
            },
        );
        let paste_surface = surface.clone();
        let paste = on_activate(
            item(
                "terminal-menu-paste",
                strings.get("terminalView.actions.paste"),
                true,
            ),
            cx,
            move |this: &mut Self, window, cx| {
                paste_surface.update(cx, |surface, cx| surface.paste(cx));
                this.menu = None;
                this.focus_active(window, cx);
                cx.notify();
            },
        );
        let popup = div()
            .id("terminal-menu")
            .role(Role::Menu)
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.menu = None;
                cx.notify();
            }))
            .min_w(px(140.0))
            .p(px(4.0))
            .rounded(px(12.0))
            .bg(palette.elevated)
            .shadow(elevation::floating(palette))
            .flex()
            .flex_col()
            .child(copy.test_support())
            .child(paste.test_support());
        Some(
            deferred(
                anchored()
                    .anchor(Anchor::TopLeft)
                    .position(menu.position)
                    .position_mode(AnchoredPositionMode::Window)
                    .snap_to_window_with_margin(Edges::all(px(8.0)))
                    .child(popup),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = cx.global::<Palette>().clone();
        let strings = locale::strings(cx).clone();
        let directory = self.directory.clone();
        let active = directory
            .as_deref()
            .and_then(|directory| self.tabs.active(directory))
            .cloned();
        let message = |text: SharedString| {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px(px(16.0))
                .text_size(px(13.0))
                .text_color(palette.muted_foreground)
                .child(text)
                .into_any_element()
        };
        let body = if self.server.is_none() {
            message(strings.get("terminalView.empty.noWorkingDirectory"))
        } else if directory.is_none() {
            message(strings.get("terminalView.empty.selectSession"))
        } else if let Some((surface, _)) =
            active.as_ref().and_then(|tab| self.surfaces.get(&tab.id))
        {
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(surface.clone())
                .into_any_element()
        } else if let Some(Lifecycle::StartFailed { .. }) =
            active.as_ref().map(|tab| &tab.lifecycle)
        {
            message(strings.get("terminalView.error.startSessionFailed"))
        } else {
            div().flex_1().into_any_element()
        };
        let notice = active
            .as_ref()
            .and_then(|tab| self.notices.get(&tab.id))
            .map(|notice| self.notice_text(notice, &strings))
            .or_else(|| {
                self.reconnecting
                    .then(|| strings.get("desktopHostSwitcher.sshPhase.reconnecting"))
            });
        let status_tone = if self.reconnecting {
            palette.status_warning_background
        } else {
            palette.status_error.opacity(0.15)
        };
        div()
            .id("terminal-panel")
            .role(Role::Region)
            .accessibility_id("terminal-panel")
            .aria_label(strings.get("layout.mainTab.terminal"))
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(palette.background)
            .child(self.render_header(&palette, &strings, cx))
            .child(body)
            .when_some(notice, |this, notice| {
                this.child(
                    div()
                        .id("terminal-status")
                        .role(Role::Status)
                        .accessibility_id("terminal-status")
                        .aria_label(notice.clone())
                        .flex_shrink_0()
                        .px(px(12.0))
                        .py(px(8.0))
                        .bg(status_tone)
                        .text_size(px(12.0))
                        .child(notice),
                )
            })
            .children(self.render_menu(&palette, &strings, cx))
            .test_support()
    }
}

impl crate::workspace::Workspace {
    /// The directory terminals open in: the selected session's, else the
    /// project a new-session draft targets.
    pub(crate) fn terminal_directory(&self) -> Option<String> {
        let host = if let Some(record) = self.selected.as_ref().and_then(|id| self.sessions.get(id))
        {
            record.directory.clone()
        } else {
            self.draft_projects.get(&self.runtime).cloned()
        };
        // A project action that runs in the project's checkout shows its tab
        // there while the panel was opened for it (`targetDirectory`).
        match (&self.terminal_target, &host) {
            (Some((owner, target)), Some(host)) if owner == host => Some(target.clone()),
            _ => host,
        }
    }

    /// Opens the terminal panel on `directory`'s active tab, as a project
    /// action's run reveals its output (`revealProjectActionTerminal`).
    pub(crate) fn reveal_terminal(
        &mut self,
        host: Option<String>,
        directory: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal_target = host
            .filter(|host| host != directory)
            .map(|host| (host, directory.to_owned()));
        self.terminal_open = true;
        self.panels.open = None;
        self.sync_terminal(false, window, cx);
        cx.notify();
    }

    /// Opens or closes the terminal panel (`toggle_terminal`, Ctrl+J).
    pub(crate) fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal_open = !self.terminal_open;
        self.terminal_target = None;
        if self.terminal_open {
            // One surface shows in the panel column at a time.
            self.panels.open = None;
            self.sync_terminal(true, window, cx);
        } else {
            window.focus(&self.focus_handle, cx);
        }
        cx.notify();
    }

    /// Keeps the panel on the current runtime and directory. A panel made
    /// before the client authenticated, or for another runtime, is replaced;
    /// dropping it detaches its terminals without stopping them.
    fn sync_terminal(&mut self, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_terminal_panel(cx);
        let directory = self.terminal_directory();
        if let Some(panel) = &self.terminal_panel {
            panel.update(cx, |panel, cx| {
                panel.show_directory(directory, focus, window, cx)
            });
        }
    }

    /// The panel for the current runtime and connection, made when missing.
    /// Project actions run through it while it is hidden too.
    pub(crate) fn ensure_terminal_panel(&mut self, cx: &mut Context<Self>) {
        self.invalidate_terminal_client(cx);
        let client = self.connection.as_ref().and_then(|connection| {
            connection
                .client
                .clone()
                .map(|client| (client, connection.network.handle()))
        });
        let stale = self.terminal_panel.as_ref().is_none_or(|panel| {
            let panel = panel.read(cx);
            panel.runtime != self.runtime || (!panel.has_server() && client.is_some())
        });
        if stale {
            let runtime = self.runtime.clone();
            let generation = self
                .connection
                .as_ref()
                .map(|connection| connection.generation);
            let preferences = self
                .settings
                .as_ref()
                .and_then(|settings| settings.read(cx).general_preferences(&runtime, cx))
                .unwrap_or_default();
            let close = preferences.terminal_close_on_exit.or_else(|| {
                self.send_catalog
                    .settings
                    .as_ref()
                    .and_then(|settings| settings.terminal_close_on_exit)
            });
            let shell = preferences.terminal_shell.or_else(|| {
                self.send_catalog
                    .settings
                    .as_ref()
                    .and_then(|settings| settings.terminal_shell.clone())
            });
            self.terminal_panel = Some(cx.new(|cx| {
                let mut panel = TerminalPanel::new(runtime, client, cx);
                panel.generation = generation;
                panel.apply_general_settings(close, shell.as_deref(), cx);
                panel
            }));
        }
    }

    /// The server's terminals changed: the panel lists the directories it
    /// shows and observes again.
    pub(crate) fn refresh_terminal_sessions(&mut self, cx: &mut Context<Self>) {
        if let Some(panel) = &self.terminal_panel {
            panel.update(cx, |panel, cx| panel.refresh_observed(cx));
        }
    }

    /// Connection lifecycle edges call this even when no terminal is rendered.
    pub(crate) fn invalidate_terminal_client(&mut self, cx: &mut Context<Self>) {
        let generation = self
            .connection
            .as_ref()
            .map(|connection| connection.generation);
        let stale = self.terminal_panel.as_ref().is_some_and(|panel| {
            let panel = panel.read(cx);
            panel.runtime != self.runtime || panel.generation != generation
        });
        if stale && let Some(panel) = self.terminal_panel.take() {
            panel.update(cx, |panel, cx| {
                panel.server = None;
                for (surface, _) in panel.surfaces.values() {
                    surface.update(cx, |surface, _| surface.stop_resizing());
                }
                panel.surfaces.clear();
                panel._keepalive = None;
                panel._settings = None;
                panel.pending_focus = None;
            });
        }
    }

    /// The panel while it is open and no other panel surface is.
    pub(crate) fn render_terminal_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.terminal_open {
            return None;
        }
        if self.panels.open.is_some() {
            // Another surface took the column; the terminals keep running.
            self.terminal_open = false;
            return None;
        }
        self.sync_terminal(false, window, cx);
        let panel = self.terminal_panel.clone()?;
        let palette = cx.global::<crate::theme::Palette>().clone();
        let strings = locale::strings(cx).clone();
        // The right zone's width, which every surface in it shares.
        Some(
            div()
                .relative()
                .w(px(self.fitted_panel_width(window)))
                .flex_shrink_0()
                .h_full()
                .child(panel)
                .child(self.render_resize_handle(
                    crate::pane_resize::Pane::Panel,
                    strings.get("contextPanel.actions.resizePanelAria"),
                    &palette,
                    strings.direction(),
                    cx,
                ))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext, TestAppContext};

    #[gpui_kit::test]
    fn global_appearance_updates_every_hidden_surface_once(cx: &mut TestAppContext) {
        use std::cell::Cell;
        use std::rc::Rc;

        let (panel, surfaces) = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(Strings::load("en"));
            crate::theme::apply(Palette::default_for(Appearance::Dark), cx);
            Theme::update(cx, |theme| theme.mono_font_family = "Kit Mono".into());
            let panel = cx.new(|cx| TerminalPanel::new(RuntimeId::from("synthetic"), None, cx));
            let surfaces = panel.update(cx, |panel, cx| {
                ["/first", "/second"].map(|directory| {
                    let id = panel.tabs.open(directory);
                    panel.new_surface(id, 80, 24, cx)
                })
            });
            (panel, surfaces)
        });
        let notifications = Rc::new(Cell::new([0; 2]));
        cx.update(|cx| {
            for (index, surface) in surfaces.iter().enumerate() {
                let notifications = Rc::clone(&notifications);
                cx.observe(surface, move |_, _| {
                    let mut counts = notifications.get();
                    counts[index] += 1;
                    notifications.set(counts);
                })
                .detach();
            }
        });

        cx.update(|cx| {
            crate::theme::apply(Palette::default_for(Appearance::Light), cx);
            Theme::update(cx, |theme| theme.mono_font_family = "Kit Mono".into());
        });
        assert_eq!(
            notifications.get(),
            [1, 1],
            "both globals from one apply update each hidden surface once"
        );
        cx.update(|cx| {
            crate::theme::apply(Palette::default_for(Appearance::Light), cx);
            Theme::update(cx, |theme| {
                theme.mono_font_family = "Kit Mono".into();
                theme.font_family = "Other UI".into();
                theme.font_size = px(32.0);
            });
        });
        assert_eq!(
            notifications.get(),
            [1, 1],
            "unchanged colors and interface typography do not visit surfaces"
        );
        cx.update(|cx| {
            // The same theme ID can resolve new terminal colors.
            let mut palette = cx.global::<Palette>().clone();
            palette.status_error = palette.primary;
            cx.set_global(palette);
        });
        assert_eq!(
            notifications.get(),
            [2, 2],
            "a palette-only notification reaches both hidden directories"
        );
        cx.update(|cx| {
            Theme::update(cx, |theme| theme.mono_font_family = "Wide Mono".into());
        });
        assert_eq!(
            notifications.get(),
            [3, 3],
            "a code-font-only notification reaches both hidden directories"
        );
        cx.read(|cx| {
            let panel = panel.read(cx);
            for surface in &surfaces {
                assert!(
                    panel
                        .surfaces
                        .values()
                        .any(|(retained, _)| retained.entity_id() == surface.entity_id()),
                    "appearance preserves each retained surface identity"
                );
                assert!(
                    !surface.read(cx).measured(),
                    "a hidden surface remains unmeasured until shown"
                );
            }
        });
    }

    #[gpui_kit::test]
    fn exited_resize_resumes_after_external_restart_snapshot(cx: &mut TestAppContext) {
        crate::workspace_tests::open(cx);
        let (panel, surface, window) = cx.update(|cx| {
            let panel = cx.new(|cx| TerminalPanel::new(RuntimeId::from("synthetic"), None, cx));
            let surface = panel.update(cx, |panel, cx| {
                panel.settings.close_on_exit = false;
                panel.tabs.adopt(
                    "/folder",
                    &[ListedTerminal {
                        session: "retained".to_owned(),
                        running: true,
                        is_terminal: true,
                        action: None,
                    }],
                );
                panel.attach_adopted("retained", cx);
                panel.on_socket_event(
                    TerminalEvent::Exit {
                        session: "retained".to_owned(),
                        exit_code: Some(0),
                        signal: None,
                    },
                    cx,
                );
                panel
                    .surface_for_session("retained")
                    .expect("retained tab")
                    .1
            });
            let (window, _) =
                gpui_kit::open_window(gpui_kit::WindowOptions::default(), cx, |_, _| {
                    surface.clone()
                })
                .expect("window");
            (panel, surface, window)
        });
        let attempts = std::rc::Rc::new(std::cell::Cell::new(0));
        cx.update(|cx| {
            let attempts = attempts.clone();
            cx.subscribe(&surface, move |_, event, _| {
                if matches!(event, SurfaceEvent::Resized { .. }) {
                    attempts.set(attempts.get() + 1);
                }
            })
            .detach();
        });
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .expect("render");
        cx.update(|cx| surface.update(cx, |surface, cx| surface.set_pty_size((1, 1), cx)));
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert!(
            cx.read(|cx| surface.read(cx).pending_resize().is_none()),
            "declined exited resize must not remain in flight"
        );
        cx.executor().advance_clock(Duration::from_secs(10));
        cx.run_until_parked();
        assert!(
            cx.read(|cx| surface.read(cx).pending_resize().is_none()),
            "decline must not retry"
        );
        assert_eq!(
            attempts.get(),
            1,
            "exited process receives only the declined proposal"
        );
        cx.update(|cx| {
            panel.update(cx, |panel, cx| {
                panel.on_socket_event(
                    TerminalEvent::Snapshot(openchamber_native_transport::terminal::Snapshot {
                        session: "retained".to_owned(),
                        sequence: 2,
                        history: String::new(),
                        cols: Some(1),
                        rows: Some(1),
                        status: TerminalStatus::Running,
                        exit_code: None,
                        signal: None,
                    }),
                    cx,
                )
            })
        });
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        cx.update(|cx| {
            surface.update(cx, |surface, cx| {
                let request = surface
                    .pending_resize()
                    .expect("running snapshot retries fitted size");
                surface.resize_finished(request, true, cx);
                assert!(
                    surface.pending_resize().is_none(),
                    "successful completion releases gate"
                );
            })
        });
        cx.executor().advance_clock(Duration::from_secs(10));
        cx.run_until_parked();
        assert!(
            cx.read(|cx| surface.read(cx).pending_resize().is_none()),
            "successful target stays reconciled"
        );
        assert_eq!(
            attempts.get(),
            2,
            "restart reconciles the retained target once"
        );
    }

    #[gpui_kit::test]
    fn asynchronous_first_tab_receives_requested_focus(cx: &mut TestAppContext) {
        let (window, workspace) = crate::workspace_tests::open(cx);
        let panel =
            cx.update(|cx| cx.new(|cx| TerminalPanel::new(RuntimeId::from("synthetic"), None, cx)));
        cx.update_window(window, |_, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.show_directory(Some("/folder".to_owned()), true, window, cx)
            });
            assert!(
                panel.read(cx).focus.is_focused(window),
                "waiting panel owns focus"
            );
        })
        .expect("window open");
        // The list completes in a later turn and yields an adopted terminal.
        let surface = cx.update(|cx| {
            panel.update(cx, |panel, cx| {
                panel.tabs.adopt(
                    "/folder",
                    &[ListedTerminal {
                        session: "adopted".to_owned(),
                        running: true,
                        is_terminal: true,
                        action: None,
                    }],
                );
                panel.attach_adopted("adopted", cx);
                panel.focus_requested_surface("/folder", cx);
                panel
                    .surface_for_session("adopted")
                    .expect("adopted surface")
                    .1
            })
        });
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            assert!(
                surface.read(cx).focus().is_focused(window),
                "async adoption receives the earlier focus request"
            );
            panel.update(cx, |panel, cx| {
                panel.show_directory(Some("/new".to_owned()), true, window, cx)
            });
        })
        .expect("window open");
        let created = cx.update(|cx| {
            panel.update(cx, |panel, cx| {
                let id = panel.tabs.open("/new");
                let surface = panel.new_surface(id, 80, 24, cx);
                panel.tabs.started("/new", id, "created");
                panel.focus_requested_surface("/new", cx);
                surface
            })
        });
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            assert!(
                created.read(cx).focus().is_focused(window),
                "async creation receives focus"
            );
            panel.update(cx, |panel, cx| {
                panel.show_directory(Some("/later".to_owned()), true, window, cx)
            });
            let focus = workspace.read(cx).focus_handle.clone();
            window.focus(&focus, cx);
        })
        .expect("window open");
        cx.update(|cx| {
            panel.update(cx, |panel, cx| {
                let id = panel.tabs.open("/later");
                panel.new_surface(id, 80, 24, cx);
                panel.tabs.started("/later", id, "later-created");
                panel.focus_requested_surface("/later", cx);
            })
        });
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            assert!(
                workspace.read(cx).focus_handle.is_focused(window),
                "later user focus wins"
            )
        })
        .expect("window open");
    }

    #[gpui_kit::test]
    fn hidden_panel_is_invalidated_on_connection_and_runtime_edges(cx: &mut TestAppContext) {
        use crate::connection::{Connection, ServerConfig};
        use openchamber_native_transport::client::ClientDescription;
        use openchamber_native_transport::endpoint::Endpoint;
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.connection = Some(
                    Connection::new(ServerConfig {
                        endpoint: Endpoint::parse("http://127.0.0.1:1").expect("endpoint"),
                        password: None,
                        client: ClientDescription::default(),
                    })
                    .expect("network"),
                );
                let connection = workspace.connection.as_mut().expect("connection");
                connection.client = Some(
                    ServerClient::new(
                        connection.config.endpoint.clone(),
                        openchamber_native_transport::credential::Credential::None,
                    )
                    .expect("client"),
                );
                workspace.sync_terminal(false, window, cx);
                workspace.terminal_open = false;
                let old = workspace.terminal_panel.clone().expect("panel");
                assert!(
                    old.read(cx).server.is_some(),
                    "panel holds a transport before reconnect"
                );
                workspace.connect(window, cx);
                assert!(
                    workspace.terminal_panel.is_none(),
                    "a new generation drops even a hidden panel"
                );
                assert!(old.read(cx).server.is_none());
                workspace.sync_terminal(false, window, cx);
                assert_eq!(
                    workspace
                        .terminal_panel
                        .as_ref()
                        .expect("replacement")
                        .read(cx)
                        .generation,
                    Some(1)
                );
                workspace.switch_runtime(RuntimeId::from("different-runtime"), window, cx);
                assert!(
                    workspace.terminal_panel.is_none(),
                    "runtime switch drops hidden panels before render"
                );
            });
        })
        .expect("window open");
    }

    #[gpui_kit::test]
    fn the_rail_button_and_ctrl_j_toggle_the_panel(cx: &mut TestAppContext) {
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            window.click("rail-terminal", cx);
            window.render_frame(cx);
            assert!(
                window.try_find("terminal-panel").is_some(),
                "the rail opens the panel"
            );
        })
        .expect("window open");
        assert!(cx.read(|cx| workspace.read(cx).terminal_open));
        // Synthetic data has no server, so the panel offers no new tab.
        cx.update_window(window, |_, window, cx| {
            assert!(window.try_find("terminal-new-tab").is_none());
            window.press("ctrl-j", cx);
            assert!(
                window.try_find("terminal-panel").is_none(),
                "Ctrl+J closes it"
            );
            window.press("ctrl-j", cx);
            assert!(window.try_find("terminal-panel").is_some());
        })
        .expect("window open");
        let panel = cx.read(|cx| workspace.read(cx).terminal_panel.clone());
        assert!(panel.is_some_and(|panel| cx.read(|cx| !panel.read(cx).has_server())));
    }
}
