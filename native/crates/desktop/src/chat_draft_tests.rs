use super::*;
use crate::connection::ServerConfig;
use gpui_kit::{AppContext, TestAppContext};
use openchamber_native_core::draft::Draft;
use openchamber_native_core::draft_content::{
    AgentMention, DraftContext, DraftFile, DraftFileContent, Mention, SkillRef,
};
use openchamber_native_core::ids::{AttachmentId, ContextItemId, SessionId};
use openchamber_native_core::session::SessionRecord;
use openchamber_native_core::session_index::SessionChange;
use openchamber_native_transport::endpoint::Endpoint;

fn open(cx: &mut TestAppContext) -> (gpui_kit::AnyWindowHandle, gpui_kit::Entity<Workspace>) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update(|cx| {
        workspace.update(cx, |this, _| {
            this.connection = Some(
                crate::connection::Connection::new(ServerConfig::for_endpoint(
                    Endpoint::parse("http://localhost:9").expect("endpoint"),
                ))
                .expect("connection"),
            );
            this.session_admission.roots_ready = true;
        })
    });
    (window, workspace)
}

#[gpui_kit::test]
fn native_agent_owner_keeps_selected_session_scope_with_a_retained_prepared_chat(
    cx: &mut TestAppContext,
) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            let project = this.topology.projects[0].path.clone();
            let session = SessionId::from("native-agent-project-session");
            let record = SessionRecord::new(session.clone(), &project);
            let _ = this.sessions.apply([SessionChange::Created(record)]);
            assert!(this.session_admitted(&session));
            this.open_new_session(NewSession::Chat, window, cx);
            let chat = this.composer_key.clone().expect("composer key");
            let retained = Draft::new("Retained Chat draft");
            this.drafts.save(chat.clone(), retained.clone());
            this.restore_composer_draft(window, cx);
            this.chat_drafts
                .slots
                .get_mut(&this.runtime)
                .expect("chat slot")
                .preparation = Preparation::Ready;
            assert_eq!(
                this.chat_prepared_directory().as_deref(),
                Some(chat.directory())
            );

            this.select_session(session.clone(), window, cx);

            assert_eq!(this.selected.as_ref(), Some(&session));
            assert_eq!(
                this.composer_key
                    .as_ref()
                    .expect("composer key")
                    .directory(),
                project
            );
            let owner = this
                .native_owner_key("synthetic-native")
                .expect("native owner");
            assert_eq!(owner.scope.directory, project);
            assert_eq!(owner.scope.session_id.as_deref(), Some(session.as_str()));
            assert!(
                this.chat_draft_open(),
                "selection retains the Chat destination"
            );
            assert_eq!(
                this.chat_prepared_directory().as_deref(),
                Some(chat.directory())
            );
            assert_eq!(this.drafts.get(&chat), Some(&retained));
        });
    })
    .expect("window");
}

#[gpui_kit::test]
fn native_agent_owner_uses_only_the_prepared_directory_for_a_new_chat(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            this.open_new_session(NewSession::Chat, window, cx);
            let chat = this.composer_key.clone().expect("composer key");
            assert!(this.selected.is_none());
            assert!(this.native_owner_key("synthetic-native").is_none());

            this.chat_drafts
                .slots
                .get_mut(&this.runtime)
                .expect("chat slot")
                .preparation = Preparation::Ready;
            let owner = this
                .native_owner_key("synthetic-native")
                .expect("native owner");
            assert_eq!(owner.scope.directory, chat.directory());
            assert_eq!(owner.scope.session_id, None);
        });
    })
    .expect("window");
}

#[gpui_kit::test]
fn moving_a_chat_draft_preserves_complete_content_and_utf8_selection(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            this.open_new_session(NewSession::Chat, window, cx);
            let chat = this.composer_key.clone().expect("composer key");
            let mut draft = Draft::new("😀 @plan hello");
            draft.selection = 4..10;
            draft.content.files.push(DraftFile {
                id: AttachmentId::from("reference"),
                name: Some("notes.txt".into()),
                media_type: Some("text/plain".into()),
                byte_len: None,
                source_document_id: None,
                content: DraftFileContent::Reference {
                    uri: "data:text/plain;base64,YQ==".into(),
                },
            });
            draft.content.agents.push(AgentMention {
                name: "plan".into(),
                mention: Some(Mention {
                    start: 3,
                    end: 8,
                    text: "@plan".into(),
                }),
            });
            draft.content.skills.push(SkillRef { id: "kept".into() });
            draft.content.metadata_json = Some("{\"retained\":true}".into());
            draft.content.context.push(DraftContext {
                id: ContextItemId::from("context"),
                text: "quoted λ".into(),
                description: Some("selection".into()),
                metadata_json: None,
                instructions: Some("Read this".into()),
            });
            this.drafts.save(chat.clone(), draft.clone());
            this.restore_composer_draft(window, cx);
            let project = this.topology.projects[0].path.clone();
            this.move_draft_to(&project, window, cx);
            let project_key = this.composer_key.clone().expect("composer key");
            assert_eq!(this.drafts.get(&project_key), Some(&draft));
            assert!(this.drafts.get(&chat).is_none());
            this.move_draft_to(chat_draft::CHAT_PROJECT_ID, window, cx);
            assert_eq!(
                this.drafts
                    .get(this.composer_key.as_ref().expect("composer key")),
                Some(&draft)
            );
            assert!(this.drafts.get(&project_key).is_none());
        })
    })
    .expect("window");
}

