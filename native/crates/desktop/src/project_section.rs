//! The work status card's Project section (`WorkStatusPrimaryGroup.tsx`): the
//! project's name with the open-in-app control, the attention callout and the
//! branch and changed-files rows from the session directory's Git status. It
//! also reads the project's setup, whose actions the header's button runs
//! (`project_action_run.rs`); the card shows no action, as the web's does.
//!
//! The status is read when a session's directory comes into view, again when
//! the last read is over five seconds old as the web's `ensureStatus` does,
//! and once a burst of finished file-changing tool calls in an open session
//! settles. Nested repositories and pull request rows are not ported yet.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui_kit::{
    AnyElement, AppContext, Context, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Role, SharedString, StatefulInteractiveElement, Styled, TestSupportExt, div, img,
    prelude::FluentBuilder, px,
};
use openchamber_native_core::chat_roots;
use openchamber_native_core::path;
use openchamber_native_core::project_actions::ProjectSetup;
use openchamber_native_core::repository::{Attention, GitStatusRead, RepositoryStatus};
use openchamber_native_transport::error::TransportError;

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::context_panel::{self, IconRow, RowValue};
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::open_in_app;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// `STATUS_STALE_THRESHOLD`: a status younger than this is not read again
/// when its directory comes back into view.
const STATUS_STALE: Duration = Duration::from_secs(5);
/// `TOOL_GIT_REFRESH_WINDOW_MS`: finished tool calls within this window
/// share one status read.
const TOOL_REFRESH_WINDOW: Duration = Duration::from_millis(250);
/// The open-in-app control's 24 px buttons with 16 px glyphs.
const APP_BUTTON: f32 = 24.0;
const APP_GLYPH: f32 = 16.0;
const APP_ICON_OPACITY: f32 = 0.85;

/// One directory's Git status reads.
#[derive(Default)]
struct GitEntry {
    /// The last answer; kept while a newer read runs or after one failed.
    read: Option<GitStatusRead>,
    fetched_at: Option<Instant>,
    fetch: Option<gpui_kit::Task<()>>,
    refresh: Option<gpui_kit::Task<()>>,
}

/// One project's setup.
#[derive(Default)]
struct ActionsEntry {
    /// `None` until read; a failed read shows no configured action, as the
    /// web falls back to its empty setup.
    setup: Option<Result<ProjectSetup, TransportError>>,
    _fetch: Option<gpui_kit::Task<()>>,
}

/// Where the file manager's icon lookup stands.
#[derive(Default)]
enum AppIcon {
    #[default]
    Unknown,
    Looking {
        _lookup: gpui_kit::Task<()>,
    },
    Found(Option<PathBuf>),
}

/// The Project section's reads, by directory and project.
#[derive(Default)]
pub(crate) struct ProjectReadouts {
    git: HashMap<String, GitEntry>,
    actions: HashMap<String, ActionsEntry>,
    app_icon: AppIcon,
    /// The directory last brought into view, so a return refreshes it.
    in_view: Option<String>,
}

impl ProjectReadouts {
    /// Settles the file-manager icon as absent. A headless test against a
    /// loopback server would otherwise scan this machine's icon themes on the
    /// test thread, where background tasks run inline.
    #[cfg(test)]
    pub(crate) fn without_app_icon(&mut self) {
        self.app_icon = AppIcon::Found(None);
    }
}

/// What the section is about: the session's directory and the project whose
/// actions run there.
pub(crate) struct Subject {
    pub(crate) directory: String,
    /// The project the directory belongs to, for its name.
    pub(crate) project_path: Option<String>,
    /// The project whose actions the header offers: the directory's, else
    /// the active one; none in a managed Chat.
    pub(crate) action_project: Option<String>,
}

pub(crate) struct HeaderProjectMeta {
    pub(crate) project: Option<SharedString>,
    pub(crate) branch: Option<SharedString>,
}

