//! Tool call rows (`ToolPart.tsx`, and `StaticToolRow` in
//! `ProgressiveGroup.tsx` for navigation tools).
//!
//! A row is one line: the tool's icon (a chevron under the pointer), its
//! name, what the call worked on, and per tool its diff counts, duration or
//! failure. Navigation tools stay that line. Every other row opens on a
//! click to the call's input, result and error beside the block line. A
//! result a history page left out is read with its whole message when the
//! row opens; the row shows the read's progress and a Retry after a failure.

use gpui_kit::component::Theme;
use gpui_kit::{
    AnyElement, App, ClipboardItem, Context, ElementId, Entity, FontWeight, Hsla,
    InteractiveElement, IntoElement, ParentElement, Role, SharedString, StatefulInteractiveElement,
    Styled, TestSupportExt, Window, div, prelude::FluentBuilder, px, rems,
};
use openchamber_native_core::ids::{MessageId, PartId, SessionId};
use openchamber_native_core::tool_shells::{self, ToolDescription, ToolFacts};
use openchamber_native_core::transcript::ToolStatus;
use openchamber_native_core::{tool_names, transcript::PartContent};

use crate::activation::on_activate;
use crate::chrome::{IconButton, glyph};
use crate::connection::ConnectionFailure;
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::markdown_view::{self, Author};
use crate::part_state::HydrationView;
use crate::reasoning_view::{ROW_LINE_HEIGHT, block_body, toggle_icon};
use crate::server_sync::apply_list_change;
use crate::theme::Palette;
use crate::transcript_clock::now_millis;
use crate::transcript_view::META_SIZE;
use crate::workspace::Workspace;

/// Input and generic output boxes: `max-h-60`.
const INPUT_MAX_HEIGHT: f32 = 240.0;
/// Shell output: `max-h-[46vh]` of a typical window.
const SHELL_OUTPUT_MAX_HEIGHT: f32 = 400.0;
/// Diffs and results without their own cap.
const RESULT_MAX_HEIGHT: f32 = 320.0;

/// One tool call as a row draws it.
pub(crate) struct ToolRow<'a> {
    pub(crate) session: &'a SessionId,
    pub(crate) message: &'a MessageId,
    pub(crate) part: &'a PartId,
    pub(crate) tool: &'a str,
    pub(crate) status: ToolStatus,
    pub(crate) output: Option<&'a str>,
    pub(crate) elided: Option<usize>,
    pub(crate) facts: &'a ToolFacts,
    pub(crate) start: Option<i64>,
    pub(crate) end: Option<i64>,
    /// The session directory paths are shown relative to.
    pub(crate) directory: &'a str,
    pub(crate) expanded: bool,
    pub(crate) hydration: Option<HydrationView>,
    /// A subagent row's Open and Stop actions.
    pub(crate) controls: Option<AnyElement>,
}

impl ToolRow<'_> {
    fn failed(&self) -> bool {
        self.status == ToolStatus::Error
    }

    fn running(&self) -> bool {
        matches!(self.status, ToolStatus::Pending | ToolStatus::Running)
    }

    /// A shell call OpenCode moved to the background: its own part settles
    /// at once while the command keeps running.
    fn background(&self) -> bool {
        tool_shells::is_shell(self.tool)
            && self.status == ToolStatus::Completed
            && self.facts.metadata.shell_id.is_some()
    }

    fn output(&self) -> &str {
        let output = self.output.unwrap_or_default();
        if self.background() {
            tool_shells::without_model_instruction(output)
        } else {
            output
        }
    }
}

