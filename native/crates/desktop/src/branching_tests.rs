//! Fork, staged revert, `/btw` and subagent navigation through the rendered
//! window and the workspace's completion handlers.

use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, AppContext, ElementId, Entity, SharedString, TestAppContext};
use openchamber_native_core::ids::{MessageId, PartId, SessionId};
use openchamber_native_core::session::{
    BtwMetadata, MetadataFacts, SessionKind, SessionPatch, SessionRecord,
};
use openchamber_native_core::transcript::{
    Message, MessageInfo, Part, PartContent, PartTime, Role, SubagentRun, SubagentRunState,
    SyntheticFacts, ToolStatus,
};
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::event::ServerEvent;

use crate::send_attempt::SessionKey;
use crate::workspace::Workspace;
use crate::workspace_tests::open;

fn name(id: impl Into<String>) -> ElementId {
    ElementId::Name(SharedString::from(id.into()))
}

fn update<T>(
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    cx: &mut TestAppContext,
    f: impl FnOnce(&mut Workspace, &mut gpui_kit::Window, &mut gpui_kit::Context<Workspace>) -> T,
) -> T {
    cx.update_window(window, |_, window, cx| {
        let result = workspace.update(cx, |workspace, cx| f(workspace, window, cx));
        window.render_frame(cx);
        result
    })
    .expect("window is open")
}

fn shown(window: AnyWindowHandle, id: impl Into<String>, cx: &mut TestAppContext) -> bool {
    let id = name(id);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.try_find(id).is_some()
    })
    .expect("window is open")
}

/// Selects a root session and returns it with its transcript's user rows.
fn select_root(
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    cx: &mut TestAppContext,
) -> (SessionId, Vec<MessageId>) {
    update(window, workspace, cx, |this, window, cx| {
        let session = this
            .sessions
            .active()
            .iter()
            .find(|record| record.parent_id.is_none() && record.directory.is_some())
            .expect("a root session")
            .id
            .clone();
        this.select_session(session.clone(), window, cx);
        let users = this.open_sessions[&session]
            .transcript
            .messages()
            .iter()
            .filter(|message| message.info.role == Role::User)
            .map(|message| message.info.id.clone())
            .collect();
        (session, users)
    })
}

