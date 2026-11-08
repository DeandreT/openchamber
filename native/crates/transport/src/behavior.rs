//! Settings → Behavior: the global `AGENTS.md` OpenCode reads at runtime
//! (`/api/behavior/agents-md`) and the response style in the settings
//! document, as `BehaviorPage.tsx` reads and writes them.

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::client::ServerClient;
use crate::error::TransportError;
use crate::files::{self, read_answer};

/// The presets `responseStyle.ts` offers, in its order.
pub const RESPONSE_STYLE_PRESETS: [&str; 7] = [
    "concise",
    "detailed",
    "mentor",
    "pushback",
    "noFiller",
    "matchEnergy",
    "warmPeer",
];

/// The global `AGENTS.md` as the server read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentsMd {
    /// `None` when the file does not exist; an existing empty file is `Some("")`.
    pub content: Option<String>,
    /// The file's absolute path on the server machine.
    pub path: Option<String>,
}

/// Why an `AGENTS.md` write did not land.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AgentsMdError {
    /// The file changed on disk since it was read; nothing was written.
    #[error("AGENTS.md changed on disk since it was loaded")]
    Conflict,
    #[error("the server refused the write (HTTP {status})")]
    Refused {
        status: u16,
        message: Option<String>,
    },
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl AgentsMdError {
    /// The server's own explanation, when it gave one.
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Refused { message, .. } => message.as_deref(),
            _ => None,
        }
    }
}

/// The settings fields the Behavior page owns. Each value is checked on its
/// own; a malformed one reads as absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BehaviorSettings {
    /// The persisted copy of the prompt, used only when `AGENTS.md` is missing.
    pub global_behavior_prompt: Option<String>,
    pub response_style_enabled: bool,
    /// A preset from `RESPONSE_STYLE_PRESETS` or `custom`; `concise` otherwise.
    pub response_style_preset: String,
    pub response_style_custom_instructions: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsWire {
    global_behavior_prompt: Option<Value>,
    response_style_enabled: Option<Value>,
    response_style_preset: Option<Value>,
    response_style_custom_instructions: Option<Value>,
}

impl BehaviorSettings {
    fn from_wire(wire: SettingsWire) -> Self {
        let preset = wire
            .response_style_preset
            .as_ref()
            .and_then(Value::as_str)
            .filter(|preset| *preset == "custom" || RESPONSE_STYLE_PRESETS.contains(preset))
            .unwrap_or("concise");
        Self {
            global_behavior_prompt: wire
                .global_behavior_prompt
                .as_ref()
                .and_then(Value::as_str)
                .map(str::to_owned),
            response_style_enabled: wire
                .response_style_enabled
                .as_ref()
                .and_then(Value::as_bool)
                == Some(true),
            response_style_preset: preset.to_owned(),
            response_style_custom_instructions: wire
                .response_style_custom_instructions
                .as_ref()
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }
    }
}

/// One Behavior settings write. The prompt copy is sent only when the prompt
/// changed; the response style is always sent whole, as the web page does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BehaviorChanges {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global_behavior_prompt: Option<String>,
    pub response_style_enabled: bool,
    pub response_style_preset: String,
    pub response_style_custom_instructions: String,
}

#[derive(Deserialize)]
struct AgentsMdWire {
    content: String,
    exists: bool,
    path: Option<String>,
}

impl ServerClient {
    /// `GET /api/behavior/agents-md`.
    pub async fn agents_md(&self) -> Result<AgentsMd, TransportError> {
        let body: AgentsMdWire = self
            .get_json(self.endpoint().url("api/behavior/agents-md"), None)
            .await?;
        Ok(AgentsMd {
            content: body.exists.then_some(body.content),
            path: body.path.filter(|path| !path.is_empty()),
        })
    }