impl Workspace {
    pub(crate) fn render_tool_row(
        &self,
        mut row: ToolRow<'_>,
        palette: &Palette,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let static_row = tool_shells::is_static(row.tool);
        let duration = self.tool_duration(&row);
        if row.running() && !static_row {
            self.tick_live_indicators(cx);
        }
        let copied = (row.expanded && !static_row).then(|| {
            window.use_keyed_state(
                ElementId::Name(format!("tool:{}:copied", row.part).into()),
                cx,
                |_, _| false,
            )
        });
        let cx: &Context<Self> = cx;
        let strings = locale::strings(cx);
        let failed = row.failed();
        let title_color = if failed {
            palette.status_error
        } else {
            palette.tools_title
        };
        let icon_color = if failed {
            palette.status_error
        } else {
            palette.tools_icon
        };
        let id: SharedString = format!("tool:{}", row.part).into();
        let group: SharedString = format!("tool-row:{}", row.part).into();

        let mut header = div()
            .id(ElementId::Name(format!("{id}:header").into()))
            .group(group.clone())
            .flex()
            .flex_row_from(direction)
            .items_center()
            .gap(px(6.0))
            .padding_start(direction, px(1.0))
            .padding_end(direction, px(8.0))
            .py(px(6.0))
            .rounded(px(12.0))
            .text_size(rems(META_SIZE))
            .line_height(px(ROW_LINE_HEIGHT))
            .child(if static_row {
                div()
                    .flex()
                    .items_center()
                    .h(px(20.0))
                    .child(glyph(tool_shells::icon(row.tool), 14.0, icon_color))
                    .into_any_element()
            } else {
                toggle_icon(
                    &group,
                    tool_shells::icon(row.tool),
                    icon_color,
                    row.expanded,
                    palette,
                )
            })
            .child(
                div()
                    .flex_shrink_0()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(title_color)
                    .child(tool_shells::display_name(row.tool)),
            )
            .children(row.controls.take())
            .when_some(duration, |this, duration| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .text_color(palette.muted_foreground)
                        .child(duration),
                )
            })
            .when(row.background(), |this| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .text_color(palette.muted_foreground)
                        .child(strings.get("chat.toolPart.background.label")),
                )
            })
            .child(self.tool_description(&row, strings, palette, direction));
        if static_row {
            return div()
                .id(ElementId::Name(id))
                .role(Role::Group)
                .child(header)
                .into_any_element();
        }
        let (session, message, part) = (row.session.clone(), row.message.clone(), row.part.clone());
        let toggle = move |this: &mut Workspace, _: &mut Window, cx: &mut Context<Workspace>| {
            this.toggle_tool_row(&session, &message, &part, cx);
        };
        header = header
            .role(Role::Button)
            .accessibility_id(format!("{id}:header"))
            .aria_label(tool_shells::display_name(row.tool).into_owned())
            .aria_expanded(row.expanded)
            .cursor_pointer();
        let header = on_activate(header, cx, toggle.clone()).test_support();
        let body = row.expanded.then(|| {
            block_body(
                format!("{id}:line").into(),
                palette,
                direction,
                cx,
                toggle,
                self.tool_body(&row, strings, palette, direction, copied, cx),
            )
        });
        div()
            .id(ElementId::Name(id))
            .flex()
            .flex_col()
            .child(header)
            .children(body)
            .into_any_element()
    }

    /// A shell, subagent or image row's duration: live while it runs on the
    /// shared clock, tenths once settled. A background call's end is not
    /// known, so it shows none.
    fn tool_duration(&self, row: &ToolRow<'_>) -> Option<String> {
        if !tool_shells::shows_duration(row.tool) || row.background() {
            return None;
        }
        let start = row.start?;
        let end = match row.status {
            ToolStatus::Completed | ToolStatus::Error => Some(row.end.unwrap_or(start)),
            ToolStatus::Pending | ToolStatus::Running => None,
        };
        Some(tool_shells::format_duration(start, end, now_millis()))
    }

    fn tool_description(
        &self,
        row: &ToolRow<'_>,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
    ) -> AnyElement {
        let text: Option<SharedString> = tool_shells::justification(row.tool, row.facts)
            .map(|text| SharedString::from(text.to_owned()))
            .or_else(|| {
                tool_shells::description(row.tool, row.facts)
                    .map(|described| describe(&described, row.directory, strings).into())
            });
        // Opened, the body carries the whole error.
        let failure = (row.failed() && !row.expanded)
            .then(|| {
                row.facts
                    .error
                    .clone()
                    .or_else(|| row.output.map(str::to_owned))
            })
            .flatten()
            .filter(|failure| !failure.is_empty());
        let stats = tool_shells::diff_stats(row.tool, &row.facts.metadata);
        let written = tool_shells::write_line_count(row.tool, &row.facts.input);
        div()
            .flex()
            .flex_row_from(direction)
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(px(4.0))
            .text_color(palette.tools_description)
            .when_some(text.clone(), |this, text| {
                this.child(div().min_w_0().truncate().child(text))
            })
            .when_some(failure, |this, failure| {
                this.when(text.is_some(), |this| {
                    this.child(
                        div()
                            .flex_shrink_0()
                            .text_color(palette.muted_foreground.opacity(0.6))
                            .child("·"),
                    )
                })
                .child(
                    div()
                        .flex_shrink_0()
                        .max_w(gpui_kit::relative(0.5))
                        .truncate()
                        .text_color(palette.status_error)
                        .child(failure),
                )
            })
            .when_some(stats, |this, (added, removed)| {
                this.child(
                    div()
                        .flex()
                        .flex_shrink_0()
                        .text_size(rems(0.8))
                        .child(
                            div()
                                .text_color(palette.status_success)
                                .child(format!("+{added}")),
                        )
                        .child(div().text_color(palette.tools_description).child("/"))
                        .child(
                            div()
                                .text_color(palette.status_error)
                                .child(format!("-{removed}")),
                        ),
                )
            })
            .when_some(written, |this, lines| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .text_size(rems(0.8))
                        .text_color(palette.status_success)
                        .child(format!("+{lines}")),
                )
            })
            .into_any_element()
    }

    fn tool_body(
        &self,
        row: &ToolRow<'_>,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        copied: Option<Entity<bool>>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let column = div().flex().flex_col().gap_2();
        if row.elided.is_some() {
            return match row.hydration {
                Some(HydrationView::Failed) => {
                    let (session, message) = (row.session.clone(), row.message.clone());
                    column
                        .child(
                            div()
                                .flex()
                                .flex_row_from(direction)
                                .items_center()
                                .gap_2()
                                .text_size(rems(META_SIZE))
                                .text_color(palette.muted_foreground)
                                .child(strings.get("chat.toolPart.content.loadFailed"))
                                .child(text_button(
                                    format!("tool:{}:retry", row.part).into(),
                                    strings.get("chat.toolPart.content.retry"),
                                    palette,
                                    cx,
                                    move |this, _, cx| this.retry_hydration(&session, &message, cx),
                                )),
                        )
                        .into_any_element()
                }
                Some(HydrationView::Loading { slow: true }) => column
                    .child(meta_text(
                        strings.get("chat.toolPart.content.loading"),
                        palette.muted_foreground,
                    ))
                    .into_any_element(),
                _ => column.into_any_element(),
            };
        }
        let mono = Theme::global(cx).mono_font_family.clone();
        let tool = row.tool;
        let mut column = column;
        if tool_shells::is_execute(tool) {
            if let Some(code) = row.facts.input.code.as_deref() {
                column = column.child(scroll_box(
                    format!("tool:{}:script", row.part),
                    INPUT_MAX_HEIGHT,
                    code_text(code, &mono, palette.muted_foreground),
                ));
            }
            if !row.facts.metadata.tool_calls.is_empty() {
                let mut calls = div().flex().flex_col().gap(px(2.0));
                for call in &row.facts.metadata.tool_calls {
                    let failed = call.status.as_deref() == Some("error");
                    calls = calls.child(
                        div()
                            .flex()
                            .flex_row_from(direction)
                            .items_baseline()
                            .gap_2()
                            .min_w_0()
                            .text_size(rems(META_SIZE))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .font_family(mono.clone())
                                    .text_color(if failed {
                                        palette.status_error
                                    } else {
                                        palette.foreground
                                    })
                                    .child(call.tool.clone()),
                            )
                            .when_some(
                                call.status
                                    .clone()
                                    .filter(|status| status != "error" && status != "completed"),
                                |this, status| {
                                    this.child(
                                        div()
                                            .flex_shrink_0()
                                            .text_color(palette.muted_foreground)
                                            .child(status),
                                    )
                                },
                            )
                            .when_some(call.input.clone(), |this, input| {
                                this.child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(palette.muted_foreground)
                                        .child(input),
                                )
                            }),
                    );
                }
                column = column.child(
                    div()
                        .child(label(strings.get("chat.toolPart.scriptCalls"), palette))
                        .child(calls),
                );
            }
        }
        if let Some(input) = tool_shells::input_text(tool, row.facts) {
            let input = if tool_names::normalize(tool) == "write" {
                added_lines(&input, &mono, palette)
            } else if tool_shells::is_subagent(tool) {
                div()
                    .text_size(rems(META_SIZE))
                    .text_color(palette.muted_foreground)
                    .child(input.into_owned())
                    .into_any_element()
            } else {
                code_text(&input, &mono, palette.muted_foreground)
            };
            column = column.child(scroll_box(
                format!("tool:{}:input", row.part),
                INPUT_MAX_HEIGHT,
                input,
            ));
        }
        if tool_shells::is_question(tool) {
            column = column.child(match row.status {
                ToolStatus::Completed if !row.output().trim().is_empty() => output_box(
                    row,
                    row.output(),
                    RESULT_MAX_HEIGHT,
                    &mono,
                    strings,
                    palette,
                    direction,
                    copied,
                    cx,
                ),
                ToolStatus::Error => error_box(row, strings, palette),
                _ => meta_text(
                    strings.get("chat.toolPart.awaitingResponse"),
                    palette.muted_foreground,
                ),
            });
            return column.into_any_element();
        }
        let output = row.output();
        let shows_result = row.status == ToolStatus::Completed
            || (tool_shells::is_shell(tool) && !output.trim().is_empty());
        if shows_result {
            column =
                column.child(self.tool_result(row, &mono, strings, palette, direction, copied, cx));
            if row.facts.metadata.truncated {
                let note = strings.get("chat.toolPart.outputTruncated");
                column = column.child(meta_text(
                    match &row.facts.metadata.output_path {
                        Some(path) => format!("{note} \u{2014} {path}").into(),
                        None => note,
                    },
                    palette.muted_foreground,
                ));
            }
        }
        if row.failed() && (row.facts.error.is_some() || !shows_result) {
            column = column.child(error_box(row, strings, palette));
        }
        column.into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn tool_result(
        &self,
        row: &ToolRow<'_>,
        mono: &SharedString,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        copied: Option<Entity<bool>>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let tool = row.tool;
        let output = row.output();
        if tool_shells::is_subagent(tool) && !output.is_empty() {
            return scroll_box(
                format!("tool:{}:result", row.part),
                RESULT_MAX_HEIGHT,
                markdown_view::markdown(
                    &format!("tool:{}:markdown", row.part),
                    output,
                    palette,
                    Author::Assistant,
                )
                .text_size(rems(META_SIZE)),
            );
        }
        let diffs: Vec<_> = row
            .facts
            .metadata
            .files
            .iter()
            .filter_map(|file| Some((file.file.as_str(), file.patch.as_deref()?)))
            .chain(
                row.facts
                    .metadata
                    .patch
                    .as_deref()
                    .filter(|_| row.facts.metadata.files.is_empty())
                    .map(|patch| (row.facts.input.path.as_deref().unwrap_or_default(), patch)),
            )
            .collect();
        if tool_shells::is_file_change(tool) && !diffs.is_empty() {
            let mut files = div().flex().flex_col().gap_3();
            for (file, patch) in diffs {
                files = files.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(rems(META_SIZE))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(palette.muted_foreground)
                                .truncate()
                                .child(
                                    tool_shells::relative_path(file, row.directory).into_owned(),
                                ),
                        )
                        .child(plain_diff(patch, mono, palette)),
                );
            }
            return scroll_box(format!("tool:{}:diff", row.part), RESULT_MAX_HEIGHT, files);
        }
        if tool_names::normalize(tool) == "write" {
            return div().into_any_element();
        }
        if !output.trim().is_empty() {
            let height = if tool_shells::is_shell(tool) {
                SHELL_OUTPUT_MAX_HEIGHT
            } else {
                RESULT_MAX_HEIGHT
            };
            return output_box(
                row, output, height, mono, strings, palette, direction, copied, cx,
            );
        }
        meta_text(
            strings.get("chat.toolPart.noOutputProduced"),
            palette.muted_foreground,
        )
    }

    /// Opens or folds a tool row; opening a row whose result a history page
    /// left out reads the whole message.
    pub(crate) fn toggle_tool_row(
        &mut self,
        session: &SessionId,
        message: &MessageId,
        part: &PartId,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.open_sessions.get_mut(session) else {
            return;
        };
        if !open.parts.is_expanded(part) {
            crate::transcript_view::hold_position_for_expand(&open.list);
        }
        let opened = open.parts.toggle_tool(part);
        let elided = open
            .transcript
            .row_of(message)
            .and_then(|row| open.transcript.message(row))
            .and_then(|held| held.parts.iter().find(|candidate| &candidate.id == part))
            .is_some_and(|held| {
                matches!(
                    held.content,
                    PartContent::Tool {
                        elided: Some(_),
                        ..
                    }
                )
            });
        if opened && elided && open.parts.needs_hydration(message) {
            self.hydrate_message(session, message, cx);
        }
        self.remeasure_message(session, message);
        cx.notify();
    }

    pub(crate) fn retry_hydration(
        &mut self,
        session: &SessionId,
        message: &MessageId,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.open_sessions.get_mut(session) else {
            return;
        };
        open.parts.clear_failure(message);
        self.hydrate_message(session, message, cx);
        self.remeasure_message(session, message);
        cx.notify();
    }

    /// Reads a whole message for the results its page left out.
    fn hydrate_message(
        &mut self,
        session: &SessionId,
        message: &MessageId,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let generation = connection.generation;
        let runtime = self.runtime.clone();
        let (request_session, request_message) = (session.clone(), message.clone());
        let task = connection.network.run(async move {
            client
                .session_message(&request_session, &request_message)
                .await
        });
        let (session_id, message_id) = (session.clone(), message.clone());
        let read = cx.spawn(async move |this, cx| {
            let result = task.await.unwrap_or(Err(
                openchamber_native_transport::error::TransportError::Cancelled,
            ));
            let _ = this.update(cx, |this, cx| {
                if this.runtime != runtime
                    || !this
                        .connection
                        .as_ref()
                        .is_some_and(|connection| connection.generation == generation)
                {
                    return;
                }
                let Some(open) = this.open_sessions.get_mut(&session_id) else {
                    return;
                };
                match result {
                    Ok(whole) => {
                        let change = open.transcript.complete_elided(&whole);
                        open.parts.finish_hydration(&message_id, true);
                        apply_list_change(open, &change, cx);
                    }
                    Err(error) => {
                        let failure = ConnectionFailure::from_error(&error);
                        tracing::warn!(?failure, "a left-out tool result could not be read");
                        open.parts.finish_hydration(&message_id, false);
                    }
                }
                this.remeasure_message(&session_id, &message_id);
                cx.notify();
            });
        });
        let slow_session = session.clone();
        let slow_message = message.clone();
        let slow = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(crate::part_state::PROGRESS_DELAY)
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(open) = this.open_sessions.get_mut(&slow_session)
                    && open.parts.mark_slow(&slow_message)
                {
                    this.remeasure_message(&slow_session, &slow_message);
                    cx.notify();
                }
            });
        });
        if let Some(open) = self.open_sessions.get_mut(session) {
            open.parts.start_hydration(message.clone(), read, slow);
        }
    }

    /// Remeasures the row of `message` after its content changed height.
    pub(crate) fn remeasure_message(&self, session: &SessionId, message: &MessageId) {
        if let Some(open) = self.open_sessions.get(session)
            && let Some(row) = open.transcript.row_of(message)
        {
            open.list.remeasure_items(row..row + 1);
        }
    }
}

