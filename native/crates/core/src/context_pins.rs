//! Messages pinned into a session's context
//! (`metadata.openchamber.context_obligatory_messages`), as
//! `packages/ui/src/lib/contextObligatoryMessages.ts` reads them, and the one
//! line a pin shows (`firstText` of `WorkStatusPinnedSection.tsx`).
//!
//! These are separate from the sidebar's session pin
//! (`metadata.openchamber.pinned.at`, [`SessionOverlay::pinned_at`]).
//!
//! [`SessionOverlay::pinned_at`]: crate::session::SessionOverlay::pinned_at

use std::cmp::Ordering;

use crate::ids::{MessageId, SessionId};
use crate::session::SessionRecord;
use crate::transcript::{Message, PartContent};

/// A pin's `createdAt`: a finite number of milliseconds since the Unix epoch.
///
/// The stored value is a JSON number, so it is kept as one. Equality and
/// order are numeric; negative zero is stored as zero so both agree with the
/// bits.
#[derive(Clone, Copy, Debug)]
pub struct PinTime(f64);

impl PinTime {
    /// A finite `millis`, else `None`.
    pub fn new(millis: f64) -> Option<Self> {
        // Adding zero turns negative zero into zero and changes nothing else.
        millis.is_finite().then_some(Self(millis + 0.0))
    }

    /// A whole number of milliseconds, as message records carry them.
    pub fn from_millis(millis: i64) -> Self {
        Self(millis as f64)
    }

    pub fn as_f64(self) -> f64 {
        self.0
    }
}

impl PartialEq for PinTime {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for PinTime {}

impl PartialOrd for PinTime {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PinTime {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Who wrote a pinned message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PinRole {
    User,
    Assistant,
}

impl PinRole {
    /// The stored `role`, when it is one a pin can have.
    pub fn from_wire(role: &str) -> Option<Self> {
        match role {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            _ => None,
        }
    }

