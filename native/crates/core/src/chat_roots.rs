//! Managed Chats directories as named by the server.

use crate::path;

/// The managed Chats roots the server reported for one runtime.
///
/// Canonical aliases come from the server's filesystem; the client never
/// guesses them by case folding. Classification always reports the original
/// root identity so folders and scopes stay stable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatRoots {
    configured: String,
    legacy: String,
    canonical_configured: String,
    canonical_legacy: String,
}

impl ChatRoots {
    /// Builds roots from the server's home information.
    ///
    /// Returns `None` when neither a usable home nor a configured root exists,
    /// matching the reference client's refusal to guess a root.
    pub fn from_home_info(
        home: Option<&str>,
        chats_root: Option<&str>,
        canonical_chats_root: Option<&str>,
        canonical_legacy_chats_root: Option<&str>,
    ) -> Option<Self> {
        let legacy = path::normalize_optional(home)
            .map(|home| format!("{}/.config/openchamber/chats", home.trim_end_matches('/')))?;
        let configured = path::normalize_optional(chats_root).unwrap_or_else(|| legacy.clone());
        Some(Self {
            canonical_configured: path::normalize_optional(canonical_chats_root)
                .unwrap_or_else(|| configured.clone()),
            canonical_legacy: path::normalize_optional(canonical_legacy_chats_root)
                .unwrap_or_else(|| legacy.clone()),
            configured,
            legacy,
        })
    }

    /// The root new chats are created under.
    pub fn configured(&self) -> &str {
        &self.configured
    }

    /// The managed root containing `directory`, if any.
    pub fn root_of(&self, directory: Option<&str>) -> Option<&str> {
        let normalized = path::normalize_optional(directory)?;
        if path::is_within(&normalized, &self.configured)
            || path::is_within(&normalized, &self.canonical_configured)
        {
            return Some(&self.configured);
        }
        if path::is_within(&normalized, &self.legacy)
            || path::is_within(&normalized, &self.canonical_legacy)
        {
            return Some(&self.legacy);
        }
        None
    }

    pub fn contains(&self, directory: Option<&str>) -> bool {
        self.root_of(directory).is_some()
    }
}

/// Whether `directory` is a managed Chat directory under known roots.
///
/// Unknown roots classify nothing as a managed Chat directory.
pub fn is_chat_directory(roots: Option<&ChatRoots>, directory: Option<&str>) -> bool {
    roots.is_some_and(|roots| roots.contains(directory))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_configured_legacy_and_canonical_aliases() {
        let roots = ChatRoots::from_home_info(
            Some("/home/me"),
            Some("/data/chats"),
            Some("/private/data/chats"),
            None,
        )
        .expect("home is usable");
        assert_eq!(
            roots.root_of(Some("/data/chats/2026/s")),
            Some("/data/chats")
        );
        assert_eq!(
            roots.root_of(Some("/private/data/chats/x")),
            Some("/data/chats")
        );
        assert_eq!(
            roots.root_of(Some("/home/me/.config/openchamber/chats/a")),
            Some("/home/me/.config/openchamber/chats")
        );
        assert_eq!(roots.root_of(Some("/data/chatsx")), None);
        assert!(!is_chat_directory(None, Some("/data/chats/a")));
    }

    #[test]
    fn requires_a_home() {
        assert_eq!(
            ChatRoots::from_home_info(None, Some("/data/chats"), None, None),
            None
        );
    }
}
