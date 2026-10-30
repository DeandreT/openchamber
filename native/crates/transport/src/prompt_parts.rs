//! What a prompt carries besides its text: attachments, agent and skill
//! references, metadata and the synthetic context admitted ahead of it, plus
//! the slash-command request. Their request bodies mirror the pinned
//! `@opencode/client` 2.2.0 `session.prompt`, `session.synthetic` and
//! `session.command` payloads and stay private to the crate.

use std::fmt;

use openchamber_native_core::ids::MessageId;
use openchamber_native_core::send_config::ModelChoice;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::prompt::Delivery;

/// An attachment by reference: a server path as a `file://` URI, a remote URL
/// or a small `data:` URI. The caller uploads or references large content first.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct PromptFile {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl fmt::Debug for PromptFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PromptFile([redacted])")
    }
}

/// Where a reference was typed in the prompt text, in the composer's offsets.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct Mention {
    pub start: u32,
    pub end: u32,
    pub text: String,
}

impl fmt::Debug for Mention {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Mention({}..{})", self.start, self.end)
    }
}

/// An agent named in the prompt (`@plan`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AgentMention {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mention: Option<Mention>,
}

/// A skill OpenCode loads with the prompt, by its resolved ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SkillRef {
    pub id: String,
}

/// A JSON object carried with a prompt or a context item. Display data only:
/// it grants no routing or permission authority. Debug output hides it.
#[derive(Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Metadata(pub Map<String, Value>);

impl fmt::Debug for Metadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Metadata({} keys)", self.0.len())
    }
}

/// A context block admitted as a synthetic message right before its prompt,
/// with the prompt's delivery and `resume: false`, so it starts no turn.
#[derive(Clone, PartialEq, Eq)]
pub struct SyntheticContext {
    /// Client-generated `msg_` identity, minted before the prompt's.
    pub id: MessageId,
    pub text: String,
    pub description: Option<String>,
    pub metadata: Option<Metadata>,
}

impl fmt::Debug for SyntheticContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SyntheticContext")
            .field("id", &self.id)
            .field("text", &"[redacted]")
            .field(
                "description",
                &self.description.as_ref().map(|_| "[redacted]"),
            )
            .field("metadata", &self.metadata)
            .finish()
    }
}

/// Everything a prompt carries besides its text. The default is a text-only
/// prompt, whose request body is exactly `{ id, text, delivery? }`.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct PromptParts {
    pub files: Vec<PromptFile>,
    pub agents: Vec<AgentMention>,
    pub skills: Vec<SkillRef>,
    pub metadata: Option<Metadata>,
    /// Admitted in order before the prompt, each under its own ID.
    pub context: Vec<SyntheticContext>,
}

impl PromptParts {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

impl fmt::Debug for PromptParts {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptParts")
            .field("files", &self.files.len())
            .field("agents", &self.agents.len())
            .field("skills", &self.skills.len())
            .field("metadata", &self.metadata)
            .field("context", &self.context)
            .finish()
    }
}

/// A slash command. OpenCode assigns its message ID and answers 204 without
/// one, so an uncertain outcome cannot be reconciled by ID.
#[derive(Clone)]
pub struct Command {
    pub name: String,
    /// The arguments after the command name; empty when there are none.
    pub text: String,
    pub files: Vec<PromptFile>,
    pub context: Vec<SyntheticContext>,
    pub model: Option<ModelChoice>,
    pub agent: Option<String>,
    pub delivery: Option<Delivery>,
}

impl fmt::Debug for Command {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Command")
            .field("name", &self.name)
            .field("text", &"[redacted]")
            .field("files", &self.files.len())
            .field("context", &self.context)
            .field("model", &self.model)
            .field("agent", &self.agent)
            .field("delivery", &self.delivery)
            .finish()
    }
}

#[derive(Serialize)]
pub(crate) struct PromptBody<'a> {
    pub id: &'a str,
    pub text: &'a str,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    pub files: &'a [PromptFile],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    pub agents: &'a [AgentMention],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    pub skills: &'a [SkillRef],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<&'a Metadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Delivery>,
}

#[derive(Serialize)]
pub(crate) struct SyntheticBody<'a> {
    pub id: &'a str,
    pub text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<&'a Metadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Delivery>,
    /// Context never starts a turn; the prompt after it does.
    pub resume: bool,
}

#[derive(Serialize)]
pub(crate) struct CommandBody<'a> {
    pub name: &'a str,
    pub text: &'a str,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    pub files: &'a [PromptFile],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Delivery>,
}

/// Rejects shapes the server would refuse before any mutation is sent.
pub(crate) fn valid_files(files: &[PromptFile]) -> bool {
    files.iter().all(|file| {
        !file.uri.trim().is_empty() && file.name.as_deref().is_none_or(|name| !name.is_empty())
    })
}

pub(crate) fn valid_context(context: &[SyntheticContext], prompt: Option<&MessageId>) -> bool {
    let mut seen = std::collections::HashSet::new();
    context.iter().all(|item| {
        item.id.as_str().starts_with("msg_")
            && Some(&item.id) != prompt
            && seen.insert(&item.id)
            && !item.text.trim().is_empty()
    })
}

pub(crate) fn valid_parts(parts: &PromptParts, prompt: &MessageId) -> bool {
    valid_files(&parts.files)
        && parts.agents.iter().all(|agent| {
            !agent.name.trim().is_empty()
                && agent
                    .mention
                    .as_ref()
                    .is_none_or(|mention| mention.start <= mention.end)
        })
        && parts.skills.iter().all(|skill| !skill.id.trim().is_empty())
        && valid_context(&parts.context, Some(prompt))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_hides_content() {
        let parts = PromptParts {
            files: vec![PromptFile {
                uri: "file:///private/notes.md".to_owned(),
                name: Some("notes.md".to_owned()),
            }],
            metadata: Some(Metadata(Map::from_iter([(
                "secret".to_owned(),
                Value::from("value"),
            )]))),
            context: vec![SyntheticContext {
                id: MessageId::from("msg_context"),
                text: "private quote".to_owned(),
                description: Some("private description".to_owned()),
                metadata: None,
            }],
            ..PromptParts::default()
        };
        let debug = format!("{parts:?}");
        for hidden in [
            "notes.md",
            "secret",
            "value",
            "private quote",
            "private description",
        ] {
            assert!(!debug.contains(hidden), "{hidden} leaks into Debug");
        }
        assert!(
            debug.contains("msg_context"),
            "IDs stay visible for diagnostics"
        );
    }

    #[test]
    fn context_ids_must_be_unique_prefixed_and_distinct_from_the_prompt() {
        let prompt = MessageId::from("msg_prompt");
        let item = |id: &str, text: &str| SyntheticContext {
            id: MessageId::from(id),
            text: text.to_owned(),
            description: None,
            metadata: None,
        };
        assert!(valid_context(
            &[item("msg_a", "a"), item("msg_b", "b")],
            Some(&prompt)
        ));
        assert!(!valid_context(
            &[item("msg_a", "a"), item("msg_a", "b")],
            Some(&prompt)
        ));
        assert!(!valid_context(&[item("ctx_a", "a")], Some(&prompt)));
        assert!(!valid_context(&[item("msg_prompt", "a")], Some(&prompt)));
        assert!(!valid_context(&[item("msg_a", " \n")], Some(&prompt)));
    }
}
