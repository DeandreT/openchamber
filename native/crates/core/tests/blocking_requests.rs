//! Form and pending-state contracts without a renderer or network.
use openchamber_native_core::form::{
    FormAnswer, FormField, FormFieldKind, FormNumber, FormOption, FormRequest, FormValidationError,
    FormValue, FormWhen, WhenOp,
};
use openchamber_native_core::ids::{FormId, RuntimeId};
use openchamber_native_core::pending_requests::{
    PendingRequest, PendingRequests, ReplyFailure, RequestKind, ResponseState,
};
use openchamber_native_core::request_list::{RequestList, RequestListIssue, RequestListIssueKind};
use openchamber_native_core::request_owner::{RequestOwner, RequestScope};

fn snapshot(scope: &RequestScope, entries: Vec<PendingRequest>) -> RequestList<PendingRequest> {
    RequestList::new(scope.clone(), None, entries, vec![])
}

fn scope(runtime: &str, directory: &str) -> RequestScope {
    RequestScope::new(RuntimeId::from(runtime), directory).expect("absolute scope")
}
fn field(key: &str, kind: FormFieldKind) -> FormField {
    FormField {
        key: key.to_owned(),
        title: None,
        description: None,
        required: false,
        hidden: false,
        when: vec![],
        kind,
    }
}
fn text() -> FormFieldKind {
    FormFieldKind::String {
        format: None,
        min_length: None,
        max_length: None,
        pattern: None,
        placeholder: None,
        default: None,
        options: None,
        custom: false,
    }
}
fn form(id: &str) -> FormRequest {
    FormRequest {
        id: FormId::from(id),
        owner: RequestOwner::Location,
        title: "Questions".into(),
        metadata_display: None,
        fields: vec![field("first", text())],
    }
}

#[test]
fn conditional_answers_cascade_and_never_include_hidden_fields() {
    let mut form = form("frm_conditions");
    let mut second = field("second", text());
    second.required = true;
    second.when.push(FormWhen {
        key: "first".into(),
        op: WhenOp::Equal,
        value: FormValue::String("show".into()),
    });
    let mut third = field("third", text());
    third.when.push(FormWhen {
        key: "second".into(),
        op: WhenOp::NotEqual,
        value: FormValue::String("no".into()),
    });
    form.fields.extend([second, third]);
    assert!(form.validate_definition().is_ok());
    let mut answers = FormAnswer::from([
        ("first".into(), FormValue::String("hide".into())),
        ("second".into(), FormValue::String("yes".into())),
    ]);
    assert_eq!(
        form.evaluate(&answers).0.len(),
        1,
        "inactive answers cannot activate dependents"
    );
    assert!(matches!(
        form.validate_answer(&answers),
        Err(FormValidationError::InactiveField(_))
    ));
    answers.insert("first".into(), FormValue::String("show".into()));
    assert_eq!(form.evaluate(&answers).0.len(), 3);
    answers.remove("second");
    assert_eq!(
        form.evaluate(&answers).0.len(),
        2,
        "unanswered neq is false"
    );
    assert!(matches!(
        form.validate_answer(&answers),
        Err(FormValidationError::Missing(_))
    ));
    form.fields[1].when[0].key = "third".into();
    assert!(matches!(
        form.validate_definition(),
        Err(FormValidationError::InvalidDefinition(_))
    ));
}

#[test]
fn all_field_values_defaults_constraints_and_external_acknowledgement() {
    let mut form = form("frm_values");
    form.fields = vec![field(
        "choice",
        FormFieldKind::String {
            options: Some(vec![FormOption {
                value: "wire".into(),
                label: "Display".into(),
                description: None,
            }]),
            default: Some("wire".into()),
            format: None,
            min_length: None,
            max_length: None,
            pattern: None,
            placeholder: None,
            custom: false,
        },
    )];
    form.fields.extend([
        field(
            "number",
            FormFieldKind::Number {
                minimum: FormNumber::new(1.0),
                maximum: FormNumber::new(4.0),
                default: FormNumber::new(2.5),
            },
        ),
        field(
            "integer",
            FormFieldKind::Integer {
                minimum: None,
                maximum: None,
                default: FormNumber::new(2.0),
            },
        ),
        field("bool", FormFieldKind::Boolean { default: None }),
        field(
            "many",
            FormFieldKind::Multiselect {
                options: vec![],
                min_items: Some(1),
                max_items: Some(2),
                custom: true,
                default: Some(vec!["custom".into()]),
            },
        ),
        field(
            "external",
            FormFieldKind::External {
                url: "https://example.com/step".into(),
            },
        ),
    ]);
    let mut answer = form.defaults();
    assert!(
        !answer.contains_key("external"),
        "showing defaults never acknowledges a link"
    );
    assert_eq!(answer["bool"], FormValue::Boolean(false));
    assert!(matches!(
        form.validate_answer(&answer),
        Err(FormValidationError::Missing(_))
    ));
    answer.insert("external".into(), FormValue::Boolean(true));
    assert_eq!(form.validate_answer(&answer), Ok(()));
    answer.insert(
        "integer".into(),
        FormValue::Number(FormNumber::new(2.5).expect("finite")),
    );
    assert!(matches!(
        form.validate_answer(&answer),
        Err(FormValidationError::InvalidValue(_))
    ));
    assert!(FormNumber::new(f64::INFINITY).is_none());
    answer.insert(
        "integer".into(),
        FormValue::Number(FormNumber::new(2.0).expect("finite")),
    );
    if let FormFieldKind::String { pattern, .. } = &mut form.fields[0].kind {
        *pattern = Some("(?<=w)ire".into());
    }
    assert_eq!(
        form.validate_answer(&answer),
        Err(FormValidationError::RequiresServerValidation(vec![
            "choice".into()
        ]))
    );
}

