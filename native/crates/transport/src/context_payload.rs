//! Reads the `openchamberContext` metadata of an attached context item
//! (`readContextPart` in `packages/ui/src/lib/messages/contextParts.ts`),
//! with OpenCode Desktop's `opencodeComment` as the compatible fallback.
//! A payload that fails its schema counts as no context.

use openchamber_native_core::session_markdown::ContextPayload;
use serde_json::{Map, Value};

/// The context payload of a synthetic message's metadata.
pub(crate) fn read(metadata: Option<&Value>) -> Option<ContextPayload> {
    let metadata = metadata?.as_object()?;
    metadata
        .get("openchamberContext")
        .and_then(Value::as_object)
        .and_then(canonical)
        .or_else(|| {
            metadata
                .get("opencodeComment")
                .and_then(Value::as_object)
                .and_then(opencode_comment)
        })
}

fn text(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key)?.as_str().map(str::to_owned)
}

fn nonempty(object: &Map<String, Value>, key: &str) -> Option<String> {
    text(object, key).filter(|value| !value.is_empty())
}

/// A line number; JSON numbers with a fraction are not lines.
fn line(object: &Map<String, Value>, key: &str) -> Option<i64> {
    let number = object.get(key)?.as_f64()?;
    (number.is_finite() && number.fract() == 0.0).then_some(number as i64)
}

fn optional_line(object: &Map<String, Value>, key: &str) -> Option<Option<i64>> {
    match object.get(key) {
        None => Some(None),
        Some(_) => line(object, key).map(Some),
    }
}

fn positive_integer(object: &Map<String, Value>, key: &str) -> Option<i64> {
    line(object, key).filter(|number| *number > 0)
}

/// An optional source-control provider identifier.
fn provider(object: &Map<String, Value>) -> Option<Option<String>> {
    match object.get("provider") {
        None => Some(None),
        Some(Value::String(id)) if !id.is_empty() => Some(Some(id.clone())),
        Some(_) => None,
    }
}

fn canonical(object: &Map<String, Value>) -> Option<ContextPayload> {
    let reference = |required: &[&str]| -> Option<ContextPayload> {
        required
            .iter()
            .all(|key| object.get(*key).is_some_and(Value::is_string))
            .then_some(ContextPayload::Reference)
    };
    match object.get("kind")?.as_str()? {
        "code-comment" => {
            let source = text(object, "source")?;
            if !matches!(source.as_str(), "diff" | "file" | "plan") {
                return None;
            }
            let side = match object.get("side") {
                None => None,
                Some(Value::String(side)) if side == "original" || side == "modified" => {
                    Some(side.clone())
                }
                Some(_) => return None,
            };
            Some(ContextPayload::CodeComment {
                file_label: text(object, "fileLabel")?,
                start_line: line(object, "startLine")?,
                end_line: line(object, "endLine")?,
                side,
                language: text(object, "language")?,
                code: text(object, "code")?,
                text: text(object, "text")?,
            })
        }
        "terminal" => {
            text(object, "terminalId")?;
            Some(ContextPayload::Terminal {
                terminal_label: text(object, "terminalLabel")?,
                start_line: line(object, "startLine")?,
                end_line: line(object, "endLine")?,
                output: text(object, "output")?,
            })
        }
        "browser-annotation" => Some(ContextPayload::BrowserAnnotation {
            page_url: text(object, "pageUrl")?,
            prompt: text(object, "prompt")?,
            text: text(object, "text")?,
        }),
        "pr-comment" => Some(ContextPayload::PrComment {
            provider: provider(object)?,
            label: text(object, "label")?,
            body: text(object, "body")?,
            text: text(object, "text")?,
        }),
        "pr-check" => Some(ContextPayload::PrCheck {
            provider: provider(object)?,
            label: text(object, "label")?,
            output: text(object, "output")?,
            text: text(object, "text")?,
        }),
        "file-quote" => Some(ContextPayload::FileQuote {
            file_label: text(object, "fileLabel")?,
            start_line: optional_line(object, "startLine")?,
            end_line: optional_line(object, "endLine")?,
            quote: text(object, "quote")?,
            text: text(object, "text")?,
        }),
        "chat-quote" => Some(ContextPayload::ChatQuote {
            quote: text(object, "quote")?,
            text: text(object, "text")?,
        }),
        "repository-issue" | "change-request" | "github-issue" | "github-pr" => {
            positive_integer(object, "number")?;
            provider(object)?;
            reference(&["title", "url"])
        }
        "repository-discussion" => {
            positive_integer(object, "number")?;
            provider(object)??;
            reference(&["title", "url"])
        }
        "tracker-item" => {
            let provider = text(object, "provider")?;
            let valid = provider.len() <= 32
                && provider.starts_with(|ch: char| ch.is_ascii_lowercase())
                && provider
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-');
            if !valid {
                return None;
            }
            nonempty(object, "identifier")?;
            reference(&["title", "url"])
        }
        "linear-issue" => {
            nonempty(object, "identifier")?;
            reference(&["title", "url"])
        }
        "guest-issue" | "guest-pr" => {
            nonempty(object, "providerId")?;
            nonempty(object, "id")?;
            reference(&["title", "url"])
        }
        "session" => {
            nonempty(object, "sessionId")?;
            reference(&["title"])
        }
        "session-message" => {
            nonempty(object, "sessionId")?;
            nonempty(object, "messageId")?;
            reference(&["sessionTitle", "excerpt"])
        }
        _ => None,
    }
}

