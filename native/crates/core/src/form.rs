//! Typed forms and deterministic visibility/validation, independent of JSON and rendering.
//!
//! JavaScript regexp and format validation remain authoritative on the server.
//! `validate_answer` reports those constraints explicitly, never claiming full validation.

use crate::ids::FormId;
use crate::request_owner::RequestOwner;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::{Deref, DerefMut};

/// A finite JSON number. Non-finite wire defaults/bounds normalize to absence in transport.
#[derive(Clone, Copy, PartialEq)]
pub struct FormNumber(f64);
impl fmt::Debug for FormNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FormNumber([redacted])")
    }
}
impl Eq for FormNumber {}
impl FormNumber {
    /// Rejects non-finite answers before serialization.
    pub fn new(value: f64) -> Option<Self> {
        value.is_finite().then_some(Self(value))
    }
    /// The finite value.
    pub fn get(self) -> f64 {
        self.0
    }
}

/// One answer value, with no untyped JSON escape hatch.
#[derive(Clone, PartialEq, Eq)]
pub enum FormValue {
    String(String),
    Number(FormNumber),
    Boolean(bool),
    Strings(Vec<String>),
}
/// Answers by declared field key.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct FormAnswer(BTreeMap<String, FormValue>);
impl FormAnswer {
    /// An empty answer with no implicit acknowledgements.
    pub fn new() -> Self {
        Self::default()
    }
}
impl fmt::Debug for FormAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FormAnswer")
            .field("fields", &self.len())
            .field("content", &"[redacted]")
            .finish()
    }
}
impl Deref for FormAnswer {
    type Target = BTreeMap<String, FormValue>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for FormAnswer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl From<BTreeMap<String, FormValue>> for FormAnswer {
    fn from(value: BTreeMap<String, FormValue>) -> Self {
        Self(value)
    }
}
impl<const N: usize> From<[(String, FormValue); N]> for FormAnswer {
    fn from(value: [(String, FormValue); N]) -> Self {
        Self(BTreeMap::from(value))
    }
}
impl FromIterator<(String, FormValue)> for FormAnswer {
    fn from_iter<T: IntoIterator<Item = (String, FormValue)>>(values: T) -> Self {
        Self(values.into_iter().collect())
    }
}
impl<'a> IntoIterator for &'a FormAnswer {
    type Item = (&'a String, &'a FormValue);
    type IntoIter = std::collections::btree_map::Iter<'a, String, FormValue>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}
impl fmt::Debug for FormValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::String(_) => "String",
            Self::Number(_) => "Number",
            Self::Boolean(_) => "Boolean",
            Self::Strings(_) => "Strings",
        };
        f.debug_tuple(kind).field(&"[redacted]").finish()
    }
}

/// An option's wire value is retained independently from its display label.
#[derive(Clone, PartialEq, Eq)]
pub struct FormOption {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}
/// Comparison against an earlier answer. Multiselect equality means inclusion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhenOp {
    Equal,
    NotEqual,
}
/// A condition with a scalar value matching its earlier field's type.
#[derive(Clone, PartialEq, Eq)]
pub struct FormWhen {
    pub key: String,
    pub op: WhenOp,
    pub value: FormValue,
}

/// The complete pinned field union. External fields have no required/default/when semantics.
#[derive(Clone, PartialEq, Eq)]
pub enum FormFieldKind {
    String {
        format: Option<String>,
        min_length: Option<usize>,
        max_length: Option<usize>,
        pattern: Option<String>,
        placeholder: Option<String>,
        default: Option<String>,
        options: Option<Vec<FormOption>>,
        custom: bool,
    },
    Number {
        minimum: Option<FormNumber>,
        maximum: Option<FormNumber>,
        default: Option<FormNumber>,
    },
    Integer {
        minimum: Option<FormNumber>,
        maximum: Option<FormNumber>,
        default: Option<FormNumber>,
    },
    Boolean {
        default: Option<bool>,
    },
    Multiselect {
        options: Vec<FormOption>,
        min_items: Option<usize>,
        max_items: Option<usize>,
        custom: bool,
        default: Option<Vec<String>>,
    },
    External {
        url: String,
    },
}

/// An ordered field. `hidden` is retained for authentication parity, not automatic approval.
#[derive(Clone, PartialEq, Eq)]
pub struct FormField {
    pub key: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub required: bool,
    pub hidden: bool,
    pub when: Vec<FormWhen>,
    pub kind: FormFieldKind,
}

/// A pending form. Display metadata never changes reply routing.
#[derive(Clone, PartialEq, Eq)]
pub struct FormRequest {
    pub id: FormId,
    pub owner: RequestOwner,
    pub title: String,
    pub metadata_display: Option<String>,
    pub fields: Vec<FormField>,
}

/// Retained server state returned by an exact form read.
#[derive(Clone, PartialEq, Eq)]
pub enum FormState {
    Pending,
    Answered(FormAnswer),
    Cancelled { message: Option<String> },
}
/// A form and its authoritative state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormDetail {
    pub form: FormRequest,
    pub state: FormState,
}

