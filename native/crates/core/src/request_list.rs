//! Pending-list completeness, shared by transport decoding and core reconciliation.

use crate::request_owner::{RequestOwner, RequestScope};

/// An entry excluded from a list. Its index is diagnostic only and grants no identity authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestListIssue {
    pub index: usize,
    pub reason: RequestListIssueKind,
}

/// Entry failures contain neither raw content nor guessed request IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestListIssueKind {
    Malformed,
    WrongOwner,
    DuplicateIdentity,
}

/// Valid siblings from one response and the issues that prevent removal authority.
/// `owner == None` is a whole-location list; a session/global-owner subset is merge-only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestList<T> {
    scope: RequestScope,
    owner: Option<RequestOwner>,
    entries: Vec<T>,
    issues: Vec<RequestListIssue>,
}

impl<T> RequestList<T> {
    /// Constructs a decoded response after validating its envelope and each retained entry.
    pub fn new(
        scope: RequestScope,
        owner: Option<RequestOwner>,
        entries: Vec<T>,
        issues: Vec<RequestListIssue>,
    ) -> Self {
        Self {
            scope,
            owner,
            entries,
            issues,
        }
    }
    /// The runtime and remote location captured for this read.
    pub fn scope(&self) -> &RequestScope {
        &self.scope
    }
    /// A requested subset owner, absent for a whole-location list.
    pub fn owner(&self) -> Option<&RequestOwner> {
        self.owner.as_ref()
    }
    /// Independently validated entries, in wire order.
    pub fn entries(&self) -> &[T] {
        &self.entries
    }
    /// Explicit entry failures. An empty valid list with issues is not authoritative empty success.
    pub fn issues(&self) -> &[RequestListIssue] {
        &self.issues
    }
    /// Whether every entry in this response was valid and unambiguous.
    pub fn is_complete(&self) -> bool {
        self.issues.is_empty()
    }
    /// Converts records while retaining scope, coverage and completeness.
    pub fn map<U>(self, project: impl FnMut(T) -> U) -> RequestList<U> {
        RequestList {
            scope: self.scope,
            owner: self.owner,
            entries: self.entries.into_iter().map(project).collect(),
            issues: self.issues,
        }
    }
    /// Consumes the response without losing its completeness information.
    pub fn into_parts(
        self,
    ) -> (
        RequestScope,
        Option<RequestOwner>,
        Vec<T>,
        Vec<RequestListIssue>,
    ) {
        (self.scope, self.owner, self.entries, self.issues)
    }
}
