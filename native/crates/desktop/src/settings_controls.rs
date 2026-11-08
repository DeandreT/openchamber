//! Settings page chrome shared by the native pages: the scrolling page frame,
//! sections, field rows, info hints, checkbox rows and the load-failure line.
//! The reference builds every page from `SettingsSection.tsx`.

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    AnyElement, App, Context, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Role, ScrollAnchor, ScrollHandle, SharedString, StatefulInteractiveElement,
    Styled, WeakEntity, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::ids::RuntimeId;
use openchamber_native_transport::client::ServerClient;

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::network::Network;
use crate::settings_navigation::Page;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// The server a page reads and writes, captured when the request starts.
#[derive(Clone)]
pub(crate) struct ServerTarget {
    pub(crate) runtime: RuntimeId,
    pub(crate) generation: u64,
    pub(crate) client: ServerClient,
    pub(crate) network: tokio::runtime::Handle,
}

impl ServerTarget {
    pub(crate) fn capture(
        owner: &WeakEntity<Workspace>,
        network: Option<&Network>,
        cx: &App,
    ) -> Option<Self> {
        let owner = owner.upgrade()?;
        let workspace = owner.read(cx);
        let connection = workspace.connection.as_ref()?;
        Some(Self {
            runtime: workspace.runtime.clone(),
            generation: connection.generation,
            client: connection.client.clone()?,
            network: network?.handle(),
        })
    }

    pub(crate) fn same(&self, other: &Self) -> bool {
        self.runtime == other.runtime && self.generation == other.generation
    }
}

/// Whether two optional targets name the same connection.
pub(crate) fn same_target(a: Option<&ServerTarget>, b: Option<&ServerTarget>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.same(b),
        (None, None) => true,
        _ => false,
    }
}

/// A page read's state. A failed read is never shown as an empty answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Load<T> {
    #[default]
    Loading,
    Ready(T),
    Failed,
}

impl<T> Load<T> {
    pub(crate) fn ready(&self) -> Option<&T> {
        match self {
            Self::Ready(value) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn ready_mut(&mut self) -> Option<&mut T> {
        match self {
            Self::Ready(value) => Some(value),
            _ => None,
        }
    }
}

/// Autosave's serial writes (`SettingsAutosave.tsx`): one write at a time,
/// and a request during a write queues exactly one follow-up.
#[derive(Debug, Default)]
pub(crate) struct SerialSave {
    running: bool,
    queued: bool,
}

impl SerialSave {
    /// Whether the caller starts a write now; otherwise one follow-up waits.
    pub(crate) fn begin(&mut self) -> bool {
        if self.running {
            self.queued = true;
            return false;
        }
        self.running = true;
        true
    }

    /// Ends the running write; `true` when a queued follow-up should run.
    pub(crate) fn finish(&mut self) -> bool {
        self.running = false;
        std::mem::take(&mut self.queued)
    }
}

/// The scrolling frame every page sits in, with its header.
pub(crate) fn page_frame(
    id: &'static str,
    page: Page,
    scroll: &ScrollHandle,
    cx: &App,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let strings = locale::strings(cx);
    let palette = cx.global::<Palette>();
    div()
        .css_text(0.875, 1.25, cx)
        .id(id)
        .role(Role::Group)
        .aria_label(strings.get(page.title()))
        .size_full()
        .min_w_0()
        .overflow_y_scroll()
        .track_scroll(scroll)
        .px(px(48.0))
        .py(px(32.0))
        .text_color(palette.foreground)
        .when(strings.direction() == Direction::RightToLeft, |view| {
            view.text_right()
        })
        .child(crate::settings_view::page_header(page, cx))
}

/// A titled block; sections after the first carry a divider.
pub(crate) fn section(
    title: &'static str,
    first: bool,
    rows: Vec<AnyElement>,
    cx: &App,
) -> AnyElement {
    section_with_info(title, None, first, rows, None, cx)
}

/// A titled block with an optional info hint and search anchor.
pub(crate) fn section_with_info(
    title: &'static str,
    info: Option<SharedString>,
    first: bool,
    rows: Vec<AnyElement>,
    anchor: Option<ScrollAnchor>,
    cx: &App,
) -> AnyElement {
    let palette = cx.global::<Palette>();
    let direction = locale::strings(cx).direction();
    div()
        .id(SharedString::from(format!("{title}-section")))
        .anchor_scroll(anchor)
        .when(!first, |section| {
            section
                .mt(px(32.0))
                .pt(px(24.0))
                .border_t_1()
                .border_color(palette.border)
        })
        .child(
            div()
                .flex_row_from(direction)
                .items_center()
                .gap(px(6.0))
                .mb(px(12.0))
                .child(
                    div()
                        .id(title)
                        .role(Role::Heading)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(locale::strings(cx).get(title)),
                )
                .children(info.map(|info| info_hint(title, info, cx))),
        )
        .child(div().flex().flex_col().gap(px(6.0)).children(rows))
        .into_any_element()
}

/// Label left, control right, wrapping when the pane is narrow.
pub(crate) fn field_row(
    item: &str,
    label: SharedString,
    info: Option<SharedString>,
    control: AnyElement,
    anchor: Option<ScrollAnchor>,
    cx: &App,
) -> AnyElement {
    let direction = locale::strings(cx).direction();
    div()
        .id(SharedString::from(format!(
            "{}-row",
            item.replace('.', "-")
        )))
        .anchor_scroll(anchor)
        .flex_row_from(direction)
        .flex_wrap()
        .items_center()
        .gap(px(12.0))
        .py(px(4.0))
        .child(
            div()
                .flex_row_from(direction)
                .items_center()
                .gap(px(6.0))
                .child(label)
                .children(info.map(|info| info_hint(item, info, cx))),
        )
        .child(control)
        .into_any_element()
}

/// Helper text behind an info icon, shown on hover.
pub(crate) fn info_hint(item: &str, text: SharedString, cx: &App) -> AnyElement {
    let id: SharedString = format!("{}-info", item.replace('.', "-")).into();
    div()
        .id(id.clone())
        .accessibility_id(id)
        .role(Role::Image)
        .aria_label(text.clone())
        .pt(px(2.0))
        .child(glyph(
            "information",
            14.0,
            cx.global::<Palette>().muted_foreground,
        ))
        .tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
        .into_any_element()
}

/// One boolean row (`SettingsCheckboxRow`). `id` is also its accessibility id.
#[allow(clippy::too_many_arguments)]
pub(crate) fn checkbox_row<T: 'static>(
    id: SharedString,
    label: SharedString,
    aria: SharedString,
    checked: bool,
    enabled: bool,
    focus: &FocusHandle,
    anchor: Option<ScrollAnchor>,
    cx: &Context<T>,
    on_change: impl Fn(&mut T, bool, &mut gpui_kit::Window, &mut Context<T>) + 'static,
) -> AnyElement {
    let direction = locale::strings(cx).direction();
    let palette = cx.global::<Palette>();
    let weak = cx.weak_entity();
    gpui_kit::base::Checkbox::new(id.clone())
        .accessibility_id(id)
        .accessibility_label(aria)
        .checked(checked)
        .disabled(!enabled)
        .track_focus(focus)
        .anchor_scroll(anchor)
        .flex_row_from(direction)
        .items_start()
        .gap(px(8.0))
        .py(px(5.0))
        .when(!enabled, |row| row.opacity(0.5))
        .on_change(move |state, _, window, cx| {
            let _ = weak.update(cx, |this, cx| {
                on_change(
                    this,
                    state == gpui_kit::base::CheckboxState::Checked,
                    window,
                    cx,
                )
            });
        })
        .child(glyph(
            if checked {
                "checkbox"
            } else {
                "checkbox-blank"
            },
            16.0,
            palette.foreground,
        ))
        .child(div().min_w_0().child(label))
        .into_any_element()
}

/// A muted helper line (`SETTINGS_HELPER_CLASS`).
pub(crate) fn helper(text: SharedString, cx: &App) -> AnyElement {
    div()
        .css_text(0.8125, 1.25, cx)
        .text_color(cx.global::<Palette>().muted_foreground)
        .child(text)
        .into_any_element()
}

/// A failed read: its message in the error color and Retry.
pub(crate) fn load_failed<T: 'static>(
    id: &'static str,
    message: SharedString,
    retry: SharedString,
    cx: &Context<T>,
    on_retry: impl Fn(&mut T, &mut Context<T>) + 'static,
) -> AnyElement {
    let palette = cx.global::<Palette>();
    let direction = locale::strings(cx).direction();
    let button_id: SharedString = format!("{id}-retry").into();
    div()
        .id(id)
        .accessibility_id(id)
        .role(Role::Status)
        .aria_label(message.clone())
        .flex_row_from(direction)
        .flex_wrap()
        .items_center()
        .gap(px(12.0))
        .child(
            div()
                .css_text(0.8125, 1.25, cx)
                .text_color(palette.status_error)
                .child(message),
        )
        .child(on_activate(
            gpui_kit::base::Button::new(button_id.clone())
                .accessibility_id(button_id)
                .accessibility_label(retry.clone())
                .h(gpui_kit::rems(1.75))
                .px(px(8.0))
                .rounded(px(7.0))
                .child(retry),
            cx,
            move |this, _, cx| on_retry(this, cx),
        ))
        .into_any_element()
}

