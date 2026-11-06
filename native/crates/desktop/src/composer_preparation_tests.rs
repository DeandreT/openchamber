use super::*;

use openchamber_native_core::message_queue::QueuedSendConfig;
use openchamber_native_transport::composer_preparation::ComposerSourceRange;
use openchamber_native_transport::prompt_parts::Metadata;
use openchamber_native_transport::queue_manifest::{
    QueueLocalFile, QueueMention, QueuePromptParts,
};
use serde_json::json;

const DIRECTORY: &str = "/repo/project";

fn mention() -> QueueMention {
    QueueMention {
        start: 0,
        end: 5,
        text: "stale".to_owned(),
    }
}

fn source(text: &str) -> QueueLocalCapture {
    QueueLocalCapture::new(
        DIRECTORY,
        QueueLocalItem {
            content: text.to_owned(),
            text: "ORIGINAL DELIVERED #never-reexpand".to_owned(),
            files: vec![QueueLocalFile {
                name: "/model/path/retained.txt".to_owned(),
                mime_type: "text/plain".to_owned(),
                byte_len: 3,
                sha256: sha256(b"old"),
                description: Some("file metadata".to_owned()),
                mention: Some(mention()),
            }],
            send_config: QueuedSendConfig {
                provider_id: "p".to_owned(),
                model_id: "m".to_owned(),
                agent: None,
                variant: None,
            },
            prompt_parts: QueuePromptParts {
                file_references: Vec::new(),
                agents: vec![QueueAgent {
                    name: "retained-agent".to_owned(),
                    mention: Some(mention()),
                }],
                skills: vec![QueueSkill {
                    id: "retained-skill".to_owned(),
                    mention: Some(mention()),
                }],
                metadata: Some(Metadata(
                    json!({"independent":{"keep":[1,2]}})
                        .as_object()
                        .expect("object")
                        .clone(),
                )),
                context: vec![QueuePromptContext {
                    id: None,
                    text: "quoted #evidence".to_owned(),
                    description: Some("unchanged".to_owned()),
                    metadata: Some(Metadata(
                        json!({"source":"evidence"})
                            .as_object()
                            .expect("object")
                            .clone(),
                    )),
                }],
            },
        },
        vec![MessageId::new("msg_original_context")],
        sha256,
    )
    .expect("complete source")
}

fn agent(name: &str, mode: Option<AgentMode>) -> AgentEntry {
    AgentEntry {
        name: name.to_owned(),
        display_name: None,
        mode,
        hidden: false,
        model: None,
        color: None,
    }
}

fn anchor(prefix: &str, text: &str) -> QueueMention {
    let start = prefix.encode_utf16().count() as u64;
    QueueMention {
        start,
        end: start + text.encode_utf16().count() as u64,
        text: text.to_owned(),
    }
}

fn capture_item(item: QueueLocalItem) -> QueueLocalCapture {
    QueueLocalCapture::new(
        DIRECTORY,
        item,
        vec![MessageId::new("msg_original_context")],
        sha256,
    )
    .expect("complete projected source")
}

fn excluded(ordinal: usize, identity: &str, mention: QueueMention) -> ComposerExcludedReference {
    ComposerExcludedReference {
        ordinal,
        identity: identity.to_owned(),
        original_mention: Some(mention.clone()),
        display_mention: Some(mention),
    }
}

fn owner() -> DraftKey {
    use openchamber_native_core::draft::DraftTarget;
    use openchamber_native_core::ids::{RuntimeId, SessionId};

    DraftKey::new(
        RuntimeId::new("original-runtime"),
        DIRECTORY,
        DraftTarget::Session(SessionId::new("ses_original")),
    )
    .expect("source owner")
}

fn preparer() -> CapturedComposerPreparation {
    use openchamber_native_transport::credential::Credential;
    use openchamber_native_transport::endpoint::Endpoint;

    let client = ServerClient::new(
        Endpoint::parse("https://example.invalid/").expect("endpoint"),
        Credential::None,
    )
    .expect("client");
    CapturedComposerPreparation::new(owner(), &client, DIRECTORY).expect("captured owner")
}

/// Supplies the captured catalogs directly so these tests perform no I/O.
fn plan(
    source: Arc<QueueLocalCapture>,
    agents: &[AgentEntry],
    skills: &[ComposerSkill],
    skill_context_id: Option<MessageId>,
    exclusions: Option<&ComposerSourceExclusions>,
) -> Result<ComposerEditPlan, ComposerPreparationError> {
    let tokens = skill_occurrences(&source.item().content);
    let (item, context_ids, file_requests) = process_mentions_with_tokens(
        &source,
        agents,
        skills,
        skill_context_id,
        exclusions,
        &tokens,
    )?;
    Ok(ComposerEditPlan {
        owner: owner(),
        source,
        item,
        context_ids,
        file_requests,
    })
}

fn output(name: &str, mention: Option<QueueMention>) -> QueueLocalFile {
    QueueLocalFile {
        name: name.to_owned(),
        mime_type: "text/plain".to_owned(),
        byte_len: 3,
        sha256: sha256(b"new"),
        description: Some("prepared output".to_owned()),
        mention,
    }
}

#[test]
fn edits_reprocess_display_and_preserve_all_independent_fields_and_ids() {
    let original = source("\n\n  new text\n\n");
    let original_bytes = original.bytes().to_vec();
    let (item, ids) = process_mentions(&original, &[], &[], None).expect("processed edit");
    assert_eq!(item.text, "  new text");
    assert_eq!(item.content, "\n\n  new text\n\n");
    assert_eq!(ids, original.context_ids());
    assert_eq!(
        item.prompt_parts.context,
        original.item().prompt_parts.context
    );
    assert_eq!(
        item.prompt_parts.metadata,
        original.item().prompt_parts.metadata
    );
    assert_eq!(item.send_config, original.item().send_config);
    let mut files = original.item().files.clone();
    files[0].mention = None;
    assert_eq!(item.files, files);
    assert_eq!(item.prompt_parts.agents[0].name, "retained-agent");
    assert_eq!(item.prompt_parts.skills[0].id, "retained-skill");
    assert!(item.prompt_parts.agents[0].mention.is_none());
    assert!(item.prompt_parts.skills[0].mention.is_none());
    assert_eq!(original.bytes(), original_bytes);
}

