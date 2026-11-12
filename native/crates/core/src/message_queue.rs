//! The server-owned message queue as the composer shows it, ported from
//! `stores/messageQueueStore.ts`.
//!
//! The OpenChamber server owns the queue (`web/server/lib/message-queue`)
//! and delivers it when a session goes idle; the client holds a projection
//! keyed by runtime, normalized directory and session. Every change the
//! server answers or broadcasts carries a revision, and a projection never
//! goes back to an older one. Nothing here is persisted: a stale local copy
//! would resurrect messages the server already delivered.

use std::collections::HashMap;
use std::fmt;

use crate::ids::{RuntimeId, SessionId};
use crate::path;
use crate::permission_mode::is_space_directory;

/// What Enter does in a busy session (`followUpBehavior`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FollowUpBehavior {
    /// Enter queues the message for when the turn ends.
    #[default]
    Queue,
    /// Enter steers the message into the running turn.
    Steer,
}

impl FollowUpBehavior {
    /// `normalizeFollowUpBehavior`: the setting, else the legacy
    /// `queueModeEnabled` boolean, else Queue. The removed `immediate` value
    /// was wire-identical to steering and reads as Steer.
    pub fn from_setting(value: Option<&str>, legacy_queue_mode: Option<bool>) -> Self {
        match value {
            Some("steer" | "immediate") => Self::Steer,
            Some("queue") => Self::Queue,
            _ => match legacy_queue_mode {
                Some(false) => Self::Steer,
                _ => Self::Queue,
            },
        }
    }

    /// The value the settings document stores.
    pub fn as_setting(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Steer => "steer",
        }
    }
}

/// A queued message's server-assigned identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QueuedItemId(String);

impl QueuedItemId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for QueuedItemId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The selection captured when the message was queued; delivery uses it as
/// is, never the composer's current one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedSendConfig {
    pub provider_id: String,
    pub model_id: String,
    pub agent: Option<String>,
    pub variant: Option<String>,
}

/// One queued message as a projection carries it: payloads (attachment data
/// and context) stay on the server until a take returns them.
#[derive(Clone, PartialEq, Eq)]
pub struct QueuedMessage {
    pub id: QueuedItemId,
    /// What the user typed, for display and editing.
    pub content: String,
    /// What is delivered.
    pub text: String,
    pub agent_mention: Option<String>,
    /// The attached files, in order, without their data.
    pub attachments: Vec<QueuedAttachment>,
    /// Whether the message carries context the server holds back.
    pub has_context: bool,
    /// A bounded display-only summary of that context.
    pub context_preview: Option<String>,
    pub created_at: i64,
    /// Absent only on a scheduled item that keeps the session's selection.
    pub send_config: Option<QueuedSendConfig>,
    /// Whether the message starts a goal when delivered.
    pub goal: bool,
    /// Whether a scheduled task queued it; those cannot be edited or sent.
    pub scheduled: bool,
    /// The durable enqueue request behind the item, when the server names
    /// one and holds it queued or uncertain (`queueDelivery`).
    pub delivery: Option<QueueDelivery>,
}

/// How the server holds a listed item's durable enqueue request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueDeliveryState {
    Queued,
    Uncertain,
}

/// The durable enqueue request behind a queued item, as its queue lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueDelivery {
    pub request: String,
    pub state: QueueDeliveryState,
}

/// What the projection says of one enqueue request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListedDelivery {
    /// Nothing was applied for the request's queue or its owner. The
    /// projection cannot tell a read that is on its way from one that failed
    /// or was never started; whoever reads the queue knows which.
    Unread,
    /// The queue was applied and no item names the request.
    Absent,
    Listed(QueueDeliveryState),
}

impl fmt::Debug for QueuedMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QueuedMessage")
            .field("id", &self.id)
            .field("attachments", &self.attachments.len())
            .field("scheduled", &self.scheduled)
            .finish_non_exhaustive()
    }
}

