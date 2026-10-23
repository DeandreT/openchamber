//! Deterministic synthetic workloads for the offline client and measurements.
//!
//! Every value derives from fixed seeds: the same call always yields the same
//! records, order and text, so parity runs and measurement receipts can
//! identify their exact content. Nothing here is personal data.

use crate::chat_roots::ChatRoots;
use crate::ids::{MessageId, PartId, ProjectId, SessionId};
use crate::ownership::{ConfiguredProject, ProjectWorktrees, WorktreeInfo};
use crate::session::{SessionOverlay, SessionRecord, SessionTime};
use crate::transcript::{Message, MessageInfo, Part, PartContent, PartTime, Role, ToolStatus};

/// The home directory every synthetic runtime reports.
pub const HOME: &str = "/home/synthetic";
/// Base timestamp of synthetic records: 2026-09-01T00:00:00Z.
const BASE_TIME_MS: i64 = 1_788_220_800_000;
const MINUTE_MS: i64 = 60_000;

/// Sizes of the documented workloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WorkloadSize {
    /// The representative 2,301-session index.
    Representative,
    /// The separate 23,010-session stress case.
    Stress,
    /// A handful of sessions for focused checks.
    Small,
}

impl WorkloadSize {
    pub fn session_count(self) -> usize {
        match self {
            Self::Representative => 2_301,
            Self::Stress => 23_010,
            Self::Small => 24,
        }
    }
}

/// A synthetic server's projects, topology and sessions.
#[derive(Clone, Debug)]
pub struct SyntheticWorkspace {
    pub projects: Vec<ConfiguredProject>,
    pub worktrees: Vec<ProjectWorktrees>,
    pub sessions: Vec<SessionRecord>,
    pub chat_roots: ChatRoots,
}

const PROJECT_NAMES: [&str; 12] = [
    "openchamber",
    "atlas",
    "harbor",
    "lumen",
    "quarry",
    "meridian",
    "cobalt",
    "tundra",
    "saffron",
    "keystone",
    "drift",
    "orchid",
];

const TITLE_SUBJECTS: [&str; 16] = [
    "sidebar visibility",
    "transcript scrolling",
    "draft restore",
    "IME submission",
    "worktree move",
    "theme tokens",
    "locale export",
    "session paging",
    "event decoding",
    "permission prompt",
    "terminal resize",
    "diff view",
    "settings search",
    "relay handshake",
    "startup timing",
    "accessibility tree",
];

const TITLE_VERBS: [&str; 8] = [
    "Fix",
    "Investigate",
    "Port",
    "Measure",
    "Review",
    "Refactor",
    "Document",
    "Test",
];

/// Small deterministic generator (SplitMix64).
#[derive(Clone, Debug)]
struct Seeded(u64);

