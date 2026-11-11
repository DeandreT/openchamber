//! Tool rows through the rendered window: the open actions and their
//! failure, and the bodies drawn from a result's own shape.

use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, AppContext, ElementId, Entity, SharedString, TestAppContext};
use openchamber_native_core::ids::{MessageId, PartId, SessionId};
use openchamber_native_core::tool_shells::diagnostics::{FileDiagnostics, ToolDiagnostic};
use openchamber_native_core::tool_shells::question::AskedQuestion;
use openchamber_native_core::tool_shells::{ToolFacts, ToolFileDiff, ToolInput, ToolMetadata};
use openchamber_native_core::transcript::{
    Message, MessageInfo, Part, PartContent, PartTime, Role, ToolStatus,
};

use crate::locale;
use crate::workspace::Workspace;
use crate::workspace_tests::open;

const MESSAGE: &str = "msg_tool_rows";

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

fn click(window: AnyWindowHandle, id: impl Into<String>, cx: &mut TestAppContext) {
    let id = name(id);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(id, cx);
        window.render_frame(cx);
    })
    .expect("window is open");
}

fn call(id: &str, tool: &str, status: ToolStatus, output: Option<&str>, facts: ToolFacts) -> Part {
    Part {
        id: PartId::from(id),
        content: PartContent::Tool {
            tool: tool.to_owned(),
            status,
            title: None,
            output: output.map(str::to_owned),
            elided: None,
            child: None,
            facts: Box::new(facts),
        },
        time: PartTime::default(),
    }
}

/// Selects a root session and gives it one assistant message of `parts`,
/// which are built from the session directory. Returns the session.
fn show_calls(
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    cx: &mut TestAppContext,
    parts: impl FnOnce(&str) -> Vec<Part>,
) -> SessionId {
    update(window, workspace, cx, |this, window, cx| {
        let record = this
            .sessions
            .active()
            .iter()
            .find(|record| record.parent_id.is_none() && record.directory.is_some())
            .expect("a root session with a directory");
        let session = record.id.clone();
        let directory = record.directory.clone().expect("checked above");
        this.select_session(session.clone(), window, cx);
        let open = this.open_sessions.get_mut(&session).expect("visited");
        let _ = open.transcript.push(Message {
            info: MessageInfo {
                id: MessageId::from(MESSAGE),
                role: Role::Assistant,
                created: 9_000_000_000_000,
                completed: Some(9_000_000_000_001),
                notice: None,
                assistant: None,
                compaction: None,
                synthetic: None,
            },
            parts: parts(&directory),
        });
        let count = open.transcript.len();
        open.list.reset(count);
        this.scroll_to_tail(cx);
        session
    })
}

fn toggle(
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    session: &SessionId,
    part: &str,
    cx: &mut TestAppContext,
) {
    update(window, workspace, cx, |this, _, cx| {
        this.toggle_tool_row(session, &MessageId::from(MESSAGE), &PartId::from(part), cx);
    });
}

fn expanded(
    workspace: &Entity<Workspace>,
    session: &SessionId,
    part: &str,
    cx: &mut TestAppContext,
) -> bool {
    cx.read(|cx| {
        workspace.read(cx).open_sessions[session]
            .parts
            .is_expanded(&PartId::from(part))
    })
}

/// Dismisses the "unavailable" prompt a failed open raised.
fn close_prompt(cx: &mut TestAppContext) {
    let close = cx.read(|cx| locale::strings(cx).get("dialog.common.actions.close"));
    cx.simulate_prompt_answer(close.as_ref());
    cx.run_until_parked();
}

fn edit_facts(directory: &str) -> ToolFacts {
    let path = format!("{directory}/src/a.rs");
    ToolFacts {
        input: ToolInput {
            path: Some(path.clone()),
            ..ToolInput::default()
        },
        metadata: ToolMetadata {
            files: vec![ToolFileDiff {
                file: path.clone(),
                patch: Some("@@ -1,2 +1,2 @@\n context\n-old\n+new".to_owned()),
                additions: Some(1),
                deletions: Some(1),
                ..ToolFileDiff::default()
            }],
            diagnostics: vec![FileDiagnostics {
                path,
                diagnostics: vec![
                    ToolDiagnostic {
                        message: "unresolved name `x`".to_owned(),
                        line: 2,
                        character: 5,
                    },
                    ToolDiagnostic {
                        message: "mismatched types".to_owned(),
                        line: 9,
                        character: 1,
                    },
                ],
            }],
            ..ToolMetadata::default()
        },
        ..ToolFacts::default()
    }
}