/// Localized text for a description; paths relative to the session
/// directory (`describeTool`).
fn describe(described: &ToolDescription, directory: &str, strings: &Strings) -> String {
    match described {
        ToolDescription::Path(path) => tool_shells::relative_path(path, directory).into_owned(),
        ToolDescription::Text(text) => text.clone(),
        ToolDescription::Questions(count) => strings
            .format(
                "chat.toolPart.questionsAsked",
                &[("count", &count.to_string())],
            )
            .to_string(),
        ToolDescription::Files(count) => strings
            .format("chat.toolPart.filesCount", &[("count", &count.to_string())])
            .to_string(),
        ToolDescription::Tools { calls, overflow } => {
            let named: Vec<String> = calls
                .iter()
                .map(|(name, count)| {
                    if *count > 1 {
                        format!("{name} \u{d7}{count}")
                    } else {
                        name.clone()
                    }
                })
                .collect();
            let named = named.join(", ");
            if *overflow > 0 {
                let more = strings.format(
                    "chat.toolPart.moreToolCalls",
                    &[("count", &overflow.to_string())],
                );
                format!("{named}, {more}")
            } else {
                named
            }
        }
    }
}

fn meta_text(text: SharedString, color: Hsla) -> AnyElement {
    div()
        .text_size(rems(META_SIZE))
        .text_color(color)
        .child(text)
        .into_any_element()
}

