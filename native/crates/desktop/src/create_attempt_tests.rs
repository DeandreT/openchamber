use super::*;
use openchamber_native_core::draft::DraftTarget;
use openchamber_native_core::ids::{DraftId, MessageId, RuntimeId, SessionId};
use openchamber_native_core::send_config::ModelChoice;

#[test]
fn session_first_correction_is_explicit_and_preserves_the_captured_first_message() {
    let (key, mut attempt) = sample();
    attempt.phase = CreationPhase::Confirmed;
    attempt.native_config = Some(crate::native_agent::CreationConfig {
        provider_id: "native-provider".into(),
        model_id: "captured-route".into(),
        variant: None,
        prepare_after_session: true,
        correction_required: false,
        config: vec![("retired-field".into(), "retired-value".into())],
    });
    attempt
        .prompt
        .parts
        .files
        .push(openchamber_native_transport::prompt_parts::PromptFile {
            uri: "file:///project/captured.txt".into(),
            name: Some("captured.txt".into()),
        });
    let before = attempt.clone();
    let mut attempts = CreateAttempts::default();
    assert!(attempts.insert(key.clone(), attempt));
    let corrected = vec![("live-field".into(), "accepted-choice".into())];
    assert!(
        !attempts.correct_native_config(&key, corrected.clone(), true),
        "ordinary updates cannot rewrite accepted intent"
    );
    assert!(attempts.require_native_correction(&key));
    assert!(attempts.correct_native_config(&key, corrected.clone(), true));
    let held = attempts.get(&key).expect("attempt");
    assert_eq!(
        held.native_config.as_ref().expect("native config").config,
        corrected
    );
    assert_eq!(held.request, before.request);
    assert_eq!(held.source, before.source);
    assert_eq!(held.prompt.message_id, before.prompt.message_id);
    assert_eq!(held.prompt.text, before.prompt.text);
    assert_eq!(held.prompt.parts.files, before.prompt.parts.files);
    assert_eq!(held.prompt.agent, before.prompt.agent);
    assert_eq!(held.prompt.model, before.prompt.model);
    assert_eq!(held.resources, before.resources);
    let restored = CreateAttempts::restore(attempts.snapshot());
    assert!(restored.get(&key).expect("attempt").navigation.is_none());
    assert_eq!(
        restored.get(&key).expect("attempt").native_config,
        held.native_config
    );
    assert!(attempts.require_native_correction(&key));
    assert!(attempts.correct_native_config(&key, Vec::new(), false));
    assert_eq!(
        attempts
            .get(&key)
            .expect("test fixture")
            .native_config
            .as_ref()
            .expect("test fixture")
            .config,
        corrected,
        "a later partial/empty snapshot cannot clear previously accepted choices"
    );
}

#[test]
fn session_first_preparation_correction_keeps_still_unknown_captured_pairs() {
    let (key, mut attempt) = sample();
    attempt.phase = CreationPhase::Confirmed;
    attempt.native_config = Some(crate::native_agent::CreationConfig {
        provider_id: "native-provider".into(),
        model_id: "route".into(),
        variant: None,
        prepare_after_session: true,
        correction_required: true,
        config: vec![
            ("runtime".into(), "old".into()),
            ("not-discovered".into(), "frozen-choice".into()),
        ],
    });
    let mut attempts = CreateAttempts::default();
    assert!(attempts.insert(key.clone(), attempt));
    assert!(attempts.correct_native_config(
        &key,
        vec![("runtime".into(), "corrected".into())],
        false
    ));
    assert_eq!(
        attempts
            .get(&key)
            .expect("test fixture")
            .native_config
            .as_ref()
            .expect("test fixture")
            .config,
        vec![
            ("not-discovered".into(), "frozen-choice".into()),
            ("runtime".into(), "corrected".into())
        ]
    );
}