impl Workspace {
    /// Header metadata uses the same retained project and Git readouts as
    /// the status card. Managed Chats carry no project metadata line.
    pub(crate) fn header_project_meta(&self) -> Option<HeaderProjectMeta> {
        let subject = self.project_subject()?;
        if chat_roots::is_chat_directory(
            self.topology.chat_roots.as_ref(),
            Some(&subject.directory),
        ) {
            return None;
        }
        let project = subject
            .project_path
            .as_deref()
            .map(|path| self.project_label(path));
        let branch = self
            .project_readouts
            .git
            .get(&subject.directory)
            .and_then(|entry| entry.read.as_ref())
            .and_then(|read| match read {
                GitStatusRead::Repository(status) => status.branch.clone().map(SharedString::from),
                _ => None,
            });
        (project.is_some() || branch.is_some()).then_some(HeaderProjectMeta { project, branch })
    }

    pub(crate) fn project_subject(&self) -> Option<Subject> {
        let directory = if self.selected.is_some() {
            let session = self.selected_session()?;
            path::normalize_optional(session.directory.as_deref())?
        } else {
            path::normalize(self.draft_projects.get(&self.runtime)?)?
        };
        let project_path = self.project_of_directory(&directory);
        let in_chat =
            chat_roots::is_chat_directory(self.topology.chat_roots.as_ref(), Some(&directory));
        let action_project = (!in_chat)
            .then(|| {
                project_path.clone().or_else(|| {
                    self.send_catalog
                        .settings
                        .as_ref()
                        .and_then(|settings| settings.active_project_path())
                        .map(str::to_owned)
                })
            })
            .flatten();
        Some(Subject {
            directory,
            project_path,
            action_project,
        })
    }

    pub(crate) fn draft_project_label(&self) -> Option<SharedString> {
        self.draft_projects
            .get(&self.runtime)
            .map(|path| self.project_label(path))
            .filter(|label| !label.is_empty())
    }

    /// `resolveProjectForSessionDirectory`: the registered project the
    /// directory is in, or the one owning the worktree it is in, whichever
    /// matches the longer path.
    pub(crate) fn project_of_directory(&self, directory: &str) -> Option<String> {
        let direct = self
            .topology
            .projects
            .iter()
            .filter(|project| path::is_within(directory, &project.path))
            .max_by_key(|project| project.path.len());
        let worktree = self
            .topology
            .worktrees
            .iter()
            .flat_map(|entry| {
                entry
                    .worktrees
                    .iter()
                    .map(move |worktree| (entry.project_path.as_str(), worktree.path.as_str()))
            })
            .filter(|(_, worktree)| path::is_within(directory, worktree))
            .max_by_key(|(_, worktree)| worktree.len());
        match (direct, worktree) {
            (Some(project), Some((owner, worktree))) if worktree.len() > project.path.len() => {
                Some(owner.to_owned())
            }
            (Some(project), _) => Some(project.path.clone()),
            (None, Some((owner, _))) => Some(owner.to_owned()),
            (None, None) => None,
        }
    }

    /// Starts the reads the selected session's Project section needs.
    pub(crate) fn ensure_project_readouts(&mut self, cx: &mut Context<Self>) {
        if self
            .connection
            .as_ref()
            .and_then(|connection| connection.client.as_ref())
            .is_none()
        {
            return;
        }
        let Some(subject) = self.project_subject() else {
            self.project_readouts.in_view = None;
            return;
        };
        let newly_in_view = self.project_readouts.in_view.as_deref() != Some(&subject.directory);
        if newly_in_view {
            self.project_readouts.in_view = Some(subject.directory.clone());
            let stale = self
                .project_readouts
                .git
                .get(&subject.directory)
                .is_none_or(|entry| {
                    entry
                        .fetched_at
                        .is_none_or(|at| at.elapsed() >= STATUS_STALE)
                        && entry.fetch.is_none()
                });
            if stale {
                self.read_git_status(&subject.directory, cx);
            }
        }
        if let Some(project) = subject.action_project
            && !self.project_readouts.actions.contains_key(&project)
        {
            self.read_project_actions(&project, cx);
        }
        if matches!(self.project_readouts.app_icon, AppIcon::Unknown) && self.desktop_local() {
            let lookup = cx.background_spawn(async { open_in_app::file_manager_icon() });
            self.project_readouts.app_icon = AppIcon::Looking {
                _lookup: cx.spawn(async move |this, cx| {
                    let icon = lookup.await;
                    let _ = this.update(cx, |this, cx| {
                        this.project_readouts.app_icon = AppIcon::Found(icon);
                        cx.notify();
                    });
                }),
            };
        }
    }

