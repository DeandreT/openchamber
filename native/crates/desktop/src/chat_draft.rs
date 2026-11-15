//! Chat target ownership, shared directory preparation and draft retargeting.

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};

use chrono::Datelike;
use gpui_kit::{Context, Task, Window};
use openchamber_native_core::chat_draft::{
    self, CHAT_DRAFT_ID, DraftDestination, OpenRequest, Preparation, RootsStatus,
};
use openchamber_native_core::chat_roots::ChatRoots;
use openchamber_native_core::draft::{DraftKey, DraftTarget, restore_failed};
use openchamber_native_core::ids::RuntimeId;

use crate::connection::ConnectionState;
use crate::ui_memory::{self, UiMemory};
use crate::workspace::Workspace;

const FAILED: &str = "chat.nativeRecovery.sendFailed";
const HOLDING_DIRECTORY: &str = "/__openchamber_chat_draft__";

#[derive(Clone, Copy)]
pub(crate) enum NewSession {
    Plain,
    FromSelected,
    Chat,
}

#[cfg(test)]
#[path = "chat_draft_tests.rs"]
mod tests;

#[derive(Clone)]
struct Slot {
    active: bool,
    folder: Option<String>,
    roots: Option<ChatRoots>,
    preparation: Preparation,
    generation: u64,
}

#[derive(Clone)]
struct DirectoryCustody {
    roots: ChatRoots,
    client: openchamber_native_transport::client::ServerClient,
    executor: tokio::runtime::Handle,
}

#[derive(Default)]
pub(crate) struct ChatDrafts {
    slots: HashMap<RuntimeId, Slot>,
    defaulted: HashSet<RuntimeId>,
    preparations: HashMap<DraftKey, Task<()>>,
    cleanups: HashMap<DraftKey, Task<()>>,
    directories: HashMap<DraftKey, DirectoryCustody>,
    handed_off: HashSet<DraftKey>,
    cleanup_failed: HashSet<DraftKey>,
    cleanup_requested: HashSet<DraftKey>,
    pending_moves: HashMap<RuntimeId, (DraftKey, DraftDestination)>,
    pending_renewals: HashSet<DraftKey>,
    roots_failed: bool,
    roots_read: Option<Task<()>>,
    sequence: u64,
}

impl ChatDrafts {
    pub(crate) fn close(&mut self, runtime: &RuntimeId) {
        if let Some(slot) = self.slots.get_mut(runtime) {
            slot.active = false;
        }
    }

    pub(crate) fn switch_runtime(&mut self) {
        self.roots_failed = false;
        self.roots_read = None;
    }

    pub(crate) fn holds_runtime(&self, runtime: &RuntimeId) -> bool {
        self.preparations
            .keys()
            .chain(self.cleanups.keys())
            .any(|key| key.runtime() == runtime)
    }
}

impl Workspace {
    pub(crate) fn chat_draft_open(&self) -> bool {
        !self.draft_projects.contains_key(&self.runtime)
            && self
                .chat_drafts
                .slots
                .get(&self.runtime)
                .is_some_and(|slot| slot.active)
    }

    pub(crate) fn chat_draft_key(&self) -> Option<DraftKey> {
        if !self.chat_draft_open() {
            return None;
        }
        let slot = self.chat_drafts.slots.get(&self.runtime)?;
        DraftKey::new(
            self.runtime.clone(),
            slot.folder.as_deref().unwrap_or(HOLDING_DIRECTORY),
            DraftTarget::NewSession(CHAT_DRAFT_ID.into()),
        )
    }

    pub(crate) fn chat_config_directory(&self) -> Option<String> {
        self.topology
            .chat_roots
            .as_ref()
            .map(|roots| roots.configured().to_owned())
    }

    /// Panels may show the known root before preparation. Mutation consumers
    /// use chat_prepared_directory instead.
    pub(crate) fn chat_execution_directory(&self) -> Option<String> {
        self.chat_prepared_directory()
            .or_else(|| self.chat_config_directory())
    }

    pub(crate) fn chat_prepared_directory(&self) -> Option<String> {
        if !self.chat_draft_open() {
            return None;
        }
        let slot = self.chat_drafts.slots.get(&self.runtime)?;
        let folder = slot.folder.as_deref()?;
        if self
            .topology
            .chat_roots
            .as_ref()?
            .canonical_session_directory(folder)
            .as_deref()
            != Some(folder)
        {
            return None;
        }
        (slot.preparation == Preparation::Ready)
            .then(|| slot.folder.clone())
            .flatten()
    }

