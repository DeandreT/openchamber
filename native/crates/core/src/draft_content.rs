//! Semantic composer content without serialization, local paths or attachment bytes.
//!
//! The desktop owns prepared bytes and their storage under each attachment identity.
//! JSON strings are opaque here; transport and persistence adapters validate them.

use std::fmt;

use crate::ids::{AttachmentId, ContextItemId};

/// Everything retained with a draft besides its text and caret.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct DraftContent {
    pub files: Vec<DraftFile>,
    pub agents: Vec<AgentMention>,
    pub skills: Vec<SkillRef>,
    pub metadata_json: Option<String>,
    pub context: Vec<DraftContext>,
}

impl DraftContent {
    /// Whether no semantic content is retained, including metadata and references.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
            && self.agents.is_empty()
            && self.skills.is_empty()
            && self.metadata_json.is_none()
            && self.context.is_empty()
    }
}

impl fmt::Debug for DraftContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DraftContent")
            .field("files", &self.files)
            .field("agents", &self.agents.len())
            .field("skills", &self.skills.len())
            .field(
                "metadata_json",
                &self.metadata_json.as_ref().map(|_| "[redacted]"),
            )
            .field("context", &self.context)
            .finish()
    }
}

/// An attachment's semantic identity and prepared representation facts.
#[derive(Clone, PartialEq, Eq)]
pub struct DraftFile {
    pub id: AttachmentId,
    pub name: Option<String>,
    pub media_type: Option<String>,
    pub byte_len: Option<u64>,
    pub source_document_id: Option<AttachmentId>,
    pub content: DraftFileContent,
}

impl fmt::Debug for DraftFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DraftFile")
            .field("id", &self.id)
            .field("byte_len", &self.byte_len)
            .field("source_document_id", &self.source_document_id)
            .field("content", &self.content)
            .finish_non_exhaustive()
    }
}

/// A sendable reference or immutable prepared bytes retained by the desktop.
#[derive(Clone, PartialEq, Eq)]
pub enum DraftFileContent {
    Reference { uri: String },
    Prepared,
}

impl fmt::Debug for DraftFileContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Reference { .. } => "Reference([redacted])",
            Self::Prepared => "Prepared",
        })
    }
}

/// One synthetic context item, before transport allocates its message identity.
#[derive(Clone, PartialEq, Eq)]
pub struct DraftContext {
    pub id: ContextItemId,
    pub text: String,
    pub description: Option<String>,
    pub metadata_json: Option<String>,
    pub instructions: Option<String>,
}

impl fmt::Debug for DraftContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DraftContext")
            .field("id", &self.id)
            .field("content", &"[redacted]")
            .finish()
    }
}

/// A reference's range in the prompt text, measured in UTF-16 code units.
#[derive(Clone, PartialEq, Eq)]
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

/// An agent named in the prompt text or attached without a textual mention.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentMention {
    pub name: String,
    pub mention: Option<Mention>,
}

impl fmt::Debug for AgentMention {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentMention")
            .field("name", &"[redacted]")
            .field("mention", &self.mention)
            .finish()
    }
}

/// A resolved skill loaded with the prompt.
#[derive(Clone, PartialEq, Eq)]
pub struct SkillRef {
    pub id: String,
}

impl fmt::Debug for SkillRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SkillRef([redacted])")
    }
}

/// Content that cannot be combined without choosing which retained value to lose.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContentConflict {
    Metadata,
    File { id: AttachmentId },
    Context { id: ContextItemId },
    MentionOffsetOverflow,
}

impl fmt::Display for ContentConflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Metadata => "Draft metadata conflicts with the retained attempt",
            Self::File { .. } => "An attachment identity has conflicting content",
            Self::Context { .. } => "A context identity has conflicting content",
            Self::MentionOffsetOverflow => "A restored mention exceeds the supported text range",
        })
    }
}

impl std::error::Error for ContentConflict {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_hide_semantic_payloads() {
        let content = DraftContent {
            files: vec![DraftFile {
                id: AttachmentId::from("attachment-id"),
                name: Some("private-filename".to_owned()),
                media_type: Some("private-media-type".to_owned()),
                byte_len: Some(3),
                source_document_id: None,
                content: DraftFileContent::Reference {
                    uri: "file:///private-path".to_owned(),
                },
            }],
            agents: vec![AgentMention {
                name: "private-agent".to_owned(),
                mention: Some(Mention {
                    start: 0,
                    end: 5,
                    text: "private-mention".to_owned(),
                }),
            }],
            skills: vec![SkillRef {
                id: "private-skill".to_owned(),
            }],
            metadata_json: Some("private-metadata".to_owned()),
            context: vec![DraftContext {
                id: ContextItemId::from("context-id"),
                text: "private-text".to_owned(),
                description: Some("private-description".to_owned()),
                metadata_json: Some("private-context-metadata".to_owned()),
                instructions: Some("private-instructions".to_owned()),
            }],
        };
        let debug = format!("{content:?} {:?} {:?}", content.agents, content.skills);
        assert!(
            !debug.contains("private-"),
            "semantic content must stay out of Debug"
        );
        assert!(debug.contains("attachment-id"));
        assert!(debug.contains("context-id"));
    }
}
