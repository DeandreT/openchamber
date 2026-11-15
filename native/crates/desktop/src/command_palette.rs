//! The command palette (`CommandPalette.tsx`): one search field over the
//! commands, Settings pages, sessions, files and projects.
//!
//! Without a query the palette lists the file open in the Files view, the
//! five most recent sessions and the everyday commands. A query ranks each
//! group with the web client's subsequence matcher and orders the groups by
//! their best match; commands, Settings pages and projects must also match
//! at word starts, so `ts` finds "TypeScript" and never the tail of "Agents".
//! Files come from the server's index 200 ms after typing stops.
//!
//! The palette is a dialog over the window. The workspace takes its lists
//! when the palette opens and keeps the focus the palette returns on close.
//! A command the client has no feature for is left out of the list.

use std::ops::Range;
use std::time::Duration;

use gpui_kit::component::WindowExt;
use gpui_kit::component::input::{
    Escape, Input, InputEvent, InputState, MoveDown, MoveEnd, MoveHome, MoveUp,
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, Entity, EntityInputHandler, FocusHandle, Focusable,
    FontWeight, HighlightStyle, InteractiveElement, IntoElement, KeyBinding, ParentElement, Render,
    Role, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, StyledText, Subscription,
    Task, TestSupportExt, WeakEntity, Window, actions, div, prelude::FluentBuilder, px, rems,
};
use openchamber_native_core::ids::{RuntimeId, SessionId};
use openchamber_native_core::search_rank::{self, SubsequenceMatch};
use openchamber_native_core::session::SessionRecord;
use openchamber_native_core::shortcuts::{self as shortcut_schema, Platform};
use openchamber_native_transport::client::ServerClient;

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::connection::ConnectionState;
use crate::files_view::PanelSurface;
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::network::NetworkTask;
use crate::panel_controls::{LABEL_LINE, LABEL_SIZE, META_LINE, META_SIZE, MICRO_LINE, MICRO_SIZE};
use crate::project_action_run::PaletteProjectAction;
use crate::settings_navigation::Page;
use crate::shortcuts::{RunShortcut, ShortcutBindings};
use crate::theme::Palette;
use crate::workspace::Workspace;
use crate::workspace_view;

actions!(
    openchamber,
    [
        /// Moves the command palette's selection down.
        PaletteNext,
        /// Moves the command palette's selection up.
        PalettePrevious,
        /// Moves the command palette's selection to the next group.
        PaletteNextGroup,
        /// Moves the command palette's selection to the previous group.
        PalettePreviousGroup,
    ]
);

/// The key context of the open palette. It sits inside a copy of the
/// workspace's context, so Ctrl+N and Ctrl+P move the selection before any
/// shortcut bound to them runs.
pub(crate) const KEY_CONTEXT: &str = "CommandPalette";

/// The shortcut that opens the palette, and closes it again.
pub(crate) const SHORTCUT: &str = "open_command_palette";

/// `useDebouncedValue(query, 200)` in front of the file search.
const FILE_DEBOUNCE: Duration = Duration::from_millis(200);
/// What a file search asks the index for; more when worktree copies inside
/// the project take result slots before they are dropped.
const FILE_SEARCH_LIMIT: usize = 40;
const FILE_SEARCH_LIMIT_WITH_COPIES: usize = 100;
/// Rows a group shows for a query, files apart.
const GROUP_LIMIT: usize = 7;
const FILE_LIMIT: usize = 10;
/// Sessions and open files listed before anything is typed.
const RECENT_LIMIT: usize = 5;

/// The dialog's width cap, the window margin beside it and how far from the
/// top it sits (`max-w-[min(720px,calc(100vw-2rem))]`, `pt-[12vh]`).
const MAX_WIDTH: f32 = 720.0;
const WINDOW_MARGIN: f32 = 32.0;
const TOP_FRACTION: f32 = 0.12;
/// The list's height: `h-[min(440px,60vh)]`.
const LIST_HEIGHT: f32 = 440.0;
const LIST_FRACTION: f32 = 0.6;
/// Group headings: 11/16 px, in rems.
const HEADING_SIZE: f32 = 0.6875;
const HEADING_LINE: f32 = 1.0;

/// Registers the palette's own keys.
pub(crate) fn bind_keys(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("ctrl-n", PaletteNext, context),
        KeyBinding::new("ctrl-p", PalettePrevious, context),
        KeyBinding::new("alt-down", PaletteNextGroup, context),
        KeyBinding::new("alt-up", PalettePreviousGroup, context),
    ]);
}

/// A command the palette runs on the workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommandId {
    NewSession,
    AddProject,
    ToggleSidebar,
    ToggleTerminal,
    CycleTheme,
    OpenSettings,
    OpenSourceBoard,
    PinSession,
    CopySessionId,
    OpenArchive,
    OpenNotes,
    /// Runs the project action, or stops it, as the row showed when it was
    /// listed and only in the project and directory it was listed for.
    ProjectAction(PaletteProjectAction),
}

impl CommandId {
    /// The command's ID in the web client's list.
    fn key(&self) -> String {
        match self {
            Self::NewSession => "new-session".to_owned(),
            Self::AddProject => "add-project".to_owned(),
            Self::ToggleSidebar => "toggle-sidebar".to_owned(),
            Self::ToggleTerminal => "toggle-terminal".to_owned(),
            Self::CycleTheme => "cycle-theme".to_owned(),
            Self::OpenSettings => "open-settings".to_owned(),
            Self::OpenSourceBoard => "open-source-board".to_owned(),
            Self::PinSession => "pin-session".to_owned(),
            Self::CopySessionId => "copy-session-id".to_owned(),
            Self::OpenArchive => "open-archive".to_owned(),
            Self::OpenNotes => "open-notes".to_owned(),
            Self::ProjectAction(action) => format!("project-action:{}", action.id),
        }
    }
}

/// One command with the text it shows and matches by.
#[derive(Clone, Debug)]
pub(crate) struct CommandEntry {
    pub(crate) id: CommandId,
    pub(crate) title: String,
    pub(crate) icon: &'static str,
    /// The effective shortcut as the Keyboard Shortcuts page writes it.
    pub(crate) shortcut: Option<SharedString>,
    /// Found by typing only, so the first screen stays short.
    pub(crate) secondary: bool,
}

/// A Settings page with its search text: the translated title, the English
/// one, the group and the page's keywords.
#[derive(Clone, Debug)]
pub(crate) struct SettingsEntry {
    pub(crate) page: Page,
    pub(crate) title: String,
    pub(crate) search: String,
}

/// A root session in the sidebar's lifecycle order.
#[derive(Clone, Debug)]
pub(crate) struct SessionEntry {
    pub(crate) id: SessionId,
    /// Empty for an untitled session, which no query matches.
    pub(crate) title: String,
    pub(crate) project: Option<SharedString>,
    pub(crate) branch: Option<SharedString>,
}

/// A registered project.
#[derive(Clone, Debug)]
pub(crate) struct ProjectEntry {
    pub(crate) path: String,
    /// The label from settings, when the project has one.
    pub(crate) label: Option<String>,
}

impl ProjectEntry {
    /// The label, else the folder's name.
    fn display_name(&self) -> &str {
        self.label.as_deref().unwrap_or_else(|| {
            self.path
                .rsplit('/')
                .find(|segment| !segment.is_empty())
                .unwrap_or(&self.path)
        })
    }

    /// `"<label> <path>"`, the text a query runs on.
    fn search_text(&self) -> String {
        format!(
            "{} {}",
            self.label.as_deref().unwrap_or_default(),
            self.path
        )
    }
}

/// A file under the palette's directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileEntry {
    pub(crate) path: String,
    pub(crate) relative_path: String,
}