/// OpenCode Desktop's comment: a file quote without a selection, a code
/// comment with one.
fn opencode_comment(object: &Map<String, Value>) -> Option<ContextPayload> {
    let path = text(object, "path")?;
    let comment = text(object, "comment")?;
    let preview = match object.get("preview") {
        None => String::new(),
        Some(value) => value.as_str()?.to_owned(),
    };
    // The origin only names the comment's source, which no text shows.
    if object
        .get("origin")
        .is_some_and(|origin| !matches!(origin.as_str(), Some("file" | "review")))
    {
        return None;
    }
    match object.get("selection") {
        None => Some(ContextPayload::FileQuote {
            file_label: path,
            start_line: None,
            end_line: None,
            quote: preview,
            text: comment,
        }),
        Some(selection) => {
            let selection = selection.as_object()?;
            let number = |key: &str| {
                selection
                    .get(key)?
                    .as_f64()
                    .filter(|number| number.is_finite())
                    .map(|number| number as i64)
            };
            Some(ContextPayload::CodeComment {
                file_label: path,
                start_line: number("startLine")?,
                end_line: number("endLine")?,
                side: None,
                language: String::new(),
                code: preview,
                text: comment,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_payloads_read_and_broken_ones_do_not() {
        let quote = read(Some(&json!({"openchamberContext": {
            "kind": "file-quote", "fileLabel": "a.rs", "startLine": 3, "quote": "x", "text": "y"
        }})));
        assert_eq!(
            quote,
            Some(ContextPayload::FileQuote {
                file_label: "a.rs".into(),
                start_line: Some(3),
                end_line: None,
                quote: "x".into(),
                text: "y".into(),
            })
        );
        assert_eq!(
            read(Some(
                &json!({"openchamberContext": {"kind": "session", "sessionId": "ses_a", "title": "T"}})
            )),
            Some(ContextPayload::Reference)
        );
        assert_eq!(
            read(Some(
                &json!({"openchamberContext": {"kind": "change-request", "number": 0, "title": "T", "url": "u"}})
            )),
            None,
            "a change request needs a positive number"
        );
        assert_eq!(read(Some(&json!({"source": "plugin"}))), None);
    }

    #[test]
    fn opencode_comments_fall_back_to_quotes_and_code_comments() {
        let comment = read(Some(&json!({"opencodeComment": {
            "path": "a.rs", "comment": "why", "preview": "fn a()",
            "selection": {"startLine": 1, "endLine": 2}
        }})));
        assert!(matches!(
            comment,
            Some(ContextPayload::CodeComment {
                start_line: 1,
                end_line: 2,
                ..
            })
        ));
    }
}
