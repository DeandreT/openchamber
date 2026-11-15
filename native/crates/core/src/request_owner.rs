//! Captured runtime and location ownership for blocking requests.

use crate::ids::{RuntimeId, SessionId};
use crate::path;

/// One remote location in one verified runtime. No ambient directory fallback exists.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestScope {
    runtime: RuntimeId,
    directory: String,
}

impl RequestScope {
    /// Captures an absolute remote directory without consulting the local filesystem.
    pub fn new(runtime: RuntimeId, directory: &str) -> Option<Self> {
        let directory = normalize_directory(directory)?;
        if runtime.as_str().is_empty() {
            return None;
        }
        Some(Self { runtime, directory })
    }

    /// The runtime captured when the request was observed.
    pub fn runtime(&self) -> &RuntimeId {
        &self.runtime
    }

    /// The remote directory captured when the request was observed.
    pub fn directory(&self) -> &str {
        &self.directory
    }
}

/// Validates an absolute remote request location without inventing a runtime or consulting disk.
pub fn normalize_directory(directory: &str) -> Option<String> {
    let directory = path::normalize(directory)?;
    let bytes = directory.as_bytes();
    let absolute = directory.starts_with('/')
        || (bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && &bytes[1..3] == b":/");
    if !absolute
        || directory.chars().any(char::is_control)
        || directory.split('/').any(|part| part == "." || part == "..")
    {
        return None;
    }
    Some(directory)
}

/// The form's actual owner. Location-owned forms use the wire sentinel `global`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RequestOwner {
    Session(SessionId),
    Location,
}

impl RequestOwner {
    /// Parses the pinned protocol's real session or location sentinel.
    pub fn from_wire(value: &str) -> Option<Self> {
        if value == "global" {
            return Some(Self::Location);
        }
        (value.starts_with("ses") && !value.chars().any(char::is_control))
            .then(|| Self::Session(SessionId::from(value)))
    }

    /// The route's session ID; `global` must retain its captured scope.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Session(id) => id.as_str(),
            Self::Location => "global",
        }
    }
}