#[test]
fn agent_mentions_follow_upstream_catalog_order_boundaries_and_mode() {
    let agents = vec![
        agent("build", Some(AgentMode::Primary)),
        agent("later", Some(AgentMode::Subagent)),
        agent("first", Some(AgentMode::All)),
    ];
    let original = source("@first before @LATER, @build");
    let (item, _) = process_mentions(&original, &agents, &[], None).expect("agents");
    assert_eq!(
        item.text,
        original.item().content,
        "upstream preserves agent tokens"
    );
    assert_eq!(
        item.prompt_parts.agents[1].name, "later",
        "catalog order wins over textual order"
    );
    for text in ["email@later", "<@later>", "@later_more", "@laterZ"] {
        assert!(
            mentioned_agent(text, &agents).is_none(),
            "boundary for {text}"
        );
    }
    assert!(mentioned_agent("😀 (@later)", &agents).is_some());
    assert!(mentioned_agent("@build", &agents).is_none());
}

#[test]
fn new_mentions_ignore_hidden_agents_but_keep_retained_hidden_references() {
    let mut hidden = agent("hidden", Some(AgentMode::Subagent));
    hidden.hidden = true;
    let visible = agent("visible", Some(AgentMode::Subagent));
    let mut item = source("@hidden before @visible").item().clone();
    item.prompt_parts.agents[0].name = "hidden".to_owned();
    item.prompt_parts.agents[0].mention = Some(anchor("", "@hidden"));
    let original = capture_item(item);
    let (item, _) =
        process_mentions(&original, &[hidden.clone(), visible], &[], None).expect("visible agent");
    assert_eq!(
        item.prompt_parts
            .agents
            .iter()
            .map(|agent| agent.name.as_str())
            .collect::<Vec<_>>(),
        vec!["hidden", "visible"]
    );
    assert_eq!(
        item.prompt_parts.agents[0].mention,
        Some(anchor("", "@hidden"))
    );
    let text = source("@hidden remains literal");
    let (item, _) =
        process_mentions(&text, &[hidden], &[], None).expect("hidden names alone are prose");
    assert_eq!(item.text, text.item().content);
    assert_eq!(
        item.prompt_parts.agents.len(),
        1,
        "the new hidden token adds no reference"
    );
    assert!(!has_file_mention("@hidden remains literal", &[]));
    let mut hidden_path_name = agent("hidden.agent", Some(AgentMode::Subagent));
    hidden_path_name.hidden = true;
    assert!(
        has_file_mention("@hidden.agent", &[hidden_path_name]),
        "hidden catalog entries do not suppress normal file classification"
    );
}

#[test]
fn projected_valid_anchors_survive_edits_outside_their_ranges() {
    let text = "changed 😀 [retained.txt] @retained-agent $retained-skill tail";
    let mut item = source(text).item().clone();
    item.files[0].mention = Some(anchor("changed 😀 ", "[retained.txt]"));
    item.prompt_parts.agents[0].mention =
        Some(anchor("changed 😀 [retained.txt] ", "@retained-agent"));
    item.prompt_parts.skills[0].mention = Some(anchor(
        "changed 😀 [retained.txt] @retained-agent ",
        "$retained-skill",
    ));
    // The document owner already rebased these ranges for the prefix edit.
    let original = capture_item(item);
    let (item, ids) = process_mentions(&original, &[], &[], None).expect("projected edit");
    assert_eq!(item.files, original.item().files);
    assert_eq!(
        item.prompt_parts.agents,
        original.item().prompt_parts.agents
    );
    assert_eq!(
        item.prompt_parts.skills,
        original.item().prompt_parts.skills
    );
    assert_eq!(
        item.prompt_parts.context,
        original.item().prompt_parts.context
    );
    assert_eq!(ids, original.context_ids());
}

#[test]
fn newline_trimming_rebases_utf16_ranges_without_trimming_unicode_prose() {
    let mut item = source("\n\n😀 @retained-agent $retained-skill [retained.txt]\n\n")
        .item()
        .clone();
    item.prompt_parts.agents[0].mention = Some(anchor("\n\n😀 ", "@retained-agent"));
    item.prompt_parts.skills[0].mention =
        Some(anchor("\n\n😀 @retained-agent ", "$retained-skill"));
    item.files[0].mention = Some(anchor(
        "\n\n😀 @retained-agent $retained-skill ",
        "[retained.txt]",
    ));
    let original = capture_item(item);
    let (item, _) = process_mentions(&original, &[], &[], None).expect("trimmed edit");
    assert_eq!(
        item.text,
        "😀 @retained-agent $retained-skill [retained.txt]"
    );
    assert_eq!(
        item.prompt_parts.agents[0].mention,
        Some(anchor("😀 ", "@retained-agent"))
    );
    assert_eq!(
        item.prompt_parts.skills[0].mention,
        Some(anchor("😀 @retained-agent ", "$retained-skill"))
    );
    assert_eq!(
        item.files[0].mention,
        Some(anchor(
            "😀 @retained-agent $retained-skill ",
            "[retained.txt]"
        ))
    );
    assert_eq!(
        item.prompt_parts.agents[0]
            .mention
            .as_ref()
            .expect("agent anchor")
            .start,
        3
    );
}

#[test]
fn invalid_utf16_mismatched_and_trimmed_anchors_clear_without_removing_references() {
    let mut item = source("\n😀 literal\n").item().clone();
    item.files[0].mention = Some(QueueMention {
        start: 1,
        end: 2,
        text: "😀".to_owned(),
    });
    item.prompt_parts.agents[0].mention = Some(QueueMention {
        start: 4,
        end: 11,
        text: "changed".to_owned(),
    });
    item.prompt_parts.skills[0].mention = Some(QueueMention {
        start: 4,
        end: 99,
        text: "literal".to_owned(),
    });
    let original = capture_item(item);
    let (item, _) = process_mentions(&original, &[], &[], None).expect("stale ranges clear");
    assert!(item.files[0].mention.is_none());
    assert!(item.prompt_parts.agents[0].mention.is_none());
    assert!(item.prompt_parts.skills[0].mention.is_none());
    assert_eq!(item.files.len(), 1);
    assert_eq!(item.prompt_parts.agents[0].name, "retained-agent");
    assert_eq!(item.prompt_parts.skills[0].id, "retained-skill");
    let mut item = original.item().clone();
    item.prompt_parts.skills[0].mention = Some(anchor("\n😀 ", "literal\n"));
    let original = capture_item(item);
    let (item, _) = process_mentions(&original, &[], &[], None).expect("trim overlap");
    assert!(item.prompt_parts.skills[0].mention.is_none());
}

