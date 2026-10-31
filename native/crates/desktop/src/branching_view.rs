//! What fork, revert, `/btw` and subagent navigation draw: a prompt's
//! Revert and Fork actions (`MessageBody.tsx`), the reverted strip at the top
//! of the composer (`RevertedMessagesStrip.tsx`), the `/btw` panel above it
//! (`BtwPanel.tsx`), a subagent call's Open and Stop controls (`ToolPart.tsx`)
//! and Return to parent (`ChatContainer.tsx`).

use std::collections::HashMap;

use gpui_kit::{
    AnyElement, Context, ElementId, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Role, SharedString, StatefulInteractiveElement, Styled, TestSupportExt, div,
    prelude::FluentBuilder, px,
};
use openchamber_native_core::branching;
use openchamber_native_core::ids::{MessageId, PartId, SessionId};
use openchamber_native_core::tool_names;
use openchamber_native_core::transcript::{Message, PartContent, Role as MessageRole, ToolStatus};

use gpui_kit::component::tooltip::Tooltip;

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{ButtonTone, IconButton, glyph};
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::markdown_view::{self, Author};
use crate::session_branching::BtwPanel;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// The group a user row's hover reveals its actions through.
pub(crate) const MESSAGE_GROUP: &str = "user-message";
const ROW_HEIGHT: f32 = 36.0;
const PANEL_MAX_HEIGHT: f32 = 240.0;
const LIST_MAX_HEIGHT: f32 = 160.0;

impl Workspace {
    /// Revert and Fork under a prompt; they appear on the row's hover and
    /// keep their place at rest, so the row never changes height.
    pub(crate) fn render_user_actions(
        &self,
        message: &MessageId,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let available = self.session_actions_available();
        let revert_id = message.clone();
        let fork_id = message.clone();
        div()
            .h(px(32.0))
            .pt(px(8.0))
            .flex_row_from(direction)
            .justify_end()
            .gap(px(6.0))
            .opacity(0.0)
            .group_hover(MESSAGE_GROUP, |style| style.opacity(1.0))
            .when(available, |row| {
                row.child(
                    IconButton::new(
                        format!("message-revert:{message}"),
                        "arrow-go-back",
                        strings.get("chat.messageBody.actions.revertAria"),
                    )
                    .size(26.0, 14.0)
                    .render(palette, cx, move |this, window, cx| {
                        this.revert_to_message(revert_id.clone(), window, cx)
                    }),
                )
                .child(
                    IconButton::new(
                        format!("message-fork:{message}"),
                        "git-branch",
                        strings.get("chat.messageBody.actions.forkAria"),
                    )
                    .size(26.0, 14.0)
                    .render(palette, cx, move |this, window, cx| {
                        this.fork_from_message(fork_id.clone(), window, cx)
                    }),
                )
            })
            .into_any_element()
    }

