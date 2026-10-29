//! Mutable composer choices, scoped to a captured draft/session and project.
//!
//! This module has no I/O and never holds a pending send. `selection()` returns
//! an owned snapshot; later picker changes cannot alter a captured submission.
//! Callers retain this store across composer remounts, pass authoritative catalog
//! status explicitly, and reconcile only ordered records from the owner's runtime.

use std::collections::HashMap;

use crate::draft::{DraftKey, DraftTarget};
use crate::ids::ProjectId;
use crate::send_config;
use crate::send_config::{
    AgentEntry, AutoRouting, CatalogProvider, ModelChoice, Selection, SelectionDefaults,
    SessionRecordChoice,
};

/// A picker effort override. A missing historical effort means `Inherit`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Effort {
    #[default]
    Inherit,
    /// Explicitly omit effort, even when the agent or settings supply one.
    Default,
    Named(String),
}

impl Effort {
    /// Resolves the override to the outgoing optional variant.
    #[must_use]
    pub fn resolve(&self, inherited: Option<&str>) -> Option<String> {
        match self {
            Self::Inherit => inherited.map(str::to_owned),
            Self::Default => None,
            Self::Named(name) => Some(name.clone()),
        }
    }
}

/// Defaults and manual choices belong to this exact runtime/draft/project tuple.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SelectionOwner {
    pub draft: DraftKey,
    pub project: Option<ProjectId>,
}

impl SelectionOwner {
    fn is_session(&self) -> bool {
        matches!(self.draft.target(), DraftTarget::Session(_))
    }
}

/// Inputs captured for this owner's directory. Failed catalog reads use `false`.
#[derive(Debug)]
pub struct SelectionContext<'a> {
    pub agents: &'a [AgentEntry],
    pub providers: &'a [CatalogProvider],
    pub defaults: &'a SelectionDefaults,
    /// Only a known model in an authoritative catalog can invalidate an effort.
    pub catalog_authoritative: bool,
}

/// Agent and model provenance are independent, as in the config store.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ManualSelection {
    pub agent: bool,
    pub model: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct EffortKey {
    agent: Option<String>,
    provider: String,
    model: String,
}

/// One owner's retained selection. Changes go through `ComposerSelections`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ComposerSelection {
    // The model's variant holds inheritance; `effort` is the separate override.
    inherited: Selection,
    effort: Effort,
    manual: ManualSelection,
    agent_models: HashMap<String, ModelChoice>,
    efforts: HashMap<EffortKey, Effort>,
    followed_agent: Option<String>,
    followed_model: Option<ModelChoice>,
    // The record agent invalidates restoration without being reapplied as a
    // candidate agent over a newer local agent pick on an unchanged echo.
    restored: Option<(Selection, Option<String>)>,
}

impl ComposerSelection {
    /// Captures the choices for a new send. Existing sends keep their own copy.
    #[must_use]
    pub fn selection(&self) -> Selection {
        let mut result = self.inherited.clone();
        if let Some(model) = &mut result.model {
            model.variant = self.effort.resolve(model.variant.as_deref());
        }
        result
    }

    #[must_use]
    pub fn effort(&self) -> &Effort {
        &self.effort
    }

    #[must_use]
    pub fn manual(&self) -> ManualSelection {
        self.manual
    }

    fn effort_key(&self, model: &ModelChoice) -> EffortKey {
        EffortKey {
            agent: self.inherited.agent_name.clone(),
            provider: model.provider_id.clone(),
            model: model.model_id.clone(),
        }
    }

    fn remember_effort(&mut self) {
        if let Some(model) = &self.inherited.model {
            let key = self.effort_key(model);
            if self.effort == Effort::Inherit {
                self.efforts.remove(&key);
            } else {
                self.efforts.insert(key, self.effort.clone());
            }
        }
    }

    fn remember_model(&mut self) {
        if let (Some(agent), Some(model)) = (&self.inherited.agent_name, &self.inherited.model) {
            self.agent_models.insert(agent.clone(), model.clone());
        }
    }

    fn apply_model(&mut self, model: ModelChoice, session: bool, context: &SelectionContext<'_>) {
        let previous = self.inherited.model.as_ref();
        let same = previous.is_some_and(|previous| same_model(previous, &model));
        let live_effort = self.effort.clone();
        let defaults = send_config::resolve_default_selection(
            context.agents,
            context.providers,
            context.defaults,
        );
        let baseline = Selection {
            agent_name: self.inherited.agent_name.clone(),
            model: if same {
                previous.cloned()
            } else {
                defaults.model
            },
        };
        let mut unvaried = model.clone();
        unvaried.variant = None;
        let resolved = send_config::resolve_session_selection(
            &baseline,
            SessionRecordChoice {
                model: Some(&unvaried),
                agent: None,
            },
            context.agents,
            context.providers,
            context.defaults,
        );
        let known = send_config::find_model(context.providers, &model);
        let inherited = if !context.catalog_authoritative || known.is_none() {
            // Absence cannot invalidate a retained choice during discovery.
            model.variant.clone().or_else(|| {
                baseline
                    .model
                    .as_ref()
                    .filter(|base| same_model(base, &model))
                    .and_then(|base| base.variant.clone())
            })
        } else {
            resolved.model.and_then(|base| base.variant)
        };
        self.effort = self
            .efforts
            .get(&self.effort_key(&model))
            .cloned()
            .unwrap_or({
                if !session && same {
                    live_effort
                } else {
                    Effort::Inherit
                }
            });
        self.inherited.model = Some(ModelChoice {
            variant: inherited,
            ..model
        });
        self.validate_effort(context);
    }

    // Settings and unchanged record echoes can refresh inheritance without
    // selecting a model, changing provenance, or replacing an explicit override.
    fn refresh_inherited(&mut self, context: &SelectionContext<'_>) {
        if let Some(model) = self.inherited.model.clone() {
            let effort = self.effort.clone();
            self.apply_model(model, true, context);
            self.effort = effort;
            self.validate_effort(context);
        }
    }

    fn validate_effort(&mut self, context: &SelectionContext<'_>) {
        if !context.catalog_authoritative {
            return;
        }
        let Some(model) = self.inherited.model.as_mut() else {
            return;
        };
        let Some(known) = send_config::find_model(context.providers, model) else {
            return;
        };
        if model
            .variant
            .as_ref()
            .is_some_and(|variant| !known.variants.contains(variant))
        {
            model.variant = None;
        }
        if matches!(&self.effort, Effort::Named(name) if !known.variants.contains(name)) {
            self.effort = Effort::Inherit;
            self.remember_effort();
        }
    }
}

