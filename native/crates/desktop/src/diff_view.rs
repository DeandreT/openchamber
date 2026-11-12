//! Patches in Kit's read-only diff (`PierreDiffViewer.tsx`): a pane for one
//! path that the Git surface shows under its changes, and the Diff surface
//! (`DiffView.tsx`) that lists every changed file of the Changed or Staged
//! scope in a virtual list, each a row that opens its own patch.
//!
//! The Diff surface reads when it is created, when the panel shows it again,
//! when a Git refresh hint names its directory (`DiffView::refresh`, called
//! by the workspace) and on Retry. Rows shown stay until the answer. A patch
//! is parsed when its open row is first drawn and kept while its text is
//! unchanged.
//!
//! Patches come from the server (`/api/git/diff` and `/api/git/diffs`) and
//! are parsed by Kit. A path the server does not diff (a nested repository,
//! a folder of new files) shows why. A failed read, a patch that does not
//! parse and a path the server left out of its answer each show the failure
//! and never an empty diff; a failed read offers Retry.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::mem;
use std::rc::Rc;

use gpui_kit::component::ActiveTheme;
use gpui_kit::component::diff::{Diff, DiffFile, DiffMode, DiffState};
use gpui_kit::{
    AnyElement, AppContext, Context, Entity, FocusHandle, FontWeight, Hsla, InteractiveElement,
    IntoElement, KeyDownEvent, ListAlignment, ListState, ParentElement, Pixels, Render, Role,
    SharedString, StatefulInteractiveElement, Styled, Task, TestSupportExt, Window, canvas, div,
    prelude::FluentBuilder, px,
};
use openchamber_native_core::git_changes::{ChangeRow, ChangeScope, ChangesRead, StatusLoads};
use openchamber_native_transport::git::{DIFFS_MAX_PATHS, PathDiff};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{ButtonTone, CONTROL_RADIUS, IconButton, glyph};
use crate::elevation;
use crate::files_view::{Notice, PanelScope, empty_state, status_line, text_button};
use crate::git_view::{FILE_ICON, change_descriptor, changed_path_label};
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::panel_controls::{LABEL_LINE, LABEL_SIZE, META_LINE, META_SIZE, MICRO_LINE, MICRO_SIZE};
use crate::theme::Palette;

mod walkthrough;

/// What a diff shows.
enum DiffLoad {
    Idle,
    Loading,
    Shown,
    /// Nothing changed in the scope.
    Empty,
    Unavailable(PathDiff),
    Failed(Option<String>),
}

/// One path's patch, owned by the view that shows it.
pub(crate) struct DiffPane {
    state: Entity<DiffState>,
    shown: Option<(String, ChangeScope)>,
    load: DiffLoad,
    task: Option<Task<()>>,
}

impl DiffPane {
    pub(crate) fn new(cx: &mut gpui_kit::App) -> Self {
        Self {
            state: cx.new(|cx| DiffState::new(Vec::new(), cx)),
            shown: None,
            load: DiffLoad::Idle,
            task: None,
        }
    }

    pub(crate) fn clear(&mut self, cx: &mut gpui_kit::App) {
        self.shown = None;
        self.task = None;
        self.load = DiffLoad::Idle;
        self.state
            .update(cx, |state, cx| state.set_files(Vec::new(), cx));
    }