#[gpui_kit::test]
fn a_changed_file_opens_from_the_row_and_a_failed_open_changes_nothing(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let session = show_calls(window, &workspace, cx, |directory| {
        vec![
            call(
                "call_edit",
                "edit",
                ToolStatus::Completed,
                Some("Edit applied."),
                edit_facts(directory),
            ),
            call(
                "call_shell",
                "shell",
                ToolStatus::Completed,
                Some("ok"),
                ToolFacts::default(),
            ),
        ]
    });
    assert!(shown(window, "tool:call_edit:open-file", cx));
    assert!(
        !shown(window, "tool:call_shell:open-file", cx),
        "a call that changed no file has nothing to open"
    );
    assert!(!shown(window, "tool:call_edit:diagnostics", cx));
    let panel = cx.read(|cx| workspace.read(cx).panels.open);

    // The synthetic workspace has no server to read the file from.
    click(window, "tool:call_edit:open-file", cx);
    assert!(cx.has_pending_prompt(), "the failed open says so");
    assert!(
        !expanded(&workspace, &session, "call_edit", cx),
        "the open action does not fold or open its row"
    );
    assert_eq!(cx.read(|cx| workspace.read(cx).panels.open), panel);
    close_prompt(cx);

    toggle(window, &workspace, &session, "call_edit", cx);
    assert!(shown(window, "tool:call_edit:diagnostics", cx));
    assert!(shown(window, "tool:call_edit:open-file:0", cx));
    click(window, "tool:call_edit:open-file:0", cx);
    assert!(cx.has_pending_prompt());
    assert!(expanded(&workspace, &session, "call_edit", cx));
    assert_eq!(cx.read(|cx| workspace.read(cx).panels.open), panel);
    close_prompt(cx);
}

#[gpui_kit::test]
fn search_hits_draw_as_cards_and_an_unreadable_result_stays_text(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let found = "## [Example](https://example.org/)\nPublished: 2026-01-05T12:00:00.000Z\n\nAn example site.\n\n## [Second](https://second.example/page)\n\nMore text.";
    let session = show_calls(window, &workspace, cx, |_| {
        let searched = ToolFacts {
            metadata: ToolMetadata {
                provider: Some("exa".to_owned()),
                ..ToolMetadata::default()
            },
            ..ToolFacts::default()
        };
        vec![
            call(
                "call_failed",
                "websearch",
                ToolStatus::Completed,
                Some("Search failed: rate limited"),
                ToolFacts::default(),
            ),
            call(
                "call_denied",
                "websearch",
                ToolStatus::Completed,
                Some("## [Trap](https://user:pw@example.org/)\n\nText."),
                ToolFacts::default(),
            ),
            call(
                "call_found",
                "websearch",
                ToolStatus::Completed,
                Some(found),
                searched,
            ),
        ]
    });
    toggle(window, &workspace, &session, "call_failed", cx);
    toggle(window, &workspace, &session, "call_denied", cx);
    toggle(window, &workspace, &session, "call_found", cx);
    assert!(
        !shown(window, "tool:call_denied:search-results", cx)
            && !shown(window, "tool:call_denied:search-result:0", cx),
        "an address the client would refuse is never drawn as a link"
    );
    assert!(
        shown(window, "tool:call_denied:copy", cx),
        "it stays text the reader can copy"
    );

    assert!(shown(window, "tool:call_found:search-results", cx));
    assert!(shown(window, "tool:call_found:search-result:0", cx));
    assert!(shown(window, "tool:call_found:search-result:1", cx));
    assert!(
        !shown(window, "tool:call_found:copy", cx),
        "hits replace the text result"
    );
    assert!(
        !shown(window, "tool:call_failed:search-results", cx),
        "a result in another shape is not a list of hits"
    );
    assert!(
        shown(window, "tool:call_failed:copy", cx),
        "the unreadable result keeps its text, beside the readable one"
    );

    // Later frames draw the hits the first one parsed.
    for _ in 0..3 {
        assert!(shown(window, "tool:call_found:search-result:1", cx));
    }
    click(window, "tool:call_found:search-result:0", cx);
    cx.run_until_parked();
    assert_eq!(cx.opened_url().as_deref(), Some("https://example.org/"));
    assert!(
        expanded(&workspace, &session, "call_found", cx),
        "opening a hit leaves its row open"
    );
}

/// A question call's facts. `listed_input` is what the input preview would
/// print, so a row that hides the preview hides something real.
fn question_facts(readable: bool) -> ToolFacts {
    ToolFacts {
        input: ToolInput {
            questions: 1,
            asked: if readable {
                vec![AskedQuestion {
                    header: Some("Scope".to_owned()),
                    question: "Which **one**?".to_owned(),
                    options: vec!["First".to_owned(), "Second".to_owned()],
                }]
            } else {
                Vec::new()
            },
            ..ToolInput::default()
        },
        listed_input: Some("Questions: [\"Which one?\"]".to_owned()),
        ..ToolFacts::default()
    }
}