fn composer_text(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> String {
    cx.read(|cx| workspace.read(cx).composer.read(cx).value().to_string())
}

#[gpui_kit::test]
fn btw_refusal_preserves_the_command_draft(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (session, _) = select_root(window, &workspace, cx);
    update(window, &workspace, cx, |this, window, cx| {
        this.connection = Some(
            crate::connection::Connection::new(crate::connection::ServerConfig {
                endpoint: openchamber_native_transport::endpoint::Endpoint::parse(
                    "http://127.0.0.1:1",
                )
                .expect("fixture endpoint is valid"),
                password: None,
                client: openchamber_native_transport::client::ClientDescription {
                    label: "test".into(),
                    kind: "desktop".into(),
                    dedupe_key: None,
                    device_platform: None,
                    app_version: None,
                },
            })
            .expect("fixture connection initializes"),
        );
        for creating in [false, true] {
            let key = this.mutation_key(&session);
            this.branching.btw.entry(key).or_default().creating = creating;
            this.composer.update(cx, |composer, cx| {
                composer.set_value("/btw retain this question", window, cx)
            });
            assert!(this.run_branch_command(window, cx));
            assert_eq!(
                this.composer.read(cx).value().as_ref(),
                "/btw retain this question"
            );
            let key = this
                .composer_key
                .as_ref()
                .expect("selected session has a draft owner");
            assert_eq!(
                this.drafts
                    .get(key)
                    .expect("refused command keeps its draft")
                    .text,
                "/btw retain this question"
            );
        }
        this.branching.btw.clear();
        ready_branch_catalog(this);
        assert!(this.send_selection().is_some());
        assert!(
            this.run_branch_command(window, cx),
            "missing client refuses task admission"
        );
        assert_eq!(
            this.composer.read(cx).value().as_ref(),
            "/btw retain this question"
        );
    });
}

fn ready_branch_catalog(this: &mut Workspace) {
    use crate::send_catalog::{DirectoryCatalog, Load};
    let mut catalog = DirectoryCatalog::default();
    catalog.agents = Load::Ready(Vec::new());
    catalog.providers = Load::Ready(Vec::new());
    catalog.opencode = Load::Ready(Default::default());
    this.send_catalog.routing = openchamber_native_core::send_config::AutoRouting::Ready;
    this.send_catalog.insert_test_directory(
        this.config_directory()
            .expect("selected session has a directory"),
        catalog,
    );
}

#[gpui_kit::test]
fn admitted_btw_tasks_keep_drafts_and_own_forks_skip_reads(cx: &mut TestAppContext) {
    use crate::connection::{Connection, ServerConfig};
    use openchamber_native_transport::client::{ClientDescription, ServerClient};
    use openchamber_native_transport::credential::Credential;
    use openchamber_native_transport::endpoint::Endpoint;
    let (window, workspace) = open(cx);
    let (session, _) = select_root(window, &workspace, cx);
    update(window, &workspace, cx, |this, window, cx| {
        let endpoint = Endpoint::parse("http://127.0.0.1:1").expect("fixture endpoint is valid");
        let mut connection = Connection::new(ServerConfig {
            endpoint: endpoint.clone(),
            password: None,
            client: ClientDescription {
                label: "test".into(),
                kind: "desktop".into(),
                dedupe_key: None,
                device_platform: None,
                app_version: None,
            },
        })
        .expect("fixture connection initializes");
        connection.client = Some(
            ServerClient::new(endpoint, Credential::None).expect("fixture client initializes"),
        );
        this.connection = Some(connection);
        ready_branch_catalog(this);
        let parent_key = this.mutation_key(&session);
        this.branching.forking.insert(parent_key.clone());
        this.on_session_forked("own_fork".into(), &session, window, cx);
        assert!(
            this.branching.fork_reads.is_empty(),
            "a local fork owns its response"
        );
        this.branching.forking.clear();
        this.composer.update(cx, |composer, cx| {
            composer.set_value("/btw question", window, cx)
        });
        this.run_branch_command(window, cx);
        assert!(this.branching.btw[&parent_key].creating);
        assert_eq!(this.composer.read(cx).value().as_ref(), "/btw question");
        let draft = this
            .composer_key
            .clone()
            .expect("selected session has a draft owner");
        assert_eq!(
            this.drafts
                .get(&draft)
                .expect("pending BTW retains its draft")
                .text,
            "/btw question"
        );
        this.on_session_forked("own_btw".into(), &session, window, cx);
        assert!(this.branching.fork_reads.is_empty());
        let fork: SessionId = "existing_btw".into();
        let mut record = SessionRecord::new(fork.clone(), "/p");
        record.kind = SessionKind::Btw;
        this.apply_server_events(
            vec![
                ServerEvent::SessionCreated(record),
                ServerEvent::SessionPatched {
                    session_id: session.clone(),
                    patch: SessionPatch {
                        metadata: Some(MetadataFacts {
                            btw: BtwMetadata {
                                link: Some(fork),
                                ..BtwMetadata::default()
                            },
                            ..MetadataFacts::default()
                        }),
                        ..SessionPatch::default()
                    },
                },
            ],
            window,
            cx,
        );
        this.branching
            .btw
            .get_mut(&parent_key)
            .expect("BTW state exists")
            .creating = false;
        this.composer.update(cx, |composer, cx| {
            composer.set_value("follow up", window, cx)
        });
        this.send_btw_follow_up(window, cx);
        assert!(this.branching.btw[&parent_key].sending);
        assert_eq!(this.composer.read(cx).value().as_ref(), "follow up");
        assert_eq!(
            this.drafts
                .get(&draft)
                .expect("follow-up retains its draft")
                .text,
            "follow up"
        );
    });
}

#[gpui_kit::test]
fn report_revert_failure_preserves_new_typing(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (session, _) = select_root(window, &workspace, cx);
    update(window, &workspace, cx, |this, window, cx| {
        let key = this.mutation_key(&session);
        this.branching
            .pending_reverts
            .insert(key.clone(), "gone-report".into());
        this.composer.update(cx, |composer, cx| {
            composer.set_value("new typing", window, cx)
        });
        this.finish_revert(
            key,
            None,
            Some(Err(TransportError::Http { status: 500 })),
            window,
            cx,
        );
        assert_eq!(this.composer.read(cx).value().as_ref(), "new typing");
    });
}

#[gpui_kit::test]
fn revert_failure_restores_captured_owner_after_runtime_switch(cx: &mut TestAppContext) {
    use crate::session_branching::RevertRestore;
    use openchamber_native_core::draft::{Draft, DraftKey, DraftTarget};
    let (window, workspace) = open(cx);
    let (session, _) = select_root(window, &workspace, cx);
    update(window, &workspace, cx, |this, window, cx| {
        let key = this.mutation_key(&session);
        let draft = this
            .composer_key
            .clone()
            .expect("selected session has a draft owner");
        this.drafts.save(draft.clone(), Draft::new("restored"));
        this.runtime = "different-runtime".into();
        this.composer_key = Some(
            DraftKey::new(
                this.runtime.clone(),
                "/other",
                DraftTarget::Session(session.clone()),
            )
            .expect("new runtime draft owner is valid"),
        );
        this.composer.update(cx, |composer, cx| {
            composer.set_value("other runtime typing", window, cx)
        });
        this.branching.switch_runtime();
        this.finish_revert(
            key.clone(),
            Some(RevertRestore {
                key: draft.clone(),
                previous: "original".into(),
                restored: "restored".into(),
            }),
            Some(Err(TransportError::Http { status: 500 })),
            window,
            cx,
        );
        assert_eq!(
            this.drafts
                .get(&draft)
                .expect("failed revert restores its draft")
                .text,
            "original"
        );
        assert_eq!(
            this.composer.read(cx).value().as_ref(),
            "other runtime typing"
        );
        assert!(this.session_management.notice.is_none());
        this.drafts.save(draft.clone(), Draft::new("newer typing"));
        this.finish_revert(
            key,
            Some(RevertRestore {
                key: draft.clone(),
                previous: "original".into(),
                restored: "restored".into(),
            }),
            Some(Err(TransportError::Http { status: 500 })),
            window,
            cx,
        );
        assert_eq!(
            this.drafts.get(&draft).expect("newer draft survives").text,
            "newer typing"
        );
    });
}

#[gpui_kit::test]
fn btw_completion_clears_only_accepted_unchanged_text(cx: &mut TestAppContext) {
    use crate::session_branching::BtwDraft;
    let (window, workspace) = open(cx);
    let (session, _) = select_root(window, &workspace, cx);
    update(window, &workspace, cx, |this, window, cx| {
        let key = this.mutation_key(&session);
        let draft = this
            .composer_key
            .clone()
            .expect("selected session has a draft owner");
        for (text, succeeds) in [
            ("/btw question", false),
            ("/btw question plus newer typing", true),
            ("/btw question", true),
        ] {
            this.composer
                .update(cx, |composer, cx| composer.set_value(text, window, cx));
            this.save_composer_draft(cx);
            let result = if succeeds {
                Ok((
                    SessionRecord::new("btw_done", "/p"),
                    MetadataFacts::default(),
                ))
            } else {
                Err(TransportError::Http { status: 500 })
            };
            this.finish_start_btw(
                key.clone(),
                Some(BtwDraft {
                    key: draft.clone(),
                    text: "/btw question".into(),
                }),
                Some(result),
                window,
                cx,
            );
            let expected = if succeeds && text == "/btw question" {
                ""
            } else {
                text
            };
            assert_eq!(this.composer.read(cx).value().as_ref(), expected);
            assert_eq!(
                this.drafts
                    .get(&draft)
                    .map(|d| d.text.as_str())
                    .unwrap_or(""),
                expected
            );
        }
    });
}

#[gpui_kit::test]
fn stale_fork_reads_and_cross_runtime_errors_do_not_publish(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (session, _) = select_root(window, &workspace, cx);
    update(window, &workspace, cx, |this, window, cx| {
        let key = this.mutation_key(&session);
        let original = this
            .sessions
            .get(&session)
            .expect("selected session exists")
            .clone();
        let mut stale = original.clone();
        stale.title = "stale raw fork".into();
        let revision = this.branching.begin_fork_read(key.clone());
        this.finish_fork_read(key, revision, false, Some(Ok(stale)), window, cx);
        assert_eq!(
            this.sessions
                .get(&session)
                .expect("selected session remains")
                .title,
            original.title
        );
        let unknown: SessionId = "fork_in_flight".into();
        let key = this.mutation_key(&unknown);
        let revision = this.branching.begin_fork_read(key.clone());
        this.apply_server_events(
            vec![ServerEvent::SessionPatched {
                session_id: unknown.clone(),
                patch: SessionPatch {
                    metadata: Some(MetadataFacts {
                        kind: SessionKind::Btw,
                        ..MetadataFacts::default()
                    }),
                    ..SessionPatch::default()
                },
            }],
            window,
            cx,
        );
        assert_eq!(this.branching.fork_reads[&key].patches.len(), 1);
        this.finish_fork_read(
            key.clone(),
            revision,
            false,
            Some(Ok(SessionRecord::new(unknown.clone(), "/p"))),
            window,
            cx,
        );
        assert!(
            this.sessions.get(&unknown).is_none(),
            "the first patched read must not publish"
        );
        let mut fresh = SessionRecord::new(unknown.clone(), "/p");
        fresh.kind = SessionKind::Btw;
        let revision = this.branching.begin_fork_read(key.clone());
        this.finish_fork_read(key, revision, true, Some(Ok(fresh)), window, cx);
        assert!(
            this.sessions
                .get(&unknown)
                .expect("fresh fork was inserted")
                .is_btw()
        );
        let stale_key = SessionKey {
            runtime: "old-runtime".into(),
            session,
        };
        this.finish_fork(
            stale_key.clone(),
            this.navigation_revision,
            None,
            Some(Err(TransportError::Http { status: 500 })),
            window,
            cx,
        );
        this.finish_stop_subagent(
            stale_key,
            Some(Err(TransportError::Http { status: 500 })),
            window,
            cx,
        );
        assert!(this.session_management.notice.is_none());
    });
}

#[gpui_kit::test]
fn patched_unknown_fork_waits_for_the_second_read(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    update(window, &workspace, cx, |this, window, cx| {
        let session: SessionId = "unknown_fork".into();
        let key = this.mutation_key(&session);
        let revision = this.branching.begin_fork_read(key.clone());
        this.apply_server_events(
            vec![ServerEvent::SessionPatched {
                session_id: session.clone(),
                patch: SessionPatch {
                    title: Some("newer".into()),
                    ..SessionPatch::default()
                },
            }],
            window,
            cx,
        );
        this.finish_fork_read(
            key,
            revision,
            false,
            Some(Ok(SessionRecord::new(session.clone(), "/p"))),
            window,
            cx,
        );
        assert!(
            this.sessions.get(&session).is_none(),
            "a patched first read cannot publish"
        );
    });
}

#[gpui_kit::test]
fn a_patch_during_the_second_fork_read_survives(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    update(window, &workspace, cx, |this, window, cx| {
        let session: SessionId = "twice_patched_fork".into();
        let key = this.mutation_key(&session);
        let patch = |kind| ServerEvent::SessionPatched {
            session_id: session.clone(),
            patch: SessionPatch {
                metadata: Some(MetadataFacts {
                    kind,
                    ..MetadataFacts::default()
                }),
                ..SessionPatch::default()
            },
        };
        let revision = this.branching.begin_fork_read(key.clone());
        this.apply_server_events(vec![patch(SessionKind::Btw)], window, cx);
        this.finish_fork_read(
            key.clone(),
            revision,
            false,
            Some(Ok(SessionRecord::new(session.clone(), "/p"))),
            window,
            cx,
        );
        assert!(this.sessions.get(&session).is_none());
        // Deliver the follow-up read and a patch that arrived while it ran.
        let revision = this.branching.begin_fork_read(key.clone());
        this.apply_server_events(vec![patch(SessionKind::Btw)], window, cx);
        this.finish_fork_read(
            key,
            revision,
            true,
            Some(Ok(SessionRecord::new(session.clone(), "/p"))),
            window,
            cx,
        );
        assert!(
            this.sessions
                .get(&session)
                .expect("fork remains present")
                .is_btw(),
            "the second read must retain the intervening marker"
        );
        assert!(this.branching.fork_reads.is_empty(), "no polling loop");
    });
}

#[gpui_kit::test]
fn unloaded_subagent_read_failure_after_runtime_switch_is_quiet(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    update(window, &workspace, cx, |this, window, cx| {
        let old = this.runtime.clone();
        this.runtime = "another-runtime".into();
        this.finish_open_subtask(
            old,
            this.navigation_revision,
            Some(Err(TransportError::Http { status: 500 })),
            window,
            cx,
        );
        assert!(this.session_management.notice.is_none());
    });
}

#[gpui_kit::test]
fn deletion_during_either_fork_read_prevents_resurrection(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    update(window, &workspace, cx, |this, window, cx| {
        for reread in [false, true] {
            let session: SessionId = if reread {
                "deleted-second-read"
            } else {
                "deleted-first-read"
            }
            .into();
            let key = this.mutation_key(&session);
            let revision = this.branching.begin_fork_read(key.clone());
            if reread {
                this.apply_server_events(
                    vec![ServerEvent::SessionPatched {
                        session_id: session.clone(),
                        patch: SessionPatch {
                            metadata: Some(MetadataFacts {
                                kind: SessionKind::Btw,
                                ..MetadataFacts::default()
                            }),
                            ..SessionPatch::default()
                        },
                    }],
                    window,
                    cx,
                );
            }
            this.apply_server_events(
                vec![ServerEvent::SessionDeleted {
                    session_id: session.clone(),
                }],
                window,
                cx,
            );
            this.finish_fork_read(
                key.clone(),
                revision,
                reread,
                Some(Ok(SessionRecord::new(session.clone(), "/p"))),
                window,
                cx,
            );
            assert!(
                this.sessions.get(&session).is_none(),
                "deleted fork must not return"
            );
            assert!(!this.branching.fork_reads.contains_key(&key));
            this.finish_fork_read(
                key,
                revision,
                reread,
                Some(Err(TransportError::Http { status: 500 })),
                window,
                cx,
            );
            assert!(this.session_management.notice.is_none());
        }
    });
}

#[gpui_kit::test]
fn deleted_fork_callback_cannot_consume_a_replacement_read(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    update(window, &workspace, cx, |this, window, cx| {
        let session: SessionId = "replacement_fork".into();
        let key = this.mutation_key(&session);
        let old = this.branching.begin_fork_read(key.clone());
        this.apply_server_events(
            vec![ServerEvent::SessionDeleted {
                session_id: session.clone(),
            }],
            window,
            cx,
        );
        this.branching.switch_runtime();
        let current = this.branching.begin_fork_read(key.clone());
        assert_ne!(old, current, "read identities survive runtime resets");
        this.finish_fork_read(
            key.clone(),
            old,
            true,
            Some(Ok(SessionRecord::new(session.clone(), "/stale"))),
            window,
            cx,
        );
        assert!(this.sessions.get(&session).is_none());
        assert!(
            this.branching.fork_reads.contains_key(&key),
            "the replacement still owns its response"
        );
        this.finish_fork_read(
            key.clone(),
            old,
            true,
            Some(Err(TransportError::Http { status: 500 })),
            window,
            cx,
        );
        assert!(this.branching.fork_reads.contains_key(&key));
        assert!(this.session_management.notice.is_none());
        this.finish_fork_read(
            key,
            current,
            false,
            Some(Ok(SessionRecord::new(session.clone(), "/fresh"))),
            window,
            cx,
        );
        assert_eq!(
            this.sessions
                .get(&session)
                .expect("session remains present")
                .directory
                .as_deref(),
            Some("/fresh")
        );
    });
}

#[gpui_kit::test]
fn commit_retires_the_initial_history_generation(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (session, _) = select_root(window, &workspace, cx);
    update(window, &workspace, cx, |this, _, cx| {
        let open = this
            .open_sessions
            .get_mut(&session)
            .expect("selected transcript is open");
        open.transcript = openchamber_native_core::transcript::Transcript::new();
        let generation = open.history_generation;
        this.on_revert_committed(&session, &"boundary-not-loaded".into(), cx);
        let open = &this.open_sessions[&session];
        assert_ne!(
            open.history_generation, generation,
            "the old read must not settle history or send attempts"
        );
        assert!(open.transcript.is_empty());
    });
}

#[gpui_kit::test]
fn fork_and_stop_errors_belong_to_the_captured_runtime(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    update(window, &workspace, cx, |this, window, cx| {
        let stale = SessionKey {
            runtime: "old".into(),
            session: "session".into(),
        };
        this.finish_fork(
            stale.clone(),
            0,
            None,
            Some(Err(TransportError::Http { status: 500 })),
            window,
            cx,
        );
        assert!(
            this.session_management.notice.is_none(),
            "stale fork error stays out of the current runtime"
        );
        this.finish_stop_subagent(
            stale,
            Some(Err(TransportError::Http { status: 500 })),
            window,
            cx,
        );
        assert!(
            this.session_management.notice.is_none(),
            "stale stop error stays out of the current runtime"
        );
    });
}

fn push(workspace: &mut Workspace, session: &SessionId, message: Message) {
    let open = workspace.open_sessions.get_mut(session).expect("visited");
    let _ = open.transcript.push(message);
    let count = open.transcript.len();
    open.list.reset(count);
}

fn info(id: &str, role: Role) -> MessageInfo {
    MessageInfo {
        id: MessageId::from(id),
        role,
        created: 9_000_000_000_000,
        completed: None,
        notice: None,
        assistant: None,
        compaction: None,
        synthetic: None,
    }
}

#[gpui_kit::test]
fn a_staged_revert_hides_its_cut_and_a_refusal_restores_it(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (session, users) = select_root(window, &workspace, cx);
    let last = users.last().expect("a prompt").clone();
    let visible = |workspace: &Entity<Workspace>, cx: &mut TestAppContext| {
        cx.read(|cx| {
            let this = workspace.read(cx);
            let rows = this.visible_rows(&session);
            this.open_sessions[&session]
                .transcript
                .row_of(&last)
                .is_some_and(|row| row < rows)
        })
    };
    assert!(visible(&workspace, cx));
    assert!(!shown(window, "revert-strip", cx));
    let key = update(window, &workspace, cx, |this, window, cx| {
        let key = this.mutation_key(&session);
        // What `revert_to_message` does before its request leaves.
        this.branching
            .pending_reverts
            .insert(key.clone(), last.clone());
        this.remeasure_transcript(&session);
        this.composer.update(cx, |composer, cx| {
            composer.set_value("restored prompt", window, cx)
        });
        key
    });
    assert!(
        !visible(&workspace, cx),
        "the optimistic marker hides the prompt at once"
    );
    assert!(shown(window, "revert-strip", cx));
    assert!(
        shown(window, "revert-restore", cx),
        "one reverted prompt shows its own Restore"
    );
    update(window, &workspace, cx, |this, window, cx| {
        this.finish_revert(
            key.clone(),
            Some(crate::session_branching::RevertRestore {
                key: this
                    .composer_key
                    .clone()
                    .expect("selected session has a draft owner"),
                previous: "earlier draft".into(),
                restored: "restored prompt".into(),
            }),
            Some(Err(TransportError::Http { status: 500 })),
            window,
            cx,
        )
    });
    assert!(
        visible(&workspace, cx),
        "a refused stage brings the messages back"
    );
    assert!(!shown(window, "revert-strip", cx));
    // The composer held the restored text only if the prompt had one.
    let text = composer_text(&workspace, cx);
    assert!(
        text == "earlier draft" || text == "restored prompt",
        "the composer keeps text the user typed: {text}"
    );
    assert!(cx.read(|cx| workspace.read(cx).branching.pending_reverts.is_empty()));
}

#[gpui_kit::test]
fn the_server_marker_lists_reverted_prompts_and_a_commit_trims_them(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (session, users) = select_root(window, &workspace, cx);
    assert!(users.len() >= 2, "the synthetic conversation has turns");
    let cut = users[users.len() - 2].clone();
    let patch = |revert| ServerEvent::SessionPatched {
        session_id: session.clone(),
        patch: SessionPatch {
            revert: Some(revert),
            ..SessionPatch::default()
        },
    };
    update(window, &workspace, cx, |this, window, cx| {
        this.apply_server_events(vec![patch(Some(cut.clone()))], window, cx)
    });
    assert!(
        shown(window, "revert-strip-toggle", cx),
        "two prompts collapse to a count"
    );
    assert!(!shown(window, "revert-strip-list", cx));
    cx.update_window(window, |_, window, cx| {
        window.click(name("revert-strip-toggle"), cx);
        window.render_frame(cx);
    })
    .expect("window is open");
    assert!(
        shown(window, "revert-strip-list", cx),
        "the count expands in place"
    );
    assert!(shown(window, format!("reverted:{cut}"), cx));
    let before = cx.read(|cx| workspace.read(cx).open_sessions[&session].transcript.len());
    update(window, &workspace, cx, |this, window, cx| {
        this.apply_server_events(
            vec![
                ServerEvent::RevertCommitted {
                    session_id: session.clone(),
                    to: cut.clone(),
                },
                patch(None),
            ],
            window,
            cx,
        )
    });
    let after = cx.read(|cx| workspace.read(cx).open_sessions[&session].transcript.len());
    assert!(
        after < before,
        "the commit deletes the cut and what follows"
    );
    assert!(cx.read(|cx| {
        workspace.read(cx).open_sessions[&session]
            .transcript
            .row_of(&cut)
            .is_none()
    }));
    assert!(!shown(window, "revert-strip", cx));
}

#[gpui_kit::test]
fn a_fork_opens_with_the_forked_prompt_in_its_composer(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (session, _) = select_root(window, &workspace, cx);
    let (key, navigation, directory) = cx.read(|cx| {
        let this = workspace.read(cx);
        (
            this.mutation_key(&session),
            this.navigation_revision,
            this.sessions
                .get(&session)
                .and_then(|s| s.directory.clone()),
        )
    });
    let fork = SessionRecord {
        title: "Forked".to_owned(),
        ..SessionRecord::new("ses_fork", directory.expect("a directory"))
    };
    update(window, &workspace, cx, |this, window, cx| {
        this.finish_fork(
            key,
            navigation,
            Some("the forked prompt".to_owned()),
            Some(Ok(fork)),
            window,
            cx,
        )
    });
    cx.read(|cx| {
        let this = workspace.read(cx);
        assert_eq!(
            this.selected.as_ref().map(SessionId::as_str),
            Some("ses_fork")
        );
        assert!(this.sessions.get(&SessionId::from("ses_fork")).is_some());
    });
    assert_eq!(composer_text(&workspace, cx), "the forked prompt");
}

#[gpui_kit::test]
fn a_linked_btw_fork_opens_its_panel_and_escape_collapses_it(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (session, _) = select_root(window, &workspace, cx);
    let directory = cx.read(|cx| {
        workspace
            .read(cx)
            .sessions
            .get(&session)
            .and_then(|record| record.directory.clone())
            .expect("a directory")
    });
    let fork = SessionRecord {
        kind: SessionKind::Btw,
        title: "btw: why".to_owned(),
        btw: BtwMetadata {
            original: Some(session.clone()),
            ..BtwMetadata::default()
        },
        ..SessionRecord::new("ses_btw", directory)
    };
    update(window, &workspace, cx, |this, window, cx| {
        this.apply_server_events(
            vec![
                ServerEvent::SessionCreated(fork),
                ServerEvent::SessionPatched {
                    session_id: session.clone(),
                    patch: SessionPatch {
                        metadata: Some(MetadataFacts {
                            btw: BtwMetadata {
                                link: Some(SessionId::from("ses_btw")),
                                ..BtwMetadata::default()
                            },
                            ..MetadataFacts::default()
                        }),
                        ..SessionPatch::default()
                    },
                },
            ],
            window,
            cx,
        )
    });
    assert!(shown(window, "btw-panel", cx));
    assert!(
        cx.read(
            |cx| !workspace.read(cx).sidebar_items.iter().any(|item| matches!(
                item,
                crate::sidebar_item::SidebarItem::Session { session_id, .. }
                    if session_id.as_str() == "ses_btw"
            ))
        ),
        "a side conversation has no sidebar row"
    );
    assert!(shown(window, "btw-messages", cx));
    assert!(cx.read(|cx| workspace.read(cx).btw_takes_composer()));
    assert!(
        cx.read(|cx| workspace
            .read(cx)
            .open_sessions
            .contains_key(&SessionId::from("ses_btw"))),
        "the side conversation's transcript is loaded"
    );
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| this.focus_composer(window, cx));
        window.press("escape", cx);
        window.render_frame(cx);
    })
    .expect("window is open");
    assert!(!cx.read(|cx| workspace.read(cx).btw_takes_composer()));
    assert!(
        shown(window, "btw-expand", cx),
        "the panel collapses to its header"
    );
    assert!(!shown(window, "btw-messages", cx));
}