#[test]
fn terminal_events_dominate_loads_replays_and_failed_mutations() {
    let scope = scope("runtime-a", "/project");
    let mut pending = PendingRequests::new(scope.clone());
    let request = PendingRequest::Form(form("frm_one"));
    let key = request.key(&scope);
    let load = pending.begin_load(RequestKind::Form);
    pending.asked(request.clone());
    let attempt = pending.begin_reply(&key).expect("pending");
    pending.settle(&key).expect("same scope");
    pending
        .apply_list(load, snapshot(&scope, vec![request.clone()]))
        .expect("snapshot");
    assert!(
        !pending.asked(request),
        "replay cannot resurrect a terminal ID"
    );
    assert!(
        pending
            .reply_failed(attempt, ReplyFailure::Unknown)
            .is_err()
    );
    assert!(pending.get(&key).is_none());
    assert!(pending.is_complete(RequestKind::Form));
    assert!(!pending.is_complete(RequestKind::Permission));
}

#[test]
fn scope_failure_and_reply_gates_are_independent() {
    let scope = scope("runtime-a", "/project");
    let mut pending = PendingRequests::new(scope.clone());
    let one = PendingRequest::Form(form("frm_one"));
    let two = PendingRequest::Form(form("frm_two"));
    let key = one.key(&scope);
    let other = two.key(&scope);
    pending.asked(one.clone());
    pending.asked(two.clone());
    let failed_read = pending.begin_load(RequestKind::Form);
    drop(failed_read);
    assert_eq!(
        pending.entries().count(),
        2,
        "failed reads preserve requests"
    );
    let attempt = pending.begin_reply(&key).expect("first request");
    assert!(
        pending.begin_reply(&key).is_err(),
        "double activation sends once"
    );
    let other_attempt = pending
        .begin_reply(&other)
        .expect("another request remains actionable");
    pending
        .reply_failed(attempt, ReplyFailure::Unknown)
        .expect("unknown reply");
    assert_eq!(
        pending.get(&key).expect("retained").response,
        ResponseState::Uncertain
    );
    pending
        .reply_failed(other_attempt, ReplyFailure::Rejected)
        .expect("definite refusal");
    assert_eq!(
        pending.get(&other).expect("retained").response,
        ResponseState::Pending
    );
    let load = pending.begin_load(RequestKind::Form);
    pending.invalidate();
    assert!(pending.apply_list(load, snapshot(&scope, vec![])).is_err());
    assert_eq!(pending.entries().count(), 2);
    assert!(
        pending.begin_reply(&other).is_err(),
        "stale state grants no mutation"
    );
    let load = pending.begin_load(RequestKind::Form);
    pending
        .apply_list(load, snapshot(&scope, vec![one.clone(), two]))
        .expect("fresh scope");
    assert_eq!(
        pending
            .get(&key)
            .expect("uncertain until exact read")
            .response,
        ResponseState::Uncertain
    );
    assert!(
        pending.begin_reply(&key).is_err(),
        "a list cannot authorize retry of an uncertain reply"
    );
    let check = pending.begin_check(&key);
    pending
        .checked(check, Some(one))
        .expect("exact pending read");
    assert_eq!(
        pending.get(&key).expect("pending again").response,
        ResponseState::Pending
    );
    let mut wrong = key;
    wrong.scope = RequestScope::new(RuntimeId::from("runtime-b"), "/project").expect("scope");
    assert!(pending.settle(&wrong).is_err());
    assert_eq!(pending.entries().count(), 2);
}