#[test]
fn snippet_expansion_maps_valid_anchors_before_and_after_actual_replacement() {
    let prefix = "[retained.txt] @retained-agent ";
    let token = "#snippet";
    let suffix = " $retained-skill";
    let mut item = source(&format!("{prefix}{token}{suffix}")).item().clone();
    item.files[0].mention = Some(anchor("", "[retained.txt]"));
    item.prompt_parts.agents[0].mention = Some(anchor("[retained.txt] ", "@retained-agent"));
    item.prompt_parts.skills[0].mention =
        Some(anchor(&format!("{prefix}{token} "), "$retained-skill"));
    let original = capture_item(item);
    let bytes = original.bytes().to_vec();
    let (mut item, _) = process_mentions(&original, &[], &[], None).expect("before expansion");
    let input_end = (prefix.len() + token.len() + suffix.len()) as u64;
    let mapped = ComposerExpansion::from_parts(
        &item.text,
        format!("P\n\n{prefix}expanded{suffix}\n\nQ"),
        vec![
            ComposerSourceRange {
                input_start: 0,
                input_end: prefix.len() as u64,
                output_start: 3,
                output_end: 3 + prefix.len() as u64,
            },
            ComposerSourceRange {
                input_start: (prefix.len() + token.len()) as u64,
                input_end,
                output_start: 3 + prefix.len() as u64 + 8,
                output_end: 3 + prefix.len() as u64 + 8 + suffix.len() as u64,
            },
        ],
    )
    .expect("actual callback ranges");
    apply_expansion(&mut item, mapped);
    assert_eq!(
        item.files[0].mention,
        Some(anchor("P\n\n", "[retained.txt]"))
    );
    assert_eq!(
        item.prompt_parts.agents[0].mention,
        Some(anchor("P\n\n[retained.txt] ", "@retained-agent"))
    );
    assert_eq!(
        item.prompt_parts.skills[0].mention,
        Some(anchor(
            &format!("P\n\n{prefix}expanded "),
            "$retained-skill"
        ))
    );
    assert_eq!(item.text, format!("P\n\n{prefix}expanded{suffix}\n\nQ"));
    assert_eq!(original.bytes(), bytes);
}

#[test]
fn same_spelling_generated_text_clears_only_the_replaced_span() {
    let mut item = source("@retained-agent #self rest").item().clone();
    item.prompt_parts.agents[0].mention = Some(anchor("", "@retained-agent"));
    item.files[0].mention = Some(anchor("@retained-agent ", "#self"));
    let original = capture_item(item);
    let (mut item, _) =
        process_mentions(&original, &[], &[], None).expect("before same-spelling expansion");
    let start = "@retained-agent ".len() as u64;
    let end = start + "#self".len() as u64;
    let length = item.text.len() as u64;
    let mapped = ComposerExpansion::from_parts(
        &item.text,
        item.text.clone(),
        vec![
            ComposerSourceRange {
                input_start: 0,
                input_end: start,
                output_start: 0,
                output_end: start,
            },
            ComposerSourceRange {
                input_start: end,
                input_end: length,
                output_start: end,
                output_end: length,
            },
        ],
    )
    .expect("generated same-spelling token is deliberately unmapped");
    apply_expansion(&mut item, mapped);
    assert!(item.files[0].mention.is_none());
    assert_eq!(item.files[0].name, original.item().files[0].name);
    assert_eq!(
        item.prompt_parts.agents[0].mention,
        Some(anchor("", "@retained-agent"))
    );
}

#[test]
fn removed_occurrences_do_not_reappear_and_equal_id_retained_occurrences_survive() {
    let text = "@removed $Removed @old.txt then @removed $Removed";
    let later = "@removed $Removed @old.txt then ";
    let mut item = source(text).item().clone();
    item.prompt_parts.agents = vec![QueueAgent {
        name: "removed".to_owned(),
        mention: Some(anchor(later, "@removed")),
    }];
    item.prompt_parts.skills = vec![
        QueueSkill {
            id: "removed-id".to_owned(),
            mention: Some(anchor(&format!("{later}@removed "), "$Removed")),
        },
        QueueSkill {
            id: "removed-id".to_owned(),
            mention: None,
        },
    ];
    let original = capture_item(item);
    let exclusions = ComposerSourceExclusions {
        source: 1,
        file_references: Vec::new(),
        agents: vec![excluded(0, "removed", anchor("", "@removed"))],
        skills: vec![excluded(0, "removed-id", anchor("@removed ", "$Removed"))],
        files: vec![excluded(
            0,
            "/model/path/old.txt",
            anchor("@removed $Removed ", "@old.txt"),
        )],
    };
    validate_exclusions(1, Some(&exclusions)).expect("this source only");
    let agents = [agent("removed", Some(AgentMode::Subagent))];
    let skills = [ComposerSkill {
        name: "Removed".to_owned(),
        id: Some("removed-id".to_owned()),
    }];
    let (item, ids) =
        process_mentions_excluding(&original, &agents, &skills, None, Some(&exclusions))
            .expect("removed occurrence stays removed");
    assert_eq!(
        item.prompt_parts.agents,
        original.item().prompt_parts.agents
    );
    assert_eq!(
        item.prompt_parts.skills,
        original.item().prompt_parts.skills
    );
    assert_eq!(
        item.files[0].name,
        original.item().files[0].name,
        "the old ordinal never indexes the retained array"
    );
    assert_eq!(ids, original.context_ids());
}

#[test]
fn exclusions_suppress_only_the_old_inline_occurrence_and_only_in_its_source() {
    let mut item = source("@removed $Removed and @removed $Removed")
        .item()
        .clone();
    item.prompt_parts.agents.clear();
    item.prompt_parts.skills.clear();
    let original = capture_item(item);
    let exclusions = ComposerSourceExclusions {
        source: 2,
        file_references: Vec::new(),
        agents: vec![excluded(3, "removed", anchor("", "@removed"))],
        skills: vec![excluded(4, "removed-id", anchor("@removed ", "$Removed"))],
        files: Vec::new(),
    };
    let agents = [agent("removed", Some(AgentMode::Subagent))];
    let skills = [ComposerSkill {
        name: "Removed".to_owned(),
        id: Some("removed-id".to_owned()),
    }];
    let (item, _) =
        process_mentions_excluding(&original, &agents, &skills, None, Some(&exclusions))
            .expect("new equal-ID occurrence is eligible");
    assert_eq!(item.prompt_parts.agents[0].name, "removed");
    assert_eq!(item.prompt_parts.skills[0].id, "removed-id");
    assert!(
        validate_exclusions(1, Some(&exclusions)).is_err(),
        "a different source cannot use these exclusions"
    );
    let (item, _) = process_mentions(&original, &agents, &skills, None)
        .expect("another source has its own normal parsing");
    assert_eq!(item.prompt_parts.agents[0].name, "removed");
}