impl Seeded {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

impl SyntheticWorkspace {
    /// Builds a workspace of the given size.
    pub fn new(size: WorkloadSize) -> Self {
        let chat_roots = ChatRoots::from_home_info(Some(HOME), None, None, None)
            .expect("the synthetic home is a usable path");
        let mut projects = Vec::new();
        let mut worktrees = Vec::new();
        for (index, name) in PROJECT_NAMES.iter().enumerate() {
            let path = format!("{HOME}/projects/{name}");
            projects.push(ConfiguredProject {
                id: ProjectId::new(format!("project-{name}")),
                path: path.clone(),
            });
            let linked: Vec<WorktreeInfo> = (0..index % 3)
                .map(|worktree| WorktreeInfo {
                    path: format!("{HOME}/worktrees/{name}-feature-{worktree}"),
                    label: None,
                    name: Some(format!("{name}-feature-{worktree}")),
                    branch: Some(format!("feature/{name}-{worktree}")),
                })
                .collect();
            worktrees.push(ProjectWorktrees {
                project_path: path,
                worktrees: linked,
            });
        }

        let mut random = Seeded(0x0c4a_3b17_2301);
        let count = size.session_count();
        let mut sessions: Vec<SessionRecord> = Vec::with_capacity(count);
        for index in 0..count {
            let id = SessionId::new(format!("ses_{index:06}"));
            let created = BASE_TIME_MS - (index as i64) * 7 * MINUTE_MS;
            let updated = created + random.below(600) as i64 * MINUTE_MS;
            let directory = if random.chance(9) {
                format!(
                    "{}/2026-09-{:02}/session-{index:06}",
                    chat_roots.configured(),
                    1 + index % 28
                )
            } else if random.chance(3) {
                format!("{HOME}/scratch/{index}")
            } else {
                let project = random.below(PROJECT_NAMES.len());
                let linked = &worktrees[project].worktrees;
                if !linked.is_empty() && random.chance(25) {
                    linked[random.below(linked.len())].path.clone()
                } else {
                    projects[project].path.clone()
                }
            };
            // Recent sessions spawn subsessions in their parent's directory.
            let parent = (index > 3 && random.chance(14)).then(|| index - 1 - random.below(3));
            let (parent_id, directory) = match parent {
                Some(parent) => (
                    Some(sessions[parent].id.clone()),
                    sessions[parent].directory.clone().unwrap_or(directory),
                ),
                None => (None, directory),
            };
            let archived = (index > 60 && random.chance(22)).then_some(updated + MINUTE_MS);
            let overlay = SessionOverlay {
                // Three or so pins in the representative workload, as a person keeps them.
                pinned_at: (parent_id.is_none() && archived.is_none() && index % 757 == 24)
                    .then_some(updated),
                in_work: parent_id.is_none()
                    && archived.is_none()
                    && index < 400
                    && random.chance(4),
            };
            sessions.push(SessionRecord {
                id,
                parent_id,
                directory: Some(directory),
                title: title(index, &mut random),
                time: SessionTime {
                    created,
                    updated,
                    idle: Some(updated),
                    archived,
                },
                project_id: None,
                project: None,
                kind: Default::default(),
                overlay,
                model: None,
                agent: None,
            });
        }

        Self {
            projects,
            worktrees,
            sessions,
            chat_roots,
        }
    }
}

fn title(index: usize, random: &mut Seeded) -> String {
    let verb = TITLE_VERBS[random.below(TITLE_VERBS.len())];
    let subject = TITLE_SUBJECTS[random.below(TITLE_SUBJECTS.len())];
    match index % 29 {
        0 => format!("{verb} {subject}: ترتيب الجلسات"),
        7 => format!("{verb} {subject} — שיחה ארוכה"),
        13 => format!("{verb} {subject} 🧪 会话 #{index}"),
        _ => format!("{verb} {subject} #{index}"),
    }
}

/// Shapes of synthetic conversations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConversationShape {
    /// The 120-turn rich conversation: Markdown, code, tables, tools and
    /// mixed-direction Unicode.
    Rich,
    /// Two short turns.
    Short,
    /// Twelve turns alternating one-line and very tall replies.
    Mixed,
}

impl ConversationShape {
    pub fn turns(self) -> usize {
        match self {
            Self::Rich => 120,
            Self::Short => 2,
            Self::Mixed => 12,
        }
    }

    /// The shape a synthetic session's transcript takes.
    pub fn for_session(session_id: &SessionId) -> Self {
        let digits: usize = session_id
            .as_str()
            .bytes()
            .filter(u8::is_ascii_digit)
            .fold(0, |total, digit| total * 10 + usize::from(digit - b'0'));
        match digits % 3 {
            0 => Self::Rich,
            1 => Self::Short,
            _ => Self::Mixed,
        }
    }
}

