//! Small controls the scheduled tasks page and the Project knowledge panel
//! share: labelled buttons in the design system's variants, a checkbox row,
//! a select trigger with its option rows, and the muted text styles.

use gpui_kit::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Toggled, Window, div,
    prelude::FluentBuilder, px,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{CONTROL_RADIUS, glyph};
use crate::elevation;
use crate::locale::Direction;
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;

/// `typography-ui-label`, `typography-meta` and `typography-micro`.
pub(crate) const LABEL_SIZE: f32 = 0.843_75;
pub(crate) const LABEL_LINE: f32 = 1.25;
pub(crate) const META_SIZE: f32 = 0.781_25;
pub(crate) const META_LINE: f32 = 1.125;
pub(crate) const MICRO_SIZE: f32 = 0.75;
pub(crate) const MICRO_LINE: f32 = 1.0625;
pub(crate) const HEADER_SIZE: f32 = 0.875;
pub(crate) const HEADER_LINE: f32 = 1.25;

/// The `Button` variants these surfaces use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Variant {
    Primary,
    Outline,
    Ghost,
    Destructive,
}

/// A text button, optionally with a leading glyph; a disabled one keeps its
/// place, dims and ignores clicks.
#[allow(clippy::too_many_arguments)]
pub(crate) fn button<V: 'static>(
    id: impl Into<SharedString>,
    icon: Option<&'static str>,
    label: Option<SharedString>,
    aria: SharedString,
    variant: Variant,
    disabled: bool,
    palette: &Palette,
    direction: Direction,
    cx: &Context<V>,
    action: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) -> AnyElement {
    let id: SharedString = id.into();
    let (fill, text, hover) = match variant {
        Variant::Primary => (
            Some(palette.primary),
            palette.primary_foreground,
            palette.primary_hover,
        ),
        Variant::Outline => (Some(palette.background), palette.foreground, palette.hover),
        Variant::Ghost => (None, palette.foreground, palette.hover),
        Variant::Destructive => (
            Some(palette.status_error_background),
            palette.status_error,
            palette.status_error_background,
        ),
    };
    let element = div()
        .id(gpui_kit::ElementId::Name(id.clone()))
        .role(Role::Button)
        .accessibility_id(id)
        .aria_label(aria)
        .h(px(28.0))
        .px(px(if label.is_some() { 10.0 } else { 6.0 }))
        .flex_shrink_0()
        .rounded(px(CONTROL_RADIUS))
        .flex_row_from(direction)
        .items_center()
        .gap(px(6.0))
        .css_text(META_SIZE, META_LINE, cx)
        .font_weight(FontWeight::MEDIUM)
        .text_color(text)
        .when_some(fill, |this, fill| this.bg(fill))
        .when(variant == Variant::Outline, |this| {
            this.shadow(elevation::control(palette))
        })
        .when_some(icon, |this, icon| this.child(glyph(icon, 16.0, text)))
        .when_some(label, |this, label| this.child(label));
    let element = if disabled {
        element
            .opacity(0.5)
            .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
    } else {
        element
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
    };
    if disabled {
        element.test_support().into_any_element()
    } else {
        on_activate(element, cx, action)
            .test_support()
            .into_any_element()
    }
}

/// A checkbox with its label; the whole row toggles.
#[allow(clippy::too_many_arguments)]
pub(crate) fn checkbox<V: 'static>(
    id: impl Into<SharedString>,
    label: SharedString,
    checked: bool,
    disabled: bool,
    palette: &Palette,
    direction: Direction,
    cx: &Context<V>,
    action: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) -> AnyElement {
    let id: SharedString = id.into();
    let element = div()
        .id(gpui_kit::ElementId::Name(id.clone()))
        .role(Role::CheckBox)
        .accessibility_id(id)
        .aria_label(label.clone())
        .aria_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        })
        .flex_row_from(direction)
        .items_center()
        .gap(px(8.0))
        .css_text(META_SIZE, META_LINE, cx)
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
        .child(label);
    if disabled {
        element
            .opacity(0.5)
            .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
            .test_support()
            .into_any_element()
    } else {
        on_activate(element.cursor_pointer(), cx, action)
            .test_support()
            .into_any_element()
    }
}

/// A select's trigger: its value and a chevron.
#[allow(clippy::too_many_arguments)]
pub(crate) fn select_trigger<V: 'static>(
    id: impl Into<SharedString>,
    value: SharedString,
    label: SharedString,
    open: bool,
    disabled: bool,
    palette: &Palette,
    direction: Direction,
    cx: &Context<V>,
    action: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) -> AnyElement {
    let id: SharedString = id.into();
    let hover = palette.hover;
    let element = div()
        .id(gpui_kit::ElementId::Name(id.clone()))
        .role(Role::ComboBox)
        .accessibility_id(id)
        .aria_label(SharedString::from(format!("{label}: {value}")))
        .aria_expanded(open)
        .h(px(30.0))
        .max_w_full()
        .px(px(10.0))
        .rounded(px(CONTROL_RADIUS))
        .bg(palette.background)
        .shadow(elevation::control(palette))
        .flex_row_from(direction)
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .min_w_0()
                .truncate()
                .css_text(LABEL_SIZE, LABEL_LINE, cx)
                .text_color(palette.foreground)
                .child(value),
        )
        .child(glyph("arrow-down-s", 14.0, palette.muted_foreground));
    if disabled {
        element
            .opacity(0.5)
            .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
            .test_support()
            .into_any_element()
    } else {
        on_activate(
            element.cursor_pointer().hover(move |style| style.bg(hover)),
            cx,
            action,
        )
        .test_support()
        .into_any_element()
    }
}

/// One option of an open select or menu.
#[allow(clippy::too_many_arguments)]
pub(crate) fn option_row<V: 'static>(
    id: impl Into<SharedString>,
    label: SharedString,
    checked: bool,
    destructive: bool,
    palette: &Palette,
    direction: Direction,
    cx: &Context<V>,
    action: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) -> AnyElement {
    let id: SharedString = id.into();
    let hover = palette.hover;
    on_activate(
        div()
            .id(gpui_kit::ElementId::Name(id.clone()))
            .role(Role::MenuItemRadio)
            .accessibility_id(id)
            .aria_label(label.clone())
            .aria_selected(checked)
            .min_h(px(28.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .css_text(LABEL_SIZE, LABEL_LINE, cx)
                    .text_color(if destructive {
                        palette.status_error
                    } else {
                        palette.foreground
                    })
                    .child(label),
            )
            .when(checked, |this| {
                this.child(glyph("check", 14.0, palette.foreground))
            }),
        cx,
        action,
    )
    .test_support()
    .into_any_element()
}

/// The floating list an open select shows under its trigger.
pub(crate) fn popup(
    id: &'static str,
    items: Vec<AnyElement>,
    width: f32,
    palette: &Palette,
) -> AnyElement {
    div()
        .id(id)
        .role(Role::ListBox)
        .accessibility_id(id)
        .absolute()
        .top(px(34.0))
        .left_0()
        .w(px(width))
        .max_h(px(280.0))
        .overflow_y_scroll()
        .p(px(4.0))
        .rounded(px(10.0))
        .bg(palette.elevated)
        .shadow(elevation::floating(palette))
        .occlude()
        .children(items)
        .into_any_element()
}

/// Text in one of the type styles.
pub(crate) fn text<V>(
    value: impl Into<SharedString>,
    size: (f32, f32),
    color: Hsla,
    cx: &Context<V>,
) -> gpui_kit::Div {
    div()
        .css_text(size.0, size.1, cx)
        .text_color(color)
        .child(value.into())
}
