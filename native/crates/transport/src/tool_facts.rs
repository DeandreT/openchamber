//! Reads a tool call's free-form input and metadata into the typed facts the
//! rows use (`inputSchema` and `metadataSchema` in `lib/opencode/tools.ts`).
//!
//! Every field is read on its own, so one malformed field never blanks the
//! rest of a row.

use openchamber_native_core::rendered_page::{PageAnswer, PageInput, PageResult};
use openchamber_native_core::tool_shells::diagnostics::{FileDiagnostics, ToolDiagnostic};
use openchamber_native_core::tool_shells::question::AskedQuestion;
use openchamber_native_core::tool_shells::{
    ExecuteToolCall, ToolFacts, ToolFileDiff, ToolInput, ToolMetadata,
};
use serde_json::{Map, Value};

const MAX_EXECUTE_CALL_INPUT_LENGTH: usize = 160;

pub(crate) fn tool_facts(
    input: Option<&Value>,
    metadata: Option<&Value>,
    error: Option<String>,
) -> ToolFacts {
    ToolFacts {
        input: input.map(read_input).unwrap_or_default(),
        metadata: metadata.map(read_metadata).unwrap_or_default(),
        error,
        listed_input: input
            .and_then(Value::as_object)
            .map(list_input)
            .filter(|text| !text.trim().is_empty()),
    }
}

/// `optionalText`: a trimmed, non-empty string.
fn text(record: &Value, key: &str) -> Option<String> {
    record
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn read_input(input: &Value) -> ToolInput {
    let parameters = input.get("parameters").filter(|value| value.is_object());
    ToolInput {
        path: text(input, "path")
            .or_else(|| text(input, "filePath"))
            .or_else(|| text(input, "file_path")),
        command: text(input, "command"),
        prompt: text(input, "prompt"),
        description: text(input, "description"),
        agent: text(input, "agent"),
        name: text(input, "name"),
        id: text(input, "id"),
        title: text(input, "title"),
        directory: text(input, "directory"),
        search: text(input, "search"),
        provider: text(input, "provider"),
        patch_text: text(input, "patchText"),
        pattern: text(input, "pattern"),
        query: text(input, "query"),
        url: text(input, "url"),
        code: text(input, "code"),
        content: input
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_owned),
        questions: input
            .get("questions")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        asked: read_questions(input),
        action: text(input, "action"),
        parameter_detail: parameters
            .and_then(parameter_detail)
            .or_else(|| text(input, "title")),
        page: read_page(input, parameters),
    }
}