#[test]
fn removed_tokens_alone_create_no_agent_skill_instruction_or_file_read() {
    let mut item = source("\n@removed $Removed @old.txt\n").item().clone();
    item.prompt_parts.agents.clear();
    item.prompt_parts.skills.clear();
    let original = capture_item(item);
    let exclusions = ComposerSourceExclusions {
        source: 1,
        file_references: Vec::new(),
        agents: vec![excluded(0, "removed", anchor("\n", "@removed"))],
        skills: vec![excluded(
            0,
            "unlisted-folder-id",
            anchor("\n@removed ", "$Removed"),
        )],
        files: vec![excluded(
            0,
            "/model/path/old.txt",
            anchor("\n@removed $Removed ", "@old.txt"),
        )],
    };
    let agents = [agent("removed", Some(AgentMode::Subagent))];
    let skills = [ComposerSkill {
        name: "Removed".to_owned(),
        id: None,
    }];
    let (item, ids) =
        process_mentions_excluding(&original, &agents, &skills, None, Some(&exclusions))
            .expect("excluded tokens are ordinary display text");
    assert_eq!(item.text, "@removed $Removed @old.txt");
    assert!(item.prompt_parts.agents.is_empty());
    assert!(item.prompt_parts.skills.is_empty());
    assert_eq!(
        item.prompt_parts.context,
        original.item().prompt_parts.context
    );
    assert_eq!(ids, original.context_ids());
}

#[test]
fn original_positions_require_proven_identical_display_and_unknown_positions_fail_closed() {
    let mut item = source("@removed").item().clone();
    item.text = item.content.clone();
    item.prompt_parts.agents.clear();
    let original = capture_item(item);
    let exclusions = ComposerSourceExclusions {
        source: 1,
        agents: vec![ComposerExcludedReference {
            ordinal: 0,
            identity: "removed".to_owned(),
            original_mention: Some(anchor("", "@removed")),
            display_mention: None,
        }],
        skills: Vec::new(),
        files: Vec::new(),
        file_references: Vec::new(),
    };
    let agents = [agent("removed", Some(AgentMode::Subagent))];
    let (item, _) = process_mentions_excluding(&original, &agents, &[], None, Some(&exclusions))
        .expect("exact original/display identity");
    assert!(item.prompt_parts.agents.is_empty());
    let mut item = original.item().clone();
    item.content = "new prefix @removed".to_owned();
    let original = capture_item(item);
    let bytes = original.bytes().to_vec();
    assert!(matches!(
        process_mentions_excluding(&original, &agents, &[], None, Some(&exclusions)),
        Err(ComposerPreparationError::ExclusionPositionUnavailable)
    ));
    assert_eq!(original.bytes(), bytes);
}

#[test]
fn skills_use_exact_registered_names_and_append_one_captured_instruction() {
    let original = source("$Deploy ¥local ＄Deploy ￥local x$ignored $deploy $unknown");
    let skills = vec![
        ComposerSkill {
            name: "Deploy".to_owned(),
            id: Some("folder-id".to_owned()),
        },
        ComposerSkill {
            name: "local".to_owned(),
            id: None,
        },
    ];
    let id = MessageId::new("msg_new_skill_instruction");
    let (item, ids) = process_mentions(&original, &[], &skills, Some(id.clone())).expect("skills");
    assert_eq!(
        item.prompt_parts
            .skills
            .iter()
            .map(|skill| skill.id.as_str())
            .collect::<Vec<_>>(),
        vec!["retained-skill", "folder-id"]
    );
    assert_eq!(ids, vec![MessageId::new("msg_original_context"), id]);
    assert_eq!(
        item.prompt_parts.context[0],
        original.item().prompt_parts.context[0]
    );
    assert_eq!(
        item.prompt_parts.context[1].text,
        "The user explicitly mentioned these skills in their message: $local. Use the corresponding skill tool when it is relevant to accomplishing the user's request."
    );
}

#[test]
fn supported_size_repeated_skills_scan_utf16_positions_in_one_pass() {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            MENTION_SCAN_BUDGET.with(|budget| budget.set(None));
        }
    }
    let _reset = Reset;
    let text = "$s ".repeat(65_000);
    MENTION_SCAN_BUDGET.with(|budget| budget.set(Some((0, text.len() as u64 * 2))));
    let tokens = skill_occurrences(&text);
    assert_eq!(
        tokens.len(),
        65_000,
        "every occurrence remains available for source-local exclusions"
    );
    let last = tokens
        .last()
        .expect("the supported-size draft contains skill tokens");
    assert_eq!((last.mention.start, last.mention.end), (194_997, 194_999));
    assert_eq!(last.mention.text, "$s");
    MENTION_SCAN_BUDGET.with(|budget| {
        assert_eq!(
            budget.get().map(|(used, _)| used),
            Some(text.len() as u64),
            "the positive work counter covers the complete actual scan without prefix recounts"
        )
    });
}

#[test]
fn supported_size_file_mentions_scan_long_prefix_and_all_occurrences_once() {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            MENTION_SCAN_BUDGET.with(|budget| budget.set(None));
        }
    }
    let _reset = Reset;
    let mut text = "x".repeat(170_000);
    text.push(' ');
    text.push_str(&"@a.txt ".repeat(4096));
    assert!(
        text.len() < 200_000,
        "the measured long-prefix scenario stays within supported input size"
    );
    let source_units = text.encode_utf16().count() as u64;
    MENTION_SCAN_BUDGET.with(|budget| budget.set(Some((0, source_units * 2))));
    let mentions = file_mentions(&text, &[]);
    assert_eq!(
        mentions.len(),
        4096,
        "every occurrence remains available for source-local exclusions"
    );
    assert_eq!(mentions[0].mention, anchor(&text[..170_001], "@a.txt"));
    let last = mentions
        .last()
        .expect("the measured input contains file mentions");
    assert_eq!(last.name, "a.txt");
    assert_eq!(
        (last.mention.start, last.mention.end),
        (170_001 + 4095 * 7, 170_001 + 4095 * 7 + 6)
    );
    assert_eq!(last.mention.text, "@a.txt");
    MENTION_SCAN_BUDGET.with(|budget| assert_eq!(budget.get().map(|(used, _)| used), Some(source_units),
        "the positive counter includes the long prefix, every token and the final prose suffix exactly once"));
}

