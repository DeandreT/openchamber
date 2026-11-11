//! Settings → Git (`GitPage.tsx`, `GitSettings.tsx`, `useGitIdentitiesStore.ts`):
//! the Git fields of the settings document and the removal of a stored
//! identity. The identity list itself is read through [`crate::git`].

use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{ServerClient, read_json};
use crate::error::TransportError;
use crate::files::read_answer;

/// How the Git view lists changed files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChangesView {
    #[default]
    Flat,
    Tree,
}

impl ChangesView {
    fn wire(self) -> &'static str {
        match self {
            Self::Flat => "flat",
            Self::Tree => "tree",
        }
    }
}

/// The Git fields of the settings document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitPreferences {
    /// The identity new repositories use: a profile ID, or `global`.
    pub default_identity: Option<String>,
    pub gitmoji_enabled: bool,
    pub changes_view: ChangesView,
}

/// One field of [`GitPreferences`] to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitPreferenceChange {
    /// `None` unsets the default.
    DefaultIdentity(Option<String>),
    GitmojiEnabled(bool),
    ChangesView(ChangesView),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreferencesWire {
    default_git_identity_id: Option<String>,
    gitmoji_enabled: Option<bool>,
    git_changes_view_mode: Option<String>,
}

impl From<PreferencesWire> for GitPreferences {
    fn from(wire: PreferencesWire) -> Self {
        Self {
            default_identity: wire
                .default_git_identity_id
                .map(|id| id.trim().to_owned())
                .filter(|id| !id.is_empty()),
            gitmoji_enabled: wire.gitmoji_enabled == Some(true),
            changes_view: match wire.git_changes_view_mode.as_deref() {
                Some("tree") => ChangesView::Tree,
                _ => ChangesView::Flat,
            },
        }
    }
}

/// Why a stored identity was not removed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GitIdentityError {
    #[error("the server refused the identity change (HTTP {status})")]
    Refused {
        status: u16,
        message: Option<String>,
    },
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl ServerClient {
    /// Reads the Git fields of the settings document.
    pub async fn git_preferences(&self) -> Result<GitPreferences, TransportError> {
        let wire: PreferencesWire = self.get_json(self.settings_url(), None).await?;
        Ok(wire.into())
    }

    /// Writes one Git field and returns the fields as the server merged them.
    pub async fn save_git_preference(
        &self,
        change: &GitPreferenceChange,
    ) -> Result<GitPreferences, TransportError> {
        let body: Value = match change {
            // An empty ID unsets the default, as the web's write does.
            GitPreferenceChange::DefaultIdentity(id) => {
                json!({ "defaultGitIdentityId": id.as_deref().unwrap_or_default() })
            }
            GitPreferenceChange::GitmojiEnabled(enabled) => json!({ "gitmojiEnabled": enabled }),
            GitPreferenceChange::ChangesView(view) => {
                json!({ "gitChangesViewMode": view.wire() })
            }
        };
        let response = self
            .authorized(Method::PUT, self.settings_url())
            .json(&body)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let wire: PreferencesWire = read_json(response).await?;
        Ok(wire.into())
    }

    /// Removes the stored identity `id` (`DELETE /api/git/identities/:id`).
    pub async fn delete_git_identity(&self, id: &str) -> Result<(), GitIdentityError> {
        let mut url = self.endpoint().url("api/git/identities/");
        url.path_segments_mut()
            .expect("http and https URLs have path segments")
            .pop_if_empty()
            .push(id);
        let response = self
            .authorized(Method::DELETE, url)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        match read_answer(response).await? {
            Ok(_) => Ok(()),
            Err(refusal) => Err(GitIdentityError::Refused {
                status: refusal.status.as_u16(),
                message: refusal.message,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn one_field_is_written_and_the_merged_document_is_read_back() {
        let (client, recorded) = crate::test_server::serve(Box::new(|request| {
            match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/api/config/settings") => (
                    200,
                    json!({"defaultGitIdentityId": " work ", "gitChangesViewMode": "tree"}),
                ),
                ("PUT", "/api/config/settings") => (
                    200,
                    json!({"defaultGitIdentityId": "", "gitmojiEnabled": true}),
                ),
                ("DELETE", "/api/git/identities/work") => (200, json!({"success": true})),
                _ => (500, json!({"error": "identity is in use"})),
            }
        }))
        .await;
        let read = client.git_preferences().await.expect("read");
        assert_eq!(read.default_identity.as_deref(), Some("work"));
        assert_eq!(read.changes_view, ChangesView::Tree);
        assert!(!read.gitmoji_enabled);
        let saved = client
            .save_git_preference(&GitPreferenceChange::DefaultIdentity(None))
            .await
            .expect("save");
        assert_eq!(saved.default_identity, None, "an empty ID is no default");
        assert!(saved.gitmoji_enabled);
        client.delete_git_identity("work").await.expect("delete");
        assert!(matches!(
            client.delete_git_identity("other").await,
            Err(GitIdentityError::Refused {
                status: 500,
                message: Some(_)
            })
        ));
        let recorded = recorded.lock().expect("log");
        assert_eq!(recorded[1].body, json!({"defaultGitIdentityId": ""}));
    }
}