fn same_model(left: &ModelChoice, right: &ModelChoice) -> bool {
    left.provider_id == right.provider_id && left.model_id == right.model_id
}

fn is_auto(model: &ModelChoice) -> bool {
    model.provider_id == "openchamber" && model.model_id == "auto"
}

/// Whether a restore completed or must be retried after catalog/history loading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreResult {
    Complete,
    Pending,
    /// The caller has not opened this owner, or it is a new-session draft.
    WrongOwner,
}

/// Whether the source draft acquired new text during session creation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreationSource {
    Consumed,
    Continued,
}

/// Retains independent composer state. Opening an owner never carries another's flags.
#[derive(Clone, Debug, Default)]
pub struct ComposerSelections {
    owners: HashMap<SelectionOwner, ComposerSelection>,
    staged: HashMap<SelectionOwner, (SelectionOwner, ComposerSelection)>,
}

impl ComposerSelections {
    /// Captures retained owners for a serialized journal boundary.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(SelectionOwner, SelectionSnapshot)> {
        let mut records: Vec<_> = self
            .owners
            .iter()
            .filter(|(owner, _)| !self.staged.contains_key(*owner))
            .map(|(owner, state)| (owner.clone(), state.snapshot()))
            .collect();
        records.extend(self.staged.iter().map(|(owner, (source, state))| {
            // A session can be opened and edited while creation is pending.
            // Persist its newest choice once, with the pending handoff marker.
            let mut snapshot = self.owners.get(owner).unwrap_or(state).snapshot();
            snapshot.staged_from = Some(source.clone());
            (owner.clone(), snapshot)
        }));
        records
    }

    /// Restores validated journal records without resolving defaults or sending.
    pub fn restore(records: Vec<(SelectionOwner, SelectionSnapshot)>) -> Self {
        let mut store = Self::default();
        for (owner, mut state) in records {
            if let Some(source) = state.staged_from.take() {
                store
                    .staged
                    .insert(owner, (source, ComposerSelection::restore(state)));
            } else {
                store
                    .owners
                    .insert(owner, ComposerSelection::restore(state));
            }
        }
        store
    }

    /// Captures choice memory with a creation attempt without exposing a session.
    pub fn stage_creation(&mut self, from: &SelectionOwner, to: SelectionOwner) -> bool {
        if from.is_session()
            || !to.is_session()
            || from.draft.runtime() != to.draft.runtime()
            || from.draft.directory() != to.draft.directory()
            || from.project != to.project
            || self.owners.contains_key(&to)
            || self.staged.contains_key(&to)
        {
            return false;
        }
        let Some(state) = self.owners.get(from) else {
            return false;
        };
        self.staged.insert(to, (from.clone(), state.clone()));
        true
    }

    /// Commits the captured choice in the same transaction as creation-to-send.
    /// A newer source choice remains available for the next project draft.
    pub fn commit_creation(
        &mut self,
        to: SelectionOwner,
        fallback: Selection,
        source: CreationSource,
    ) {
        let state = if let Some((owner, state)) = self.staged.remove(&to) {
            if source == CreationSource::Consumed && self.owners.get(&owner) == Some(&state) {
                self.owners.remove(&owner);
            }
            state
        } else {
            ComposerSelection {
                effort: fallback
                    .model
                    .as_ref()
                    .and_then(|model| model.variant.clone())
                    .map_or(Effort::Default, Effort::Named),
                inherited: fallback,
                manual: ManualSelection {
                    agent: true,
                    model: true,
                },
                ..ComposerSelection::default()
            }
        };
        self.owners.entry(to).or_insert(state);
    }
    /// Opens or revisits an exact owner. Revisit retains local picks and follow baselines.
    pub fn open(
        &mut self,
        owner: SelectionOwner,
        context: &SelectionContext<'_>,
    ) -> &ComposerSelection {
        self.owners.entry(owner.clone()).or_insert_with(|| {
            self.staged
                .get(&owner)
                .map(|(_, state)| state.clone())
                .unwrap_or_else(|| ComposerSelection {
                    inherited: send_config::resolve_default_selection(
                        context.agents,
                        context.providers,
                        context.defaults,
                    ),
                    ..ComposerSelection::default()
                })
        })
    }

    #[must_use]
    pub fn get(&self, owner: &SelectionOwner) -> Option<&ComposerSelection> {
        self.owners.get(owner)
    }

    /// Transfers a newly created draft to its session. An existing destination wins.
    /// Runtime, actual directory and project must agree. Failure changes neither owner.
    pub fn materialize(&mut self, from: &SelectionOwner, to: SelectionOwner) -> bool {
        if from.is_session()
            || !to.is_session()
            || from.draft.runtime() != to.draft.runtime()
            || from.draft.directory() != to.draft.directory()
            || from.project != to.project
            || self.owners.contains_key(&to)
        {
            return false;
        }
        let Some(state) = self.owners.remove(from) else {
            return false;
        };
        self.owners.insert(to, state);
        true
    }

    /// Drops only this owner's memory, for an explicit discard or session deletion.
    pub fn forget(&mut self, owner: &SelectionOwner) {
        self.owners.remove(owner);
    }

    /// Re-resolves draft defaults. Either manual flag protects its valid pair.
    /// Existing sessions fill missing choices and refresh inherited effort.
    /// A settings completion must use the owner captured when that load started.
    pub fn refresh_defaults(&mut self, owner: &SelectionOwner, context: &SelectionContext<'_>) {
        let Some(state) = self.owners.get_mut(owner) else {
            return;
        };
        if state.inherited.model.as_ref().is_some_and(is_auto)
            && matches!(
                context.defaults.auto_routing,
                AutoRouting::Unavailable | AutoRouting::NotReady
            )
        {
            let fallback = send_config::resolve_default_selection(
                context.agents,
                context.providers,
                context.defaults,
            )
            .model;
            if let Some(model) = fallback.filter(|model| !is_auto(model)) {
                state.apply_model(model, owner.is_session(), context);
                state.manual.model = false;
            }
        }
        if owner.is_session() {
            let resolved = send_config::resolve_default_selection(
                context.agents,
                context.providers,
                context.defaults,
            );
            if state.inherited.agent_name.is_none() && !state.manual.agent {
                state.inherited.agent_name = resolved.agent_name;
            }
            if state.inherited.model.is_none() && !state.manual.model {
                state.inherited.model = resolved.model;
            }
            state.refresh_inherited(context);
            return;
        }
        let resolved = send_config::resolve_default_selection(
            context.agents,
            context.providers,
            context.defaults,
        );
        let preserve =
            state.manual.model || (state.manual.agent && state.inherited.agent_name.is_some());
        if !preserve {
            let old = state.inherited.model.clone();
            state.inherited.agent_name = resolved.agent_name;
            if let Some(model) = resolved.model {
                state.apply_model(model.clone(), false, context);
                // Fresh defaults supply inheritance, while same-model overrides survive.
                if context.catalog_authoritative {
                    state
                        .inherited
                        .model
                        .as_mut()
                        .expect("apply_model sets a model")
                        .variant = model.variant;
                }
            } else if old.is_none() {
                state.effort = Effort::Inherit;
            }
        } else {
            if state.inherited.agent_name.is_none() {
                state.inherited.agent_name = resolved.agent_name;
            }
            if state.inherited.model.is_none() {
                state.inherited.model = resolved.model;
            }
        }
        state.validate_effort(context);
    }

    /// Records a model pick. `None` restores this agent/model's effort memory;
    /// `Some(Inherit)` explicitly forgets it. The model's variant is inheritance.
    pub fn pick_model(
        &mut self,
        owner: &SelectionOwner,
        model: ModelChoice,
        effort: Option<Effort>,
        context: &SelectionContext<'_>,
    ) {
        if model.provider_id.is_empty() || model.model_id.is_empty() {
            return;
        }
        let Some(state) = self.owners.get_mut(owner) else {
            return;
        };
        state.apply_model(model, owner.is_session(), context);
        if let Some(effort) = effort {
            state.effort = effort;
        }
        state.manual.model = true;
        state.validate_effort(context);
        state.remember_effort();
        state.remember_model();
    }

    /// Records an effort without marking the model or agent as manually selected.
    pub fn pick_effort(
        &mut self,
        owner: &SelectionOwner,
        effort: Effort,
        context: &SelectionContext<'_>,
    ) {
        let Some(state) = self.owners.get_mut(owner) else {
            return;
        };
        state.refresh_inherited(context);
        state.effort = effort;
        state.validate_effort(context);
        state.remember_effort();
    }

    /// Picks an agent using session memory, an available pin, a compatible manual
    /// model, an available configured default, then the current model.
    pub fn pick_agent(
        &mut self,
        owner: &SelectionOwner,
        agent: &str,
        context: &SelectionContext<'_>,
    ) {
        if agent.is_empty() {
            return;
        }
        let Some(state) = self.owners.get_mut(owner) else {
            return;
        };
        let target = context.agents.iter().find(|entry| entry.name == agent);
        let previous = context
            .agents
            .iter()
            .find(|entry| Some(&entry.name) == state.inherited.agent_name.as_ref());
        let pin = target.and_then(|entry| entry.model.as_ref());
        let previous_pinned = previous
            .and_then(|entry| entry.model.as_ref())
            .zip(state.inherited.model.as_ref())
            .is_some_and(|(pin, model)| same_model(pin, model));
        let remembered = owner
            .is_session()
            .then(|| state.agent_models.get(agent))
            .flatten();
        let available_pin =
            pin.filter(|pin| send_config::find_model(context.providers, pin).is_some());
        let compatible = pin.is_none() && !previous_pinned && state.manual.model;
        let configured = send_config::resolve_default_selection(
            &[],
            context.providers,
            &SelectionDefaults {
                allow_fallback: false,
                ..context.defaults.clone()
            },
        )
        .model;
        let (model, manual) = if let Some(model) = remembered {
            (Some(model.clone()), true)
        } else if let Some(model) = available_pin {
            (Some(model.clone()), false)
        } else if compatible {
            (state.inherited.model.clone(), true)
        } else if let Some(model) =
            configured.filter(|model| send_config::find_model(context.providers, model).is_some())
        {
            (Some(model), false)
        } else {
            (state.inherited.model.clone(), state.manual.model)
        };
        state.inherited.agent_name = Some(agent.to_owned());
        state.manual.agent = true;
        state.manual.model = manual;
        if let Some(model) = model {
            state.apply_model(model, owner.is_session(), context);
        }
        if owner.is_session() {
            state.remember_model();
        }
    }

    /// Follows the record before a fully loaded latest-assistant fallback.
    /// Missing catalogs retain the choice and leave restore pending. Record
    /// agent changes keep the active model; model changes require a prior baseline.
    pub fn reconcile(
        &mut self,
        owner: &SelectionOwner,
        record: SessionRecordChoice<'_>,
        latest_assistant: Option<&Selection>,
        context: &SelectionContext<'_>,
    ) -> RestoreResult {
        if !owner.is_session() {
            return RestoreResult::WrongOwner;
        }
        let Some(state) = self.owners.get_mut(owner) else {
            return RestoreResult::WrongOwner;
        };
        state.refresh_inherited(context);
        if let Some(agent) = record.agent.filter(|agent| !agent.is_empty())
            && state.followed_agent.as_deref() != Some(agent)
        {
            state.followed_agent = Some(agent.to_owned());
            state.inherited.agent_name = Some(agent.to_owned());
            state.manual.agent = false;
            // Follow only the agent. Its remembered model is unrelated to
            // the model this session currently runs on, but effort is scoped
            // to the new agent and the unchanged active model.
            if let Some(model) = state.inherited.model.clone() {
                state.apply_model(model, true, context);
            }
        }
        if !context.catalog_authoritative {
            return RestoreResult::Pending;
        }
        let candidate = if let Some(model) = record.model {
            Selection {
                agent_name: None,
                model: Some(model.clone()),
            }
        } else if let Some(reply) = latest_assistant {
            Selection {
                agent_name: if record.agent.is_none() {
                    reply.agent_name.clone()
                } else {
                    None
                },
                model: reply.model.clone(),
            }
        } else {
            return RestoreResult::Pending;
        };
        let Some(model) = &candidate.model else {
            return RestoreResult::Pending;
        };
        let saved_auto = state.inherited.model.as_ref().is_some_and(is_auto);
        let stale_auto = saved_auto
            && matches!(
                context.defaults.auto_routing,
                AutoRouting::Unavailable | AutoRouting::NotReady
            );
        if state
            .restored
            .as_ref()
            .is_some_and(|(choice, agent)| choice == &candidate && agent.as_deref() == record.agent)
            && !stale_auto
        {
            state.validate_effort(context);
            return RestoreResult::Complete;
        }
        let switched = record.model.is_some_and(|model| {
            state
                .followed_model
                .as_ref()
                .is_some_and(|old| old != model)
        });
        if saved_auto && context.defaults.auto_routing == AutoRouting::Unknown {
            return RestoreResult::Pending;
        }
        let keep_auto = saved_auto && context.defaults.auto_routing == AutoRouting::Ready;
        let keep_manual =
            !saved_auto && !switched && state.manual.model && state.inherited.model.is_some();
        if keep_auto || keep_manual {
            if !keep_auto
                && state.inherited.model.as_ref().is_some_and(|choice| {
                    send_config::find_model(context.providers, choice).is_none()
                })
            {
                return RestoreResult::Pending;
            }
        } else {
            if send_config::find_model(context.providers, model).is_none() {
                return RestoreResult::Pending;
            }
            if let Some(agent) = &candidate.agent_name {
                state.inherited.agent_name = Some(agent.clone());
            }
            state.apply_model(model.clone(), true, context);
            let historical = model.variant.as_ref().filter(|variant| {
                send_config::find_model(context.providers, model)
                    .is_some_and(|known| known.variants.contains(variant))
            });
            if (switched || state.effort == Effort::Inherit)
                && let Some(variant) = historical
            {
                state.effort = Effort::Named(variant.clone());
            }
            state.manual.model = false;
            state.remember_effort();
            state.remember_model();
        }
        if let Some(model) = record.model {
            state.followed_model = Some(model.clone());
        }
        state.restored = Some((candidate, record.agent.map(str::to_owned)));
        state.validate_effort(context);
        RestoreResult::Complete
    }
}