impl QueuedMessage {
    /// The row's one line (`getQueuedMessagePreview`): the typed text, else
    /// the context summary, else the first file name, cut at its first line
    /// and 100 characters with an ellipsis when anything was cut.
    pub fn preview(&self) -> String {
        let text = [
            self.content.trim(),
            self.context_preview.as_deref().unwrap_or_default().trim(),
            self.attachments
                .first()
                .map(|file| file.filename.as_str())
                .unwrap_or_default(),
        ]
        .into_iter()
        .find(|text| !text.is_empty())
        .unwrap_or_default();
        let first_line = text.split('\n').next().unwrap_or_default();
        // JavaScript slices UTF-16 units.
        let units: Vec<u16> = first_line.encode_utf16().take(100).collect();
        let cut = String::from_utf16_lossy(&units);
        let first_units = first_line.encode_utf16().count();
        if text.len() > first_line.len() || first_units > 100 {
            format!("{cut}...")
        } else {
            cut
        }
    }

    /// Whether this client may take the message out of the queue to send it
    /// itself. A scheduled message belongs to its task, and a goal message
    /// would lose its goal, since native cannot start one; an attachment
    /// whose reference would exceed `reference_limit` bytes could not be
    /// journaled. Each stays for the server to deliver.
    pub fn takeable(&self, reference_limit: usize) -> bool {
        !self.scheduled
            && !self.goal
            && self
                .attachments
                .iter()
                .all(|file| file.reference_bound() <= reference_limit)
    }

    /// Whether a sealed payload transfer can preserve this item's ownership.
    /// File bytes and rich context travel separately from the native journal.
    pub fn payload_takeable(&self) -> bool {
        !self.scheduled && !self.goal && self.send_config.is_some()
    }

    /// Whether a take can bring the whole message back into the composer:
    /// text only, since the native composer has no attachment or context chips.
    pub fn editable_as_text(&self) -> bool {
        !self.scheduled && !self.goal && self.attachments.is_empty() && !self.has_context
    }
}

/// Where an attachment's bytes live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentSource {
    Local,
    Server,
    Vscode,
}

/// A queued attachment as a projection carries it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedAttachment {
    pub filename: String,
    pub source: AttachmentSource,
    /// The file's size in bytes.
    pub size: u64,
    /// The server path a server attachment is referenced by.
    pub server_path: Option<String>,
}

/// The room a data URL's media type and markers take, beyond the payload.
const DATA_URL_OVERHEAD: u64 = 256;

impl QueuedAttachment {
    /// An upper bound on the length of the reference a send carries: a
    /// `file://` URL with every byte percent-encoded for a server file, else
    /// a base64 data URL of the whole file.
    pub fn reference_bound(&self) -> usize {
        let bound = match (&self.server_path, self.source) {
            (Some(path), AttachmentSource::Server) if !path.is_empty() => {
                "file:///".len() as u64 + 3 * path.len() as u64
            }
            _ => self
                .size
                .div_ceil(3)
                .saturating_mul(4)
                .saturating_add(DATA_URL_OVERHEAD),
        };
        usize::try_from(bound).unwrap_or(usize::MAX)
    }
}

/// One session's queue as the server reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueSession {
    pub session: SessionId,
    pub directory: String,
    pub items: Vec<QueuedMessage>,
    /// The item the server is delivering right now.
    pub sending: Option<QueuedItemId>,
}

/// The isolated space (`/spaces/<id>`) whose own server owns a directory's
/// queue, or `None` for the host (`spaceIdOfDirectory`).
pub fn queue_owner(directory: &str) -> Option<&str> {
    is_space_directory(directory).then(|| &directory["/spaces/".len().."/spaces/".len() + 12])
}

/// Where a queue lives in the projection.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct QueueKey {
    pub runtime: RuntimeId,
    pub directory: String,
    pub session: SessionId,
}

impl QueueKey {
    /// `createMessageQueueTarget`: `None` for an unusable directory.
    pub fn new(runtime: RuntimeId, directory: &str, session: SessionId) -> Option<Self> {
        let directory = path::normalize(directory)?;
        (!session.as_str().is_empty()).then_some(Self {
            runtime,
            directory,
            session,
        })
    }

    /// The server that owns this queue: a space, or the host.
    pub fn owner(&self) -> Option<&str> {
        queue_owner(&self.directory)
    }
}

/// `MAX_MESSAGES_PER_QUEUE` on the server.
pub const MAX_MESSAGES_PER_QUEUE: usize = 20;

/// The projection of every queue the client has seen.
#[derive(Debug, Default)]
pub struct MessageQueue {
    queues: HashMap<QueueKey, Vec<QueuedMessage>>,
    sending: HashMap<QueueKey, QueuedItemId>,
    /// The revision last applied per queue; older answers are ignored.
    applied: HashMap<QueueKey, u64>,
    /// The revision of the last complete snapshot per runtime and owner.
    snapshots: HashMap<(RuntimeId, Option<String>), u64>,
}

