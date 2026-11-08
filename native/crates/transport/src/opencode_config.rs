//! OpenCode's own configuration as Settings reads it (`GET /api/config`,
//! folded like `mergeConfigDocuments`) and the compaction keys Settings
//! writes (`PUT /api/config/compaction`).

use reqwest::Method;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::client::{ServerClient, encode_directory};
use crate::error::TransportError;
use crate::files::read_answer;
use crate::wire;

/// One effective config: later documents win per key and record-valued keys
/// merge by name, so a project file adding one entry hides no global one.
pub type EffectiveConfig = Map<String, Value>;

pub(crate) fn merge_documents(entries: Vec<wire::ConfigEntry>) -> EffectiveConfig {
    let mut merged = Map::new();
    for entry in entries {
        let (true, Some(info)) = (entry.kind == "document", entry.info) else {
            continue;
        };
        for (key, value) in info {
            if value.is_null() {
                continue;
            }
            match (merged.get_mut(&key), value) {
                (Some(Value::Object(previous)), Value::Object(next)) => previous.extend(next),
                (_, value) => {
                    merged.insert(key, value);
                }
            }
        }
    }
    merged
}

/// A compaction buffer: a share of the model's window (`"20%"`) or a token
/// count from a hand-written config.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum CompactionBuffer {
    Share(String),
    Tokens(u64),
}

/// The compaction keys Settings edits; `None` leaves OpenCode's default.
#[derive(Clone, Debug, PartialEq)]
pub struct Compaction {
    /// `compaction.auto`; absent means on.
    pub auto: bool,
    pub buffer: Option<CompactionBuffer>,
    /// `compaction.keep.tokens`.
    pub keep_tokens: Option<u64>,
    /// `experimental.background_compaction`; absent means off.
    pub background: bool,
}

impl Compaction {
    pub fn from_config(config: &EffectiveConfig) -> Self {
        let compaction = config.get("compaction");
        let buffer = compaction.and_then(|value| value.get("buffer"));
        Self {
            auto: compaction
                .and_then(|value| value.get("auto"))
                .and_then(Value::as_bool)
                != Some(false),
            buffer: match buffer {
                Some(Value::String(share)) => Some(CompactionBuffer::Share(share.clone())),
                Some(value) => value.as_u64().map(CompactionBuffer::Tokens),
                None => None,
            },
            keep_tokens: compaction
                .and_then(|value| value.pointer("/keep/tokens"))
                .and_then(Value::as_u64),
            background: config
                .get("experimental")
                .and_then(|value| value.get("background_compaction"))
                .and_then(Value::as_bool)
                == Some(true),
        }
    }
}

/// One compaction field a write changes.
#[derive(Clone, Debug, PartialEq)]
pub enum CompactionChange {
    Auto(bool),
    Buffer(Option<CompactionBuffer>),
    KeepTokens(Option<u64>),
    Background(bool),
}

impl CompactionChange {
    fn body(&self) -> Value {
        match self {
            Self::Auto(auto) => json!({ "auto": auto }),
            Self::Buffer(buffer) => json!({ "buffer": buffer }),
            Self::KeepTokens(tokens) => json!({ "keepTokens": tokens }),
            Self::Background(background) => json!({ "background": background }),
        }
    }
}

impl ServerClient {
    /// The effective OpenCode config; `directory` adds its project documents.
    pub async fn opencode_config(
        &self,
        directory: Option<&str>,
    ) -> Result<EffectiveConfig, TransportError> {
        let entries: Vec<wire::ConfigEntry> = self
            .get_json(self.opencode_url("config"), directory)
            .await?;
        Ok(merge_documents(entries))
    }

    /// Writes one compaction field into the global OpenCode config.
    pub async fn save_compaction(&self, change: &CompactionChange) -> Result<(), TransportError> {
        self.put_config("api/config/compaction", change.body(), None)
            .await
    }

    /// Turns OpenCode's warming for every session on or off.
    pub async fn save_opencode_warming(&self, enabled: bool) -> Result<(), TransportError> {
        self.put_config("api/config/warming", json!({ "enabled": enabled }), None)
            .await
    }

    async fn put_config(
        &self,
        route: &str,
        body: Value,
        directory: Option<&str>,
    ) -> Result<(), TransportError> {
        let mut request = self
            .authorized(Method::PUT, self.endpoint().url(route))
            .json(&body);
        if let Some(directory) = directory {
            request = request.header("x-opencode-directory", encode_directory(directory));
        }
        let response = request.send().await.map_err(TransportError::from_request)?;
        match read_answer(response).await? {
            Ok(_) => Ok(()),
            Err(refusal) => Err(TransportError::Http {
                status: refusal.status.as_u16(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(value: Value) -> Vec<wire::ConfigEntry> {
        serde_json::from_value(value).expect("config entries")
    }

    #[test]
    fn documents_fold_with_later_ones_winning_and_records_merging() {
        let merged = merge_documents(entries(json!([
            {"type": "document", "info": {"compaction": {"auto": false, "buffer": "20%"}, "model": "a/b"}},
            {"type": "folder", "info": {"model": "ignored/c"}},
            {"type": "document", "info": {"compaction": {"auto": true}, "model": "x/y"}},
        ])));
        assert_eq!(merged["model"], "x/y");
        assert_eq!(merged["compaction"], json!({"auto": true, "buffer": "20%"}));
    }

    #[test]
    fn compaction_reads_defaults_and_hand_written_values() {
        let empty = Compaction::from_config(&Map::new());
        assert_eq!(
            empty,
            Compaction {
                auto: true,
                buffer: None,
                keep_tokens: None,
                background: false
            }
        );
        let config = merge_documents(entries(json!([{"type": "document", "info": {
            "compaction": {"auto": false, "buffer": 30000, "keep": {"tokens": 50000}},
            "experimental": {"background_compaction": true},
        }}])));
        let read = Compaction::from_config(&config);
        assert!(!read.auto);
        assert_eq!(read.buffer, Some(CompactionBuffer::Tokens(30_000)));
        assert_eq!(read.keep_tokens, Some(50_000));
        assert!(read.background);
    }

    #[tokio::test]
    async fn each_change_writes_only_its_own_field() {
        let (client, recorded) = crate::test_server::serve(Box::new(|request| {
            if request.body.get("auto").is_some() {
                (500, json!({"error": "disk full"}))
            } else {
                (200, json!({"success": true, "changed": true}))
            }
        }))
        .await;
        client
            .save_compaction(&CompactionChange::Buffer(Some(CompactionBuffer::Share(
                "20%".into(),
            ))))
            .await
            .expect("buffer");
        client
            .save_compaction(&CompactionChange::KeepTokens(None))
            .await
            .expect("keep");
        assert!(matches!(
            client.save_compaction(&CompactionChange::Auto(false)).await,
            Err(TransportError::Http { status: 500 })
        ));
        client.save_opencode_warming(true).await.expect("warming");
        let recorded = recorded.lock().expect("log");
        assert_eq!(recorded[0].path, "/api/config/compaction");
        assert_eq!(recorded[0].body, json!({"buffer": "20%"}));
        assert_eq!(
            recorded[1].body,
            json!({"keepTokens": null}),
            "null restores the default"
        );
        assert_eq!(recorded[3].path, "/api/config/warming");
        assert_eq!(recorded[3].body, json!({"enabled": true}));
    }
}