    pub fn as_wire(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// One valid entry of `context_obligatory_messages`, in stored order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextPin {
    pub id: MessageId,
    pub created_at: PinTime,
    pub role: PinRole,
    /// `sessionID`: the session the pin was made in. Pins written before the
    /// field existed have none.
    pub session_id: Option<SessionId>,
}

/// The pins `session` shows, in conversation order: ascending `createdAt`,
/// then message ID by UTF-16 code unit, as `getContextObligatoryMessages`
/// sorts them.
///
/// A root session keeps every stored pin, including those a user fork
/// inherited. A child session keeps the pins that name it; a pin without a
/// session counts only when it is newer than the child itself.
pub fn owned(session: &SessionRecord) -> Vec<&ContextPin> {
    if session.context_pins.is_empty() || session.id.as_str().is_empty() {
        return Vec::new();
    }
    let child = session
        .parent_id
        .as_ref()
        .is_some_and(|parent| !parent.as_str().is_empty());
    let mut pins: Vec<&ContextPin> = session
        .context_pins
        .iter()
        .filter(|pin| {
            if !child {
                return true;
            }
            match &pin.session_id {
                Some(owner) => *owner == session.id,
                None => pin.created_at.as_f64() > session.time.created as f64,
            }
        })
        .collect();
    pins.sort_by(|a, b| {
        a.created_at.cmp(&b.created_at).then_with(|| {
            a.id.as_str()
                .encode_utf16()
                .cmp(b.id.as_str().encode_utf16())
        })
    });
    pins
}

/// Whether `message` is one of the pins `session` shows.
pub fn is_pinned(session: &SessionRecord, message: &MessageId) -> bool {
    owned(session).iter().any(|pin| pin.id == *message)
}

/// The line a pin shows for `message`: its first text part through
/// [`compact_text`]. A first text part without text shows nothing, even when
/// a later part has some.
pub fn first_text(message: &Message) -> Option<String> {
    first_text_part(message).and_then(compact_text)
}

/// The text of the message's first text part, as it was written.
pub fn first_text_part(message: &Message) -> Option<&str> {
    message.parts.iter().find_map(|part| match &part.content {
        PartContent::Text { text } => Some(text.as_str()),
        _ => None,
    })
}

/// `text` on one line without the Markdown marks that read as noise in plain
/// text: fence lines, quote markers, heading markers, `**`, `__` and
/// backticks. Whitespace collapses to single spaces. `None` when nothing is
/// left.
///
/// Each step is one of the reference's regular expressions, applied in its
/// order to the previous step's result.
pub fn compact_text(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let chars = strip_at_line_starts(&chars, fence_line);
    let chars = strip_at_line_starts(&chars, quote_markers);
    let chars = strip_at_line_starts(&chars, heading_marker);
    let chars = strip_emphasis(&chars);
    let mut plain = String::with_capacity(chars.len());
    let mut pending_space = false;
    for c in chars {
        if is_js_whitespace(c) {
            pending_space = !plain.is_empty();
        } else {
            if pending_space {
                plain.push(' ');
                pending_space = false;
            }
            plain.push(c);
        }
    }
    (!plain.is_empty()).then_some(plain)
}

/// `\s` of an ECMAScript regular expression.
fn is_js_whitespace(c: char) -> bool {
    matches!(c,
        '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' |
        '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' |
        '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// Whether `^` of a multiline expression matches at `at`.
fn is_line_start(chars: &[char], at: usize) -> bool {
    at == 0 || matches!(chars[at - 1], '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn skip_whitespace(chars: &[char], mut at: usize) -> usize {
    while chars.get(at).is_some_and(|c| is_js_whitespace(*c)) {
        at += 1;
    }
    at
}

/// A global multiline replace with nothing: wherever `matcher` matches at a
/// line start it answers the match's end, and the match is left out.
fn strip_at_line_starts(chars: &[char], matcher: fn(&[char], usize) -> Option<usize>) -> Vec<char> {
    let mut kept = Vec::with_capacity(chars.len());
    let mut at = 0;
    while at < chars.len() {
        if is_line_start(chars, at)
            && let Some(end) = matcher(chars, at)
            && end > at
        {
            at = end;
            continue;
        }
        kept.push(chars[at]);
        at += 1;
    }
    kept
}

/// `^\s*(`{3,}|~{3,})[^\n]*$`: a fence line, up to its line feed.
fn fence_line(chars: &[char], at: usize) -> Option<usize> {
    let start = skip_whitespace(chars, at);
    let marker = *chars.get(start).filter(|c| matches!(**c, '`' | '~'))?;
    let run = chars[start..].iter().take_while(|c| **c == marker).count();
    if run < 3 {
        return None;
    }
    let rest = start + run;
    Some(
        chars[rest..]
            .iter()
            .position(|c| *c == '\n')
            .map_or(chars.len(), |offset| rest + offset),
    )
}

/// `^\s*(>\s*)+`: a line's quote markers.
fn quote_markers(chars: &[char], at: usize) -> Option<usize> {
    let mut end = skip_whitespace(chars, at);
    if chars.get(end) != Some(&'>') {
        return None;
    }
    while chars.get(end) == Some(&'>') {
        end = skip_whitespace(chars, end + 1);
    }
    Some(end)
}

/// `^\s*#{1,6}\s+`: a heading's marker and the space after it.
fn heading_marker(chars: &[char], at: usize) -> Option<usize> {
    let start = skip_whitespace(chars, at);
    let run = chars[start..].iter().take_while(|c| **c == '#').count();
    if !(1..=6).contains(&run) {
        return None;
    }
    let rest = start + run;
    let end = skip_whitespace(chars, rest);
    (end > rest).then_some(end)
}

/// `(\*\*|__|`)` replaced with nothing.
fn strip_emphasis(chars: &[char]) -> Vec<char> {
    let mut kept = Vec::with_capacity(chars.len());
    let mut at = 0;
    while at < chars.len() {
        let pair = matches!(chars[at], '*' | '_') && chars.get(at + 1) == Some(&chars[at]);
        if pair {
            at += 2;
        } else if chars[at] == '`' {
            at += 1;
        } else {
            kept.push(chars[at]);
            at += 1;
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionTime;
    use crate::transcript::{MessageInfo, Part, Role};

    fn pin(id: &str, created_at: f64, session: Option<&str>) -> ContextPin {
        ContextPin {
            id: MessageId::from(id),
            created_at: PinTime::new(created_at).expect("fixture time is finite"),
            role: PinRole::User,
            session_id: session.map(SessionId::from),
        }
    }

    fn session(
        id: &str,
        parent: Option<&str>,
        created: i64,
        pins: Vec<ContextPin>,
    ) -> SessionRecord {
        SessionRecord {
            parent_id: parent.map(SessionId::from),
            time: SessionTime {
                created,
                ..SessionTime::default()
            },
            context_pins: pins,
            ..SessionRecord::new(id, "/p")
        }
    }

    fn ids(session: &SessionRecord) -> Vec<&str> {
        owned(session).iter().map(|pin| pin.id.as_str()).collect()
    }

    #[test]
    fn a_root_session_keeps_inherited_pins_in_conversation_order() {
        let root = session(
            "ses_root",
            None,
            500,
            vec![
                pin("msg_c", 30.0, Some("ses_other")),
                pin("msg_b", 10.0, None),
                pin("msg_a", 10.0, Some("ses_root")),
                pin("msg_d", 20.5, None),
            ],
        );
        assert_eq!(ids(&root), ["msg_a", "msg_b", "msg_d", "msg_c"]);
        assert!(is_pinned(&root, &MessageId::from("msg_c")));
        assert!(!is_pinned(&root, &MessageId::from("msg_z")));
    }

    #[test]
    fn a_child_session_keeps_its_own_pins_and_newer_ownerless_ones() {
        let child = session(
            "ses_child",
            Some("ses_root"),
            100,
            vec![
                pin("msg_parent", 150.0, Some("ses_root")),
                pin("msg_own_old", 50.0, Some("ses_child")),
                pin("msg_legacy_old", 100.0, None),
                pin("msg_legacy_new", 100.5, None),
            ],
        );
        assert_eq!(ids(&child), ["msg_own_old", "msg_legacy_new"]);
    }

    #[test]
    fn a_blank_parent_reads_as_a_root_and_a_blank_session_shows_nothing() {
        let pins = vec![pin("msg_a", 1.0, Some("ses_other"))];
        assert_eq!(
            ids(&session("ses_x", Some(""), 100, pins.clone())),
            ["msg_a"]
        );
        assert!(owned(&session("", None, 100, pins)).is_empty());
    }

    #[test]
    fn ties_order_by_utf16_code_unit() {
        // U+FF5E sorts before U+1F600 by code point and after it by UTF-16
        // code unit, which is how JavaScript compares strings.
        let root = session(
            "ses_root",
            None,
            0,
            vec![pin("\u{ff5e}", 1.0, None), pin("\u{1f600}", 1.0, None)],
        );
        assert_eq!(ids(&root), ["\u{1f600}", "\u{ff5e}"]);
    }

    #[test]
    fn pin_times_are_finite_and_compare_numerically() {
        assert!(PinTime::new(f64::NAN).is_none());
        assert!(PinTime::new(f64::INFINITY).is_none());
        assert_eq!(PinTime::new(-0.0), PinTime::new(0.0));
        assert!(PinTime::from_millis(2) > PinTime::new(1.5).expect("finite"));
        assert_eq!(PinRole::from_wire("assistant"), Some(PinRole::Assistant));
        assert_eq!(PinRole::from_wire("system"), None);
        assert_eq!(PinRole::User.as_wire(), "user");
    }

    #[test]
    fn compact_text_drops_the_listed_marks_and_collapses_whitespace() {
        assert_eq!(
            compact_text("## Plan\n\n> **Ship** the `fix`\n>> and __rest__\n").as_deref(),
            Some("Plan Ship the fix and rest")
        );
        assert_eq!(
            compact_text("intro\n```rust title\nlet a = 1;\n~~~~\ndone").as_deref(),
            Some("intro let a = 1; done")
        );
        // A fence needs three marks, a heading at most six and a space.
        assert_eq!(compact_text("``x``").as_deref(), Some("x"));
        assert_eq!(compact_text("~~x~~").as_deref(), Some("~~x~~"));
        assert_eq!(
            compact_text("####### seven").as_deref(),
            Some("####### seven")
        );
        assert_eq!(compact_text("#tag").as_deref(), Some("#tag"));
        // Marks inside a line stay; single emphasis marks are not listed.
        assert_eq!(
            compact_text("a > b # c *d* _e_").as_deref(),
            Some("a > b # c *d* _e_")
        );
        assert_eq!(
            compact_text("a\u{00a0}\t b\r\n\u{2028}c").as_deref(),
            Some("a b c")
        );
    }

    #[test]
    fn compact_text_of_only_marks_or_whitespace_is_absent() {
        assert_eq!(compact_text(""), None);
        assert_eq!(compact_text(" \n\t"), None);
        assert_eq!(compact_text("```\n> \n# \n**__``"), None);
    }

    #[test]
    fn the_first_text_part_decides_the_line() {
        let message = |parts: Vec<Part>| Message {
            info: MessageInfo {
                id: MessageId::from("msg_1"),
                role: Role::Assistant,
                created: 1,
                completed: None,
                notice: None,
                assistant: None,
                compaction: None,
                synthetic: None,
            },
            parts,
        };
        let reasoning = Part {
            content: PartContent::Reasoning {
                text: "thinking".to_owned(),
            },
            ..Part::text("msg_1:reasoning:0", "")
        };
        assert_eq!(
            first_text(&message(vec![
                reasoning.clone(),
                Part::text("a", "**Answer**")
            ]))
            .as_deref(),
            Some("Answer")
        );
        assert_eq!(
            first_text(&message(vec![
                Part::text("a", " "),
                Part::text("b", "later")
            ])),
            None,
            "the first text part is the only one read"
        );
        assert_eq!(first_text(&message(vec![reasoning])), None);
    }
}