fn label(text: SharedString, palette: &Palette) -> AnyElement {
    div()
        .mb_1()
        .text_size(rems(META_SIZE))
        .font_weight(FontWeight::MEDIUM)
        .text_color(palette.muted_foreground)
        .child(text)
        .into_any_element()
}

/// Code-like text: mono, wrapping, left to right in every locale.
fn code_text(text: &str, mono: &SharedString, color: Hsla) -> AnyElement {
    div()
        .font_family(mono.clone())
        .text_size(rems(META_SIZE))
        .text_color(color)
        .whitespace_normal()
        .child(text.to_owned())
        .into_any_element()
}

fn scroll_box(
    id: impl Into<SharedString>,
    max_height: f32,
    content: impl IntoElement,
) -> AnyElement {
    div()
        .id(ElementId::Name(id.into()))
        .max_h(px(max_height))
        .overflow_y_scroll()
        .child(content)
        .into_any_element()
}

/// The result text with its copy action.
#[allow(clippy::too_many_arguments)]
fn output_box(
    row: &ToolRow<'_>,
    output: &str,
    max_height: f32,
    mono: &SharedString,
    strings: &Strings,
    palette: &Palette,
    direction: Direction,
    copied: Option<Entity<bool>>,
    cx: &Context<Workspace>,
) -> AnyElement {
    let Some(copied) = copied else {
        return code_text(output, mono, palette.foreground.opacity(0.9));
    };
    let is_copied = *copied.read(cx);
    let text = output.to_owned();
    let copy = IconButton::new(
        format!("tool:{}:copy", row.part),
        if is_copied { "check" } else { "file-copy" },
        strings.get(if is_copied {
            "chat.toolPart.copiedOutput"
        } else {
            "chat.toolPart.copyOutput"
        }),
    )
    .size(24.0, 14.0)
    .render(palette, cx, move |_, window, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        copied.update(cx, |copied, _| *copied = true);
        window.refresh();
        let copied = copied.clone();
        cx.spawn(async move |_, cx| {
            cx.background_executor().timer(COPIED_FOR).await;
            cx.update(|cx: &mut App| {
                copied.update(cx, |copied, _| *copied = false);
                cx.refresh_windows();
            });
        })
        .detach();
    });
    div()
        .relative()
        .child(scroll_box(
            format!("tool:{}:output", row.part),
            max_height,
            code_text(output, mono, palette.foreground.opacity(0.9)),
        ))
        .child(
            div()
                .absolute()
                .top_0()
                .inset_end(direction, px(0.0))
                .child(copy),
        )
        .into_any_element()
}

