//! The new-session view the window opens on: a heading, the project choice,
//! the composer and the starter prompts, centered in the chat column.
//!
//! Mirrors the draft state of the web client's chat container
//! (`packages/ui/src/components/chat/`), whose form sits a little above the
//! middle (`pb-[6vh]`).

use gpui_kit::{
    Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Role,
    StatefulInteractiveElement, Styled, TestSupportExt, TextAlign, Window, div, px,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{self, CONTROL_RADIUS, glyph};
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
const LABEL_SIZE: f32 = 0.843_75;
const LABEL_LINE: f32 = 1.25;
const META_SIZE: f32 = 0.781_25;
const META_LINE: f32 = 1.125;

/// The default starter prompts: ID and glyph.
const STARTERS: &[(&str, &str)] = &[
    ("explore", "compass-3"),
    ("catchup", "history"),
    ("weigh", "scales-3"),
    ("plan", "survey"),
    ("craftGoal", "target"),
    ("scheduleTask", "calendar-schedule"),
    ("debug", "bug"),
    ("review", "search-eye"),
];

impl Workspace {
    pub(crate) fn render_startup(
        &self,
        palette: &Palette,
        direction: Direction,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let title = strings.get("chat.emptyState.draftTitle");
        // `pb-[6vh]`: six percent of the content's height.
        let lift = chrome::content_size(window).height * 0.06;
        let column_padding = chrome::chat_column_padding(window, self.chat_area_width(window));
        let project_chooser = on_activate(
            div()
                .id("choose-project")
                .role(Role::Button)
                .accessibility_id("choose-project")
                .aria_label(strings.get("chat.chatInput.chooseProject"))
                .h(px(28.0))
                .px(px(6.0))
                .rounded(px(CONTROL_RADIUS))
                .flex_row_from(direction)
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .font_weight(FontWeight::MEDIUM)
                .text_color(palette.foreground)
                .child(
                    div()
                        .css_text(LABEL_SIZE, LABEL_LINE, cx)
                        .child(strings.get("chat.chatInput.chooseProject")),
                )
                .child(glyph("arrow-down-s", 16.0, palette.foreground).opacity(0.5)),
            cx,
            |_, _, _| {},
        )
        .test_support();
        let hover_fill = palette.hover;
        let foreground = palette.foreground;
        let chip = |id: &str, icon: &'static str| {
            let label = strings.get(&format!("chat.draftPresets.{id}.label"));
            let group = format!("starter-{id}");
            on_activate(
                div()
                    .id(gpui_kit::SharedString::from(group.clone()))
                    .group(group.clone())
                    .role(Role::Button)
                    .accessibility_id(group.clone())
                    .aria_label(label.clone())
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
                        glyph(icon, 12.0, palette.muted_foreground)
                            .opacity(0.7)
                            .group_hover(group, move |style| {
                                style.opacity(1.0).text_color(foreground)
                            }),
                    )
                    .child(div().css_text(META_SIZE, META_LINE, cx).child(label)),
                cx,
                |_, _, _| {},
            )
            .test_support()
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
            |_, _, _| {},
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
                                .child(tracked_text(title, TITLE_TRACKING, TextAlign::Center)),
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
                            .child(self.render_composer_box(palette, direction, window, cx)),
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
                            .children(STARTERS.iter().map(|(id, icon)| chip(id, icon)))
                            .child(add_starter),
                    ),
            )
    }
}