/// Builds a session's synthetic transcript in server order.
pub fn conversation(session_id: &SessionId, shape: ConversationShape) -> Vec<Message> {
    let mut messages = Vec::with_capacity(shape.turns() * 2);
    for turn in 0..shape.turns() {
        let created = BASE_TIME_MS + (turn as i64) * MINUTE_MS;
        let user_id = MessageId::new(format!("{session_id}-msg-{:04}", turn * 2));
        messages.push(Message {
            info: MessageInfo {
                id: user_id.clone(),
                role: Role::User,
                created,
                completed: None,
                notice: None,
                assistant: None,
                compaction: None,
            },
            parts: vec![Part::text(
                PartId::new(format!("{user_id}-p0")),
                user_prompt(turn, shape),
            )],
        });
        let assistant_id = MessageId::new(format!("{session_id}-msg-{:04}", turn * 2 + 1));
        messages.push(Message {
            info: MessageInfo {
                id: assistant_id.clone(),
                role: Role::Assistant,
                created: created + 1_000,
                completed: Some(created + 30_000),
                notice: None,
                assistant: None,
                compaction: None,
            },
            parts: assistant_parts(&assistant_id, turn, shape),
        });
    }
    messages
}

fn user_prompt(turn: usize, shape: ConversationShape) -> String {
    match (shape, turn % 5) {
        (ConversationShape::Short, _) => format!("Short question {turn}?"),
        (_, 0) => format!("Turn {turn}: explain how the sidebar keeps unowned chats reachable."),
        (_, 1) => format!("Turn {turn}: اشرح كيف يحفظ المحرر المسودة عند التبديل."),
        (_, 2) => format!("Turn {turn}: show the Rust for a bounded event queue."),
        (_, 3) => format!("Turn {turn}: מה קורה כאשר הזרם נקטע באמצע?"),
        _ => format!("Turn {turn}: compare the two layouts in a table 📊"),
    }
}

fn assistant_parts(message_id: &MessageId, turn: usize, shape: ConversationShape) -> Vec<Part> {
    let part = |index: usize, content: PartContent| Part {
        id: PartId::new(format!("{message_id}-p{index}")),
        content,
        time: PartTime::default(),
    };
    match shape {
        ConversationShape::Short => vec![part(
            0,
            PartContent::Text {
                text: format!("A short answer for turn {turn}."),
            },
        )],
        ConversationShape::Mixed if turn.is_multiple_of(2) => vec![part(
            0,
            PartContent::Text {
                text: format!("One line, turn {turn}."),
            },
        )],
        ConversationShape::Mixed => vec![part(
            0,
            PartContent::Text {
                text: tall_reply(turn),
            },
        )],
        ConversationShape::Rich => {
            let mut parts = Vec::new();
            if turn.is_multiple_of(4) {
                parts.push(part(
                    parts.len(),
                    PartContent::Reasoning {
                        text: format!("Checking the ownership index before answering turn {turn}."),
                    },
                ));
            }
            if turn.is_multiple_of(3) {
                parts.push(part(
                    parts.len(),
                    PartContent::Tool {
                        tool: "read".to_owned(),
                        status: ToolStatus::Completed,
                        title: Some(format!("packages/ui/src/sync/turn-{turn}.ts")),
                        output: Some(format!("{} lines", 40 + turn)),
                        elided: None,
                    },
                ));
            }
            parts.push(part(
                parts.len(),
                PartContent::Text {
                    text: rich_reply(turn),
                },
            ));
            parts
        }
    }
}

fn rich_reply(turn: usize) -> String {
    match turn % 5 {
        0 => format!(
            "## Ownership, turn {turn}\n\nA root conversation stays in **Chats** until a configured \
             project owns its directory. Its descendants follow it, even across directories.\n\n\
             - Missing parents never promote children.\n- Archived parents never promote children.\n\
             - `/btw` forks stay out of every projection."
        ),
        1 => format!(
            "المسودة تبقى مع الجلسة التي كُتبت فيها، حتى بعد التبديل بين الجلسات. (turn {turn})\n\n\
             النص المختلط: the key is `DraftKey`, ثم يعود النص كما هو."
        ),
        2 => format!(
            "Here is a bounded queue for turn {turn}:\n\n```rust\nuse tokio::sync::mpsc;\n\n\
             let (sender, mut receiver) = mpsc::channel::<Event>(256);\n\
             while let Some(event) = receiver.recv().await {{\n    store.apply(event);\n}}\n```\n\n\
             The sender waits when the queue is full, so no event is dropped."
        ),
        3 => format!(
            "כאשר הזרם נקטע, המצב האחרון התקין נשמר והלקוח מתאם מחדש מהשרת. (turn {turn})\n\n\
             > The OpenCode stream is live-only; reconnecting does not replay missed events."
        ),
        _ => format!(
            "| Layout | Sidebar | Transcript |\n| --- | --- | --- |\n| Compact | 264 px | fluid |\n\
             | Wide | 320 px | fluid |\n\nTurn {turn} keeps both layouts 🧭 with the same tokens. \
             Combining marks: é ñ ü; ZWJ: 👩‍💻."
        ),
    }
}

