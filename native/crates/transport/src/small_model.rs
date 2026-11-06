//! The server's small background model (`/api/small-model`): whether one
//! can run for a directory, and one generation, as the AI rename uses them
//! (`useSmallModelStore.ts`, `generateSessionTitle`).

use reqwest::Method;
use serde::Deserialize;
use serde_json::json;

use crate::client::{ServerClient, encode_directory, read_json};
use crate::error::TransportError;

/// One generation request.
#[derive(Clone, Debug)]
pub struct Generation<'a> {
    pub prompt: &'a str,
    pub system: &'a str,
    pub directory: &'a str,
    pub preferred_provider: Option<&'a str>,
    pub preferred_model: Option<&'a str>,
}

#[derive(Deserialize)]
struct AvailabilityBody {
    available: Option<bool>,
}

#[derive(Deserialize)]
struct GenerationBody {
    text: Option<String>,
}

impl ServerClient {
    /// Whether the small model can run for `directory`; `None` when the
    /// answer does not say.
    pub async fn small_model_available(
        &self,
        directory: Option<&str>,
    ) -> Result<Option<bool>, TransportError> {
        let mut url = self.endpoint().url("api/small-model");
        if let Some(directory) = directory.map(str::trim).filter(|text| !text.is_empty()) {
            url.query_pairs_mut().append_pair("directory", directory);
        }
        let body: AvailabilityBody = self.get_json(url, None).await?;
        Ok(body.available)
    }

    /// Runs one generation and returns its raw text.
    pub async fn generate_with_small_model(
        &self,
        generation: &Generation<'_>,
    ) -> Result<String, TransportError> {
        let response = self
            .authorized(
                Method::POST,
                self.endpoint().url("api/small-model/generate"),
            )
            .header(
                "x-opencode-directory",
                encode_directory(generation.directory),
            )
            .json(&json!({
                "prompt": generation.prompt,
                "system": generation.system,
                "directory": generation.directory,
                "preferredProviderID": generation.preferred_provider,
                "preferredModelID": generation.preferred_model,
                // Read by servers before 2026-10.
                "restrictToPreferredProvider": true,
            }))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let body: GenerationBody = read_json(response).await?;
        body.text
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| TransportError::Malformed("the generation has no text".to_owned()))
    }
}
