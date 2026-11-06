use super::*;

use crate::credential::Credential;
use crate::endpoint::Endpoint;

#[test]
fn expansion_uses_the_captured_endpoint_directory_and_exact_text_body() {
    let client = ServerClient::new(
        Endpoint::parse("https://example.invalid/owner/").expect("endpoint"),
        Credential::None,
    )
    .expect("client");
    let captured = ComposerPreparation::new(&client, "/repo/原 owner").expect("captured owner");
    let request = captured
        .expansion_request("use #deploy\n")
        .build()
        .expect("request");
    assert_eq!(request.method(), Method::POST);
    assert_eq!(request.url().path(), "/owner/api/config/snippets/expand");
    assert_eq!(
        request
            .url()
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>(),
        vec![("directory".to_owned(), "/repo/原 owner".to_owned())]
    );
    assert_eq!(
        request.headers()["x-opencode-directory"],
        "%2Frepo%2F%E5%8E%9F%20owner"
    );
    assert_eq!(request.headers()["x-opencode-directory-encoding"], "uri");
    assert_eq!(
        request.body().and_then(reqwest::Body::as_bytes),
        Some(br##"{"text":"use #deploy\n"}"##.as_slice())
    );
}

#[test]
fn malformed_and_partial_catalogs_never_become_empty_success() {
    assert!(serde_json::from_str::<DiscoveredSkills>(r#"{}"#).is_err());
    assert!(serde_json::from_str::<Expansion>(r#"{"text":null}"#).is_err());
    assert!(serde_json::from_str::<Expansion>(r#"{}"#).is_err());
    let discovered = serde_json::from_str(r#"{"skills":[{"name":"s","path":"/repo/s/SKILL.md"}]}"#)
        .expect("discovery");
    let config = serde_json::from_str(r#"[{"type":"document","info":{"policies":[{"action":"integration.use","effect":"wrong","resource":"skill:s"}]}}]"#).expect("wire config");
    assert!(available_skills(discovered, config, vec![]).is_err());
    let partial = serde_json::from_str(r#"{"skills":[],"openCodeSkillsUnavailable":true}"#)
        .expect("partial discovery");
    assert!(available_skills(partial, vec![], vec![]).is_err());
    let client = ServerClient::new(
        Endpoint::parse("https://example.invalid/").expect("endpoint"),
        Credential::None,
    )
    .expect("client");
    let captured = ComposerPreparation::new(&client, "/repo/owner").expect("owner");
    assert!(
        captured
            .check_location(&Location {
                directory: "/repo/other".to_owned()
            })
            .is_err()
    );
}

#[test]
fn skill_names_respect_external_flags_policy_order_and_real_attachment_ids() {
    let discovered = serde_json::from_str(
        r#"{"skills":[
        {"name":"deploy","path":"/repo/.claude/skills/old/SKILL.md"},
        {"name":"deploy","path":"/repo/.agents/skills/deploy-folder/SKILL.md"},
        {"name":"blocked","path":"/repo/.agents/skills/blocked/SKILL.md"},
        {"name":"local","path":"/repo/.opencode/skills/local.md"}],
        "externalSkills":{"claudeDisabled":false,"allDisabled":false}}"#,
    )
    .expect("discovery");
    let config = serde_json::from_str(r#"[
        {"type":"document","info":{"policies":[{"action":"integration.use","effect":"deny","resource":"skill:blocked"}]}},
        {"type":"document","info":{"experimental":{"policies":[{"action":"integration.use","effect":"allow","resource":"skill:*"}]}}}
    ]"#).expect("config");
    let listed = vec![ListedSkill {
        id: "deploy-folder".to_owned(),
        name: "deploy".to_owned(),
    }];
    let skills = available_skills(discovered, config, listed).expect("complete skills");
    assert_eq!(
        skills,
        vec![
            ComposerSkill {
                name: "deploy".to_owned(),
                id: Some("deploy-folder".to_owned())
            },
            ComposerSkill {
                name: "local".to_owned(),
                id: None
            },
        ]
    );
}

#[test]
fn wildcard_policy_matching_keeps_utf16_and_optional_trailing_space_star() {
    assert!(wildcard_matches("skill:deploy", "skill:*"));
    assert!(wildcard_matches("skill:deploy", "skill:deploy *"));
    assert!(wildcard_matches("skill:deploy extra", "skill:deploy *"));
    assert!(wildcard_matches("skill:😀", "skill:??"));
    assert!(!wildcard_matches("skill:😀", "skill:?"));
    assert!(!wildcard_matches("skill:deploy-more", "skill:deploy *"));
}

#[test]
fn snippet_detection_matches_the_existing_store_without_a_catalog_guess() {
    for text in ["#name", "issue#42", "#-name", "#_name"] {
        assert!(has_snippet_token(text));
    }
    for text in ["plain", "#", "# name", "#原"] {
        assert!(!has_snippet_token(text));
    }
}

fn range(
    input_start: u64,
    input_end: u64,
    output_start: u64,
    output_end: u64,
) -> ComposerSourceRange {
    ComposerSourceRange {
        input_start,
        input_end,
        output_start,
        output_end,
    }
}

#[test]
fn mapped_request_retains_captured_owner_and_opt_in_body() {
    let client = ServerClient::new(
        Endpoint::parse("https://example.invalid/original/").expect("endpoint"),
        Credential::None,
    )
    .expect("client");
    let captured = ComposerPreparation::new(&client, "/repo/原 owner").expect("captured owner");
    let request = captured
        .mapped_expansion_request("use #deploy")
        .build()
        .expect("mapped request");
    assert_eq!(request.method(), Method::POST);
    assert_eq!(request.url().path(), "/original/api/config/snippets/expand");
    assert_eq!(
        request
            .url()
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>(),
        vec![("directory".to_owned(), "/repo/原 owner".to_owned())]
    );
    assert_eq!(
        request.headers()["x-opencode-directory"],
        "%2Frepo%2F%E5%8E%9F%20owner"
    );
    assert_eq!(
        request.body().and_then(reqwest::Body::as_bytes),
        Some(br##"{"text":"use #deploy","sourceMap":true}"##.as_slice())
    );
}

#[test]
fn mapped_expansion_validates_real_utf16_ranges_and_merges_exact_adjacency() {
    let mapped = ComposerExpansion::from_parts(
        "😀 A #x B 😀",
        "P\n\n😀 A Z B 😀\n\nQ".to_owned(),
        vec![range(0, 5, 3, 8), range(7, 12, 9, 14)],
    )
    .expect("actual unchanged ranges");
    assert_eq!(
        mapped.source_ranges(),
        &[range(0, 5, 3, 8), range(7, 12, 9, 14)]
    );
    assert_eq!(mapped.text(), "P\n\n😀 A Z B 😀\n\nQ");
    let merged = ComposerExpansion::from_parts(
        "abc",
        "abc".to_owned(),
        vec![range(0, 1, 0, 1), range(1, 3, 1, 3)],
    )
    .expect("adjacent ranges");
    assert_eq!(merged.source_ranges(), &[range(0, 3, 0, 3)]);
}

#[test]
fn unsupported_malformed_or_false_position_evidence_is_an_error() {
    for body in [
        r#"{"text":"old server"}"#,
        r#"{"text":"x","sourceRanges":null}"#,
        r#"{"text":"x","sourceRanges":[{"inputStart":0.5,"inputEnd":1,"outputStart":0,"outputEnd":1}]}"#,
        r#"{"text":"x","sourceRanges":[{"inputStart":0,"inputEnd":1,"outputStart":0,"outputEnd":1,"file":"unauthorized"}]}"#,
    ] {
        assert!(
            serde_json::from_str::<MappedExpansion>(body).is_err(),
            "malformed map must not become identity"
        );
    }
    for ranges in [
        vec![range(0, 4, 0, 4)],
        vec![range(0, 2, 0, 1)],
        vec![range(1, 2, 1, 2), range(0, 1, 0, 1)],
        vec![range(0, 2, 0, 2), range(1, 3, 1, 3)],
        vec![range(0, 1, 2, 3), range(2, 3, 0, 1)],
        vec![range(0, MAX_SAFE_INTEGER + 1, 0, MAX_SAFE_INTEGER + 1)],
    ] {
        assert!(ComposerExpansion::from_parts("abc", "abc".to_owned(), ranges).is_err());
    }
    assert!(
        ComposerExpansion::from_parts("abc", "abd".to_owned(), vec![range(0, 3, 0, 3)]).is_err()
    );
    assert!(ComposerExpansion::from_parts("😀", "😀".to_owned(), vec![range(0, 1, 0, 1)]).is_err());
    assert!(ComposerExpansion::from_parts("a", "😀a".to_owned(), vec![range(0, 1, 1, 2)]).is_err());
    assert!(
        ComposerExpansion::from_parts(
            "a",
            "a".to_owned(),
            vec![range(0, 1, 0, 1); SOURCE_RANGES + 1]
        )
        .is_err()
    );
    let oversized = serde_json::json!({"text":"a", "sourceRanges": vec![serde_json::json!({"inputStart":0,"inputEnd":1,"outputStart":0,"outputEnd":1}); SOURCE_RANGES + 1]});
    assert!(serde_json::from_value::<MappedExpansion>(oversized).is_err());
}

#[test]
fn identical_result_text_does_not_authorize_unmapped_generated_positions() {
    let mapped = ComposerExpansion::from_parts(
        "a #self b",
        "a #self b".to_owned(),
        vec![range(0, 2, 0, 2), range(7, 9, 7, 9)],
    )
    .expect("same spelling with actual replacement");
    assert_eq!(
        mapped.source_ranges(),
        &[range(0, 2, 0, 2), range(7, 9, 7, 9)]
    );
}
