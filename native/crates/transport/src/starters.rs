//! What the new-session starters read and write besides settings: a
//! project's starters in its setup (`getProjectDraftStarters`,
//! `saveProjectDraftStarters`, `updateSharedProjectSetup` in
//! `lib/openchamberConfig.ts`), and the magic-prompt overrides a built-in
//! starter renders with (`loadMagicPrompts` in `lib/magicPrompts.ts`).

use std::collections::HashMap;

use openchamber_native_core::draft_starters::{
    ProjectStarter, StarterKind, StarterRef, StarterSource,
};
use openchamber_native_core::ids::ProjectId;
use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{ServerClient, read_json};
use crate::error::TransportError;

#[derive(Deserialize)]
struct SourcedStarterWire {
    #[serde(rename = "type")]
    kind: String,
    name: String,
    source: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetupStartersWire {
    draft_starters: Vec<SourcedStarterWire>,
}

/// The merged list, shared starters first, as the server answers it. Like
/// the web's schema, any malformed entry fails the whole read.
fn project_starters(wire: SetupStartersWire) -> Result<Vec<ProjectStarter>, TransportError> {
    wire.draft_starters
        .into_iter()
        .map(|entry| {
            let kind = StarterKind::from_wire(&entry.kind);
            let source = match entry.source.as_str() {
                "shared" => Some(StarterSource::Shared),
                "personal" => Some(StarterSource::Personal),
                _ => None,
            };
            match (kind, source) {
                (Some(kind), Some(source)) if !entry.name.is_empty() => Ok(ProjectStarter {
                    starter: StarterRef {
                        kind,
                        name: entry.name,
                    },
                    source,
                }),
                _ => Err(TransportError::Malformed(
                    "invalid project starter".to_owned(),
                )),
            }
        })
        .collect()
}

fn refs_body(starters: &[StarterRef]) -> Value {
    Value::Array(
        starters
            .iter()
            .map(|starter| json!({ "type": starter.kind.as_wire(), "name": starter.name }))
            .collect(),
    )
}

#[derive(Deserialize)]
struct OverridesWire {
    version: u64,
    overrides: HashMap<String, String>,
}

impl ServerClient {
    fn project_starters_id(project_path: &str) -> Result<ProjectId, TransportError> {
        ProjectId::from_path(project_path)
            .ok_or_else(|| TransportError::Malformed("the project has no path".to_owned()))
    }

    /// The starters pinned for the project at `project_path`, shared ones
    /// first. A failure is an error: an empty list written back after it
    /// would replace what is on disk.
    pub async fn project_starters(
        &self,
        project_path: &str,
    ) -> Result<Vec<ProjectStarter>, TransportError> {
        let project = Self::project_starters_id(project_path)?;
        let wire: SetupStartersWire = self
            .get_json(self.project_setup_url(&project), None)
            .await?;
        project_starters(wire)
    }

    /// Replaces the user's own starters of the project; shared ones stay.
    /// Answers the merged list after the write.
    pub async fn save_project_starters(
        &self,
        project_path: &str,
        starters: &[StarterRef],
    ) -> Result<Vec<ProjectStarter>, TransportError> {
        let project = Self::project_starters_id(project_path)?;
        let body = json!({
            "draftStarters": refs_body(starters),
            "projectPath": project_path.trim(),
        });
        let response = self
            .authorized(Method::PUT, self.project_setup_url(&project))
            .json(&body)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        project_starters(read_json(response).await?)
    }

    /// Replaces the starters in the repository's shared file.
    pub async fn save_shared_starters(
        &self,
        project_path: &str,
        starters: &[StarterRef],
    ) -> Result<Vec<ProjectStarter>, TransportError> {
        let project = Self::project_starters_id(project_path)?;
        let mut url = self.project_setup_url(&project);
        url.path_segments_mut()
            .expect("http and https URLs have path segments")
            .push("shared");
        let response = self
            .authorized(Method::PUT, url)
            .json(&json!({ "draftStarters": refs_body(starters) }))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        project_starters(read_json(response).await?)
    }

    /// The URL of the magic-prompt overrides for work in `directory`, or the
    /// global ones without a directory.
    pub fn magic_prompts_url(&self, directory: Option<&str>) -> Url {
        let mut url = self.endpoint().url("api/magic-prompts");
        if let Some(directory) = directory {
            url.query_pairs_mut()
                .append_pair("sessionDirectory", directory);
        }
        url
    }

    /// The overrides the server resolved for `directory`. A failed or
    /// malformed read is an error: built-in text never stands in for
    /// overrides that could not be read.
    pub async fn magic_prompt_overrides(
        &self,
        directory: Option<&str>,
    ) -> Result<HashMap<String, String>, TransportError> {
        let wire: OverridesWire = self
            .get_json(self.magic_prompts_url(directory), None)
            .await?;
        if wire.version != 1 {
            return Err(TransportError::Malformed(
                "unknown magic prompts version".to_owned(),
            ));
        }
        Ok(wire.overrides)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_malformed_entry_fails_the_whole_list() {
        let read =
            |json: &str| project_starters(serde_json::from_str(json).expect("fixture is JSON"));
        let list = read(
            r#"{"draftStarters":[{"type":"command","name":"ship","source":"shared"},{"type":"skill","name":"lint","source":"personal"}],"other":1}"#,
        )
        .expect("valid list");
        assert_eq!(list[0].source, StarterSource::Shared);
        assert_eq!(list[1].starter.kind, StarterKind::Skill);
        assert!(
            read(r#"{"draftStarters":[{"type":"prompt","name":"x","source":"shared"}]}"#).is_err()
        );
        assert!(
            read(r#"{"draftStarters":[{"type":"skill","name":"","source":"personal"}]}"#).is_err()
        );
        assert!(serde_json::from_str::<SetupStartersWire>("{}").is_err());
    }

    #[test]
    fn writes_name_each_starter_by_type() {
        assert_eq!(
            refs_body(&[StarterRef::command("explore")]),
            json!([{ "type": "command", "name": "explore" }])
        );
    }
}
