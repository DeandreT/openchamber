//! The new-session view the window opens on: a heading, the project choice,
//! the composer and the starter prompts, centered in the chat column.
//!
//! Mirrors the draft state of the web client's chat container
//! (`packages/ui/src/components/chat/`), whose form sits a little above the
//! middle (`pb-[6vh]`).

use gpui_kit::component::WindowExt;
use gpui_kit::{
    AnyElement, App, AppContext, Context, InteractiveElement, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, TextAlign, Window, div,
    prelude::FluentBuilder, px,
};
use openchamber_native_core::draft_starters::{ResolvedStarter, StarterGroup, StarterLabel};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{self, glyph};
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::tracked_text::tracked_text;
use crate::workspace::Workspace;

const CHIP_HEIGHT: f32 = 24.0;
/// `text-3xl`, `typography-ui-label` and `typography-meta`, in rems.
const TITLE_SIZE: f32 = 1.875;
const TITLE_LINE: f32 = 2.25;
/// `tracking-tight`, in ems.
const TITLE_TRACKING: f32 = -0.025;
const META_SIZE: f32 = 0.781_25;
const META_LINE: f32 = 1.125;

impl Workspace {
    pub(crate) fn render_startup(
        &mut self,
        palette: &Palette,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let project_label = self.draft_project_label();
        let title = if let Some(project) = &project_label {
            strings.format(
                "chat.emptyState.draftTitleWithProject",
                &[("project", project.as_ref())],
            )
        } else {
            strings.get("chat.emptyState.draftTitle")
        };
        let emphasis = project_label.as_ref().and_then(|project| {
            title
                .find(project.as_ref())
                .map(|start| start..start + project.len())
        });
        // `pb-[6vh]`: six percent of the content's height.
        let lift = chrome::content_size(window).height * 0.06;
        let column_padding = chrome::chat_column_padding(window, self.chat_area_width(window));
        let project_chooser = self.render_project_chooser(palette, direction, window, cx);
        let composer = self.render_composer_box(palette, direction, window, cx);
        let hover_fill = palette.hover;
        let foreground = palette.foreground;
        let chips: Vec<gpui_kit::AnyElement> = if self.starters_visible() && !self.is_synthetic() {
            self.resolved_starters()
                .into_iter()
                .map(|chip| self.render_starter_chip(chip, palette, direction, cx))
                .collect()
        } else if self.starters_visible() {
            // Synthetic data has no server to create a session on.
            openchamber_native_core::draft_starters::BUILTIN_STARTERS
                .iter()
                .filter_map(|starter| {
                    openchamber_native_core::draft_starters::resolve(
                        &openchamber_native_core::draft_starters::StarterRef::command(starter.name),
                        openchamber_native_core::draft_starters::StarterGroup::Global,
                        false,
                        &Default::default(),
                    )
                })
                .map(|chip| self.render_starter_chip(chip, palette, direction, cx))
                .collect()
        } else {
            Vec::new()
        };
        let add_starter = on_activate(
            div()
                .id("starter-add")
                .group("starter-add")
                .role(Role::Button)
                .accessibility_id("starter-add")
                .aria_label(strings.get("chat.draftStarters.add"))
                .size(px(CHIP_HEIGHT))
                .flex_shrink_0()
                .rounded_full()
                .border_1()
                .border_color(palette.border)
                .bg(palette.elevated)
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(move |style| style.bg(hover_fill))
                .child(
                    glyph("add", 14.0, palette.muted_foreground)
                        .group_hover("starter-add", move |style| style.text_color(foreground)),
                ),
            cx,
            |this, window, cx| this.open_starter_picker(window, cx),
        )
        .test_support();
        div()
            .id("startup")
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .justify_center()
            .pb(lift)
            .child(
                div()
                    .w_full()
                    .pb(px(16.0))
                    .flex()
                    .flex_col()
                    .child(
                        div().px(column_padding).mb(px(28.0)).child(
                            div()
                                .id("startup-title")
                                .role(Role::Heading)
                                .accessibility_id("startup-title")
                                .aria_level(1)
                                .w_full()
                                .aria_label(title.clone())
                                .flex()
                                .justify_center()
                                .css_text(TITLE_SIZE, TITLE_LINE, cx)
                                .text_color(palette.foreground)
                                .child({
                                    let text =
                                        tracked_text(title, TITLE_TRACKING, TextAlign::Center);
                                    if let Some(range) = emphasis {
                                        text.emphasize(range)
                                    } else {
                                        text
                                    }
                                }),
                        ),
                    )
                    .child(
                        div()
                            .px(column_padding)
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .mb(px(6.0))
                                    .px(px(2.0))
                                    .flex_row_from(direction)
                                    .items_center()
                                    .child(project_chooser),
                            )
                            .child(composer),
                    )
                    .child(
                        div()
                            .mt(px(16.0))
                            .px(column_padding)
                            .flex_row_from(direction)
                            .flex_wrap()
                            .items_center()
                            .justify_center()
                            .gap(px(6.0))
                            .children(chips)
                            .when(self.starters_visible(), |this| this.child(add_starter)),
                    ),
            )
    }
}