const COPIED_FOR: std::time::Duration = std::time::Duration::from_secs(2);

fn error_box(row: &ToolRow<'_>, strings: &Strings, palette: &Palette) -> AnyElement {
    let message = row
        .facts
        .error
        .clone()
        .or_else(|| row.output.map(str::to_owned))
        .unwrap_or_default();
    div()
        .child(label(strings.get("chat.toolPart.error"), palette))
        .child(
            div()
                .p_2()
                .rounded(px(12.0))
                .bg(palette.status_error_background)
                .text_size(rems(META_SIZE))
                .text_color(palette.status_error)
                .child(message),
        )
        .into_any_element()
}

/// A patch as plain colored lines (`PlainDiffFallback`), from its first hunk:
/// the file headers repeat the path the entry's title already shows.
fn plain_diff(patch: &str, mono: &SharedString, palette: &Palette) -> AnyElement {
    let mut lines = div()
        .flex()
        .flex_col()
        .font_family(mono.clone())
        .text_size(rems(META_SIZE))
        .rounded(px(8.0))
        .overflow_hidden();
    let body = patch
        .find("\n@@")
        .map_or(patch, |start| &patch[start + 1..]);
    for line in body.lines() {
        let (background, color) = match line.as_bytes().first() {
            Some(b'+') if !line.starts_with("+++") => (
                Some(palette.status_success.opacity(0.12)),
                palette.foreground,
            ),
            Some(b'-') if !line.starts_with("---") => {
                (Some(palette.status_error.opacity(0.12)), palette.foreground)
            }
            Some(b'@') => (None, palette.info),
            _ => (None, palette.muted_foreground),
        };
        lines = lines.child(
            div()
                .px_2()
                .when_some(background, |this, background| this.bg(background))
                .text_color(color)
                .child(if line.is_empty() {
                    " ".to_owned()
                } else {
                    line.to_owned()
                }),
        );
    }
    lines.into_any_element()
}

