//! The row menu's Export Markdown.
//!
//! Follows `SessionNodeItem`'s `handleExportSession` and `doExportSession`
//! with `exportSession.ts`. A session with sub-agent tasks first asks
//! whether to include them. The whole history is read; a sub-agent that
//! cannot be loaded is skipped with its own tasks and counted. The file is
//! written where the platform's save dialog says, and the success toast
//! offers to reveal it.

use std::path::{Path, PathBuf};

use chrono::{Datelike, Local, TimeZone, Timelike};
use gpui_kit::component::WindowExt;
use gpui_kit::component::button::Button;
use gpui_kit::component::notification::Notification;
use gpui_kit::{
    Context, Hsla, InteractiveElement, ParentElement, Role, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::ids::SessionId;
use openchamber_native_core::session::SessionRecord;
use openchamber_native_core::session_index::SessionIndex;
use openchamber_native_core::session_markdown::{self, ChildExport, ExportClock};
use openchamber_native_core::transcript::Message;
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::error::TransportError;

use crate::chrome::glyph;
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

const PAGE: usize = 200;

/// A sub-agent session and its own sub-agents, from the session list.
#[derive(Clone, Debug)]
struct ChildTree {
    id: SessionId,
    title: String,
    agent: Option<String>,
    children: Vec<ChildTree>,
}

impl ChildTree {
    fn count(&self) -> usize {
        1 + self.children.iter().map(ChildTree::count).sum::<usize>()
    }
}

/// The active sub-agent tree under `root` (`buildActiveSessionTreeFromCache`).
fn child_trees(index: &SessionIndex, root: &SessionId, untitled: &str) -> Vec<ChildTree> {
    let mut children: Vec<&SessionRecord> = index
        .active()
        .iter()
        .filter(|session| session.parent_id.as_ref() == Some(root))
        .collect();
    children.sort_by_key(|session| session.time.created);
    children
        .into_iter()
        .map(|session| ChildTree {
            id: session.id.clone(),
            title: if session.title.is_empty() {
                untitled.to_owned()
            } else {
                session.title.clone()
            },
            agent: session.agent.clone(),
            children: child_trees(index, &session.id, untitled),
        })
        .collect()
}

/// An export waiting for the include-sub-agents answer.
#[derive(Clone, Debug)]
pub(crate) struct ExportConfirm {
    session: SessionId,
    title: String,
    children: Vec<ChildTree>,
    include: bool,
}

/// The header times of the file: the locale's short month, the day, the
/// year and a 24-hour clock, in local time (`formatTimestamp`).
struct LocalClock {
    months: Vec<String>,
}

impl ExportClock for LocalClock {
    fn format(&self, millis: i64) -> String {
        let Some(time) = Local.timestamp_millis_opt(millis).single() else {
            return String::new();
        };
        let month = self
            .months
            .get(time.month0() as usize)
            .cloned()
            .unwrap_or_default();
        format!(
            "{month} {}, {}, {:02}:{:02}",
            time.day(),
            time.year(),
            time.hour(),
            time.minute()
        )
    }
}

/// Every message of a session, oldest first.
async fn all_messages(
    client: &ServerClient,
    session: &SessionId,
) -> Result<Vec<Message>, TransportError> {
    let page = client.latest_messages(session, PAGE).await?;
    let mut messages = page.messages;
    let mut cursor = page.older;
    let mut visited = std::collections::HashSet::new();
    while let Some(older) = cursor.take() {
        if !visited.insert(older.clone()) {
            return Err(TransportError::Malformed(
                "session history pagination made no progress".to_owned(),
            ));
        }
        let page = client.earlier_messages(session, &older, PAGE).await?;
        let mut earlier = page.messages;
        earlier.append(&mut messages);
        messages = earlier;
        cursor = page.older;
    }
    Ok(messages)
}

/// Loads the sub-agents depth first; one that fails is skipped with its
/// own sub-agents, and counted.
async fn child_exports(client: &ServerClient, trees: &[ChildTree]) -> (Vec<ChildExport>, usize) {
    let mut exports = Vec::new();
    let mut skipped = 0;
    for tree in trees {
        match all_messages(client, &tree.id).await {
            Ok(messages) => {
                let (children, missed) = Box::pin(child_exports(client, &tree.children)).await;
                skipped += missed;
                exports.push(ChildExport {
                    title: tree.title.clone(),
                    agent: tree.agent.clone(),
                    messages,
                    children,
                });
            }
            Err(_) => skipped += tree.count(),
        }
    }
    (exports, skipped)
}

/// What the export read.
enum Loaded {
    Ready {
        messages: Vec<Message>,
        children: Vec<ChildExport>,
        skipped: usize,
    },
    Failed,
}

fn today() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

/// Where the save dialog starts: the Downloads folder, else the home folder.
fn save_directory() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let downloads = home.join("Downloads");
    if downloads.is_dir() { downloads } else { home }
}

