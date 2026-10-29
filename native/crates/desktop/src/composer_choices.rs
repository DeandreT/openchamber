//! Binds the composer's captured owner to core selection rules.

use gpui_kit::Context;
use openchamber_native_core::composer_selection::{
    ComposerSelections, Effort, SelectionContext, SelectionOwner,
};
use openchamber_native_core::send_config::{
    AgentEntry, CatalogProvider, ModelChoice, SelectionDefaults, SessionRecordChoice,
};
use openchamber_native_transport::catalog::OpenCodeDefaults;

use crate::workspace::Workspace;

/// Catalogs remain borrowed while the owning selection store mutates.
pub(crate) struct ChoiceInputs<'a> {
    pub agents: &'a [AgentEntry],
    pub providers: &'a [CatalogProvider],
    pub defaults: SelectionDefaults,
    authoritative: bool,
}

impl ChoiceInputs<'_> {
    pub(crate) fn context(&self) -> SelectionContext<'_> {
        SelectionContext {
            agents: self.agents,
            providers: self.providers,
            defaults: &self.defaults,
            catalog_authoritative: self.authoritative,
        }
    }
}

impl Workspace {
    pub(crate) fn selection_owner(&self) -> Option<SelectionOwner> {
        let draft = self.composer_key.clone()?;
        let project = self
            .owning_project(draft.directory())
            .and_then(|path| {
                self.topology
                    .projects
                    .iter()
                    .find(|project| project.path == path)
            })
            .map(|project| project.id.clone());
        Some(SelectionOwner { draft, project })
    }

    pub(crate) fn choice_inputs(&self) -> Option<ChoiceInputs<'_>> {
        let directory = self.config_directory()?;
        let catalog = self.send_catalog.directory(&directory)?;
        let fallback = OpenCodeDefaults::default();
        Some(ChoiceInputs {
            agents: catalog
                .agents
                .ready()
                .map(Vec::as_slice)
                .unwrap_or_default(),
            providers: catalog
                .providers
                .ready()
                .map(Vec::as_slice)
                .unwrap_or_default(),
            defaults: self
                .selection_defaults(&directory, catalog.opencode.ready().unwrap_or(&fallback)),
            authoritative: catalog.providers.ready().is_some(),
        })
    }

    fn edit_choices<R>(
        &mut self,
        edit: impl FnOnce(&Self, &mut ComposerSelections, &ChoiceInputs<'_>) -> R,
    ) -> Option<R> {
        let mut choices = std::mem::take(&mut self.composer_selections);
        let result = self
            .choice_inputs()
            .map(|inputs| edit(self, &mut choices, &inputs));
        self.composer_selections = choices;
        result
    }

    pub(crate) fn refresh_composer_choices(&mut self, cx: &mut Context<Self>) {
        // Hydration owns the initial state. Rendering while disk loads cannot
        // create a default snapshot that later replaces the saved choice.
        if !self.is_synthetic() && !self.composer_editable() {
            return;
        }
        let Some(owner) = self.selection_owner() else {
            return;
        };
        let changed = self.edit_choices(|this, choices, inputs| {
            let previous = choices.get(&owner).cloned();
            choices.open(owner.clone(), &inputs.context());
            choices.refresh_defaults(&owner, &inputs.context());
            if let Some(session) = this.selected_session() {
                let model = session.model.clone();
                let agent = session.agent.clone();
                let latest = this
                    .open_sessions
                    .get(&session.id)
                    .filter(|open| matches!(open.history, crate::workspace::History::Complete))
                    .and_then(|open| {
                        open.transcript
                            .messages()
                            .iter()
                            .rev()
                            .find_map(|message| message.info.assistant.as_ref())
                    })
                    .map(|facts| openchamber_native_core::send_config::Selection {
                        agent_name: facts.agent.clone(),
                        model: facts.model.clone(),
                    });
                choices.reconcile(
                    &owner,
                    SessionRecordChoice {
                        model: model.as_ref(),
                        agent: agent.as_deref(),
                    },
                    latest.as_ref(),
                    &inputs.context(),
                );
            }
            previous.as_ref() != choices.get(&owner)
        });
        if changed == Some(true) {
            self.queue_persistence(cx);
        }
    }

    pub(crate) fn choose_composer_model(
        &mut self,
        owner: &SelectionOwner,
        model: ModelChoice,
        cx: &mut Context<Self>,
    ) {
        if !self.composer_editable() || self.selection_owner().as_ref() != Some(owner) {
            return;
        }
        let changed = self.edit_choices(|_, choices, inputs| {
            let auto = model.provider_id == "openchamber" && model.model_id == "auto";
            if (auto
                && inputs.defaults.auto_routing
                    != openchamber_native_core::send_config::AutoRouting::Ready)
                || (!auto
                    && openchamber_native_core::send_config::find_model(inputs.providers, &model)
                        .is_none())
                || inputs
                    .defaults
                    .hidden_models
                    .contains(&(model.provider_id.clone(), model.model_id.clone()))
            {
                return false;
            }
            choices.pick_model(owner, model, None, &inputs.context());
            true
        });
        if changed != Some(true) {
            return;
        }
        self.queue_persistence(cx);
        cx.notify();
    }

    pub(crate) fn choose_composer_agent(
        &mut self,
        owner: &SelectionOwner,
        agent: &str,
        cx: &mut Context<Self>,
    ) {
        if !self.composer_editable() || self.selection_owner().as_ref() != Some(owner) {
            return;
        }
        let changed = self.edit_choices(|_, choices, inputs| {
            if !inputs.agents.iter().any(|entry| {
                entry.name == agent
                    && !entry.hidden
                    && entry.mode != Some(openchamber_native_core::send_config::AgentMode::Subagent)
            }) {
                return false;
            }
            if choices
                .get(owner)
                .and_then(|state| state.selection().model)
                .is_some_and(|model| model.provider_id == "openchamber" && model.model_id == "auto")
            {
                return false;
            }
            choices.pick_agent(owner, agent, &inputs.context());
            true
        });
        if changed != Some(true) {
            return;
        }
        self.queue_persistence(cx);
        cx.notify();
    }

    pub(crate) fn choose_composer_effort(
        &mut self,
        owner: &SelectionOwner,
        effort: Effort,
        cx: &mut Context<Self>,
    ) {
        if !self.composer_editable() || self.selection_owner().as_ref() != Some(owner) {
            return;
        }
        let changed = self.edit_choices(|_, choices, inputs| {
            if let Effort::Named(name) = &effort {
                let valid = choices
                    .get(owner)
                    .and_then(|state| state.selection().model)
                    .and_then(|model| {
                        openchamber_native_core::send_config::find_model(inputs.providers, &model)
                    })
                    .is_some_and(|model| model.variants.contains(name));
                if !valid {
                    return false;
                }
            }
            choices.pick_effort(owner, effort, &inputs.context());
            true
        });
        if changed != Some(true) {
            return;
        }
        self.queue_persistence(cx);
        cx.notify();
    }
}