/// The runtime and directory a palette's lists were taken for. A row runs,
/// and file results land, only while the workspace still stands there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Origin {
    pub(crate) runtime: RuntimeId,
    /// The directory files are searched and opened in.
    pub(crate) root: Option<String>,
}

/// What the workspace hands the palette when it opens.
#[derive(Clone)]
pub(crate) struct Snapshot {
    pub(crate) origin: Origin,
    pub(crate) commands: Vec<CommandEntry>,
    pub(crate) settings: Vec<SettingsEntry>,
    pub(crate) sessions: Vec<SessionEntry>,
    pub(crate) projects: Vec<ProjectEntry>,
    pub(crate) open_files: Vec<FileEntry>,
    /// Worktrees of the repository inside the origin's root, each with a trailing
    /// slash: their files are copies and stay out of the results.
    pub(crate) nested_worktrees: Vec<String>,
    /// The connection file searches go through; absent without a server.
    pub(crate) search: Option<(ServerClient, tokio::runtime::Handle)>,
}

/// The palette's groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Group {
    OpenFiles,
    Commands,
    Settings,
    Sessions,
    Files,
    Projects,
}

impl Group {
    fn label_key(self) -> &'static str {
        match self {
            Self::OpenFiles => "commandPalette.group.openFiles",
            Self::Commands => "commandPalette.group.commands",
            Self::Settings => "commandPalette.group.settings",
            Self::Sessions => "commandPalette.group.sessions",
            Self::Files => "commandPalette.group.files",
            Self::Projects => "commandPalette.group.projects",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::OpenFiles => "open-files",
            Self::Commands => "commands",
            Self::Settings => "settings",
            Self::Sessions => "sessions",
            Self::Files => "files",
            Self::Projects => "projects",
        }
    }
}

/// What choosing a row does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Command(CommandId),
    Settings(Page),
    Session(SessionId),
    /// An absolute file path.
    File(String),
    /// A project path.
    Project(String),
}

/// What a row shows after its title.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Detail {
    None,
    Shortcut(SharedString),
    Session {
        project: Option<SharedString>,
        branch: Option<SharedString>,
    },
    /// A folder or path, with the matched letters marked.
    Path {
        text: SharedString,
        marks: Vec<Range<usize>>,
    },
}

/// One selectable row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    /// The row's element and accessibility ID.
    pub(crate) key: SharedString,
    pub(crate) icon: &'static str,
    pub(crate) title: SharedString,
    /// The matched letters of the title, as byte ranges.
    pub(crate) marks: Vec<Range<usize>>,
    pub(crate) detail: Detail,
    pub(crate) target: Target,
}

/// One line of the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    Heading(Group),
    Item(Box<Row>),
}

/// `matchesWordStarts`: every query word starts a word of `text`.
pub(crate) fn matches_word_starts(text: &str, query: &str) -> bool {
    let text = text.to_lowercase();
    let words: Vec<&str> = text
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|token| words.iter().any(|word| word.starts_with(token)))
}

/// The part of `ranges` that falls on `text`, as byte ranges of `text`.
///
/// `ranges` are character offsets in the string the matcher ran on, and
/// `text` is the run of that string starting at character `start`: a row
/// shows one field of a longer search text (`rangesWithin`).
fn marks(text: &str, ranges: &[(usize, usize)], start: usize) -> Vec<Range<usize>> {
    let offsets: Vec<usize> = text
        .char_indices()
        .map(|(offset, _)| offset)
        .chain([text.len()])
        .collect();
    let end = start + offsets.len() - 1;
    ranges
        .iter()
        .filter_map(|&(from, to)| {
            let from = from.max(start);
            let to = to.min(end);
            (to > from).then(|| offsets[from - start]..offsets[to - start])
        })
        .collect()
}

/// A scored group: its rows and its best match.
struct Scored {
    group: Group,
    rows: Vec<Row>,
    best: f64,
}

impl Scored {
    fn new(group: Group, rows: Vec<Row>, best: Option<f64>) -> Self {
        Self {
            group,
            rows,
            best: best.unwrap_or(f64::NEG_INFINITY),
        }
    }
}

fn best_score(ranked: &[(usize, SubsequenceMatch)]) -> Option<f64> {
    ranked.first().map(|(_, matched)| matched.score)
}

fn command_row(command: &CommandEntry, ranges: &[(usize, usize)]) -> Row {
    Row {
        key: format!("command-palette:{}", command.id.key()).into(),
        icon: command.icon,
        title: command.title.clone().into(),
        marks: marks(&command.title, ranges, 0),
        detail: command
            .shortcut
            .clone()
            .map_or(Detail::None, Detail::Shortcut),
        target: Target::Command(command.id.clone()),
    }
}

fn session_row(session: &SessionEntry, ranges: &[(usize, usize)], untitled: &str) -> Row {
    let title = if session.title.is_empty() {
        untitled
    } else {
        &session.title
    };
    Row {
        key: format!("command-palette:session:{}", session.id.as_str()).into(),
        icon: "chat-ai-3",
        title: title.to_owned().into(),
        // An untitled session never matches, so its ranges are empty.
        marks: marks(title, ranges, 0),
        detail: Detail::Session {
            project: session.project.clone(),
            branch: session.branch.clone(),
        },
        target: Target::Session(session.id.clone()),
    }
}

/// A file row: the name, then its folder, each with the matches inside it
/// (`fileRowMatchRanges`). `ranges` are offsets in the relative path.
fn file_row(file: &FileEntry, ranges: &[(usize, usize)], prefix: &str) -> Row {
    let shown = if file.relative_path.is_empty() {
        &file.path
    } else {
        &file.relative_path
    };
    let (directory, name) = shown.rsplit_once('/').unwrap_or(("", shown));
    let name_start = shown.chars().count() - name.chars().count();
    Row {
        key: format!("command-palette:{prefix}:{}", file.path).into(),
        icon: "file",
        title: name.to_owned().into(),
        marks: marks(name, ranges, name_start),
        detail: if directory.is_empty() {
            Detail::None
        } else {
            Detail::Path {
                text: directory.to_owned().into(),
                marks: marks(directory, ranges, 0),
            }
        },
        target: Target::File(file.path.clone()),
    }
}

/// A project row: the name, then the path (`projectRowMatchRanges`).
/// `ranges` are offsets in `"<label> <path>"`.
fn project_row(project: &ProjectEntry, ranges: &[(usize, usize)]) -> Row {
    let name = project.display_name();
    let path_start = project
        .label
        .as_deref()
        .map_or(0, |label| label.chars().count())
        + 1;
    let name_start = if project.label.is_some() {
        0
    } else {
        path_start + project.path.chars().count() - name.chars().count()
    };
    Row {
        key: format!("command-palette:project:{}", project.path).into(),
        icon: "folder",
        title: name.to_owned().into(),
        marks: marks(name, ranges, name_start),
        detail: Detail::Path {
            text: project.path.clone().into(),
            marks: marks(&project.path, ranges, path_start),
        },
        target: Target::Project(project.path.clone()),
    }
}

/// The files of a finished search for `query`, name matches first
/// (`scoreCommandPaletteFiles`).
fn ranked_files(files: &[FileEntry], query: &str) -> Scored {
    let ranked = search_rank::rank_by_subsequence(
        files,
        query,
        |file| {
            if file.relative_path.is_empty() {
                file.path.as_str()
            } else {
                file.relative_path.as_str()
            }
        },
        FILE_LIMIT,
        true,
    );
    Scored::new(
        Group::Files,
        ranked
            .iter()
            .map(|(index, matched)| file_row(&files[*index], &matched.ranges, "file"))
            .collect(),
        best_score(&ranked),
    )
}