    /// The staged revert as the composer's top row: one reverted entry is
    /// its own row; several collapse into a count that expands in place.
    /// Restore stays on the header either way.
    pub(crate) fn render_revert_strip(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let session = self.selected.as_ref()?;
        let marker = self.revert_marker(session)?;
        let open = self.open_sessions.get(session)?;
        let entries = branching::reverted_entries(open.transcript.messages(), Some(&marker));
        if entries.is_empty() {
            return None;
        }
        let strings = locale::strings(cx);
        let fallback = strings.get("chat.revertPopover.noTextContent");
        let text_of = |preview: &Option<String>| -> SharedString {
            preview
                .clone()
                .map(SharedString::from)
                .unwrap_or_else(|| fallback.clone())
        };
        let key = self.mutation_key(session);
        let restoring = self.branching.restoring.contains(&key);
        let pending = self.branching.pending_reverts.contains_key(&key);
        let restore_label = strings.get(if entries.len() == 1 {
            "chat.revertPopover.restore"
        } else {
            "chat.revertPopover.restoreAll"
        });
        let enabled = !restoring && !pending && self.session_actions_available();
        let primary = palette.primary;
        let restore = {
            let button = div()
                .id("revert-restore")
                .accessibility_id("revert-restore")
                .role(Role::Button)
                .aria_label(restore_label.clone())
                .h(px(28.0))
                .px(px(8.0))
                .flex_shrink_0()
                .flex()
                .items_center()
                .rounded(px(6.0))
                .text_color(primary.opacity(0.8))
                .child(div().css_text(0.875, 1.25, cx).child(restore_label));
            if enabled {
                on_activate(
                    button
                        .cursor_pointer()
                        .hover(move |style| style.text_color(primary)),
                    cx,
                    |this, window, cx| this.restore_revert(window, cx),
                )
                .test_support()
                .into_any_element()
            } else {
                button
                    .opacity(0.5)
                    .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
                    .test_support()
                    .into_any_element()
            }
        };
        let row = || {
            div()
                .min_h(px(ROW_HEIGHT))
                .min_w_0()
                .padding_start(direction, px(12.0))
                .padding_end(direction, px(6.0))
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
        };
        let line = |text: SharedString| {
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .css_text(0.875, 1.25, cx)
                .text_color(palette.muted_foreground)
                .child(text)
        };
        let title = strings.get("chat.revertPopover.title");
        let strip = div()
            .id("revert-strip")
            .accessibility_id("revert-strip")
            .role(Role::Group)
            .aria_label(title)
            .w_full()
            .border_b_1()
            .border_color(palette.border.opacity(0.6))
            .flex()
            .flex_col()
            .test_support();
        if let [entry] = &entries[..] {
            return Some(
                strip
                    .child(
                        row()
                            .child(glyph("arrow-go-back", 14.0, palette.muted_foreground))
                            .child(line(text_of(&entry.preview)))
                            .child(restore),
                    )
                    .into_any_element(),
            );
        }
        let expanded = self.branching.strip_expanded.as_ref() == Some(&(key, marker));
        let count = entries.len().to_string();
        let header_label = strings.format("chat.revertPopover.staged", &[("count", &count)]);
        let toggle = on_activate(
            div()
                .id("revert-strip-toggle")
                .accessibility_id("revert-strip-toggle")
                .role(Role::Button)
                .aria_label(header_label.clone())
                .aria_expanded(expanded)
                .flex_1()
                .min_w_0()
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .cursor_pointer()
                .child(glyph("arrow-go-back", 14.0, palette.muted_foreground))
                .child(line(header_label))
                .child(glyph(
                    if expanded {
                        "arrow-up-s"
                    } else {
                        "arrow-down-s"
                    },
                    16.0,
                    palette.muted_foreground,
                )),
            cx,
            |this, _, cx| this.toggle_reverted_list(cx),
        )
        .test_support();
        let mut strip = strip.child(row().child(toggle).child(restore));
        if expanded {
            let mut list = div()
                .id("revert-strip-list")
                .accessibility_id("revert-strip-list")
                .max_h(px(LIST_MAX_HEIGHT))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .test_support();
            for entry in &entries {
                list = list.child(
                    row()
                        .id(ElementId::Name(format!("reverted:{}", entry.id).into()))
                        // Lines the text up with the header's label.
                        .child(div().size(px(14.0)).flex_shrink_0())
                        .child(line(text_of(&entry.preview)))
                        .test_support(),
                );
            }
            strip = strip.child(list);
        }
        Some(strip.into_any_element())
    }