/// Invalid input or a constraint this standard-library-only core cannot fully validate.
#[derive(Clone, PartialEq, Eq)]
pub enum FormValidationError {
    InvalidDefinition(String),
    UnknownField(String),
    InactiveField(String),
    Missing(String),
    InvalidValue(String),
    RequiresServerValidation(Vec<String>),
}

impl fmt::Debug for FormOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FormOption([redacted])")
    }
}
impl fmt::Debug for FormWhen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FormWhen")
            .field("op", &self.op)
            .field("content", &"[redacted]")
            .finish()
    }
}
impl fmt::Debug for FormFieldKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::String { .. } => "String",
            Self::Number { .. } => "Number",
            Self::Integer { .. } => "Integer",
            Self::Boolean { .. } => "Boolean",
            Self::Multiselect { .. } => "Multiselect",
            Self::External { .. } => "External",
        };
        f.debug_tuple(kind).field(&"[redacted]").finish()
    }
}
impl fmt::Debug for FormField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FormField")
            .field("kind", &self.kind)
            .field("required", &self.required)
            .field("content", &"[redacted]")
            .finish()
    }
}
impl fmt::Debug for FormRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FormRequest")
            .field("id", &self.id)
            .field("owner", &self.owner)
            .field("fields", &self.fields)
            .field("content", &"[redacted]")
            .finish()
    }
}
impl fmt::Debug for FormState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => f.write_str("Pending"),
            Self::Answered(answer) => f.debug_tuple("Answered").field(answer).finish(),
            Self::Cancelled { .. } => f.write_str("Cancelled { message: [redacted] }"),
        }
    }
}
impl fmt::Debug for FormValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::InvalidDefinition(_) => "InvalidDefinition",
            Self::UnknownField(_) => "UnknownField",
            Self::InactiveField(_) => "InactiveField",
            Self::Missing(_) => "Missing",
            Self::InvalidValue(_) => "InvalidValue",
            Self::RequiresServerValidation(_) => "RequiresServerValidation",
        };
        f.debug_tuple(kind).field(&"[redacted]").finish()
    }
}

impl FormRequest {
    /// Validates field identity and conditional references before granting the form authority.
    pub fn validate_definition(&self) -> Result<(), FormValidationError> {
        if self.fields.is_empty() {
            return Err(FormValidationError::InvalidDefinition(String::new()));
        }
        let mut earlier: BTreeMap<&str, &FormField> = BTreeMap::new();
        let mut keys = BTreeSet::new();
        for field in &self.fields {
            if !keys.insert(&field.key) {
                return Err(FormValidationError::InvalidDefinition(field.key.clone()));
            }
            if matches!(field.kind, FormFieldKind::External { .. }) {
                if !field.when.is_empty() || field.required || field.hidden {
                    return Err(FormValidationError::InvalidDefinition(field.key.clone()));
                }
                continue;
            }
            for clause in &field.when {
                let Some(target) = earlier.get(clause.key.as_str()) else {
                    return Err(FormValidationError::InvalidDefinition(field.key.clone()));
                };
                let valid = match (&target.kind, &clause.value) {
                    (FormFieldKind::Boolean { .. }, FormValue::Boolean(_)) => true,
                    (
                        FormFieldKind::Number { .. } | FormFieldKind::Integer { .. },
                        FormValue::Number(_),
                    ) => true,
                    (
                        FormFieldKind::String {
                            options, custom, ..
                        },
                        FormValue::String(value),
                    ) => {
                        *custom
                            || options
                                .as_ref()
                                .is_none_or(|options| options.iter().any(|o| &o.value == value))
                    }
                    (
                        FormFieldKind::Multiselect {
                            options, custom, ..
                        },
                        FormValue::String(value),
                    ) => *custom || options.iter().any(|o| &o.value == value),
                    _ => false,
                };
                if !valid {
                    return Err(FormValidationError::InvalidDefinition(field.key.clone()));
                }
            }
            earlier.insert(&field.key, field);
        }
        Ok(())
    }

    /// Seeds editable default answers using Electron's trimming and omission rules.
    /// Evaluate these values to omit inactive fields. External acknowledgements require an action.
    pub fn defaults(&self) -> FormAnswer {
        let mut answer = FormAnswer::new();
        for field in &self.fields {
            let value =
                match &field.kind {
                    FormFieldKind::String {
                        default, options, ..
                    } => default.as_ref().and_then(|value| {
                        // initialFormValues selects an exact option before fieldAnswer considers text.
                        if options.as_ref().is_some_and(|options| {
                            options.iter().any(|option| option.value == *value)
                        }) {
                            return Some(FormValue::String(value.clone()));
                        }
                        let text = trim_ecmascript(value);
                        (!text.is_empty()).then(|| FormValue::String(text.to_owned()))
                    }),
                    FormFieldKind::Number { default, .. }
                    | FormFieldKind::Integer { default, .. } => default.map(FormValue::Number),
                    FormFieldKind::Boolean { default } => {
                        Some(FormValue::Boolean(default.unwrap_or(false)))
                    }
                    FormFieldKind::Multiselect { default, .. } => default
                        .as_ref()
                        .filter(|values| !values.is_empty())
                        .cloned()
                        .map(FormValue::Strings),
                    FormFieldKind::External { .. } => None,
                };
            if let Some(value) = value {
                answer.insert(field.key.clone(), value);
            }
        }
        answer
    }