    /// `PUT /api/behavior/agents-md`. `expected` is the file as last read or
    /// written (`None`: no file); the server refuses to replace a file that
    /// differs from it.
    pub async fn save_agents_md(
        &self,
        content: &str,
        expected: Option<&str>,
    ) -> Result<(), AgentsMdError> {
        let response = self
            .authorized(Method::PUT, self.endpoint().url("api/behavior/agents-md"))
            .json(&json!({ "content": content, "expectedContent": expected }))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        match read_answer(response).await? {
            Ok(_) => Ok(()),
            Err(refusal) if refusal.status.as_u16() == 409 => Err(AgentsMdError::Conflict),
            Err(refusal) => Err(AgentsMdError::Refused {
                status: refusal.status.as_u16(),
                message: refusal.message,
            }),
        }
    }

    /// The Behavior fields of the settings document.
    pub async fn behavior_settings(&self) -> Result<BehaviorSettings, TransportError> {
        let wire: SettingsWire = self.get_json(self.settings_url(), None).await?;
        Ok(BehaviorSettings::from_wire(wire))
    }

    /// Saves the Behavior fields and returns them as the server merged them.
    pub async fn save_behavior_settings(
        &self,
        changes: &BehaviorChanges,
    ) -> Result<BehaviorSettings, TransportError> {
        let response = self
            .authorized(Method::PUT, self.settings_url())
            .json(changes)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let bytes = match read_answer(response).await? {
            Ok(bytes) => bytes,
            Err(refusal) => {
                return Err(TransportError::Http {
                    status: refusal.status.as_u16(),
                });
            }
        };
        let wire: SettingsWire = files::parse(&bytes)?;
        Ok(BehaviorSettings::from_wire(wire))
    }
}