#[test]
fn linear_file_scan_keeps_unicode_boundaries_punctuation_catalog_precedence_and_token_order() {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            MENTION_SCAN_BUDGET.with(|budget| budget.set(None));
        }
    }
    let _reset = Reset;
    let text = "😀 [@`src/é🦀.rs`],\u{feff}(@NOTES.PDF) @Review.Agent @hidden.agent email@no.txt pkg/@scope/name @import</style> @unknown @... @inside/a@b.rs @x.rs\u{0085}$literal @end.png prose😀";
    let mut hidden = agent("hidden.agent", Some(AgentMode::Subagent));
    hidden.hidden = true;
    let agents = [agent("review.agent", Some(AgentMode::Subagent)), hidden];
    let source_units = text.encode_utf16().count() as u64;
    MENTION_SCAN_BUDGET.with(|budget| budget.set(Some((0, source_units * 2))));
    let mentions = file_mentions(text, &agents);
    let expected = [
        ("src/é🦀.rs", "@`src/é🦀.rs"),
        ("NOTES.PDF", "@NOTES.PDF"),
        ("hidden.agent", "@hidden.agent"),
        ("inside/a@b.rs", "@inside/a@b.rs"),
        ("x.rs\u{0085}$literal", "@x.rs\u{0085}$literal"),
        ("end.png", "@end.png"),
    ];
    assert_eq!(mentions.len(), expected.len());
    for (file, (name, token)) in mentions.iter().zip(expected) {
        let start = text
            .find(token)
            .expect("the expected token occurs in the original source");
        assert_eq!(file.name, name);
        assert_eq!(
            file.mention,
            anchor(&text[..start], token),
            "UTF-16 offsets select the exact original token"
        );
    }
    MENTION_SCAN_BUDGET.with(|budget| assert_eq!(budget.get().map(|(used, _)| used), Some(source_units),
        "accepted and rejected tokens, Unicode prefixes and trailing prose all contribute measured work"));
}

#[test]
fn missing_instruction_identity_and_unresolved_byte_requests_preserve_original_source() {
    let original = source("$local");
    let bytes = original.bytes().to_vec();
    let skills = vec![ComposerSkill {
        name: "local".to_owned(),
        id: None,
    }];
    assert!(matches!(
        process_mentions(&original, &[], &skills, None),
        Err(ComposerPreparationError::SkillContextIdentityRequired)
    ));
    assert!(matches!(
        process_mentions(
            &original,
            &[],
            &skills,
            Some(MessageId::new("msg_original_context"))
        ),
        Err(ComposerPreparationError::SkillContextIdentityRequired)
    ));
    assert_eq!(original.bytes(), bytes);
    let file = source("please read @new/file.docx");
    let bytes = file.bytes().to_vec();
    assert!(matches!(
        process_mentions(&file, &[], &[], None),
        Err(ComposerPreparationError::FilePreparationUnavailable)
    ));
    assert_eq!(file.bytes(), bytes);
}

#[test]
fn reference_only_mentions_use_strict_file_uris_and_upstream_basename_metadata() {
    let original = Arc::new(source(
        "\n😀 @docs/guide.PDF [@src/é#100%.rs] @c:\\repo\\file.txt\n",
    ));
    let bytes = original.bytes().to_vec();
    let plan = plan(Arc::clone(&original), &[], &[], None, None).expect("reference-only plan");
    assert!(
        Arc::ptr_eq(plan.source(), &original),
        "requests share the exact original capture"
    );
    assert_eq!(plan.owner(), &owner());
    assert!(
        plan.file_requests().is_empty(),
        "PDF and ordinary paths require no byte intake"
    );
    assert_eq!(
        plan.item.prompt_parts.file_references,
        vec![
            QueueFileReference {
                uri: "file:///repo/project/docs/guide.PDF".to_owned(),
                name: "guide.PDF".to_owned(),
                description: None,
                mention: Some(anchor("\n😀 ", "@docs/guide.PDF"))
            },
            QueueFileReference {
                uri: "file:///repo/project/src/%C3%A9%23100%25.rs".to_owned(),
                name: "é#100%.rs".to_owned(),
                description: None,
                mention: Some(anchor("\n😀 @docs/guide.PDF [", "@src/é#100%.rs"))
            },
            QueueFileReference {
                uri: "file:///C:/repo/file.txt".to_owned(),
                name: "file.txt".to_owned(),
                description: None,
                mention: Some(anchor(
                    "\n😀 @docs/guide.PDF [@src/é#100%.rs] ",
                    "@c:\\repo\\file.txt"
                ))
            },
        ]
    );
    assert_eq!(
        plan.item.content,
        original.item().content,
        "mention text stays literal"
    );
    assert_eq!(
        plan.item.files,
        original.item().files,
        "reference-only planning creates no byte authority"
    );
    assert_eq!(original.bytes(), bytes);
}

#[test]
fn absent_file_reference_arrays_remain_empty_through_plain_edit_and_local_recapture() {
    assert!(QueuePromptParts::default().file_references.is_empty());
    let original = source("plain edited source");
    let value: serde_json::Value =
        serde_json::from_slice(original.bytes()).expect("local source JSON");
    assert!(
        value["item"]["promptParts"].get("fileReferences").is_none(),
        "legacy empty arrays stay omitted"
    );
    let restored = QueueLocalCapture::from_bytes(original.bytes().to_vec(), DIRECTORY, sha256)
        .expect("omitted optional references");
    assert!(restored.item().prompt_parts.file_references.is_empty());
    let (item, _) = process_mentions(&restored, &[], &[], None).expect("plain edit");
    assert!(item.prompt_parts.file_references.is_empty());
}

#[test]
fn path_resolution_uses_captured_directory_and_rejects_ambiguous_or_traversing_paths() {
    for (directory, typed, expected) in [
        (DIRECTORY, "./src/a.rs", "/repo/project/src/a.rs"),
        (DIRECTORY, "src\\a.rs", "/repo/project/src/a.rs"),
        (DIRECTORY, "/outside/a.rs", "/outside/a.rs"),
        ("c:\\repo", "src/a.rs", "C:/repo/src/a.rs"),
        (DIRECTORY, "d:\\other\\a.rs", "D:/other/a.rs"),
        (DIRECTORY, "\\\\host\\share\\a.rs", "//host/share/a.rs"),
        ("/", "a.rs", "/a.rs"),
    ] {
        assert_eq!(
            resolve_file_path(directory, typed).expect("strict resolved path"),
            expected
        );
    }
    for typed in [
        "./",
        "../a.rs",
        "src/../a.rs",
        "src/./a.rs",
        "a//b.rs",
        "C:relative.rs",
        "https://host/a.rs",
        "file:///a.rs",
        "\\rooted.rs",
        "//?/C:/a.rs",
        "///a.rs",
        "/a\\b.rs",
        "a\u{0000}.rs",
    ] {
        assert!(
            matches!(
                resolve_file_path(DIRECTORY, typed),
                Err(ComposerPreparationError::InvalidFilePath)
            ),
            "invalid typed path {typed:?}"
        );
    }
}