    /// Reads and shows the patch of `path` in `scope`. `pane` finds this
    /// pane in the view again when the read finishes.
    pub(crate) fn show<V: 'static>(
        &mut self,
        scope: &PanelScope,
        path: &str,
        change_scope: ChangeScope,
        pane: fn(&mut V) -> &mut DiffPane,
        cx: &mut Context<V>,
    ) {
        self.shown = Some((path.to_owned(), change_scope));
        self.load = DiffLoad::Loading;
        let mut scope = scope.clone();
        let Some(client) = scope.client(cx) else {
            self.load = DiffLoad::Failed(None);
            return;
        };
        let directory = scope.directory.clone();
        let read_path = path.to_owned();
        let task = scope.run(async move {
            client
                .git_diff(&directory, &read_path, change_scope == ChangeScope::Staged)
                .await
        });
        let expected = (path.to_owned(), change_scope);
        self.task = Some(cx.spawn(async move |this, cx| {
            let Some(result) = task.await else {
                return;
            };
            let _ = this.update(cx, |view, cx| {
                let pane = pane(view);
                if pane.shown.as_ref() != Some(&expected) {
                    return;
                }
                pane.task = None;
                pane.apply(
                    result
                        .map(|diff| vec![diff])
                        .map_err(|error| error.message().map(str::to_owned)),
                    cx,
                );
                cx.notify();
            });
        }));
    }

    /// Installs patches, or the reason there is none.
    fn apply(&mut self, result: Result<Vec<PathDiff>, Option<String>>, cx: &mut gpui_kit::App) {
        let diffs = match result {
            Ok(diffs) => diffs,
            Err(message) => {
                self.load = DiffLoad::Failed(message);
                return;
            }
        };
        let mut files = Vec::new();
        let mut unavailable = None;
        for diff in diffs {
            match diff {
                PathDiff::Patch(patch) if !patch.trim().is_empty() => match DiffFile::parse(&patch)
                {
                    Ok(parsed) => files.extend(parsed),
                    Err(error) => {
                        self.load = DiffLoad::Failed(Some(error.message().to_string()));
                        return;
                    }
                },
                PathDiff::Patch(_) => {}
                other => unavailable = Some(other),
            }
        }
        self.load = match (files.is_empty(), unavailable) {
            (true, Some(reason)) => DiffLoad::Unavailable(reason),
            (true, None) => DiffLoad::Empty,
            (false, _) => DiffLoad::Shown,
        };
        self.state
            .update(cx, |state, cx| state.set_files(files, cx));
    }

    pub(crate) fn render<V>(&self, wrap: bool, palette: &Palette, cx: &Context<V>) -> AnyElement {
        let strings = locale::strings(cx);
        let text = match &self.load {
            DiffLoad::Idle => return div().into_any_element(),
            DiffLoad::Loading => strings.get("diffView.state.loadingDiff"),
            DiffLoad::Empty => strings.get("diffView.state.cleanWorkingTree"),
            DiffLoad::Unavailable(PathDiff::NestedRepository) => {
                strings.get("diffView.nestedRepository.loading")
            }
            DiffLoad::Unavailable(PathDiff::UntrackedDirectory) => {
                strings.get("diffView.unavailable.untrackedDirectoryDescription")
            }
            DiffLoad::Unavailable(_) => strings.get("diffView.state.cleanWorkingTree"),
            DiffLoad::Failed(message) => {
                let title = strings.get("diffView.state.failedToLoadDiff");
                match message {
                    Some(message) => format!("{title}: {message}").into(),
                    None => title,
                }
            }
            DiffLoad::Shown => {
                return div()
                    .id("diff-content")
                    .test_support()
                    .role(Role::Group)
                    .accessibility_id("diff-content")
                    .aria_label(strings.get("contextPanel.mode.diff"))
                    .size_full()
                    // Code reads left to right in every interface direction.
                    .child(
                        Diff::new(&self.state)
                            .line_number(true)
                            .soft_wrap(wrap)
                            .size_full(),
                    )
                    .test_support()
                    .into_any_element();
            }
        };
        div()
            .id("diff-message")
            .role(Role::Status)
            .accessibility_id("diff-message")
            .aria_label(text.clone())
            .size_full()
            .child(empty_state(text, palette, cx))
            .into_any_element()
    }
}

/// The panel header's row with its bottom line (`h-10 border-b`).
const TOOLBAR_ROW: f32 = 41.0;
/// A file row: 24 px of controls in `py-2`.
const FILE_ROW: f32 = 40.0;
/// The tallest an expanded file's patch stands before it scrolls.
const EXPANDED_MAX: f32 = 480.0;
/// How far past the viewport the list draws rows.
const LIST_OVERDRAW: f32 = 400.0;
/// The scope menu (`w-40`).
const SCOPE_MENU_WIDTH: f32 = 160.0;
/// Kit's gutter beside wrapped code, in characters, for the height estimate.
const GUTTER_COLUMNS: f32 = 12.0;

/// What the server gave for one path.
enum FilePatch {
    /// The patch text; parsed when its row is first drawn open.
    Text(String),
    /// The patch could not be read or parsed. Never shown as "no changes".
    Failed(Option<String>),
    Unavailable(PathDiff),
}

impl FilePatch {
    fn same_text(&self, other: &Self) -> bool {
        matches!((self, other), (Self::Text(mine), Self::Text(theirs)) if mine == theirs)
    }
}

/// What an open patch needs to size itself: the length of each row it shows
/// in unified mode (hunk headers and body lines).
struct PatchMetrics {
    lengths: Vec<u16>,
}

impl PatchMetrics {
    fn read(patch: &str) -> Self {
        let lengths = patch
            .lines()
            .skip_while(|line| !line.starts_with("@@"))
            .filter(|line| !line.starts_with('\\'))
            .map(|line| u16::try_from(line.chars().count()).unwrap_or(u16::MAX))
            .collect();
        Self { lengths }
    }

    /// The rows shown when a row holds `columns` characters; none wraps
    /// without a limit.
    fn rows(&self, columns: Option<usize>) -> usize {
        match columns {
            None => self.lengths.len(),
            Some(columns) => self
                .lengths
                .iter()
                .map(|length| usize::from(*length).div_ceil(columns.max(1)).max(1))
                .sum(),
        }
    }
}