    /// Whether the server runs on this machine, so its paths open here.
    fn desktop_local(&self) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|connection| connection.config.endpoint.is_loopback())
    }

    fn read_git_status(&mut self, directory: &str, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let read_directory = directory.to_owned();
        let task = connection
            .network
            .run(async move { client.git_status(&read_directory).await });
        let key = directory.to_owned();
        let fetch = cx.spawn(async move |this, cx| {
            let outcome = task.await;
            let _ = this.update(cx, |this, cx| {
                let entry = this.project_readouts.git.entry(key).or_default();
                entry.fetch = None;
                match outcome {
                    Some(Ok(read)) => {
                        entry.read = Some(read);
                        entry.fetched_at = Some(Instant::now());
                    }
                    Some(Err(error)) => {
                        tracing::debug!(%error, "the Git status could not be read");
                    }
                    None => {}
                }
                cx.notify();
            });
        });
        self.project_readouts
            .git
            .entry(directory.to_owned())
            .or_default()
            .fetch = Some(fetch);
    }

    /// The project's setup once read; a failed read offers no action.
    pub(crate) fn project_setup(&self, project_path: &str) -> Option<&ProjectSetup> {
        self.project_readouts
            .actions
            .get(project_path)?
            .setup
            .as_ref()?
            .as_ref()
            .ok()
    }

    /// This instance now trusts the project's shared commands with `hash`:
    /// the prompt stays away at once, and the answer is recorded on the
    /// server. A failed record brings the prompt back with the next read.
    pub(crate) fn trusted_shared_setup(
        &mut self,
        project_path: &str,
        hash: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(Some(Ok(setup))) = self
            .project_readouts
            .actions
            .get_mut(project_path)
            .map(|entry| entry.setup.as_mut())
        {
            setup.untrusted = None;
        }
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let project = project_path.to_owned();
        let hash = hash.to_owned();
        let record = connection
            .network
            .run(async move { client.trust_shared_setup(&project, &hash).await });
        cx.background_executor()
            .spawn(async move {
                if let Some(Err(error)) = record.await {
                    tracing::warn!(%error, "the trust answer could not be recorded");
                }
            })
            .detach();
    }

    fn read_project_actions(&mut self, project_path: &str, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let read_path = project_path.to_owned();
        let task = connection
            .network
            .run(async move { client.project_setup(&read_path).await });
        let key = project_path.to_owned();
        let fetch = cx.spawn(async move |this, cx| {
            let Some(outcome) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if let Err(error) = &outcome {
                    tracing::debug!(%error, "the project actions could not be read");
                }
                this.project_readouts.actions.entry(key).or_default().setup = Some(outcome);
                cx.notify();
            });
        });
        self.project_readouts.actions.insert(
            project_path.to_owned(),
            ActionsEntry {
                setup: None,
                _fetch: Some(fetch),
            },
        );
    }

    /// A file-changing tool call in a session of `directory` finished: its
    /// status is read once the burst settles.
    pub(crate) fn refresh_git_status_soon(&mut self, directory: &str, cx: &mut Context<Self>) {
        let Some(directory) = path::normalize(directory) else {
            return;
        };
        let Some(entry) = self.project_readouts.git.get_mut(&directory) else {
            // Never shown: it is read when it comes into view.
            return;
        };
        if entry.refresh.is_some() {
            return;
        }
        let key = directory.clone();
        entry.refresh = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TOOL_REFRESH_WINDOW).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(entry) = this.project_readouts.git.get_mut(&key) {
                    entry.refresh = None;
                }
                this.read_git_status(&key, cx);
            });
        }));
    }

    /// Forgets every read, as a new runtime must.
    pub(crate) fn reset_project_readouts(&mut self) {
        let app_icon = std::mem::take(&mut self.project_readouts.app_icon);
        self.project_readouts = ProjectReadouts {
            app_icon,
            ..ProjectReadouts::default()
        };
    }

    /// The section, when the session's directory has a branch, changes or an
    /// operation needing attention, or the open-in-app control applies.
    pub(crate) fn render_project_section(
        &self,
        palette: &Palette,
        direction: Direction,
        first_heading: bool,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let subject = self.project_subject()?;
        let status = match self
            .project_readouts
            .git
            .get(&subject.directory)
            .and_then(|entry| entry.read.as_ref())
        {
            Some(GitStatusRead::Repository(status)) => Some(status),
            _ => None,
        };
        let has_readout = status.is_some_and(|status| {
            status.branch.is_some() || status.changed().is_some() || status.attention.is_some()
        });
        if !has_readout && (subject.action_project.is_none() || !self.desktop_local()) {
            return None;
        }
        let strings = locale::strings(cx);
        let label = subject
            .project_path
            .as_deref()
            .map(|path| self.project_label(path));
        let mut rows: Vec<AnyElement> = Vec::new();
        if let Some(status) = status {
            rows.extend(self.repository_rows(status, strings, palette, direction, cx));
        }
        let summary = subject
            .action_project
            .is_some()
            .then(|| self.render_open_in_app(&subject.directory, strings, palette, direction, cx))
            .flatten();
        Some(
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .mb(px(2.0))
                        .px(px(4.0))
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(8.0))
                        .when(first_heading, |heading| {
                            heading.padding_end(direction, px(28.0))
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex_row_from(direction)
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .css_text(
                                            context_panel::XS_SIZE,
                                            context_panel::XS_LINE,
                                            cx,
                                        )
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(palette.foreground)
                                        .child(strings.get("chat.workStatus.section.project")),
                                )
                                .children(label.map(|label| {
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .css_text(
                                            context_panel::XS_SIZE,
                                            context_panel::XS_LINE,
                                            cx,
                                        )
                                        .text_color(palette.muted_foreground)
                                        .child(label)
                                })),
                        )
                        .children(summary),
                )
                .children(rows)
                .into_any_element(),
        )
    }

    /// The project's label from settings, else its folder's name.
    pub(crate) fn project_label(&self, project_path: &str) -> SharedString {
        self.send_catalog
            .settings
            .as_ref()
            .and_then(|settings| {
                settings
                    .project_labels
                    .iter()
                    .find(|entry| entry.path == project_path)
            })
            .map(|entry| SharedString::from(entry.label.clone()))
            .unwrap_or_else(|| {
                project_path
                    .rsplit('/')
                    .find(|segment| !segment.is_empty())
                    .unwrap_or(project_path)
                    .to_owned()
                    .into()
            })
    }

    fn repository_rows(
        &self,
        status: &RepositoryStatus,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = Vec::new();
        if let Some(attention) = status.attention {
            rows.push(render_callout(
                attention_label(attention, strings),
                palette,
                direction,
                cx,
            ));
        }
        if let Some(branch) = &status.branch {
            let values = status
                .divergence()
                .into_iter()
                .map(|count| RowValue {
                    text: count.into(),
                    color: None,
                })
                .collect();
            rows.push(context_panel::icon_row(
                IconRow {
                    icon: "git-branch",
                    label: branch.clone().into(),
                    values,
                },
                palette,
                direction,
                cx,
            ));
        }
        if let Some(changes) = status.changed() {
            let key = if changes.files == 1 {
                "chat.workStatus.git.changedFileSingle"
            } else {
                "chat.workStatus.git.changedFilePlural"
            };
            let values = changes
                .line_counts
                .filter(|counts| counts.additions > 0 || counts.deletions > 0)
                .map(|counts| {
                    vec![
                        RowValue {
                            text: format!("+{}", counts.additions).into(),
                            color: Some(palette.status_success),
                        },
                        RowValue {
                            text: "/".into(),
                            color: None,
                        },
                        RowValue {
                            text: format!("\u{2212}{}", counts.deletions).into(),
                            color: Some(palette.status_error),
                        },
                    ]
                })
                .unwrap_or_default();
            rows.push(context_panel::icon_row(
                IconRow {
                    icon: "file-edit",
                    label: strings.format(key, &[("count", &changes.files.to_string())]),
                    values,
                },
                palette,
                direction,
                cx,
            ));
        }
        rows
    }

    /// The file manager control at the heading's end: its icon, or the
    /// label's initial, and the chevron of the app menu.
    fn render_open_in_app(
        &self,
        directory: &str,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !self.desktop_local() {
            return None;
        }
        let icon: AnyElement = match &self.project_readouts.app_icon {
            AppIcon::Found(Some(file)) => img(file.clone())
                .size(px(APP_GLYPH))
                .rounded(px(4.0))
                .into_any_element(),
            _ => div()
                .size(px(APP_GLYPH))
                .rounded(px(4.0))
                .bg(palette.muted)
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(9.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(palette.muted_foreground)
                .child(open_in_app::initial())
                .into_any_element(),
        };
        let label = strings.format(
            "openInApp.actions.openInAria",
            &[("app", open_in_app::LABEL)],
        );
        let target = PathBuf::from(directory);
        let open = on_activate(
            div()
                .id("open-in-app")
                .role(Role::Button)
                .accessibility_id("open-in-app")
                .aria_label(label)
                .size(px(APP_BUTTON))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .opacity(APP_ICON_OPACITY)
                .hover(|style| style.opacity(1.0))
                .cursor_pointer()
                .child(icon),
            cx,
            move |_, _, _| open_directory(&target),
        )
        .test_support();
        Some(
            div()
                .flex_shrink_0()
                .flex_row_from(direction)
                .items_center()
                .text_color(palette.muted_foreground)
                .child(open)
                .child(
                    div()
                        .size(px(APP_BUTTON))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(glyph("arrow-down-s", APP_GLYPH, palette.muted_foreground)),
                )
                .into_any_element(),
        )
    }
}