/// A `question` call's questions (`questionInput` in `ToolPart.tsx`). Each
/// field is read on its own: a question without options keeps its text, and
/// an option without a text label is left out.
fn read_questions(input: &Value) -> Vec<AskedQuestion> {
    let Some(questions) = input.get("questions").and_then(Value::as_array) else {
        return Vec::new();
    };
    questions
        .iter()
        .filter(|question| question.is_object())
        .map(|question| AskedQuestion {
            header: text(question, "header"),
            question: question
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            options: question
                .get("options")
                .and_then(Value::as_array)
                .map(|options| {
                    options
                        .iter()
                        .filter_map(|option| text(option, "label"))
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect()
}

/// `metadata.diagnostics`: LSP diagnostics by file path
/// (`normalizeToolDiagnostic`). Only errors are kept, severity 1 or none
/// given, at 1-based positions. A key keeps its place even when nothing
/// under it is an error, so a later key never stands in for it.
fn read_diagnostics(metadata: &Value) -> Vec<FileDiagnostics> {
    let Some(by_file) = metadata.get("diagnostics").and_then(Value::as_object) else {
        return Vec::new();
    };
    by_file
        .iter()
        .filter(|(_, listed)| !listed.is_null())
        .map(|(path, listed)| FileDiagnostics {
            path: path.clone(),
            diagnostics: listed
                .as_array()
                .map(|listed| listed.iter().filter_map(read_diagnostic).collect())
                .unwrap_or_default(),
        })
        .collect()
}

fn read_diagnostic(diagnostic: &Value) -> Option<ToolDiagnostic> {
    let message = text(diagnostic, "message")?;
    if let Some(severity) = diagnostic.get("severity").and_then(Value::as_f64)
        && severity.is_finite()
        && severity.trunc() != 1.0
    {
        return None;
    }
    let start = diagnostic.get("range").and_then(|range| range.get("start"));
    // Zero-based on the wire; a missing or negative position reads as zero.
    let position = |key: &str| -> u32 {
        let raw = start
            .and_then(|start| start.get(key))
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
            .map_or(0.0, |value| value.trunc().max(0.0));
        // `as` saturates, and the row's position is 1-based.
        (raw as u32).saturating_add(1)
    };
    Some(ToolDiagnostic {
        message,
        line: position("line"),
        character: position("character"),
    })
}

/// A render call's page fields, `parameters` first and field by field
/// (`renderPageInputSchema`).
fn read_page(input: &Value, parameters: Option<&Value>) -> PageInput {
    let raw = |key: &str| -> Option<String> {
        parameters
            .and_then(|parameters| parameters.get(key)?.as_str())
            .or_else(|| input.get(key)?.as_str())
            .map(str::to_owned)
    };
    // `booleanish`: a boolean, or the strings "true" and "false".
    let flag = |record: &Value| -> Option<bool> {
        match record.get("awaitAnswer")? {
            Value::Bool(value) => Some(*value),
            Value::String(value) if value == "true" => Some(true),
            Value::String(value) if value == "false" => Some(false),
            _ => None,
        }
    };
    PageInput {
        html: raw("html"),
        ui: raw("ui"),
        title: parameters
            .and_then(|parameters| text(parameters, "title"))
            .or_else(|| text(input, "title")),
        await_answer: parameters.and_then(flag).or_else(|| flag(input)),
    }
}

/// The `openchamber` result of a render call (`openChamberResultSchema`).
fn read_page_result(metadata: &Value) -> PageResult {
    let Some(result) = metadata
        .get("openchamber")
        .filter(|value| value.is_object())
    else {
        return PageResult::default();
    };
    let answer =
        result
            .get("answer")
            .and_then(|answer| match answer.get("answered")?.as_bool()? {
                true => Some(PageAnswer::Answered {
                    summary: answer.get("summary")?.as_str()?.to_owned(),
                    data: answer
                        .get("data")
                        .map_or_else(|| "null".to_owned(), Value::to_string),
                }),
                false => Some(PageAnswer::NotAnswered {
                    reason: answer
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                }),
            });
    PageResult {
        ok: result.get("ok").and_then(Value::as_bool),
        answer,
    }
}

/// The one detail of an OpenChamber tool's parameters that tells calls
/// apart (`describeOpenChamberTool`).
fn parameter_detail(parameters: &Value) -> Option<String> {
    let linked = parameters.get("link").map(|link| {
        [text(link, "identifier"), text(link, "title")]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ")
    });
    linked
        .filter(|linked| !linked.is_empty())
        .or_else(|| {
            ["title", "name", "path", "url", "query", "text", "selector"]
                .into_iter()
                .find_map(|key| text(parameters, key))
        })
        .or_else(|| {
            text(parameters, "prompt")
                .map(|prompt| prompt.split('\n').next().unwrap_or_default().to_owned())
        })
        .or_else(|| text(parameters, "taskId"))
}

fn read_metadata(metadata: &Value) -> ToolMetadata {
    let count = |record: &Value, key: &str| record.get(key).and_then(Value::as_u64);
    let files = metadata
        .get("files")
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(|entry| {
                    Some(ToolFileDiff {
                        file: text(entry, "file")?,
                        patch: text(entry, "patch"),
                        additions: count(entry, "additions"),
                        deletions: count(entry, "deletions"),
                        deleted: entry.get("status").and_then(Value::as_str) == Some("deleted")
                            || entry.get("type").and_then(Value::as_str) == Some("delete"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let patch_text = |value: Option<&Value>| -> Option<String> {
        let value = value?;
        let patch = value.as_str().or_else(|| value.get("patch")?.as_str())?;
        let patch = patch.trim();
        (!patch.is_empty()).then(|| patch.to_owned())
    };
    let tool_calls = metadata
        .get("toolCalls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(|call| {
                    let input = call
                        .get("input")
                        .filter(|input| !input.is_null())
                        .and_then(|input| serde_json::to_string(input).ok())
                        .filter(|json| json != "{}")
                        .map(|json| {
                            if json.chars().count() > MAX_EXECUTE_CALL_INPUT_LENGTH {
                                let cut: String =
                                    json.chars().take(MAX_EXECUTE_CALL_INPUT_LENGTH).collect();
                                format!("{cut}\u{2026}")
                            } else {
                                json
                            }
                        });
                    Some(ExecuteToolCall {
                        tool: text(call, "tool")?,
                        status: text(call, "status"),
                        input,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    ToolMetadata {
        files,
        patch: patch_text(metadata.get("patch")).or_else(|| patch_text(metadata.get("diff"))),
        name: text(metadata, "name"),
        shell_id: text(metadata, "shellID"),
        tool_calls,
        truncated: metadata.get("truncated").and_then(Value::as_bool) == Some(true),
        output_path: text(metadata, "outputPath"),
        diagnostics: read_diagnostics(metadata),
        provider: metadata
            .get("provider")
            .and_then(Value::as_str)
            .filter(|provider| !provider.is_empty())
            .map(str::to_owned),
        page_result: read_page_result(metadata),
    }
}

/// Every field of the input, one per line: what `formatToolInput` prints
/// for a tool without a field of its own (`tool_shells::input_text`).
///
/// Fields print in the input's own order where `serde_json` keeps it (its
/// `preserve_order` feature, which the desktop build enables), as the web
/// prints them; a build without it sorts them by key.
fn list_input(input: &Map<String, Value>) -> String {
    let lines: Vec<String> = input
        .iter()
        .filter(|(_, value)| !matches!(value, Value::Null) && value.as_str() != Some(""))
        .map(|(key, value)| {
            let value = match value {
                Value::String(text) => text.clone(),
                Value::Bool(true) => "Yes".to_owned(),
                Value::Bool(false) => "No".to_owned(),
                Value::Number(number) => number.to_string(),
                other => serde_json::to_string_pretty(other).unwrap_or_default(),
            };
            format!("{}: {value}", field_label(key))
        })
        .collect();
    lines.join("\n")
}

/// `someKey_name` → `Some key name`.
fn field_label(key: &str) -> String {
    let mut spaced = String::with_capacity(key.len() + 4);
    for character in key.chars() {
        if character.is_ascii_uppercase() {
            spaced.push(' ');
        }
        spaced.push(if character == '_' { ' ' } else { character });
    }
    let lowered = spaced.to_lowercase();
    let mut characters = lowered.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => lowered,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_each_field_on_its_own() {
        let facts = tool_facts(
            Some(&json!({"filePath": " /a.rs ", "pattern": 3, "questions": [1, 2]})),
            Some(&json!({
                "files": [{"file": "a.rs", "additions": 2, "deletions": 1}, {"additions": 9}, null],
                "shellID": "shl_1",
                "toolCalls": [{"tool": "x", "input": {"q": "y"}}, {"status": "running"}],
                "truncated": true,
            })),
            None,
        );
        assert_eq!(facts.input.path.as_deref(), Some("/a.rs"));
        assert_eq!(facts.input.pattern, None, "a number is not text");
        assert_eq!(facts.input.questions, 2);
        assert_eq!(facts.metadata.files.len(), 1);
        assert_eq!(facts.metadata.files[0].additions, Some(2));
        assert_eq!(facts.metadata.shell_id.as_deref(), Some("shl_1"));
        assert_eq!(facts.metadata.tool_calls.len(), 1);
        assert_eq!(
            facts.metadata.tool_calls[0].input.as_deref(),
            Some(r#"{"q":"y"}"#)
        );
        assert!(facts.metadata.truncated);
    }

    #[test]
    fn questions_keep_what_reads_and_drop_what_does_not() {
        let input = json!({"questions": [
            {"header": " Scope ", "question": "Which **one**?", "options": [
                {"label": "First", "description": "d"}, {"label": 2}, "bare", {"label": "Second"}
            ]},
            {"question": 7, "options": "none"},
            "not a record",
        ]});
        let facts = tool_facts(Some(&input), None, None);
        assert_eq!(facts.input.questions, 3, "the row still counts every entry");
        assert_eq!(
            facts.input.asked,
            [
                AskedQuestion {
                    header: Some("Scope".to_owned()),
                    question: "Which **one**?".to_owned(),
                    options: vec!["First".to_owned(), "Second".to_owned()],
                },
                AskedQuestion::default(),
            ]
        );
        assert!(
            tool_facts(Some(&json!({"questions": "x"})), None, None)
                .input
                .asked
                .is_empty()
        );
    }

    #[test]
    fn diagnostics_keep_errors_at_one_based_positions() {
        let metadata = json!({
            "provider": "exa",
            "files": [{"file": "gone.rs", "status": "deleted"}, {"file": "old.rs", "type": "delete"}, {"file": "a.rs"}],
            "diagnostics": {
                "/repo/a.rs": [
                    {"message": " unresolved name ", "severity": 1, "range": {"start": {"line": 4, "character": 2}}},
                    {"message": "unused", "severity": 2, "range": {"start": {"line": 9, "character": 0}}},
                    {"message": "no severity or range"},
                    {"message": "   ", "severity": 1},
                    {"message": "odd position", "range": {"start": {"line": -3, "character": "x"}}},
                    "not a record",
                ],
                "/repo/warnings.rs": [{"message": "unused", "severity": 2}],
                "/repo/broken.rs": "not a list",
                "/repo/null.rs": null,
            },
        });
        let facts = tool_facts(None, Some(&metadata), None);
        assert_eq!(facts.metadata.provider.as_deref(), Some("exa"));
        let deleted: Vec<bool> = facts
            .metadata
            .files
            .iter()
            .map(|file| file.deleted)
            .collect();
        assert_eq!(deleted, [true, true, false]);
        let listed = &facts.metadata.diagnostics;
        let entry = |path: &str| {
            listed
                .iter()
                .find(|entry| entry.path == path)
                .map(|entry| entry.diagnostics.clone())
        };
        assert_eq!(
            entry("/repo/a.rs").expect("the file is listed"),
            [
                ToolDiagnostic {
                    message: "unresolved name".to_owned(),
                    line: 5,
                    character: 3,
                },
                ToolDiagnostic {
                    message: "no severity or range".to_owned(),
                    line: 1,
                    character: 1,
                },
                ToolDiagnostic {
                    message: "odd position".to_owned(),
                    line: 1,
                    character: 1,
                },
            ]
        );
        assert_eq!(entry("/repo/warnings.rs"), Some(Vec::new()));
        assert_eq!(
            entry("/repo/broken.rs"),
            Some(Vec::new()),
            "one malformed file does not drop the others"
        );
        assert_eq!(entry("/repo/null.rs"), None);
        let none = tool_facts(None, Some(&json!({"diagnostics": []})), None);
        assert!(none.metadata.diagnostics.is_empty());
        assert_eq!(none.metadata.provider, None);
    }

    #[test]
    fn lists_every_field_for_tools_without_one_of_their_own() {
        let generic = json!({"maxResults": 5, "fileName": "a", "recursive": true, "empty": ""});
        let listed = tool_facts(Some(&generic), None, None)
            .listed_input
            .expect("fields to list");
        let mut lines: Vec<&str> = listed.lines().collect();
        lines.sort_unstable();
        assert_eq!(lines, ["File name: a", "Max results: 5", "Recursive: Yes"]);
    }

    #[test]
    fn render_calls_read_their_page_and_answer() {
        let input = json!({"action": "render.page", "html": "<p>outer</p>", "parameters": {"html": "<p>inner</p>", "awaitAnswer": "true"}});
        let metadata = json!({"openchamber": {"ok": true, "answer": {"answered": true, "summary": "A", "data": {"pick": 1}}}});
        let facts = tool_facts(Some(&input), Some(&metadata), None);
        assert_eq!(facts.input.page.html.as_deref(), Some("<p>inner</p>"));
        assert_eq!(facts.input.page.await_answer, Some(true));
        assert_eq!(facts.metadata.page_result.ok, Some(true));
        assert_eq!(
            facts.metadata.page_result.answer,
            Some(PageAnswer::Answered {
                summary: "A".to_owned(),
                data: "{\"pick\":1}".to_owned()
            })
        );
        let bare = tool_facts(
            Some(&json!({"html": "<b>x</b>", "awaitAnswer": 1})),
            None,
            None,
        );
        assert_eq!(bare.input.page.html.as_deref(), Some("<b>x</b>"));
        assert_eq!(bare.input.page.await_answer, None);
    }

    #[test]
    fn openchamber_tools_name_their_action_and_detail() {
        let input = json!({"action": "session.link", "parameters": {"link": {"identifier": "!42", "title": "Fix"}}});
        let facts = tool_facts(Some(&input), None, None);
        assert_eq!(facts.input.action.as_deref(), Some("session.link"));
        assert_eq!(facts.input.parameter_detail.as_deref(), Some("!42 Fix"));
    }
}
