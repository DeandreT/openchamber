//! Strict desktop serialization boundary for retained composer choices.

use std::collections::HashSet;

use openchamber_native_core::composer_selection::{
    Effort, ManualSelection, SelectionOwner, SelectionSnapshot,
};
use openchamber_native_core::ids::ProjectId;
use openchamber_native_core::send_config::{ModelChoice, Selection};
use serde::{Deserialize, Serialize};

use crate::mutation_journal::{JournalError, Owner, StoredModel, identifier};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum StoredEffort {
    Inherit,
    Default,
    Named(String),
}

impl StoredEffort {
    fn capture(effort: &Effort) -> Self {
        match effort {
            Effort::Inherit => Self::Inherit,
            Effort::Default => Self::Default,
            Effort::Named(name) => Self::Named(name.clone()),
        }
    }
    fn restore(self) -> Effort {
        match self {
            Self::Inherit => Effort::Inherit,
            Self::Default => Effort::Default,
            Self::Named(name) => Effort::Named(name),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredChoice {
    agent: Option<String>,
    model: Option<StoredModel>,
}

impl StoredChoice {
    fn capture(choice: &Selection) -> Self {
        Self {
            agent: choice.agent_name.clone(),
            model: choice.model.as_ref().map(StoredModel::capture),
        }
    }
    fn restore(self) -> Selection {
        Selection {
            agent_name: self.agent,
            model: self.model.map(StoredModel::into_choice),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredSelection {
    owner: Owner,
    project: Option<String>,
    staged_from: Option<(Owner, Option<String>)>,
    inherited: StoredChoice,
    effort: StoredEffort,
    manual_agent: bool,
    manual_model: bool,
    agent_models: Vec<(String, StoredModel)>,
    efforts: Vec<(Option<String>, String, String, StoredEffort)>,
    followed_agent: Option<String>,
    followed_model: Option<StoredModel>,
    restored: Option<(StoredChoice, Option<String>)>,
}

impl StoredSelection {
    pub(crate) fn capture(owner: &SelectionOwner, state: &SelectionSnapshot) -> Self {
        Self {
            staged_from: state.staged_from.as_ref().map(|source| {
                (
                    Owner::capture(&source.draft),
                    source.project.as_ref().map(ToString::to_string),
                )
            }),
            owner: Owner::capture(&owner.draft),
            project: owner.project.as_ref().map(ToString::to_string),
            inherited: StoredChoice::capture(&state.inherited),
            effort: StoredEffort::capture(&state.effort),
            manual_agent: state.manual.agent,
            manual_model: state.manual.model,
            agent_models: state
                .agent_models
                .iter()
                .map(|(agent, model)| (agent.clone(), StoredModel::capture(model)))
                .collect(),
            efforts: state
                .efforts
                .iter()
                .map(|(agent, provider, model, effort)| {
                    (
                        agent.clone(),
                        provider.clone(),
                        model.clone(),
                        StoredEffort::capture(effort),
                    )
                })
                .collect(),
            followed_agent: state.followed_agent.clone(),
            followed_model: state.followed_model.as_ref().map(StoredModel::capture),
            restored: state
                .restored
                .as_ref()
                .map(|(choice, agent)| (StoredChoice::capture(choice), agent.clone())),
        }
    }

    pub(crate) fn restore(self) -> Result<(SelectionOwner, SelectionSnapshot), JournalError> {
        let owner = SelectionOwner {
            draft: self.owner.into_key()?,
            project: self.project.map(ProjectId::from),
        };
        let state = SelectionSnapshot {
            staged_from: self
                .staged_from
                .map(|(draft, project)| {
                    Ok::<_, JournalError>(SelectionOwner {
                        draft: draft.into_key()?,
                        project: project.map(ProjectId::from),
                    })
                })
                .transpose()?,
            inherited: self.inherited.restore(),
            effort: self.effort.restore(),
            manual: ManualSelection {
                agent: self.manual_agent,
                model: self.manual_model,
            },
            agent_models: self
                .agent_models
                .into_iter()
                .map(|(agent, model)| (agent, model.into_choice()))
                .collect(),
            efforts: self
                .efforts
                .into_iter()
                .map(|(agent, provider, model, effort)| (agent, provider, model, effort.restore()))
                .collect(),
            followed_agent: self.followed_agent,
            followed_model: self.followed_model.map(StoredModel::into_choice),
            restored: self
                .restored
                .map(|(choice, agent)| (choice.restore(), agent)),
        };
        Ok((owner, state))
    }
}

fn optional(value: Option<&str>) -> Result<(), JournalError> {
    value.map_or(Ok(()), identifier)
}
fn model(value: &ModelChoice) -> Result<(), JournalError> {
    identifier(&value.provider_id)?;
    identifier(&value.model_id)?;
    optional(value.variant.as_deref())
}
fn choice(value: &Selection) -> Result<(), JournalError> {
    optional(value.agent_name.as_deref())?;
    value.model.as_ref().map_or(Ok(()), model)
}
fn effort(value: &Effort) -> Result<(), JournalError> {
    if let Effort::Named(name) = value {
        identifier(name)?;
    }
    Ok(())
}

pub(crate) fn validate(
    records: &[(SelectionOwner, SelectionSnapshot)],
) -> Result<(), JournalError> {
    if records.len() > 4096 {
        return Err(JournalError::Invalid("selection count limit exceeded"));
    }
    let mut owners = HashSet::new();
    for (owner, state) in records {
        Owner::capture(&owner.draft).into_key()?;
        optional(owner.project.as_ref().map(|id| id.as_str()))?;
        if let Some(source) = &state.staged_from {
            Owner::capture(&source.draft).into_key()?;
            optional(source.project.as_ref().map(|id| id.as_str()))?;
            if !matches!(
                source.draft.target(),
                openchamber_native_core::draft::DraftTarget::NewSession(_)
            ) || !matches!(
                owner.draft.target(),
                openchamber_native_core::draft::DraftTarget::Session(_)
            ) || source.draft.runtime() != owner.draft.runtime()
                || source.draft.directory() != owner.draft.directory()
                || source.project != owner.project
            {
                return Err(JournalError::Invalid("invalid staged selection owner"));
            }
        }
        if !owners.insert(owner) {
            return Err(JournalError::Invalid("duplicate selection owner"));
        }
        choice(&state.inherited)?;
        effort(&state.effort)?;
        optional(state.followed_agent.as_deref())?;
        if let Some(value) = &state.followed_model {
            model(value)?;
        }
        if let Some((value, agent)) = &state.restored {
            choice(value)?;
            optional(agent.as_deref())?;
        }
        let mut agents = HashSet::new();
        for (agent, value) in &state.agent_models {
            identifier(agent)?;
            model(value)?;
            if !agents.insert(agent) {
                return Err(JournalError::Invalid("duplicate agent memory"));
            }
        }
        let mut efforts = HashSet::new();
        for (agent, provider, model, value) in &state.efforts {
            optional(agent.as_deref())?;
            identifier(provider)?;
            identifier(model)?;
            effort(value)?;
            if !efforts.insert((agent, provider, model)) {
                return Err(JournalError::Invalid("duplicate effort memory"));
            }
        }
    }
    Ok(())
}