/// Renderer-independent persistence data. The I/O boundary validates identifiers,
/// duplicate keys and format bounds before constructing these records.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectionSnapshot {
    /// Present only while the target belongs to an unresolved creation.
    pub staged_from: Option<SelectionOwner>,
    pub inherited: Selection,
    pub effort: Effort,
    pub manual: ManualSelection,
    pub agent_models: Vec<(String, ModelChoice)>,
    pub efforts: Vec<(Option<String>, String, String, Effort)>,
    pub followed_agent: Option<String>,
    pub followed_model: Option<ModelChoice>,
    pub restored: Option<(Selection, Option<String>)>,
}

impl ComposerSelection {
    fn snapshot(&self) -> SelectionSnapshot {
        SelectionSnapshot {
            staged_from: None,
            inherited: self.inherited.clone(),
            effort: self.effort.clone(),
            manual: self.manual,
            agent_models: self
                .agent_models
                .iter()
                .map(|(agent, model)| (agent.clone(), model.clone()))
                .collect(),
            efforts: self
                .efforts
                .iter()
                .map(|(key, effort)| {
                    (
                        key.agent.clone(),
                        key.provider.clone(),
                        key.model.clone(),
                        effort.clone(),
                    )
                })
                .collect(),
            followed_agent: self.followed_agent.clone(),
            followed_model: self.followed_model.clone(),
            restored: self.restored.clone(),
        }
    }