/// The list for `query`.
///
/// `files` are the results of the last finished file search, already
/// matched to the query they were searched for; `untitled` names a session
/// without a title.
pub(crate) fn build_entries(
    snapshot: &Snapshot,
    query: &str,
    files: &[FileEntry],
    file_query: &str,
    untitled: &str,
) -> Vec<Entry> {
    let query = query.trim();
    let mut groups: Vec<Scored> = Vec::new();
    if query.is_empty() {
        groups.push(Scored::new(
            Group::OpenFiles,
            snapshot
                .open_files
                .iter()
                .take(RECENT_LIMIT)
                .map(|file| file_row(file, &[], "open-file"))
                .collect(),
            None,
        ));
        groups.push(Scored::new(
            Group::Sessions,
            snapshot
                .sessions
                .iter()
                .take(RECENT_LIMIT)
                .map(|session| session_row(session, &[], untitled))
                .collect(),
            None,
        ));
        groups.push(Scored::new(
            Group::Commands,
            snapshot
                .commands
                .iter()
                .filter(|command| !command.secondary)
                .map(|command| command_row(command, &[]))
                .collect(),
            None,
        ));
    } else {
        let commands: Vec<&CommandEntry> = snapshot
            .commands
            .iter()
            .filter(|command| matches_word_starts(&command.title, query))
            .collect();
        let ranked = search_rank::rank_by_subsequence(
            &commands,
            query,
            |command| command.title.as_str(),
            GROUP_LIMIT,
            false,
        );
        groups.push(Scored::new(
            Group::Commands,
            ranked
                .iter()
                .map(|(index, matched)| command_row(commands[*index], &matched.ranges))
                .collect(),
            best_score(&ranked),
        ));

        let settings: Vec<&SettingsEntry> = snapshot
            .settings
            .iter()
            .filter(|page| matches_word_starts(&page.search, query))
            .collect();
        let ranked = search_rank::rank_by_subsequence(
            &settings,
            query,
            |page| page.search.as_str(),
            GROUP_LIMIT,
            false,
        );
        groups.push(Scored::new(
            Group::Settings,
            ranked
                .iter()
                .map(|(index, matched)| {
                    let page = settings[*index];
                    Row {
                        key: format!("command-palette:settings:{}", page.page.id()).into(),
                        icon: page.page.icon(),
                        title: page.title.clone().into(),
                        // A match only in the group or keywords leaves the
                        // title plain.
                        marks: marks(&page.title, &matched.ranges, 0),
                        detail: Detail::None,
                        target: Target::Settings(page.page),
                    }
                })
                .collect(),
            best_score(&ranked),
        ));

        let ranked = search_rank::rank_by_subsequence(
            &snapshot.sessions,
            query,
            |session| session.title.as_str(),
            GROUP_LIMIT,
            false,
        );
        groups.push(Scored::new(
            Group::Sessions,
            ranked
                .iter()
                .map(|(index, matched)| {
                    session_row(&snapshot.sessions[*index], &matched.ranges, untitled)
                })
                .collect(),
            best_score(&ranked),
        ));

        groups.push(if files.is_empty() || file_query != query {
            Scored::new(Group::Files, Vec::new(), None)
        } else {
            ranked_files(files, query)
        });

        let projects: Vec<(&ProjectEntry, String)> = snapshot
            .projects
            .iter()
            .map(|project| (project, project.search_text()))
            .filter(|(_, search)| matches_word_starts(search, query))
            .collect();
        let ranked = search_rank::rank_by_subsequence(
            &projects,
            query,
            |(_, search)| search.as_str(),
            GROUP_LIMIT,
            false,
        );
        groups.push(Scored::new(
            Group::Projects,
            ranked
                .iter()
                .map(|(index, matched)| project_row(projects[*index].0, &matched.ranges))
                .collect(),
            best_score(&ranked),
        ));
        // The group with the best match comes first; equal groups keep the
        // order above.
        groups.sort_by(|left, right| right.best.total_cmp(&left.best));
    }
    let mut entries = Vec::new();
    for group in groups {
        if group.rows.is_empty() {
            continue;
        }
        entries.push(Entry::Heading(group.group));
        entries.extend(group.rows.into_iter().map(|row| Entry::Item(Box::new(row))));
    }
    entries
}

/// The entry `delta` items from `current`, wrapping around (`loop`).
pub(crate) fn step_item(entries: &[Entry], current: Option<usize>, delta: isize) -> Option<usize> {
    let items: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| matches!(entry, Entry::Item(_)).then_some(index))
        .collect();
    if items.is_empty() {
        return None;
    }
    let position = current.and_then(|current| items.iter().position(|index| *index == current));
    let next = match position {
        Some(position) => (position as isize + delta).rem_euclid(items.len() as isize) as usize,
        None if delta < 0 => items.len() - 1,
        None => 0,
    };
    Some(items[next])
}

/// The first item of the group after (`forward`) or before the one holding
/// `current`; the neighbouring item when there is no such group.
pub(crate) fn step_group(
    entries: &[Entry],
    current: Option<usize>,
    forward: bool,
) -> Option<usize> {
    let fallback = || step_item(entries, current, if forward { 1 } else { -1 });
    let Some(current) = current else {
        return fallback();
    };
    let heading = |index: &usize| matches!(entries.get(*index), Some(Entry::Heading(_)));
    let target = if forward {
        (current + 1..entries.len()).find(heading)
    } else {
        let own = (0..=current).rev().find(heading);
        own.and_then(|own| (0..own).rev().find(heading))
    };
    match target {
        Some(heading) if matches!(entries.get(heading + 1), Some(Entry::Item(_))) => {
            Some(heading + 1)
        }
        _ => fallback(),
    }
}

/// The results of the last finished file search and the one in flight.
#[derive(Default)]
struct FileSearch {
    /// The trimmed query `hits` answer; empty before any search finished.
    query: String,
    hits: Vec<FileEntry>,
    pending: Option<Task<()>>,
}

/// Which row a rebuilt list selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Selection {
    /// The first row, at the top of the list: the query changed.
    First,
    /// The row that was selected, where the list is scrolled: results for
    /// the same query arrived.
    Keep,
}

/// The open palette.
pub(crate) struct CommandPalette {
    workspace: WeakEntity<Workspace>,
    input: Entity<InputState>,
    snapshot: Snapshot,
    entries: Vec<Entry>,
    /// The selected item's index in `entries`.
    selected: Option<usize>,
    scroll: ScrollHandle,
    files: FileSearch,
    _subscription: Subscription,
}

