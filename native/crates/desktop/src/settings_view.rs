//! Native Settings presentation. Page models retain their own data and writes.

use gpui_kit::component::WindowExt;
use gpui_kit::{
    App, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement, Role,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};

use crate::baseline::CssTextExt;
use crate::host_manager::HostManager;
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;

/// Opens the implemented host page; other Settings pages have separate owners.
pub(crate) fn open_hosts(manager: Entity<HostManager>, window: &mut Window, cx: &mut App) {
    window.close_dialog(cx);
    window.open_dialog(cx, move |dialog, window, cx| {
        let viewport = window.viewport_size();
        let height = viewport.height * 0.85;
        dialog
            .w(viewport.width * 0.9)
            .h(height)
            .margin_top((viewport.height - height) / 2.0)
            .p_0()
            .child(hosts_page(manager.clone(), cx))
    });
}

fn hosts_page(manager: Entity<HostManager>, cx: &App) -> impl IntoElement {
    let strings = locale::strings(cx);
    let palette = cx.global::<Palette>();
    let scroll = manager.read(cx).settings_scroll.clone();
    div()
        .id("settings-dialog")
        .accessibility_id("settings-dialog")
        .role(Role::Group)
        .aria_label(strings.get("settings.window.description"))
        .size_full()
        .flex_row_from(strings.direction())
        .overflow_hidden()
        .rounded(px(10.0))
        .css_text(0.875, 1.25, cx)
        .when(
            strings.direction() == locale::Direction::RightToLeft,
            |root| root.text_right(),
        )
        .bg(palette.background)
        .text_color(palette.foreground)
        .child(
            div()
                .id("settings-navigation")
                .accessibility_id("settings-navigation")
                .role(Role::Navigation)
                .w(px(256.0))
                .flex_shrink_0()
                .h_full()
                .p(px(16.0))
                .bg(palette.elevated)
                .child(
                    div()
                        .css_text(0.75, 1.0, cx)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(palette.muted_foreground)
                        .mb(px(8.0))
                        // The dialog's close control stays at physical right.
                        .when(
                            strings.direction() == locale::Direction::RightToLeft,
                            |heading| heading.pr(px(32.0)),
                        )
                        .child(strings.get("settings.view.nav.group.projects")),
                )
                .child(
                    div()
                        .id("settings-current-page")
                        .accessibility_id("settings-current-page")
                        .role(Role::Group)
                        .aria_label(strings.get("settings.page.remoteInstances.title"))
                        .px(px(12.0))
                        .py(px(8.0))
                        .rounded(px(7.0))
                        .bg(palette.selection)
                        .text_color(palette.selection_foreground)
                        .child(strings.get("settings.page.remoteInstances.title")),
                ),
        )
        .child(
            div()
                .id("settings-content")
                .flex_1()
                .min_w_0()
                .h_full()
                .overflow_y_scroll()
                .track_scroll(&scroll)
                .px(px(48.0))
                .py(px(32.0))
                .child(
                    div()
                        .max_w(px(840.0))
                        .mx_auto()
                        .child(
                            div()
                                .css_text(1.125, 1.5, cx)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(palette.muted_foreground)
                                .mb(px(24.0))
                                .child(strings.get("settings.page.remoteInstances.title")),
                        )
                        .child(manager),
                ),
        )
}
