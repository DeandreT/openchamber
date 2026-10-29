//! One nonmodal request panel above the composer, with permission priority.

use gpui_kit::component::input::Textarea;
use gpui_kit::{
    AnyElement, Context, EntityInputHandler, InteractiveElement, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window, div,
    prelude::FluentBuilder, px,
};
use openchamber_native_core::form::{FormFieldKind, FormValue};
use openchamber_native_core::pending_requests::{
    PendingRequest, RequestId, RequestKey, ResponseState,
};
use openchamber_native_core::permission::PermissionReply;

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::blocking_requests::RequestReply;
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

#[derive(Clone, Copy)]
enum Control {
    Raised,
    Primary,
    Advance(bool),
    Ghost,
    Custom(bool),
    Check(bool),
    Radio(bool),
    Tab(bool),
    Icon(&'static str),
}

fn identity(key: &RequestKey) -> &str {
    match &key.id {
        RequestId::Permission(id) => id.as_str(),
        RequestId::Form(id) => id.as_str(),
    }
}

fn option_label(label: &str, recommended: &str) -> String {
    if let Some(index) = label.to_ascii_lowercase().find("(recommended)") {
        format!(
            "{} · {recommended}",
            format!(
                "{} {}",
                label[..index].trim_end(),
                label[index + "(recommended)".len()..].trim_start()
            )
            .trim()
        )
    } else {
        label.to_owned()
    }
}

fn button(
    id: String,
    label: SharedString,
    enabled: bool,
    control: Control,
    palette: &Palette,
    context: (&mut Window, &mut Context<Workspace>),
    action: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) -> AnyElement {
    let (window, cx) = context;
    let selected = matches!(
        control,
        Control::Check(true) | Control::Radio(true) | Control::Tab(true)
    );
    let role = match control {
        Control::Check(_) => Role::CheckBox,
        Control::Radio(_) => Role::RadioButton,
        Control::Tab(_) => Role::Tab,
        _ => Role::Button,
    };
    // Next and Submit keep their automation IDs but share keyboard focus.
    let focus_id = if matches!(control, Control::Advance(_)) {
        format!(
            "{}-advance-focus",
            id.rsplit_once('-').expect("action suffix").0
        )
    } else {
        format!("{id}-focus")
    };
    let focus = window
        .use_keyed_state(gpui_kit::ElementId::Name(focus_id.into()), cx, |_, cx| {
            cx.focus_handle().tab_index(0).tab_stop(true)
        })
        .read(cx)
        .clone();
    on_activate(
        div()
            .id(SharedString::from(id.clone()))
            .role(role)
            .accessibility_id(id)
            .aria_label(label.clone())
            .test_support()
            .flex()
            .flex_row_from(locale::strings(cx).direction())
            .items_center()
            .gap(px(8.))
            .min_h(px(24.))
            .px(px(8.))
            .py(px(3.5))
            .rounded(px(5.))
            .text_size(px(12.))
            .text_color(palette.elevated_foreground)
            .map(|element| match control {
                Control::Check(checked) | Control::Radio(checked) => element
                    .aria_toggled(if checked {
                        gpui_kit::Toggled::True
                    } else {
                        gpui_kit::Toggled::False
                    })
                    .w_full()
                    .px(px(6.))
                    .py(px(4.))
                    .when(selected, |element| {
                        element.bg(palette.selection.opacity(0.2))
                    })
                    .child(
                        div()
                            .size(px(14.))
                            .flex_shrink_0()
                            .border_1()
                            .border_color(if checked {
                                palette.primary
                            } else {
                                palette.border
                            })
                            .rounded(px(if matches!(control, Control::Radio(_)) {
                                7.
                            } else {
                                3.
                            }))
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(checked, |element| {
                                if matches!(control, Control::Radio(_)) {
                                    element.child(
                                        div().size(px(8.)).rounded(px(4.)).bg(palette.primary),
                                    )
                                } else {
                                    element.bg(palette.primary).child(crate::chrome::glyph(
                                        "check",
                                        12.,
                                        palette.primary_foreground,
                                    ))
                                }
                            }),
                    ),
                Control::Tab(active) => element
                    .aria_selected(active)
                    .size(px(16.))
                    .min_h(px(16.))
                    .p_0()
                    .justify_center()
                    .child(div().size(px(6.)).rounded(px(3.)).bg(if active {
                        palette.primary
                    } else {
                        palette.foreground.opacity(0.15)
                    })),
                Control::Custom(active) => element
                    .w_full()
                    .px(px(6.))
                    .py(px(4.))
                    .when(active, |element| element.bg(palette.selection.opacity(0.2)))
                    .child(crate::chrome::glyph(
                        "edit",
                        14.,
                        if active {
                            palette.primary
                        } else {
                            palette.muted_foreground
                        },
                    )),
                Control::Raised | Control::Advance(false) => element
                    .bg(match palette.appearance {
                        crate::theme::Appearance::Light => palette.background,
                        crate::theme::Appearance::Dark => palette.elevated,
                    })
                    .shadow(raised(palette)),
                Control::Primary | Control::Advance(true) => element
                    .bg(palette.primary.opacity(0.1))
                    .text_color(palette.primary)
                    .shadow(raised(palette)),
                Control::Ghost => element,
                Control::Icon(icon) => element
                    .size(px(24.))
                    .p_0()
                    .justify_center()
                    .child(crate::chrome::glyph(icon, 12., palette.muted_foreground)),
            })
            .when(enabled, |element| {
                let pointer_focus = focus.clone();
                element
                    .cursor_pointer()
                    .track_focus(&focus)
                    .on_mouse_down(gpui_kit::MouseButton::Left, move |_, window, cx| {
                        pointer_focus.focus(window, cx)
                    })
                    .hover(|style| style.bg(palette.hover))
                    .focus(|style| style.bg(palette.hover))
            })
            .when(!enabled, |element| {
                element
                    .opacity(0.5)
                    .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
            })
            .when(
                !matches!(control, Control::Tab(_) | Control::Icon(_)),
                |element| {
                    element.child(
                        div()
                            .css_text(
                                if matches!(
                                    control,
                                    Control::Check(_) | Control::Radio(_) | Control::Custom(_)
                                ) {
                                    0.78125
                                } else {
                                    0.75
                                },
                                if matches!(
                                    control,
                                    Control::Check(_) | Control::Radio(_) | Control::Custom(_)
                                ) {
                                    1.125
                                } else {
                                    1.0625
                                },
                                cx,
                            )
                            .child(label),
                    )
                },
            ),
        cx,
        move |this, window, cx| {
            if enabled {
                action(this, window, cx);
            }
        },
    )
    .into_any_element()
}

// The shared web `.oc-raised` recipe, kept local until native text buttons
// have a common primitive. CSS shadows paint in reverse order in GPUI.
fn raised(palette: &Palette) -> Vec<gpui_kit::BoxShadow> {
    let dark = palette.appearance == crate::theme::Appearance::Dark;
    let mut shadows = vec![
        gpui_kit::BoxShadow {
            color: gpui_kit::hsla(0., 0., 0., if dark { 0.28 } else { 0.06 }),
            offset: gpui_kit::point(px(0.), px(1.)),
            blur_radius: px(if dark { 0.5 } else { 1. }),
            spread_radius: px(0.),
            inset: false,
        },
        gpui_kit::BoxShadow {
            color: palette.foreground.opacity(if dark { 0.06 } else { 0.1 }),
            offset: gpui_kit::point(px(0.), px(0.)),
            blur_radius: px(0.),
            spread_radius: px(1.),
            inset: true,
        },
    ];
    if dark {
        shadows.push(gpui_kit::BoxShadow {
            color: palette.foreground.opacity(0.015),
            offset: gpui_kit::point(px(0.), px(1.)),
            blur_radius: px(0.),
            spread_radius: px(0.),
            inset: true,
        });
    }
    shadows
}

impl Workspace {
    pub(crate) fn render_request_dock(
        &self,
        palette: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let key = self.active_request()?;
        let entry = self.blocking.stores.get(&key.scope)?.get(&key)?;
        let id = identity(&key).to_owned();
        let enabled = self.request_actionable(&key);
        let strings = locale::strings(cx).clone();
        let mut header = div()
            .flex()
            .flex_row_from(strings.direction())
            .items_center()
            .gap(px(6.))
            .min_w_0();
        let title: SharedString = match &entry.request {
            PendingRequest::Permission(_) => strings.get("chat.permissionCard.title"),
            PendingRequest::Form(form) => {
                if form.title.trim().is_empty() {
                    strings.get("chat.formCard.inputNeeded")
                } else {
                    form.title.clone().into()
                }
            }
        };
        header = header.child(
            div()
                .min_w_0()
                .truncate()
                .css_text(0.84375, 1.25, cx)
                .child(title.clone()),
        );
        if let Some(draft) = self.blocking.drafts.get(&key) {
            let shown = draft.shown(cx);
            header = header.child(
                div()
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .text_color(palette.muted_foreground)
                    .css_text(0.75, 1.0625, cx)
                    .child(
                        strings.format(
                            "chat.formDock.progress",
                            &[
                                (
                                    "current",
                                    &(draft.step.min(shown.len().saturating_sub(1)))
                                        .saturating_add(1)
                                        .to_string(),
                                ),
                                ("total", &shown.len().to_string()),
                            ],
                        ),
                    ),
            );
        }
        if let openchamber_native_core::request_owner::RequestOwner::Session(owner) = &key.owner
            && self
                .sessions
                .get(owner)
                .is_some_and(|session| session.parent_id.is_some())
        {
            header = header.child(
                div()
                    .text_color(palette.muted_foreground)
                    .child(strings.get("chat.formCard.fromSubagent")),
            );
        }
        let collapse = strings.get(
            match (
                matches!(key.id, RequestId::Permission(_)),
                self.blocking.collapsed,
            ) {
                (true, true) => "chat.permissionDock.expandAria",
                (true, false) => "chat.permissionDock.collapseAria",
                (false, true) => "chat.formDock.expandAria",
                (false, false) => "chat.formDock.collapseAria",
            },
        );
        let collapse_id: SharedString = format!("request-{id}-collapse").into();
        let focus = window
            .use_keyed_state(
                gpui_kit::ElementId::Name(format!("{collapse_id}-focus").into()),
                cx,
                |_, cx| cx.focus_handle().tab_index(0).tab_stop(true),
            )
            .read(cx)
            .clone();
        let expanded = !self.blocking.collapsed;
        header = div()
            .flex()
            .flex_row_from(strings.direction())
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .py(px(6.))
            .min_h(px(32.))
            .child(on_activate(
                div()
                    .id(collapse_id.clone())
                    .accessibility_id(collapse_id)
                    .role(Role::Button)
                    .aria_label(collapse)
                    .aria_expanded(expanded)
                    .test_support()
                    .track_focus(&focus)
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row_from(strings.direction())
                    .gap(px(8.))
                    .items_center()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|style| style.bg(palette.hover))
                    .focus(|style| style.bg(palette.hover))
                    .child(crate::chrome::glyph(
                        "question",
                        14.,
                        if matches!(key.id, RequestId::Permission(_)) {
                            palette.warning
                        } else {
                            palette.primary
                        },
                    ))
                    .child(crate::chrome::glyph(
                        if self.blocking.collapsed {
                            "arrow-up-s"
                        } else {
                            "arrow-down-s"
                        },
                        16.,
                        palette.foreground,
                    ))
                    .child(header),
                cx,
                |this, _, cx| {
                    this.blocking.collapsed = !this.blocking.collapsed;
                    this.acknowledge_visible_form(cx);
                    cx.notify();
                },
            ));
        let mut panel = div()
            .id("request-dock")
            .role(Role::Dialog)
            .aria_label(title)
            .accessibility_id("request-dock")
            .test_support()
            .w_full()
            .mb(px(12.))
            .rounded(px(12.))
            .bg(palette.elevated)
            .text_color(palette.elevated_foreground)
            .shadow(crate::elevation::floating(palette))
            .text_size(px(12.5))
            .flex()
            .flex_col()
            .gap(px(0.));
        if self.blocking.collapsed {
            return Some(panel.child(header).into_any_element());
        }
        let relevant: Vec<_> = self
            .visible_requests()
            .into_iter()
            .filter(|candidate| candidate.id.kind() == key.id.kind())
            .collect();
        if relevant.len() > 1 {
            header = header.child(
                div()
                    .id("request-tabs")
                    .role(Role::TabList)
                    .flex()
                    .flex_row_from(strings.direction())
                    .gap(px(4.))
                    .children(relevant.into_iter().enumerate().map(|(index, candidate)| {
                        let selected = candidate == key;
                        let label = self
                            .blocking
                            .stores
                            .get(&candidate.scope)
                            .and_then(|store| store.get(&candidate))
                            .map(|entry| match &entry.request {
                                PendingRequest::Permission(request) => strings.format(
                                    "chat.permissionDock.stepAria",
                                    &[
                                        ("index", &(index + 1).to_string()),
                                        ("tool", &request.action),
                                    ],
                                ),
                                PendingRequest::Form(form) => strings.format(
                                    "chat.formDock.stepAria",
                                    &[("index", &(index + 1).to_string()), ("title", &form.title)],
                                ),
                            })
                            .expect("visible requests exist in their scoped store");
                        button(
                            format!("request-{}-tab", identity(&candidate)),
                            label,
                            true,
                            Control::Tab(selected),
                            palette,
                            (window, cx),
                            move |this, _, cx| {
                                this.blocking.selected = Some(candidate.clone());
                                this.acknowledge_visible_form(cx);
                                cx.notify();
                            },
                        )
                    })),
            );
        }
        let mut body = div()
            .id(SharedString::from(format!("request-{id}-body")))
            .max_h(px((f32::from(crate::chrome::content_size(window).height)
                * 0.5)
                .min(320.)))
            .overflow_y_scroll()
            .px(px(12.))
            .py(px(4.))
            .flex()
            .flex_col()
            .gap(px(2.));
        let footer;
        match &entry.request {
            PendingRequest::Permission(request) => {
                body = body.child(request.action.clone());
                if let Some(message) = &request.message {
                    body = body.child(message.clone());
                }
                for resource in &request.resources {
                    body = body.child(resource.clone());
                }
                if !request.save.is_empty() {
                    body = body
                        .child(strings.get("chat.permissionCard.patterns"))
                        .child(request.save.join("\n"));
                }
                let mut actions = div()
                    .flex()
                    .flex_row_from(strings.direction())
                    .items_center()
                    .gap(px(6.))
                    .px(px(12.))
                    .pt(px(4.))
                    .pb(px(8.));
                for (suffix, label, decision) in [
                    (
                        "reject",
                        "chat.permissionCard.deny",
                        PermissionReply::Reject,
                    ),
                    (
                        "always",
                        "chat.permissionCard.alwaysAllow",
                        PermissionReply::Always,
                    ),
                    (
                        "once",
                        "chat.permissionCard.allowOnce",
                        PermissionReply::Once,
                    ),
                ] {
                    if decision == PermissionReply::Always && !request.can_save() {
                        continue;
                    }
                    let captured = key.clone();
                    actions = actions.child(button(
                        format!("permission-{id}-{suffix}"),
                        strings.get(label),
                        enabled,
                        if decision == PermissionReply::Once {
                            Control::Primary
                        } else if decision == PermissionReply::Reject {
                            Control::Ghost
                        } else {
                            Control::Raised
                        },
                        palette,
                        (window, cx),
                        move |this, window, cx| {
                            this.reply_request(
                                captured.clone(),
                                RequestReply::Permission(decision),
                                window,
                                cx,
                            )
                        },
                    ));
                    if decision == PermissionReply::Reject {
                        actions = actions.child(div().flex_1().min_w_0());
                    }
                }
                footer = actions;
            }
            PendingRequest::Form(_) => {
                let draft = self.blocking.drafts.get(&key)?;
                let shown = draft.shown(cx);
                let step = draft.step.min(shown.len().saturating_sub(1));
                let mut tabs = div()
                    .id("form-steps")
                    .role(Role::TabList)
                    .flex()
                    .flex_row_from(strings.direction())
                    .gap(px(4.));
                for (index, field) in shown.iter().enumerate() {
                    let captured = key.clone();
                    tabs = tabs.child(button(
                        format!("form-{id}-step-{index}"),
                        strings.format(
                            "chat.formDock.stepAria",
                            &[
                                ("index", &(index + 1).to_string()),
                                ("title", field.title.as_deref().unwrap_or(&field.key)),
                            ],
                        ),
                        enabled,
                        Control::Tab(index == step),
                        palette,
                        (window, cx),
                        move |this, _, cx| this.set_form_step(captured.clone(), index, cx),
                    ));
                }
                header = header.child(tabs);
                if let Some(field) = shown.get(step) {
                    if matches!(&field.kind, FormFieldKind::String { options: Some(options), .. } if !options.is_empty())
                    {
                        body = body
                            .role(Role::RadioGroup)
                            .aria_label(field.title.as_deref().unwrap_or(&field.key).to_owned());
                    }
                    let mut label = div()
                        .flex()
                        .flex_row_from(strings.direction())
                        .items_center()
                        .gap(px(6.))
                        .child(
                            div()
                                .css_text(0.78125, 1.125, cx)
                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                .child(
                                    field
                                        .title
                                        .as_ref()
                                        .filter(|title| !title.trim().is_empty())
                                        .unwrap_or(&field.key)
                                        .clone(),
                                ),
                        );
                    if field.required {
                        label = label.child(
                            div()
                                .text_color(palette.muted_foreground)
                                .css_text(0.75, 1.0625, cx)
                                .child(strings.get("chat.formCard.required")),
                        );
                    }
                    body = body.child(label);
                    if let Some(description) = &field.description {
                        body = body.child(
                            crate::markdown_view::markdown(
                                &format!("form-{id}-{}-description", field.key),
                                description,
                                palette,
                                crate::markdown_view::Author::Assistant,
                            )
                            .text_size(px(12.)),
                        );
                    }
                    match &field.kind {
                        FormFieldKind::External { url } => {
                            let url = url.clone();
                            body = body.child(url.clone()).child(button(
                                format!("form-{id}-link"),
                                strings.get("chat.formCard.openLink"),
                                enabled,
                                Control::Ghost,
                                palette,
                                (window, cx),
                                move |_, window, cx| {
                                    crate::markdown_links::open_user_link(&url, window, cx)
                                },
                            ));
                        }
                        FormFieldKind::Boolean { .. } => {
                            let captured = key.clone();
                            let field_key = field.key.clone();
                            let checked = matches!(
                                draft.values.get(&field.key),
                                Some(FormValue::Boolean(true))
                            );
                            body = body.child(button(
                                format!("form-input-{id}-{}", field.key),
                                strings.get("chat.formCard.yes"),
                                enabled,
                                Control::Check(checked),
                                palette,
                                (window, cx),
                                move |this, _, cx| {
                                    if !this.request_actionable(&captured) {
                                        return;
                                    }
                                    if let Some(draft) = this.blocking.drafts.get_mut(&captured) {
                                        let checked = matches!(
                                            draft.values.get(&field_key),
                                            Some(FormValue::Boolean(true))
                                        );
                                        draft.values.insert(
                                            field_key.clone(),
                                            FormValue::Boolean(!checked),
                                        );
                                    }
                                    this.acknowledge_visible_form(cx);
                                    cx.notify();
                                },
                            ));
                        }
                        _ => {
                            let (options, custom) = match &field.kind {
                                FormFieldKind::String {
                                    options, custom, ..
                                } => (options.as_deref().unwrap_or_default(), *custom),
                                FormFieldKind::Multiselect {
                                    options, custom, ..
                                } => (options.as_slice(), *custom),
                                _ => (&[][..], false),
                            };
                            for (ordinal, option) in options.iter().enumerate() {
                                let selected = match draft.values.get(&field.key) {
                                    Some(FormValue::String(value)) => {
                                        value == &option.value && !draft.custom.contains(&field.key)
                                    }
                                    Some(FormValue::Strings(values)) => {
                                        values.contains(&option.value)
                                    }
                                    _ => false,
                                };
                                let captured = key.clone();
                                let field_key = field.key.clone();
                                body = body.child(button(
                                    format!("form-option-{id}-{}-{ordinal}", field.key),
                                    option_label(
                                        &option.label,
                                        &strings.get("chat.questionCard.recommended"),
                                    )
                                    .into(),
                                    enabled,
                                    if matches!(field.kind, FormFieldKind::Multiselect { .. }) {
                                        Control::Check(selected)
                                    } else {
                                        Control::Radio(selected)
                                    },
                                    palette,
                                    (window, cx),
                                    move |this, _, cx| {
                                        this.choose_form_option(
                                            captured.clone(),
                                            field_key.clone(),
                                            ordinal,
                                            cx,
                                        )
                                    },
                                ));
                                if let Some(description) = &option.description {
                                    body = body.child(
                                        crate::markdown_view::markdown(
                                            &format!(
                                                "form-{id}-{}-option-{ordinal}-description",
                                                field.key
                                            ),
                                            description,
                                            palette,
                                            crate::markdown_view::Author::Assistant,
                                        )
                                        .text_size(px(12.))
                                        .text_color(palette.muted_foreground)
                                        .padding_start(strings.direction(), px(28.)),
                                    );
                                }
                            }
                            if custom && !options.is_empty() {
                                let captured = key.clone();
                                let field_key = field.key.clone();
                                body = body.child(button(
                                    format!("form-{id}-{}-custom", field.key),
                                    strings.get("chat.formCard.other"),
                                    enabled,
                                    Control::Custom(draft.custom.contains(&field.key)),
                                    palette,
                                    (window, cx),
                                    move |this, _, cx| {
                                        if !this.request_actionable(&captured) {
                                            return;
                                        }
                                        if let Some(draft) = this.blocking.drafts.get_mut(&captured)
                                        {
                                            draft.custom.insert(field_key.clone());
                                        }
                                        this.acknowledge_visible_form(cx);
                                        cx.notify();
                                    },
                                ));
                            }
                            if (options.is_empty() || draft.custom.contains(&field.key))
                                && let Some(input) = draft.inputs.get(&field.key)
                            {
                                body = body.child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "form-input-{id}-{}",
                                            field.key
                                        )))
                                        .test_support()
                                        .child(
                                            Textarea::new(input)
                                                .disabled(!enabled)
                                                .h(px(40.))
                                                .px(px(8.))
                                                .py(px(4.))
                                                .text_size(px(12.5))
                                                .accessibility_id(format!(
                                                    "form-input-{id}-{}",
                                                    field.key
                                                ))
                                                .aria_label(
                                                    field
                                                        .title
                                                        .as_ref()
                                                        .unwrap_or(&field.key)
                                                        .clone(),
                                                ),
                                        ),
                                );
                            }
                        }
                    }
                }
                let mut actions = div()
                    .flex()
                    .flex_row_from(strings.direction())
                    .items_center()
                    .gap(px(6.))
                    .px(px(12.))
                    .pt(px(4.))
                    .pb(px(8.));
                let captured = key.clone();
                actions = actions
                    .child(button(
                        format!("form-{id}-cancel"),
                        strings.get("chat.questionCard.dismiss"),
                        enabled,
                        Control::Ghost,
                        palette,
                        (window, cx),
                        move |this, window, cx| {
                            this.reply_request(captured.clone(), RequestReply::Dismiss, window, cx)
                        },
                    ))
                    .child(div().flex_1().min_w_0());
                let captured = key.clone();
                if step > 0 {
                    actions = actions.child(button(
                        format!("form-{id}-back"),
                        strings.get("chat.formDock.back"),
                        enabled && step > 0,
                        Control::Raised,
                        palette,
                        (window, cx),
                        move |this, _, cx| {
                            this.set_form_step(captured.clone(), step.saturating_sub(1), cx)
                        },
                    ));
                }
                let captured = key.clone();
                let last = step + 1 >= shown.len();
                actions = actions.child(button(
                    format!("form-{id}-{}", if last { "submit" } else { "next" }),
                    strings.get(if last {
                        "chat.formCard.submit"
                    } else {
                        "chat.questionCard.next"
                    }),
                    enabled && (!last || draft.missing(cx, true).is_none()),
                    Control::Advance(last),
                    palette,
                    (window, cx),
                    move |this, window, cx| this.advance_form(captured.clone(), window, cx),
                ));
                footer = actions;
                let mut copy = div().flex().flex_row_from(strings.direction()).gap(px(6.));
                for json in [false, true] {
                    let captured = key.clone();
                    copy = copy.child(button(
                        format!("form-{id}-copy-{}", if json { "json" } else { "markdown" }),
                        strings.get(if json {
                            "chat.questionCard.copyJson"
                        } else {
                            "chat.questionCard.copyMarkdown"
                        }),
                        true,
                        Control::Icon(if json { "code" } else { "file-text" }),
                        palette,
                        (window, cx),
                        move |this, _, cx| {
                            if let Some(draft) = this.blocking.drafts.get(&captured) {
                                let text = if json {
                                    crate::form_copy::json(&draft.form)
                                } else {
                                    crate::form_copy::markdown(&draft.form)
                                };
                                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
                            }
                        },
                    ));
                }
                header = header.child(copy);
                if draft.show_errors && draft.missing(cx, false).is_some() {
                    body = body.child(strings.get("chat.formCard.missingRequired"));
                }
            }
        }
        panel = panel.child(header).child(body).child(footer);
        if entry.response == ResponseState::Uncertain
            || !entry.current
            || self.blocking.errors.contains(&key)
        {
            let captured = key.clone();
            panel = panel.child(button(
                format!("request-{id}-check"),
                strings.get("chat.nativeRecovery.check"),
                entry.response != ResponseState::Responding,
                Control::Raised,
                palette,
                (window, cx),
                move |this, window, cx| this.check_request(captured.clone(), window, cx),
            ));
            if self.blocking.errors.contains(&key) {
                panel = panel.child(strings.get("chat.formCard.submitFailed"));
            }
        }
        let captured = key;
        panel = panel.on_key_down(cx.listener(
            move |this, event: &gpui_kit::KeyDownEvent, window, cx| {
                if event.keystroke.key == "enter"
                    && (event.keystroke.modifiers.control || event.keystroke.modifiers.platform)
                {
                    let composing = this.blocking.drafts.get(&captured).is_some_and(|draft| {
                        draft.inputs.values().any(|input| {
                            input.update(cx, |input, cx| {
                                input.marked_text_range(window, cx).is_some()
                            })
                        })
                    });
                    if !composing {
                        cx.stop_propagation();
                        this.submit_form(captured.clone(), window, cx);
                    }
                }
            },
        ));
        Some(panel.into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, AppContext, ElementId, Entity, TestAppContext};
    use openchamber_native_core::form::{FormField, FormOption, FormRequest};
    use openchamber_native_core::request_owner::RequestOwner;
    use openchamber_native_transport::event::BlockingRequestEvent;

    fn open_form(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Workspace>) {
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                let session = this
                    .sidebar_items
                    .iter()
                    .find_map(|item| match item {
                        crate::sidebar_item::SidebarItem::Session { session_id, .. } => {
                            Some(session_id.clone())
                        }
                        _ => None,
                    })
                    .expect("synthetic session");
                this.select_session(session.clone(), window, cx);
                let mut connection =
                    crate::connection::Connection::new(crate::connection::ServerConfig {
                        endpoint: openchamber_native_transport::endpoint::Endpoint::parse(
                            "http://localhost:1",
                        )
                        .expect("fixture URL"),
                        password: None,
                        client: openchamber_native_transport::client::ClientDescription {
                            label: "Test".into(),
                            kind: "desktop".into(),
                            dedupe_key: None,
                            device_platform: None,
                            app_version: None,
                        },
                    })
                    .expect("network runtime");
                connection.state = crate::connection::ConnectionState::Connected;
                connection.client = Some(
                    openchamber_native_transport::client::ServerClient::new(
                        connection.config.endpoint.clone(),
                        openchamber_native_transport::credential::Credential::None,
                    )
                    .expect("fixture client"),
                );
                this.connection = Some(connection);
                let directory = this
                    .sessions
                    .get(&session)
                    .expect("session")
                    .resolved_directory();
                this.blocking_event(
                    directory,
                    BlockingRequestEvent::Asked(PendingRequest::Form(FormRequest {
                        id: "frm_accessible".into(),
                        owner: RequestOwner::Session(session),
                        title: "Accessible form".into(),
                        metadata_display: None,
                        fields: vec![
                            FormField {
                                key: "agree".into(),
                                title: None,
                                description: None,
                                required: false,
                                hidden: false,
                                when: Vec::new(),
                                kind: FormFieldKind::Boolean { default: None },
                            },
                            FormField {
                                key: "single".into(),
                                title: None,
                                description: None,
                                required: false,
                                hidden: false,
                                when: Vec::new(),
                                kind: FormFieldKind::String {
                                    format: None,
                                    min_length: None,
                                    max_length: None,
                                    pattern: None,
                                    placeholder: None,
                                    default: None,
                                    custom: true,
                                    options: Some(vec![FormOption {
                                        value: "one".into(),
                                        label: "One".into(),
                                        description: None,
                                    }]),
                                },
                            },
                            FormField {
                                key: "multi".into(),
                                title: None,
                                description: None,
                                required: false,
                                hidden: false,
                                when: Vec::new(),
                                kind: FormFieldKind::Multiselect {
                                    options: vec![FormOption {
                                        value: "one".into(),
                                        label: "One".into(),
                                        description: None,
                                    }],
                                    custom: true,
                                    default: None,
                                    min_items: None,
                                    max_items: None,
                                },
                            },
                        ],
                    })),
                    window,
                    cx,
                );
                cx.notify();
            });
        })
        .expect("open form");
        (window, workspace)
    }

    #[gpui_kit::test]
    fn rendered_dock_exposes_dialog_and_expand_state(cx: &mut TestAppContext) {
        let (window, workspace) = open_form(cx);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.find(ElementId::Name("request-dock".into())).role(),
                Some(Role::Dialog)
            );
            assert_eq!(
                window.find(ElementId::Name("request-dock".into())).label(),
                Some("Accessible form")
            );
            let collapse = ElementId::Name("request-frm_accessible-collapse".into());
            assert_eq!(window.find(collapse.clone()).expanded(), Some(true));
            window.click(collapse.clone(), cx);
            window.render_frame(cx);
            assert_eq!(window.find(collapse).expanded(), Some(false));
            window.click(
                ElementId::Name("request-frm_accessible-collapse".into()),
                cx,
            );
            window.render_frame(cx);
            for (step, control, role) in [
                (0, "form-input-frm_accessible-agree", Role::CheckBox),
                (1, "form-option-frm_accessible-single-0", Role::RadioButton),
                (2, "form-option-frm_accessible-multi-0", Role::CheckBox),
            ] {
                let tab = ElementId::Name(format!("form-frm_accessible-step-{step}").into());
                window.click(tab.clone(), cx);
                window.render_frame(cx);
                assert_eq!(window.find(tab.clone()).role(), Some(Role::Tab));
                assert_eq!(window.find(tab).selected(), Some(true));
                let control = ElementId::Name(control.into());
                assert_eq!(window.find(control.clone()).role(), Some(role));
                assert_eq!(window.find(control.clone()).checked(), Some(false));
                window.click(control.clone(), cx);
                window.render_frame(cx);
                assert_eq!(window.find(control.clone()).checked(), Some(true));
                if role == Role::CheckBox {
                    assert_eq!(window.find(control.clone()).focused(), Some(true));
                    window.press("space", cx);
                    window.render_frame(cx);
                    assert_eq!(window.find(control.clone()).checked(), Some(false));
                    window.press("space", cx);
                    window.render_frame(cx);
                    assert_eq!(window.find(control).checked(), Some(true));
                }
            }
            workspace.update(cx, |this, cx| {
                this.connection.as_mut().expect("fixture connection").state =
                    crate::connection::ConnectionState::Connecting;
                cx.notify();
            });
            window.render_frame(cx);
            let control = ElementId::Name("form-option-frm_accessible-multi-0".into());
            window.click(control.clone(), cx);
            window.render_frame(cx);
            assert_eq!(
                window.find(control).checked(),
                Some(true),
                "locked option ignores activation"
            );
        })
        .expect("render dock");
    }

    #[gpui_kit::test]
    fn other_is_an_edit_button_and_preserves_selected_options(cx: &mut TestAppContext) {
        let (window, workspace) = open_form(cx);
        cx.update_window(window, |_, window, cx| {
            window.click(ElementId::Name("form-frm_accessible-step-2".into()), cx);
            let option = ElementId::Name("form-option-frm_accessible-multi-0".into());
            let other = ElementId::Name("form-frm_accessible-multi-custom".into());
            let input = ElementId::Name("form-input-frm_accessible-multi".into());
            window.click(option.clone(), cx);
            assert_eq!(window.find(other.clone()).role(), Some(Role::Button));
            assert_eq!(window.find(other.clone()).checked(), None);
            window.click(other.clone(), cx);
            window.click(input.clone(), cx);
            window.input("Custom answer", cx);
            let answer = |cx: &gpui_kit::App| {
                let workspace = workspace.read(cx);
                let key = workspace.active_request().expect("active form");
                workspace.blocking.drafts[&key].answer(cx)
            };
            let expected = Some(FormValue::Strings(vec![
                "one".into(),
                "Custom answer".into(),
            ]));
            assert_eq!(answer(cx).get("multi").cloned(), expected);
            window.click(other.clone(), cx);
            for key in ["space", "enter"] {
                window.press(key, cx);
                assert_eq!(window.find(other.clone()).focused(), Some(true));
                assert_eq!(window.find(option.clone()).checked(), Some(true));
                assert_eq!(answer(cx).get("multi").cloned(), expected);
            }
            // The reference removes a custom answer by clearing its text.
            window.click(input, cx);
            window.press("ctrl-a", cx);
            window.press("backspace", cx);
            assert_eq!(
                answer(cx).get("multi"),
                Some(&FormValue::Strings(vec!["one".into()]))
            );
            assert_eq!(window.find(option).checked(), Some(true));
        })
        .expect("edit custom answer");
    }

    fn keyboard_submit(cx: &mut TestAppContext, key: &str) {
        let (window, workspace) = open_form(cx);
        cx.update_window(window, |_, window, cx| {
            let next = ElementId::Name("form-frm_accessible-next".into());
            let submit = ElementId::Name("form-frm_accessible-submit".into());
            for (step, control) in [
                (0, "form-input-frm_accessible-agree"),
                (1, "form-option-frm_accessible-single-0"),
                (2, "form-option-frm_accessible-multi-0"),
            ] {
                window.click(
                    ElementId::Name(format!("form-frm_accessible-step-{step}").into()),
                    cx,
                );
                window.click(ElementId::Name(control.into()), cx);
            }
            window.click(ElementId::Name("form-frm_accessible-step-0".into()), cx);
            window.click(next.clone(), cx);
            assert_eq!(window.find(next).focused(), Some(true));
            window.press(key, cx);
            assert_eq!(
                window
                    .find(ElementId::Name("form-frm_accessible-step-2".into()))
                    .selected(),
                Some(true)
            );
            assert_eq!(window.find(submit).focused(), Some(true));
            window.press(key, cx);
            let workspace = workspace.read(cx);
            let key = workspace
                .active_request()
                .expect("responding form remains visible");
            assert_eq!(
                workspace.blocking.stores[&key.scope]
                    .get(&key)
                    .expect("form")
                    .response,
                ResponseState::Responding,
                "the second key activation submits the form"
            );
        })
        .expect("submit with retained focus");
    }

    #[gpui_kit::test]
    fn next_to_submit_keeps_enter_activation(cx: &mut TestAppContext) {
        keyboard_submit(cx, "enter");
    }

    #[gpui_kit::test]
    fn next_to_submit_keeps_space_activation(cx: &mut TestAppContext) {
        keyboard_submit(cx, "space");
    }
}
