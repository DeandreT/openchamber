//! Small controls the OpenCode configuration pages share: the segmented chip
//! group (`SettingsChipGroup`), text and icon buttons, and a status pill.

use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::{
    AnyElement, Context, FocusHandle, FontWeight, Hsla, InteractiveElement, IntoElement,
    ParentElement, Role, SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Toggled,
    Window, div, prelude::FluentBuilder, px,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::settings_controls::focus_handle;
use crate::theme::Palette;

/// One option of a chip group. `id` is also its accessibility id.
pub(crate) struct ChipOption {
    pub(crate) id: SharedString,
    pub(crate) label: SharedString,
    pub(crate) selected: bool,
}

/// A short segmented choice, as wide as its labels. Left and right arrows
/// move between the options in reading order.
pub(crate) fn chip_group<T: 'static>(
    id: SharedString,
    aria: SharedString,
    options: Vec<ChipOption>,
    enabled: bool,
    focus: &mut HashMap<SharedString, FocusHandle>,
    cx: &mut Context<T>,
    on_select: impl Fn(&mut T, usize, &mut Window, &mut Context<T>) + 'static,
) -> AnyElement {
    let palette = cx.global::<Palette>().clone();
    let direction = locale::strings(cx).direction();
    let hover = palette.hover;
    let on_select = Rc::new(on_select);
    let handles: Vec<FocusHandle> = options
        .iter()
        .map(|option| focus_handle(focus, &option.id, cx))
        .collect();
    let count = options.len();
    let mut group = div()
        .id(id)
        .role(Role::Group)
        .aria_label(aria)
        .flex_shrink_0()
        .flex_row_from(direction)
        .items_center()
        .gap(px(2.0))
        .p(px(2.0))
        .h(gpui_kit::rems(2.0))
        .rounded(px(7.0))
        .bg(palette.elevated)
        .when(!enabled, |group| group.opacity(0.5));
    for (index, option) in options.into_iter().enumerate() {
        let chip = div()
            .id(option.id.clone())
            .accessibility_id(option.id)
            .role(Role::Button)
            .aria_label(option.label.clone())
            .aria_toggled(if option.selected {
                Toggled::True
            } else {
                Toggled::False
            })
            .h_full()
            .px(px(10.0))
            .rounded(px(5.0))
            .flex()
            .items_center()
            .when(option.selected, |chip| {
                chip.bg(palette.background).font_weight(FontWeight::MEDIUM)
            })
            .child(option.label);
        if !enabled {
            group = group.child(chip.test_support());
            continue;
        }
        let select = on_select.clone();
        let handles = handles.clone();
        group = group.child(
            on_activate(
                chip.track_focus(&handles[index])
                    .tab_index(0)
                    .cursor_pointer()
                    .when(!option.selected, |chip| {
                        chip.hover(move |style| style.bg(hover))
                    }),
                cx,
                move |this, window, cx| select(this, index, window, cx),
            )
            .on_key_down(
                cx.listener(move |_, event: &gpui_kit::KeyDownEvent, window, cx| {
                    let key = &event.keystroke;
                    if key.modifiers.control || key.modifiers.alt || key.modifiers.platform {
                        return;
                    }
                    let step = match (key.key.as_str(), direction) {
                        ("right", Direction::LeftToRight) | ("left", Direction::RightToLeft) => 1,
                        ("left", Direction::LeftToRight) | ("right", Direction::RightToLeft) => {
                            count - 1
                        }
                        _ => return,
                    };
                    window.focus(&handles[(index + step) % count], cx);
                    cx.stop_propagation();
                }),
            )
            .test_support(),
        );
    }
    group.into_any_element()
}

/// A text button at the Settings control height.
pub(crate) fn text_button<T: 'static>(
    id: impl Into<SharedString>,
    label: SharedString,
    enabled: bool,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> AnyElement {
    let id: SharedString = id.into();
    on_activate(
        gpui_kit::base::Button::new(id.clone())
            .accessibility_id(id)
            .accessibility_label(label.clone())
            .disabled(!enabled)
            .h(gpui_kit::rems(1.75))
            .px(px(8.0))
            .rounded(px(7.0))
            .child(label),
        cx,
        action,
    )
    .into_any_element()
}

/// A quiet square icon button with an accessible name.
pub(crate) fn icon_button<T: 'static>(
    id: SharedString,
    icon: &'static str,
    label: SharedString,
    focus: &mut HashMap<SharedString, FocusHandle>,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> AnyElement {
    let palette = cx.global::<Palette>().clone();
    let handle = focus_handle(focus, &id, cx);
    on_activate(
        div()
            .id(id.clone())
            .accessibility_id(id)
            .role(Role::Button)
            .aria_label(label)
            .track_focus(&handle)
            .tab_index(0)
            .size(px(24.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.0))
            .cursor_pointer()
            .hover(move |style| style.bg(palette.hover))
            .child(glyph(icon, 14.0, palette.muted_foreground)),
        cx,
        action,
    )
    .test_support()
    .into_any_element()
}

/// A small status word in its state's color.
pub(crate) fn pill<T: 'static>(
    id: SharedString,
    label: SharedString,
    color: Hsla,
    cx: &Context<T>,
) -> AnyElement {
    div()
        .id(id.clone())
        .accessibility_id(id)
        .role(Role::Status)
        .aria_label(label.clone())
        .flex_shrink_0()
        .css_text(0.6875, 1.0, cx)
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .child(label)
        .into_any_element()
}