#[gpui_kit::test]
fn a_subagent_report_and_call_open_the_child_and_return(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (parent, child) = cx.read(|cx| {
        let this = workspace.read(cx);
        let child = this
            .sessions
            .active()
            .iter()
            .find(|record| {
                record
                    .parent_id
                    .as_ref()
                    .is_some_and(|parent| this.session(parent).is_some())
            })
            .expect("a child session");
        (child.parent_id.clone().expect("parent"), child.id.clone())
    });
    update(window, &workspace, cx, |this, window, cx| {
        this.select_session(parent.clone(), window, cx);
        let mut report = info("msg_report", Role::Synthetic);
        report.synthetic = Some(Box::new(SyntheticFacts {
            description: Some("Explore the repo".to_owned()),
            subagent: Some(SubagentRun {
                child: child.clone(),
                agent: Some("explore".to_owned()),
                state: SubagentRunState::Completed,
            }),
            ..SyntheticFacts::default()
        }));
        push(
            this,
            &parent,
            Message {
                info: report,
                parts: Vec::new(),
            },
        );
        push(
            this,
            &parent,
            Message {
                info: info("msg_call", Role::Assistant),
                parts: vec![Part {
                    id: PartId::from("call_1"),
                    content: PartContent::Tool {
                        tool: "subagent".to_owned(),
                        status: ToolStatus::Running,
                        title: None,
                        output: None,
                        elided: None,
                        child: Some(child.clone()),
                    },
                    time: PartTime::default(),
                }],
            },
        );
        this.scroll_to_tail(cx);
    });
    assert!(shown(window, "subtask-open:msg_report", cx));
    assert!(shown(window, "subtask-open:call_1", cx));
    assert!(
        !shown(window, "subtask-stop:call_1", cx),
        "offline, nothing can be stopped"
    );
    assert!(
        !shown(window, "return-to-parent", cx),
        "a root has no parent"
    );
    cx.update_window(window, |_, window, cx| {
        window.click(name("subtask-open:call_1"), cx);
        window.render_frame(cx);
    })
    .expect("window is open");
    assert_eq!(
        cx.read(|cx| workspace.read(cx).selected.clone()),
        Some(child)
    );
    assert!(shown(window, "return-to-parent", cx));
    cx.update_window(window, |_, window, cx| {
        window.click(name("return-to-parent"), cx);
        window.render_frame(cx);
    })
    .expect("window is open");
    assert_eq!(
        cx.read(|cx| workspace.read(cx).selected.clone()),
        Some(parent)
    );
}

#[gpui_kit::test]
fn completions_for_another_runtime_change_nothing(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (session, users) = select_root(window, &workspace, cx);
    let stale = SessionKey {
        runtime: "another".into(),
        session: session.clone(),
    };
    update(window, &workspace, cx, |this, window, cx| {
        this.finish_revert(stale.clone(), None, Some(Ok(users[0].clone())), window, cx);
        this.finish_fork(
            stale,
            this.navigation_revision,
            Some("x".to_owned()),
            Some(Ok(SessionRecord::new("ses_other_runtime", "/p"))),
            window,
            cx,
        );
    });
    cx.read(|cx| {
        let this = workspace.read(cx);
        assert_eq!(this.revert_marker(&session), None);
        assert!(
            this.sessions
                .get(&SessionId::from("ses_other_runtime"))
                .is_none()
        );
        assert_eq!(this.selected.as_ref(), Some(&session));
    });
}