impl CommandPalette {
    fn new(
        workspace: WeakEntity<Workspace>,
        snapshot: Snapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = locale::strings(cx).get("commandPalette.input.placeholder");
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let subscription =
            cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                InputEvent::Change => this.query_changed(cx),
                InputEvent::PressEnter { .. } => this.run_selected(window, cx),
                _ => {}
            });
        let mut palette = Self {
            workspace,
            input,
            snapshot,
            entries: Vec::new(),
            selected: None,
            scroll: ScrollHandle::new(),
            files: FileSearch::default(),
            _subscription: subscription,
        };
        palette.rebuild(Selection::First, cx);
        palette
    }

    /// The query field.
    #[cfg(test)]
    pub(crate) fn input(&self) -> &Entity<InputState> {
        &self.input
    }

    /// The selected row's key.
    pub(crate) fn selected_key(&self) -> Option<SharedString> {
        match self.entries.get(self.selected?) {
            Some(Entry::Item(row)) => Some(row.key.clone()),
            _ => None,
        }
    }

    /// The listed rows' keys, in order.
    #[cfg(test)]
    pub(crate) fn row_keys(&self) -> Vec<SharedString> {
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Item(row) => Some(row.key.clone()),
                Entry::Heading(_) => None,
            })
            .collect()
    }

    /// The query field's focus handle.
    pub(crate) fn input_focus(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }

    fn query(&self, cx: &App) -> String {
        self.input.read(cx).value().trim().to_owned()
    }

    /// Lists the rows for the query in the field. A kept selection follows
    /// its row to wherever the new list puts it and leaves the scroll
    /// position alone; when that row is gone the first one is selected.
    fn rebuild(&mut self, selection: Selection, cx: &mut Context<Self>) {
        let kept = match selection {
            Selection::First => None,
            Selection::Keep => self.selected_key(),
        };
        let untitled = locale::strings(cx).get("commandPalette.session.untitled");
        let query = self.query(cx);
        self.entries = build_entries(
            &self.snapshot,
            &query,
            &self.files.hits,
            &self.files.query,
            &untitled,
        );
        let kept = kept.and_then(|key| {
            self.entries
                .iter()
                .position(|entry| matches!(entry, Entry::Item(row) if row.key == key))
        });
        match kept {
            Some(index) => self.selected = Some(index),
            None => {
                self.selected = step_item(&self.entries, None, 1);
                self.scroll.scroll_to_item(0);
            }
        }
        cx.notify();
    }

    fn query_changed(&mut self, cx: &mut Context<Self>) {
        self.search_files(cx);
        self.rebuild(Selection::First, cx);
    }

    /// Takes a finished file search: its hits, or none when it failed. The
    /// selection and the scroll position stay where the user has them.
    /// Results for a query the field no longer holds change nothing, and
    /// results for a runtime or directory the workspace has left list no
    /// files.
    pub(crate) fn apply_file_results(
        &mut self,
        query: String,
        hits: Vec<FileEntry>,
        cx: &mut Context<Self>,
    ) {
        if self.query(cx) != query {
            return;
        }
        let current = self
            .workspace
            .upgrade()
            .is_some_and(|workspace| workspace.read(cx).palette_origin() == self.snapshot.origin);
        self.files = FileSearch {
            query,
            hits: if current { hits } else { Vec::new() },
            pending: None,
        };
        self.rebuild(Selection::Keep, cx);
    }

    /// Asks the file index for the query once typing has stopped. A search
    /// for a query the field no longer holds changes nothing.
    fn search_files(&mut self, cx: &mut Context<Self>) {
        let query = self.query(cx);
        let (Some(root), Some((client, network))) = (
            self.snapshot.origin.root.clone(),
            self.snapshot.search.clone(),
        ) else {
            return;
        };
        if query.is_empty() {
            self.files = FileSearch::default();
            return;
        }
        if self.files.query == query {
            self.files.pending = None;
            return;
        }
        let excluded = self.snapshot.nested_worktrees.clone();
        let limit = if excluded.is_empty() {
            FILE_SEARCH_LIMIT
        } else {
            FILE_SEARCH_LIMIT_WITH_COPIES
        };
        self.files.pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FILE_DEBOUNCE).await;
            let request = query.clone();
            let result = NetworkTask::spawn_on(&network, async move {
                client.find_files(&root, &request, limit).await
            })
            .await;
            // A failed search lists no files; the other groups stand.
            let hits: Vec<FileEntry> = result
                .and_then(Result::ok)
                .unwrap_or_default()
                .into_iter()
                .filter(|hit| !excluded.iter().any(|prefix| hit.path.starts_with(prefix)))
                .map(|hit| FileEntry {
                    path: hit.path,
                    relative_path: hit.relative_path,
                })
                .collect();
            let _ = this.update(cx, |this, cx| this.apply_file_results(query, hits, cx));
        }));
    }

    fn select(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        let Some(index) = index else {
            return;
        };
        if self.selected == Some(index) {
            return;
        }
        self.selected = Some(index);
        // A group's first row brings its heading into view with it.
        let first_in_group =
            index > 0 && matches!(self.entries.get(index - 1), Some(Entry::Heading(_)));
        self.scroll
            .scroll_to_item(if first_in_group { index - 1 } else { index });
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.select(step_item(&self.entries, self.selected, delta), cx);
    }

    fn step_group(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.select(step_group(&self.entries, self.selected, forward), cx);
    }

    fn select_edge(&mut self, last: bool, cx: &mut Context<Self>) {
        self.select(
            step_item(&self.entries, None, if last { -1 } else { 1 }),
            cx,
        );
    }

    /// The pointer selects the row under it without scrolling the list.
    fn hover(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.selected != Some(index) {
            self.selected = Some(index);
            cx.notify();
        }
    }

    fn run_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected {
            self.run(index, window, cx);
        }
    }

    /// Closes the palette, then runs the row's command on the workspace.
    fn run(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Entry::Item(row)) = self.entries.get(index) else {
            return;
        };
        let target = row.target.clone();
        let origin = self.snapshot.origin.clone();
        let workspace = self.workspace.clone();
        window.close_dialog(cx);
        window.defer(cx, move |window, cx| {
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.finish_command_palette(window, cx);
                workspace.run_palette_target(&origin, target, window, cx);
            });
        });
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        window.close_dialog(cx);
        window.defer(cx, move |window, cx| {
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.finish_command_palette(window, cx);
            });
        });
    }

    fn render_row(
        &self,
        index: usize,
        row: &Row,
        last_in_group: bool,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.selected == Some(index);
        let hover = palette.hover;
        let mark = HighlightStyle {
            background_color: Some(palette.primary.opacity(0.25)),
            color: Some(palette.foreground),
            ..HighlightStyle::default()
        };
        let marked = |text: &SharedString, marks: &[Range<usize>]| {
            StyledText::new(text.clone())
                .with_highlights(marks.iter().map(|range| (range.clone(), mark)))
        };
        let muted = palette.muted_foreground;
        let detail: Option<AnyElement> = match &row.detail {
            Detail::None => None,
            // A shortcut reads left to right in every language.
            Detail::Shortcut(shortcut) => Some(
                div()
                    .id(SharedString::from(format!("{}:shortcut", row.key)))
                    .flex_shrink_0()
                    .text_color(muted)
                    .child(shortcut.clone())
                    .test_support()
                    .into_any_element(),
            ),
            Detail::Session { project, branch } => Some(
                div()
                    .flex_shrink_0()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(8.0))
                    .text_color(muted)
                    .children(
                        project
                            .clone()
                            .map(|project| div().max_w(px(180.0)).truncate().child(project)),
                    )
                    .children(branch.clone().map(|branch| {
                        div()
                            .max_w(px(160.0))
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(4.0))
                            .child(glyph("git-branch", 12.0, muted))
                            .child(div().min_w_0().truncate().child(branch))
                    }))
                    .into_any_element(),
            ),
            Detail::Path { text, marks } => Some(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(muted)
                    .child(marked(text, marks))
                    .into_any_element(),
            ),
        };
        // A title before a path keeps its width; any other title takes the
        // room the detail leaves.
        let title_shrinks = !matches!(row.detail, Detail::Path { .. });
        on_activate(
            div()
                .id(row.key.clone())
                .role(Role::ListBoxOption)
                .accessibility_id(row.key.clone())
                .aria_label(row.title.clone())
                .aria_selected(selected)
                .mx(px(8.0))
                .when(last_in_group, |this| this.mb(px(4.0)))
                .px(px(8.0))
                .py(px(6.0))
                .rounded(px(8.0))
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .cursor_pointer()
                .css_text(META_SIZE, META_LINE, cx)
                .when(selected, |this| {
                    this.bg(palette.selection)
                        .text_color(palette.selection_foreground)
                })
                .when(!selected, |this| this.hover(move |style| style.bg(hover)))
                .on_mouse_move(cx.listener(move |this, _, _, cx| this.hover(index, cx)))
                .child(
                    div()
                        .flex_shrink_0()
                        .margin_end(direction, px(8.0))
                        .child(glyph(row.icon, 16.0, muted)),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("{}:title", row.key)))
                        .when(title_shrinks, |this| this.flex_1())
                        .when(!title_shrinks, |this| this.flex_shrink_0())
                        .min_w_0()
                        .truncate()
                        .child(marked(&row.title, &row.marks))
                        .test_support(),
                )
                .children(detail),
            cx,
            move |this, window, cx| this.run(index, window, cx),
        )
        .test_support()
        .into_any_element()
    }
}

