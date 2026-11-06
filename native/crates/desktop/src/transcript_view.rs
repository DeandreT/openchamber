//! The transcript rows of the selected session.
//!
//! Each visited session keeps its own `ListState`, so its anchor message and
//! pixel offset survive switching away and back. Text deltas remeasure only
//! the affected row; tail following continues only while the reader is at
//! the end.

use gpui_kit::SharedString;
use gpui_kit::{
    AnyElement, Bounds, Context, ElementId, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Role, StatefulInteractiveElement, Styled, TestSupportExt, Window, canvas, div,
    fill, linear_color_stop, linear_gradient, list, point, prelude::FluentBuilder, px, rems, size,
};
use openchamber_native_core::ids::{MessageId, PartId, SessionId};
use openchamber_native_core::session::LiveStatus;
use openchamber_native_core::tool_shells::{self, ToolFacts, ToolInput};
use openchamber_native_core::transcript::{
    CompactionStatus, Message, Part, PartContent, Role as MessageRole, ToolStatus,
};

use crate::activation::on_activate;
use crate::chrome::{self, glyph};
use crate::connection::ConnectionFailure;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::reasoning_view::{self, Block, BlockLook};
use crate::theme::Palette;
use crate::tool_part_view::ToolRow;
use crate::transcript_clock::now_millis;
use crate::workspace::{History, OpenSession, Workspace};
use crate::{attachment_part_view, text_part_view};

pub(crate) const META_SIZE: f32 = 0.781_25;

