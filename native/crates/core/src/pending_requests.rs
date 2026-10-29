//! One location's pending requests, with authoritative snapshots and terminal tombstones.
//!
//! A failed read changes no records. A reply affects one request; there is no shared mutation
//! gate, automatic retry, persistence, or approval policy in this owner.

use crate::form::FormRequest;
use crate::ids::{FormId, PermissionId};
use crate::permission::PermissionRequest;
use crate::request_list::RequestList;
use crate::request_owner::{RequestOwner, RequestScope};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_OWNER: AtomicU64 = AtomicU64::new(0);

/// Which complete location list a read can replace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RequestKind {
    Permission,
    Form,
}
/// Disjoint request identities.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RequestId {
    Permission(PermissionId),
    Form(FormId),
}
impl RequestId {
    /// The authoritative list containing this ID.
    pub fn kind(&self) -> RequestKind {
        match self {
            Self::Permission(_) => RequestKind::Permission,
            Self::Form(_) => RequestKind::Form,
        }
    }
}
/// Immutable routing identity retained through selection and runtime changes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RequestKey {
    pub scope: RequestScope,
    pub owner: RequestOwner,
    pub id: RequestId,
}
/// A validated request from transport, without renderer state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingRequest {
    Permission(PermissionRequest),
    Form(FormRequest),
}
impl PendingRequest {
    /// Identity and actual owner, independent of the session displaying the request.
    pub fn key(&self, scope: &RequestScope) -> RequestKey {
        let (owner, id) = match self {
            Self::Permission(p) => (
                RequestOwner::Session(p.session_id.clone()),
                RequestId::Permission(p.id.clone()),
            ),
            Self::Form(f) => (f.owner.clone(), RequestId::Form(f.id.clone())),
        };
        RequestKey {
            scope: scope.clone(),
            owner,
            id,
        }
    }
}
/// Local response state. Uncertain outcomes require an authoritative read before another reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseState {
    Pending,
    Responding,
    Uncertain,
}
/// A retained request and its local response lifecycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingEntry {
    pub request: PendingRequest,
    pub response: ResponseState,
    pub current: bool,
}
/// A complete-read baseline, tied to one scope, kind and epoch.
#[derive(Clone, Debug)]
pub struct RequestLoad {
    instance: u64,
    scope: RequestScope,
    kind: RequestKind,
    epoch: u64,
    revision: u64,
    sequence: u64,
}
/// One explicit in-flight reply. Its completion cannot affect another request or a later attempt.
#[derive(Clone, Debug)]
pub struct RequestAttempt {
    instance: u64,
    key: RequestKey,
    epoch: u64,
    revision: u64,
}
impl RequestAttempt {
    /// The owner captured before I/O.
    pub fn key(&self) -> &RequestKey {
        &self.key
    }
}
/// One exact read bound to an owner instance and mutation revision.
#[derive(Clone, Debug)]
pub struct RequestCheck {
    instance: u64,
    epoch: u64,
    revision: u64,
    key: RequestKey,
}

/// Why a pending-state transition was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestTransitionError {
    WrongScope,
    Stale,
    InvalidSnapshot,
    NotActionable,
}
/// Local failure certainty; only a definite refusal makes another explicit action available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyFailure {
    Rejected,
    Unknown,
}