impl MessageQueue {
    /// The queue at `key`, oldest first.
    pub fn queue(&self, key: &QueueKey) -> &[QueuedMessage] {
        self.queues.get(key).map_or(&[], Vec::as_slice)
    }

    /// The item the server is delivering for `key`.
    pub fn sending(&self, key: &QueueKey) -> Option<&QueuedItemId> {
        self.sending.get(key)
    }

    /// How the queue at `key` lists the enqueue request `request`.
    pub fn listed_delivery(&self, key: &QueueKey, request: &str) -> ListedDelivery {
        let listed = self
            .queue(key)
            .iter()
            .filter_map(|message| message.delivery.as_ref())
            .find(|delivery| delivery.request == request);
        if let Some(delivery) = listed {
            return ListedDelivery::Listed(delivery.state);
        }
        let owner = (key.runtime.clone(), key.owner().map(str::to_owned));
        if self.applied.contains_key(key) || self.snapshots.contains_key(&owner) {
            ListedDelivery::Absent
        } else {
            ListedDelivery::Unread
        }
    }

    /// Applies one session's authoritative state from `owner` (a space, or
    /// the host), answered or broadcast at `revision`. A session the owner
    /// may not speak for, or an answer older than one applied, changes
    /// nothing. Returns whether the projection changed.
    pub fn apply_session(
        &mut self,
        runtime: &RuntimeId,
        owner: Option<&str>,
        session: QueueSession,
        revision: u64,
    ) -> bool {
        if queue_owner(&session.directory) != owner {
            return false;
        }
        let owner_key = (runtime.clone(), owner.map(str::to_owned));
        if self
            .snapshots
            .get(&owner_key)
            .is_some_and(|applied| *applied > revision)
        {
            return false;
        }
        let Some(key) = QueueKey::new(runtime.clone(), &session.directory, session.session.clone())
        else {
            // Servers before 1.22.2 drop the directory of an emptied queue; a
            // session ID is unique across directories, so it still names the
            // projection that is done.
            return session.items.is_empty()
                && self.clear_session(runtime, &session.session, revision);
        };
        if self
            .applied
            .get(&key)
            .is_some_and(|applied| *applied > revision)
        {
            return false;
        }
        self.applied.insert(key.clone(), revision);
        let changed = self.queue(&key) != session.items.as_slice()
            || self.sending.get(&key) != session.sending.as_ref();
        if session.items.is_empty() {
            self.queues.remove(&key);
        } else {
            self.queues.insert(key.clone(), session.items);
        }
        match session.sending {
            Some(id) => self.sending.insert(key, id),
            None => self.sending.remove(&key),
        };
        changed
    }

    /// Applies a complete snapshot of `owner`'s queues: it replaces that
    /// owner's part of the projection, except queues changed after it.
    /// Returns false for a snapshot older than one already applied.
    pub fn apply_snapshot(
        &mut self,
        runtime: &RuntimeId,
        owner: Option<&str>,
        revision: u64,
        sessions: Vec<QueueSession>,
    ) -> bool {
        let owner_key = (runtime.clone(), owner.map(str::to_owned));
        if self
            .snapshots
            .get(&owner_key)
            .is_some_and(|applied| *applied > revision)
        {
            return false;
        }
        self.snapshots.insert(owner_key, revision);
        let newer = |applied: &HashMap<QueueKey, u64>, key: &QueueKey| {
            applied.get(key).is_some_and(|applied| *applied > revision)
        };
        let owned = |key: &QueueKey| &key.runtime == runtime && key.owner() == owner;
        let stale: Vec<QueueKey> = self
            .queues
            .keys()
            .chain(self.sending.keys())
            .filter(|key| owned(key) && !newer(&self.applied, key))
            .cloned()
            .collect();
        for key in stale {
            self.queues.remove(&key);
            self.sending.remove(&key);
        }
        for session in sessions {
            if queue_owner(&session.directory) != owner {
                continue;
            }
            let Some(key) =
                QueueKey::new(runtime.clone(), &session.directory, session.session.clone())
            else {
                continue;
            };
            if newer(&self.applied, &key) {
                continue;
            }
            self.applied.insert(key.clone(), revision);
            if !session.items.is_empty() {
                self.queues.insert(key.clone(), session.items);
            }
            if let Some(id) = session.sending {
                self.sending.insert(key, id);
            }
        }
        true
    }