impl Workspace {
    pub(crate) fn render_transcript(
        &self,
        palette: &Palette,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(open) = self
            .selected
            .as_ref()
            .and_then(|id| self.open_sessions.get(id))
        else {
            return div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .text_color(palette.muted_foreground)
                .child(locale::strings(cx).get("chat.chatInput.placeholder.selectSession"))
                .into_any_element();
        };
        open.selection
            .read(cx)
            .document
            .clone()
            .activate(window, cx);
        if let History::Failed(failure) = &open.history
            && open.transcript.is_empty()
        {
            let strings = locale::strings(cx);
            let description = match failure {
                ConnectionFailure::AuthenticationRequired
                | ConnectionFailure::AuthenticationRejected => {
                    strings.get("chat.container.sessionLoadError.authDescription")
                }
                _ => strings.get("chat.container.sessionLoadError.description"),
            };
            return div()
                .id("transcript-load-error")
                .role(Role::Alert)
                .accessibility_id("transcript-load-error")
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .gap_1()
                .items_center()
                .justify_center()
                .px_6()
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .child(strings.get("chat.container.sessionLoadError.title")),
                )
                .child(
                    div()
                        .max_w(px(420.0))
                        .text_center()
                        .text_size(rems(META_SIZE))
                        .text_color(palette.muted_foreground)
                        .child(description),
                )
                .into_any_element();
        }
        let following =
            open.list.is_following_tail() || open.list.is_scrolled_to_end().unwrap_or(true);
        let scroll_label = locale::strings(cx).get("chat.scrollToBottom.aria");
        let rail = self.render_prompt_rail(
            palette,
            direction,
            locale::strings(cx).get("chat.promptNavigator.aria"),
            window,
            cx,
        );
        let rows = list(
            open.list.clone(),
            cx.processor(|this, index: usize, window, cx| this.render_message(index, window, cx)),
        )
        .size_full();
        let list = open.list.clone();
        let composer_slot = self.composer_slot.clone();
        let background = palette.chat_background;
        // `index.css`: the fades sit above the transcript and below its controls.
        // Read the current scroll geometry during paint, including scroll-only frames.
        let fades = canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                let band = px(48.0);
                if list.scroll_px_offset_for_scrollbar().y < px(-1.0) {
                    window.paint_quad(fill(
                        Bounds::new(bounds.origin, size(bounds.size.width, band)),
                        linear_gradient(
                            180.0,
                            linear_color_stop(background, 0.0),
                            linear_color_stop(background.opacity(0.0), 1.0),
                        ),
                    ));
                }
                let height = px(composer_slot.get().height()) + band;
                window.paint_quad(fill(
                    Bounds::new(
                        point(bounds.left(), bounds.bottom() - height),
                        size(bounds.size.width, height),
                    ),
                    linear_gradient(
                        180.0,
                        linear_color_stop(background.opacity(0.0), 0.0),
                        linear_color_stop(background, 1.0),
                    ),
                ));
            },
        )
        .absolute()
        .inset_0();
        div()
            .id("transcript")
            .role(Role::Log)
            .accessibility_id("transcript")
            .absolute()
            .inset_0()
            .child(rows)
            .child(fades)
            .children(rail)
            .children(self.render_return_to_parent(palette, direction, cx))
            .when(!following, |this| {
                this.child(
                    div()
                        .absolute()
                        .bottom_3()
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            on_activate(
                                div()
                                    .id("scroll-to-bottom")
                                    .role(Role::Button)
                                    .accessibility_id("scroll-to-bottom")
                                    .aria_label(scroll_label)
                                    .size(px(32.0))
                                    .rounded_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(palette.elevated)
                                    .border_1()
                                    .border_color(palette.border)
                                    .cursor_pointer()
                                    .hover(|style| style.bg(palette.hover))
                                    .child(glyph("arrow-down-s", 16.0, palette.foreground)),
                                cx,
                                |this, _, cx| this.scroll_to_tail(cx),
                            )
                            .test_support(),
                        ),
                )
            })
            .test_support()
            .into_any_element()
    }

    fn render_message(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = cx.global::<Palette>().clone();
        let Some(open) = self
            .selected
            .as_ref()
            .and_then(|id| self.open_sessions.get(id))
        else {
            return div().into_any_element();
        };
        let open_len = open.transcript.len();
        let Some(message) = open.transcript.message(index) else {
            return div().into_any_element();
        };
        let direction = locale::strings(cx).direction();
        let session = self
            .selected
            .as_ref()
            .expect("an open transcript is selected");
        // A staged revert hides its cut and everything after it.
        let hidden = index >= self.visible_rows(session);
        let body = match message.info.role {
            _ if hidden => None,
            MessageRole::User => Some(
                div()
                    .flex()
                    .flex_col()
                    .child(text_part_view::user_message(
                        message,
                        &palette,
                        open.selection.read(cx),
                        cx,
                    ))
                    .child(self.render_user_actions(&message.info.id, &palette, direction, cx))
                    .into_any_element(),
            ),
            MessageRole::Assistant => Some(
                div()
                    .flex()
                    .flex_col()
                    .child(self.render_assistant_parts(
                        session, open, message, &palette, direction, window, cx,
                    ))
                    .children(
                        self.render_turn_footer(index, message, &palette, direction, window, cx),
                    )
                    .into_any_element(),
            ),
            MessageRole::Compaction => {
                Some(self.render_compaction(session, open, message, &palette, direction, cx))
            }
            MessageRole::Shell => Some(
                self.render_shell_command(session, open, message, &palette, direction, window, cx),
            ),
            // A background subagent's report; other synthetic records are plumbing.
            MessageRole::Synthetic => self.render_subagent_report(message, &palette, direction, cx),
            // `timelineRoles.ts`: plumbing, and switches the composer already shows.
            MessageRole::System
            | MessageRole::Skill
            | MessageRole::LocationSwitched
            | MessageRole::Idle
            | MessageRole::AgentSwitched
            | MessageRole::ModelSwitched => None,
        };
        // The last row, shown or not, carries the tail spacer that keeps the
        // transcript's end clear of the docked composer.
        let tail = (index + 1 == open_len).then(|| self.composer_slot.get().tail_inset());
        let Some(body) = body else {
            return div()
                .when_some(tail, |this, tail| this.h(px(tail)))
                .into_any_element();
        };
        // A message's own spacing (`MessageList`): 16 px above a user's
        // message, 8 px below an assistant's.
        let (above, below) = match message.info.role {
            MessageRole::Assistant => (0.0, 8.0),
            _ => (16.0, 0.0),
        };
        div()
            .id(ElementId::Name(message.info.id.as_str().to_owned().into()))
            .role(Role::Paragraph)
            .accessibility_id(format!("message:{}", message.info.id))
            .when(message.info.role == MessageRole::User, |row| {
                row.group(crate::branching_view::MESSAGE_GROUP)
            })
            .when(window.is_a11y_active(), |row| {
                row.aria_label(
                    open.selection
                        .read(cx)
                        .accessible_text(&message.info.id, cx),
                )
            })
            .w_full()
            .pt(px(above))
            .pb(px(below + tail.unwrap_or_default()))
            .flex()
            .justify_center()
            .child(
                div()
                    .w_full()
                    .max_w(px(chrome::CHAT_COLUMN_MAX_WIDTH))
                    .px(chrome::chat_column_padding(
                        window,
                        self.chat_area_width(window),
                    ))
                    .child(body),
            )
            .into_any_element()
    }
}