/// One changed path of the scope with what the server gave for it.
struct FileEntry {
    row: ChangeRow,
    patch: FilePatch,
    /// Kit's state of the parsed patch: made when the row is first drawn
    /// open, kept across reads while the patch text stays the same.
    state: Option<Entity<DiffState>>,
    metrics: Option<PatchMetrics>,
}

/// Where the read of the scope's files stands.
enum FilesLoad {
    Loading,
    Ready,
    Failed(Option<String>),
}

/// The Diff surface: every changed file of one scope, each a row that opens
/// its patch (`DiffView.tsx`'s stacked view, collapsed until opened).
pub(crate) struct DiffView {
    scope: PanelScope,
    change_scope: ChangeScope,
    scope_open: bool,
    focus: FocusHandle,
    status: StatusLoads,
    files: Vec<FileEntry>,
    expanded: HashSet<String>,
    list: ListState,
    /// The list's painted width, for the wrapped height estimate.
    content_width: Rc<Cell<f32>>,
    load: FilesLoad,
    /// The newest read failed and the rows shown are an earlier read's.
    stale: bool,
    wrap: bool,
    split: bool,
    notice: Option<Notice>,
    task: Option<Task<()>>,
    walkthrough: walkthrough::WalkthroughPanel,
}

impl DiffView {
    pub(crate) fn new(scope: PanelScope, cx: &mut Context<Self>) -> Self {
        let view = Self {
            scope,
            change_scope: ChangeScope::Unstaged,
            scope_open: false,
            focus: cx.focus_handle(),
            status: StatusLoads::default(),
            files: Vec::new(),
            expanded: HashSet::new(),
            list: ListState::new(0, ListAlignment::Top, px(LIST_OVERDRAW)),
            content_width: Rc::new(Cell::new(0.0)),
            load: FilesLoad::Loading,
            stale: false,
            wrap: false,
            split: false,
            notice: None,
            task: None,
            walkthrough: walkthrough::WalkthroughPanel::new(cx),
        };
        let entity = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = entity.update(cx, |view, cx| view.refresh(cx));
        });
        view
    }

    #[cfg(test)]
    pub(crate) fn scope_shown(&self) -> ChangeScope {
        self.change_scope
    }

    #[cfg(test)]
    pub(crate) fn listed_paths(&self) -> Vec<String> {
        self.files
            .iter()
            .map(|file| file.row.path.clone())
            .collect()
    }

    /// The failure `path` shows: its detail, empty when the server gave none.
    #[cfg(test)]
    pub(crate) fn failure_of(&self, path: &str) -> Option<String> {
        self.files
            .iter()
            .find(|file| file.row.path == path)
            .and_then(|file| match &file.patch {
                FilePatch::Failed(detail) => Some(detail.clone().unwrap_or_default()),
                _ => None,
            })
    }

    /// Whether Kit holds a parsed state for `path`.
    #[cfg(test)]
    pub(crate) fn has_state(&self, path: &str) -> bool {
        self.files
            .iter()
            .any(|file| file.row.path == path && file.state.is_some())
    }

    /// Reads the status, then the patches of every path in the scope. Rows
    /// already shown stay until the answer; only the newest read of the
    /// scope it was started for applies.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.walkthrough.open {
            self.load_walkthrough(cx);
            return;
        }
        let ticket = self.status.begin();
        let Some(client) = self.scope.client(cx) else {
            self.status.complete(ticket, Err(()));
            self.fail_read(None);
            cx.notify();
            return;
        };
        let directory = self.scope.directory.clone();
        let requested = self.change_scope;
        let staged = requested == ChangeScope::Staged;
        if self.files.is_empty() {
            self.load = FilesLoad::Loading;
        }
        let task = self.scope.run(async move {
            let status = client.git_changes(&directory).await?;
            let rows: Vec<ChangeRow> = match &status {
                ChangesRead::Repository(changes) if staged => changes.staged(),
                ChangesRead::Repository(changes) => changes.unstaged(),
                ChangesRead::NotRepository => Vec::new(),
            };
            let paths: Vec<String> = rows.iter().map(|row| row.path.clone()).collect();
            let mut diffs = Vec::new();
            for chunk in paths.chunks(DIFFS_MAX_PATHS) {
                diffs.extend(client.git_diffs(&directory, chunk, staged).await?);
            }
            Ok::<_, openchamber_native_transport::git::GitError>((status, rows, diffs))
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let Some(result) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                // A read started for another scope never fills this one.
                if this.change_scope != requested {
                    return;
                }
                this.task = None;
                match result {
                    Ok((status, rows, diffs)) => {
                        if !this.status.complete(ticket, Ok(status)) {
                            return;
                        }
                        this.install(rows, diffs);
                        this.stale = false;
                        this.notice = None;
                    }
                    Err(error) => {
                        if !this.status.complete(ticket, Err(())) {
                            return;
                        }
                        this.fail_read(error.message().map(str::to_owned));
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// A failed read: rows of an earlier read stay, marked stale with the
    /// failure in the status line; with none to keep, the failure takes the
    /// surface. Either way Retry reads again.
    fn fail_read(&mut self, detail: Option<String>) {
        if self.files.is_empty() {
            self.load = FilesLoad::Failed(detail);
            return;
        }
        self.stale = true;
    }

    /// Replaces the listed files. A file whose patch text is unchanged keeps
    /// its parsed state, so its scroll position and selection survive.
    fn install(&mut self, rows: Vec<ChangeRow>, diffs: Vec<(String, PathDiff)>) {
        let mut previous: HashMap<String, FileEntry> = mem::take(&mut self.files)
            .into_iter()
            .map(|file| (file.row.path.clone(), file))
            .collect();
        let mut by_path: HashMap<String, PathDiff> = diffs.into_iter().collect();
        let mut files = Vec::with_capacity(rows.len());
        for row in rows {
            let patch = match by_path.remove(&row.path) {
                Some(PathDiff::Patch(text)) if !text.trim().is_empty() => FilePatch::Text(text),
                Some(PathDiff::Patch(_)) => FilePatch::Unavailable(PathDiff::NotFound),
                Some(other) => FilePatch::Unavailable(other),
                // The server answered without this path.
                None => FilePatch::Failed(None),
            };
            let kept = previous
                .remove(&row.path)
                .filter(|old| old.patch.same_text(&patch));
            files.push(match kept {
                Some(old) => FileEntry { row, ..old },
                None => FileEntry {
                    row,
                    patch,
                    state: None,
                    metrics: None,
                },
            });
        }
        self.files = files;
        let files = &self.files;
        self.expanded
            .retain(|path| files.iter().any(|file| &file.row.path == path));
        if self.list.item_count() == self.files.len() {
            self.list.remeasure();
        } else {
            self.list.reset(self.files.len());
        }
        self.load = FilesLoad::Ready;
    }

    /// Parses the patch of an open row the first time it is drawn, so
    /// opening every file starts work only for the rows on screen.
    fn ensure_state(&mut self, index: usize, cx: &mut Context<Self>) {
        let mode = self.mode();
        let Some(file) = self.files.get_mut(index) else {
            return;
        };
        if file.state.is_some() {
            return;
        }
        let FilePatch::Text(text) = &file.patch else {
            return;
        };
        match DiffFile::parse(text) {
            Ok(parsed) => {
                file.metrics = Some(PatchMetrics::read(text));
                file.state = Some(cx.new(|cx| DiffState::new(parsed, cx).with_mode(mode)));
            }
            Err(error) => {
                file.patch = FilePatch::Failed(Some(error.message().to_string()));
            }
        }
    }

    fn mode(&self) -> DiffMode {
        if self.split {
            DiffMode::Split
        } else {
            DiffMode::Unified
        }
    }

    fn set_split(&mut self, split: bool, cx: &mut Context<Self>) {
        self.split = split;
        let mode = self.mode();
        for file in &self.files {
            if let Some(state) = &file.state {
                state.update(cx, |state, cx| state.set_mode(mode, cx));
            }
        }
        cx.notify();
    }

    fn set_wrap(&mut self, wrap: bool, cx: &mut Context<Self>) {
        self.wrap = wrap;
        // Wrapped rows change every open patch's height.
        self.list.remeasure();
        cx.notify();
    }

    pub(crate) fn set_scope(&mut self, scope: ChangeScope, cx: &mut Context<Self>) {
        self.scope_open = false;
        if self.change_scope != scope {
            self.change_scope = scope;
            // The read in flight was for the other scope.
            self.task = None;
            self.files.clear();
            self.expanded.clear();
            self.list.reset(0);
            self.stale = false;
            self.notice = None;
            self.load = FilesLoad::Loading;
            let open = self.walkthrough.open;
            self.walkthrough = walkthrough::WalkthroughPanel::new(cx);
            self.walkthrough.open = open;
            self.refresh(cx);
        }
        cx.notify();
    }

    fn set_scope_open(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.scope_open = open;
        if open {
            // Escape reaches the panel while its menu is open.
            window.focus(&self.focus, cx);
        }
        cx.notify();
    }

    fn panel_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.scope_open && event.keystroke.key == "escape" {
            self.scope_open = false;
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn toggle_file(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_owned());
        }
        if let Some(index) = self.files.iter().position(|file| file.row.path == path) {
            self.list.remeasure_items(index..index + 1);
        }
        cx.notify();
    }

    /// Collapses every file when any is open, else opens them all. Only the
    /// open set changes here; a patch is parsed when its row is drawn.
    fn toggle_all(&mut self, cx: &mut Context<Self>) {
        if self.expanded.is_empty() {
            self.expanded = self
                .files
                .iter()
                .map(|file| file.row.path.clone())
                .collect();
        } else {
            self.expanded.clear();
        }
        self.list.remeasure();
        cx.notify();
    }

    /// `ChangeScopeSelector`'s trigger: the scope, then its file count.
    fn render_scope_trigger(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let name = match self.change_scope {
            ChangeScope::Unstaged => strings.get("diffView.scope.changed"),
            ChangeScope::Staged => strings.get("diffView.scope.staged"),
        };
        let label: SharedString = match self.load {
            FilesLoad::Ready => format!("{name}: {}", self.files.len()).into(),
            _ => name,
        };
        let hover = palette.hover;
        let open = self.scope_open;
        on_activate(
            div()
                .id("diff-scope")
                .role(Role::Button)
                .accessibility_id("diff-scope")
                .aria_label(SharedString::from(format!(
                    "{}: {label}",
                    strings.get("diffView.scope.selectorAria")
                )))
                .aria_expanded(open)
                .h(px(28.0))
                .px(px(8.0))
                .flex_shrink_0()
                .rounded(px(CONTROL_RADIUS))
                .flex_row_from(direction)
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .child(
                    div()
                        .css_text(LABEL_SIZE, LABEL_LINE, cx)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(palette.foreground)
                        .child(label),
                )
                .child(glyph("arrow-down-s", 16.0, palette.muted_foreground)),
            cx,
            move |this: &mut Self, window, cx| this.set_scope_open(!open, window, cx),
        )
        .test_support()
        .into_any_element()
    }

    /// The scope menu over the panel (`DropdownMenuContent align="start"
    /// className="w-40"`), with the layer that closes it on an outside click.
    fn render_scopes(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<[AnyElement; 2]> {
        if !self.scope_open {
            return None;
        }
        let strings = locale::strings(cx);
        let counts = match self.status.current() {
            Some(ChangesRead::Repository(changes)) => {
                Some((changes.unstaged().len(), changes.staged().len()))
            }
            _ => None,
        };
        let hover = palette.hover;
        let option = |id: &'static str, scope: ChangeScope, label: SharedString| {
            let checked = self.change_scope == scope;
            let count = counts.map(|(changed, staged)| match scope {
                ChangeScope::Unstaged => changed,
                ChangeScope::Staged => staged,
            });
            on_activate(
                div()
                    .id(id)
                    .role(Role::MenuItemRadio)
                    .accessibility_id(id)
                    .aria_label(label.clone())
                    .aria_selected(checked)
                    .h(px(28.0))
                    .px(px(8.0))
                    .rounded(px(6.0))
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(12.0))
                    .cursor_pointer()
                    .when(checked, |this| this.bg(palette.selection))
                    .when(!checked, |this| this.hover(move |style| style.bg(hover)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .css_text(LABEL_SIZE, LABEL_LINE, cx)
                            .text_color(palette.foreground)
                            .child(label),
                    )
                    .children(count.map(|count| {
                        div()
                            .flex_shrink_0()
                            .css_text(META_SIZE, META_LINE, cx)
                            .text_color(palette.muted_foreground)
                            .child(SharedString::from(count.to_string()))
                    })),
                cx,
                move |this: &mut Self, _, cx| this.set_scope(scope, cx),
            )
            .test_support()
        };
        let dismiss = on_activate(
            div().id("diff-scope-dismiss").absolute().inset_0(),
            cx,
            |this: &mut Self, _, cx| {
                this.scope_open = false;
                cx.notify();
            },
        );
        let menu = div()
            .id("diff-scopes")
            .role(Role::Menu)
            .accessibility_id("diff-scopes")
            .aria_label(strings.get("diffView.scope.selectorAria"))
            .absolute()
            .top(px(38.0))
            .inset_start(direction, px(6.0))
            .w(px(SCOPE_MENU_WIDTH))
            .p(px(4.0))
            .rounded(px(10.0))
            .bg(palette.elevated)
            .shadow(elevation::floating(palette))
            .occlude()
            .flex()
            .flex_col()
            .child(option(
                "diff-scope-changed",
                ChangeScope::Unstaged,
                strings.get("diffView.scope.changed"),
            ))
            .child(option(
                "diff-scope-staged",
                ChangeScope::Staged,
                strings.get("diffView.scope.staged"),
            ));
        Some([dismiss.into_any_element(), menu.into_any_element()])
    }

    fn render_toolbar(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let expand_label = if self.expanded.is_empty() {
            strings.get("diffView.actions.expandAll")
        } else {
            strings.get("diffView.actions.collapseAll")
        };
        let foreground = palette.foreground;
        // `Button variant="ghost" size="sm"` with `gap-1`, muted until hovered.
        let expand = (!self.files.is_empty() && !self.walkthrough.open).then(|| {
            on_activate(
                div()
                    .id("diff-expand-all")
                    .role(Role::Button)
                    .accessibility_id("diff-expand-all")
                    .aria_label(expand_label.clone())
                    .h(px(28.0))
                    .px(px(8.0))
                    .flex_shrink_0()
                    .rounded(px(CONTROL_RADIUS))
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(4.0))
                    .cursor_pointer()
                    .text_color(palette.muted_foreground)
                    .hover(move |style| style.text_color(foreground))
                    .child(glyph("expand-up-down", 16.0, palette.muted_foreground))
                    .child(
                        div()
                            .css_text(LABEL_SIZE, LABEL_LINE, cx)
                            .child(expand_label),
                    ),
                cx,
                |this: &mut Self, _, cx| this.toggle_all(cx),
            )
            .test_support()
        });
        let walkthrough_label = strings.get("walkthrough.action.open");
        // The outline button in the info tint (`WALKTHROUGH_ACTION_CLASS`),
        // down to its glyph in a panel this narrow.
        let walkthrough = on_activate(
            div()
                .id("diff-walkthrough")
                .role(Role::Button)
                .accessibility_id("diff-walkthrough")
                .aria_label(walkthrough_label)
                .aria_toggled(if self.walkthrough.open {
                    gpui_kit::Toggled::True
                } else {
                    gpui_kit::Toggled::False
                })
                .size(px(28.0))
                .flex_shrink_0()
                .rounded(px(CONTROL_RADIUS))
                .bg(palette.status_info.opacity(0.12))
                .border_1()
                .border_color(palette.status_info.opacity(0.3))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .child(glyph("route", 16.0, palette.status_info)),
            cx,
            |this: &mut Self, _, cx| this.toggle_walkthrough(cx),
        )
        .test_support();
        let tone = |on: bool| {
            if on {
                ButtonTone::Active
            } else {
                ButtonTone::Quiet
            }
        };
        let (wrap, split) = (self.wrap, self.split);
        div()
            .id("diff-toolbar")
            .role(Role::Group)
            .aria_label(strings.get("diffView.scope.selectorAria"))
            .h(px(TOOLBAR_ROW))
            .flex_shrink_0()
            .px(px(6.0))
            .border_b_1()
            .border_color(palette.divider)
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.0))
            .child(self.render_scope_trigger(palette, direction, cx))
            .child(div().flex_1())
            .children(expand)
            .child(walkthrough)
            .child(
                IconButton::new(
                    "diff-wrap",
                    "text-wrap",
                    strings.get("diffView.actions.wrapLines"),
                )
                .size(24.0, 14.0)
                .tone(tone(wrap))
                .render(palette, cx, move |this: &mut Self, _, cx| {
                    this.set_wrap(!wrap, cx)
                }),
            )
            .child(
                IconButton::new(
                    "diff-split",
                    "layout-column",
                    strings.get("diffView.layout.sideBySide"),
                )
                .size(24.0, 14.0)
                .tone(tone(split))
                .render(palette, cx, move |this: &mut Self, _, cx| {
                    this.set_split(!split, cx)
                }),
            )
            .test_support()
            .into_any_element()
    }

    /// How tall an open patch stands: Kit's notice for a patch without
    /// source rows (binary, mode-only), else its rows, counted wrapped when
    /// wrapping is on. Kit scrolls inside when the estimate falls short.
    fn patch_height(
        &self,
        state: &Entity<DiffState>,
        metrics: Option<&PatchMetrics>,
        rem: Pixels,
        cx: &gpui_kit::App,
    ) -> Pixels {
        let font_size = rem * (f32::from(cx.theme().mono_font_size) / 16.0);
        let rows = metrics.map_or(0, |metrics| {
            let columns = self.wrap.then(|| {
                let characters = self.content_width.get() / (f32::from(font_size) * 0.6);
                (characters - GUTTER_COLUMNS).max(20.0) as usize
            });
            metrics.rows(columns)
        });
        let files = state.read(cx).files();
        if rows == 0 || files.iter().any(DiffFile::is_binary) {
            // `render_notice`: `py_2`, one `text_xs` line and `gap_1` per
            // extended header, then one `text_sm` line.
            let headers: usize = files.iter().map(|file| file.extended_headers().len()).sum();
            return rem * (1.0 + 1.25 + 1.25 * headers as f32);
        }
        (font_size * 1.6 * rows as f32).min(px(EXPANDED_MAX))
    }

    /// One row of the virtual list: parses an open file's patch on first
    /// draw, then draws the file.
    fn render_file_row(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(path) = self.files.get(index).map(|file| file.row.path.clone()) else {
            return div().into_any_element();
        };
        if self.expanded.contains(&path) {
            self.ensure_state(index, cx);
        }
        let palette = cx.global::<Palette>().clone();
        let direction = locale::strings(cx).direction();
        let last = index + 1 == self.files.len();
        self.render_file(
            &self.files[index],
            last,
            window.rem_size(),
            &palette,
            direction,
            cx,
        )
    }

    /// One file of the stacked view: its header row, then its patch while open.
    fn render_file(
        &self,
        file: &FileEntry,
        last: bool,
        rem: Pixels,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let path = file.row.path.clone();
        let expanded = self.expanded.contains(&path);
        let (code, color) = change_descriptor(file.row.kind, palette);
        let id = SharedString::from(format!("diff-file:{path}"));
        let hover = palette.hover.opacity(0.5);
        let totals = file
            .row
            .stat
            .filter(|stat| stat.insertions > 0 || stat.deletions > 0)
            .map(|stat| {
                let part = |text: String, color: Hsla| {
                    div()
                        .css_text(MICRO_SIZE, MICRO_LINE, cx)
                        .text_color(color)
                        .child(SharedString::from(text))
                };
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .when(stat.insertions > 0, |this| {
                        this.child(part(
                            format!("+{}", stat.insertions),
                            palette.status_success,
                        ))
                    })
                    .when(stat.deletions > 0, |this| {
                        this.child(part(format!("-{}", stat.deletions), palette.status_error))
                    })
            });
        let toggle_path = path.clone();
        let header = on_activate(
            div()
                .id(gpui_kit::ElementId::Name(id.clone()))
                .role(Role::Button)
                .accessibility_id(id)
                .aria_label(SharedString::from(path.clone()))
                .aria_expanded(expanded)
                .h(px(FILE_ROW))
                .px(px(12.0))
                .bg(palette.elevated.opacity(0.9))
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .child(
                    div()
                        .size(px(20.0))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(glyph(
                            match (expanded, direction) {
                                (true, _) => "arrow-down-s",
                                (false, Direction::LeftToRight) => "arrow-right-s",
                                (false, Direction::RightToLeft) => "arrow-left-s",
                            },
                            16.0,
                            palette.muted_foreground.opacity(0.7),
                        )),
                )
                .child(
                    div()
                        .w(px(16.0))
                        .flex_shrink_0()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .css_text(MICRO_SIZE, MICRO_LINE, cx)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(color)
                                .child(code),
                        ),
                )
                .child(glyph(FILE_ICON, 14.0, palette.status_info))
                .child(changed_path_label(&path, palette, cx))
                .children(totals),
            cx,
            move |this: &mut Self, _, cx| this.toggle_file(&toggle_path, cx),
        )
        .test_support();
        let message = |text: SharedString, failed: bool| {
            div()
                .id(gpui_kit::ElementId::Name(
                    format!("diff-file-message:{path}").into(),
                ))
                .role(Role::Status)
                .accessibility_id(SharedString::from(format!("diff-file-message:{path}")))
                .aria_label(text.clone())
                .px(px(12.0))
                .py(px(12.0))
                .bg(palette.background)
                .child(
                    div()
                        .css_text(META_SIZE, META_LINE, cx)
                        .text_color(if failed {
                            palette.status_error
                        } else {
                            palette.muted_foreground
                        })
                        .child(text),
                )
                .test_support()
                .into_any_element()
        };
        let body = expanded.then(|| match (&file.patch, &file.state) {
            (FilePatch::Text(_), Some(state)) => div()
                .id(gpui_kit::ElementId::Name(
                    format!("diff-file-patch:{path}").into(),
                ))
                .h(self.patch_height(state, file.metrics.as_ref(), rem, cx))
                .bg(palette.background)
                // Code reads left to right in every interface direction.
                .child(
                    Diff::new(state)
                        .header_visible(false)
                        .line_number(true)
                        .soft_wrap(self.wrap)
                        .size_full(),
                )
                .test_support()
                .into_any_element(),
            // Parsed when the list draws the row; one frame at most.
            (FilePatch::Text(_), None) => message(strings.get("diffView.state.loadingDiff"), false),
            (FilePatch::Failed(detail), _) => {
                let title = strings.get("diffView.state.failedToLoadDiff");
                message(
                    match detail {
                        Some(detail) => format!("{title}: {detail}").into(),
                        None => title,
                    },
                    true,
                )
            }
            (FilePatch::Unavailable(PathDiff::NestedRepository), _) => {
                message(strings.get("diffView.nestedRepository.loading"), false)
            }
            (FilePatch::Unavailable(PathDiff::UntrackedDirectory), _) => message(
                strings.get("diffView.unavailable.untrackedDirectoryDescription"),
                false,
            ),
            // The path is no longer changed.
            (FilePatch::Unavailable(_), _) => {
                message(strings.get("diffView.state.cleanWorkingTree"), false)
            }
        });
        // The header's own line, then the file's, which the last file drops.
        // The list lays the row out as a root, so it names its own width.
        div()
            .w_full()
            .flex()
            .flex_col()
            .child(header)
            .child(div().h(px(1.0)).bg(palette.border.opacity(0.35)))
            .children(body)
            .when(!last, |this| {
                this.child(div().h(px(1.0)).bg(palette.border.opacity(0.4)))
            })
            .into_any_element()
    }

    fn render_retry(&self, palette: &Palette, cx: &Context<Self>) -> AnyElement {
        text_button(
            "diff-retry",
            locale::strings(cx).get("diffView.actions.retry"),
            None,
            palette,
            cx,
            |this: &mut Self, _, cx| this.refresh(cx),
        )
    }

    fn render_files(&self, palette: &Palette, cx: &Context<Self>) -> AnyElement {
        let strings = locale::strings(cx);
        let message = |text: SharedString, retry: Option<AnyElement>| {
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .child(
                    div()
                        .id("diff-message")
                        .role(Role::Status)
                        .accessibility_id("diff-message")
                        .aria_label(text.clone())
                        .test_support()
                        .css_text(LABEL_SIZE, LABEL_LINE, cx)
                        .text_color(palette.muted_foreground)
                        .child(text),
                )
                .children(retry)
                .into_any_element()
        };
        match &self.load {
            FilesLoad::Loading => {
                return message(strings.get("diffView.state.loadingDiff"), None);
            }
            FilesLoad::Failed(detail) => {
                let title = strings.get("diffView.state.failedToLoadDiff");
                return message(
                    match detail {
                        Some(detail) => format!("{title}: {detail}").into(),
                        None => title,
                    },
                    Some(self.render_retry(palette, cx)),
                );
            }
            FilesLoad::Ready if self.files.is_empty() => {
                return message(strings.get("diffView.state.cleanWorkingTree"), None);
            }
            FilesLoad::Ready => {}
        }
        // Wrapped heights follow the list's width; a new width measures again.
        let (width, list) = (self.content_width.clone(), self.list.clone());
        let view = cx.entity().downgrade();
        let measure = canvas(
            move |bounds, _, cx| {
                let next = f32::from(bounds.size.width);
                if width.replace(next) != next {
                    let (list, view) = (list.clone(), view.clone());
                    // Applied once this draw ends; a refresh asked for
                    // while drawing is dropped.
                    cx.defer(move |cx| {
                        list.remeasure();
                        let _ = view.update(cx, |_, cx| cx.notify());
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();
        let rows = gpui_kit::list(
            self.list.clone(),
            cx.processor(|this, index: usize, window, cx| this.render_file_row(index, window, cx)),
        )
        .size_full();
        div()
            .id("diff-content")
            .role(Role::List)
            .accessibility_id("diff-content")
            .aria_label(strings.get("contextPanel.mode.diff"))
            .relative()
            .size_full()
            .child(measure)
            .child(rows)
            .test_support()
            .into_any_element()
    }
}

impl Render for DiffView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = cx.global::<Palette>().clone();
        let strings = locale::strings(cx);
        let direction: Direction = strings.direction();
        let body = if self.walkthrough.open {
            self.render_walkthrough(&palette, direction, cx)
        } else {
            match self.status.current() {
                Some(ChangesRead::NotRepository) => {
                    empty_state(strings.get("diffView.state.notGitRepository"), &palette, cx)
                }
                None if self.status.failed() => self.render_files(&palette, cx),
                None => empty_state(
                    strings.get("diffView.state.loadingRepositoryStatus"),
                    &palette,
                    cx,
                ),
                Some(ChangesRead::Repository(_)) => self.render_files(&palette, cx),
            }
        };
        // Rows of an earlier read stay under a failed one; the line says so
        // and offers the read again.
        let stale = self.stale.then(|| {
            let text = strings.get("diffView.state.failedToLoadDiff");
            div()
                .flex_shrink_0()
                .padding_end(direction, px(8.0))
                .flex_row_from(direction)
                .items_center()
                .child(div().flex_1().min_w_0().child(status_line(
                    "diff-notice",
                    Some(&Notice::error(text, None)),
                    &palette,
                    direction,
                    cx,
                )))
                .child(self.render_retry(&palette, cx))
        });
        div()
            .id("diff-panel")
            .role(Role::Group)
            .accessibility_id("diff-panel")
            .aria_label(strings.get("contextPanel.mode.diff"))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::panel_key))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_toolbar(&palette, direction, cx))
            .children(stale)
            // The line takes its row only while there is an outcome to read.
            .when(self.notice.is_some() && !self.stale, |this| {
                this.child(status_line(
                    "diff-notice",
                    self.notice.as_ref(),
                    &palette,
                    direction,
                    cx,
                ))
            })
            .child(div().flex_1().min_h_0().child(body))
            .children(
                self.render_scopes(&palette, direction, cx)
                    .into_iter()
                    .flatten(),
            )
    }
}
