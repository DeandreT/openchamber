//! Patches in Kit's read-only diff (`PierreDiffViewer.tsx`): a pane for one
//! path that the Git surface shows under its changes, and the Diff surface
//! (`DiffView.tsx`) that shows every changed file of the Changed or Staged
//! scope in one virtualized view.
//!
//! Patches come from the server (`/api/git/diff` and `/api/git/diffs`) and
//! are parsed by Kit. A path the server does not diff (a nested repository,
//! a folder of new files) shows why; a failed read shows the failure with a
//! retry and never an empty diff.

use gpui_kit::component::diff::{Diff, DiffFile, DiffMode, DiffState};
use gpui_kit::{
    AnyElement, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, Role, StatefulInteractiveElement, Styled, Task, Window, div, px,
};
use openchamber_native_core::git_changes::{ChangeScope, ChangesRead, StatusLoads};
use openchamber_native_transport::git::{DIFFS_MAX_PATHS, PathDiff};

use crate::chrome::{ButtonTone, IconButton};
use crate::files_view::{Notice, PanelScope, empty_state, status_line, text_button};
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;

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

    pub(crate) fn set_split(&self, split: bool, cx: &mut gpui_kit::App) {
        let mode = if split {
            DiffMode::Split
        } else {
            DiffMode::Unified
        };
        self.state.update(cx, |state, cx| state.set_mode(mode, cx));
    }

    pub(crate) fn is_split(&self, cx: &gpui_kit::App) -> bool {
        self.state.read(cx).mode() == DiffMode::Split
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

/// The Diff surface: every changed file of one scope.
pub(crate) struct DiffView {
    scope: PanelScope,
    change_scope: ChangeScope,
    status: StatusLoads,
    pane: DiffPane,
    wrap: bool,
    notice: Option<Notice>,
    task: Option<Task<()>>,
}

impl DiffView {
    pub(crate) fn new(scope: PanelScope, cx: &mut Context<Self>) -> Self {
        let view = Self {
            scope,
            change_scope: ChangeScope::Unstaged,
            status: StatusLoads::default(),
            pane: DiffPane::new(cx),
            wrap: false,
            notice: None,
            task: None,
        };
        let entity = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = entity.update(cx, |view, cx| view.refresh(cx));
        });
        view
    }

    /// Reads the status, then the patches of every path in the scope.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let ticket = self.status.begin();
        let Some(client) = self.scope.client(cx) else {
            self.status.complete(ticket, Err(()));
            self.pane.load = DiffLoad::Failed(None);
            cx.notify();
            return;
        };
        let directory = self.scope.directory.clone();
        let staged = self.change_scope == ChangeScope::Staged;
        self.pane.load = DiffLoad::Loading;
        let task = self.scope.run(async move {
            let status = client.git_changes(&directory).await?;
            let paths: Vec<String> = match &status {
                ChangesRead::Repository(changes) => {
                    let rows = if staged {
                        changes.staged()
                    } else {
                        changes.unstaged()
                    };
                    rows.into_iter().map(|row| row.path).collect()
                }
                ChangesRead::NotRepository => Vec::new(),
            };
            let mut diffs = Vec::new();
            for chunk in paths.chunks(DIFFS_MAX_PATHS) {
                diffs.extend(
                    client
                        .git_diffs(&directory, chunk, staged)
                        .await?
                        .into_iter()
                        .map(|(_, diff)| diff),
                );
            }
            Ok::<_, openchamber_native_transport::git::GitError>((status, diffs))
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let Some(result) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.task = None;
                match result {
                    Ok((status, diffs)) => {
                        this.status.complete(ticket, Ok(status));
                        this.pane.apply(Ok(diffs), cx);
                        this.notice = None;
                    }
                    Err(error) => {
                        this.status.complete(ticket, Err(()));
                        let message = error.message().map(str::to_owned);
                        this.pane.load = DiffLoad::Failed(message);
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn set_scope(&mut self, scope: ChangeScope, cx: &mut Context<Self>) {
        if self.change_scope != scope {
            self.change_scope = scope;
            self.refresh(cx);
        }
    }
}

impl Render for DiffView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = cx.global::<Palette>().clone();
        let strings = locale::strings(cx);
        let direction: Direction = strings.direction();
        let split = self.pane.is_split(cx);
        let body = match self.status.current() {
            Some(ChangesRead::NotRepository) => {
                empty_state(strings.get("diffView.state.notGitRepository"), &palette, cx)
            }
            None if self.status.failed() => self.pane.render(self.wrap, &palette, cx),
            None => empty_state(
                strings.get("diffView.state.loadingRepositoryStatus"),
                &palette,
                cx,
            ),
            Some(ChangesRead::Repository(_)) => self.pane.render(self.wrap, &palette, cx),
        };
        div()
            .id("diff-panel")
            .role(Role::Group)
            .accessibility_id("diff-panel")
            .aria_label(strings.get("contextPanel.mode.diff"))
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("diff-scope")
                    .role(Role::Group)
                    .aria_label(strings.get("diffView.scope.selectorAria"))
                    .h(px(40.0))
                    .px(px(8.0))
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(4.0))
                    .child(text_button(
                        "diff-scope-changed",
                        strings.get("diffView.scope.changed"),
                        Some(self.change_scope == ChangeScope::Unstaged),
                        &palette,
                        cx,
                        |this: &mut Self, _, cx| this.set_scope(ChangeScope::Unstaged, cx),
                    ))
                    .child(text_button(
                        "diff-scope-staged",
                        strings.get("diffView.scope.staged"),
                        Some(self.change_scope == ChangeScope::Staged),
                        &palette,
                        cx,
                        |this: &mut Self, _, cx| this.set_scope(ChangeScope::Staged, cx),
                    ))
                    .child(div().flex_1())
                    .child(
                        IconButton::new(
                            "diff-wrap",
                            "text-wrap",
                            strings.get("diffView.actions.wrapLines"),
                        )
                        .tone(if self.wrap {
                            ButtonTone::Active
                        } else {
                            ButtonTone::Quiet
                        })
                        .render(&palette, cx, |this: &mut Self, _, cx| {
                            this.wrap = !this.wrap;
                            cx.notify();
                        }),
                    )
                    .child(
                        IconButton::new(
                            "diff-split",
                            "layout-column",
                            strings.get("diffView.layout.sideBySide"),
                        )
                        .tone(if split {
                            ButtonTone::Active
                        } else {
                            ButtonTone::Quiet
                        })
                        .render(
                            &palette,
                            cx,
                            move |this: &mut Self, _, cx| {
                                this.pane.set_split(!split, cx);
                                cx.notify();
                            },
                        ),
                    )
                    .child(
                        IconButton::new(
                            "diff-refresh",
                            "refresh",
                            strings.get("gitView.history.refresh"),
                        )
                        .render(&palette, cx, |this: &mut Self, _, cx| this.refresh(cx)),
                    ),
            )
            .child(status_line(
                "diff-notice",
                self.notice.as_ref(),
                &palette,
                direction,
                cx,
            ))
            .child(div().flex_1().min_h_0().child(body))
    }
}