/// Stores exactly one runtime/location. Dropping it is the only tombstone eviction.
#[derive(Debug)]
pub struct PendingRequests {
    instance: u64,
    scope: RequestScope,
    epoch: u64,
    revision: u64,
    loads: BTreeMap<RequestKind, u64>,
    complete: BTreeSet<RequestKind>,
    entries: BTreeMap<RequestKey, PendingEntry>,
    mutations: BTreeMap<RequestKey, u64>,
    attempts: BTreeMap<RequestKey, u64>,
    terminal: BTreeSet<RequestKey>,
}
impl PendingRequests {
    /// Creates an owner without claiming either list is complete.
    ///
    /// Panics if the process has exhausted all 64-bit owner identities.
    pub fn new(scope: RequestScope) -> Self {
        Self {
            instance: NEXT_OWNER
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    value.checked_add(1)
                })
                .expect("pending request owner identity space exhausted"),
            scope,
            epoch: 0,
            revision: 0,
            loads: BTreeMap::new(),
            complete: BTreeSet::new(),
            entries: BTreeMap::new(),
            mutations: BTreeMap::new(),
            attempts: BTreeMap::new(),
            terminal: BTreeSet::new(),
        }
    }
    /// Its captured runtime/location.
    pub fn scope(&self) -> &RequestScope {
        &self.scope
    }
    /// The retained entries, including stale ones while reconciliation fails.
    pub fn entries(&self) -> impl Iterator<Item = (&RequestKey, &PendingEntry)> {
        self.entries.iter()
    }
    /// Reads one exact captured identity.
    pub fn get(&self, key: &RequestKey) -> Option<&PendingEntry> {
        self.entries.get(key)
    }
    /// Captures an exact read without granting mutation authority.
    pub fn begin_check(&self, key: &RequestKey) -> RequestCheck {
        RequestCheck {
            instance: self.instance,
            epoch: self.epoch,
            revision: self.revision,
            key: key.clone(),
        }
    }
    /// Commits a verified exact pending read or declared terminal result.
    pub fn checked(
        &mut self,
        check: RequestCheck,
        request: Option<PendingRequest>,
    ) -> Result<(), RequestTransitionError> {
        if check.key.scope != self.scope
            || check.instance != self.instance
            || check.epoch != self.epoch
            || self
                .mutations
                .get(&check.key)
                .is_some_and(|revision| *revision > check.revision)
            || self.terminal.contains(&check.key)
        {
            return Err(RequestTransitionError::Stale);
        }
        if let Some(request) = request {
            if request.key(&self.scope) != check.key {
                return Err(RequestTransitionError::InvalidSnapshot);
            }
            if self
                .entries
                .get(&check.key)
                .is_some_and(|entry| entry.response == ResponseState::Responding)
            {
                return Err(RequestTransitionError::NotActionable);
            }
            self.change(&check.key);
            self.entries.insert(
                check.key,
                PendingEntry {
                    request,
                    response: ResponseState::Pending,
                    current: true,
                },
            );
        } else {
            self.settle(&check.key)?;
        }
        Ok(())
    }
    /// Whether a complete authoritative list established this kind's current baseline.
    pub fn is_complete(&self, kind: RequestKind) -> bool {
        self.complete.contains(&kind)
    }
    fn change(&mut self, key: &RequestKey) -> u64 {
        self.revision += 1;
        self.mutations.insert(key.clone(), self.revision);
        self.revision
    }
    /// Begins a complete location read. On failure, simply retain this store unchanged.
    pub fn begin_load(&mut self, kind: RequestKind) -> RequestLoad {
        let sequence = self.loads.entry(kind).or_default();
        *sequence += 1;
        RequestLoad {
            instance: self.instance,
            scope: self.scope.clone(),
            kind,
            epoch: self.epoch,
            revision: self.revision,
            sequence: *sequence,
        }
    }
    /// Publishes valid siblings. Only a complete whole-location list can remove missing entries.
    /// Failed envelopes never reach this method; incomplete or owner-subset lists are merge-only.
    pub fn apply_list(
        &mut self,
        load: RequestLoad,
        list: RequestList<PendingRequest>,
    ) -> Result<(), RequestTransitionError> {
        if load.scope != self.scope || list.scope() != &self.scope {
            return Err(RequestTransitionError::WrongScope);
        }
        if load.instance != self.instance
            || load.epoch != self.epoch
            || self.loads.get(&load.kind) != Some(&load.sequence)
        {
            return Err(RequestTransitionError::Stale);
        }
        let authoritative = list.is_complete() && list.owner().is_none();
        let (_, owner, requests, _) = list.into_parts();
        let mut snapshot = BTreeMap::new();
        let mut ids = BTreeSet::new();
        for request in requests {
            let key = request.key(&self.scope);
            if key.id.kind() != load.kind
                || !ids.insert(key.id.clone())
                || owner.as_ref().is_some_and(|owner| *owner != key.owner)
            {
                return Err(RequestTransitionError::InvalidSnapshot);
            }
            snapshot.insert(key, request);
        }
        let removed: Vec<_> = self
            .entries
            .keys()
            .filter(|key| {
                authoritative
                    && key.id.kind() == load.kind
                    && !snapshot.contains_key(*key)
                    && self
                        .mutations
                        .get(*key)
                        .is_none_or(|revision| *revision <= load.revision)
            })
            .cloned()
            .collect();
        for key in removed {
            self.settle(&key)?;
        }
        for (key, request) in snapshot {
            if self.terminal.contains(&key)
                || self
                    .mutations
                    .get(&key)
                    .is_some_and(|revision| *revision > load.revision)
            {
                continue;
            }
            // A read cannot release an in-flight mutation, even when it began after that mutation.
            let response = self
                .entries
                .get(&key)
                .map_or(ResponseState::Pending, |entry| entry.response);
            self.entries.insert(
                key,
                PendingEntry {
                    request,
                    response,
                    current: true,
                },
            );
        }
        if authoritative {
            self.complete.insert(load.kind);
        } else {
            self.complete.remove(&load.kind);
        }
        Ok(())
    }
    /// Applies a live ask. Terminal IDs never resurrect from replay.
    pub fn asked(&mut self, request: PendingRequest) -> bool {
        let key = request.key(&self.scope);
        if self.terminal.contains(&key) {
            return false;
        }
        if self
            .entries
            .get(&key)
            .is_some_and(|entry| entry.request == request && entry.current)
        {
            return false;
        }
        let response = self
            .entries
            .get(&key)
            .map_or(ResponseState::Pending, |entry| entry.response);
        self.change(&key);
        self.entries.insert(
            key,
            PendingEntry {
                request,
                response,
                current: true,
            },
        );
        true
    }
    /// Applies a terminal event or authoritative terminal response, even before the ask arrived.
    pub fn settle(&mut self, key: &RequestKey) -> Result<(), RequestTransitionError> {
        if key.scope != self.scope {
            return Err(RequestTransitionError::WrongScope);
        }
        self.change(key);
        self.entries.remove(key);
        self.attempts.remove(key);
        self.terminal.insert(key.clone());
        Ok(())
    }
    /// A replay gap or location shutdown makes retained cards non-actionable until refreshed.
    pub fn invalidate(&mut self) {
        self.epoch += 1;
        self.complete.clear();
        self.attempts.clear();
        for entry in self.entries.values_mut() {
            entry.current = false;
            if entry.response == ResponseState::Responding {
                entry.response = ResponseState::Uncertain;
            }
        }
    }
    /// Captures exactly one explicit response; unrelated requests remain available.
    pub fn begin_reply(
        &mut self,
        key: &RequestKey,
    ) -> Result<RequestAttempt, RequestTransitionError> {
        if key.scope != self.scope {
            return Err(RequestTransitionError::WrongScope);
        }
        let entry = self
            .entries
            .get_mut(key)
            .ok_or(RequestTransitionError::NotActionable)?;
        if !entry.current || entry.response != ResponseState::Pending {
            return Err(RequestTransitionError::NotActionable);
        }
        entry.response = ResponseState::Responding;
        let revision = self.change(key);
        self.attempts.insert(key.clone(), revision);
        Ok(RequestAttempt {
            instance: self.instance,
            key: key.clone(),
            epoch: self.epoch,
            revision,
        })
    }
    /// A failed mutation never removes a request or overrides a later terminal event.
    pub fn reply_failed(
        &mut self,
        attempt: RequestAttempt,
        failure: ReplyFailure,
    ) -> Result<(), RequestTransitionError> {
        if attempt.key.scope != self.scope {
            return Err(RequestTransitionError::WrongScope);
        }
        if attempt.instance != self.instance
            || attempt.epoch != self.epoch
            || self.attempts.get(&attempt.key) != Some(&attempt.revision)
        {
            return Err(RequestTransitionError::Stale);
        }
        let Some(entry) = self.entries.get_mut(&attempt.key) else {
            return Err(RequestTransitionError::Stale);
        };
        entry.response = match failure {
            ReplyFailure::Rejected => ResponseState::Pending,
            ReplyFailure::Unknown => ResponseState::Uncertain,
        };
        self.attempts.remove(&attempt.key);
        self.change(&attempt.key);
        Ok(())
    }
}