const ANSWERED: &str = "User has answered your questions: \"Which **one**?\"=\"First\". You can now continue with the user's answers in mind.";

#[gpui_kit::test]
fn a_question_lists_its_questions_once_until_its_answers_read(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let session = show_calls(window, &workspace, cx, |_| {
        let mut failed = question_facts(true);
        failed.error = Some("The question was dismissed".to_owned());
        vec![
            call(
                "call_waiting",
                "question",
                ToolStatus::Running,
                None,
                question_facts(true),
            ),
            call(
                "call_answered",
                "question",
                ToolStatus::Completed,
                Some(ANSWERED),
                question_facts(true),
            ),
            call(
                "call_other",
                "question",
                ToolStatus::Completed,
                Some("The user moved on."),
                question_facts(true),
            ),
            call("call_failed", "question", ToolStatus::Error, None, failed),
            call(
                "call_reported",
                "question",
                ToolStatus::Error,
                Some("Dismissed by the user"),
                question_facts(true),
            ),
        ]
    });
    let parts = [
        "call_waiting",
        "call_answered",
        "call_other",
        "call_failed",
        "call_reported",
    ];
    for part in parts {
        toggle(window, &workspace, &session, part, cx);
    }

    assert!(shown(window, "tool:call_waiting:questions", cx));
    assert!(!shown(window, "tool:call_waiting:answers", cx));
    assert!(
        !shown(window, "tool:call_waiting:awaiting", cx),
        "the questions themselves say what the row waits for"
    );
    assert!(shown(window, "tool:call_answered:answers", cx));
    assert!(!shown(window, "tool:call_answered:questions", cx));
    assert!(
        shown(window, "tool:call_other:questions", cx),
        "a result in another shape falls back to what was asked"
    );
    assert!(shown(window, "tool:call_failed:error", cx));
    assert!(
        !shown(window, "tool:call_failed:questions", cx)
            && !shown(window, "tool:call_failed:answers", cx),
        "a failed question shows its error"
    );
    assert!(
        shown(window, "tool:call_reported:error", cx)
            && !shown(window, "tool:call_reported:questions", cx),
        "a failure reported only in the result still shows as the error"
    );
    for part in parts {
        assert!(
            !shown(window, format!("tool:{part}:input"), cx),
            "{part}: the input preview would print the questions a second time"
        );
    }
}

#[gpui_kit::test]
fn a_question_without_readable_questions_keeps_its_input_and_result(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let session = show_calls(window, &workspace, cx, |_| {
        vec![
            call(
                "call_waiting",
                "question",
                ToolStatus::Running,
                None,
                question_facts(false),
            ),
            call(
                "call_done",
                "question",
                ToolStatus::Completed,
                Some("The user moved on."),
                question_facts(false),
            ),
            call(
                "call_answered",
                "question",
                ToolStatus::Completed,
                Some(ANSWERED),
                question_facts(false),
            ),
            call(
                "call_reported",
                "question",
                ToolStatus::Error,
                Some("Dismissed by the user"),
                question_facts(false),
            ),
            call(
                "call_failed",
                "question",
                ToolStatus::Error,
                None,
                question_facts(false),
            ),
            call(
                "call_silent",
                "question",
                ToolStatus::Completed,
                None,
                question_facts(false),
            ),
        ]
    });
    for part in [
        "call_waiting",
        "call_done",
        "call_answered",
        "call_reported",
        "call_failed",
        "call_silent",
    ] {
        toggle(window, &workspace, &session, part, cx);
    }

    assert!(
        shown(window, "tool:call_waiting:awaiting", cx),
        "only a call still running waits for a response"
    );
    assert!(
        shown(window, "tool:call_waiting:input", cx),
        "with no question to list, the input preview is all that says what was asked"
    );

    assert!(
        shown(window, "tool:call_done:copy", cx),
        "a finished call keeps its result, with Copy"
    );
    assert!(shown(window, "tool:call_done:input", cx));
    assert!(!shown(window, "tool:call_done:awaiting", cx));

    assert!(shown(window, "tool:call_answered:answers", cx));
    assert!(
        !shown(window, "tool:call_answered:input", cx),
        "parsed answers already name their questions"
    );

    for part in ["call_reported", "call_failed"] {
        assert!(
            shown(window, format!("tool:{part}:error"), cx),
            "{part}: a failed call shows the error box"
        );
        assert!(!shown(window, format!("tool:{part}:awaiting"), cx));
    }

    assert!(
        !shown(window, "tool:call_silent:awaiting", cx),
        "a finished call no longer waits"
    );
    assert!(!shown(window, "tool:call_silent:copy", cx));
}