#[test]
fn file_reference_exclusions_match_uri_and_occurrence_without_removing_equal_retained_entries() {
    let text = "@same.pdf first @same.pdf then @same.pdf";
    let uri = "file:///repo/project/same.pdf";
    let retained = QueueFileReference {
        uri: uri.to_owned(),
        name: "retained model name".to_owned(),
        description: Some("keep description".to_owned()),
        mention: Some(anchor("@same.pdf first ", "@same.pdf")),
    };
    let mut item = source(text).item().clone();
    item.prompt_parts.file_references = vec![retained.clone(), retained.clone()];
    let original = Arc::new(capture_item(item));
    let exclusions = ComposerSourceExclusions {
        source: 7,
        agents: Vec::new(),
        skills: Vec::new(),
        files: Vec::new(),
        file_references: vec![excluded(0, uri, anchor("", "@same.pdf"))],
    };
    validate_exclusions(7, Some(&exclusions)).expect("reference ordinal is source-local");
    let planned = plan(Arc::clone(&original), &[], &[], None, Some(&exclusions))
        .expect("exact reference removal");
    let references = &planned.item.prompt_parts.file_references;
    assert_eq!(
        references.len(),
        3,
        "the excluded first occurrence stays prose while the new third occurrence resolves"
    );
    assert_eq!(&references[..2], &[retained.clone(), retained]);
    assert_eq!(references[2].uri, uri);
    assert_eq!(
        references[2].mention,
        Some(anchor("@same.pdf first @same.pdf then ", "@same.pdf"))
    );
    let unrelated_uri = ComposerSourceExclusions {
        source: 7,
        agents: Vec::new(),
        skills: Vec::new(),
        files: Vec::new(),
        file_references: vec![excluded(
            0,
            "file:///other/same.pdf",
            anchor("", "@same.pdf"),
        )],
    };
    let planned = plan(original, &[], &[], None, Some(&unrelated_uri))
        .expect("same text with another URI is unrelated");
    assert_eq!(
        planned.item.prompt_parts.file_references.len(),
        4,
        "URI identity is required as well as a matching span"
    );
}

#[test]
fn file_reference_unknown_positions_and_duplicate_ordinals_fail_closed() {
    let original = Arc::new(source("prefix @same.pdf"));
    let exclusions = ComposerSourceExclusions {
        source: 1,
        agents: Vec::new(),
        skills: Vec::new(),
        files: Vec::new(),
        file_references: vec![ComposerExcludedReference {
            ordinal: 0,
            identity: "file:///repo/project/same.pdf".to_owned(),
            original_mention: Some(anchor("", "@same.pdf")),
            display_mention: None,
        }],
    };
    assert!(matches!(
        plan(Arc::clone(&original), &[], &[], None, Some(&exclusions)),
        Err(ComposerPreparationError::ExclusionPositionUnavailable)
    ));
    assert!(validate_exclusions(0, Some(&exclusions)).is_err());
    let duplicate = ComposerSourceExclusions {
        source: 1,
        agents: Vec::new(),
        skills: Vec::new(),
        files: Vec::new(),
        file_references: vec![
            excluded(0, "file:///repo/project/a.pdf", anchor("", "@a.pdf")),
            excluded(0, "file:///repo/project/b.pdf", anchor("@a.pdf ", "@b.pdf")),
        ],
    };
    assert!(validate_exclusions(1, Some(&duplicate)).is_err());
}

#[test]
fn byte_requests_classify_only_current_typed_document_and_image_paths() {
    let text = "😀 @notes.DOCX @slides.pptx @table.xlsx @doc.odt @slides.odp @table.ods @a.png @b.jpg @c.jpeg @d.gif @e.webp @f.heic @g.heif @guide.pdf @source.svg";
    let mut item = source(text).item().clone();
    item.files[0].name = "/repo/project/notes.DOCX".to_owned();
    item.files[0].mime_type = "image/png".to_owned();
    item.files[0].mention = Some(anchor("😀 ", "@notes.DOCX"));
    let original = Arc::new(capture_item(item));
    let planned = plan(Arc::clone(&original), &[], &[], None, None).expect("typed requests");
    assert!(Arc::ptr_eq(planned.source(), &original));
    let names = [
        "notes.DOCX",
        "slides.pptx",
        "table.xlsx",
        "doc.odt",
        "slides.odp",
        "table.ods",
        "a.png",
        "b.jpg",
        "c.jpeg",
        "d.gif",
        "e.webp",
        "f.heic",
        "g.heif",
    ];
    assert_eq!(planned.file_requests().len(), names.len());
    for (request, filename) in planned.file_requests().iter().zip(names) {
        assert_eq!(request.resolved_path(), format!("{DIRECTORY}/{filename}"));
        assert_eq!(
            utf16_slice(text, request.mention().start, request.mention().end),
            Some(request.mention().text.as_str())
        );
        assert_eq!(request.mention().text, format!("@{filename}"));
    }
    assert_eq!(
        planned.item.files,
        original.item().files,
        "held byte metadata neither resolves nor suppresses a new typed request"
    );
    assert_eq!(
        planned
            .item
            .prompt_parts
            .file_references
            .iter()
            .map(|file| file.name.as_str())
            .collect::<Vec<_>>(),
        vec!["guide.pdf", "source.svg"]
    );
    let plain = plan(
        Arc::new(source("ordinary prose without mentions")),
        &[],
        &[],
        None,
        None,
    )
    .expect("no typed file path");
    assert!(plain.file_requests().is_empty());
    assert!(plain.item.prompt_parts.file_references.is_empty());
}

#[test]
fn file_planning_keeps_agent_precedence_skills_and_captured_context_order() {
    let original = Arc::new(source("@review.agent $Known $local @new.png @guide.pdf"));
    let agents = [agent("review.agent", Some(AgentMode::Subagent))];
    let skills = [
        ComposerSkill {
            name: "Known".to_owned(),
            id: Some("registered-id".to_owned()),
        },
        ComposerSkill {
            name: "local".to_owned(),
            id: None,
        },
    ];
    let id = MessageId::new("msg_new_instruction");
    let planned = plan(
        Arc::clone(&original),
        &agents,
        &skills,
        Some(id.clone()),
        None,
    )
    .expect("combined plan");
    assert_eq!(planned.file_requests().len(), 1);
    assert_eq!(
        planned.file_requests()[0].resolved_path(),
        "/repo/project/new.png"
    );
    assert_eq!(planned.item.prompt_parts.agents[1].name, "review.agent");
    assert_eq!(planned.item.prompt_parts.skills[1].id, "registered-id");
    assert_eq!(
        planned.context_ids,
        vec![MessageId::new("msg_original_context"), id]
    );
    assert_eq!(
        planned.item.prompt_parts.context[0],
        original.item().prompt_parts.context[0]
    );
    assert_eq!(
        planned.item.prompt_parts.context[1].text,
        skill_instruction(&["local".to_owned()])
    );
}

