//! Other sessions and their messages mentioned in the composer's text
//! (`composer/sessionReferences.ts`, `lib/sessionLinks.ts`).
//!
//! A mention is a session link in the text, `openchamber://session/<id>`
//! with `?message=<id>` for one message, written by the `@` picker or
//! pasted. The text decides what goes out: beside it, each linked session or
//! message goes once, in the order it first appears, as a context item saying
//! what it is. A session goes by name, since the openchamber tool reads it; a
//! message goes whole. A link deleted from the text is not sent.

use crate::transcript::{Message, PartContent, Role};

/// The link the `@` picker writes for a session.
pub fn session_link_text(session_id: &str) -> String {
    format!("openchamber://session/{session_id}")
}

/// What a link points at: a session, or one message of it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionLinkTarget {
    pub session_id: String,
    pub message_id: Option<String>,
}

/// A link candidate as it sits in the text: the deep link, or a web address
/// that names `session`. Punctuation that ends the sentence is not part of
/// it. Byte offsets; the caller parses the link with its own origins.
pub fn link_candidates(text: &str) -> Vec<(usize, &str)> {
    if !text.contains("session") {
        return Vec::new();
    }
    let lower = text.to_ascii_lowercase();
    let mut found = Vec::new();
    let mut from = 0;
    while from < text.len() {
        let next = ["openchamber://session/", "https://", "http://"]
            .iter()
            .filter_map(|prefix| {
                lower[from..]
                    .find(prefix)
                    .map(|at| (from + at, prefix.len()))
            })
            .min_by_key(|(at, _)| *at);
        let Some((start, prefix)) = next else {
            break;
        };
        let rest = &text[start + prefix..];
        let length = rest
            .find(|character: char| character.is_whitespace() || "<>()\"'`".contains(character))
            .unwrap_or(rest.len());
        if length == 0 {
            from = start + prefix;
            continue;
        }
        let end = start + prefix + length;
        let link = text[start..end].trim_end_matches(['.', ',', ';', ':', '!', '?']);
        if link.contains("session") {
            found.push((start, link));
        }
        from = end;
    }
    found
}

/// The key of a link's target: one per session, and per message of it.
pub fn link_key(target: &SessionLinkTarget) -> String {
    format!(
        "{}:{}",
        target.session_id,
        target.message_id.as_deref().unwrap_or("")
    )
}

/// A session or message the text mentions, as it is sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionReference {
    /// `message_id`: the message link that named this session, when the
    /// message had no text of its own.
    Session {
        session_id: String,
        title: String,
        message_id: Option<String>,
        context_text: String,
    },
    Message {
        session_id: String,
        message_id: String,
        session_title: String,
        excerpt: String,
        context_text: String,
    },
}

impl SessionReference {
    /// The key of the link that named it, matching `link_key`.
    pub fn key(&self) -> String {
        match self {
            Self::Session {
                session_id,
                message_id,
                ..
            } => link_key(&SessionLinkTarget {
                session_id: session_id.clone(),
                message_id: message_id.clone(),
            }),
            Self::Message {
                session_id,
                message_id,
                ..
            } => link_key(&SessionLinkTarget {
                session_id: session_id.clone(),
                message_id: Some(message_id.clone()),
            }),
        }
    }

    /// What the model reads.
    pub fn context_text(&self) -> &str {
        match self {
            Self::Session { context_text, .. } | Self::Message { context_text, .. } => context_text,
        }
    }

    /// How its chip reads: the session's title, or the start of the message.
    pub fn label(&self) -> &str {
        match self {
            Self::Session { title, .. } => title,
            Self::Message { excerpt, .. } => excerpt,
        }
    }
}

const EXCERPT_LENGTH: usize = 120;

/// `sessionReference`: a session by name.
pub fn session_reference(session_id: &str, title: &str) -> SessionReference {
    SessionReference::Session {
        session_id: session_id.to_owned(),
        title: title.to_owned(),
        message_id: None,
        context_text: format!(
            "The user referenced another OpenChamber session, \"{title}\" ({session_id}). \
             Its messages can be read with the openchamber tool (session.messages, sessionId {session_id})."
        ),
    }
}