#[test]
fn snapshot_validation_is_atomic_and_newer_events_survive_absence() {
    let scope = scope("r", "/project");
    let mut pending = PendingRequests::new(scope.clone());
    let one = PendingRequest::Form(form("frm_one"));
    let load = pending.begin_load(RequestKind::Form);
    pending.asked(one.clone());
    pending
        .apply_list(load, snapshot(&scope, vec![]))
        .expect("complete old snapshot");
    assert_eq!(pending.entries().count(), 1);
    let load = pending.begin_load(RequestKind::Form);
    assert!(
        pending
            .apply_list(load, snapshot(&scope, vec![one.clone(), one]))
            .is_err()
    );
    assert_eq!(
        pending.entries().count(),
        1,
        "malformed snapshot has no removal authority"
    );
    let load = pending.begin_load(RequestKind::Form);
    pending
        .apply_list(load, snapshot(&scope, vec![]))
        .expect("authoritative empty");
    assert_eq!(pending.entries().count(), 0);
}

#[test]
fn multiselect_conditions_closed_choices_and_utf16_lengths_match_server_rules() {
    let mut form = form("frm_multiselect");
    form.fields[0] = field(
        "first",
        FormFieldKind::Multiselect {
            options: vec![FormOption {
                value: "a".into(),
                label: "A".into(),
                description: None,
            }],
            custom: false,
            min_items: Some(1),
            max_items: Some(1),
            default: None,
        },
    );
    let mut next = field(
        "next",
        FormFieldKind::String {
            format: None,
            min_length: Some(2),
            max_length: Some(2),
            pattern: None,
            placeholder: None,
            default: None,
            options: None,
            custom: false,
        },
    );
    next.when.push(FormWhen {
        key: "first".into(),
        op: WhenOp::Equal,
        value: FormValue::String("a".into()),
    });
    form.fields.push(next);
    let mut answer = FormAnswer::from([
        ("first".into(), FormValue::Strings(vec!["a".into()])),
        ("next".into(), FormValue::String("🙂".into())),
    ]);
    assert_eq!(form.evaluate(&answer).0.len(), 2);
    assert_eq!(
        form.validate_answer(&answer),
        Ok(()),
        "JS lengths count UTF-16 code units"
    );
    answer.insert("first".into(), FormValue::Strings(vec!["unknown".into()]));
    assert!(matches!(
        form.validate_answer(&answer),
        Err(FormValidationError::InvalidValue(_))
    ));
    form.fields[1].when[0].value = FormValue::String("unknown".into());
    assert!(
        form.validate_definition().is_err(),
        "closed conditions require declared option values"
    );
}

#[test]
fn request_metadata_updates_do_not_strand_an_inflight_reply() {
    let scope = scope("r", "/project");
    let mut pending = PendingRequests::new(scope.clone());
    let request = PendingRequest::Form(form("frm_update"));
    let key = request.key(&scope);
    pending.asked(request);
    let attempt = pending.begin_reply(&key).expect("pending");
    let mut updated = form("frm_update");
    updated.title = "Updated title".into();
    pending.asked(PendingRequest::Form(updated));
    pending
        .reply_failed(attempt, ReplyFailure::Rejected)
        .expect("same attempt still owns completion");
    assert_eq!(
        pending.get(&key).expect("retained").response,
        ResponseState::Pending
    );
    assert!(pending.begin_reply(&key).is_ok());
}

#[test]
fn recreated_owner_rejects_previous_instances_tokens_even_for_the_same_scope() {
    let scope = scope("r", "/project");
    let request = PendingRequest::Form(form("frm_reopen"));
    let key = request.key(&scope);
    let mut previous = PendingRequests::new(scope.clone());
    previous.asked(request.clone());
    let old_load = previous.begin_load(RequestKind::Form);
    let old_attempt = previous.begin_reply(&key).expect("first owner");
    let mut current = PendingRequests::new(scope.clone());
    current.asked(request.clone());
    let _current_load = current.begin_load(RequestKind::Form);
    let _current_attempt = current.begin_reply(&key).expect("new owner");
    assert!(
        current
            .apply_list(old_load, snapshot(&scope, vec![]))
            .is_err()
    );
    assert!(
        current
            .reply_failed(old_attempt, ReplyFailure::Rejected)
            .is_err()
    );
    assert_eq!(
        current.get(&key).expect("new owner retained").response,
        ResponseState::Responding
    );
}