pub(crate) fn sample() -> (SessionKey, CreateAttempt) {
    let key = SessionKey {
        runtime: RuntimeId::from("creation-runtime"),
        session: SessionId::from("ses_creation123"),
    };
    let source = DraftKey::new(
        key.runtime.clone(),
        "/project",
        DraftTarget::NewSession(DraftId::from("project")),
    )
    .expect("source");
    let model = Some(ModelChoice {
        provider_id: "openchamber".into(),
        model_id: "auto".into(),
        variant: None,
    });
    let draft = Draft {
        text: "saved text λ".into(),
        selection: 2..4,
        ..Draft::default()
    };
    let request = CreateSession::new(
        key.session.clone(),
        source.directory(),
        model.clone(),
        Some("build".into()),
    )
    .expect("request");
    (
        key,
        CreateAttempt {
            source,
            project: "registered-project".into(),
            request,
            draft: draft.clone(),
            resources: Vec::new(),
            native_config: None,
            prompt: Prompt {
                message_id: MessageId::from("msg_creation123"),
                text: draft.text,
                parts: Default::default(),
                model,
                agent: Some("build".into()),
                delivery: None,
            },
            created: 1793209852000,
            phase: CreationPhase::Preparing,
            navigation: Some(7),
        },
    )
}

#[test]
fn creation_owner_gate_and_deletion_survive_late_completion() {
    let (key, attempt) = sample();
    let mut attempts = CreateAttempts::default();
    assert!(attempts.insert(key.clone(), attempt.clone()));
    assert!(!attempts.can_begin(&attempt.source));
    assert!(!attempts.insert(
        SessionKey {
            session: SessionId::from("ses_other"),
            ..key.clone()
        },
        attempt
    ));
    assert!(attempts.phase(&key, CreationPhase::Deleted));
    assert!(!attempts.phase(&key, CreationPhase::Confirmed));
    assert!(!attempts.phase(&key, CreationPhase::Uncertain));
    assert_eq!(
        attempts.get(&key).expect("retained").phase,
        CreationPhase::Deleted
    );
}

#[test]
fn interrupted_creation_restores_without_navigation_or_dispatch() {
    for phase in [
        CreationPhase::Preparing,
        CreationPhase::Creating,
        CreationPhase::Uncertain,
        CreationPhase::Confirmed,
        CreationPhase::Mismatch,
        CreationPhase::Deleted,
    ] {
        let (key, mut attempt) = sample();
        attempt.phase = phase;
        let restored = CreateAttempts::restore(vec![(key.clone(), attempt)]);
        let held = restored.get(&key).expect("retained");
        assert!(held.navigation.is_none());
        assert_eq!(
            held.phase,
            if matches!(phase, CreationPhase::Preparing | CreationPhase::Creating) {
                CreationPhase::Uncertain
            } else {
                phase
            }
        );
        assert!(
            held.request.model().is_none(),
            "Auto omitted from saved create body"
        );
        assert_eq!(
            held.prompt.model.as_ref().expect("Auto retained").model_id,
            "auto"
        );
    }
}

#[test]
fn first_prompt_skips_identical_agent_but_preserves_original_and_reapplies_auto() {
    use crate::session_creation;
    use openchamber_native_core::session::SessionRecord;
    let (key, attempt) = sample();
    let mut session = SessionRecord::new(key.session, "/project");
    session.agent = attempt.prompt.agent.clone();
    session.model = attempt.prompt.model.clone();
    let dispatched = session_creation::first_prompt_selection(&attempt.prompt, &session);
    assert!(dispatched.agent.is_none());
    assert_eq!(
        dispatched.model, attempt.prompt.model,
        "Auto must use intercepted switch"
    );
    let mut concrete = attempt.prompt;
    concrete.model.as_mut().expect("model").provider_id = "provider".into();
    concrete.model.as_mut().expect("model").model_id = "model".into();
    session.model = concrete.model.clone();
    let dispatched = session_creation::first_prompt_selection(&concrete, &session);
    assert!(dispatched.model.is_none());
    assert!(dispatched.agent.is_none());
    assert!(concrete.agent.is_some(), "retry keeps original agent");
    session.agent = Some("different".into());
    session.model.as_mut().expect("model").variant = Some("high".into());
    let retry = session_creation::first_prompt_selection(&concrete, &session);
    assert_eq!(
        retry.model, concrete.model,
        "missing variant must clear old effort"
    );
    assert_eq!(retry.agent, concrete.agent);
}
