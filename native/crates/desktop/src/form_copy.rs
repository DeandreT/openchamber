//! Explicit clipboard exports exclude transient request and owner identities.

use openchamber_native_core::form::{FormFieldKind, FormRequest, FormValue, WhenOp};
use serde_json::{Value, json as value};

fn answer(value: &FormValue) -> Value {
    match value {
        FormValue::String(value) => value!(value),
        FormValue::Strings(value) => value!(value),
        FormValue::Number(value) => value!(value.get()),
        FormValue::Boolean(value) => value!(value),
    }
}

pub(crate) fn json(form: &FormRequest) -> String {
    let fields: Vec<_> = form.fields.iter().map(|field| {
        let options = |options: &[openchamber_native_core::form::FormOption]| options.iter().map(|option| value!({"value":option.value,"label":option.label,"description":option.description})).collect::<Vec<_>>();
        let mut result = match &field.kind {
            FormFieldKind::String { format, min_length, max_length, pattern, placeholder, default, options: choices, custom } => value!({"type":"string","format":format,"minLength":min_length,"maxLength":max_length,"pattern":pattern,"placeholder":placeholder,"default":default,"options":choices.as_ref().map(|choices|options(choices)),"custom":custom}),
            FormFieldKind::Number { minimum, maximum, default } | FormFieldKind::Integer { minimum, maximum, default } => value!({"type":if matches!(field.kind,FormFieldKind::Integer { .. }) {"integer"} else {"number"},"minimum":minimum.map(|n|n.get()),"maximum":maximum.map(|n|n.get()),"default":default.map(|n|n.get())}),
            FormFieldKind::Boolean { default } => value!({"type":"boolean","default":default}),
            FormFieldKind::Multiselect { options: choices, min_items, max_items, custom, default } => value!({"type":"multiselect","options":options(choices),"minItems":min_items,"maxItems":max_items,"custom":custom,"default":default}),
            FormFieldKind::External { url } => value!({"type":"external","url":url}),
        };
        let object = result.as_object_mut().expect("field object");
        object.insert("key".into(), value!(field.key));
        object.insert("title".into(), value!(field.title));
        object.insert("description".into(), value!(field.description));
        if !matches!(field.kind, FormFieldKind::External { .. }) {
            object.insert("required".into(), value!(field.required));
            object.insert("hidden".into(), value!(field.hidden));
            if !field.when.is_empty() { object.insert("when".into(), value!(field.when.iter().map(|clause|value!({"key":clause.key,"op":match clause.op { WhenOp::Equal => "eq", WhenOp::NotEqual => "neq" },"value":answer(&clause.value)})).collect::<Vec<_>>())); }
        }
        remove_nulls(&mut result);
        result
    }).collect();
    serde_json::to_string_pretty(&value!({"title":form.title,"fields":fields}))
        .expect("finite form values serialize")
}

fn remove_nulls(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.retain(|_, value| !value.is_null());
            for value in object.values_mut() {
                remove_nulls(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                remove_nulls(value);
            }
        }
        _ => {}
    }
}

pub(crate) fn markdown(form: &FormRequest) -> String {
    let mut lines = Vec::new();
    if !form.title.trim().is_empty() {
        lines.extend([format!("# {}", form.title.trim()), String::new()]);
    }
    for field in &form.fields {
        lines.extend([
            format!(
                "## {}",
                field
                    .title
                    .as_deref()
                    .map(str::trim)
                    .filter(|title| !title.is_empty())
                    .unwrap_or(&field.key)
            ),
            String::new(),
        ]);
        if let Some(description) = field
            .description
            .as_deref()
            .map(str::trim)
            .filter(|description| !description.is_empty())
        {
            lines.extend([description.to_owned(), String::new()]);
        }
        let options = match &field.kind {
            FormFieldKind::External { url } => {
                lines.extend([format!("<{url}>"), String::new()]);
                continue;
            }
            FormFieldKind::String { options, .. } => options.as_deref().unwrap_or_default(),
            FormFieldKind::Multiselect { options, .. } => {
                lines.extend(["_Select all that apply._".into(), String::new()]);
                options
            }
            _ => &[],
        };
        for option in options {
            lines.push(
                match option
                    .description
                    .as_deref()
                    .map(str::trim)
                    .filter(|description| !description.is_empty())
                {
                    Some(description) => format!("- **{}** — {description}", option.label),
                    None => format!("- **{}**", option.label),
                },
            );
        }
        if !options.is_empty() {
            lines.push(String::new());
        }
    }
    lines.join("\n").trim_end().to_owned()
}
