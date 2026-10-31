//! The queued messages strip, the composer's top row while a turn runs
//! (`QueuedMessagesStrip.tsx`). One message is its own row; several fold
//! into a count that opens in place into a list reordered by dragging,
//! four rows tall before it scrolls.

use gpui_kit::{
    AnyElement, App, AppContext, Context, InteractiveElement, IntoElement, ParentElement, Render,
    Role, SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window, div,
    prelude::FluentBuilder, px,
};
use openchamber_native_core::message_queue::{QueuedItemId, QueuedMessage};
use openchamber_native_core::send_config;

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// `.oc-composer-row`.
const ROW_HEIGHT: f32 = 32.0;
/// `max-h-40`.
const LIST_MAX_HEIGHT: f32 = 160.0;
/// `text-sm` and `typography-ui-label`, in rems.
const TEXT_SIZE: f32 = 0.875;
const TEXT_LINE: f32 = 1.25;
const ACTION_SIZE: f32 = 0.75;
const ACTION_LINE: f32 = 1.0;

/// A queued message being dragged.
#[derive(Clone)]
pub(crate) struct QueuedDrag(pub QueuedItemId);

/// What a drag shows under the pointer: the dragged item's text.
pub(crate) struct DragLabel {
    pub label: SharedString,
    pub background: gpui_kit::Hsla,
    pub foreground: gpui_kit::Hsla,
}

impl Render for DragLabel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(2.0))
            .rounded(px(6.0))
            .bg(self.background)
            .text_color(self.foreground)
            .opacity(0.6)
            .child(self.label.clone())
    }
}