    /// Every projection of `session` in `runtime`, whatever directory it was
    /// keyed under, unless a newer revision already applied there.
    fn clear_session(&mut self, runtime: &RuntimeId, session: &SessionId, revision: u64) -> bool {
        let keys: Vec<QueueKey> = self
            .queues
            .keys()
            .chain(self.sending.keys())
            .filter(|key| &key.runtime == runtime && &key.session == session)
            .cloned()
            .collect();
        let mut changed = false;
        for key in keys {
            if self
                .applied
                .get(&key)
                .is_some_and(|applied| *applied > revision)
            {
                continue;
            }
            self.applied.insert(key.clone(), revision);
            changed |= self.queues.remove(&key).is_some();
            changed |= self.sending.remove(&key).is_some();
        }
        changed
    }

    /// Drops a deleted session's projection without a server call
    /// (`forgetQueue`).
    pub fn forget_session(&mut self, runtime: &RuntimeId, session: &SessionId) -> bool {
        let matches = |key: &QueueKey| &key.runtime == runtime && &key.session == session;
        let before = self.queues.len() + self.sending.len();
        self.queues.retain(|key, _| !matches(key));
        self.sending.retain(|key, _| !matches(key));
        self.applied.retain(|key, _| !matches(key));
        before != self.queues.len() + self.sending.len()
    }

    /// Forgets everything of `runtime`; switching back reads it again from
    /// its server (`resetForRuntimeSwitch`).
    pub fn reset_runtime(&mut self, runtime: &RuntimeId) {
        self.queues.retain(|key, _| &key.runtime != runtime);
        self.sending.retain(|key, _| &key.runtime != runtime);
        self.applied.retain(|key, _| &key.runtime != runtime);
        self.snapshots
            .retain(|(owner_runtime, _), _| owner_runtime != runtime);
    }

    /// Removes one message before the server answers (`removeFromQueue`);
    /// the answer, or a reread after a failure, replaces the queue.
    pub fn remove_local(&mut self, key: &QueueKey, id: &QueuedItemId) -> bool {
        let Some(queue) = self.queues.get_mut(key) else {
            return false;
        };
        let before = queue.len();
        queue.retain(|message| &message.id != id);
        let removed = queue.len() != before;
        if queue.is_empty() {
            self.queues.remove(key);
        }
        removed
    }

