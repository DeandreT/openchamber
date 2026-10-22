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