impl Render for CommandPalette {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let list_height = LIST_HEIGHT.min(f32::from(window.viewport_size().height) * LIST_FRACTION);
        let mut rows: Vec<AnyElement> = Vec::with_capacity(self.entries.len());
        for (index, entry) in self.entries.iter().enumerate() {
            rows.push(match entry {
                Entry::Heading(group) => div()
                    .id(SharedString::from(format!(
                        "command-palette-group:{}",
                        group.id()
                    )))
                    .px(px(16.0))
                    .pt(px(12.0))
                    .pb(px(4.0))
                    .css_text(HEADING_SIZE, HEADING_LINE, cx)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(palette.muted_foreground)
                    .child(strings.get(group.label_key()))
                    .into_any_element(),
                Entry::Item(row) => {
                    let last_in_group =
                        !matches!(self.entries.get(index + 1), Some(Entry::Item(_)));
                    self.render_row(index, row, last_in_group, &palette, direction, cx)
                }
            });
        }
        let searching_files = self.files.pending.is_some();
        let empty = self.entries.is_empty() && !searching_files;
        let escape_input = self.input.clone();
        let hint = |keys: &'static str, label: &'static str| {
            div()
                .flex_row_from(direction)
                .gap(px(4.0))
                .child(keys)
                .child(strings.get(label))
        };
        div()
            .id("command-palette-shortcuts")
            // The workspace's shortcuts reach the dialog through this copy
            // of its context: the one that opened the palette closes it.
            .key_context(workspace_view::KEY_CONTEXT)
            .on_action(cx.listener(|this, action: &RunShortcut, window, cx| {
                if action.id == SHORTCUT {
                    this.close(window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .child(
                div()
                    .id("command-palette")
                    .role(Role::Group)
                    .accessibility_id("command-palette")
                    .aria_label(strings.get("commandPalette.title"))
                    .key_context(KEY_CONTEXT)
                    // The field's own arrow, Home, End and Escape keys act
                    // on the list. Escape that ends an IME composition
                    // stays with the field.
                    .capture_action(cx.listener(move |this, _: &Escape, window, cx| {
                        let composing = escape_input.update(cx, |input, cx| {
                            input.marked_text_range(window, cx).is_some()
                        });
                        if !composing {
                            cx.stop_propagation();
                            this.close(window, cx);
                        }
                    }))
                    .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                        cx.stop_propagation();
                        this.step(1, cx);
                    }))
                    .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                        cx.stop_propagation();
                        this.step(-1, cx);
                    }))
                    .capture_action(cx.listener(|this, _: &MoveHome, _, cx| {
                        cx.stop_propagation();
                        this.select_edge(false, cx);
                    }))
                    .capture_action(cx.listener(|this, _: &MoveEnd, _, cx| {
                        cx.stop_propagation();
                        this.select_edge(true, cx);
                    }))
                    .on_action(cx.listener(|this, _: &PaletteNext, _, cx| this.step(1, cx)))
                    .on_action(cx.listener(|this, _: &PalettePrevious, _, cx| this.step(-1, cx)))
                    .on_action(
                        cx.listener(|this, _: &PaletteNextGroup, _, cx| this.step_group(true, cx)),
                    )
                    .on_action(cx.listener(|this, _: &PalettePreviousGroup, _, cx| {
                        this.step_group(false, cx)
                    }))
                    .w_full()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .rounded(px(12.0))
                    .bg(palette.elevated)
                    .text_color(palette.elevated_foreground)
                    .child(
                        div()
                            .h(px(32.0))
                            .flex_shrink_0()
                            .px(px(12.0))
                            .border_b_1()
                            .border_color(palette.border)
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(8.0))
                            .child(glyph(
                                "search",
                                16.0,
                                palette.elevated_foreground.opacity(0.5),
                            ))
                            .child(
                                div().flex_1().min_w_0().child(
                                    Input::new(&self.input)
                                        .appearance(false)
                                        .bordered(false)
                                        .text_size(rems(META_SIZE)),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .id("command-palette-list")
                            .role(Role::ListBox)
                            .accessibility_id("command-palette-list")
                            .aria_label(strings.get("commandPalette.description"))
                            .h(px(list_height))
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .flex()
                            .flex_col()
                            .children(rows)
                            .when(empty, |this| {
                                this.child(
                                    div()
                                        .id("command-palette-empty")
                                        .role(Role::Status)
                                        .accessibility_id("command-palette-empty")
                                        .py(px(24.0))
                                        .text_center()
                                        .css_text(LABEL_SIZE, LABEL_LINE, cx)
                                        .child(strings.get("commandPalette.empty.noResults"))
                                        .test_support(),
                                )
                            })
                            .when(searching_files, |this| {
                                this.child(
                                    div()
                                        .id("command-palette-searching")
                                        .role(Role::Status)
                                        .accessibility_id("command-palette-searching")
                                        .px(px(16.0))
                                        .py(px(8.0))
                                        .css_text(META_SIZE, META_LINE, cx)
                                        .text_color(palette.muted_foreground)
                                        .child(strings.get("commandPalette.empty.searchingFiles"))
                                        .test_support(),
                                )
                            })
                            .test_support(),
                    )
                    .child(
                        div()
                            .id("command-palette-footer")
                            .flex_shrink_0()
                            .px(px(12.0))
                            .py(px(6.0))
                            .border_t_1()
                            .border_color(palette.border)
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(16.0))
                            .css_text(MICRO_SIZE, MICRO_LINE, cx)
                            .text_color(palette.muted_foreground)
                            .child(hint("↑↓", "commandPalette.footer.navigate"))
                            .child(hint("↵", "commandPalette.footer.open"))
                            .child(hint("esc", "commandPalette.footer.close")),
                    )
                    .test_support(),
            )
    }
}

/// The open palette and the focus it returns.
pub(crate) struct OpenPalette {
    #[cfg(test)]
    pub(crate) palette: Entity<CommandPalette>,
    restore: Option<FocusHandle>,
}