/// A confirmation above Settings (`Dialog` with Cancel and an action), as
/// the reference's discard and delete dialogs.
pub(crate) fn confirm<T: 'static>(
    page: WeakEntity<T>,
    title: SharedString,
    description: SharedString,
    action: SharedString,
    window: &mut gpui_kit::Window,
    cx: &mut App,
    on_confirm: impl Fn(&mut T, &mut gpui_kit::Window, &mut Context<T>) + 'static,
) {
    use gpui_kit::component::WindowExt;
    let cancel = locale::strings(cx).get("settings.common.actions.cancel");
    let on_confirm = std::rc::Rc::new(on_confirm);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let page = page.clone();
        let on_confirm = on_confirm.clone();
        alert
            .title(title.clone())
            .description(description.clone())
            .confirm()
            .ok_text(action.clone())
            .cancel_text(cancel.clone())
            .on_ok(move |_, window, cx| {
                let _ = page.update(cx, |this, cx| on_confirm(this, window, cx));
                true
            })
    });
}

/// A focus handle per stable control id, created on first use.
pub(crate) fn focus_handle<T: 'static>(
    handles: &mut std::collections::HashMap<SharedString, FocusHandle>,
    id: &SharedString,
    cx: &mut Context<T>,
) -> FocusHandle {
    handles
        .entry(id.clone())
        .or_insert_with(|| cx.focus_handle().tab_index(0).tab_stop(true))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_save_runs_one_write_and_one_follow_up() {
        let mut save = SerialSave::default();
        assert!(save.begin(), "an idle autosave writes at once");
        assert!(!save.begin(), "a request during a write waits");
        assert!(!save.begin(), "further requests share one follow-up");
        assert!(save.finish(), "the queued follow-up runs");
        assert!(save.begin());
        assert!(!save.finish(), "nothing else was requested");
        assert!(save.begin(), "a finished write leaves the autosave idle");
    }
}
