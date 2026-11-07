//! Registered projects as the settings document stores them.
//!
//! The web client writes the whole `projects` array with `activeProjectId`
//! (`persistProjects` in `useProjectsStore.ts`). Each entry carries more
//! than this client reads (icon, color, defaults), so a change reads the
//! stored entries, edits only what it changes and writes the rest back as
//! they were.

use reqwest::Method;
use serde_json::{Map, Value, json};

use crate::client::{ServerClient, read_json};
use crate::error::TransportError;

/// The stored entries and the active project, as read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProjectEntries {
    pub entries: Vec<Map<String, Value>>,
    pub active_project_id: Option<String>,
}

impl ProjectEntries {
    /// The stored path of an entry.
    pub fn path(entry: &Map<String, Value>) -> Option<&str> {
        entry.get("path").and_then(Value::as_str)
    }

    pub fn id(entry: &Map<String, Value>) -> Option<&str> {
        entry.get("id").and_then(Value::as_str)
    }
}

impl ServerClient {
    /// The settings document's project entries.
    pub async fn project_entries(&self) -> Result<ProjectEntries, TransportError> {
        let body: Value = self.get_json(self.settings_url(), None).await?;
        let entries = match body.get("projects") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(list)) => list
                .iter()
                .filter_map(|entry| entry.as_object().cloned())
                .collect(),
            Some(_) => {
                return Err(TransportError::Malformed(
                    "the stored projects are not a list".to_owned(),
                ));
            }
        };
        Ok(ProjectEntries {
            entries,
            active_project_id: body
                .get("activeProjectId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned),
        })
    }

    /// Writes the whole project list and the active project. The caller
    /// reads the merged settings again.
    pub async fn save_project_entries(
        &self,
        projects: &ProjectEntries,
    ) -> Result<(), TransportError> {
        let mut body = json!({ "projects": projects.entries });
        if let Some(active) = &projects.active_project_id {
            body["activeProjectId"] = json!(active);
        }
        let response = self
            .authorized(Method::PUT, self.settings_url())
            .json(&body)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let _: Value = read_json(response).await?;
        Ok(())
    }
}