#[test]
fn incomplete_reordered_or_misanchored_bundles_are_rejected_without_appending_files() {
    let original = Arc::new(source("@a.docx @b.png"));
    let planned = plan(Arc::clone(&original), &[], &[], None, None).expect("ordered requests");
    let a = output("a.docx", Some(planned.file_requests()[0].mention().clone()));
    let b = output("b.png", Some(planned.file_requests()[1].mention().clone()));
    for bundles in [
        Vec::new(),
        vec![vec![a.clone()]],
        vec![vec![a.clone()], Vec::new()],
        vec![vec![b.clone()], vec![a.clone()]],
        vec![vec![output("a.docx", None)], vec![b.clone()]],
        vec![vec![a.clone(), a.clone()], vec![b.clone()]],
        vec![vec![a.clone()], vec![b.clone()], vec![b.clone()]],
    ] {
        let mut item = planned.item.clone();
        let retained = item.files.clone();
        assert!(matches!(
            append_prepared_files(&mut item, planned.file_requests(), bundles),
            Err(ComposerPreparationError::PreparedFilesMismatch)
        ));
        assert_eq!(
            item.files, retained,
            "rejection preserves every independent byte file"
        );
    }
}

#[tokio::test]
async fn finishing_complete_bundles_preserves_source_and_rebases_first_output_and_references() {
    let original = Arc::new(source("\n\n😀 @a.docx then @guide.pdf\n"));
    let bytes = original.bytes().to_vec();
    let planned = plan(Arc::clone(&original), &[], &[], None, None).expect("source plan");
    let bundle = vec![
        output("a.docx", Some(planned.file_requests()[0].mention().clone())),
        output("image.png", None),
    ];
    let prepared = preparer()
        .finish_edit(planned, vec![bundle])
        .await
        .expect("complete caller-owned bundle");
    assert_eq!(prepared.owner(), &owner());
    let item = prepared.capture().item();
    assert_eq!(item.text, "😀 @a.docx then @guide.pdf");
    assert_eq!(item.content, original.item().content);
    assert_eq!(item.files.len(), 3);
    assert_eq!(item.files[0].sha256, original.item().files[0].sha256);
    assert!(
        item.files[0].mention.is_none(),
        "the original stale anchor clears"
    );
    assert_eq!(item.files[1].mention, Some(anchor("😀 ", "@a.docx")));
    assert!(
        item.files[2].mention.is_none(),
        "embedded document images have no source mention"
    );
    assert_eq!(
        item.prompt_parts.file_references[0].mention,
        Some(anchor("😀 @a.docx then ", "@guide.pdf"))
    );
    assert_eq!(
        item.prompt_parts.metadata,
        original.item().prompt_parts.metadata
    );
    assert_eq!(
        item.prompt_parts.context,
        original.item().prompt_parts.context
    );
    assert_eq!(prepared.capture().context_ids(), original.context_ids());
    assert_eq!(original.bytes(), bytes);
}

#[test]
fn snippet_expansion_rebases_original_and_new_files_and_clears_replaced_reference_anchors() {
    let prefix = "😀 [retained.txt] @new.png ";
    let token = "#snippet";
    let suffix = " @guide.pdf";
    let mut item = source(&format!("\n{prefix}{token}{suffix}\n"))
        .item()
        .clone();
    item.files[0].mention = Some(anchor("\n😀 ", "[retained.txt]"));
    item.prompt_parts.file_references.push(QueueFileReference {
        uri: "file:///repo/project/replaced.txt".to_owned(),
        name: "replaced.txt".to_owned(),
        description: Some("keep semantic reference".to_owned()),
        mention: Some(anchor(&format!("\n{prefix}"), token)),
    });
    let original = Arc::new(capture_item(item));
    let planned = plan(Arc::clone(&original), &[], &[], None, None).expect("snippet plan");
    let bundle = vec![output(
        "new.png",
        Some(planned.file_requests()[0].mention().clone()),
    )];
    let ComposerEditPlan {
        mut item,
        file_requests,
        ..
    } = planned;
    append_prepared_files(&mut item, &file_requests, vec![bundle]).expect("complete new file");
    rebase_trimmed_mentions(&mut item);
    let prefix_units = prefix.encode_utf16().count() as u64;
    let token_end = prefix_units + token.len() as u64;
    let input_end = token_end + suffix.len() as u64;
    let expanded = ComposerExpansion::from_parts(
        &item.text,
        format!("P\n{prefix}replacement{suffix}"),
        vec![
            ComposerSourceRange {
                input_start: 0,
                input_end: prefix_units,
                output_start: 2,
                output_end: 2 + prefix_units,
            },
            ComposerSourceRange {
                input_start: token_end,
                input_end,
                output_start: 2 + prefix_units + 11,
                output_end: 2 + prefix_units + 11 + suffix.len() as u64,
            },
        ],
    )
    .expect("actual mapped expansion");
    apply_expansion(&mut item, expanded);
    assert_eq!(
        item.files[0].mention,
        Some(anchor("P\n😀 ", "[retained.txt]"))
    );
    assert_eq!(
        item.files[1].mention,
        Some(anchor("P\n😀 [retained.txt] ", "@new.png"))
    );
    assert!(item.prompt_parts.file_references[0].mention.is_none());
    assert_eq!(
        item.prompt_parts.file_references[0].description.as_deref(),
        Some("keep semantic reference")
    );
    assert_eq!(
        item.prompt_parts.file_references[1].mention,
        Some(anchor(&format!("P\n{prefix}replacement "), "@guide.pdf"))
    );
}

#[test]
fn scanner_does_not_treat_emails_packages_html_or_bare_unknowns_as_files() {
    for text in [
        "someone@example.com",
        "pkg/@scope/name",
        "@unknown",
        "@import</style>",
        "@...",
    ] {
        assert!(!has_file_mention(text, &[]), "ordinary prose {text}");
    }
    for text in [
        "see [@src/a.rs]",
        "@./README",
        "(@notes.docx)",
        "@C:\\repo\\a.txt",
    ] {
        assert!(
            has_file_mention(text, &[]),
            "requires file preparation {text}"
        );
    }
    assert!(!has_file_mention(
        "@review.agent",
        &[agent("review.agent", Some(AgentMode::Subagent))]
    ));
}