impl Workspace {
    /// Export Markdown from the row menu.
    pub(crate) fn export_session_markdown(
        &mut self,
        session: &SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(record) = self.sessions.get(session) else {
            return;
        };
        if record.resolved_directory().is_none() {
            self.push_error(
                "sessions.sidebar.session.export.nothingToExport",
                None,
                window,
                cx,
            );
            return;
        }
        let strings = locale::strings(cx).clone();
        let untitled = strings.get("sessions.sidebar.session.export.untitledSubagent");
        let children = child_trees(&self.sessions, session, &untitled);
        let confirm = ExportConfirm {
            session: session.clone(),
            title: record.title.clone(),
            children,
            include: true,
        };
        if confirm.children.is_empty() {
            self.run_export(confirm, window, cx);
        } else {
            self.open_export_dialog(confirm, window, cx);
        }
    }

    fn run_export(&mut self, confirm: ExportConfirm, window: &mut Window, cx: &mut Context<Self>) {
        let id = confirm.session.clone();
        let trees = if confirm.include {
            confirm.children.clone()
        } else {
            Vec::new()
        };
        let title = confirm.title.clone();
        self.spawn_session_task(
            move |client| {
                Box::pin(async move {
                    let Ok(messages) = all_messages(&client, &id).await else {
                        return Loaded::Failed;
                    };
                    let (children, skipped) = child_exports(&client, &trees).await;
                    Loaded::Ready {
                        messages,
                        children,
                        skipped,
                    }
                })
            },
            move |this, loaded, window, cx| match loaded {
                Some(Loaded::Ready {
                    messages,
                    children,
                    skipped,
                }) => this.save_export(&title, messages, children, skipped, window, cx),
                Some(Loaded::Failed) => this.push_error(
                    "sessions.sidebar.session.export.failedLoadHistory",
                    None,
                    window,
                    cx,
                ),
                None => {}
            },
            window,
            cx,
        );
    }

    fn save_export(
        &mut self,
        title: &str,
        messages: Vec<Message>,
        children: Vec<ChildExport>,
        skipped: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if messages.is_empty() {
            self.push_error(
                "sessions.sidebar.session.export.nothingToExport",
                None,
                window,
                cx,
            );
            return;
        }
        let date = today();
        let clock = LocalClock {
            months: locale::strings(cx).dates().months.clone(),
        };
        let markdown =
            session_markdown::session_markdown(&messages, title, &children, &date, &clock);
        let name = session_markdown::export_filename(title, &date);
        let answer = cx.prompt_for_new_path(&save_directory(), Some(&name));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = answer.await else {
                return;
            };
            let target = path.clone();
            let written = cx
                .background_executor()
                .spawn(async move { std::fs::write(&target, markdown) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match written {
                Ok(()) => this.exported(path, skipped, window, cx),
                Err(error) => {
                    // The operating system's own explanation, such as a
                    // missing permission.
                    let text: SharedString = error.to_string().into();
                    this.push_notice(Notification::error(text.clone()), text, window, cx);
                }
            });
        })
        .detach();
    }

    fn exported(
        &mut self,
        path: PathBuf,
        skipped: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let strings = locale::strings(cx).clone();
        let text = strings.get("sessions.sidebar.session.export.success");
        let reveal = strings.get(reveal_label_key());
        let notification = Notification::success(text.clone()).action(move |_, _, _| {
            let path = path.clone();
            Button::new("session-export-reveal")
                .accessibility_id("session-export-reveal")
                .label(reveal.clone())
                .on_click(move |_, _, cx| reveal_export(&path, cx))
        });
        self.push_notice(notification, text, window, cx);
        if skipped > 0 {
            let count = skipped.to_string();
            let key = if skipped == 1 {
                "sessions.sidebar.session.export.skippedSubtaskSingle"
            } else {
                "sessions.sidebar.session.export.skippedSubtaskMany"
            };
            let warning = strings.format(key, &[("count", &count)]);
            self.push_notice(Notification::warning(warning.clone()), warning, window, cx);
        }
    }

    /// Asks whether the export includes the sub-agent tasks.
    fn open_export_dialog(
        &mut self,
        confirm: ExportConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session_management.export = Some(confirm);
        let workspace = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let Some(this) = workspace.upgrade() else {
                return dialog;
            };
            let Some(confirm) = this.read(cx).session_management.export.clone() else {
                return dialog;
            };
            let palette = cx.global::<Palette>().clone();
            let strings = locale::strings(cx).clone();
            let direction = strings.direction();
            let count: usize = confirm.children.iter().map(ChildTree::count).sum();
            let description = strings.format(
                if count == 1 {
                    "sessions.sidebar.session.export.dialog.descriptionSingle"
                } else {
                    "sessions.sidebar.session.export.dialog.descriptionMany"
                },
                &[("count", &count.to_string())],
            );
            let on_toggle = workspace.clone();
            let on_cancel = workspace.clone();
            let on_confirm = workspace.clone();
            let on_close = workspace.clone();
            dialog
                .w(px(384.0))
                .close_button(false)
                .on_close(move |_, _, cx| {
                    let _ = on_close.update(cx, |this, cx| {
                        this.session_management.export = None;
                        cx.notify();
                    });
                })
                .title(strings.get("sessions.sidebar.session.export.dialog.title"))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(12.0))
                        .child(
                            div()
                                .text_size(gpui_kit::rems(0.843_75))
                                .text_color(palette.muted_foreground)
                                .child(description),
                        )
                        .child(
                            checkbox(
                                "session-export-include",
                                strings
                                    .get("sessions.sidebar.session.export.dialog.includeSubtasks"),
                                confirm.include,
                                &palette,
                                direction,
                            )
                            .on_click(move |_, _, cx| {
                                let _ = on_toggle.update(cx, |this, cx| {
                                    if let Some(confirm) = this.session_management.export.as_mut() {
                                        confirm.include = !confirm.include;
                                    }
                                    cx.notify();
                                });
                            }),
                        ),
                )
                .footer(
                    div()
                        .w_full()
                        .flex_row_from(direction)
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            dialog_button(
                                "session-export-cancel",
                                strings.get("sessions.sidebar.dialogs.cancel"),
                                None,
                                palette.foreground,
                                &palette,
                            )
                            .on_click(move |_, window, cx| {
                                let _ = on_cancel.update(cx, |this, cx| {
                                    this.session_management.export = None;
                                    window.close_dialog(cx);
                                    cx.notify();
                                });
                            }),
                        )
                        .child(
                            dialog_button(
                                "session-export-confirm",
                                strings.get("sessions.sidebar.session.export.dialog.confirm"),
                                Some(palette.primary),
                                palette.primary_foreground,
                                &palette,
                            )
                            .on_click(move |_, window, cx| {
                                let _ = on_confirm.update(cx, |this, cx| {
                                    let Some(confirm) = this.session_management.export.take()
                                    else {
                                        return;
                                    };
                                    window.close_dialog(cx);
                                    this.run_export(confirm, window, cx);
                                });
                            }),
                        ),
                )
        });
        cx.notify();
    }
}