/// A starter chip being dragged, by chip ID.
#[derive(Clone)]
struct StarterDrag(String);

/// The chip's corner actions: remove at the end, share at the start.
const CORNER_SIZE: f32 = 16.0;

impl Workspace {
    /// One starter chip: a click submits it, a drag reorders it within its
    /// group, and on hover a corner button removes it (not a shared one)
    /// and, on a project chip, moves it in or out of the repository's file.
    fn render_starter_chip(
        &self,
        chip: ResolvedStarter,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let label: SharedString = match &chip.label {
            StarterLabel::Key(key) => strings.get(key),
            StarterLabel::Text(text) => text.clone().into(),
        };
        let id = chip.id.clone();
        let group = format!("starter-{}", id.replace(':', "-"));
        let hover_fill = palette.hover;
        let foreground = palette.foreground;
        let activate_id = id.clone();
        let button = on_activate(
            div()
                .id(SharedString::from(group.clone()))
                .role(Role::Button)
                .accessibility_id(SharedString::from(group.clone()))
                .aria_label(label.clone())
                .when(chip.shared, |this| {
                    let tip = strings.get("chat.draftStarters.sharedTitle");
                    this.tooltip(move |window, cx| {
                        let tip = tip.clone();
                        gpui_kit::component::tooltip::Tooltip::element(move |_, _| tip.clone())
                            .build(window, cx)
                    })
                })
                .h(px(CHIP_HEIGHT))
                .px(px(10.0))
                .rounded_full()
                .border_1()
                .border_color(palette.border)
                .bg(palette.elevated)
                .flex_row_from(direction)
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .text_color(palette.muted_foreground)
                .hover(move |style| style.bg(hover_fill).text_color(foreground))
                .child(
                    glyph(chip.icon, 12.0, palette.muted_foreground)
                        .opacity(0.7)
                        .group_hover(group.clone(), move |style| {
                            style.opacity(1.0).text_color(foreground)
                        }),
                )
                .child(
                    div()
                        .css_text(META_SIZE, META_LINE, cx)
                        .child(label.clone()),
                ),
            cx,
            move |this, window, cx| this.activate_starter(&activate_id, window, cx),
        )
        .test_support();
        let corner = |name: &str, icon: &'static str, tip: SharedString, start: bool| {
            div()
                .id(SharedString::from(format!("{group}-{name}")))
                .role(Role::Button)
                .accessibility_id(SharedString::from(format!("{group}-{name}")))
                .aria_label(tip.clone())
                .absolute()
                .top(px(-6.0))
                .map(|this| {
                    if start {
                        this.inset_start(direction, px(-6.0))
                    } else {
                        this.inset_end(direction, px(-6.0))
                    }
                })
                .size(px(CORNER_SIZE))
                .rounded_full()
                .border_1()
                .border_color(palette.border)
                .bg(palette.elevated)
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .opacity(0.0)
                .group_hover(group.clone(), |style| style.opacity(1.0))
                .child(glyph(icon, 10.0, palette.muted_foreground))
        };
        let drag = StarterDrag(id.clone());
        let drag_label = label.clone();
        let (drag_background, drag_foreground) = (palette.elevated, palette.foreground);
        let drop_target = id.clone();
        let remove_id = id.clone();
        let share_id = id.clone();
        div()
            .id(SharedString::from(format!("{group}-slot")))
            .group(group.clone())
            .relative()
            .on_drag(drag, move |_, _, _, cx: &mut App| {
                let label = drag_label.clone();
                cx.new(|_| crate::queue_strip::DragLabel {
                    label,
                    background: drag_background,
                    foreground: drag_foreground,
                })
            })
            .on_drop(cx.listener(move |this, drag: &StarterDrag, _, cx| {
                this.reorder_starter(&drag.0, &drop_target, cx);
            }))
            .child(button)
            .when(chip.group == StarterGroup::Project, |this| {
                let key = if chip.shared {
                    "chat.draftStarters.makePersonal"
                } else {
                    "chat.draftStarters.share"
                };
                this.child(
                    on_activate(
                        corner(
                            "share",
                            if chip.shared { "user" } else { "team" },
                            strings.get(key),
                            true,
                        ),
                        cx,
                        move |this, _, cx| this.toggle_shared_starter(&share_id, cx),
                    )
                    .test_support(),
                )
            })
            .when(!chip.shared, |this| {
                this.child(
                    on_activate(
                        corner(
                            "remove",
                            "close",
                            strings.get("chat.draftStarters.remove"),
                            false,
                        ),
                        cx,
                        move |this, _, cx| this.remove_starter(&remove_id, cx),
                    )
                    .test_support(),
                )
            })
            .into_any_element()
    }

    /// The add picker: the built-ins not pinned yet, under their section.
    pub(crate) fn open_starter_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let Some(this) = workspace.upgrade() else {
                return dialog;
            };
            let pinnable = this.read(cx).pinnable_starters();
            let palette = cx.global::<Palette>().clone();
            let strings = locale::strings(cx).clone();
            let direction = strings.direction();
            let hover = palette.hover;
            let body = if pinnable.is_empty() {
                div()
                    .text_color(palette.muted_foreground)
                    .child(strings.get("chat.draftStarters.empty"))
                    .into_any_element()
            } else {
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px(px(8.0))
                            .py(px(4.0))
                            .text_color(palette.muted_foreground)
                            .css_text(META_SIZE, META_LINE, cx)
                            .child(strings.get("chat.draftStarters.sectionBuiltIn")),
                    )
                    .children(pinnable.into_iter().map(|starter| {
                        let label =
                            openchamber_native_core::draft_starters::built_in(&starter.name)
                                .map_or_else(
                                    || starter.name.clone().into(),
                                    |built_in| strings.get(built_in.label_key),
                                );
                        let id = SharedString::from(format!("starter-pick-{}", starter.name));
                        let workspace = workspace.clone();
                        div()
                            .id(id.clone())
                            .role(Role::Button)
                            .accessibility_id(id)
                            .aria_label(label.clone())
                            .h(px(32.0))
                            .px(px(8.0))
                            .rounded(px(6.0))
                            .flex_row_from(direction)
                            .items_center()
                            .cursor_pointer()
                            .hover(move |style| style.bg(hover))
                            .child(div().truncate().child(label))
                            .on_click(move |_, window, cx| {
                                let _ = workspace
                                    .update(cx, |this, cx| this.add_starter(starter.clone(), cx));
                                window.close_dialog(cx);
                            })
                            .test_support()
                    }))
                    .into_any_element()
            };
            dialog
                .w(px(384.0))
                .title(strings.get("chat.draftStarters.add"))
                .child(body)
        });
    }
}
