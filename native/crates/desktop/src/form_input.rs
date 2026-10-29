//! Typed in-memory form drafts. Text widgets retain their own edits and IME state.

use std::collections::{BTreeMap, BTreeSet};

use gpui_kit::component::input::{InputEvent, TextareaState};
use gpui_kit::{App, AppContext, Context, Entity, EntityInputHandler, Subscription, Window};
use openchamber_native_core::form::{
    FormAnswer, FormField, FormFieldKind, FormNumber, FormRequest, FormValue,
};
use openchamber_native_core::pending_requests::RequestKey;

use crate::blocking_requests::RequestReply;
use crate::workspace::Workspace;

pub(crate) struct FormDraft {
    pub form: FormRequest,
    pub values: FormAnswer,
    pub inputs: BTreeMap<String, Entity<TextareaState>>,
    pub custom: BTreeSet<String>,
    pub step: usize,
    pub show_errors: bool,
    _subscriptions: Vec<Subscription>,
}

fn trim(value: &str) -> &str {
    value.trim_matches(|c| matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' |
        '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'))
}

impl FormDraft {
    pub fn same_fields(&self, form: &FormRequest) -> bool {
        self.form.fields.len() == form.fields.len()
            && self.form.fields.iter().zip(&form.fields).all(|(old, new)| {
                old.key == new.key
                    && std::mem::discriminant(&old.kind) == std::mem::discriminant(&new.kind)
            })
    }

    pub fn new(
        key: RequestKey,
        form: FormRequest,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let values = form.defaults();
        let mut inputs = BTreeMap::new();
        let mut custom = BTreeSet::new();
        let mut subscriptions = Vec::new();
        for field in &form.fields {
            let initial = match &field.kind {
                FormFieldKind::String {
                    default, options, ..
                } => {
                    let selected = default.as_ref().is_some_and(|value| {
                        options.as_ref().is_some_and(|options| {
                            options.iter().any(|option| &option.value == value)
                        })
                    });
                    if !selected
                        && default.is_some()
                        && options.as_ref().is_some_and(|options| !options.is_empty())
                    {
                        custom.insert(field.key.clone());
                    }
                    if selected {
                        String::new()
                    } else {
                        default.clone().unwrap_or_default()
                    }
                }
                FormFieldKind::Number { default, .. } | FormFieldKind::Integer { default, .. } => {
                    default
                        .map(|number| number.get().to_string())
                        .unwrap_or_default()
                }
                FormFieldKind::Multiselect {
                    options,
                    custom: allowed,
                    ..
                } => {
                    if options.is_empty() && *allowed {
                        custom.insert(field.key.clone());
                    }
                    String::new()
                }
                _ => continue,
            };
            let placeholder = match &field.kind {
                FormFieldKind::String {
                    placeholder: Some(value),
                    ..
                } => value.clone().into(),
                _ => crate::locale::strings(cx).get("chat.formCard.yourAnswer"),
            };
            let input = cx.new(|cx| {
                let mut state = TextareaState::new(window, cx)
                    .auto_grow(1, 5)
                    .submit_on_enter(true)
                    .placeholder(placeholder);
                state.set_value(initial, window, cx);
                state
            });
            let captured = key.clone();
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                move |this, input, event, window, cx| {
                    if !this.request_actionable(&captured) {
                        return;
                    }
                    match event {
                        InputEvent::Change => {
                            this.acknowledge_visible_form(cx);
                            cx.notify();
                        }
                        InputEvent::PressEnter {
                            secondary,
                            shift: false,
                        } => {
                            let composing = input.update(cx, |input, cx| {
                                input.marked_text_range(window, cx).is_some()
                            });
                            if !composing {
                                if *secondary {
                                    this.submit_form(captured.clone(), window, cx);
                                } else {
                                    this.advance_form(captured.clone(), window, cx);
                                }
                            }
                        }
                        _ => {}
                    }
                },
            ));
            inputs.insert(field.key.clone(), input);
        }
        Self {
            form,
            values,
            inputs,
            custom,
            step: 0,
            show_errors: false,
            _subscriptions: subscriptions,
        }
    }

    pub fn answer(&self, cx: &App) -> FormAnswer {
        let mut values = self.values.clone();
        for field in &self.form.fields {
            let Some(input) = self.inputs.get(&field.key) else {
                continue;
            };
            let text = input.read(cx).value();
            let text = trim(&text);
            let value = match &field.kind {
                FormFieldKind::String { options, .. }
                    if options.as_ref().is_none_or(|options| options.is_empty())
                        || self.custom.contains(&field.key) =>
                {
                    (!text.is_empty()).then(|| FormValue::String(text.to_owned()))
                }
                FormFieldKind::Number { .. } | FormFieldKind::Integer { .. } => text
                    .parse::<f64>()
                    .ok()
                    .and_then(FormNumber::new)
                    .map(FormValue::Number),
                FormFieldKind::Multiselect { .. } => {
                    let mut selected = match values.get(&field.key) {
                        Some(FormValue::Strings(values)) => values.clone(),
                        _ => Vec::new(),
                    };
                    if self.custom.contains(&field.key) && !text.is_empty() {
                        selected.push(text.to_owned());
                    }
                    (!selected.is_empty()).then_some(FormValue::Strings(selected))
                }
                _ => continue,
            };
            if let Some(value) = value {
                values.insert(field.key.clone(), value);
            } else {
                values.remove(&field.key);
            }
        }
        self.form.evaluate(&values).1
    }

    pub fn shown(&self, cx: &App) -> Vec<&FormField> {
        self.form.evaluate(&self.answer(cx)).0
    }

    pub fn missing(&self, cx: &App, all: bool) -> Option<usize> {
        let answer = self.answer(cx);
        self.shown(cx).iter().position(|field| {
            (all || field.required || matches!(field.kind, FormFieldKind::External { .. }))
                && answer.get(&field.key).is_none_or(
                    |value| matches!(value, FormValue::Strings(values) if values.is_empty()),
                )
        })
    }
}