fn tall_reply(turn: usize) -> String {
    let mut text = format!("### Tall reply {turn}\n\n");
    for line in 0..36 {
        text.push_str(&format!(
            "Line {line} of a deliberately tall reply that wraps across the transcript width so \
             anchors inside a single row can be restored at a nonzero offset.\n\n"
        ));
    }
    text
}

/// A deterministic assistant reply, streamed as fixed Unicode chunks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseScript {
    chunks: Vec<String>,
}

impl ResponseScript {
    /// The reply to the prompt numbered `sequence` in its session.
    pub fn new(sequence: usize) -> Self {
        let reply = format!(
            "Synthetic reply {sequence}. This development build has no provider; the text \
             streams from a fixed script.\n\nمرحبا بالعالم — שלום עולם — 你好世界 👋🏽\n\n\
             ```text\nchunk boundaries never split a character\n```\n\nDone."
        );
        Self {
            chunks: split_chunks(&reply, 6),
        }
    }

    pub fn chunks(&self) -> &[String] {
        &self.chunks
    }

    /// The full text once every chunk has played.
    pub fn text(&self) -> String {
        self.chunks.concat()
    }
}

/// Splits `text` into chunks of `size` grapheme-safe characters.
///
/// Splits only after characters that cannot start a combining sequence, so a
/// chunk never ends inside a ZWJ emoji or before a combining mark.
fn split_chunks(text: &str, size: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut count = 0;
    let characters: Vec<char> = text.chars().collect();
    for (index, character) in characters.iter().enumerate() {
        current.push(*character);
        count += 1;
        let next = characters.get(index + 1).copied();
        let joins = |c: char| {
            c == '\u{200d}'
                || ('\u{1f3fb}'..='\u{1f3ff}').contains(&c)
                || c == '\u{fe0f}'
                || ('\u{300}'..='\u{36f}').contains(&c)
        };
        let boundary = !joins(*character) && next.is_none_or(|next| !joins(next));
        if count >= size && boundary {
            chunks.push(std::mem::take(&mut current));
            count = 0;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workloads_have_their_documented_sizes_and_are_stable() {
        let first = SyntheticWorkspace::new(WorkloadSize::Representative);
        let second = SyntheticWorkspace::new(WorkloadSize::Representative);
        assert_eq!(first.sessions.len(), 2_301);
        assert_eq!(first.sessions, second.sessions);
        assert!(
            first
                .sessions
                .iter()
                .any(|session| session.parent_id.is_some())
        );
        assert!(first.sessions.iter().any(SessionRecord::is_archived));
        assert!(
            first
                .sessions
                .iter()
                .any(|session| first.chat_roots.contains(session.directory.as_deref()))
        );
    }

    #[test]
    fn the_rich_conversation_has_120_turns() {
        let messages = conversation(&SessionId::from("ses_000000"), ConversationShape::Rich);
        assert_eq!(messages.len(), 240);
    }

    #[test]
    fn response_chunks_reassemble_without_splitting_joined_emoji() {
        let script = ResponseScript::new(1);
        assert!(script.chunks().len() > 10);
        assert!(script.text().contains("👋🏽"));
        for chunk in script.chunks() {
            assert!(
                !chunk.starts_with('\u{1f3fd}'),
                "a skin-tone modifier must stay with its emoji"
            );
        }
    }
}