/// A native Settings page's English title, group and keywords in
/// `SETTINGS_PAGE_METADATA`, which stay searchable next to the translated
/// title.
fn settings_search_terms(page: Page) -> (&'static str, &'static str, &'static str) {
    match page {
        Page::General => (
            "General",
            "general",
            "general startup launch at login autostart tray password passkey security privacy \
             telemetry transport network lan binary cli",
        ),
        Page::Appearance => (
            "Appearance",
            "general",
            "theme font spacing padding corner radius radius input bar keyboard viewport mobile \
             terminal pwa install name app shortcuts",
        ),
        Page::Chat => (
            "Chat",
            "general",
            "tools diff reasoning dotfiles draft queue output copy image split messages message \
             actions",
        ),
        Page::Notifications => (
            "Notifications",
            "general",
            "alerts native summary summarization",
        ),
        Page::Sessions => (
            "Sessions",
            "general",
            "defaults default agent default model retention memory limits zen",
        ),
        Page::Shortcuts => (
            "Shortcuts",
            "general",
            "keyboard hotkeys shortcuts bindings",
        ),
        Page::Voice => ("Voice", "general", "tts speech voice"),
        Page::Usage => ("Usage", "general", "quota billing tokens usage limits"),
        Page::Hosts => (
            "Remote Instances",
            "projects",
            "ssh remote instances tunnels forwarding connection",
        ),
        Page::Behavior => (
            "Behavior",
            "opencode",
            "behavior agents.md system prompt global rules instructions override",
        ),
        Page::About => (
            "About",
            "general",
            "about version updates release changelog canary channel",
        ),
        Page::Git => (
            "Git",
            "projects",
            "git github gitlab identity identities ssh profiles credentials keys commit gitmoji \
             oauth prs issues",
        ),
        Page::Commands => (
            "Commands",
            "opencode",
            "command commands slash macros automation",
        ),
        Page::MagicPrompts => (
            "Magic Prompts",
            "content",
            "prompts templates git github review commit pull request",
        ),
        Page::Snippets => (
            "Snippets",
            "content",
            "prompt templates multi-run strategy approach",
        ),
    }
}