impl Workspace {
    pub(crate) fn acknowledge_visible_form(&mut self, cx: &App) {
        let active = self.active_request();
        if self.blocking.presented != active {
            self.blocking.presented = active.clone();
            self.blocking.collapsed = false;
        }
        if self.blocking.collapsed {
            return;
        }
        let Some(key) = active else {
            return;
        };
        let Some(draft) = self.blocking.drafts.get_mut(&key) else {
            return;
        };
        let shown = draft.shown(cx);
        let step = draft.step.min(shown.len().saturating_sub(1));
        let external = shown
            .get(step)
            .filter(|field| matches!(field.kind, FormFieldKind::External { .. }))
            .map(|field| field.key.clone());
        draft.step = step;
        if let Some(field) = external {
            draft.values.insert(field, FormValue::Boolean(true));
        }
    }

    pub fn set_form_step(&mut self, key: RequestKey, step: usize, cx: &mut Context<Self>) {
        if !self.request_actionable(&key) {
            return;
        }
        if let Some(draft) = self.blocking.drafts.get_mut(&key) {
            draft.step = step;
        }
        self.acknowledge_visible_form(cx);
        cx.notify();
    }

    pub fn choose_form_option(
        &mut self,
        key: RequestKey,
        field: String,
        option: usize,
        cx: &mut Context<Self>,
    ) {
        if !self.request_actionable(&key) {
            return;
        }
        let Some(draft) = self.blocking.drafts.get_mut(&key) else {
            return;
        };
        let Some(definition) = draft.form.fields.iter().find(|entry| entry.key == field) else {
            return;
        };
        match &definition.kind {
            FormFieldKind::String {
                options: Some(options),
                ..
            } => {
                let Some(option) = options.get(option) else {
                    return;
                };
                draft
                    .values
                    .insert(field.clone(), FormValue::String(option.value.clone()));
                draft.custom.remove(&field);
            }
            FormFieldKind::Multiselect { options, .. } => {
                let Some(option) = options.get(option) else {
                    return;
                };
                let mut selected = match draft.values.get(&field) {
                    Some(FormValue::Strings(values)) => values.clone(),
                    _ => Vec::new(),
                };
                if selected.contains(&option.value) {
                    selected.retain(|value| value != &option.value);
                } else {
                    selected.push(option.value.clone());
                }
                draft.values.insert(field, FormValue::Strings(selected));
            }
            _ => return,
        }
        self.acknowledge_visible_form(cx);
        cx.notify();
    }

    pub fn advance_form(&mut self, key: RequestKey, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.blocking.drafts.get(&key) else {
            return;
        };
        if draft.step + 1 >= draft.shown(cx).len() {
            self.submit_form(key, window, cx);
        } else {
            self.set_form_step(key, draft.step + 1, cx);
        }
    }

    pub fn submit_form(&mut self, key: RequestKey, window: &mut Window, cx: &mut Context<Self>) {
        if !self.request_actionable(&key) {
            return;
        }
        let Some(draft) = self.blocking.drafts.get_mut(&key) else {
            return;
        };
        // The reference gates required presence. Format and regex validation stay server-owned.
        if let Some(missing) = draft.missing(cx, false) {
            draft.step = missing;
            draft.show_errors = true;
            self.acknowledge_visible_form(cx);
            cx.notify();
            return;
        }
        let answer = draft.answer(cx);
        self.reply_request(key, RequestReply::Form(answer), window, cx);
    }
}