#[gpui_kit::test]
fn root_failure_retains_the_holding_draft_and_never_grants_execution(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            let roots = this.topology.chat_roots.take().expect("chat roots");
            this.session_admission.roots_ready = false;
            this.chat_roots_read(false, window, cx);
            this.open_new_session(NewSession::Chat, window, cx);
            let holding = this.composer_key.clone().expect("composer key");
            this.drafts.save(holding.clone(), Draft::new("kept"));
            this.restore_composer_draft(window, cx);
            assert_eq!(this.chat_roots_status(), RootsStatus::Failed);
            assert_eq!(this.chat_config_directory(), None);
            assert_eq!(this.chat_prepared_directory(), None);
            assert_eq!(this.mcp_directory(), None);
            assert!(!this.is_chat_creation(&holding));
            this.topology.chat_roots = Some(roots);
            this.session_admission.roots_ready = true;
            this.chat_roots_read(true, window, cx);
            let named = this.composer_key.clone().expect("composer key");
            assert_ne!(holding, named);
            assert!(this.is_chat_creation(&named));
            assert_eq!(
                this.chat_prepared_directory(),
                None,
                "naming is not mkdir success"
            );
            assert_eq!(this.drafts.get(&named).expect("draft").text, "kept");
            assert!(this.drafts.get(&holding).is_none());
        })
    })
    .expect("window");
}

#[gpui_kit::test]
fn a_metadata_conflict_leaves_both_drafts_and_target_unchanged(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            this.open_new_session(NewSession::Chat, window, cx);
            let chat = this.composer_key.clone().expect("composer key");
            let mut source = Draft::new("source");
            source.content.metadata_json = Some("{\"value\":1}".into());
            this.drafts.save(chat.clone(), source.clone());
            this.restore_composer_draft(window, cx);
            let project = this.topology.projects[0].path.clone();
            let destination = DraftKey::new(
                this.runtime.clone(),
                &project,
                DraftTarget::NewSession(crate::workspace::PROJECT_DRAFT_ID.into()),
            )
            .expect("test fixture");
            let mut kept = Draft::new("destination");
            kept.content.metadata_json = Some("{\"value\":2}".into());
            this.drafts.save(destination.clone(), kept.clone());
            this.move_draft_to(&project, window, cx);
            assert!(this.chat_draft_open());
            assert_eq!(this.composer_key.as_ref(), Some(&chat));
            assert_eq!(this.drafts.get(&chat), Some(&source));
            assert_eq!(this.drafts.get(&destination), Some(&kept));
        })
    })
    .expect("window");
}

#[gpui_kit::test]
fn renewal_never_restores_the_consumed_folder_when_newer_typing_remains(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            this.open_new_session(NewSession::Chat, window, cx);
            let used = this.composer_key.clone().expect("composer key");
            let mut newer = Draft::new("newer 😀 text");
            newer.selection = 6..10;
            this.drafts.save(used.clone(), newer.clone());
            this.restore_composer_draft(window, cx);
            this.handoff_chat_directory(&used);
            let runtime = this.runtime.clone();
            this.renew_chat_folder(&runtime, &used, window, cx);
            let renewed = this.chat_draft_key().expect("chat key");
            assert_ne!(renewed.directory(), used.directory());
            assert_eq!(this.composer_key.as_ref(), Some(&renewed));
            assert_eq!(this.drafts.get(&renewed), Some(&newer));
            assert!(this.drafts.get(&used).is_none());
            assert_eq!(this.chat_prepared_directory(), None);
            assert!(this.chat_drafts.handed_off.contains(&used));
        });
    })
    .expect("window");
}