/// A `write` call's content as an all-added file (`buildWritePreviewPatch`).
fn added_lines(content: &str, mono: &SharedString, palette: &Palette) -> AnyElement {
    let mut lines = div()
        .flex()
        .flex_col()
        .font_family(mono.clone())
        .text_size(rems(META_SIZE))
        .rounded(px(8.0))
        .overflow_hidden();
    for line in content.replace("\r\n", "\n").split('\n') {
        lines = lines.child(
            div()
                .px_2()
                .bg(palette.status_success.opacity(0.12))
                .text_color(palette.foreground)
                .child(format!("+{line}")),
        );
    }
    lines.into_any_element()
}

/// A small text button (`Button variant="ghost" size="xs"`).
fn text_button(
    id: SharedString,
    text: SharedString,
    palette: &Palette,
    cx: &Context<Workspace>,
    action: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) -> AnyElement {
    on_activate(
        div()
            .id(ElementId::Name(id.clone()))
            .role(Role::Button)
            .accessibility_id(id)
            .aria_label(text.clone())
            .px_2()
            .h(px(24.0))
            .flex()
            .items_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .text_color(palette.foreground)
            .hover(|style| style.bg(palette.hover))
            .child(text),
        cx,
        action,
    )
    .test_support()
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execute_descriptions_count_repeats_and_overflow() {
        let strings = locale::Strings::load("en");
        let described = ToolDescription::Tools {
            calls: vec![("a".into(), 2), ("b".into(), 1)],
            overflow: 3,
        };
        assert_eq!(describe(&described, "", &strings), "a \u{d7}2, b, +3 more");
        assert_eq!(
            describe(
                &ToolDescription::Path("/repo/src/a.rs".into()),
                "/repo",
                &strings
            ),
            "src/a.rs"
        );
        assert_eq!(
            describe(&ToolDescription::Questions(2), "", &strings),
            "Questions asked: 2"
        );
    }
}
