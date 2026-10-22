//! The global session index: every active and archived session the server
//! listed, reconciled with the live session events.
//!
//! Ports the reconciliation of `packages/ui/src/stores/useGlobalSessionsStore.ts`
//! and `packages/ui/src/sync/session-event-router.ts`. Every applied change
//! records a mutation revision, removals included even when the session was
//! already absent. A load captures the revision it started at, and its result
//! never overrides a session changed after that: a delayed page cannot
//! restore a deleted session or move one back to its former directory.

use std::collections::HashMap;

use crate::ids::SessionId;
use crate::path;
use crate::session::{SessionPatch, SessionRecord};

/// A load of the whole list. Only the newest load may complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub struct LoadTicket {
    generation: u64,
    baseline: u64,
}

/// Where the list stands with the server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IndexStatus {
    #[default]
    Idle,
    Loading,
    /// The last load completed; the list covers every session.
    Ready,
    /// The last load failed; the list keeps what earlier reads and events
    /// established.
    Failed,
}

/// A change the server announced for one session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionChange {
    Created(SessionRecord),
    Patched {
        session_id: SessionId,
        patch: SessionPatch,
    },
    Deleted(SessionId),
    /// The session's turn ended: a deferred recency update applies now.
    Settled(SessionId),
}

/// A session that moved to another directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionMove {
    pub session_id: SessionId,
    pub directory: String,
}

/// What a change to the index did.
#[derive(Debug, Default, PartialEq, Eq)]
#[must_use]
pub struct IndexUpdate {
    /// The active or archived list changed.
    pub changed: bool,
    /// Sessions whose directory changed, with their destination.
    pub moves: Vec<SessionMove>,
}

