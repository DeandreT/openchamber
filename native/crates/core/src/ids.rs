//! Typed identifiers for server-owned and client-owned entities.
//!
//! Every identifier is an opaque string. Identifier spelling never
//! establishes chronology: OpenCode IDs can roll over and user-message IDs can
//! come from another client's clock, so ordering always comes from the server's
//! sequence.

use std::borrow::Borrow;
use std::fmt;

macro_rules! string_id {
    ($(#[$attr:meta])* $name:ident) => {
        $(#[$attr])*
        #[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            /// Wraps an identifier received from its owner.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// The identifier as received.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }
    };
}

string_id!(
    /// A pending OpenCode permission request.
    PermissionId
);
string_id!(
    /// An OpenCode form, distinct from a session or permission.
    FormId
);
string_id!(
    /// An OpenCode session identifier.
    SessionId
);
string_id!(
    /// An OpenCode message identifier, stable for the message's lifetime.
    MessageId
);
string_id!(
    /// An OpenCode message-part identifier, stable for the part's lifetime.
    PartId
);
string_id!(
    /// OpenChamber's path-derived project identifier.
    ///
    /// This is a different namespace from OpenCode's project IDs; only
    /// canonical worktree metadata can bridge the two.
    ProjectId
);
string_id!(
    /// An OpenCode project identifier as reported in session metadata.
    OpenCodeProjectId
);
string_id!(
    /// An isolated space identifier.
    SpaceId
);
string_id!(
    /// A client-owned identifier for a new-session draft that has no session yet.
    DraftId
);
string_id!(
    /// The verified server identity and endpoint path prefix of a runtime.
    ///
    /// Client-owned records such as drafts and scroll anchors keep the runtime
    /// they were created under, so a runtime switch never restores them into a
    /// different server's session.
    RuntimeId
);

impl ProjectId {
    /// The identifier OpenChamber derives from a project path:
    /// `path_` and the unpadded URL-safe Base64 of the path's UTF-8 bytes,
    /// as `createProjectIdFromPath` in `packages/ui/src/lib/projectId.ts`.
    ///
    /// Backslashes become slashes and trailing slashes are dropped first.
    /// Returns `None` for an empty path.
    pub fn from_path(path: &str) -> Option<Self> {
        let normalized = path.replace('\\', "/");
        let normalized = normalized.trim_end_matches('/').trim();
        if normalized.is_empty() {
            return None;
        }
        Some(Self::new(format!(
            "path_{}",
            base64_url(normalized.as_bytes())
        )))
    }
}

/// Unpadded URL-safe Base64.
fn base64_url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let triple = chunk.iter().enumerate().fold(0u32, |value, (index, byte)| {
            value | u32::from(*byte) << (16 - 8 * index)
        });
        for index in 0..=chunk.len() {
            let sextet = (triple >> (18 - 6 * index)) & 0x3f;
            encoded.push(char::from(ALPHABET[sextet as usize]));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_ids_match_the_web_client() {
        // Expected values come from `createProjectIdFromPath`.
        assert_eq!(
            ProjectId::from_path("/home/synthetic/project/")
                .as_ref()
                .map(ProjectId::as_str),
            Some("path_L2hvbWUvc3ludGhldGljL3Byb2plY3Q")
        );
        assert_eq!(
            ProjectId::from_path("C:\\work\\ä")
                .as_ref()
                .map(ProjectId::as_str),
            Some("path_Qzovd29yay_DpA")
        );
        assert_eq!(ProjectId::from_path("///"), None);
    }
}