/// The instructions a preset stands for (`getResponseStylePresetInstructions`).
/// They are model instructions, so they are not translated.
pub fn response_style_instructions(preset: &str) -> Option<&'static str> {
    Some(match preset {
        "concise" => {
            "Keep replies short. Answer first, no preamble or recap of the question. Write like you're texting a colleague who already has the context — plain sentences, not headings or bullets. Reach for a list only when the content is genuinely a list; never use one to look organised."
        }
        "detailed" => {
            "Take the space you need to actually explain things. Walk through what's going on, why it matters, and where the real tradeoffs are. Prefer flowing prose over bullet points and headings — structure the answer with paragraphs and let the reasoning carry it. Lists are fine when something really is enumerable, but don't fragment a normal explanation into bullets."
        }
        "mentor" => {
            "Talk like a patient senior engineer pairing with someone less experienced. Explain the underlying idea before the answer, think out loud about how you'd approach it, and drop in a small concrete example when it actually helps. Keep it conversational — no lecture format, no checklists, no numbered steps unless the task literally is a sequence."
        }
        "pushback" => {
            "Don't agree automatically. If something I say sounds off — a wrong assumption, a flawed approach, a request that won't actually do what I think it will — push back first. Explain what you disagree with and why, and only proceed once I've responded. Disagreement is welcome; sycophancy is not. Don't soften it with 'you might want to consider' — just say it."
        }
        "noFiller" => {
            "Cut the filler. No 'Great question', no 'Certainly', no 'I'll help you with that', no restating what I just asked. No closing summary of what you did when the diff or output already shows it. No trailing 'let me know if you need anything else'. Open with the actual content and stop when you're done."
        }
        "matchEnergy" => {
            "Mirror the size and register of my message. A one-line question gets a one-line answer. A casual aside gets a casual reply, not a structured breakdown. If I write three words, don't respond with three paragraphs. Match the tone too — informal stays informal, technical stays technical. Don't inflate small asks into full essays."
        }
        "warmPeer" => {
            "Talk like a colleague, not an assistant. First person is fine and encouraged — 'I'd do this', 'I don't love that approach', 'that was sloppy of me'. Have actual opinions and share them. Push back when you disagree. Admit when you screwed up without grovelling. Skip the corporate helpfulness and performative politeness — just be a person."
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(value: Value) -> BehaviorSettings {
        BehaviorSettings::from_wire(serde_json::from_value(value).expect("settings wire"))
    }

    #[test]
    fn settings_read_each_field_on_its_own() {
        assert_eq!(
            wire(json!({})),
            BehaviorSettings {
                global_behavior_prompt: None,
                response_style_enabled: false,
                response_style_preset: "concise".into(),
                response_style_custom_instructions: String::new(),
            },
            "absent fields read as the page's defaults"
        );
        let read = wire(json!({
            "globalBehaviorPrompt": "Be kind.",
            "responseStyleEnabled": "yes",
            "responseStylePreset": "warmPeer",
            "responseStyleCustomInstructions": 7,
        }));
        assert_eq!(read.global_behavior_prompt.as_deref(), Some("Be kind."));
        assert!(!read.response_style_enabled, "only a boolean enables");
        assert_eq!(read.response_style_preset, "warmPeer");
        assert_eq!(read.response_style_custom_instructions, "");
        assert_eq!(
            wire(json!({"responseStylePreset": "loud"})).response_style_preset,
            "concise",
            "an unknown preset is sanitized"
        );
        assert_eq!(
            wire(json!({"responseStylePreset": "custom"})).response_style_preset,
            "custom"
        );
    }

    #[test]
    fn changes_send_the_prompt_copy_only_when_given() {
        let changes = BehaviorChanges {
            global_behavior_prompt: None,
            response_style_enabled: true,
            response_style_preset: "mentor".into(),
            response_style_custom_instructions: String::new(),
        };
        assert_eq!(
            serde_json::to_value(&changes).expect("changes"),
            json!({
                "responseStyleEnabled": true,
                "responseStylePreset": "mentor",
                "responseStyleCustomInstructions": "",
            })
        );
        let with_prompt = BehaviorChanges {
            global_behavior_prompt: Some("Rules\n".into()),
            ..changes
        };
        assert_eq!(
            serde_json::to_value(&with_prompt).expect("changes")["globalBehaviorPrompt"],
            "Rules\n"
        );
    }

    #[tokio::test]
    async fn agents_md_reads_missing_and_empty_files_apart_and_reports_a_conflict() {
        let (client, recorded) = crate::test_server::serve(Box::new(|request| {
            match (request.method.as_str(), &request.body["expectedContent"]) {
                ("GET", _) => (
                    200,
                    json!({"content": "", "exists": true, "path": "/cfg/AGENTS.md"}),
                ),
                ("PUT", Value::Null) => (
                    409,
                    json!({"error": "changed", "code": "AGENTS_MD_CONFLICT"}),
                ),
                ("PUT", Value::String(expected)) if expected == "boom" => {
                    (413, json!({"error": "Content exceeds maximum size"}))
                }
                _ => (200, json!({"success": true})),
            }
        }))
        .await;
        let read = client.agents_md().await.expect("read");
        assert_eq!(read.content.as_deref(), Some(""), "an empty file exists");
        assert_eq!(read.path.as_deref(), Some("/cfg/AGENTS.md"));
        assert!(matches!(
            client.save_agents_md("x\n", None).await,
            Err(AgentsMdError::Conflict)
        ));
        let refused = client
            .save_agents_md("x\n", Some("boom"))
            .await
            .expect_err("refused");
        assert_eq!(refused.message(), Some("Content exceeds maximum size"));
        client.save_agents_md("x\n", Some("")).await.expect("saved");
        let recorded = recorded.lock().expect("log");
        assert_eq!(recorded[1].path, "/api/behavior/agents-md");
        assert_eq!(
            recorded[1].body,
            json!({"content": "x\n", "expectedContent": null}),
            "a missing file is sent as an explicit null"
        );
    }

    #[test]
    fn every_preset_has_instructions() {
        for preset in RESPONSE_STYLE_PRESETS {
            assert!(response_style_instructions(preset).is_some(), "{preset}");
        }
        assert_eq!(response_style_instructions("custom"), None);
    }
}