#[gpui_kit::test]
fn renewal_moves_retained_chat_content_without_changing_the_selected_project_draft(
    cx: &mut TestAppContext,
) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            this.open_new_session(NewSession::Chat, window, cx);
            let used = this.composer_key.clone().expect("composer key");
            let newer = Draft::new("Retained while another target is open");
            this.drafts.save(used.clone(), newer.clone());
            this.restore_composer_draft(window, cx);
            let project = this.topology.projects[0].path.clone();
            this.choose_draft_project(&project, window, cx);
            let project_key = this.composer_key.clone();
            this.handoff_chat_directory(&used);
            this.renew_chat_folder(&this.runtime.clone(), &used, window, cx);
            assert_eq!(this.composer_key, project_key);
            assert!(!this.chat_draft_open());
            assert!(this.drafts.get(&used).is_none());
            let retained: Vec<_> = this.drafts.iter().filter(|(key, _)| matches!(key.target(), DraftTarget::NewSession(id) if id.as_str() == CHAT_DRAFT_ID)).collect();
            assert_eq!(retained.len(), 1);
            assert_ne!(retained[0].0.directory(), used.directory());
            assert_eq!(retained[0].1, &newer);
        });
    }).expect("window");
}

#[gpui_kit::test]
fn execution_readiness_waits_for_current_runtime_root_discovery(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            this.open_new_session(NewSession::Chat, window, cx);
            let key = this.composer_key.clone().expect("composer key");
            this.chat_drafts
                .slots
                .get_mut(&this.runtime)
                .expect("chat slot")
                .preparation = Preparation::Ready;
            assert!(this.chat_prepared_directory().is_some());
            this.topology.chat_roots = None;
            assert_eq!(this.chat_prepared_directory(), None);
            assert!(!this.chat_creation_ready(&key));
            assert_eq!(this.mcp_directory(), None);
        });
    })
    .expect("window");
}

#[gpui_kit::test]
fn chat_sidebar_reopens_the_exact_draft_and_discard_removes_its_preview(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            this.open_new_session(NewSession::Chat, window, cx);
            let chat = this.composer_key.clone().expect("composer key");
            let draft = Draft::new("Retained Chat draft");
            this.drafts.save(chat.clone(), draft.clone());
            this.save_draft_preview(&chat, &draft, cx);
            this.restore_composer_draft(window, cx);
            assert!(this.sidebar_items.iter().any(|item| matches!(item, crate::sidebar_item::SidebarItem::Draft { draft, .. } if draft == &chat)));
            let project = this.topology.projects[0].path.clone();
            this.choose_draft_project(&project, window, cx);
            this.open_draft(&chat, window, cx);
            assert!(this.chat_draft_open());
            assert_eq!(this.composer_key.as_ref(), Some(&chat));
            assert_eq!(this.composer.read(cx).value().to_string(), draft.text);
            assert!(!this.draft_projects.contains_key(&this.runtime));
            this.discard_draft(&chat, window, cx);
            assert!(this.drafts.get(&chat).is_none());
            assert!(!this.draft_previews.contains_key(&chat));
        });
    }).expect("window");
}

#[gpui_kit::test]
fn prepared_attachment_retarget_keeps_bytes_and_fixed_uri(cx: &mut TestAppContext) {
    use crate::attachment_catalog::{PreparedAttachment, StagingDescriptor, StagingPhase};
    use crate::attachment_spool::{BlobDescriptor, BlobId};
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            this.open_new_session(NewSession::Chat, window, cx);
            let chat = this.composer_key.clone().expect("composer key");
            let id = AttachmentId::from("0123456789abcdef0123456789abcdef");
            let uri = "file:///tmp/opencode/openchamber/0123456789abcdef/notes.txt";
            let blob =
                BlobDescriptor::new(BlobId::parse(id.as_str()).expect("blob ID"), 3, [0x55; 32]);
            this.attachments.insert(
                id.clone(),
                PreparedAttachment {
                    blob,
                    staging: Some(StagingDescriptor {
                        runtime: this.runtime.clone(),
                        directory: chat.directory().to_owned(),
                        temporary_directory: "/tmp/opencode".into(),
                        server_path: "/tmp/opencode/openchamber/0123456789abcdef/notes.txt".into(),
                        uri: uri.into(),
                        filename: "notes.txt".into(),
                        phase: StagingPhase::Ready,
                    }),
                },
            );
            let mut draft = Draft::new("keep file");
            draft.content.files.push(DraftFile {
                id: id.clone(),
                name: Some("notes.txt".into()),
                media_type: Some("text/plain".into()),
                byte_len: Some(3),
                source_document_id: None,
                content: DraftFileContent::Prepared,
            });
            this.drafts.save(chat.clone(), draft.clone());
            this.restore_composer_draft(window, cx);
            let project = this.topology.projects[0].path.clone();
            this.move_draft_to(&project, window, cx);
            assert_eq!(
                this.drafts
                    .get(this.composer_key.as_ref().expect("composer key")),
                Some(&draft)
            );
            let attachment = this.attachments.get(&id).expect("attachment");
            assert_eq!(attachment.blob, blob);
            assert_eq!(attachment.staging.as_ref().expect("staging").uri, uri);
            assert_eq!(
                attachment.staging.as_ref().expect("staging").directory,
                project
            );
        });
    })
    .expect("window");
}