#[test]
fn incomplete_lists_publish_siblings_without_removal_authority() {
    let scope = scope("r", "/project");
    let mut pending = PendingRequests::new(scope.clone());
    let retained = PendingRequest::Form(form("frm_retained"));
    let retained_key = retained.key(&scope);
    let load = pending.begin_load(RequestKind::Form);
    pending
        .apply_list(load, snapshot(&scope, vec![retained]))
        .expect("baseline");
    assert!(pending.is_complete(RequestKind::Form));
    let first = PendingRequest::Form(form("frm_before"));
    let last = PendingRequest::Form(form("frm_after"));
    let load = pending.begin_load(RequestKind::Form);
    pending
        .apply_list(
            load,
            RequestList::new(
                scope.clone(),
                None,
                vec![first, last],
                vec![RequestListIssue {
                    index: 1,
                    reason: RequestListIssueKind::Malformed,
                }],
            ),
        )
        .expect("valid siblings");
    assert_eq!(pending.entries().count(), 3);
    assert!(pending.get(&retained_key).is_some());
    assert!(!pending.is_complete(RequestKind::Form));
    let load = pending.begin_load(RequestKind::Form);
    pending
        .apply_list(
            load,
            RequestList::new(
                scope.clone(),
                None,
                vec![],
                vec![RequestListIssue {
                    index: 0,
                    reason: RequestListIssueKind::Malformed,
                }],
            ),
        )
        .expect("all entries failed");
    assert_eq!(
        pending.entries().count(),
        3,
        "incomplete empty has no removal authority"
    );
    let load = pending.begin_load(RequestKind::Form);
    pending
        .apply_list(
            load,
            RequestList::new(scope.clone(), Some(RequestOwner::Location), vec![], vec![]),
        )
        .expect("owner subset");
    assert_eq!(
        pending.entries().count(),
        3,
        "a subset cannot replace the whole location"
    );
    let load = pending.begin_load(RequestKind::Form);
    pending
        .apply_list(load, snapshot(&scope, vec![]))
        .expect("complete empty");
    assert_eq!(pending.entries().count(), 0);
}

#[test]
fn request_debug_redacts_answers_defaults_content_and_nested_stores() {
    use openchamber_native_core::form::{FormDetail, FormState};
    use openchamber_native_core::ids::{PermissionId, SessionId};
    use openchamber_native_core::permission::PermissionRequest;
    const SECRET: &str = "SYNTHETIC_SECRET_MARKER";
    let value = FormValue::String(SECRET.into());
    let answer = FormAnswer::from([(SECRET.into(), value.clone())]);
    let mut request = form("frm_debug");
    request.title = SECRET.into();
    request.metadata_display = Some(SECRET.into());
    request.fields[0].key = SECRET.into();
    request.fields[0].title = Some(SECRET.into());
    request.fields[0].description = Some(SECRET.into());
    if let FormFieldKind::String {
        default,
        pattern,
        placeholder,
        options,
        ..
    } = &mut request.fields[0].kind
    {
        *default = Some(SECRET.into());
        *pattern = Some(SECRET.into());
        *placeholder = Some(SECRET.into());
        *options = Some(vec![FormOption {
            value: SECRET.into(),
            label: SECRET.into(),
            description: Some(SECRET.into()),
        }]);
    }
    let cancelled = FormState::Cancelled {
        message: Some(SECRET.into()),
    };
    let detail = FormDetail {
        form: request.clone(),
        state: FormState::Answered(answer.clone()),
    };
    let permission = PermissionRequest {
        id: PermissionId::from("per_debug"),
        session_id: SessionId::from("ses_debug"),
        action: "shell".into(),
        resources: vec![SECRET.into()],
        save: vec![SECRET.into()],
        source: None,
        message: Some(SECRET.into()),
        metadata_display: Some(SECRET.into()),
    };
    let mut pending = PendingRequests::new(scope("r", "/project"));
    pending.asked(PendingRequest::Form(request.clone()));
    pending.asked(PendingRequest::Permission(permission.clone()));
    for debug in [
        format!("{value:?}"),
        format!("{answer:?}"),
        format!("{:?}", request.defaults()),
        format!("{:?}", request.fields[0].kind),
        format!("{cancelled:?}"),
        format!("{detail:?}"),
        format!("{permission:?}"),
        format!("{pending:?}"),
        format!("{:?}", FormValidationError::Missing(SECRET.into())),
    ] {
        assert!(!debug.contains(SECRET), "Debug leaked request content");
        assert!(debug.contains("redacted"));
    }
    assert!(format!("{pending:?}").contains("frm_debug"));
    assert!(format!("{pending:?}").contains("per_debug"));
    assert!(format!("{permission:?}").contains("shell"));
}