/// `getRevealLabelKey` on Linux.
fn reveal_label_key() -> &'static str {
    if cfg!(target_os = "macos") {
        "common.revealPath.finder"
    } else if cfg!(target_os = "windows") {
        "common.revealPath.fileExplorer"
    } else {
        "common.revealPath.fileManager"
    }
}

fn reveal_export(path: &Path, cx: &mut gpui_kit::App) {
    cx.reveal_path(path);
}

fn checkbox(
    id: &'static str,
    label: SharedString,
    checked: bool,
    palette: &Palette,
    direction: locale::Direction,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .role(Role::CheckBox)
        .accessibility_id(id)
        .aria_label(label.clone())
        .aria_toggled(if checked {
            gpui_kit::Toggled::True
        } else {
            gpui_kit::Toggled::False
        })
        .flex_row_from(direction)
        .items_center()
        .gap(px(6.0))
        .cursor_pointer()
        .text_color(palette.foreground)
        .child(glyph(
            if checked {
                "checkbox"
            } else {
                "checkbox-blank"
            },
            16.0,
            if checked {
                palette.primary
            } else {
                palette.muted_foreground
            },
        ))
        .child(div().text_size(gpui_kit::rems(0.843_75)).child(label))
}

pub(crate) fn dialog_button(
    id: &'static str,
    label: SharedString,
    fill: Option<Hsla>,
    text: Hsla,
    palette: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .role(Role::Button)
        .accessibility_id(id)
        .aria_label(label.clone())
        .h(px(32.0))
        .px(px(12.0))
        .rounded(px(6.0))
        .flex()
        .items_center()
        .cursor_pointer()
        .text_color(text)
        .when_some(fill, |this, fill| this.bg(fill))
        .when(fill.is_none(), |this| {
            this.border_1().border_color(palette.border)
        })
        .child(div().text_size(gpui_kit::rems(0.843_75)).child(label))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_writes_short_month_day_year_and_24_hours() {
        let clock = LocalClock {
            months: [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ]
            .map(str::to_owned)
            .to_vec(),
        };
        let time = Local
            .with_ymd_and_hms(2026, 3, 5, 9, 7, 0)
            .single()
            .expect("a valid local time");
        assert_eq!(clock.format(time.timestamp_millis()), "Mar 5, 2026, 09:07");
    }

    #[test]
    fn a_child_counts_with_its_own_sub_agents() {
        let leaf = ChildTree {
            id: SessionId::from("c"),
            title: "c".into(),
            agent: None,
            children: Vec::new(),
        };
        let tree = ChildTree {
            id: SessionId::from("b"),
            title: "b".into(),
            agent: None,
            children: vec![leaf],
        };
        assert_eq!(tree.count(), 2);
    }
}
