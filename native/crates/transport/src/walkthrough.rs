//! User-requested diff walkthroughs through OpenChamber's existing routes.
//!
//! Reads never generate. Generation survives detaching the client; only the
//! explicit cancel route stops it. Hunk identities come from the server.

use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::client::ServerClient;
use crate::error::TransportError;
use crate::request::RequestBuilder;

/// The working-tree comparison being explained.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    All,
    Staged,
    Working,
}

/// Local sources supported by the native Changes surface.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Source {
    WorkingTree {
        scope: Scope,
    },
    Branch {
        #[serde(rename = "baseRef")]
        base_ref: String,
        #[serde(rename = "headRef")]
        head_ref: String,
    },
    Commit {
        hash: String,
    },
}

/// Captured composer provider and prose language. An absent model override
/// leaves the server's configured walkthrough/small-model choice intact.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Options {
    #[serde(skip_serializing_if = "Option::is_none", rename = "providerID")]
    pub provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "modelID")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub language: String,
}

/// One current hunk, including the standalone patch supplied by the server.
#[derive(Clone, Deserialize)]
pub struct Hunk {
    pub id: String,
    pub path: String,
    pub patch: String,
}

impl fmt::Debug for Hunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hunk")
            .field("id", &self.id)
            .field("patch_bytes", &self.patch.len())
            .finish_non_exhaustive()
    }
}

/// One stop's additional fact about the change.
#[derive(Clone, Deserialize)]
pub struct Observation {
    pub kind: String,
    pub note: String,
}

impl fmt::Debug for Observation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Observation")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// A reading stop whose anchors are opaque hunk IDs.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stop {
    pub id: String,
    pub title: String,
    pub hunk_ids: Vec<String>,
    pub prose: String,
    #[serde(default)]
    pub observations: Vec<Observation>,
}

impl fmt::Debug for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stop")
            .field("id", &self.id)
            .field("hunks", &self.hunk_ids.len())
            .finish_non_exhaustive()
    }
}

/// A chapter in the model's reading order.
#[derive(Clone, Deserialize)]
pub struct Chapter {
    pub id: String,
    pub title: String,
    pub blurb: String,
    pub stops: Vec<Stop>,
}

impl fmt::Debug for Chapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Chapter")
            .field("id", &self.id)
            .field("stops", &self.stops.len())
            .finish_non_exhaustive()
    }
}

/// The generated narrative. Code remains in the separately returned index.
#[derive(Clone, Deserialize)]
pub struct Walkthrough {
    pub title: String,
    pub focus: String,
    pub chapters: Vec<Chapter>,
}

impl fmt::Debug for Walkthrough {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Walkthrough")
            .field("chapters", &self.chapters.len())
            .finish_non_exhaustive()
    }
}

/// Whether an explicit generation is possible for this comparison.
#[derive(Clone, Debug, Deserialize)]
pub struct Readiness {
    pub ready: bool,
    pub reason: Option<String>,
}

/// A complete authoritative read of this source and its current anchors.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Read {
    pub source: Source,
    pub walkthrough: Option<Walkthrough>,
    pub hunks: Vec<Hunk>,
    pub hunk_count: usize,
    pub readiness: Option<Readiness>,
    #[serde(default)]
    pub generating: bool,
    #[serde(default)]
    pub is_stale: bool,
    #[serde(default)]
    pub stale_stop_ids: Vec<String>,
    #[serde(default)]
    pub uncovered_hunk_ids: Vec<String>,
}

impl fmt::Debug for Read {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WalkthroughRead")
            .field("hunks", &self.hunks.len())
            .field("generating", &self.generating)
            .finish_non_exhaustive()
    }
}

impl Read {
    fn complete_coverage(&mut self) {
        let covered: HashSet<&str> = self
            .walkthrough
            .iter()
            .flat_map(|walkthrough| &walkthrough.chapters)
            .flat_map(|chapter| &chapter.stops)
            .flat_map(|stop| stop.hunk_ids.iter().map(String::as_str))
            .collect();
        self.uncovered_hunk_ids = self
            .hunks
            .iter()
            .filter(|hunk| !covered.contains(hunk.id.as_str()))
            .map(|hunk| hunk.id.clone())
            .collect();
    }

    fn validate(&self, expected: &Source) -> Result<(), Error> {
        let mut hunks = HashSet::new();
        if &self.source != expected
            || self.hunk_count != self.hunks.len()
            || self
                .hunks
                .iter()
                .any(|hunk| hunk.id.is_empty() || !hunks.insert(&hunk.id))
        {
            return Err(Error::Malformed);
        }
        if let Some(walkthrough) = &self.walkthrough {
            let mut chapters = HashSet::new();
            let mut stops = HashSet::new();
            for chapter in &walkthrough.chapters {
                if chapter.id.is_empty() || !chapters.insert(&chapter.id) {
                    return Err(Error::Malformed);
                }
                for stop in &chapter.stops {
                    if stop.id.is_empty() || !stops.insert(&stop.id) {
                        return Err(Error::Malformed);
                    }
                    if stop.observations.iter().any(|observation| {
                        !matches!(
                            observation.kind.as_str(),
                            "decision" | "cost" | "limitation" | "asymmetry" | "unproven"
                        )
                    }) {
                        return Err(Error::Malformed);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Coarse progress from the server's memory-only job registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Collecting,
    Asking,
    Retrying,
    Assembling,
}

/// A failed read preserves any previously displayed walkthrough.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("this server has no walkthrough API")]
    Unsupported,
    #[error("the server returned an invalid walkthrough")]
    Malformed,
    #[error("the walkthrough request was refused (HTTP {status})")]
    Refused {
        status: u16,
        code: Option<String>,
        message: Option<String>,
    },
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl Error {
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Unsupported => Some("server-unsupported"),
            Self::Refused { code, .. } => code.as_deref(),
            _ => None,
        }
    }

    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Refused { message, .. } => message.as_deref(),
            _ => None,
        }
    }
}

impl ServerClient {
    fn walkthrough_read_request(
        &self,
        route: &str,
        directory: &str,
        source: &Source,
        options: Option<&Options>,
    ) -> Result<RequestBuilder, Error> {
        let mut url = self.endpoint().url(route);
        url.query_pairs_mut()
            .append_pair("directory", directory)
            .append_pair(
                "source",
                &serde_json::to_string(source).map_err(|_| Error::Malformed)?,
            );
        if let Some(options) = options {
            url.query_pairs_mut()
                .append_pair("language", &options.language);
            for (name, value) in [
                ("providerID", &options.provider_id),
                ("modelID", &options.model_id),
                ("model", &options.model),
            ] {
                if let Some(value) = value {
                    url.query_pairs_mut().append_pair(name, value);
                }
            }
        }
        Ok(self.authorized(Method::GET, url))
    }