    /// Render-time activation starts a named directory once. An explicit
    /// interaction may retry Failed through prepare_chat_draft.
    pub(crate) fn warm_chat_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Failed roots would make prepare_chat_draft re-read them on every render.
        if self.chat_roots_status() != RootsStatus::Failed
            && self
                .chat_drafts
                .slots
                .get(&self.runtime)
                .is_some_and(|slot| slot.preparation == Preparation::Named)
        {
            self.prepare_chat_draft(window, cx);
        }
    }

    pub(crate) fn chat_roots_status(&self) -> RootsStatus {
        RootsStatus::of(
            self.topology.chat_roots.as_ref(),
            self.session_admission.roots_ready,
            self.chat_drafts.roots_failed,
        )
    }

    pub(crate) fn is_chat_creation(&self, key: &DraftKey) -> bool {
        if !matches!(key.target(), DraftTarget::NewSession(id) if id.as_str() == CHAT_DRAFT_ID) {
            return false;
        }
        let roots = if let Some(custody) = self.chat_drafts.directories.get(key) {
            Some(&custody.roots)
        } else if key.runtime() == &self.runtime {
            self.topology.chat_roots.as_ref()
        } else {
            self.chat_drafts
                .slots
                .get(key.runtime())
                .and_then(|slot| slot.roots.as_ref())
        };
        roots.is_some_and(|roots| roots.session_suffix(key.directory()).is_some())
    }

    pub(crate) fn chat_creation_ready(&self, key: &DraftKey) -> bool {
        if key.runtime() == &self.runtime
            && !self.chat_drafts.handed_off.contains(key)
            && self
                .topology
                .chat_roots
                .as_ref()
                .and_then(|roots| roots.canonical_session_directory(key.directory()))
                .as_deref()
                != Some(key.directory())
        {
            return false;
        }
        self.is_chat_creation(key)
            && self
                .chat_drafts
                .slots
                .get(key.runtime())
                .is_some_and(|slot| {
                    slot.folder.as_deref() == Some(key.directory())
                        && matches!(
                            slot.preparation,
                            Preparation::Ready | Preparation::HandedOff
                        )
                })
    }

    pub(crate) fn handoff_chat_directory(&mut self, key: &DraftKey) {
        if self.is_chat_creation(key) {
            self.chat_drafts.handed_off.insert(key.clone());
        }
        if let Some(slot) = self.chat_drafts.slots.get_mut(key.runtime())
            && slot.folder.as_deref() == Some(key.directory())
        {
            slot.preparation = Preparation::HandedOff;
        }
    }

    pub(crate) fn open_new_session(
        &mut self,
        origin: NewSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let directory = match origin {
            NewSession::FromSelected => self
                .selected_session()
                .and_then(|session| session.directory.clone()),
            _ => None,
        };
        let last = cx
            .try_global::<UiMemory>()
            .and_then(|memory| memory.draft_destination(self.runtime.as_str()));
        let destination = chat_draft::resolve_destination(
            OpenRequest {
                chat: matches!(origin, NewSession::Chat),
                directory: directory.as_deref(),
                last: last.as_ref(),
            },
            self.topology.chat_roots.as_ref(),
            |directory| self.draft_destination_project(directory).is_some(),
        );
        self.show_new_session(window, cx);
        self.set_draft_destination(&destination, false, window, cx);
        self.focus_composer(window, cx);
        self.retry_failed_chat_roots(window, cx);
    }

    pub(crate) fn move_draft_to(
        &mut self,
        choice: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected.is_some() || self.close_in_progress() {
            return;
        }
        let destination = if choice == chat_draft::CHAT_PROJECT_ID {
            DraftDestination::Chat
        } else if self.draft_destination_project(choice).is_some() {
            DraftDestination::Project(choice.to_owned())
        } else {
            return;
        };
        self.set_draft_destination(&destination, true, window, cx);
        self.focus_composer(window, cx);
    }

    pub(crate) fn open_draft_target(&mut self, key: &DraftKey, cx: &mut Context<Self>) {
        let destination = if matches!(key.target(), DraftTarget::NewSession(id) if id.as_str() == CHAT_DRAFT_ID)
        {
            self.draft_projects.remove(&self.runtime);
            let roots = self.topology.chat_roots.clone();
            let slot = self
                .chat_drafts
                .slots
                .entry(self.runtime.clone())
                .or_insert(Slot {
                    active: true,
                    folder: Some(key.directory().to_owned()),
                    roots: roots.clone(),
                    preparation: Preparation::Named,
                    generation: 0,
                });
            if slot.folder.as_deref() != Some(key.directory()) {
                slot.folder = Some(key.directory().to_owned());
                slot.roots = roots;
                slot.preparation = Preparation::Named;
                slot.generation = slot.generation.wrapping_add(1);
            }
            slot.active = true;
            DraftDestination::Chat
        } else {
            self.chat_drafts.close(&self.runtime);
            self.draft_projects
                .insert(self.runtime.clone(), key.directory().to_owned());
            DraftDestination::Project(key.directory().to_owned())
        };
        self.remember_draft_destination(&destination, cx);
    }

    fn set_draft_destination(
        &mut self,
        destination: &DraftDestination,
        carry: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_composer_draft(cx);
        let before = self.new_session_draft_key();
        if carry
            && let Some(before) = &before
            && (self.chat_retarget_busy(before) || self.composer.read(cx).is_composing())
        {
            self.chat_drafts
                .pending_moves
                .insert(self.runtime.clone(), (before.clone(), destination.clone()));
            return;
        }
        let previous_project = self.draft_projects.get(&self.runtime).cloned();
        let previous_active = self
            .chat_drafts
            .slots
            .get(&self.runtime)
            .is_some_and(|slot| slot.active);
        match destination {
            DraftDestination::Chat => {
                self.draft_projects.remove(&self.runtime);
                self.chat_drafts
                    .slots
                    .entry(self.runtime.clone())
                    .or_insert(Slot {
                        active: true,
                        folder: None,
                        roots: None,
                        preparation: Preparation::Named,
                        generation: 0,
                    })
                    .active = true;
                self.name_chat_folder();
            }
            DraftDestination::Project(directory) => {
                self.chat_drafts.close(&self.runtime);
                self.draft_projects
                    .insert(self.runtime.clone(), directory.clone());
            }
        }
        let after = self.new_session_draft_key();
        if carry
            && let (Some(before), Some(after)) = (&before, &after)
            && !self.transfer_complete_draft(before, after, cx)
        {
            match previous_project {
                Some(project) => {
                    self.draft_projects.insert(self.runtime.clone(), project);
                }
                None => {
                    self.draft_projects.remove(&self.runtime);
                }
            }
            if let Some(slot) = self.chat_drafts.slots.get_mut(&self.runtime) {
                slot.active = previous_active;
            }
            self.notify_mutation_failure(FAILED, window, cx);
            return;
        }
        self.remember_draft_destination(destination, cx);
        self.navigation_revision = self.navigation_revision.wrapping_add(1);
        self.composer_key = after;
        self.restore_composer_draft(window, cx);
        self.ensure_send_catalog(cx);
        if let Some(owner) = self.composer_key.clone() {
            self.resume_chat_attachments(&owner, window, cx);
        }
        if self.chat_draft_open() && self.composer_has_sendable_content(cx) {
            self.prepare_chat_draft(window, cx);
        }
        if carry && let Some(before) = before {
            self.cleanup_unused_chat_directory(&before, window, cx);
        }
        self.queue_persistence(cx);
        self.rebuild_sidebar();
        cx.notify();
    }

    fn chat_retarget_busy(&self, owner: &DraftKey) -> bool {
        self.attachment_storage.has_pending(owner) || self.attachment_staging.active_for(owner)
    }

    pub(crate) fn resume_chat_retarget(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.composer.read(cx).is_composing() {
            return;
        }
        let renewals: Vec<_> = self
            .chat_drafts
            .pending_renewals
            .iter()
            .filter(|owner| owner.runtime() == &self.runtime && !self.chat_retarget_busy(owner))
            .cloned()
            .collect();
        for owner in renewals {
            self.chat_drafts.pending_renewals.remove(&owner);
            self.renew_chat_folder(&owner.runtime().clone(), &owner, window, cx);
        }
        if self.chat_draft_open()
            && self
                .composer_key
                .as_ref()
                .is_some_and(|owner| !self.chat_retarget_busy(owner))
        {
            self.settle_chat_folder(window, cx);
        }
        let Some((owner, destination)) = self.chat_drafts.pending_moves.get(&self.runtime).cloned()
        else {
            return;
        };
        if self.composer_key.as_ref() != Some(&owner)
            || self.selected.is_some()
            || self.chat_retarget_busy(&owner)
        {
            return;
        }
        self.chat_drafts.pending_moves.remove(&self.runtime);
        self.set_draft_destination(&destination, true, window, cx);
    }

    /// Merge before removing either owner. Conflicts preserve both complete drafts.
    fn transfer_complete_draft(
        &mut self,
        from: &DraftKey,
        to: &DraftKey,
        cx: &mut Context<Self>,
    ) -> bool {
        if from == to {
            return true;
        }
        if from.runtime() != to.runtime()
            || self.has_composer_document(from)
            || self.has_composer_document(to)
        {
            return false;
        }
        let Some(source) = self.drafts.get(from) else {
            return true;
        };
        let destination = self.drafts.get(to);
        let prefix = destination
            .filter(|draft| {
                !draft.text.is_empty() && !source.text.is_empty() && draft.text != source.text
            })
            .map_or(0, |draft| draft.text.len() + 2);
        let Ok(mut merged) = restore_failed(destination, source) else {
            return false;
        };
        merged.selection = source.selection.start + prefix..source.selection.end + prefix;
        if !self.retarget_draft_attachments(from, to, &merged) {
            return false;
        }
        self.retarget_attachment_storage(from, to);
        self.attachment_input.retarget(from, to, prefix);
        self.record_native_composer_change(from);
        self.record_native_composer_change(to);
        self.drafts.take(from);
        self.save_draft_preview(from, &Default::default(), cx);
        self.save_draft_preview(to, &merged, cx);
        self.drafts.save(to.clone(), merged);
        self.queue_persistence(cx);
        true
    }

    pub(crate) fn remember_draft_destination(
        &self,
        destination: &DraftDestination,
        cx: &mut Context<Self>,
    ) {
        if !self.is_synthetic() {
            ui_memory::remember_draft_destination(self.runtime.as_str(), destination, cx);
        }
    }

    pub(crate) fn open_default_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_synthetic()
            || !self.persistence_loaded()
            || self.chat_drafts.defaulted.contains(&self.runtime)
        {
            return;
        }
        self.chat_drafts.defaulted.insert(self.runtime.clone());
        if self.draft_projects.contains_key(&self.runtime) || self.chat_draft_open() {
            return;
        }
        let last = cx
            .try_global::<UiMemory>()
            .and_then(|memory| memory.draft_destination(self.runtime.as_str()));
        let destination = chat_draft::resolve_destination(
            OpenRequest {
                last: last.as_ref(),
                ..Default::default()
            },
            self.topology.chat_roots.as_ref(),
            |directory| self.draft_destination_project(directory).is_some(),
        );
        if self.selected.is_none() {
            self.set_draft_destination(&destination, true, window, cx);
        }
    }

    fn name_chat_folder(&mut self) {
        let Some(roots) = self.topology.chat_roots.clone() else {
            return;
        };
        let runtime = self.runtime.clone();
        let Some(slot) = self.chat_drafts.slots.get(&runtime) else {
            return;
        };
        if slot.folder.as_deref().is_some_and(|folder| {
            roots.canonical_session_directory(folder).as_deref() == Some(folder)
        }) {
            return;
        }
        let restored = slot
            .folder
            .as_deref()
            .and_then(|folder| roots.canonical_session_directory(folder))
            .or_else(|| {
                self.drafts
                    .iter()
                    .map(|(key, _)| key)
                    .filter(|key| {
                        key.runtime() == &runtime
                            && matches!(key.target(), DraftTarget::NewSession(id) if id.as_str() == CHAT_DRAFT_ID)
                            && !self.chat_drafts.handed_off.contains(*key)
                            && roots.session_suffix(key.directory()).is_some()
                    })
                    .filter_map(|key| roots.canonical_session_directory(key.directory()))
                    .min()
            });
        self.chat_drafts.sequence = self.chat_drafts.sequence.wrapping_add(1);
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(self.chat_drafts.sequence);
        let today = chrono::Local::now().date_naive();
        let folder = restored.or_else(|| {
            chat_draft::session_directory(
                &roots,
                chat_draft::ChatDate {
                    year: today.year(),
                    month: today.month(),
                    day: today.day(),
                },
                &format!("{:016x}", hasher.finish()),
            )
        });
        let slot = self
            .chat_drafts
            .slots
            .get_mut(&runtime)
            .expect("the Chat draft exists");
        slot.folder = folder;
        slot.roots = Some(roots);
        slot.generation = slot.generation.wrapping_add(1);
        slot.preparation = Preparation::Named;
    }

    pub(crate) fn chat_roots_read(
        &mut self,
        succeeded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.chat_drafts.roots_failed = !succeeded;
        if succeeded {
            self.settle_chat_folder(window, cx);
            self.resume_chat_retarget(window, cx);
            let failed: Vec<_> = self
                .chat_drafts
                .cleanup_failed
                .iter()
                .filter(|key| key.runtime() == &self.runtime)
                .cloned()
                .collect();
            for key in failed {
                self.cleanup_unused_chat_directory(&key, window, cx);
            }
        }
        cx.notify();
    }

    pub(crate) fn settle_chat_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.chat_draft_open() || self.composer.read(cx).is_composing() {
            return;
        }
        let before = self.chat_draft_key();
        if let Some(before) = &before
            && self.chat_drafts.handed_off.contains(before)
            && !self
                .creations
                .iter()
                .any(|(_, attempt)| &attempt.source == before)
        {
            self.renew_chat_folder(&before.runtime().clone(), before, window, cx);
            return;
        }
        if before
            .as_ref()
            .is_some_and(|owner| self.chat_retarget_busy(owner))
        {
            return;
        }
        let previous_slot = self.chat_drafts.slots.get(&self.runtime).cloned();
        self.name_chat_folder();
        let after = self.chat_draft_key();
        if before != after
            && let (Some(before), Some(after)) = (&before, &after)
        {
            if self.composer_key.as_ref() == Some(before) {
                self.save_composer_draft(cx);
            }
            if !self.transfer_complete_draft(before, after, cx) {
                if let Some(slot) = previous_slot {
                    self.chat_drafts.slots.insert(self.runtime.clone(), slot);
                }
                return;
            }
            if self.selected.is_none() && self.composer_key.as_ref() == Some(before) {
                self.composer_key = Some(after.clone());
                self.restore_composer_draft(window, cx);
            }
        }
        if let Some(after) = &after {
            let aliases: Vec<_> = self
                .drafts
                .iter()
                .map(|(key, _)| key)
                .filter(|key| {
                    *key != after
                        && key.runtime() == after.runtime()
                        && key.target() == after.target()
                        && self.topology.chat_roots.as_ref().is_some_and(|roots| {
                            roots.same_session_directory(key.directory(), after.directory())
                        })
                })
                .cloned()
                .collect();
            let restored = !aliases.is_empty();
            for alias in aliases {
                if !self.transfer_complete_draft(&alias, after, cx) {
                    return;
                }
            }
            if restored && self.composer_key.as_ref() == Some(after) {
                self.restore_composer_draft(window, cx);
            }
        }
        self.ensure_send_catalog(cx);
        if self.selected.is_none()
            && self.composer_has_sendable_content(cx)
            && self
                .chat_drafts
                .slots
                .get(&self.runtime)
                .is_some_and(|slot| slot.preparation == Preparation::Named)
        {
            self.prepare_chat_draft(window, cx);
        }
    }

    pub(crate) fn prepare_chat_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.chat_draft_open() || !self.persistence_ready() || self.close_in_progress() {
            return;
        }
        let Some(key) = self.chat_draft_key() else {
            return;
        };
        if self.chat_drafts.cleanups.contains_key(&key) {
            return;
        }
        if !self
            .chat_drafts
            .slots
            .get(&self.runtime)
            .is_some_and(|slot| {
                matches!(slot.preparation, Preparation::Named | Preparation::Failed)
            })
        {
            return;
        }
        if !self.is_chat_creation(&key) {
            self.retry_failed_chat_roots(window, cx);
            return;
        }
        let Some(connection) = self
            .connection
            .as_ref()
            .filter(|connection| connection.state == ConnectionState::Connected)
        else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let generation = connection.generation;
        let executor = connection.network.handle();
        let Some((stamp, saved)) = self.persistence_barrier(cx) else {
            return;
        };
        let Some(slot) = self.chat_drafts.slots.get_mut(&self.runtime) else {
            return;
        };
        if !slot.preparation.begin() {
            return;
        }
        let Some(roots) = slot.roots.clone() else {
            slot.preparation = Preparation::Failed;
            return;
        };
        let revision = slot.generation;
        self.chat_drafts.directories.insert(
            key.clone(),
            DirectoryCustody {
                roots: roots.clone(),
                client: client.clone(),
                executor: executor.clone(),
            },
        );
        let directory = key.directory().to_owned();
        let owner = key.clone();
        self.chat_drafts.preparations.insert(
            key,
            cx.spawn_in(window, async move |this, cx| {
                let saved = saved
                    .await
                    .unwrap_or(Err(crate::journal_worker::JournalFailure::WorkerStopped));
                let dispatch = this
                    .update_in(cx, |this, _, cx| {
                        this.record_persistence(stamp, &saved, cx)
                            && this.persistence_ready()
                            && this
                                .chat_drafts
                                .slots
                                .get(owner.runtime())
                                .is_some_and(|slot| {
                                    slot.generation == revision
                                        && slot.folder.as_deref() == Some(owner.directory())
                                })
                    })
                    .unwrap_or(false);
                let succeeded = if dispatch {
                    let captured = client.clone();
                    let roots = roots.clone();
                    let directory = directory.clone();
                    matches!(
                        crate::network::NetworkTask::spawn_on(&executor, async move {
                            captured.create_chat_directory(&roots, &directory).await
                        })
                        .await,
                        Some(Ok(()))
                    )
                } else {
                    false
                };
                let _ = this.update_in(cx, |this, window, cx| {
                    this.chat_drafts.preparations.remove(&owner);
                    let current = this
                        .chat_drafts
                        .slots
                        .get_mut(owner.runtime())
                        .filter(|slot| {
                            slot.generation == revision
                                && slot.folder.as_deref() == Some(owner.directory())
                        });
                    if let Some(slot) = current {
                        slot.preparation.complete(succeeded);
                        this.starter_chat_prepared(&owner, succeeded, window, cx);
                    }
                    let shown = this.runtime == *owner.runtime()
                        && this
                            .connection
                            .as_ref()
                            .is_some_and(|connection| connection.generation == generation)
                        && this.composer_key.as_ref() == Some(&owner);
                    if shown {
                        if succeeded {
                            this.ensure_send_catalog(cx);
                            this.resume_chat_attachments(&owner, window, cx);
                        } else {
                            this.notify_mutation_failure(FAILED, window, cx);
                        }
                    }
                    this.resume_chat_retarget(window, cx);
                    let cleanup_requested = this.chat_drafts.cleanup_requested.remove(&owner);
                    if cleanup_requested
                        || !this
                            .chat_drafts
                            .slots
                            .get(owner.runtime())
                            .is_some_and(|slot| {
                                slot.active && slot.folder.as_deref() == Some(owner.directory())
                            })
                    {
                        this.cleanup_unused_chat_directory(&owner, window, cx);
                    }
                    cx.notify();
                });
            }),
        );
        cx.notify();
    }

    pub(crate) fn renew_chat_folder(
        &mut self,
        runtime: &RuntimeId,
        used: &DraftKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(used.target(), DraftTarget::NewSession(id) if id.as_str() == CHAT_DRAFT_ID) {
            return;
        }
        // A recovered creation may reach handoff without a live preparation
        // slot. Its consumed folder must still be excluded from restoration.
        self.chat_drafts.handed_off.insert(used.clone());
        if runtime != &self.runtime || self.chat_retarget_busy(used) {
            self.chat_drafts.pending_renewals.insert(used.clone());
            return;
        }
        let previous_slot = self.chat_drafts.slots.get(runtime).cloned();
        let Some(slot) = self
            .chat_drafts
            .slots
            .get_mut(runtime)
            .filter(|slot| slot.folder.as_deref() == Some(used.directory()))
        else {
            return;
        };
        slot.folder = None;
        slot.preparation = Preparation::Named;
        slot.generation = slot.generation.wrapping_add(1);
        let on_draft = self.selected.is_none() && self.composer_key.as_ref() == Some(used);
        if on_draft {
            self.save_composer_draft(cx);
        }
        self.name_chat_folder();
        if let Some(after) = self
            .chat_drafts
            .slots
            .get(runtime)
            .and_then(|slot| slot.folder.as_deref())
            .and_then(|folder| {
                DraftKey::new(
                    runtime.clone(),
                    folder,
                    DraftTarget::NewSession(CHAT_DRAFT_ID.into()),
                )
            })
        {
            if !self.transfer_complete_draft(used, &after, cx) {
                if let Some(slot) = previous_slot {
                    self.chat_drafts.slots.insert(runtime.clone(), slot);
                }
                self.notify_mutation_failure(FAILED, window, cx);
                return;
            }
            if on_draft {
                self.composer_key = Some(after);
                self.restore_composer_draft(window, cx);
            }
        }
    }

    pub(crate) fn cleanup_unused_chat_directory(
        &mut self,
        key: &DraftKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_chat_creation(key)
            || self.drafts.get(key).is_some()
            || self
                .creations
                .iter()
                .any(|(_, attempt)| &attempt.source == key)
            || self
                .starters
                .captures
                .iter()
                .any(|capture| &capture.owner == key)
            || self.chat_drafts.cleanups.contains_key(key)
        {
            return;
        }
        if self.chat_drafts.preparations.contains_key(key) {
            self.chat_drafts.cleanup_requested.insert(key.clone());
            return;
        }
        let Some(custody) = self.chat_drafts.directories.get(key).cloned() else {
            return;
        };
        if self.chat_drafts.handed_off.contains(key) {
            return;
        }
        let roots = custody.roots;
        if self
            .sessions
            .active()
            .iter()
            .chain(self.sessions.archived())
            .any(|session| {
                session.directory.as_deref().is_some_and(|directory| {
                    roots.same_session_directory(directory, key.directory())
                })
            })
        {
            return;
        }
        let (client, executor) = if key.runtime() == &self.runtime {
            match self.connection.as_ref().and_then(|connection| {
                connection
                    .client
                    .clone()
                    .map(|client| (client, connection.network.handle()))
            }) {
                Some(server) => server,
                None => (custody.client, custody.executor),
            }
        } else {
            (custody.client, custody.executor)
        };
        let directory = key.directory().to_owned();
        if let Some(slot) = self.chat_drafts.slots.get_mut(key.runtime())
            && slot.folder.as_deref() == Some(key.directory())
        {
            slot.preparation = Preparation::Named;
        }
        let cleanup = crate::network::NetworkTask::spawn_on(&executor, async move {
            client.delete_chat_directory(&roots, &directory).await
        });
        let owner = key.clone();
        self.chat_drafts.cleanups.insert(
            key.clone(),
            cx.spawn_in(window, async move |this, cx| {
                let succeeded = matches!(cleanup.await, Some(Ok(())));
                let _ = this.update_in(cx, |this, window, cx| {
                    this.chat_drafts.cleanups.remove(&owner);
                    if succeeded {
                        this.chat_drafts.cleanup_failed.remove(&owner);
                        this.chat_drafts.directories.remove(&owner);
                        if let Some(slot) = this.chat_drafts.slots.get_mut(owner.runtime())
                            && slot.folder.as_deref() == Some(owner.directory())
                        {
                            slot.preparation = Preparation::Named;
                        }
                        if this.chat_draft_key().as_ref() == Some(&owner)
                            && this.composer_has_sendable_content(cx)
                        {
                            this.prepare_chat_draft(window, cx);
                        }
                    } else {
                        this.chat_drafts.cleanup_failed.insert(owner.clone());
                        if this.runtime == *owner.runtime() {
                            this.notify_mutation_failure(FAILED, window, cx);
                        }
                    }
                    cx.notify();
                });
            }),
        );
    }

    pub(crate) fn retry_failed_chat_roots(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.chat_roots_status() != RootsStatus::Failed || self.chat_drafts.roots_read.is_some()
        {
            return;
        }
        let Some(connection) = self
            .connection
            .as_ref()
            .filter(|connection| connection.state == ConnectionState::Connected)
        else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let runtime = self.runtime.clone();
        let generation = connection.generation;
        let read = connection
            .network
            .run(async move { client.chat_roots().await });
        self.chat_drafts.roots_read = Some(cx.spawn_in(window, async move |this, cx| {
            let result = read.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.chat_drafts.roots_read = None;
                if this.runtime != runtime
                    || this
                        .connection
                        .as_ref()
                        .is_none_or(|connection| connection.generation != generation)
                {
                    return;
                }
                match result {
                    Some(Ok(roots)) => {
                        this.topology.chat_roots = roots;
                        this.session_admission.roots_ready = true;
                        this.chat_roots_read(true, window, cx);
                    }
                    Some(Err(_)) => this.chat_roots_read(false, window, cx),
                    None => {}
                }
                cx.notify();
            });
        }));
    }
}