impl Workspace {
    /// Opens the palette, or closes it when it is open (`toggleCommandPalette`).
    pub(crate) fn toggle_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.command_palette.is_some() {
            self.close_command_palette(window, cx);
        } else {
            self.open_command_palette(window, cx);
        }
    }

    fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.command_palette_snapshot(cx);
        let restore = window.focused(cx);
        let owner = cx.weak_entity();
        let palette = cx.new(|cx| CommandPalette::new(owner.clone(), snapshot, window, cx));
        self.command_palette = Some(OpenPalette {
            #[cfg(test)]
            palette: palette.clone(),
            restore,
        });
        let focus = palette.read(cx).input_focus(cx);
        window.open_dialog(cx, move |dialog, window, _| {
            let viewport = window.viewport_size();
            let width = MAX_WIDTH.min(f32::from(viewport.width) - WINDOW_MARGIN);
            let owner = owner.clone();
            let palette = palette.clone();
            dialog
                .w(px(width))
                .margin_top(viewport.height * TOP_FRACTION)
                .p_0()
                .close_button(false)
                // The palette closes on Escape itself, so an Escape that
                // only ends an IME composition never reaches the dialog.
                .keyboard(false)
                // Enter runs the selected row; the dialog's own OK does nothing.
                .on_ok(|_, _, _| false)
                // Escape and a press outside close the dialog themselves; the
                // workspace then forgets the palette and returns the focus.
                .on_close(move |_, window, cx| {
                    let owner = owner.clone();
                    window.defer(cx, move |window, cx| {
                        let _ =
                            owner.update(cx, |owner, cx| owner.finish_command_palette(window, cx));
                    });
                })
                .content(move |content, _, _| content.child(palette.clone()))
        });
        focus.focus(window, cx);
        cx.notify();
    }

    /// Closes the open palette and returns the focus it took.
    pub(crate) fn close_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.command_palette.is_none() {
            return;
        }
        window.close_dialog(cx);
        self.finish_command_palette(window, cx);
    }

    /// Forgets the palette once its dialog is gone, and gives focus back to
    /// what had it, or to the workspace, so its shortcuts keep working.
    pub(crate) fn finish_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.command_palette.take() else {
            return;
        };
        match open.restore {
            Some(handle) => handle.focus(window, cx),
            None => self.focus_handle.focus(window, cx),
        }
        cx.notify();
    }

    /// The lists the palette searches, as the workspace stands now.
    fn command_palette_snapshot(&mut self, cx: &mut Context<Self>) -> Snapshot {
        let strings = locale::strings(cx).clone();
        let origin = self.palette_origin();
        let root = origin.root.clone();
        let nested_worktrees = root.as_deref().map_or_else(Vec::new, |root| {
            let inside = format!("{root}/");
            self.topology
                .worktrees
                .iter()
                .flat_map(|entry| &entry.worktrees)
                .filter(|worktree| worktree.path.starts_with(&inside))
                .map(|worktree| format!("{}/", worktree.path))
                .collect()
        });
        let open_files = root
            .as_deref()
            .and_then(|root| {
                let path = self.open_file_path(root, cx)?;
                let relative_path = path
                    .strip_prefix(&format!("{root}/"))
                    .unwrap_or(&path)
                    .to_owned();
                Some(FileEntry {
                    path,
                    relative_path,
                })
            })
            .into_iter()
            .collect();
        let search = self
            .connection
            .as_ref()
            .filter(|connection| connection.state == ConnectionState::Connected)
            .and_then(|connection| Some((connection.client.clone()?, connection.network.handle())));
        Snapshot {
            origin,
            commands: self.palette_commands(&strings, cx),
            settings: Page::ALL
                .into_iter()
                .map(|page| {
                    let title = strings.get(page.title()).to_string();
                    let (english, group, keywords) = settings_search_terms(page);
                    SettingsEntry {
                        page,
                        search: format!("{title} {english} {group} {keywords}"),
                        title,
                    }
                })
                .collect(),
            sessions: self.palette_sessions(),
            projects: self
                .topology
                .projects
                .iter()
                .map(|project| ProjectEntry {
                    path: project.path.clone(),
                    label: self.send_catalog.settings.as_ref().and_then(|settings| {
                        settings
                            .project_labels
                            .iter()
                            .find(|entry| entry.path == project.path)
                            .map(|entry| entry.label.clone())
                    }),
                })
                .collect(),
            open_files,
            nested_worktrees,
            search,
        }
    }

    /// The commands this client can run, in the web client's order.
    fn palette_commands(&self, strings: &Strings, cx: &App) -> Vec<CommandEntry> {
        let overrides = cx
            .try_global::<ShortcutBindings>()
            .map(|bindings| bindings.overrides.clone())
            .unwrap_or_default();
        let shortcut = |id: &str| {
            let combo = shortcut_schema::effective_combo(id, &overrides);
            let shown = shortcut_schema::format_for_display(&combo, "", Platform::current());
            (!shown.is_empty()).then(|| SharedString::from(shown))
        };
        let command = |id: CommandId,
                       title: &str,
                       icon: &'static str,
                       shortcut_id: Option<&str>,
                       secondary: bool| CommandEntry {
            id,
            title: strings.get(title).to_string(),
            icon,
            shortcut: shortcut_id.and_then(&shortcut),
            secondary,
        };
        let mut commands = vec![command(
            CommandId::NewSession,
            "commandPalette.item.newSession",
            "add",
            Some("new_chat"),
            false,
        )];
        // Add Project asks the server to register a folder.
        if self.session_actions_available() {
            commands.push(command(
                CommandId::AddProject,
                "commandPalette.item.addProject",
                "folder-add",
                None,
                false,
            ));
        }
        commands.extend([
            command(
                CommandId::ToggleSidebar,
                "commandPalette.item.toggleSidebar",
                "layout-left",
                Some("toggle_sidebar"),
                false,
            ),
            command(
                CommandId::ToggleTerminal,
                "commandPalette.item.toggleTerminal",
                "terminal-box",
                Some("toggle_terminal"),
                false,
            ),
            command(
                CommandId::CycleTheme,
                "commandPalette.item.cycleTheme",
                "palette",
                Some("cycle_theme"),
                true,
            ),
            command(
                CommandId::OpenSettings,
                "commandPalette.item.openSettings",
                "settings-3",
                Some("open_settings"),
                false,
            ),
            command(
                CommandId::OpenSourceBoard,
                "sourceBoard.title",
                "git-pull-request",
                Some("toggle_source_board"),
                true,
            ),
            command(
                CommandId::PinSession,
                "commandPalette.item.pinSession",
                "pushpin",
                None,
                true,
            ),
            command(
                CommandId::CopySessionId,
                "commandPalette.item.copySessionId",
                "file-copy",
                None,
                true,
            ),
            command(
                CommandId::OpenArchive,
                "commandPalette.item.openArchive",
                "archive",
                None,
                true,
            ),
            command(
                CommandId::OpenNotes,
                "commandPalette.item.openNotes",
                "sticky-note",
                None,
                true,
            ),
        ]);
        // Search-only: typing "run", "stop" or an action's name finds them.
        for action in self.palette_project_actions(cx) {
            let key = if action.running {
                "commandPalette.item.stopProjectAction"
            } else {
                "commandPalette.item.runProjectAction"
            };
            commands.push(CommandEntry {
                title: strings
                    .format(key, &[("name", action.name.as_str())])
                    .to_string(),
                icon: action.glyph,
                shortcut: None,
                secondary: true,
                id: CommandId::ProjectAction(action),
            });
        }
        commands
    }

    /// The root sessions the sidebar admits, in lifecycle order
    /// (`orderVisibleRootSessions`): `/btw` forks stay hidden and subagent
    /// sessions are reached through their parent.
    fn palette_sessions(&mut self) -> Vec<SessionEntry> {
        // Pins the server has not answered for yet order at once.
        let pinned = self.session_management.pins.overlay(self.sessions.active());
        let active = pinned.as_deref().unwrap_or(self.sessions.active());
        let roots: Vec<&SessionRecord> = active
            .iter()
            .filter(|session| {
                !session.is_btw() && session.is_root() && self.session_admitted(&session.id)
            })
            .collect();
        let ordered = self.ordering.order_scopes(&roots);
        ordered
            .into_iter()
            .map(|session| {
                let directory = session.resolved_directory();
                SessionEntry {
                    id: session.id.clone(),
                    title: session.title.clone(),
                    project: directory
                        .as_deref()
                        .and_then(|directory| self.project_of_directory(directory))
                        .map(|project| self.project_label(&project)),
                    branch: directory.as_deref().and_then(|directory| {
                        self.topology
                            .worktrees
                            .iter()
                            .flat_map(|entry| &entry.worktrees)
                            .find(|worktree| worktree.path == directory)
                            .and_then(|worktree| worktree.branch.as_deref())
                            .map(str::trim)
                            .filter(|branch| !branch.is_empty())
                            .map(|branch| SharedString::from(branch.to_owned()))
                    }),
                }
            })
            .collect()
    }

    /// The runtime and directory a palette opened now would list.
    pub(crate) fn palette_origin(&self) -> Origin {
        Origin {
            runtime: self.runtime.clone(),
            root: self.panel_directory(),
        }
    }

    /// Runs what a chosen row stands for, while the workspace still shows
    /// the runtime and directory the row was listed for. A row from another
    /// server or directory names sessions, files and projects that are not
    /// these ones, so it does nothing.
    pub(crate) fn run_palette_target(
        &mut self,
        origin: &Origin,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if *origin != self.palette_origin() {
            return;
        }
        match target {
            Target::Command(command) => self.run_palette_command(command, window, cx),
            Target::Settings(page) => {
                self.settings_host_manager(window, cx)
                    .update(cx, |hosts, cx| hosts.prepare_settings(cx));
                self.show_settings(Some(page), window, cx);
            }
            Target::Session(session) => self.select_session(session, window, cx),
            Target::File(path) => {
                self.open_context_file(path, cx);
            }
            Target::Project(path) => {
                // A project removed since the palette opened leaves the
                // selected session where it is.
                if self.draft_destination_project(&path).is_none() {
                    return;
                }
                self.show_new_session(window, cx);
                self.choose_draft_project(&path, window, cx);
            }
        }
    }

    fn run_palette_command(
        &mut self,
        command: CommandId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            CommandId::NewSession => {
                self.open_new_session(crate::chat_draft::NewSession::Plain, window, cx)
            }
            CommandId::AddProject => self.open_add_project(None, window, cx),
            CommandId::ToggleSidebar => self.toggle_sidebar(cx),
            CommandId::ToggleTerminal => self.toggle_terminal(window, cx),
            CommandId::CycleTheme => {
                self.run_shortcut("cycle_theme", window, cx);
            }
            CommandId::OpenSettings => self.open_settings(window, cx),
            CommandId::OpenSourceBoard => {
                if !self.source_board_open {
                    self.toggle_source_board(window, cx);
                }
            }
            CommandId::PinSession => {
                if let Some(session) = self.selected.clone() {
                    let pinned = self
                        .sessions
                        .get(&session)
                        .is_some_and(|record| self.session_management.pins.shows_pinned(record));
                    self.set_pinned(&session, !pinned, window, cx);
                }
            }
            CommandId::CopySessionId => {
                if let Some(session) = self.selected.clone() {
                    self.copy_session_id(&session, window, cx);
                }
            }
            CommandId::OpenArchive => {
                if !self.archive_open {
                    self.toggle_archive_page(window, cx);
                }
            }
            CommandId::OpenNotes => {
                if self.panels.open != Some(PanelSurface::Notes) {
                    self.toggle_panel(PanelSurface::Notes, cx);
                }
            }
            CommandId::ProjectAction(action) => {
                self.run_palette_project_action(&action, window, cx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(id: CommandId, title: &str, secondary: bool) -> CommandEntry {
        CommandEntry {
            id,
            title: title.to_owned(),
            icon: "add",
            shortcut: None,
            secondary,
        }
    }

    fn session(id: &str, title: &str) -> SessionEntry {
        SessionEntry {
            id: SessionId::from(id),
            title: title.to_owned(),
            project: None,
            branch: None,
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            commands: vec![
                command(CommandId::NewSession, "New Session", false),
                command(CommandId::ToggleSidebar, "Toggle Sidebar", false),
                command(CommandId::ToggleTerminal, "Toggle Terminal", false),
                command(CommandId::CycleTheme, "Cycle theme", true),
                command(CommandId::OpenArchive, "Open archived sessions", true),
            ],
            settings: vec![
                SettingsEntry {
                    page: Page::Appearance,
                    title: "Appearance".to_owned(),
                    search: "Appearance Appearance general theme font".to_owned(),
                },
                SettingsEntry {
                    page: Page::Behavior,
                    title: "Behavior".to_owned(),
                    search: "Behavior Behavior opencode agents.md".to_owned(),
                },
            ],
            sessions: (0..8)
                .map(|index| session(&format!("s{index}"), &format!("Relay limits {index}")))
                .chain([session("untitled", "")])
                .collect(),
            projects: vec![
                ProjectEntry {
                    path: "/work/openchamber".to_owned(),
                    label: None,
                },
                ProjectEntry {
                    path: "/work/site".to_owned(),
                    label: Some("Docs".to_owned()),
                },
            ],
            open_files: vec![FileEntry {
                path: "/work/openchamber/src/main.rs".to_owned(),
                relative_path: "src/main.rs".to_owned(),
            }],
            origin: Origin {
                runtime: RuntimeId::from("test"),
                root: Some("/work/openchamber".to_owned()),
            },
            nested_worktrees: Vec::new(),
            search: None,
        }
    }

    fn groups(entries: &[Entry]) -> Vec<Group> {
        entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Heading(group) => Some(*group),
                Entry::Item(_) => None,
            })
            .collect()
    }

    fn rows(entries: &[Entry], group: Group) -> Vec<&Row> {
        entries
            .iter()
            .skip_while(|entry| **entry != Entry::Heading(group))
            .skip(1)
            .map_while(|entry| match entry {
                Entry::Item(row) => Some(&**row),
                Entry::Heading(_) => None,
            })
            .collect()
    }

    #[test]
    fn word_starts_match_and_word_tails_do_not() {
        assert!(matches_word_starts("TypeScript settings", "type set"));
        assert!(
            !matches_word_starts("TypeScript settings", "ts"),
            "a capital inside a word starts no word"
        );
        assert!(!matches_word_starts("Agents", "ts"));
        assert!(matches_word_starts("Open archived sessions", "arch open"));
        assert!(!matches_word_starts("Toggle Sidebar", "bar"));
        assert!(matches_word_starts("Behavior opencode agents.md", "md"));
        assert!(matches_word_starts("anything", "  "));
    }

    #[test]
    fn marks_follow_the_shown_field_in_bytes() {
        // "é" is two bytes; the matcher counts it as one character.
        assert_eq!(marks("résumé", &[(1, 3)], 0), vec![1..4]);
        // The row shows characters 4.. of the matched text.
        assert_eq!(
            marks("main.rs", &[(0, 2), (4, 6), (9, 20)], 4),
            vec![0..2, 5..7]
        );
        assert!(marks("main", &[(10, 12)], 0).is_empty());
    }

    #[test]
    fn an_empty_query_lists_open_files_recent_sessions_and_everyday_commands() {
        let entries = build_entries(&snapshot(), "  ", &[], "", "Untitled Session");
        assert_eq!(
            groups(&entries),
            [Group::OpenFiles, Group::Sessions, Group::Commands]
        );
        assert_eq!(rows(&entries, Group::Sessions).len(), RECENT_LIMIT);
        let commands = rows(&entries, Group::Commands);
        assert_eq!(
            commands.len(),
            3,
            "search-only commands stay off the first screen"
        );
        assert!(commands.iter().all(|row| row.marks.is_empty()));
        let open = rows(&entries, Group::OpenFiles);
        assert_eq!(open[0].title.as_ref(), "main.rs");
        assert_eq!(
            open[0].detail,
            Detail::Path {
                text: "src".into(),
                marks: Vec::new()
            }
        );
    }

    #[test]
    fn a_query_finds_search_only_commands_and_settings_pages() {
        let entries = build_entries(&snapshot(), "theme", &[], "", "Untitled Session");
        assert_eq!(groups(&entries), [Group::Commands, Group::Settings]);
        let commands = rows(&entries, Group::Commands);
        assert_eq!(commands[0].target, Target::Command(CommandId::CycleTheme));
        assert_eq!(commands[0].marks, vec![6..11]);
        let settings = rows(&entries, Group::Settings);
        assert_eq!(settings[0].target, Target::Settings(Page::Appearance));
        assert!(
            settings[0].marks.is_empty(),
            "a keyword match leaves the title plain"
        );
    }

    #[test]
    fn groups_order_by_their_best_match_and_cap_their_rows() {
        let entries = build_entries(&snapshot(), "relay", &[], "", "Untitled Session");
        assert_eq!(groups(&entries), [Group::Sessions]);
        let sessions = rows(&entries, Group::Sessions);
        assert_eq!(sessions.len(), GROUP_LIMIT);
        assert_eq!(sessions[0].marks, vec![0..5]);

        // Both groups hold a whole-word match, so they keep the list's own
        // order: commands before sessions.
        let mut snapshot = snapshot();
        snapshot.sessions = vec![session("a", "Fix toggle flicker")];
        let entries = build_entries(&snapshot, "toggle", &[], "", "Untitled Session");
        assert_eq!(groups(&entries), [Group::Commands, Group::Sessions]);
        let commands = rows(&entries, Group::Commands);
        assert_eq!(
            commands[0].target,
            Target::Command(CommandId::ToggleSidebar),
            "equal scores go to the shorter title"
        );
        assert_eq!(
            commands[1].target,
            Target::Command(CommandId::ToggleTerminal)
        );
    }

    #[test]
    fn untitled_sessions_show_their_placeholder_and_never_match() {
        let mut snapshot = snapshot();
        snapshot.sessions = vec![session("untitled", "")];
        let entries = build_entries(&snapshot, "", &[], "", "Untitled Session");
        assert_eq!(
            rows(&entries, Group::Sessions)[0].title.as_ref(),
            "Untitled Session"
        );
        let entries = build_entries(&snapshot, "untitled", &[], "", "Untitled Session");
        assert!(rows(&entries, Group::Sessions).is_empty());
    }

    #[test]
    fn files_show_only_for_the_query_they_were_searched_for() {
        let files = [
            FileEntry {
                path: "/work/openchamber/packages/ui/src/stores/index.ts".to_owned(),
                relative_path: "packages/ui/src/stores/index.ts".to_owned(),
            },
            FileEntry {
                path: "/work/openchamber/src/useUIStore.ts".to_owned(),
                relative_path: "src/useUIStore.ts".to_owned(),
            },
        ];
        let entries = build_entries(&snapshot(), "uistore", &files, "uistore", "Untitled");
        let found = rows(&entries, Group::Files);
        assert_eq!(
            found[0].title.as_ref(),
            "useUIStore.ts",
            "a name match ranks first"
        );
        assert_eq!(found[0].marks, vec![3..10]);
        assert_eq!(
            found[0].target,
            Target::File("/work/openchamber/src/useUIStore.ts".to_owned())
        );
        let stale = build_entries(&snapshot(), "uistor", &files, "uistore", "Untitled");
        assert!(rows(&stale, Group::Files).is_empty());
    }

    #[test]
    fn projects_mark_the_name_and_the_path_apart() {
        let entries = build_entries(&snapshot(), "docs", &[], "", "Untitled");
        let projects = rows(&entries, Group::Projects);
        assert_eq!(projects[0].title.as_ref(), "Docs");
        assert_eq!(projects[0].marks, vec![0..4]);
        assert_eq!(
            projects[0].detail,
            Detail::Path {
                text: "/work/site".into(),
                marks: Vec::new()
            }
        );
        // Without a label the name is the path's last segment, and both
        // show the match.
        let entries = build_entries(&snapshot(), "openchamber", &[], "", "Untitled");
        let projects = rows(&entries, Group::Projects);
        assert_eq!(projects[0].title.as_ref(), "openchamber");
        assert_eq!(projects[0].marks, vec![0..11]);
        assert_eq!(
            projects[0].detail,
            Detail::Path {
                text: "/work/openchamber".into(),
                marks: std::iter::once(6..17).collect(),
            }
        );
    }

    #[test]
    fn stepping_wraps_and_skips_headings() {
        let entries = build_entries(&snapshot(), "", &[], "", "Untitled");
        // Heading, open file, heading, five sessions, heading, three commands.
        assert_eq!(entries.len(), 12);
        assert_eq!(step_item(&entries, None, 1), Some(1));
        assert_eq!(step_item(&entries, Some(1), 1), Some(3));
        assert_eq!(step_item(&entries, Some(1), -1), Some(11), "up wraps");
        assert_eq!(step_item(&entries, Some(11), 1), Some(1), "down wraps");
        assert_eq!(step_item(&entries, None, -1), Some(11));
        assert_eq!(step_item(&[], None, 1), None);
    }

    #[test]
    fn group_steps_land_on_a_groups_first_row() {
        let entries = build_entries(&snapshot(), "", &[], "", "Untitled");
        assert_eq!(step_group(&entries, Some(1), true), Some(3));
        assert_eq!(step_group(&entries, Some(5), true), Some(9));
        assert_eq!(step_group(&entries, Some(5), false), Some(1));
        // No group follows the last or precedes the first: one row instead.
        assert_eq!(step_group(&entries, Some(10), true), Some(11));
        assert_eq!(step_group(&entries, Some(1), false), Some(11));
    }
}