/// `messageText`: a message's own text parts, trimmed, a blank line apart.
pub fn message_text(message: &Message) -> String {
    message
        .parts
        .iter()
        .filter_map(|part| match &part.content {
            PartContent::Text { text } => Some(text.trim()),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn author(role: Role) -> &'static str {
    match role {
        Role::User => "the user of that session",
        Role::Assistant => "the agent of that session",
        _ => "OpenChamber",
    }
}

/// `sessionMessageReference`: the message whole, or None when it has no text
/// of its own; the caller then names its session.
pub fn message_reference(
    session_id: &str,
    title: &str,
    message: &Message,
) -> Option<SessionReference> {
    let text = message_text(message);
    if text.is_empty() {
        return None;
    }
    let excerpt = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let excerpt = if excerpt.encode_utf16().count() > EXCERPT_LENGTH {
        let mut cut = String::new();
        let mut units = 0;
        for character in excerpt.chars() {
            units += character.len_utf16();
            if units > EXCERPT_LENGTH - 1 {
                break;
            }
            cut.push(character);
        }
        cut.push('…');
        cut
    } else {
        excerpt
    };
    let message_id = message.info.id.as_str();
    Some(SessionReference::Message {
        session_id: session_id.to_owned(),
        message_id: message_id.to_owned(),
        session_title: title.to_owned(),
        excerpt,
        context_text: format!(
            "The user attached a message from another OpenChamber session, \"{title}\" \
             ({session_id}, message {message_id}). It was written by {}. The message:\n\n{text}",
            author(message.info.role)
        ),
    })
}

/// A message link whose message has no text names its session, keeping the
/// message's ID.
pub fn session_reference_for_message(
    session_id: &str,
    title: &str,
    message_id: &str,
) -> SessionReference {
    match session_reference(session_id, title) {
        SessionReference::Session {
            session_id,
            title,
            context_text,
            ..
        } => SessionReference::Session {
            session_id,
            title,
            message_id: Some(message_id.to_owned()),
            context_text,
        },
        message => message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{MessageId, PartId};
    use crate::transcript::{MessageInfo, Part};

    #[test]
    fn candidates_drop_sentence_punctuation_and_unrelated_links() {
        let text = "see openchamber://session/ses_a, and https://x.test/?session=ses_b. not https://x.test/a";
        let found: Vec<&str> = link_candidates(text)
            .into_iter()
            .map(|(_, link)| link)
            .collect();
        assert_eq!(
            found,
            [
                "openchamber://session/ses_a",
                "https://x.test/?session=ses_b"
            ]
        );
        assert!(link_candidates("no links").is_empty());
        let (start, _) = link_candidates("(openchamber://session/ses_a)")[0];
        assert_eq!(start, 1);
    }

    #[test]
    fn keys_distinguish_sessions_from_their_messages() {
        let session = session_reference("ses_a", "Tweets");
        assert_eq!(session.key(), "ses_a:");
        assert_eq!(session.label(), "Tweets");
        assert!(session.context_text().contains("sessionId ses_a"));
        assert_eq!(
            session_reference_for_message("ses_a", "Tweets", "msg_b").key(),
            "ses_a:msg_b"
        );
        assert_eq!(session_link_text("ses_a"), "openchamber://session/ses_a");
    }

    fn message(role: Role, parts: Vec<Part>) -> Message {
        Message {
            info: MessageInfo {
                id: MessageId::new("msg_a"),
                role,
                created: 0,
                completed: None,
                notice: None,
                assistant: None,
                compaction: None,
                synthetic: None,
            },
            parts,
        }
    }

    #[test]
    fn a_message_goes_whole_with_its_author_and_a_short_excerpt() {
        let long = "word ".repeat(40);
        let message = message(
            Role::Assistant,
            vec![Part::text(
                PartId::new("p"),
                format!("Line one\nline two {long}"),
            )],
        );
        let reference = message_reference("ses_a", "Tweet writing", &message).expect("text");
        let SessionReference::Message {
            excerpt,
            context_text,
            ..
        } = &reference
        else {
            panic!("a message reference");
        };
        assert!(excerpt.starts_with("Line one line two word"));
        assert_eq!(excerpt.encode_utf16().count(), EXCERPT_LENGTH);
        assert!(excerpt.ends_with('…'));
        assert!(context_text.contains("written by the agent of that session"));
        assert_eq!(reference.key(), "ses_a:msg_a");
        let empty = super::tests::message(Role::User, Vec::new());
        assert!(message_reference("ses_a", "t", &empty).is_none());
    }
}