impl Workspace {
    /// An assistant message's parts, each drawn by its kind's module.
    #[allow(clippy::too_many_arguments)]
    fn render_assistant_parts(
        &self,
        session: &SessionId,
        open: &OpenSession,
        message: &Message,
        palette: &Palette,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut controls = self.subagent_controls(session, message, palette, cx);
        let live = self.session_streams(session) && message.info.completed.is_none();
        let directory = self
            .sessions
            .get(session)
            .and_then(|record| record.directory.as_deref())
            .unwrap_or_default();
        // The text parts' `my-1`.
        let mut column = div().py(px(4.0)).flex().flex_col().gap_2();
        for part in &message.parts {
            column = match &part.content {
                PartContent::Text { text } => column.child(text_part_view::assistant_text(
                    message,
                    &part.id,
                    text,
                    palette,
                    open.selection.read(cx),
                    cx,
                )),
                PartContent::Reasoning { text } => {
                    let streaming = live && part.time.end.is_none();
                    if !streaming && tool_shells::is_invisible(text) {
                        continue;
                    }
                    column.child(self.render_reasoning(
                        session, open, message, part, text, streaming, palette, direction, cx,
                    ))
                }
                PartContent::Tool {
                    tool,
                    status,
                    output,
                    elided,
                    facts,
                    ..
                } => column.child(self.render_tool_row(
                    ToolRow {
                        session,
                        message: &message.info.id,
                        part: &part.id,
                        tool,
                        status: *status,
                        output: output.as_deref(),
                        elided: *elided,
                        facts,
                        start: part.time.start,
                        end: part.time.end,
                        directory,
                        expanded: open.parts.is_expanded(&part.id),
                        hydration: open.parts.hydration(&message.info.id),
                        controls: controls.remove(&part.id),
                    },
                    palette,
                    direction,
                    window,
                    cx,
                )),
                PartContent::File { filename, .. } => {
                    column.child(attachment_part_view::file(filename, palette))
                }
                PartContent::Agent { name } => {
                    column.child(attachment_part_view::agent_mention(name, palette))
                }
                PartContent::Unsupported { .. } => column,
            };
        }
        column.into_any_element()
    }