impl IndexUpdate {
    fn merge(&mut self, other: Self) {
        self.changed |= other.changed;
        self.moves.extend(other.moves);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bucket {
    Active,
    Archived,
}

/// Every session the server knows, split by archive state.
#[derive(Debug, Default)]
pub struct SessionIndex {
    active: Vec<SessionRecord>,
    archived: Vec<SessionRecord>,
    slots: HashMap<SessionId, (Bucket, usize)>,
    mutation_revision: u64,
    revision_by_session: HashMap<SessionId, u64>,
    /// Recency-only updates held until the session's turn ends, so token
    /// traffic never rebuilds the sidebar.
    deferred: HashMap<SessionId, SessionRecord>,
    generation: u64,
    status: IndexStatus,
    has_loaded: bool,
}

impl SessionIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Active sessions, the most recently added first.
    pub fn active(&self) -> &[SessionRecord] {
        &self.active
    }

    pub fn archived(&self) -> &[SessionRecord] {
        &self.archived
    }

    pub fn get(&self, id: &SessionId) -> Option<&SessionRecord> {
        let (bucket, index) = *self.slots.get(id)?;
        self.list(bucket).get(index)
    }

    pub fn status(&self) -> IndexStatus {
        self.status
    }

    /// Whether a complete load ever succeeded, which grants full-list
    /// authority.
    pub fn has_loaded(&self) -> bool {
        self.has_loaded
    }

    /// Starts a load of the whole list. A later `begin_load` makes this one
    /// obsolete.
    pub fn begin_load(&mut self) -> LoadTicket {
        self.generation += 1;
        self.status = IndexStatus::Loading;
        LoadTicket {
            generation: self.generation,
            baseline: self.mutation_revision,
        }
    }

    /// Merges one page of a load in flight. The page has no authority to
    /// remove sessions; sessions changed since the load began keep their
    /// current state.
    pub fn merge_page(&mut self, ticket: LoadTicket, page: Vec<SessionRecord>) -> IndexUpdate {
        if ticket.generation != self.generation {
            return IndexUpdate::default();
        }
        let (archived, active): (Vec<SessionRecord>, Vec<SessionRecord>) =
            page.into_iter().partition(SessionRecord::is_archived);
        let incoming_active: Vec<&SessionId> = active.iter().map(|session| &session.id).collect();
        let incoming_archived: Vec<&SessionId> =
            archived.iter().map(|session| &session.id).collect();
        let mut next_active = merge_lists(&self.active, &active);
        next_active.retain(|session| !incoming_archived.contains(&&session.id));
        let mut next_archived = merge_lists(&self.archived, &archived);
        next_archived.retain(|session| !incoming_active.contains(&&session.id));
        let (next_active, next_archived) =
            self.overlay_mutations_since(next_active, next_archived, ticket.baseline);
        self.install(next_active, next_archived)
    }

    /// Completes a load with every session the server listed. Sessions
    /// changed since the load began keep their current state.
    pub fn complete_load(
        &mut self,
        ticket: LoadTicket,
        sessions: Vec<SessionRecord>,
    ) -> IndexUpdate {
        if ticket.generation != self.generation {
            return IndexUpdate::default();
        }
        let (archived, active): (Vec<SessionRecord>, Vec<SessionRecord>) =
            sessions.into_iter().partition(SessionRecord::is_archived);
        let (active, archived) = self.overlay_mutations_since(active, archived, ticket.baseline);
        self.status = IndexStatus::Ready;
        self.has_loaded = true;
        self.install(active, archived)
    }

    /// Records a failed load; the list keeps its records.
    pub fn fail_load(&mut self, ticket: LoadTicket) {
        if ticket.generation == self.generation {
            self.status = IndexStatus::Failed;
        }
    }

    /// Applies live session changes in order.
    pub fn apply(&mut self, changes: impl IntoIterator<Item = SessionChange>) -> IndexUpdate {
        let mut update = IndexUpdate::default();
        for change in changes {
            update.merge(self.apply_one(change));
        }
        update
    }

    fn apply_one(&mut self, change: SessionChange) -> IndexUpdate {
        match change {
            SessionChange::Created(session) => {
                if self
                    .get(&session.id)
                    .is_some_and(|current| session.recency() < current.recency())
                {
                    return IndexUpdate::default();
                }
                self.upsert(session)
            }
            SessionChange::Patched { session_id, patch } => {
                // A partial patch cannot stand in for a record never listed.
                let Some(current) = self.get(&session_id) else {
                    return IndexUpdate::default();
                };
                let next = current.patched(&patch);
                if next.recency() < current.recency() {
                    return IndexUpdate::default();
                }
                if is_recency_only(current, &next) {
                    self.deferred.insert(session_id, next);
                    return IndexUpdate::default();
                }
                let moved = patch
                    .directory
                    .as_ref()
                    .filter(|directory| path::normalize(directory) != current.resolved_directory());
                let moves = moved
                    .map(|directory| SessionMove {
                        session_id: session_id.clone(),
                        directory: directory.clone(),
                    })
                    .into_iter()
                    .collect();
                self.deferred.remove(&session_id);
                let mut update = self.upsert(next);
                update.moves = moves;
                update
            }
            SessionChange::Deleted(session_id) => {
                self.deferred.remove(&session_id);
                self.record_mutation(&session_id);
                let Some((bucket, index)) = self.slots.get(&session_id).copied() else {
                    return IndexUpdate::default();
                };
                self.list_mut(bucket).remove(index);
                self.reindex();
                IndexUpdate {
                    changed: true,
                    moves: Vec::new(),
                }
            }
            SessionChange::Settled(session_id) => {
                let Some(deferred) = self.deferred.remove(&session_id) else {
                    return IndexUpdate::default();
                };
                let Some(current) = self.get(&session_id) else {
                    return IndexUpdate::default();
                };
                if deferred.recency() < current.recency() || !is_recency_only(current, &deferred) {
                    return IndexUpdate::default();
                }
                self.upsert(deferred)
            }
        }
    }

    /// Inserts or replaces a record, moving it between the active and
    /// archived lists when its archive state changed.
    fn upsert(&mut self, session: SessionRecord) -> IndexUpdate {
        self.record_mutation(&session.id);
        let session = match self.get(&session.id) {
            Some(current) => keep_directory_metadata(session, current),
            None => session,
        };
        let bucket = if session.is_archived() {
            Bucket::Archived
        } else {
            Bucket::Active
        };
        match self.slots.get(&session.id).copied() {
            Some((held_bucket, index)) if held_bucket == bucket => {
                if self.list(bucket)[index] == session {
                    return IndexUpdate::default();
                }
                self.list_mut(bucket)[index] = session;
            }
            Some((held_bucket, index)) => {
                self.list_mut(held_bucket).remove(index);
                self.list_mut(bucket).insert(0, session);
                self.reindex();
            }
            None => {
                self.list_mut(bucket).insert(0, session);
                self.reindex();
            }
        }
        IndexUpdate {
            changed: true,
            moves: Vec::new(),
        }
    }

    fn record_mutation(&mut self, id: &SessionId) {
        self.mutation_revision += 1;
        self.revision_by_session
            .insert(id.clone(), self.mutation_revision);
    }

    /// Replaces a load's version of every session changed after `baseline`
    /// with the current one, or leaves it out when the session is gone.
    fn overlay_mutations_since(
        &self,
        mut active: Vec<SessionRecord>,
        mut archived: Vec<SessionRecord>,
        baseline: u64,
    ) -> (Vec<SessionRecord>, Vec<SessionRecord>) {
        let affected: Vec<&SessionId> = self
            .revision_by_session
            .iter()
            .filter(|(_, revision)| **revision > baseline)
            .map(|(id, _)| id)
            .collect();
        if affected.is_empty() {
            return (active, archived);
        }
        active.retain(|session| !affected.contains(&&session.id));
        archived.retain(|session| !affected.contains(&&session.id));
        for id in affected {
            let Some((bucket, index)) = self.slots.get(id).copied() else {
                continue;
            };
            let current = self.list(bucket)[index].clone();
            match bucket {
                Bucket::Active => upsert_into(&mut active, current),
                Bucket::Archived => upsert_into(&mut archived, current),
            }
        }
        (active, archived)
    }

    fn install(&mut self, active: Vec<SessionRecord>, archived: Vec<SessionRecord>) -> IndexUpdate {
        let changed = active != self.active || archived != self.archived;
        if changed {
            self.active = active;
            self.archived = archived;
            self.reindex();
        }
        IndexUpdate {
            changed,
            moves: Vec::new(),
        }
    }

    fn list(&self, bucket: Bucket) -> &Vec<SessionRecord> {
        match bucket {
            Bucket::Active => &self.active,
            Bucket::Archived => &self.archived,
        }
    }

    fn list_mut(&mut self, bucket: Bucket) -> &mut Vec<SessionRecord> {
        match bucket {
            Bucket::Active => &mut self.active,
            Bucket::Archived => &mut self.archived,
        }
    }

    fn reindex(&mut self) {
        self.slots.clear();
        for (index, session) in self.active.iter().enumerate() {
            self.slots
                .insert(session.id.clone(), (Bucket::Active, index));
        }
        for (index, session) in self.archived.iter().enumerate() {
            self.slots
                .insert(session.id.clone(), (Bucket::Archived, index));
        }
    }
}

/// `isGlobalSessionRecencyOnlyUpdate`: only the update time moved.
fn is_recency_only(current: &SessionRecord, next: &SessionRecord) -> bool {
    let merged = keep_directory_metadata(next.clone(), current);
    current.time.updated != merged.time.updated && current.same_structure(&merged)
}

/// `mergeSessionDirectoryMetadata`: a record without a directory or project
/// worktree keeps the ones already known, unless the project identities
/// conflict.
fn keep_directory_metadata(mut incoming: SessionRecord, existing: &SessionRecord) -> SessionRecord {
    if path::normalize_optional(incoming.directory.as_deref()).is_none()
        && path::normalize_optional(existing.directory.as_deref()).is_some()
    {
        incoming.directory.clone_from(&existing.directory);
    }
    let conflict = matches!(
        (incoming.opencode_project_id(), existing.opencode_project_id()),
        (Some(incoming), Some(existing)) if incoming != existing
    );
    let incoming_worktree = incoming
        .project
        .as_ref()
        .and_then(|project| path::normalize_optional(project.worktree.as_deref()));
    let existing_worktree = existing
        .project
        .as_ref()
        .and_then(|project| path::normalize_optional(project.worktree.as_deref()));
    if !conflict && incoming_worktree.is_none() && existing_worktree.is_some() {
        let mut project = existing.project.clone().unwrap_or_default();
        if let Some(id) = incoming
            .project
            .as_ref()
            .and_then(|project| project.id.clone())
        {
            project.id = Some(id);
        }
        incoming.project = Some(project);
    }
    incoming
}

/// `mergeSessionLists`: existing order with records replaced, then records
/// the page adds.
fn merge_lists(existing: &[SessionRecord], incoming: &[SessionRecord]) -> Vec<SessionRecord> {
    if incoming.is_empty() {
        return existing.to_vec();
    }
    let by_id: HashMap<&SessionId, &SessionRecord> = incoming
        .iter()
        .map(|session| (&session.id, session))
        .collect();
    let mut merged: Vec<SessionRecord> = existing
        .iter()
        .map(|session| match by_id.get(&session.id) {
            Some(next) => keep_directory_metadata((*next).clone(), session),
            None => session.clone(),
        })
        .collect();
    let held: Vec<&SessionId> = existing.iter().map(|session| &session.id).collect();
    for session in incoming {
        if !held.contains(&&session.id) {
            merged.push(session.clone());
        }
    }
    merged
}

/// `upsertSessionIntoList`: replaces in place, or adds at the front.
fn upsert_into(list: &mut Vec<SessionRecord>, session: SessionRecord) {
    match list.iter().position(|candidate| candidate.id == session.id) {
        Some(index) => {
            let merged = keep_directory_metadata(session, &list[index]);
            list[index] = merged;
        }
        None => list.insert(0, session),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionTime;

    fn session(id: &str, directory: &str, updated: i64) -> SessionRecord {
        SessionRecord {
            time: SessionTime {
                created: 1,
                updated,
                idle: None,
                archived: None,
            },
            ..SessionRecord::new(id, directory)
        }
    }

    fn ids(sessions: &[SessionRecord]) -> Vec<&str> {
        sessions.iter().map(|session| session.id.as_str()).collect()
    }

    #[test]
    fn a_delayed_snapshot_cannot_restore_a_deleted_session_or_undo_a_move() {
        let mut index = SessionIndex::new();
        let first = index.begin_load();
        let _ = index.complete_load(first, vec![session("a", "/p", 1), session("b", "/p", 1)]);

        let ticket = index.begin_load();
        let _ = index.apply([SessionChange::Deleted(SessionId::from("a"))]);
        let update = index.apply([SessionChange::Patched {
            session_id: SessionId::from("b"),
            patch: SessionPatch {
                directory: Some("/q".to_owned()),
                updated: Some(2),
                ..SessionPatch::default()
            },
        }]);
        assert_eq!(
            update.moves,
            [SessionMove {
                session_id: SessionId::from("b"),
                directory: "/q".to_owned(),
            }]
        );
        // The load left the server before both changes.
        let _ = index.complete_load(ticket, vec![session("a", "/p", 1), session("b", "/p", 1)]);
        assert_eq!(ids(index.active()), ["b"]);
        assert_eq!(
            index
                .get(&SessionId::from("b"))
                .and_then(|b| b.directory.as_deref()),
            Some("/q")
        );
        assert_eq!(index.status(), IndexStatus::Ready);
    }

    #[test]
    fn removals_count_even_for_sessions_not_yet_listed() {
        let mut index = SessionIndex::new();
        let ticket = index.begin_load();
        let _ = index.apply([SessionChange::Deleted(SessionId::from("late"))]);
        let _ = index.complete_load(ticket, vec![session("late", "/p", 1)]);
        assert!(index.active().is_empty());
    }

    #[test]
    fn an_obsolete_load_changes_nothing() {
        let mut index = SessionIndex::new();
        let old = index.begin_load();
        let new = index.begin_load();
        let update = index.complete_load(old, vec![session("a", "/p", 1)]);
        assert!(!update.changed);
        assert!(index.active().is_empty());
        let _ = index.complete_load(new, Vec::new());
        assert!(index.has_loaded());
    }

    #[test]
    fn a_failed_load_keeps_the_list() {
        let mut index = SessionIndex::new();
        let first = index.begin_load();
        let _ = index.complete_load(first, vec![session("a", "/p", 1)]);
        let second = index.begin_load();
        index.fail_load(second);
        assert_eq!(index.status(), IndexStatus::Failed);
        assert_eq!(ids(index.active()), ["a"]);
    }

    #[test]
    fn recency_waits_for_the_turn_to_settle() {
        let mut index = SessionIndex::new();
        let ticket = index.begin_load();
        let _ = index.complete_load(ticket, vec![session("a", "/p", 1)]);
        let id = SessionId::from("a");
        let update = index.apply([SessionChange::Patched {
            session_id: id.clone(),
            patch: SessionPatch {
                updated: Some(5),
                ..SessionPatch::default()
            },
        }]);
        assert!(
            !update.changed,
            "token traffic does not rebuild the sidebar"
        );
        assert_eq!(index.get(&id).map(|a| a.time.updated), Some(1));
        let update = index.apply([SessionChange::Settled(id.clone())]);
        assert!(update.changed);
        assert_eq!(index.get(&id).map(|a| a.time.updated), Some(5));
    }

    #[test]
    fn archive_moves_between_lists_and_patches_need_a_record() {
        let mut index = SessionIndex::new();
        let ticket = index.begin_load();
        let _ = index.complete_load(ticket, vec![session("a", "/p", 1), session("b", "/p", 1)]);
        let update = index.apply([
            SessionChange::Patched {
                session_id: SessionId::from("a"),
                patch: SessionPatch {
                    archived: Some(Some(9)),
                    ..SessionPatch::default()
                },
            },
            SessionChange::Patched {
                session_id: SessionId::from("unknown"),
                patch: SessionPatch {
                    title: Some("x".to_owned()),
                    ..SessionPatch::default()
                },
            },
        ]);
        assert!(update.changed);
        assert_eq!(ids(index.active()), ["b"]);
        assert_eq!(ids(index.archived()), ["a"]);
        assert!(index.get(&SessionId::from("unknown")).is_none());
        let _ = index.apply([SessionChange::Patched {
            session_id: SessionId::from("a"),
            patch: SessionPatch {
                archived: Some(None),
                ..SessionPatch::default()
            },
        }]);
        assert_eq!(ids(index.active()), ["a", "b"]);
    }

    #[test]
    fn stale_events_are_ignored_and_pages_merge_without_removing() {
        let mut index = SessionIndex::new();
        let ticket = index.begin_load();
        let _ = index.merge_page(ticket, vec![session("a", "/p", 5)]);
        let _ = index.apply([SessionChange::Created(session("a", "/old", 3))]);
        assert_eq!(
            index
                .get(&SessionId::from("a"))
                .and_then(|a| a.directory.as_deref()),
            Some("/p")
        );
        let _ = index.merge_page(ticket, vec![session("b", "/p", 4)]);
        assert_eq!(ids(index.active()), ["a", "b"]);
        assert!(!index.has_loaded(), "a page grants no full-list authority");
    }
}