#[test]
fn whitespace_and_skill_scanning_match_javascript_boundaries() {
    assert_eq!(
        skill_occurrences("$My_Skill $My_Skill\u{feff}¥next\u{0085}$no ($no)")
            .iter()
            .map(|token| token.name)
            .collect::<Vec<_>>(),
        vec!["My_Skill", "My_Skill", "next"]
    );
    let original = source("\n\u{feff}\t \n");
    assert!(
        process_mentions(&original, &[], &[], None)
            .expect("blank")
            .0
            .text
            .is_empty()
    );
    let original = source("\u{0085}");
    assert_eq!(
        process_mentions(&original, &[], &[], None)
            .expect("nonblank")
            .0
            .text,
        "\u{0085}"
    );
}

#[tokio::test]
async fn completed_edits_retain_owner_and_survive_exact_local_recapture() {
    use openchamber_native_core::draft::DraftTarget;
    use openchamber_native_core::ids::{RuntimeId, SessionId};
    use openchamber_native_transport::credential::Credential;
    use openchamber_native_transport::endpoint::Endpoint;

    let owner = DraftKey::new(
        RuntimeId::new("original-runtime"),
        DIRECTORY,
        DraftTarget::Session(SessionId::new("ses_original")),
    )
    .expect("owner");
    let client = ServerClient::new(
        Endpoint::parse("https://example.invalid/").expect("endpoint"),
        Credential::None,
    )
    .expect("client");
    let preparer = CapturedComposerPreparation::new(owner.clone(), &client, DIRECTORY)
        .expect("captured owner");
    let original = source("current edited display");
    let original = QueueLocalCapture::from_bytes(original.bytes().to_vec(), DIRECTORY, sha256)
        .expect("the verified source uses the transport's JavaScript-number semantics");
    let prepared = preparer
        .prepare_edit(&original, None)
        .await
        .expect("plain edits need no request");
    assert_eq!(prepared.owner(), &owner);
    assert_eq!(prepared.capture().item().text, "current edited display");
    let capture = prepared.into_capture();
    let restored = QueueLocalCapture::from_bytes(capture.bytes().to_vec(), DIRECTORY, sha256)
        .expect("immutable prepared source");
    assert_eq!(restored.bytes(), capture.bytes());
    assert_eq!(
        restored.item().prompt_parts.metadata,
        original.item().prompt_parts.metadata
    );
    assert_eq!(restored.context_ids(), original.context_ids());
    let other = DraftKey::new(
        RuntimeId::new("other-runtime"),
        "/other/project",
        DraftTarget::Session(SessionId::new("ses_other")),
    )
    .expect("other owner");
    let other = CapturedComposerPreparation::new(other, &client, "/other/project")
        .expect("other captured owner");
    assert!(matches!(
        other.prepare_edit(&original, None).await,
        Err(ComposerPreparationError::WrongOwner)
    ));
    let shared = Arc::new(source("plain shared source"));
    let planned = preparer
        .plan_edit(Arc::clone(&shared), None)
        .await
        .expect("plain captured-owner plan");
    assert!(Arc::ptr_eq(planned.source(), &shared));
    assert!(
        matches!(
            other.finish_edit(planned, Vec::new()).await,
            Err(ComposerPreparationError::WrongOwner)
        ),
        "finishing cannot move the plan to another captured owner"
    );
}

#[test]
fn captured_preparation_rejects_directory_outside_its_normalized_draft_owner() {
    use openchamber_native_transport::credential::Credential;
    use openchamber_native_transport::endpoint::Endpoint;

    let client = ServerClient::new(
        Endpoint::parse("https://example.invalid/").expect("inert endpoint"),
        Credential::None,
    )
    .expect("captured client");
    assert!(matches!(
        CapturedComposerPreparation::new(owner(), &client, "/other/project"),
        Err(ComposerPreparationError::WrongOwner)
    ));
}

#[tokio::test]
async fn captured_exact_directory_keeps_normalized_owner_and_refuses_alias_plans() {
    use openchamber_native_core::draft::DraftTarget;
    use openchamber_native_core::ids::{RuntimeId, SessionId};
    use openchamber_native_transport::credential::Credential;
    use openchamber_native_transport::endpoint::Endpoint;

    let client = ServerClient::new(
        Endpoint::parse("https://example.invalid/").expect("inert endpoint"),
        Credential::None,
    )
    .expect("captured client");
    for directory in ["/project/", r"C:\project"] {
        let owner = DraftKey::new(
            RuntimeId::from("captured-runtime"),
            directory,
            DraftTarget::Session(SessionId::from("ses_captured")),
        )
        .expect("normalized draft identity");
        assert_ne!(owner.directory(), directory);
        let captured = CapturedComposerPreparation::new(owner.clone(), &client, directory)
            .expect("exact captured server scope");
        let normalized =
            CapturedComposerPreparation::new(owner.clone(), &client, owner.directory())
                .expect("independent alias scope");
        let source = Arc::new(
            QueueLocalCapture::new(
                directory,
                QueueLocalItem {
                    content: "plain source".into(),
                    text: "plain source".into(),
                    files: Vec::new(),
                    send_config: QueuedSendConfig {
                        provider_id: "p".into(),
                        model_id: "m".into(),
                        agent: None,
                        variant: None,
                    },
                    prompt_parts: QueuePromptParts::default(),
                },
                Vec::new(),
                sha256,
            )
            .expect("source retains exact transport spelling"),
        );
        let planned = captured
            .plan_edit(Arc::clone(&source), None)
            .await
            .expect("matching exact scope needs no requests");
        assert_eq!(planned.owner(), &owner);
        assert_eq!(planned.source().directory(), directory);
        assert!(
            matches!(
                normalized.prepare_edit(&source, None).await,
                Err(ComposerPreparationError::WrongOwner)
            ),
            "matching normalized ownership cannot substitute another exact source directory"
        );
        assert!(
            matches!(
                normalized.finish_edit(planned, Vec::new()).await,
                Err(ComposerPreparationError::WrongOwner)
            ),
            "a plan cannot finish through an alias scope with the same normalized owner"
        );
        let prepared = captured
            .prepare_edit(&source, None)
            .await
            .expect("captured exact directory survives reparse and finish");
        assert_eq!(prepared.owner(), &owner);
        assert_eq!(prepared.capture().directory(), directory);
        assert_eq!(prepared.capture().item().text, "plain source");
    }
}