    /// Whether the session's reply is still arriving on the live channel.
    fn session_streams(&self, session: &SessionId) -> bool {
        matches!(
            self.live_status.get(session),
            Some(LiveStatus::Busy | LiveStatus::Retry)
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn render_reasoning(
        &self,
        session: &SessionId,
        open: &OpenSession,
        message: &Message,
        part: &Part,
        text: &str,
        streaming: bool,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // `expandReasoningWhileStreaming` is off by default: a streaming
        // block stays folded until the reader opens it.
        let expanded = open.parts.block_open(&part.id, false);
        if streaming {
            self.tick_live_indicators(cx);
        }
        let summary = if streaming || expanded {
            SharedString::default()
        } else {
            open.parts.reasoning_summary(&part.id, text)
        };
        let body = if expanded {
            tool_shells::clean_reasoning(text)
        } else {
            String::new()
        };
        let strings = locale::strings(cx);
        let look = BlockLook {
            icon: "brain-ai-3",
            icon_color: None,
            title: strings.get("chat.reasoningTrace.thinking"),
            expand_label: strings.get("chat.reasoningTrace.expandAria"),
            collapse_label: strings.get("chat.reasoningTrace.collapseAria"),
            max_height: reasoning_view::REASONING_MAX_HEIGHT,
            prose: false,
        };
        let (session, message_id, part_id) =
            (session.clone(), message.info.id.clone(), part.id.clone());
        reasoning_view::render(
            Block {
                id: format!("reasoning:{}", part.id).into(),
                text: &body,
                summary,
                streaming,
                expanded,
                dots: reasoning_view::busy_dots(now_millis() / 1000),
            },
            look,
            palette,
            direction,
            cx,
            move |this, _, cx| this.toggle_block(&session, &message_id, &part_id, false, cx),
        )
    }

    /// A compaction reads like a thinking row whose body is the summary
    /// (`CompactionNotice`).
    fn render_compaction(
        &self,
        session: &SessionId,
        open: &OpenSession,
        message: &Message,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let status = message
            .info
            .compaction
            .unwrap_or(CompactionStatus::Completed);
        let running = status == CompactionStatus::Running;
        let failed = status == CompactionStatus::Failed;
        let summary = message.info.notice.as_deref().unwrap_or_default().trim();
        let block_id = PartId::from(message.info.id.as_str());
        if running {
            self.tick_live_indicators(cx);
        }
        let strings = locale::strings(cx);
        let title = strings.get(match status {
            CompactionStatus::Running => "chat.compaction.running",
            CompactionStatus::Failed => "chat.compaction.failed",
            CompactionStatus::Completed => "chat.compaction.completed",
        });
        let icon = if failed { "error-warning" } else { "scissors" };
        let icon_color = failed.then_some(palette.status_error);
        // The compaction opens a turn; its row spaces itself above (`pt-1`).
        let row = div().pt_1();
        if !running && summary.is_empty() {
            return row
                .child(
                    div()
                        .flex()
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(6.0))
                        .py(px(6.0))
                        .padding_start(direction, px(1.0))
                        .text_size(rems(META_SIZE))
                        .child(glyph(icon, 14.0, icon_color.unwrap_or(palette.tools_icon)))
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(palette.tools_title)
                                .child(title),
                        ),
                )
                .into_any_element();
        }
        let expanded = open.parts.block_open(&block_id, false);
        let look = BlockLook {
            icon,
            icon_color,
            title,
            expand_label: strings.get("chat.compaction.showSummary"),
            collapse_label: strings.get("chat.compaction.hideSummary"),
            max_height: COMPACTION_MAX_HEIGHT,
            prose: true,
        };
        let block = Block {
            id: format!("compaction:{}", message.info.id).into(),
            text: summary,
            summary: if running || expanded {
                SharedString::default()
            } else {
                open.parts.reasoning_summary(&block_id, summary)
            },
            streaming: running,
            expanded,
            dots: reasoning_view::busy_dots(now_millis() / 1000),
        };
        let (session, message_id) = (session.clone(), message.info.id.clone());
        row.child(reasoning_view::render(
            block,
            look,
            palette,
            direction,
            cx,
            move |this, _, cx| this.toggle_block(&session, &message_id, &block_id, false, cx),
        ))
        .into_any_element()
    }

    /// A `!command` run, shown as the shell tool the agent would have called.
    #[allow(clippy::too_many_arguments)]
    fn render_shell_command(
        &self,
        session: &SessionId,
        open: &OpenSession,
        message: &Message,
        palette: &Palette,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let facts = ToolFacts {
            input: ToolInput {
                command: message.info.notice.clone(),
                ..ToolInput::default()
            },
            ..ToolFacts::default()
        };
        let part = PartId::from(format!("{}:shell", message.info.id));
        let finished = message.info.completed.is_some();
        self.render_tool_row(
            ToolRow {
                session,
                message: &message.info.id,
                part: &part,
                tool: "shell",
                status: if finished {
                    ToolStatus::Completed
                } else {
                    ToolStatus::Running
                },
                output: None,
                elided: None,
                facts: &facts,
                start: Some(message.info.created),
                end: message.info.completed,
                directory: "",
                expanded: open.parts.is_expanded(&part),
                hydration: None,
                controls: None,
            },
            palette,
            direction,
            window,
            cx,
        )
    }

    /// Opens or folds a reasoning or compaction block.
    pub(crate) fn toggle_block(
        &mut self,
        session: &SessionId,
        message: &MessageId,
        block: &PartId,
        default: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.open_sessions.get_mut(session) else {
            return;
        };
        open.parts.toggle_block(block, default);
        self.remeasure_message(session, message);
        cx.notify();
    }
}

/// The compaction summary's `max-h-[60vh]` of a typical window.
const COMPACTION_MAX_HEIGHT: f32 = 520.0;