impl Workspace {
    /// The strip, when the selected session has queued messages.
    pub(crate) fn render_queue_strip(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let queue = self.selected_queue();
        if queue.is_empty() || self.is_synthetic() {
            return None;
        }
        let strings = locale::strings(cx);
        let title = strings.get("chat.queuedMessage.title");
        let strip = div()
            .id("queued-strip")
            .role(Role::Group)
            .accessibility_id("queued-strip")
            .aria_label(title.clone())
            .border_b_1()
            .border_color(palette.border.opacity(0.6));
        if let [message] = queue {
            return Some(
                strip
                    .child(self.queued_row(message, false, palette, direction, cx))
                    .into_any_element(),
            );
        }
        let expanded = self.queue.expanded;
        let header = on_activate(
            div()
                .id("queued-toggle")
                .role(Role::Button)
                .accessibility_id("queued-toggle")
                .aria_label(format!("{title} {}", queue.len()))
                .when(expanded, |this| this.aria_expanded(true))
                .when(!expanded, |this| this.aria_expanded(false))
                .h(px(ROW_HEIGHT))
                .padding_start(direction, px(12.0))
                .padding_end(direction, px(12.0))
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .cursor_pointer()
                .text_color(palette.muted_foreground)
                .hover(|style| style.text_color(palette.foreground))
                .on_mouse_down(gpui_kit::MouseButton::Left, |_, window, _| {
                    window.prevent_default()
                })
                .child(glyph("time", 14.0, palette.muted_foreground))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .css_text(TEXT_SIZE, TEXT_LINE, cx)
                        .child(format!("{title} {}", queue.len())),
                )
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
            |this, _, cx| {
                this.queue.expanded = !this.queue.expanded;
                cx.notify();
            },
        )
        .test_support();
        Some(
            strip
                .child(header)
                .when(expanded, |this| {
                    this.child(
                        div()
                            .id("queued-list")
                            .max_h(px(LIST_MAX_HEIGHT))
                            .overflow_y_scroll()
                            .children(queue.iter().map(|message| {
                                self.queued_row(message, true, palette, direction, cx)
                            })),
                    )
                })
                .into_any_element(),
        )
    }

    /// One queued message: its lead (the queue glyph, or a drag handle in
    /// the list), its first line, the agent it was captured with when that
    /// differs from the composer's, and edit, send and remove.
    fn queued_row(
        &self,
        message: &QueuedMessage,
        sortable: bool,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let id = message.id.clone();
        let key = id.as_str().to_owned();
        let row_id = SharedString::from(format!("queued-row-{key}"));
        let preview = message.preview();
        let line = if preview.is_empty() {
            strings.get("chat.queuedMessage.empty")
        } else {
            preview.into()
        };
        let action = |name: &str, icon: &'static str, label: SharedString, palette: &Palette| {
            div()
                .id(SharedString::from(format!("queued-{name}-{key}")))
                .role(Role::Button)
                .accessibility_id(SharedString::from(format!("queued-{name}-{key}")))
                .aria_label(label.clone())
                .h(px(28.0))
                .px(px(6.0))
                .flex_shrink_0()
                .flex_row_from(direction)
                .items_center()
                .gap(px(4.0))
                .cursor_pointer()
                .text_color(palette.muted_foreground)
                .hover(|style| style.text_color(palette.foreground))
                // A press here never takes focus from the editor.
                .on_mouse_down(gpui_kit::MouseButton::Left, |_, window, _| {
                    window.prevent_default()
                })
                .child(glyph(
                    icon,
                    if icon == "close" { 16.0 } else { 14.0 },
                    palette.muted_foreground,
                ))
        };
        let captured_agent = message
            .send_config
            .as_ref()
            .and_then(|config| config.agent.clone())
            .filter(|agent| Some(agent) != self.composer_agent_name().as_ref());
        // Goal, scheduled and unjournalable messages stay for the server.
        let editable = message.takeable(crate::mutation_journal::MAX_URI);
        let lead = if sortable {
            div()
                .id(SharedString::from(format!("queued-handle-{key}")))
                .role(Role::Button)
                .accessibility_id(SharedString::from(format!("queued-handle-{key}")))
                .aria_label(strings.get("chat.queuedMessage.reorderAria"))
                .size(px(14.0))
                .flex_shrink_0()
                .cursor_grab()
                .child(glyph("draggable", 14.0, palette.muted_foreground))
                .into_any_element()
        } else {
            glyph("time", 14.0, palette.muted_foreground).into_any_element()
        };
        let drag_label = SharedString::from(line.to_string());
        let (drag_background, drag_foreground) = (palette.elevated, palette.foreground);
        let drop_target = id.clone();
        div()
            .id(row_id.clone())
            .role(Role::Group)
            .accessibility_id(row_id)
            .aria_label(line.clone())
            .h(px(ROW_HEIGHT))
            .min_w_0()
            .padding_start(direction, px(12.0))
            .padding_end(direction, px(6.0))
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.0))
            .when(sortable, |this| {
                this.on_drag(QueuedDrag(id.clone()), move |_, _, _, cx: &mut App| {
                    let label = drag_label.clone();
                    cx.new(|_| DragLabel {
                        label,
                        background: drag_background,
                        foreground: drag_foreground,
                    })
                })
                .drag_over::<QueuedDrag>(move |style, _, _, _| {
                    style.bg(palette_hover(drag_background))
                })
                .on_drop(cx.listener(move |this, drag: &QueuedDrag, _, cx| {
                    this.reorder_queued(&drag.0, &drop_target, cx);
                }))
            })
            .child(lead)
            .when(message.goal, |this| {
                this.child(glyph("target-fill", 14.0, palette.status_info))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .css_text(TEXT_SIZE, TEXT_LINE, cx)
                    .text_color(palette.foreground)
                    .child(line)
                    .when(!message.attachments.is_empty(), |this| {
                        this.child(
                            div()
                                .padding_start(direction, px(4.0))
                                .text_color(palette.muted_foreground)
                                .child(strings.format(
                                    "chat.queuedMessage.attachments",
                                    &[("count", &message.attachments.len().to_string())],
                                )),
                        )
                    }),
            )
            .when_some(captured_agent, |this, agent| {
                let (label, color) = self.queued_agent_badge(&agent, palette);
                let tooltip =
                    strings.format("chat.queuedMessage.sendsAsAgent", &[("agent", &label)]);
                this.child(
                    div()
                        .id(SharedString::from(format!("queued-agent-{key}")))
                        .aria_label(tooltip)
                        .max_w(gpui_kit::relative(0.4))
                        .min_w_0()
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(6.0))
                        .text_color(color)
                        .child(glyph("ai-agent", 14.0, color))
                        .child(
                            div()
                                .truncate()
                                .css_text(ACTION_SIZE, ACTION_LINE, cx)
                                .child(label),
                        ),
                )
            })
            .when(editable, |this| {
                let edit_id = id.clone();
                let send_id = id.clone();
                let edit_label = strings.get("chat.queuedMessage.edit");
                let send_label = strings.get("chat.queuedMessage.send");
                this.when(message.editable_as_text(), |this| {
                    this.child(
                        on_activate(
                            action("edit", "edit", edit_label.clone(), palette).child(
                                div()
                                    .css_text(ACTION_SIZE, ACTION_LINE, cx)
                                    .child(edit_label),
                            ),
                            cx,
                            move |this, window, cx| this.edit_queued(&edit_id, window, cx),
                        )
                        .test_support(),
                    )
                })
                .child(
                    on_activate(
                        action("send", "send-plane", send_label.clone(), palette).child(
                            div()
                                .css_text(ACTION_SIZE, ACTION_LINE, cx)
                                .child(send_label),
                        ),
                        cx,
                        move |this, window, cx| this.send_queued(&send_id, window, cx),
                    )
                    .test_support(),
                )
            })
            .child(
                on_activate(
                    action(
                        "remove",
                        "close",
                        strings.get("chat.queuedMessage.removeAria"),
                        palette,
                    ),
                    cx,
                    move |this, _, cx| this.remove_queued(&id, cx),
                )
                .test_support(),
            )
            .test_support()
            .into_any_element()
    }

    /// The agent the composer would send as now.
    fn composer_agent_name(&self) -> Option<String> {
        self.send_selection()
            .and_then(|(_, selection)| selection.agent_name)
    }

    /// A captured agent's label and color, as the composer's agent control
    /// draws them.
    fn queued_agent_badge(&self, name: &str, palette: &Palette) -> (String, gpui_kit::Hsla) {
        let agents = self
            .config_directory()
            .as_deref()
            .and_then(|directory| self.send_catalog.directory(directory))
            .and_then(|entry| entry.agents.ready());
        let Some(agents) = agents else {
            return (send_config::agent_name_label(name), palette.foreground);
        };
        let label = agents
            .iter()
            .find(|agent| agent.name == name)
            .map(send_config::agent_label)
            .unwrap_or_else(|| send_config::agent_name_label(name));
        let color = send_config::agent_color_with_palette(
            Some(name),
            agents,
            &palette.agent_palette,
            palette.agent_syntax.len(),
        );
        (label, palette.agent_color(&color))
    }
}

/// The drop target's fill while a dragged message is over it.
fn palette_hover(color: gpui_kit::Hsla) -> gpui_kit::Hsla {
    color.opacity(0.6)
}