    /// The `/btw` panel above the composer: starting, open with the side
    /// conversation's own messages, or collapsed to one row.
    pub(crate) fn render_btw_panel(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let panel = self.btw_panel()?;
        let strings = locale::strings(cx);
        let frame = || {
            div()
                .id("btw-panel")
                .accessibility_id("btw-panel")
                .role(Role::Group)
                .mb(px(12.0))
                .w_full()
                .rounded(px(12.0))
                .bg(palette.elevated)
                .text_color(palette.elevated_foreground)
                .shadow(crate::elevation::composer(palette))
                .flex()
                .flex_col()
                .test_support()
        };
        let (fork, boundary, local) = match panel {
            BtwPanel::Creating => {
                return Some(
                    frame()
                        .aria_label(strings.get("chat.btw.loading"))
                        .child(
                            div()
                                .px(px(16.0))
                                .py(px(16.0))
                                .flex_row_from(direction)
                                .items_center()
                                .gap(px(8.0))
                                .text_color(palette.muted_foreground)
                                .child(glyph("loader-4", 16.0, palette.muted_foreground))
                                .child(
                                    div()
                                        .css_text(0.875, 1.25, cx)
                                        .child(strings.get("chat.btw.loading")),
                                ),
                        )
                        .into_any_element(),
                );
            }
            BtwPanel::Open {
                fork,
                boundary,
                local,
            } => (fork, boundary, local),
        };
        let title: SharedString = self
            .session(&fork)
            .map(|record| record.title.clone())
            .filter(|title| !title.trim().is_empty())
            .map(SharedString::from)
            .unwrap_or_else(|| strings.get("chat.btw.titleFallback"));
        let button = |id: &'static str, icon: &'static str, key: &str| {
            IconButton::new(id, icon, strings.get(key)).tone(ButtonTone::Quiet)
        };
        let toggle = if local.collapsed {
            button("btw-expand", "arrow-up-s", "chat.btw.expandAria").render(
                palette,
                cx,
                |this, _, cx| this.set_btw_collapsed(false, cx),
            )
        } else {
            button("btw-collapse", "arrow-down-s", "chat.btw.collapseAria").render(
                palette,
                cx,
                |this, _, cx| this.set_btw_collapsed(true, cx),
            )
        };
        let header = div()
            .h(px(40.0))
            .padding_start(direction, px(14.0))
            .padding_end(direction, px(8.0))
            .flex_row_from(direction)
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::MEDIUM)
                    .css_text(0.875, 1.25, cx)
                    .child(title.clone()),
            )
            .when(self.is_streaming(&fork) || local.sending, |this| {
                this.child(
                    div()
                        .css_text(0.75, 1.0, cx)
                        .text_color(palette.muted_foreground)
                        .child(strings.get("chat.btw.working")),
                )
            })
            .child(
                button("btw-promote", "external-link", "chat.btw.promoteAria").render(
                    palette,
                    cx,
                    |this, window, cx| this.promote_btw(window, cx),
                ),
            )
            .child(toggle)
            .child(
                button("btw-destroy", "delete-bin", "chat.btw.destroyAria").render(
                    palette,
                    cx,
                    |this, window, cx| this.destroy_btw(window, cx),
                ),
            );
        let mut panel = frame().aria_label(title).child(header);
        if !local.collapsed {
            let mut body = div()
                .id("btw-messages")
                .accessibility_id("btw-messages")
                .max_h(px(PANEL_MAX_HEIGHT))
                .overflow_y_scroll()
                .px(px(14.0))
                .pb(px(12.0))
                .flex()
                .flex_col()
                .gap(px(8.0))
                .test_support();
            if let Some(open) = self.open_sessions.get(&fork) {
                for message in branching::btw_tail(open.transcript.messages(), boundary.as_ref()) {
                    let author = match message.info.role {
                        MessageRole::User => Author::User,
                        MessageRole::Assistant => Author::Assistant,
                        _ => continue,
                    };
                    let text = message.plain_text();
                    if text.trim().is_empty() {
                        continue;
                    }
                    body = body.child(
                        div()
                            .id(ElementId::Name(
                                format!("btw-message:{}", message.info.id).into(),
                            ))
                            .accessibility_id(format!("btw-message:{}", message.info.id))
                            .role(Role::Paragraph)
                            .aria_label(text.clone())
                            .when(author == Author::User, |this| {
                                this.px(px(10.0))
                                    .py(px(6.0))
                                    .rounded(px(10.0))
                                    .bg(palette.user_message_background)
                            })
                            .child(markdown_view::markdown(
                                &format!("btw:{}", message.info.id),
                                &text,
                                palette,
                                author,
                            )),
                    );
                }
            }
            panel = panel.child(body);
        }
        Some(panel.into_any_element())
    }

    /// Open and Stop for each `subagent` call of an assistant message that
    /// names its child, keyed by part.
    pub(crate) fn subagent_controls(
        &self,
        session: &SessionId,
        message: &Message,
        palette: &Palette,
        cx: &Context<Self>,
    ) -> HashMap<PartId, AnyElement> {
        let strings = locale::strings(cx);
        let mut controls = HashMap::new();
        for part in &message.parts {
            let PartContent::Tool {
                tool,
                status,
                child: Some(child),
                ..
            } = &part.content
            else {
                continue;
            };
            if tool_names::normalize(tool) != "subagent" {
                continue;
            }
            let open_label = strings.format("chat.toolPart.openSubtask", &[("type", "Subagent")]);
            let open_child = child.clone();
            let mut row = div().flex().items_center().gap(px(4.0)).child(
                IconButton::new(
                    format!("subtask-open:{}", part.id),
                    "external-link",
                    open_label,
                )
                .size(20.0, 12.0)
                .render(palette, cx, move |this, window, cx| {
                    this.open_subtask(open_child.clone(), window, cx)
                }),
            );
            let stopping = self.branching.stopping.contains(&self.mutation_key(child));
            if *status == ToolStatus::Running && !stopping && self.session_actions_available() {
                let (parent, stop_child) = (session.clone(), child.clone());
                row = row.child(
                    IconButton::new(
                        format!("subtask-stop:{}", part.id),
                        "stop",
                        strings.get("chat.toolPart.subagent.stop"),
                    )
                    .size(20.0, 12.0)
                    .render(palette, cx, move |this, window, cx| {
                        this.stop_subagent(parent.clone(), stop_child.clone(), window, cx)
                    }),
                );
            }
            controls.insert(part.id.clone(), row.into_any_element());
        }
        controls
    }

    /// A background subagent's report: its description and Open.
    pub(crate) fn render_subagent_report(
        &self,
        message: &Message,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let facts = message.info.synthetic.as_ref()?;
        let run = facts.subagent.as_ref()?;
        let strings = locale::strings(cx);
        let agent = run.agent.clone().unwrap_or_else(|| "subagent".to_owned());
        let mut label = agent.clone();
        if let Some(first) = label.get_mut(0..1) {
            first.make_ascii_uppercase();
        }
        let description: SharedString = facts
            .description
            .clone()
            .filter(|text| !text.trim().is_empty())
            .unwrap_or(agent)
            .into();
        let child = run.child.clone();
        Some(
            div()
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .text_color(palette.muted_foreground)
                .child(glyph("ai-agent", 14.0, palette.muted_foreground))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .css_text(0.875, 1.25, cx)
                        .child(description),
                )
                .child(
                    IconButton::new(
                        format!("subtask-open:{}", message.info.id),
                        "external-link",
                        strings.format("chat.toolPart.openSubtask", &[("type", &label)]),
                    )
                    .size(20.0, 12.0)
                    .render(palette, cx, move |this, window, cx| {
                        this.open_subtask(child.clone(), window, cx)
                    }),
                )
                .into_any_element(),
        )
    }

    /// Return to parent, over the transcript's start corner.
    pub(crate) fn render_return_to_parent(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let parent = self.parent_session()?;
        let strings = locale::strings(cx);
        let label = strings.get("chat.container.returnToParent.label");
        let tooltip = if parent.title.trim().is_empty() {
            strings.get("chat.container.returnToParent.title")
        } else {
            strings.format(
                "chat.container.returnToParent.titleNamed",
                &[("title", &parent.title)],
            )
        };
        let hover = palette.hover;
        Some(
            div()
                .absolute()
                .top(px(12.0))
                .inset_start(direction, px(12.0))
                .child(
                    on_activate(
                        div()
                            .id("return-to-parent")
                            .accessibility_id("return-to-parent")
                            .role(Role::Button)
                            .aria_label(strings.get("chat.container.returnToParent.aria"))
                            .tooltip(move |window, cx| {
                                Tooltip::new(tooltip.clone()).build(window, cx)
                            })
                            .h(px(24.0))
                            .px(px(8.0))
                            .rounded(px(6.0))
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(4.0))
                            .bg(palette.background.opacity(0.95))
                            .border_1()
                            .border_color(palette.border)
                            .cursor_pointer()
                            .hover(move |style| style.bg(hover))
                            .child(glyph("arrow-left", 16.0, palette.foreground))
                            .child(div().css_text(0.75, 1.0, cx).child(label)),
                        cx,
                        |this, window, cx| this.return_to_parent(window, cx),
                    )
                    .test_support(),
                )
                .into_any_element(),
        )
    }
}
