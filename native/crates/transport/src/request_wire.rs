//! Private OpenCode 2.2 permission/form decoding. No wire values leave transport.

use crate::requests::RequestError;
use openchamber_native_core::form::{
    FormAnswer, FormDetail, FormField, FormFieldKind, FormNumber, FormOption, FormRequest,
    FormState, FormValue, FormWhen, WhenOp,
};
use openchamber_native_core::ids::{FormId, MessageId, PermissionId};
use openchamber_native_core::permission::{PermissionRequest, PermissionSource};
use openchamber_native_core::request_owner::RequestOwner;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Permission {
    id: String,
    #[serde(rename = "sessionID")]
    session_id: String,
    action: String,
    resources: Vec<String>,
    #[serde(default)]
    save: Vec<String>,
    metadata: Option<BTreeMap<String, Value>>,
    source: Option<Source>,
    message: Option<String>,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Source {
    Tool {
        #[serde(rename = "messageID")]
        message_id: String,
        id: String,
    },
}
impl Permission {
    pub(crate) fn project(self) -> Result<PermissionRequest, RequestError> {
        let Some(RequestOwner::Session(session_id)) = RequestOwner::from_wire(&self.session_id)
        else {
            return Err(RequestError::InvalidResponse);
        };
        if !valid_id(&self.id, "per") {
            return Err(RequestError::InvalidResponse);
        }
        Ok(PermissionRequest {
            id: PermissionId::new(self.id),
            session_id,
            action: self.action,
            resources: self.resources,
            save: self.save,
            message: self.message,
            metadata_display: display_metadata(self.metadata),
            source: self
                .source
                .map(|Source::Tool { message_id, id }| PermissionSource {
                    message_id: MessageId::new(message_id),
                    call_id: id,
                }),
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Form {
    id: String,
    #[serde(rename = "sessionID")]
    session_id: String,
    title: String,
    metadata: Option<BTreeMap<String, Value>>,
    fields: Vec<Field>,
}
#[derive(Deserialize)]
struct Field {
    key: String,
    title: Option<String>,
    description: Option<String>,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    when: Vec<When>,
    #[serde(flatten)]
    kind: FieldKind,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
enum FieldKind {
    String {
        format: Option<String>,
        #[serde(rename = "minLength")]
        min_length: Option<usize>,
        #[serde(rename = "maxLength")]
        max_length: Option<usize>,
        pattern: Option<String>,
        placeholder: Option<String>,
        default: Option<String>,
        options: Option<Vec<Choice>>,
        #[serde(default)]
        custom: bool,
    },
    Number {
        minimum: Option<Number>,
        maximum: Option<Number>,
        default: Option<Number>,
    },
    Integer {
        minimum: Option<Number>,
        maximum: Option<Number>,
        default: Option<Number>,
    },
    Boolean {
        default: Option<bool>,
    },
    Multiselect {
        options: Vec<Choice>,
        #[serde(rename = "minItems")]
        min_items: Option<usize>,
        #[serde(rename = "maxItems")]
        max_items: Option<usize>,
        #[serde(default)]
        custom: bool,
        default: Option<Vec<String>>,
    },
    External {
        url: String,
    },
}
#[derive(Deserialize)]
struct Choice {
    value: String,
    label: String,
    description: Option<String>,
}
impl Choice {
    fn project(self) -> FormOption {
        FormOption {
            value: self.value,
            label: self.label,
            description: self.description,
        }
    }
}
#[derive(Deserialize)]
struct When {
    key: String,
    op: Op,
    value: Scalar,
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Op {
    Eq,
    Neq,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Scalar {
    String(String),
    Number(f64),
    Boolean(bool),
}
impl Scalar {
    fn project(self) -> Result<FormValue, RequestError> {
        Ok(match self {
            Self::String(v) => FormValue::String(v),
            Self::Boolean(v) => FormValue::Boolean(v),
            Self::Number(v) => {
                FormValue::Number(FormNumber::new(v).ok_or(RequestError::InvalidResponse)?)
            }
        })
    }
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Number {
    Finite(f64),
    Sentinel(Sentinel),
}
#[derive(Deserialize)]
enum Sentinel {
    Infinity,
    #[serde(rename = "-Infinity")]
    NegativeInfinity,
    NaN,
}
impl Number {
    fn finite(self) -> Option<FormNumber> {
        match self {
            Self::Finite(value) => FormNumber::new(value),
            Self::Sentinel(value) => {
                let _ = value;
                None
            }
        }
    }
}
impl Form {
    pub(crate) fn project(self) -> Result<FormRequest, RequestError> {
        if !valid_id(&self.id, "frm_") {
            return Err(RequestError::InvalidResponse);
        }
        let mut fields = Vec::with_capacity(self.fields.len());
        for field in self.fields {
            let kind = match field.kind {
                FieldKind::String {
                    format,
                    min_length,
                    max_length,
                    pattern,
                    placeholder,
                    default,
                    options,
                    custom,
                } => {
                    if format
                        .as_deref()
                        .is_some_and(|f| !matches!(f, "email" | "uri" | "date" | "date-time"))
                    {
                        return Err(RequestError::InvalidResponse);
                    }
                    FormFieldKind::String {
                        format,
                        min_length,
                        max_length,
                        pattern,
                        placeholder,
                        default,
                        options: options.map(|v| v.into_iter().map(Choice::project).collect()),
                        custom,
                    }
                }
                FieldKind::Number {
                    minimum,
                    maximum,
                    default,
                } => FormFieldKind::Number {
                    minimum: minimum.and_then(Number::finite),
                    maximum: maximum.and_then(Number::finite),
                    default: default.and_then(Number::finite),
                },
                FieldKind::Integer {
                    minimum,
                    maximum,
                    default,
                } => FormFieldKind::Integer {
                    minimum: minimum.and_then(Number::finite),
                    maximum: maximum.and_then(Number::finite),
                    default: default.and_then(Number::finite),
                },
                FieldKind::Boolean { default } => FormFieldKind::Boolean { default },
                FieldKind::Multiselect {
                    options,
                    min_items,
                    max_items,
                    custom,
                    default,
                } => FormFieldKind::Multiselect {
                    options: options.into_iter().map(Choice::project).collect(),
                    min_items,
                    max_items,
                    custom,
                    default,
                },
                FieldKind::External { url } => FormFieldKind::External { url },
            };
            let when = field
                .when
                .into_iter()
                .map(|clause| {
                    Ok(FormWhen {
                        key: clause.key,
                        op: match clause.op {
                            Op::Eq => WhenOp::Equal,
                            Op::Neq => WhenOp::NotEqual,
                        },
                        value: clause.value.project()?,
                    })
                })
                .collect::<Result<Vec<_>, RequestError>>()?;
            fields.push(FormField {
                key: field.key,
                title: field.title,
                description: field.description,
                required: field.required,
                hidden: field.hidden,
                when,
                kind,
            });
        }
        let form = FormRequest {
            id: FormId::new(self.id),
            owner: RequestOwner::from_wire(&self.session_id)
                .ok_or(RequestError::InvalidResponse)?,
            title: self.title,
            metadata_display: display_metadata(self.metadata),
            fields,
        };
        form.validate_definition()
            .map_err(|_| RequestError::InvalidResponse)?;
        Ok(form)
    }
}

#[derive(Deserialize)]
pub(crate) struct Detail {
    #[serde(flatten)]
    form: Form,
    state: State,
}
#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum State {
    Pending,
    Answered {
        answer: BTreeMap<String, AnswerValue>,
    },
    Cancelled {
        message: Option<String>,
    },
}
#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum AnswerValue {
    String(String),
    Number(f64),
    Boolean(bool),
    Strings(Vec<String>),
}
impl Serialize for AnswerValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::String(value) => serializer.serialize_str(value),
            Self::Boolean(value) => serializer.serialize_bool(*value),
            Self::Strings(value) => value.serialize(serializer),
            // JavaScript JSON.stringify writes integral safe numbers without a decimal suffix.
            Self::Number(value)
                if value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0 =>
            {
                serializer.serialize_i64(*value as i64)
            }
            Self::Number(value) => serializer.serialize_f64(*value),
        }
    }
}
impl Detail {
    pub(crate) fn project(self) -> Result<FormDetail, RequestError> {
        let state = match self.state {
            State::Pending => FormState::Pending,
            State::Cancelled { message } => FormState::Cancelled { message },
            State::Answered { answer } => FormState::Answered(
                answer
                    .into_iter()
                    .map(|(key, value)| {
                        let value = match value {
                            AnswerValue::String(v) => FormValue::String(v),
                            AnswerValue::Boolean(v) => FormValue::Boolean(v),
                            AnswerValue::Strings(v) => FormValue::Strings(v),
                            AnswerValue::Number(v) => FormValue::Number(
                                FormNumber::new(v).ok_or(RequestError::InvalidResponse)?,
                            ),
                        };
                        Ok((key, value))
                    })
                    .collect::<Result<_, RequestError>>()?,
            ),
        };
        Ok(FormDetail {
            form: self.form.project()?,
            state,
        })
    }
}
pub(crate) fn answer_wire(answer: &FormAnswer) -> BTreeMap<&str, AnswerValue> {
    answer
        .iter()
        .map(|(key, value)| {
            (
                key.as_str(),
                match value {
                    FormValue::String(v) => AnswerValue::String(v.clone()),
                    FormValue::Number(v) => AnswerValue::Number(v.get()),
                    FormValue::Boolean(v) => AnswerValue::Boolean(*v),
                    FormValue::Strings(v) => AnswerValue::Strings(v.clone()),
                },
            )
        })
        .collect()
}
pub(crate) fn valid_id(value: &str, prefix: &str) -> bool {
    value.starts_with(prefix) && !value.chars().any(char::is_control)
}
fn display_metadata(metadata: Option<BTreeMap<String, Value>>) -> Option<String> {
    let mut text = serde_json::to_string(&metadata?).ok()?;
    const LIMIT: usize = 8192;
    if text.len() > LIMIT {
        let mut end = LIMIT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(" [truncated]");
    }
    Some(text)
}