    /// Moves `from` to where `to` is (`reorderQueue`, an `arrayMove`) and
    /// returns the whole new order for the server, or `None` when either is
    /// not queued or they are the same.
    pub fn reorder_local(
        &mut self,
        key: &QueueKey,
        from: &QueuedItemId,
        to: &QueuedItemId,
    ) -> Option<Vec<QueuedItemId>> {
        if from == to {
            return None;
        }
        let queue = self.queues.get_mut(key)?;
        let from_index = queue.iter().position(|message| &message.id == from)?;
        let to_index = queue.iter().position(|message| &message.id == to)?;
        let moved = queue.remove(from_index);
        queue.insert(to_index, moved);
        Some(queue.iter().map(|message| message.id.clone()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> RuntimeId {
        RuntimeId::from("runtime")
    }

    fn message(id: &str, content: &str) -> QueuedMessage {
        QueuedMessage {
            id: QueuedItemId::new(id),
            content: content.to_owned(),
            text: content.to_owned(),
            agent_mention: None,
            attachments: Vec::new(),
            has_context: false,
            context_preview: None,
            created_at: 1,
            send_config: None,
            goal: false,
            scheduled: false,
            delivery: None,
        }
    }

    fn session(directory: &str, ids: &[&str]) -> QueueSession {
        QueueSession {
            session: SessionId::from("ses_1"),
            directory: directory.to_owned(),
            items: ids.iter().map(|id| message(id, id)).collect(),
            sending: None,
        }
    }

    fn key(directory: &str) -> QueueKey {
        QueueKey::new(runtime(), directory, SessionId::from("ses_1")).expect("valid key")
    }

    fn ids(queue: &MessageQueue, directory: &str) -> Vec<String> {
        queue
            .queue(&key(directory))
            .iter()
            .map(|message| message.id.as_str().to_owned())
            .collect()
    }

    #[test]
    fn a_request_is_unread_then_listed_then_absent() {
        let mut queue = MessageQueue::default();
        let key = key("/repo");
        assert_eq!(
            queue.listed_delivery(&key, "request"),
            ListedDelivery::Unread
        );
        let listed = |state| {
            let mut session = session("/repo", &["a", "b"]);
            session.items[1].delivery = Some(QueueDelivery {
                request: "request".to_owned(),
                state,
            });
            session
        };
        assert!(queue.apply_session(&runtime(), None, listed(QueueDeliveryState::Queued), 1));
        assert_eq!(
            queue.listed_delivery(&key, "request"),
            ListedDelivery::Listed(QueueDeliveryState::Queued)
        );
        assert_eq!(
            queue.listed_delivery(&key, "another"),
            ListedDelivery::Absent,
            "the applied queue names no such request"
        );
        assert!(queue.apply_session(&runtime(), None, listed(QueueDeliveryState::Uncertain), 2));
        assert_eq!(
            queue.listed_delivery(&key, "request"),
            ListedDelivery::Listed(QueueDeliveryState::Uncertain)
        );
        assert!(queue.apply_session(&runtime(), None, session("/repo", &[]), 3));
        assert_eq!(
            queue.listed_delivery(&key, "request"),
            ListedDelivery::Absent,
            "the item left its queue"
        );
        let mut snapshot = MessageQueue::default();
        assert!(snapshot.apply_snapshot(&runtime(), None, 1, Vec::new()));
        assert_eq!(
            snapshot.listed_delivery(&key, "request"),
            ListedDelivery::Absent,
            "a complete snapshot of the owner lists no such queue"
        );
    }

    #[test]
    fn older_answers_never_replace_newer_ones() {
        let mut queue = MessageQueue::default();
        assert!(queue.apply_session(&runtime(), None, session("/repo/", &["a", "b"]), 5));
        assert!(!queue.apply_session(&runtime(), None, session("/repo", &["a"]), 4));
        assert_eq!(
            ids(&queue, "/repo"),
            ["a", "b"],
            "the trailing slash keys the same queue"
        );
        assert!(queue.apply_session(&runtime(), None, session("/repo", &[]), 6));
        assert!(queue.queue(&key("/repo")).is_empty());
    }

    #[test]
    fn an_owner_speaks_only_for_its_own_directories() {
        let mut queue = MessageQueue::default();
        let space = "/spaces/0123456789ab/repo";
        assert!(!queue.apply_session(&runtime(), None, session(space, &["a"]), 1));
        assert!(queue.apply_session(&runtime(), Some("0123456789ab"), session(space, &["a"]), 1));
        assert_eq!(key(space).owner(), Some("0123456789ab"));
        assert_eq!(queue_owner("/spaces/xyz/repo"), None, "not a space ID");
    }

    #[test]
    fn a_snapshot_replaces_its_owner_and_keeps_newer_broadcasts() {
        let mut queue = MessageQueue::default();
        queue.apply_session(&runtime(), None, session("/gone", &["x"]), 1);
        queue.apply_session(&runtime(), None, session("/kept", &["y"]), 9);
        let space = "/spaces/0123456789ab/repo";
        queue.apply_session(&runtime(), Some("0123456789ab"), session(space, &["s"]), 1);
        assert!(queue.apply_snapshot(&runtime(), None, 5, vec![session("/new", &["n"])]));
        assert!(ids(&queue, "/gone").is_empty(), "the snapshot omits it");
        assert_eq!(ids(&queue, "/kept"), ["y"], "changed after the snapshot");
        assert_eq!(ids(&queue, "/new"), ["n"]);
        assert_eq!(ids(&queue, space), ["s"], "another owner's queue stays");
        assert!(
            !queue.apply_snapshot(&runtime(), None, 4, Vec::new()),
            "older snapshot"
        );
        assert!(
            !queue.apply_session(&runtime(), None, session("/late", &["l"]), 3),
            "a broadcast older than the snapshot"
        );
    }

    #[test]
    fn an_empty_session_without_a_directory_clears_every_projection() {
        let mut queue = MessageQueue::default();
        queue.apply_session(&runtime(), None, session("/a", &["x"]), 1);
        assert!(queue.apply_session(&runtime(), None, session("", &[]), 2));
        assert!(queue.queue(&key("/a")).is_empty());
        assert!(!queue.apply_session(&runtime(), None, session("", &["y"]), 3));
    }

    #[test]
    fn local_edits_and_runtime_resets() {
        let mut queue = MessageQueue::default();
        queue.apply_session(&runtime(), None, session("/a", &["1", "2", "3"]), 1);
        let order = queue
            .reorder_local(&key("/a"), &QueuedItemId::new("3"), &QueuedItemId::new("1"))
            .expect("both queued");
        assert_eq!(
            order.iter().map(QueuedItemId::as_str).collect::<Vec<_>>(),
            ["3", "1", "2"]
        );
        assert!(queue.remove_local(&key("/a"), &QueuedItemId::new("1")));
        assert_eq!(ids(&queue, "/a"), ["3", "2"]);
        queue.reset_runtime(&runtime());
        assert!(queue.queue(&key("/a")).is_empty());
        assert!(
            queue.apply_session(&runtime(), None, session("/a", &["9"]), 0),
            "revisions start over after a reset"
        );
        assert!(queue.forget_session(&runtime(), &SessionId::from("ses_1")));
    }

    #[test]
    fn payload_takes_keep_rich_items_and_require_their_captured_configuration() {
        let mut item = message("rich", "display text");
        item.text = "delivered text".to_owned();
        item.has_context = true;
        item.attachments.push(QueuedAttachment {
            filename: "large.bin".to_owned(),
            source: AttachmentSource::Local,
            size: 20 * 1024 * 1024,
            server_path: None,
        });
        assert!(
            !item.payload_takeable(),
            "a take cannot invent missing send configuration"
        );
        item.send_config = Some(QueuedSendConfig {
            provider_id: "provider".to_owned(),
            model_id: "model".to_owned(),
            agent: Some("build".to_owned()),
            variant: None,
        });
        assert!(
            item.payload_takeable(),
            "payload transfers retain rich content outside the journal"
        );
        assert!(
            !item.takeable(64 * 1024),
            "legacy URI limits do not govern payload transfers"
        );
        item.goal = true;
        assert!(
            !item.payload_takeable(),
            "goal ownership stays with the server"
        );
        item.goal = false;
        item.scheduled = true;
        assert!(
            !item.payload_takeable(),
            "scheduled ownership stays with its task"
        );
    }

    #[test]
    fn goal_scheduled_and_unjournalable_messages_stay_for_the_server() {
        let mut item = message("a", "text");
        assert!(item.takeable(64 * 1024) && item.editable_as_text());
        item.goal = true;
        assert!(!item.takeable(64 * 1024), "a goal would be lost");
        assert!(!item.editable_as_text());
        item.goal = false;
        let inline = |size| QueuedAttachment {
            filename: "a.png".to_owned(),
            source: AttachmentSource::Local,
            size,
            server_path: None,
        };
        item.attachments = vec![inline(1024)];
        assert!(item.takeable(64 * 1024));
        item.attachments = vec![inline(48 * 1024)];
        assert!(
            !item.takeable(64 * 1024),
            "its data URL exceeds the journal's limit"
        );
        item.attachments = vec![QueuedAttachment {
            source: AttachmentSource::Server,
            server_path: Some("/repo/big.bin".to_owned()),
            ..inline(1 << 30)
        }];
        assert!(
            item.takeable(64 * 1024),
            "a server file is referenced by path"
        );
        item.scheduled = true;
        assert!(!item.takeable(64 * 1024));
    }

    #[test]
    fn follow_up_behavior_reads_like_the_web() {
        assert_eq!(
            FollowUpBehavior::from_setting(Some("immediate"), None),
            FollowUpBehavior::Steer
        );
        assert_eq!(
            FollowUpBehavior::from_setting(None, Some(false)),
            FollowUpBehavior::Steer
        );
        assert_eq!(
            FollowUpBehavior::from_setting(Some("bogus"), Some(true)),
            FollowUpBehavior::Queue
        );
        assert_eq!(FollowUpBehavior::default().as_setting(), "queue");
    }

    #[test]
    fn previews_cut_at_the_first_line_and_100_units() {
        let mut item = message("a", "first\nsecond");
        assert_eq!(item.preview(), "first...");
        item.content = "x".repeat(101);
        assert_eq!(item.preview(), format!("{}...", "x".repeat(100)));
        item.content = "  ".to_owned();
        item.attachments = vec![QueuedAttachment {
            filename: "image.png".to_owned(),
            source: AttachmentSource::Local,
            size: 3,
            server_path: None,
        }];
        assert_eq!(item.preview(), "image.png");
        assert!(!item.editable_as_text());
    }
}