    fn restore(state: SelectionSnapshot) -> Self {
        Self {
            inherited: state.inherited,
            effort: state.effort,
            manual: state.manual,
            agent_models: state.agent_models.into_iter().collect(),
            efforts: state
                .efforts
                .into_iter()
                .map(|(agent, provider, model, effort)| {
                    (
                        EffortKey {
                            agent,
                            provider,
                            model,
                        },
                        effort,
                    )
                })
                .collect(),
            followed_agent: state.followed_agent,
            followed_model: state.followed_model,
            restored: state.restored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{DraftId, RuntimeId, SessionId};
    use crate::send_config::CatalogModel;

    #[test]
    fn explicit_default_survives_protocol_default_echo_and_restart() {
        let mut fixture = Fixture::new();
        fixture.agents[0].model = Some(model("a", Some("high")));
        let owner = owner("r", "s", true);
        let mut store = ComposerSelections::default();
        store.open(owner.clone(), &fixture.context());
        let high = model("a", Some("high"));
        store.reconcile(
            &owner,
            SessionRecordChoice {
                model: Some(&high),
                agent: Some("build"),
            },
            None,
            &fixture.context(),
        );
        store.pick_effort(&owner, Effort::Default, &fixture.context());
        let omitted_send = snapshot(&store, &owner);
        assert_eq!(omitted_send.model.expect("model").variant, None);
        let canonical = model("a", Some("default"));
        store.reconcile(
            &owner,
            SessionRecordChoice {
                model: Some(&canonical),
                agent: Some("build"),
            },
            None,
            &fixture.context(),
        );
        let mut restored = ComposerSelections::restore(store.snapshot());
        restored.reconcile(
            &owner,
            SessionRecordChoice {
                model: Some(&canonical),
                agent: Some("build"),
            },
            None,
            &fixture.context(),
        );
        assert_eq!(
            restored.get(&owner).expect("owner").effort(),
            &Effort::Default
        );
        assert_eq!(
            snapshot(&restored, &owner).model.expect("model").variant,
            None
        );

        fixture.providers[0].models[0]
            .variants
            .push("default".into());
        restored.reconcile(
            &owner,
            SessionRecordChoice {
                model: Some(&high),
                agent: Some("build"),
            },
            None,
            &fixture.context(),
        );
        restored.reconcile(
            &owner,
            SessionRecordChoice {
                model: Some(&canonical),
                agent: Some("build"),
            },
            None,
            &fixture.context(),
        );
        assert_eq!(
            snapshot(&restored, &owner)
                .model
                .expect("model")
                .variant
                .as_deref(),
            Some("default"),
            "a real named variant remains a named variant"
        );
    }

    #[test]
    fn staged_creation_round_trip_preserves_capture_and_newer_draft_choice() {
        let fixture = Fixture::new();
        let source = owner("r", "draft", false);
        let target = owner("r", "session", true);
        let mut store = ComposerSelections::default();
        store.open(source.clone(), &fixture.context());
        store.pick_model(
            &source,
            model("b", None),
            Some(Effort::Default),
            &fixture.context(),
        );
        let captured = snapshot(&store, &source);
        assert!(store.stage_creation(&source, target.clone()));
        assert!(
            store.get(&target).is_none(),
            "staging is not a materialized session"
        );
        store.pick_model(
            &source,
            model("a", None),
            Some(Effort::Named("high".into())),
            &fixture.context(),
        );
        let mut store = ComposerSelections::restore(store.snapshot());
        store.commit_creation(
            target.clone(),
            Selection::default(),
            CreationSource::Continued,
        );
        assert_eq!(snapshot(&store, &target), captured);
        assert_eq!(
            snapshot(&store, &source).model,
            Some(model("a", Some("high")))
        );
        assert_eq!(
            store.get(&target).expect("session").effort(),
            &Effort::Default
        );
    }

    #[test]
    fn record_agent_change_replays_named_effort_with_target_memory_precedence() {
        let mut fixture = Fixture::new();
        fixture.agents[1].model = Some(model("a", Some("high")));
        let owner = owner("r", "s", true);
        let mut store = ComposerSelections::default();
        store.open(owner.clone(), &fixture.context());
        let recorded = model("a", Some("low"));
        let build = SessionRecordChoice {
            model: Some(&recorded),
            agent: Some("build"),
        };
        let plan = SessionRecordChoice {
            model: Some(&recorded),
            agent: Some("plan"),
        };
        store.reconcile(&owner, build, None, &fixture.context());
        store.reconcile(&owner, plan, None, &fixture.context());
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("a", Some("low"))),
            "record effort precedes the new agent's pin"
        );
        for effort in [Effort::Default, Effort::Named("high".to_owned())] {
            store.pick_effort(&owner, effort.clone(), &fixture.context());
            store.reconcile(&owner, build, None, &fixture.context());
            store.reconcile(&owner, plan, None, &fixture.context());
            assert_eq!(
                snapshot(&store, &owner).model,
                Some(model("a", effort.resolve(None).as_deref())),
                "target saved effort precedes unchanged record effort"
            );
        }
        store.pick_model(
            &owner,
            model("b", None),
            Some(Effort::Default),
            &fixture.context(),
        );
        store.reconcile(&owner, build, None, &fixture.context());
        store.reconcile(&owner, plan, None, &fixture.context());
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("b", None)),
            "agent-only change preserves the newer manual model"
        );
    }

    #[test]
    fn partial_catalog_preserves_inherited_effort_in_defaults_and_model_application() {
        let mut fixture = Fixture::new();
        let owner = owner("r", "d", false);
        let mut store = ComposerSelections::default();
        store.open(owner.clone(), &fixture.context());
        fixture.providers[0].models[0].variants.clear();
        let partial = SelectionContext {
            catalog_authoritative: false,
            ..fixture.context()
        };
        store.refresh_defaults(&owner, &partial);
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("a", Some("high")))
        );
        store.pick_model(&owner, model("a", None), None, &partial);
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("a", Some("high")))
        );
        store.refresh_defaults(&owner, &fixture.context());
        assert_eq!(snapshot(&store, &owner).model, Some(model("a", None)));
    }

    #[test]
    fn draft_agent_switch_replaces_inheritance_but_preserves_explicit_effort() {
        for effort in [
            Effort::Inherit,
            Effort::Default,
            Effort::Named("low".to_owned()),
        ] {
            let mut fixture = Fixture::new();
            fixture.defaults.settings_default_variant = Some("low".to_owned());
            fixture.agents[0].model = Some(model("a", Some("low")));
            fixture.agents[1].model = Some(model("a", Some("high")));
            let owner = owner("r", "d", false);
            let mut store = ComposerSelections::default();
            store.open(owner.clone(), &fixture.context());
            store.pick_agent(&owner, "build", &fixture.context());
            store.pick_effort(&owner, effort.clone(), &fixture.context());
            store.pick_agent(&owner, "plan", &fixture.context());
            assert_eq!(
                snapshot(&store, &owner).model,
                Some(model("a", effort.resolve(Some("high")).as_deref()))
            );
            store.pick_effort(&owner, Effort::Inherit, &fixture.context());
            assert_eq!(
                snapshot(&store, &owner).model,
                Some(model("a", Some("high")))
            );
        }
    }

    #[test]
    fn record_agent_change_refreshes_effort_without_restoring_agent_model() {
        let mut fixture = Fixture::new();
        fixture.agents[0].model = Some(model("a", Some("low")));
        fixture.agents[1].model = Some(model("a", Some("high")));
        let owner = owner("r", "s", true);
        let mut store = ComposerSelections::default();
        store.open(owner.clone(), &fixture.context());
        store.pick_agent(&owner, "plan", &fixture.context());
        store.pick_model(
            &owner,
            model("b", None),
            Some(Effort::Default),
            &fixture.context(),
        );
        let current = model("a", None);
        store.reconcile(
            &owner,
            SessionRecordChoice {
                model: Some(&current),
                agent: Some("build"),
            },
            None,
            &fixture.context(),
        );
        store.pick_model(
            &owner,
            current.clone(),
            Some(Effort::Inherit),
            &fixture.context(),
        );
        store.reconcile(
            &owner,
            SessionRecordChoice {
                model: Some(&current),
                agent: Some("plan"),
            },
            None,
            &fixture.context(),
        );
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("a", Some("high")))
        );
        assert_eq!(snapshot(&store, &owner).agent_name.as_deref(), Some("plan"));
        store.pick_effort(&owner, Effort::Default, &fixture.context());
        store.reconcile(
            &owner,
            SessionRecordChoice {
                model: Some(&current),
                agent: Some("build"),
            },
            None,
            &fixture.context(),
        );
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("a", Some("low")))
        );
        store.reconcile(
            &owner,
            SessionRecordChoice {
                model: Some(&current),
                agent: Some("plan"),
            },
            None,
            &fixture.context(),
        );
        assert_eq!(snapshot(&store, &owner).model, Some(model("a", None)));
        assert_eq!(
            store.get(&owner).expect("opened").effort(),
            &Effort::Default
        );
    }

    #[test]
    fn session_inheritance_refreshes_on_defaults_echo_and_explicit_inherit() {
        for path in ["defaults", "echo", "inherit"] {
            let mut fixture = Fixture::new();
            let owner = owner("r", "s", true);
            let mut store = ComposerSelections::default();
            store.open(owner.clone(), &fixture.context());
            let current = model("a", None);
            let record = SessionRecordChoice {
                model: Some(&current),
                agent: Some("build"),
            };
            store.reconcile(&owner, record, None, &fixture.context());
            if path == "inherit" {
                store.pick_effort(&owner, Effort::Default, &fixture.context());
            }
            fixture.defaults.settings_default_variant = Some("low".to_owned());
            match path {
                "defaults" => store.refresh_defaults(&owner, &fixture.context()),
                "echo" => {
                    store.reconcile(&owner, record, None, &fixture.context());
                }
                _ => store.pick_effort(&owner, Effort::Inherit, &fixture.context()),
            }
            assert_eq!(
                snapshot(&store, &owner).model,
                Some(model("a", Some("low"))),
                "{path}"
            );
            store.pick_effort(&owner, Effort::Default, &fixture.context());
            fixture.defaults.settings_default_variant = Some("high".to_owned());
            store.refresh_defaults(&owner, &fixture.context());
            assert_eq!(snapshot(&store, &owner).model, Some(model("a", None)));
            assert_eq!(
                snapshot(&store, &owner).agent_name.as_deref(),
                Some("build")
            );
        }
    }

    fn model(id: &str, variant: Option<&str>) -> ModelChoice {
        ModelChoice {
            provider_id: "p".to_owned(),
            model_id: id.to_owned(),
            variant: variant.map(str::to_owned),
        }
    }

    fn owner(runtime: &str, id: &str, session: bool) -> SelectionOwner {
        let target = if session {
            DraftTarget::Session(SessionId::from(id))
        } else {
            DraftTarget::NewSession(DraftId::from(id))
        };
        SelectionOwner {
            draft: DraftKey::new(RuntimeId::from(runtime), "/project", target)
                .expect("valid directory"),
            project: Some(ProjectId::from("project")),
        }
    }

    struct Fixture {
        agents: Vec<AgentEntry>,
        providers: Vec<CatalogProvider>,
        defaults: SelectionDefaults,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                agents: ["build", "plan", "review"]
                    .map(|name| AgentEntry {
                        name: name.to_owned(),
                        display_name: None,
                        mode: None,
                        hidden: false,
                        model: None,
                        color: None,
                    })
                    .to_vec(),
                providers: vec![CatalogProvider {
                    id: "p".to_owned(),
                    name: None,
                    models: ["a", "b", "c"]
                        .map(|id| CatalogModel {
                            id: id.to_owned(),
                            provider_model_id: None,
                            name: None,
                            variants: vec!["low".to_owned(), "high".to_owned()],
                            context_limit: None,
                        })
                        .to_vec(),
                }],
                defaults: SelectionDefaults {
                    settings_default_model: Some("p/a".to_owned()),
                    settings_default_variant: Some("high".to_owned()),
                    allow_fallback: true,
                    ..SelectionDefaults::default()
                },
            }
        }

        fn context(&self) -> SelectionContext<'_> {
            SelectionContext {
                agents: &self.agents,
                providers: &self.providers,
                defaults: &self.defaults,
                catalog_authoritative: true,
            }
        }
    }

    fn snapshot(store: &ComposerSelections, owner: &SelectionOwner) -> Selection {
        store.get(owner).expect("opened owner").selection()
    }

    #[test]
    fn empty_session_acquires_late_defaults_without_replacing_explicit_choices() {
        let fixture = Fixture::new();
        let owner = owner("r", "empty", true);
        let unloaded = SelectionDefaults::default();
        let gap = SelectionContext {
            agents: &[],
            providers: &[],
            defaults: &unloaded,
            catalog_authoritative: false,
        };
        let mut store = ComposerSelections::default();
        store.open(owner.clone(), &gap);
        assert!(snapshot(&store, &owner).model.is_none());
        assert_eq!(
            store.reconcile(
                &owner,
                SessionRecordChoice {
                    model: None,
                    agent: None
                },
                None,
                &gap
            ),
            RestoreResult::Pending
        );
        store.refresh_defaults(&owner, &fixture.context());
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("a", Some("high")))
        );
        assert!(snapshot(&store, &owner).agent_name.is_some());
        store.pick_model(
            &owner,
            model("b", None),
            Some(Effort::Default),
            &fixture.context(),
        );
        store.pick_agent(&owner, "plan", &fixture.context());
        let picked = snapshot(&store, &owner);
        store.refresh_defaults(&owner, &gap);
        store.refresh_defaults(&owner, &fixture.context());
        assert_eq!(snapshot(&store, &owner), picked);
    }

    #[test]
    fn effort_has_three_distinct_states() {
        assert_eq!(
            Effort::Inherit.resolve(Some("high")).as_deref(),
            Some("high")
        );
        assert_eq!(Effort::Default.resolve(Some("high")), None);
        assert_eq!(
            Effort::Named("low".to_owned())
                .resolve(Some("high"))
                .as_deref(),
            Some("low")
        );
    }

    #[test]
    fn confirmed_unavailable_auto_leaves_manual_drafts_but_unknown_keeps_them() {
        let mut fixture = Fixture::new();
        let owner = owner("r", "draft", false);
        let mut store = ComposerSelections::default();
        store.open(owner.clone(), &fixture.context());
        let auto = ModelChoice {
            provider_id: "openchamber".into(),
            model_id: "auto".into(),
            variant: None,
        };
        store.pick_model(&owner, auto.clone(), None, &fixture.context());
        store.refresh_defaults(&owner, &fixture.context());
        assert_eq!(snapshot(&store, &owner).model, Some(auto));
        fixture.defaults.auto_routing = AutoRouting::NotReady;
        store.refresh_defaults(&owner, &fixture.context());
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("a", Some("high")))
        );
        assert!(!store.get(&owner).expect("owner").manual().model);
    }

    #[test]
    fn explicit_default_survives_echo_and_captured_sends_stay_unchanged() {
        let fixture = Fixture::new();
        let context = fixture.context();
        let owner = owner("r", "s", true);
        let mut store = ComposerSelections::default();
        store.open(owner.clone(), &context);
        store.pick_effort(&owner, Effort::Default, &context);
        let captured = snapshot(&store, &owner);
        assert_eq!(
            store.reconcile(
                &owner,
                SessionRecordChoice {
                    model: Some(&model("a", None)),
                    agent: Some("build")
                },
                None,
                &context
            ),
            RestoreResult::Complete
        );
        assert_eq!(
            store.get(&owner).expect("opened").effort(),
            &Effort::Default
        );
        store.pick_model(
            &owner,
            model("b", None),
            Some(Effort::Named("low".to_owned())),
            &context,
        );
        assert_eq!(
            captured.model,
            Some(model("a", None)),
            "a captured send is immutable"
        );
    }

    #[test]
    fn external_model_requires_a_changed_followed_baseline() {
        let fixture = Fixture::new();
        let context = fixture.context();
        let owner = owner("r", "s", true);
        let mut store = ComposerSelections::default();
        store.open(owner.clone(), &context);
        store.pick_model(&owner, model("a", None), Some(Effort::Default), &context);
        let first = model("b", None);
        let record = SessionRecordChoice {
            model: Some(&first),
            agent: Some("build"),
        };
        store.reconcile(&owner, record, None, &context);
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("a", None)),
            "unseen model cannot prove a switch"
        );
        store.pick_model(
            &owner,
            model("a", None),
            Some(Effort::Named("low".to_owned())),
            &context,
        );
        store.reconcile(&owner, record, None, &context);
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("a", Some("low")))
        );
        store.reconcile(
            &owner,
            SessionRecordChoice {
                model: Some(&model("c", Some("high"))),
                agent: Some("build"),
            },
            None,
            &context,
        );
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("c", Some("high")))
        );
        assert!(!store.get(&owner).expect("opened").manual().model);
    }

    #[test]
    fn agent_follows_first_and_changed_record_without_restoring_its_old_model() {
        let fixture = Fixture::new();
        let context = fixture.context();
        let owner = owner("r", "s", true);
        let mut store = ComposerSelections::default();
        store.open(owner.clone(), &context);
        store.pick_agent(&owner, "plan", &context);
        store.pick_model(&owner, model("b", None), Some(Effort::Default), &context);
        store.pick_agent(&owner, "build", &context);
        store.pick_model(&owner, model("c", None), Some(Effort::Default), &context);
        let record = SessionRecordChoice {
            model: None,
            agent: Some("plan"),
        };
        store.reconcile(&owner, record, None, &context);
        assert_eq!(snapshot(&store, &owner).agent_name.as_deref(), Some("plan"));
        assert_eq!(snapshot(&store, &owner).model, Some(model("c", None)));
        store.pick_agent(&owner, "review", &context);
        store.open(owner.clone(), &context);
        store.reconcile(&owner, record, None, &context);
        assert_eq!(
            snapshot(&store, &owner).agent_name.as_deref(),
            Some("review"),
            "unchanged agent preserves newer pick on revisit"
        );
        store.reconcile(
            &owner,
            SessionRecordChoice {
                model: None,
                agent: Some("build"),
            },
            None,
            &context,
        );
        assert_eq!(
            snapshot(&store, &owner).agent_name.as_deref(),
            Some("build")
        );
    }

    #[test]
    fn record_precedes_history_and_catalog_gaps_do_not_finalize_restore() {
        let fixture = Fixture::new();
        let context = fixture.context();
        let gap = SelectionContext {
            providers: &[],
            catalog_authoritative: false,
            ..fixture.context()
        };
        let owner = owner("r", "s", true);
        let mut store = ComposerSelections::default();
        store.open(owner.clone(), &context);
        let record_model = model("b", Some("low"));
        let record = SessionRecordChoice {
            model: Some(&record_model),
            agent: Some("build"),
        };
        let reply = Selection {
            agent_name: Some("plan".to_owned()),
            model: Some(model("c", Some("high"))),
        };
        assert_eq!(
            store.reconcile(&owner, record, Some(&reply), &gap),
            RestoreResult::Pending
        );
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("a", Some("high")))
        );
        let partial = SelectionContext {
            catalog_authoritative: false,
            ..fixture.context()
        };
        assert_eq!(
            store.reconcile(&owner, record, Some(&reply), &partial),
            RestoreResult::Pending,
            "a partial catalog cannot finish restoration even when it lists the model"
        );
        assert_eq!(
            store.reconcile(&owner, record, Some(&reply), &context),
            RestoreResult::Complete
        );
        assert_eq!(snapshot(&store, &owner).model, Some(record_model));
        assert_eq!(
            snapshot(&store, &owner).agent_name.as_deref(),
            Some("build")
        );
    }

    #[test]
    fn assistant_without_effort_inherits_and_keeps_record_agent() {
        let fixture = Fixture::new();
        let context = fixture.context();
        let owner = owner("r", "s", true);
        let mut store = ComposerSelections::default();
        store.open(owner.clone(), &context);
        let reply = Selection {
            agent_name: Some("plan".to_owned()),
            model: Some(model("a", None)),
        };
        store.reconcile(
            &owner,
            SessionRecordChoice {
                model: None,
                agent: Some("review"),
            },
            Some(&reply),
            &context,
        );
        assert_eq!(
            snapshot(&store, &owner).agent_name.as_deref(),
            Some("review")
        );
        assert_eq!(
            store.get(&owner).expect("opened").effort(),
            &Effort::Inherit
        );
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("a", Some("high")))
        );
    }

    #[test]
    fn named_effort_is_invalidated_only_by_a_known_authoritative_model() {
        let mut fixture = Fixture::new();
        let owner = owner("r", "d", false);
        let mut store = ComposerSelections::default();
        store.open(owner.clone(), &fixture.context());
        store.pick_model(
            &owner,
            model("a", None),
            Some(Effort::Named("low".to_owned())),
            &fixture.context(),
        );
        fixture.providers[0].models[0].variants.clear();
        let partial = SelectionContext {
            catalog_authoritative: false,
            ..fixture.context()
        };
        store.refresh_defaults(&owner, &partial);
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("a", Some("low")))
        );
        let absent = SelectionContext {
            providers: &[],
            ..fixture.context()
        };
        store.refresh_defaults(&owner, &absent);
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("a", Some("low")))
        );
        store.refresh_defaults(&owner, &fixture.context());
        assert_eq!(
            store.get(&owner).expect("opened").effort(),
            &Effort::Inherit
        );
        assert_eq!(snapshot(&store, &owner).model, Some(model("a", None)));
    }

    #[test]
    fn owners_isolate_runtime_draft_project_and_explicit_materialization() {
        let fixture = Fixture::new();
        let context = fixture.context();
        let original = owner("r", "d", false);
        let mut project = original.clone();
        project.project = Some(ProjectId::from("other"));
        let others = [
            owner("other", "d", false),
            owner("r", "other", false),
            project,
        ];
        let mut store = ComposerSelections::default();
        store.open(original.clone(), &context);
        store.pick_model(&original, model("b", None), Some(Effort::Default), &context);
        for other in &others {
            store.open(other.clone(), &context);
            assert_eq!(
                snapshot(&store, other).model,
                Some(model("a", Some("high")))
            );
            assert_eq!(
                store.get(other).expect("opened").manual(),
                ManualSelection::default()
            );
        }
        let session = owner("r", "s", true);
        assert!(!store.materialize(&original, owner("other", "s", true)));
        assert!(store.materialize(&original, session.clone()));
        assert!(store.get(&original).is_none());
        assert_eq!(snapshot(&store, &session).model, Some(model("b", None)));
        assert!(
            !store.materialize(&others[1], session.clone()),
            "destination cannot be overwritten"
        );
        assert!(store.get(&others[1]).is_some());
    }

    #[test]
    fn remembered_agent_models_and_efforts_are_independent() {
        let mut fixture = Fixture::new();
        fixture.agents[1].model = Some(model("c", Some("high")));
        let context = fixture.context();
        let owner = owner("r", "s", true);
        let mut store = ComposerSelections::default();
        store.open(owner.clone(), &context);
        store.pick_model(&owner, model("b", None), Some(Effort::Default), &context);
        store.pick_agent(&owner, "plan", &context);
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("c", Some("high")))
        );
        store.pick_model(
            &owner,
            model("a", None),
            Some(Effort::Named("low".to_owned())),
            &context,
        );
        store.pick_agent(&owner, "build", &context);
        assert_eq!(snapshot(&store, &owner).model, Some(model("b", None)));
        assert_eq!(
            store.get(&owner).expect("opened").effort(),
            &Effort::Default
        );
        store.pick_agent(&owner, "plan", &context);
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("a", Some("low"))),
            "session memory wins over pin"
        );
    }

    #[test]
    fn draft_default_refresh_obeys_each_manual_flag() {
        let mut fixture = Fixture::new();
        fixture.agents[1].model = Some(model("b", Some("low")));
        let owner = owner("r", "d", false);
        let mut store = ComposerSelections::default();
        store.open(owner.clone(), &fixture.context());
        store.pick_agent(&owner, "plan", &fixture.context());
        assert_eq!(
            store.get(&owner).expect("opened").manual(),
            ManualSelection {
                agent: true,
                model: false
            }
        );
        store.refresh_defaults(&owner, &fixture.context());
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("b", Some("low")))
        );
        assert_eq!(snapshot(&store, &owner).agent_name.as_deref(), Some("plan"));
        store.forget(&owner);
        store.open(owner.clone(), &fixture.context());
        store.pick_effort(&owner, Effort::Default, &fixture.context());
        fixture.defaults.project_default_model = Some("p/a".to_owned());
        fixture.defaults.project_default_variant = Some("low".to_owned());
        store.refresh_defaults(&owner, &fixture.context());
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("a", None)),
            "effort-only pick survives same-model defaults"
        );
        assert_eq!(
            store.get(&owner).expect("opened").manual(),
            ManualSelection::default()
        );
    }

    #[test]
    fn agent_resolution_uses_compatible_manual_then_available_default_then_current() {
        let mut fixture = Fixture::new();
        let owner = owner("r", "d", false);
        let mut store = ComposerSelections::default();
        store.open(owner.clone(), &fixture.context());
        store.pick_model(
            &owner,
            model("b", None),
            Some(Effort::Default),
            &fixture.context(),
        );
        store.pick_agent(&owner, "plan", &fixture.context());
        assert_eq!(snapshot(&store, &owner).model, Some(model("b", None)));
        fixture.agents[2].model = Some(model("missing", None));
        store.pick_agent(&owner, "review", &fixture.context());
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("a", Some("high"))),
            "unavailable pin still prevents compatible-manual branch"
        );
        fixture.defaults.settings_default_model = Some("p/missing".to_owned());
        store.pick_agent(&owner, "review", &fixture.context());
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("a", Some("high"))),
            "unavailable configured model retains current"
        );
    }

    #[test]
    fn saved_auto_waits_for_readiness_and_ignores_router_model_switches() {
        let mut fixture = Fixture::new();
        let owner = owner("r", "s", true);
        let mut store = ComposerSelections::default();
        store.open(owner.clone(), &fixture.context());
        let auto = ModelChoice {
            provider_id: "openchamber".to_owned(),
            model_id: "auto".to_owned(),
            variant: None,
        };
        store.pick_model(
            &owner,
            auto.clone(),
            Some(Effort::Default),
            &fixture.context(),
        );
        let concrete = model("b", None);
        let record = SessionRecordChoice {
            model: Some(&concrete),
            agent: Some("plan"),
        };
        assert_eq!(
            store.reconcile(&owner, record, None, &fixture.context()),
            RestoreResult::Pending
        );
        fixture.defaults.auto_routing = AutoRouting::Ready;
        assert_eq!(
            store.reconcile(&owner, record, None, &fixture.context()),
            RestoreResult::Complete
        );
        store.reconcile(
            &owner,
            SessionRecordChoice {
                model: Some(&model("c", None)),
                agent: Some("review"),
            },
            None,
            &fixture.context(),
        );
        assert_eq!(snapshot(&store, &owner).model, Some(auto));
        assert_eq!(
            snapshot(&store, &owner).agent_name.as_deref(),
            Some("review")
        );
    }

    #[test]
    fn model_picker_restores_effort_memory_and_explicit_inherit_forgets_it() {
        let fixture = Fixture::new();
        let context = fixture.context();
        let owner = owner("r", "s", true);
        let mut store = ComposerSelections::default();
        store.open(owner.clone(), &context);
        store.pick_model(&owner, model("a", None), Some(Effort::Default), &context);
        store.pick_model(&owner, model("b", None), None, &context);
        store.pick_effort(&owner, Effort::Named("low".to_owned()), &context);
        store.pick_model(&owner, model("a", None), None, &context);
        assert_eq!(snapshot(&store, &owner).model, Some(model("a", None)));
        store.pick_model(&owner, model("a", None), Some(Effort::Inherit), &context);
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("a", Some("high")))
        );
        store.pick_model(&owner, model("b", None), None, &context);
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("b", Some("low")))
        );
    }

    #[test]
    fn changed_record_effort_beats_saved_default_but_missing_effort_preserves_it() {
        let fixture = Fixture::new();
        let context = fixture.context();
        let owner = owner("r", "s", true);
        let mut store = ComposerSelections::default();
        store.open(owner.clone(), &context);
        store.pick_effort(&owner, Effort::Default, &context);
        let first = model("a", Some("low"));
        store.reconcile(
            &owner,
            SessionRecordChoice {
                model: Some(&first),
                agent: Some("build"),
            },
            None,
            &context,
        );
        assert_eq!(snapshot(&store, &owner).model, Some(model("a", None)));
        store.reconcile(
            &owner,
            SessionRecordChoice {
                model: Some(&model("a", Some("high"))),
                agent: Some("build"),
            },
            None,
            &context,
        );
        assert_eq!(
            snapshot(&store, &owner).model,
            Some(model("a", Some("high")))
        );
        store.pick_effort(&owner, Effort::Default, &context);
        store.reconcile(
            &owner,
            SessionRecordChoice {
                model: Some(&model("a", None)),
                agent: Some("build"),
            },
            None,
            &context,
        );
        assert_eq!(
            store.get(&owner).expect("opened").effort(),
            &Effort::Default
        );
    }

    #[test]
    fn remembered_model_survives_missing_catalog_and_other_runtime_memory() {
        let mut fixture = Fixture::new();
        let first = owner("r", "s", true);
        let other = owner("other", "s", true);
        let mut store = ComposerSelections::default();
        store.open(first.clone(), &fixture.context());
        store.pick_model(
            &first,
            model("b", None),
            Some(Effort::Named("low".to_owned())),
            &fixture.context(),
        );
        store.pick_agent(&first, "plan", &fixture.context());
        store.open(other.clone(), &fixture.context());
        store.pick_agent(&other, "plan", &fixture.context());
        store.pick_model(
            &other,
            model("c", None),
            Some(Effort::Default),
            &fixture.context(),
        );
        fixture.providers.clear();
        store.pick_agent(&first, "build", &fixture.context());
        assert_eq!(
            snapshot(&store, &first).model,
            Some(model("b", Some("low")))
        );
        assert_eq!(snapshot(&store, &other).model, Some(model("c", None)));
    }
}