    /// Reads cached prose and current hunks without spending model tokens.
    pub async fn read_walkthrough(
        &self,
        directory: &str,
        source: &Source,
        options: &Options,
    ) -> Result<Read, Error> {
        let mut read: Read = answer(self.walkthrough_read_request(
            "api/walkthrough",
            directory,
            source,
            Some(options),
        )?)
        .await?;
        read.validate(source)?;
        read.complete_coverage();
        Ok(read)
    }

    /// Starts or attaches to an explicit generation. Disconnecting does not
    /// cancel the server's paid work. No automatic retries occur here.
    pub async fn generate_walkthrough(
        &self,
        directory: &str,
        source: &Source,
        options: &Options,
        force: bool,
    ) -> Result<Read, Error> {
        let mut body = serde_json::to_value(options).map_err(|_| Error::Malformed)?;
        let map = body.as_object_mut().ok_or(Error::Malformed)?;
        map.insert("directory".into(), json!(directory));
        map.insert("source".into(), json!(source));
        map.insert("force".into(), json!(force));
        let request = self
            .authorized(
                Method::POST,
                self.endpoint().url("api/walkthrough/generate"),
            )
            .json(&body)
            .timeout(Duration::from_secs(1900));
        let mut read: Read = answer(request).await?;
        read.validate(source)?;
        read.complete_coverage();
        Ok(read)
    }

    /// Reads only running-job memory, never the Git diff again.
    pub async fn walkthrough_progress(
        &self,
        directory: &str,
        source: &Source,
    ) -> Result<Option<Stage>, Error> {
        #[derive(Deserialize)]
        struct Progress {
            stage: Option<Stage>,
        }
        let progress: Progress = answer(self.walkthrough_read_request(
            "api/walkthrough/progress",
            directory,
            source,
            None,
        )?)
        .await?;
        Ok(progress.stage)
    }

    /// Explicitly stops generation on the captured source.
    pub async fn cancel_walkthrough(&self, directory: &str, source: &Source) -> Result<(), Error> {
        let request = self
            .authorized(Method::POST, self.endpoint().url("api/walkthrough/cancel"))
            .json(&json!({ "directory": directory, "source": source }));
        let _: serde_json::Value = answer(request).await?;
        Ok(())
    }
}

async fn answer<T: serde::de::DeserializeOwned>(request: RequestBuilder) -> Result<T, Error> {
    // A model's missing login is a provider refusal, not revocation of the
    // native client's server credential.
    let response = request
        .send_for_provider()
        .await
        .map_err(TransportError::from_request)?;
    if response.server_lock {
        return Err(TransportError::AuthenticationRequired.into());
    }
    if response.status.is_redirection() {
        return Err(TransportError::Redirected.into());
    }
    if !response.json && (response.status.is_success() || response.status.as_u16() == 404) {
        return Err(Error::Unsupported);
    }
    if !response.status.is_success() {
        #[derive(Deserialize)]
        struct Refusal {
            error: Option<String>,
            code: Option<String>,
        }
        let refusal = serde_json::from_slice::<Refusal>(&response.bytes).ok();
        return Err(Error::Refused {
            status: response.status.as_u16(),
            code: refusal.as_ref().and_then(|value| value.code.clone()),
            message: refusal.and_then(|value| value.error),
        });
    }
    serde_json::from_slice(&response.bytes).map_err(|_| Error::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_are_server_owned_and_a_different_source_is_refused() {
        let read: Read = serde_json::from_value(json!({
            "source": { "kind": "working-tree", "scope": "working" },
            "walkthrough": null, "hunks": [{ "id": "opaque", "path": "a", "patch": "patch" }],
            "hunkCount": 1
        }))
        .expect("a complete fixture read");
        assert!(
            read.validate(&Source::WorkingTree {
                scope: Scope::Working
            })
            .is_ok()
        );
        assert!(
            read.validate(&Source::WorkingTree {
                scope: Scope::Staged
            })
            .is_err()
        );
        let mut duplicate = read.clone();
        duplicate.hunks.push(read.hunks[0].clone());
        duplicate.hunk_count = 2;
        assert!(
            duplicate.validate(&read.source).is_err(),
            "duplicate IDs cannot select a hunk"
        );
    }
}
