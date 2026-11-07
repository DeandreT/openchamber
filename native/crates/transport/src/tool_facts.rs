//! Reads a tool call's free-form input and metadata into the typed facts the
//! rows use (`inputSchema` and `metadataSchema` in `lib/opencode/tools.ts`).
//!
//! Every field is read on its own, so one malformed field never blanks the
//! rest of a row.

use openchamber_native_core::rendered_page::{PageAnswer, PageInput, PageResult};
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
        action: text(input, "action"),
        parameter_detail: parameters
            .and_then(parameter_detail)
            .or_else(|| text(input, "title")),
        page: read_page(input, parameters),
    }
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