    /// Filters answers and fields in declaration order, so inactive dependencies cascade.
    pub fn evaluate<'a>(&'a self, values: &FormAnswer) -> (Vec<&'a FormField>, FormAnswer) {
        let mut active = Vec::new();
        let mut answer = FormAnswer::new();
        for field in &self.fields {
            if !matches!(field.kind, FormFieldKind::External { .. })
                && !field
                    .when
                    .iter()
                    .all(|clause| matches_when(clause, answer.get(&clause.key)))
            {
                continue;
            }
            active.push(field);
            if let Some(value) = values.get(&field.key) {
                answer.insert(field.key.clone(), value.clone());
            }
        }
        (active, answer)
    }

    /// Checks a submitted answer without dropping unknown or inactive keys.
    /// Regexp and format constraints return `RequiresServerValidation` after local checks pass.
    pub fn validate_answer(&self, answer: &FormAnswer) -> Result<(), FormValidationError> {
        self.validate_definition()?;
        for key in answer.keys() {
            if !self.fields.iter().any(|field| &field.key == key) {
                return Err(FormValidationError::UnknownField(key.clone()));
            }
        }
        let (active, _) = self.evaluate(answer);
        let mut server_checks = Vec::new();
        for field in &self.fields {
            let value = answer.get(&field.key);
            if !active.iter().any(|candidate| candidate.key == field.key) {
                if value.is_some() {
                    return Err(FormValidationError::InactiveField(field.key.clone()));
                }
                continue;
            }
            if matches!(field.kind, FormFieldKind::External { .. }) {
                if value != Some(&FormValue::Boolean(true)) {
                    return Err(FormValidationError::Missing(field.key.clone()));
                }
                continue;
            }
            let Some(value) = value else {
                if field.required {
                    return Err(FormValidationError::Missing(field.key.clone()));
                }
                continue;
            };
            let valid = match (&field.kind, value) {
                (
                    FormFieldKind::String {
                        min_length,
                        max_length,
                        options,
                        custom,
                        pattern,
                        format,
                        ..
                    },
                    FormValue::String(text),
                ) => {
                    if pattern.is_some() || format.is_some() {
                        server_checks.push(field.key.clone());
                    }
                    let length = text.encode_utf16().count();
                    (!field.required || !text.is_empty())
                        && min_length.is_none_or(|min| length >= min)
                        && max_length.is_none_or(|max| length <= max)
                        && (*custom
                            || options
                                .as_ref()
                                .is_none_or(|options| options.iter().any(|o| o.value == *text)))
                }
                (
                    FormFieldKind::Number {
                        minimum, maximum, ..
                    }
                    | FormFieldKind::Integer {
                        minimum, maximum, ..
                    },
                    FormValue::Number(value),
                ) => {
                    let number = value.get();
                    minimum.is_none_or(|min| number >= min.get())
                        && maximum.is_none_or(|max| number <= max.get())
                        && (!matches!(field.kind, FormFieldKind::Integer { .. })
                            || number.fract() == 0.0)
                }
                (FormFieldKind::Boolean { .. }, FormValue::Boolean(_)) => true,
                (
                    FormFieldKind::Multiselect {
                        options,
                        custom,
                        min_items,
                        max_items,
                        ..
                    },
                    FormValue::Strings(values),
                ) => {
                    (!field.required || !values.is_empty())
                        && min_items.is_none_or(|min| values.len() >= min)
                        && max_items.is_none_or(|max| values.len() <= max)
                        && (*custom || values.iter().all(|v| options.iter().any(|o| o.value == *v)))
                }
                _ => false,
            };
            if !valid {
                return Err(FormValidationError::InvalidValue(field.key.clone()));
            }
        }
        if !server_checks.is_empty() {
            return Err(FormValidationError::RequiresServerValidation(server_checks));
        }
        Ok(())
    }
}

/// ECMAScript TrimString's WhiteSpace and LineTerminator set. Rust's trim includes
/// U+0085 and excludes U+FEFF, so it cannot implement the reference's String.trim().
fn trim_ecmascript(value: &str) -> &str {
    value.trim_matches(|character| {
        matches!(character,
        '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' |
        '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' |
        '\u{205f}' | '\u{3000}' | '\u{feff}')
    })
}

fn matches_when(clause: &FormWhen, answer: Option<&FormValue>) -> bool {
    let Some(answer) = answer else {
        return false;
    };
    let hit = match (answer, &clause.value) {
        (FormValue::Strings(values), FormValue::String(value)) => values.contains(value),
        _ => answer == &clause.value,
    };
    match clause.op {
        WhenOp::Equal => hit,
        WhenOp::NotEqual => !hit,
    }
}