fn open_directory(directory: &Path) {
    if let Err(error) = open_in_app::open_directory(directory) {
        tracing::warn!(%error, "the file manager could not be opened");
    }
}

fn attention_label(attention: Attention, strings: &Strings) -> SharedString {
    strings.get(match attention {
        Attention::Merge => "chat.workStatus.attention.merge",
        Attention::Rebase => "chat.workStatus.attention.rebase",
        Attention::CherryPick => "chat.workStatus.attention.cherryPick",
        Attention::Revert => "chat.workStatus.attention.revert",
        Attention::Bisect => "chat.workStatus.attention.bisect",
    })
}

/// `WorkStatusCallout`: a warning-toned band above the rows.
fn render_callout(
    label: SharedString,
    palette: &Palette,
    direction: Direction,
    cx: &Context<Workspace>,
) -> AnyElement {
    div()
        .mx(px(4.0))
        .mb(px(4.0))
        .px(px(8.0))
        .py(px(6.0))
        .rounded(px(6.0))
        .bg(palette.status_warning_background)
        .flex_row_from(direction)
        .items_center()
        .gap(px(8.0))
        .text_color(palette.status_warning)
        .child(glyph("alert", 16.0, palette.status_warning))
        .child(
            div()
                .min_w_0()
                .truncate()
                .css_text(context_panel::ROW_SIZE, context_panel::ROW_LINE, cx)
                .font_weight(FontWeight::MEDIUM)
                .child(label),
        )
        .into_any_element()
}
