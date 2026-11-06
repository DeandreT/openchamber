//! Model-scoped profile effort memory. Composer overrides remain in their journal.
//!
//! Picks update the effective map synchronously. One write per original runtime
//! reads and replaces the server's map, preserving unrelated model entries.
//! Failed edits remain live until explicit retry. Shutdown callers drain this
//! owner and retain its captured network runtimes until the drain completes.

use std::collections::{HashMap, HashSet};

use gpui_kit::{Context, Task};
use openchamber_native_core::ids::{MessageId, RuntimeId};
use openchamber_native_core::send_config::ModelChoice;
use openchamber_native_transport::client::{ServerClient, ServerSettings, SettingsChanges};

use crate::network::NetworkTask;
use crate::workspace::Workspace;

const MEMORY_LIMIT: usize = 512;
type Memory = HashMap<String, Option<String>>;

enum ChoiceSource {
    Picker,
    SuccessfulSend,
}

fn trim_choice(value: &str) -> &str {
    value.trim_matches(|ch: char| (ch.is_whitespace() && ch != '\u{0085}') || ch == '\u{feff}')
}

fn effort_key(model: &ModelChoice) -> String {
    format!(
        "{}/{}",
        trim_choice(&model.provider_id),
        trim_choice(&model.model_id)
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Edit {
    key: String,
    effort: Option<String>,
    revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Write {
    snapshot: Snapshot,
    edits: Vec<Edit>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Snapshot {
    revision: u64,
    sequence: u64,
}

#[derive(Default)]
struct State {
    values: Memory,
    order: Vec<String>,
    revision: u64,
    revisions: HashMap<String, u64>,
    effort_revisions: HashMap<String, u64>,
    dirty: HashMap<String, Edit>,
    writing: Option<Write>,
    failed: HashSet<String>,
    next_snapshot: u64,
    last_snapshot: u64,
}

impl State {
    fn pick(&mut self, key: String, effort: Option<String>) {
        // Only an explicit choice for this model retries its failed edit.
        // Unrelated successful sends can proceed without resubmitting it.
        self.failed.remove(&key);
        self.remember(key, effort);
    }

    fn remember(&mut self, key: String, effort: Option<String>) {
        self.revision += 1;
        self.revisions.insert(key.clone(), self.revision);
        *self.effort_revisions.entry(key.clone()).or_default() += 1;
        // A repeat still supersedes an older send, without a redundant PUT.
        if self.values.get(&key) == Some(&effort) {
            return;
        }
        put(&mut self.values, &mut self.order, &key, effort.clone());
        self.dirty.insert(
            key.clone(),
            Edit {
                key,
                effort,
                revision: self.revision,
            },
        );
        while self.order.len() > MEMORY_LIMIT {
            let oldest = self.order.remove(0);
            self.values.remove(&oldest);
            self.dirty.remove(&oldest);
            self.failed.remove(&oldest);
            self.revisions.insert(oldest, self.revision);
        }
    }

    fn begin(&mut self) -> Option<Write> {
        if self.writing.is_some() {
            return None;
        }
        let mut edits: Vec<Edit> = self
            .dirty
            .values()
            .filter(|edit| !self.failed.contains(&edit.key))
            .cloned()
            .collect();
        if edits.is_empty() {
            return None;
        }
        edits.sort_by_key(|edit| edit.revision);
        let write = Write {
            snapshot: self.capture_snapshot(),
            edits,
        };
        self.writing = Some(write.clone());
        Some(write)
    }

    fn capture_snapshot(&mut self) -> Snapshot {
        self.next_snapshot += 1;
        Snapshot {
            revision: self.revision,
            sequence: self.next_snapshot,
        }
    }

    fn reconcile(&mut self, snapshot: Snapshot, settings: &ServerSettings) {
        // Settings reads and whole-map write responses share one ordering domain.
        if settings.last_used_efforts.is_some() && snapshot.sequence >= self.last_snapshot {
            self.adopt(snapshot.revision, settings);
            self.last_snapshot = snapshot.sequence;
        }
    }

    fn adopt(&mut self, revision: u64, settings: &ServerSettings) {
        let Some(incoming) = &settings.last_used_efforts else {
            return;
        };
        let mut values = incoming.clone();
        let mut order = memory_order(incoming, &settings.last_used_effort_order);
        for (key, changed) in &self.revisions {
            if *changed > revision && !self.values.contains_key(key) {
                values.remove(key);
                order.retain(|entry| entry != key);
            }
        }
        for key in &self.order {
            if self
                .revisions
                .get(key)
                .is_some_and(|changed| *changed > revision)
                || self.dirty.contains_key(key)
            {
                put(&mut values, &mut order, key, self.values[key].clone());
            }
        }
        trim(&mut values, &mut order);
        self.values = values;
        self.order = order;
    }

    fn finish(&mut self, write: &Write, response: Option<&ServerSettings>) {
        if self.writing.as_ref() != Some(write) {
            return;
        }
        self.writing = None;
        let accepted = response.and_then(|settings| settings.last_used_efforts.as_ref());
        for edit in &write.edits {
            if accepted.and_then(|values| values.get(&edit.key)) == Some(&edit.effort) {
                if self.dirty.get(&edit.key) == Some(edit) {
                    self.dirty.remove(&edit.key);
                    self.failed.remove(&edit.key);
                    // A read begun while this write was pending can still carry
                    // the old server value after acknowledgement cleared dirty.
                    self.revision += 1;
                    self.revisions.insert(edit.key.clone(), self.revision);
                }
            } else if self.dirty.get(&edit.key) == Some(edit) {
                self.failed.insert(edit.key.clone());
            }
        }
        if let Some(response) = response {
            self.reconcile(write.snapshot, response);
        }
    }

    fn flushed(&self) -> bool {
        self.writing.is_none() && self.dirty.keys().all(|key| self.failed.contains(key))
    }

    fn has_failed_edits(&self) -> bool {
        !self.failed.is_empty()
    }

    fn project(&self, settings: &mut ServerSettings) {
        settings.last_used_efforts = (!self.values.is_empty()).then(|| self.values.clone());
        settings.last_used_effort_order = self.order.clone();
    }
}

fn memory_order(values: &Memory, supplied: &[String]) -> Vec<String> {
    let mut order = Vec::new();
    for key in supplied {
        if values.contains_key(key) && !order.contains(key) {
            order.push(key.clone());
        }
    }
    // Programmatic settings in tests can omit order. Real wire reads preserve it.
    let mut missing: Vec<String> = values
        .keys()
        .filter(|key| !order.contains(key))
        .cloned()
        .collect();
    missing.sort();
    order.extend(missing);
    order
}

fn put(values: &mut Memory, order: &mut Vec<String>, key: &str, effort: Option<String>) {
    values.insert(key.to_owned(), effort);
    order.retain(|entry| entry != key);
    order.push(key.to_owned());
}

fn trim(values: &mut Memory, order: &mut Vec<String>) {
    let excess = order.len().saturating_sub(MEMORY_LIMIT);
    for key in order.drain(..excess) {
        values.remove(&key);
    }
}

#[derive(Clone)]
struct Target {
    client: ServerClient,
    network: tokio::runtime::Handle,
}

#[derive(Default)]
struct RuntimePreferences {
    state: State,
    target: Option<Target>,
    task: Option<Task<()>>,
    io: Option<NetworkTask<Option<ServerSettings>>>,
}

/// Captured before a settings read. Reconcile only its original runtime.
#[derive(Clone, Debug)]
#[must_use]
pub(crate) struct EffortRead {
    runtime: RuntimeId,
    snapshot: Snapshot,
}

/// Memory-only submit capture. Never reconstruct this from the mutation journal.
#[derive(Clone, Debug)]
#[must_use]
pub(crate) struct UserEffort {
    runtime: RuntimeId,
    instance: u64,
    model: ModelChoice,
    revision: u64,
}

#[derive(Default)]
pub(crate) struct EffortPreferences {
    runtimes: HashMap<RuntimeId, RuntimePreferences>,
    active: Option<RuntimeId>,
    instance: u64,
    closing: bool,
    waiters: Vec<tokio::sync::oneshot::Sender<()>>,
    user_sends: HashMap<(RuntimeId, MessageId), UserEffort>,
}

impl EffortPreferences {
    pub(crate) fn is_closing(&self) -> bool {
        self.closing
    }

    /// Invalidate submit captures as soon as a different endpoint is chosen,
    /// even if that connection never reaches verified runtime activation.
    pub(crate) fn invalidate_instance(&mut self) {
        self.instance += 1;
    }

    /// Call on every runtime switch, including a switch away and back.
    pub(crate) fn activate(&mut self, runtime: &RuntimeId) {
        if self.active.as_ref() != Some(runtime) {
            self.instance += 1;
            self.active = Some(runtime.clone());
        }
    }

    /// Pending and failed work must retain its original Tokio runtime for retry.
    pub(crate) fn holds_runtime(&self, runtime: &RuntimeId) -> bool {
        self.runtimes.get(runtime).is_some_and(|slot| {
            slot.io.is_some() || slot.state.writing.is_some() || !slot.state.dirty.is_empty()
        })
    }

    pub(crate) fn has_failed_writes(&self) -> bool {
        self.runtimes
            .values()
            .any(|slot| slot.state.has_failed_edits())
    }

    /// Bind a submit-time capture to its original message identity, in memory only.
    pub(crate) fn retain_user_send(
        &mut self,
        runtime: &RuntimeId,
        id: &MessageId,
        capture: UserEffort,
    ) {
        if &capture.runtime == runtime {
            self.user_sends
                .entry((runtime.clone(), id.clone()))
                .or_insert(capture);
        }
    }

    pub(crate) fn take_user_send(
        &mut self,
        runtime: &RuntimeId,
        id: &MessageId,
    ) -> Option<UserEffort> {
        self.user_sends.remove(&(runtime.clone(), id.clone()))
    }

    pub(crate) fn forget_user_send(&mut self, runtime: &RuntimeId, id: &MessageId) {
        self.user_sends.remove(&(runtime.clone(), id.clone()));
    }

    fn accepts(&self, capture: &UserEffort) -> bool {
        let key = effort_key(&capture.model);
        self.active.as_ref() == Some(&capture.runtime)
            && self.instance == capture.instance
            && self.runtimes.get(&capture.runtime).is_some_and(|slot| {
                slot.state.effort_revisions.get(&key).copied().unwrap_or(0) == capture.revision
            })
    }

    fn flushed(&self) -> bool {
        self.runtimes.values().all(|slot| slot.state.flushed())
    }

    fn wake_close(&mut self) {
        if self.flushed() {
            for waiter in self.waiters.drain(..) {
                let _ = waiter.send(());
            }
        }
    }
}

fn changes(settings: &ServerSettings, write: &Write) -> SettingsChanges {
    let mut values = settings.last_used_efforts.clone().unwrap_or_default();
    let mut order = memory_order(&values, &settings.last_used_effort_order);
    for edit in &write.edits {
        put(&mut values, &mut order, &edit.key, edit.effort.clone());
    }
    trim(&mut values, &mut order);
    SettingsChanges {
        last_used_efforts: Some(
            order
                .into_iter()
                .map(|key| {
                    let effort = values
                        .remove(&key)
                        .expect("the order contains only known keys");
                    (key, effort)
                })
                .collect(),
        ),
        ..SettingsChanges::default()
    }
}

async fn save(target: &Target, write: &Write) -> Option<ServerSettings> {
    // The endpoint replaces this whole property. Refresh its valid siblings
    // before applying the captured model edits to the original server profile.
    let settings = target.client.settings().await.ok()?;
    target
        .client
        .save_settings(&changes(&settings, write))
        .await
        .ok()
}

impl Workspace {
    fn prepare_effort_preferences(&mut self) {
        self.effort_preferences.activate(&self.runtime);
        let slot = self
            .effort_preferences
            .runtimes
            .entry(self.runtime.clone())
            .or_default();
        if slot.state.revision == 0
            && slot.state.last_snapshot == 0
            && let Some(settings) = &self.send_catalog.settings
        {
            slot.state.adopt(0, settings);
        }
        if let Some(connection) = &self.connection
            && let Some(client) = &connection.client
        {
            slot.target = Some(Target {
                client: client.clone(),
                network: connection.network.handle(),
            });
        }
    }

    /// Capture before starting a settings request, then use `reconcile_effort_settings`.
    pub(crate) fn begin_effort_settings_read(&mut self) -> EffortRead {
        self.prepare_effort_preferences();
        let state = &mut self
            .effort_preferences
            .runtimes
            .get_mut(&self.runtime)
            .expect("the current runtime was prepared")
            .state;
        EffortRead {
            runtime: self.runtime.clone(),
            snapshot: state.capture_snapshot(),
        }
    }

    /// Overlay newer/unsaved model choices before publishing a settings response.
    pub(crate) fn reconcile_effort_settings(
        &mut self,
        read: &EffortRead,
        settings: &mut ServerSettings,
    ) {
        let Some(slot) = self.effort_preferences.runtimes.get_mut(&read.runtime) else {
            return;
        };
        slot.state.reconcile(read.snapshot, settings);
        slot.state.project(settings);
    }

    fn project_effort_preferences(&mut self, runtime: &RuntimeId) {
        if &self.runtime == runtime
            && let Some(settings) = &mut self.send_catalog.settings
            && let Some(slot) = self.effort_preferences.runtimes.get(runtime)
        {
            slot.state.project(settings);
        }
    }

    fn remember_effort_choice(
        &mut self,
        model: &ModelChoice,
        source: ChoiceSource,
        cx: &mut Context<Self>,
    ) {
        if trim_choice(&model.provider_id).is_empty() || trim_choice(&model.model_id).is_empty() {
            return;
        }
        self.prepare_effort_preferences();
        let runtime = self.runtime.clone();
        let slot = self
            .effort_preferences
            .runtimes
            .get_mut(&runtime)
            .expect("the current runtime was prepared");
        let effort = model
            .variant
            .as_deref()
            .map(trim_choice)
            .filter(|variant| !variant.is_empty())
            .map(str::to_owned);
        match source {
            ChoiceSource::Picker => slot.state.pick(effort_key(model), effort),
            ChoiceSource::SuccessfulSend => slot.state.remember(effort_key(model), effort),
        }
        self.project_effort_preferences(&runtime);
        self.start_effort_write(&runtime, cx);
        cx.notify();
    }

    /// Only explicit picker effort gestures call this; model-only picks do not.
    pub(crate) fn remember_picker_effort(&mut self, model: &ModelChoice, cx: &mut Context<Self>) {
        if !self.effort_preferences.closing {
            self.remember_effort_choice(model, ChoiceSource::Picker, cx);
        }
    }

    /// Capture with the outgoing model before any attachment or admission await.
    pub(crate) fn capture_user_effort(&mut self, model: &ModelChoice) -> UserEffort {
        self.prepare_effort_preferences();
        let key = effort_key(model);
        let slot = &self.effort_preferences.runtimes[&self.runtime];
        UserEffort {
            runtime: self.runtime.clone(),
            instance: self.effort_preferences.instance,
            model: model.clone(),
            revision: slot.state.effort_revisions.get(&key).copied().unwrap_or(0),
        }
    }

    /// Call only after an ordinary user-composer send succeeds. BTW, queue-only,
    /// background and recovered sends never create this profile write. Close
    /// freezes new records while already accepted edits finish draining.
    pub(crate) fn record_user_effort(&mut self, capture: UserEffort, cx: &mut Context<Self>) {
        if !self.effort_preferences.is_closing()
            && self.runtime == capture.runtime
            && self.effort_preferences.accepts(&capture)
        {
            self.remember_effort_choice(&capture.model, ChoiceSource::SuccessfulSend, cx);
        }
    }

    fn start_effort_write(&mut self, runtime: &RuntimeId, cx: &mut Context<Self>) {
        let Some(slot) = self.effort_preferences.runtimes.get_mut(runtime) else {
            return;
        };
        let Some(write) = slot.state.begin() else {
            return;
        };
        let Some(target) = slot.target.clone() else {
            slot.state.finish(&write, None);
            self.effort_preferences.wake_close();
            self.session_management.notice =
                Some(crate::locale::strings(cx).get("settings.common.status.saveFailed"));
            return;
        };
        let (completed, receipt) = tokio::sync::oneshot::channel();
        let network = target.network.clone();
        let carried = write.clone();
        slot.io = Some(NetworkTask::spawn_on(&network, async move {
            let response = save(&target, &carried).await;
            let _ = completed.send(response.clone());
            response
        }));
        let runtime = runtime.clone();
        slot.task = Some(cx.spawn(async move |this, cx| {
            let response = receipt.await.ok().flatten();
            let _ = this.update(cx, |this, cx| {
                let Some(slot) = this.effort_preferences.runtimes.get_mut(&runtime) else {
                    return;
                };
                slot.task = None;
                slot.io = None;
                slot.state.finish(&write, response.as_ref());
                if slot.state.has_failed_edits() {
                    this.session_management.notice =
                        Some(crate::locale::strings(cx).get("settings.common.status.saveFailed"));
                }
                this.project_effort_preferences(&runtime);
                this.start_effort_write(&runtime, cx);
                this.effort_preferences.wake_close();
                cx.notify();
            });
        }));
    }

    /// Await accepted writes, then veto close if `has_failed_writes` is true.
    pub(crate) fn take_effort_close(&mut self) -> Option<tokio::sync::oneshot::Receiver<()>> {
        self.effort_preferences.closing = true;
        if self.effort_preferences.flushed() {
            return None;
        }
        let (ready, waiting) = tokio::sync::oneshot::channel();
        self.effort_preferences.waiters.push(ready);
        Some(waiting)
    }

    pub(crate) fn cancel_effort_close(&mut self) {
        self.effort_preferences.closing = false;
    }

    /// The caller retains active and retired Networks until this task finishes.
    pub(crate) fn take_effort_quit_flush(&mut self, cx: &Context<Self>) -> Option<Task<()>> {
        self.effort_close = None;
        self.effort_preferences.closing = true;
        self.effort_preferences.waiters.clear();
        let mut drains = Vec::new();
        for slot in self.effort_preferences.runtimes.values_mut() {
            slot.task = None;
            let pending = slot.io.take();
            let previous = slot.state.writing.take();
            if let Some(previous) = &previous {
                for edit in &previous.edits {
                    if slot.state.dirty.get(&edit.key) == Some(edit) {
                        slot.state.dirty.remove(&edit.key);
                        slot.state.failed.remove(&edit.key);
                    }
                }
            }
            let queued = slot.state.begin();
            let target = slot.target.clone();
            if pending.is_none() && queued.is_none() {
                continue;
            }
            drains.push(cx.background_executor().spawn(async move {
                if let Some(pending) = pending {
                    let _ = pending.await;
                }
                if let (Some(write), Some(target)) = (queued, target) {
                    let network = target.network.clone();
                    let saving =
                        NetworkTask::spawn_on(&network, async move { save(&target, &write).await });
                    let _ = saving.await;
                }
            }));
        }
        if drains.is_empty() {
            return None;
        }
        Some(cx.background_executor().spawn(async move {
            for drain in drains {
                drain.await;
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(entries: &[(&str, Option<&str>)]) -> ServerSettings {
        ServerSettings {
            last_used_efforts: Some(
                entries
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), value.map(str::to_owned)))
                    .collect(),
            ),
            last_used_effort_order: entries.iter().map(|(key, _)| (*key).to_owned()).collect(),
            ..ServerSettings::default()
        }
    }

    #[test]
    fn choices_are_immediate_and_profile_writes_preserve_other_models_and_null() {
        let original = settings(&[("p/other", Some("low")), ("p/model", Some("low"))]);
        let mut state = State::default();
        state.adopt(0, &original);
        state.remember("p/model".to_owned(), None);
        assert_eq!(state.values.get("p/model"), Some(&None));
        let write = state.begin().expect("the explicit Default needs a save");
        assert!(state.begin().is_none(), "writes serialize per runtime");
        let payload = changes(&original, &write);
        assert_eq!(
            serde_json::to_string(&payload).expect("profile changes serialize"),
            r#"{"lastUsedEfforts":{"p/other":"low","p/model":null}}"#
        );
        assert_eq!(
            original.last_used_efforts.as_ref().expect("map")["p/model"].as_deref(),
            Some("low")
        );
    }

    #[test]
    fn newer_picker_choices_survive_old_write_responses_and_reads_after_ack() {
        let original = settings(&[("p/model", Some("low"))]);
        let mut state = State::default();
        state.adopt(0, &original);
        state.remember("p/model".to_owned(), Some("high".to_owned()));
        let high = state.begin().expect("High is pending");
        state.remember("p/model".to_owned(), None);
        let old_read = state.revision;
        state.finish(&high, Some(&settings(&[("p/model", Some("high"))])));
        assert_eq!(state.values.get("p/model"), Some(&None));
        let default = state
            .begin()
            .expect("the newer Default still needs its write");
        state.finish(&default, Some(&settings(&[("p/model", None)])));
        assert!(state.flushed());
        state.adopt(old_read, &original);
        assert_eq!(
            state.values.get("p/model"),
            Some(&None),
            "an old read cannot undo an acknowledged edit"
        );
    }

    #[test]
    fn failures_retain_choices_and_require_retry_but_superseded_failures_do_not_block() {
        let mut state = State::default();
        state.remember("p/model".to_owned(), Some("high".to_owned()));
        let high = state.begin().expect("High is pending");
        state.finish(&high, None);
        assert!(state.has_failed_edits());
        assert!(state.begin().is_none(), "failure must not silently retry");
        state.adopt(0, &settings(&[("p/model", Some("low"))]));
        assert_eq!(state.values["p/model"].as_deref(), Some("high"));
        state.pick("p/model".to_owned(), Some("high".to_owned()));
        let retry = state.begin().expect("explicit retry retains the edit");
        state.remember("p/model".to_owned(), None);
        state.finish(&retry, None);
        assert!(
            !state.has_failed_edits(),
            "the failed older value was already superseded"
        );
        assert!(state.begin().is_some(), "the newer choice can proceed");
    }

    #[test]
    fn reselecting_the_same_picker_effort_retries_retained_dirty_intent() {
        let mut state = State::default();
        state.pick("p/model".to_owned(), None);
        let first = state.begin().expect("Default has an initial write");
        state.finish(&first, None);
        assert!(state.has_failed_edits());
        state.remember("p/model".to_owned(), None);
        assert!(
            state.begin().is_none(),
            "an unchanged send result is not a retry gesture"
        );
        state.pick("p/model".to_owned(), None);
        let retried = state
            .begin()
            .expect("an explicit reselect flushes the retained edit");
        assert_eq!(
            retried.edits, first.edits,
            "the original dirty value is retained"
        );
        state.finish(&retried, Some(&settings(&[("p/model", None)])));
        assert!(state.flushed());
        assert!(!state.has_failed_edits());
    }

    #[test]
    fn successful_send_for_another_model_does_not_retry_a_failed_edit() {
        let original = settings(&[
            ("p/a", Some("low")),
            ("p/b", Some("low")),
            ("p/sibling", None),
        ]);
        let mut state = State::default();
        state.adopt(0, &original);
        state.pick("p/a".to_owned(), Some("high".to_owned()));
        let failed = state.begin().expect("A has its initial write");
        state.finish(&failed, None);

        state.remember("p/b".to_owned(), Some("high".to_owned()));
        let automatic = state.begin().expect("B can proceed independently");
        assert_eq!(automatic.edits.len(), 1);
        assert_eq!(automatic.edits[0].key, "p/b");
        let payload = changes(&original, &automatic);
        assert_eq!(
            payload.last_used_efforts,
            Some(vec![
                ("p/a".to_owned(), Some("low".to_owned())),
                ("p/sibling".to_owned(), None),
                ("p/b".to_owned(), Some("high".to_owned())),
            ]),
            "B preserves the server's A value without resubmitting failed High"
        );
        state.finish(
            &automatic,
            Some(&settings(&[
                ("p/a", Some("low")),
                ("p/sibling", None),
                ("p/b", Some("high")),
            ])),
        );
        assert!(state.has_failed_edits(), "A still vetoes normal close");
        assert!(state.flushed(), "all eligible work has drained");
        assert_eq!(state.values["p/a"].as_deref(), Some("high"));

        state.pick("p/b".to_owned(), Some("high".to_owned()));
        assert!(state.begin().is_none(), "a B pick cannot retry failed A");
        state.remember("p/a".to_owned(), Some("high".to_owned()));
        assert!(
            state.begin().is_none(),
            "an automatic A repeat is not a retry"
        );
        state.pick("p/a".to_owned(), Some("high".to_owned()));
        let retry = state.begin().expect("an explicit A reselect retries A");
        assert_eq!(retry.edits, failed.edits);
        state.finish(
            &retry,
            Some(&settings(&[
                ("p/sibling", None),
                ("p/b", Some("high")),
                ("p/a", Some("high")),
            ])),
        );
        assert!(state.flushed());
        assert!(!state.has_failed_edits());
        assert_eq!(state.order, ["p/sibling", "p/b", "p/a"]);
    }

    #[test]
    fn older_settings_read_cannot_replace_a_newer_whole_map_write_snapshot() {
        let original = settings(&[("p/a", Some("low")), ("p/b", Some("low"))]);
        let mut state = State::default();
        state.adopt(0, &original);
        let old_read = state.capture_snapshot();
        state.pick("p/a".to_owned(), Some("high".to_owned()));
        let write = state.begin().expect("A has a newer write snapshot");
        state.pick("p/a".to_owned(), None);
        state.finish(
            &write,
            Some(&settings(&[
                ("p/a", Some("high")),
                ("p/b", Some("high")),
                ("p/added", None),
            ])),
        );
        state.reconcile(old_read, &original);
        assert_eq!(state.values["p/a"], None, "newer local Default survives");
        assert_eq!(
            state.values["p/b"].as_deref(),
            Some("high"),
            "the write's unrelated server entry survives the delayed old GET"
        );
        assert_eq!(state.values.get("p/added"), Some(&None));
        assert_eq!(state.order, ["p/b", "p/added", "p/a"]);
    }

    #[test]
    fn older_write_response_cannot_replace_a_newer_whole_map_settings_read() {
        let original = settings(&[("p/a", Some("low")), ("p/b", Some("low"))]);
        let mut state = State::default();
        state.adopt(0, &original);
        state.pick("p/a".to_owned(), Some("high".to_owned()));
        let write = state.begin().expect("A has an initial write snapshot");
        let newer_read = state.capture_snapshot();
        state.pick("p/a".to_owned(), None);
        state.reconcile(
            newer_read,
            &settings(&[
                ("p/a", Some("high")),
                ("p/b", Some("high")),
                ("p/added", None),
            ]),
        );
        state.finish(
            &write,
            Some(&settings(&[("p/a", Some("high")), ("p/b", Some("low"))])),
        );
        assert_eq!(state.values["p/a"], None, "newer local Default survives");
        assert_eq!(
            state.values["p/b"].as_deref(),
            Some("high"),
            "the newer GET's unrelated entry survives the delayed old write"
        );
        assert_eq!(state.values.get("p/added"), Some(&None));
        assert_eq!(state.order, ["p/b", "p/added", "p/a"]);
        let pending = state
            .begin()
            .expect("newer local Default still needs its write");
        assert_eq!(pending.edits.len(), 1);
        assert_eq!(pending.edits[0].effort, None);
    }

    #[test]
    fn missing_or_malformed_field_does_not_erase_effective_memory() {
        let mut state = State::default();
        state.adopt(0, &settings(&[("p/model", Some("high"))]));
        let valid = state.capture_snapshot();
        let unknown = state.capture_snapshot();
        state.reconcile(unknown, &ServerSettings::default());
        assert_eq!(state.values["p/model"].as_deref(), Some("high"));
        state.reconcile(
            valid,
            &settings(&[("p/model", None), ("p/sibling", Some("low"))]),
        );
        assert_eq!(state.values["p/model"], None);
        assert_eq!(
            state.values["p/sibling"].as_deref(),
            Some("low"),
            "an unknown field grants no snapshot authority over a valid response"
        );
    }

    #[test]
    fn profile_retention_uses_actual_wire_order_and_keeps_the_newest_choice() {
        let original = ServerSettings {
            last_used_efforts: Some(
                (0..MEMORY_LIMIT)
                    .map(|index| (format!("p/{index:03}"), Some("low".to_owned())))
                    .collect(),
            ),
            last_used_effort_order: (0..MEMORY_LIMIT)
                .rev()
                .map(|index| format!("p/{index:03}"))
                .collect(),
            ..ServerSettings::default()
        };
        let mut state = State::default();
        state.adopt(0, &original);
        state.remember("p/new".to_owned(), None);
        assert_eq!(state.values.len(), MEMORY_LIMIT);
        assert!(
            !state.values.contains_key("p/511"),
            "the first wire entry is oldest"
        );
        assert_eq!(state.order.last().map(String::as_str), Some("p/new"));
        let payload = changes(
            &original,
            &state.begin().expect("the new model needs a save"),
        );
        let entries = payload
            .last_used_efforts
            .expect("the profile field is present");
        assert_eq!(entries.len(), MEMORY_LIMIT);
        assert_eq!(entries.last(), Some(&("p/new".to_owned(), None)));
        assert!(!entries.iter().any(|(key, _)| key == "p/511"));
    }

    #[test]
    fn same_value_picks_and_runtime_round_trips_supersede_user_send_captures() {
        let runtime = RuntimeId::from("original");
        let mut preferences = EffortPreferences::default();
        preferences.activate(&runtime);
        preferences.runtimes.entry(runtime.clone()).or_default();
        let model = ModelChoice {
            provider_id: "p".to_owned(),
            model_id: "model".to_owned(),
            variant: Some("high".to_owned()),
        };
        let capture = UserEffort {
            runtime: runtime.clone(),
            instance: preferences.instance,
            model,
            revision: 0,
        };
        assert!(preferences.accepts(&capture));
        let id = MessageId::from("msg_captured_effort");
        preferences.retain_user_send(&runtime, &id, capture.clone());
        assert!(
            preferences
                .take_user_send(&RuntimeId::from("other"), &id)
                .is_none()
        );
        assert!(preferences.take_user_send(&runtime, &id).is_some());
        assert!(
            preferences.take_user_send(&runtime, &id).is_none(),
            "submit captures settle once"
        );
        let state = &mut preferences
            .runtimes
            .get_mut(&runtime)
            .expect("original slot")
            .state;
        state.remember("p/model".to_owned(), Some("high".to_owned()));
        state.remember("p/model".to_owned(), Some("high".to_owned()));
        assert!(
            !preferences.accepts(&capture),
            "even a repeated picker value is newer intent"
        );
        let mut current = capture.clone();
        current.revision = 2;
        assert!(preferences.accepts(&current));
        let slot = preferences
            .runtimes
            .get_mut(&runtime)
            .expect("original slot");
        let write = slot.state.begin().expect("one profile write");
        slot.state
            .finish(&write, Some(&settings(&[("p/model", Some("high"))])));
        assert!(
            preferences.accepts(&current),
            "profile acknowledgement is not a newer picker choice"
        );
        preferences.invalidate_instance();
        assert!(
            !preferences.accepts(&current),
            "choosing another endpoint invalidates a send before that host connects"
        );
        current.instance = preferences.instance;
        preferences.activate(&RuntimeId::from("other"));
        preferences.activate(&runtime);
        assert!(
            !preferences.accepts(&current),
            "returning to the original runtime is a new instance"
        );
        assert!(
            preferences.runtimes[&runtime]
                .state
                .values
                .contains_key("p/model")
        );
    }

    #[test]
    fn failed_original_runtime_stays_retained_and_close_cannot_claim_success() {
        let runtime = RuntimeId::from("original");
        let mut preferences = EffortPreferences::default();
        let slot = preferences.runtimes.entry(runtime.clone()).or_default();
        slot.state.remember("p/model".to_owned(), None);
        let write = slot.state.begin().expect("the original profile write");
        slot.state.finish(&write, None);
        preferences.activate(&RuntimeId::from("other"));
        assert!(preferences.holds_runtime(&runtime));
        assert!(
            preferences.flushed(),
            "the close waiter settles after failure"
        );
        assert!(
            preferences.has_failed_writes(),
            "normal close must veto after draining"
        );
    }

    #[test]
    fn failed_edits_do_not_block_another_runtime() {
        let original = RuntimeId::from("original");
        let other = RuntimeId::from("other");
        let mut preferences = EffortPreferences::default();
        let slot = preferences.runtimes.entry(original.clone()).or_default();
        slot.state.pick("p/a".to_owned(), None);
        let failed = slot.state.begin().expect("the original runtime can write");
        slot.state.finish(&failed, None);

        let slot = preferences.runtimes.entry(other.clone()).or_default();
        slot.state
            .remember("p/b".to_owned(), Some("high".to_owned()));
        let write = slot
            .state
            .begin()
            .expect("the other runtime remains independent");
        assert_eq!(
            write.snapshot.sequence, 1,
            "snapshot order is runtime-scoped"
        );
        slot.state
            .finish(&write, Some(&settings(&[("p/b", Some("high"))])));
        assert!(preferences.has_failed_writes());
        assert!(
            preferences.holds_runtime(&original),
            "failed work retains its executor"
        );
        assert!(
            !preferences.holds_runtime(&other),
            "successful work can release its executor"
        );
        assert!(
            preferences.flushed(),
            "all remaining work requires explicit retry"
        );
    }

    #[gpui_kit::test]
    fn close_cancellation_preserves_failed_accepted_and_queued_edits(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        let (_, workspace) = crate::workspace_tests::open(cx);
        cx.update(|cx| {
            workspace.update(cx, |workspace, _| {
                let runtime = workspace.runtime.clone();
                let state = &mut workspace
                    .effort_preferences
                    .runtimes
                    .entry(runtime.clone())
                    .or_default()
                    .state;
                state.pick("p/a".to_owned(), Some("high".to_owned()));
                let failed = state.begin().expect("A has its initial write");
                state.finish(&failed, None);
                state.remember("p/b".to_owned(), Some("high".to_owned()));
                let accepted = state.begin().expect("B is accepted despite failed A");
                state.remember("p/c".to_owned(), None);
                let dirty = state.dirty.clone();
                let failures = state.failed.clone();

                let mut drained = workspace
                    .take_effort_close()
                    .expect("accepted work must drain");
                workspace.cancel_effort_close();
                assert!(!workspace.effort_preferences.is_closing());
                let state = &mut workspace
                    .effort_preferences
                    .runtimes
                    .get_mut(&runtime)
                    .expect("the original runtime is retained")
                    .state;
                assert_eq!(state.writing.as_ref(), Some(&accepted));
                assert_eq!(state.dirty, dirty);
                assert_eq!(state.failed, failures);
                state.finish(
                    &accepted,
                    Some(&settings(&[("p/a", Some("low")), ("p/b", Some("high"))])),
                );
                let queued = state
                    .begin()
                    .expect("accepted C can still drain after cancellation");
                assert_eq!(queued.edits.len(), 1);
                assert_eq!(queued.edits[0].key, "p/c");
                state.finish(
                    &queued,
                    Some(&settings(&[
                        ("p/a", Some("low")),
                        ("p/b", Some("high")),
                        ("p/c", None),
                    ])),
                );
                workspace.effort_preferences.wake_close();
                assert!(
                    matches!(drained.try_recv(), Ok(())),
                    "the accepted work settled"
                );
                assert!(workspace.effort_preferences.has_failed_writes());
                assert!(workspace.effort_preferences.holds_runtime(&runtime));
                assert!(
                    workspace.take_effort_close().is_none(),
                    "failed A is already drained"
                );
                workspace.cancel_effort_close();
                let state = &mut workspace
                    .effort_preferences
                    .runtimes
                    .get_mut(&runtime)
                    .expect("failed A remains retryable")
                    .state;
                state.pick("p/a".to_owned(), Some("high".to_owned()));
                let retry = state
                    .begin()
                    .expect("explicit A reselect remains available");
                assert_eq!(retry.edits, failed.edits);
            });
        });
    }

    #[gpui_kit::test]
    fn late_user_send_success_after_effort_close_drain_creates_no_profile_write(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::AppContext;
        use openchamber_native_core::ids::SessionId;
        use openchamber_native_transport::prompt::{Delivery, PromptReceipt};

        use crate::send_attempt::{Admission, SessionKey, tests as attempts};

        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.send_catalog.settings = Some(settings(&[("p/model", Some("low"))]));
                let model = ModelChoice {
                    provider_id: "p".to_owned(),
                    model_id: "model".to_owned(),
                    variant: Some("high".to_owned()),
                };
                let key = SessionKey {
                    runtime: workspace.runtime.clone(),
                    session: SessionId::from("ses_late_effort"),
                };
                let id = workspace.message_ids.next().0;
                let mut attempt = attempts::attempt(&key);
                attempt.admission = Admission::Sending { observed: false };
                attempt.prompt.message_id = id.clone();
                attempt.prompt.model = Some(model.clone());
                attempt.optimistic.info.id = id.clone();
                assert!(workspace.send_attempts.begin(key.clone(), attempt));
                let capture = workspace.capture_user_effort(&model);
                workspace
                    .effort_preferences
                    .retain_user_send(&key.runtime, &id, capture);
                assert!(
                    workspace.take_effort_close().is_none(),
                    "the effort drain has finished"
                );
                let state = &workspace.effort_preferences.runtimes[&key.runtime].state;
                let revision = state.revision;
                let snapshots = state.next_snapshot;

                // The window still exists for its remaining close barriers.
                workspace.finish_send(
                    &key,
                    &id,
                    Some(Ok(PromptReceipt {
                        session_id: key.session.clone(),
                        message_id: id.clone(),
                        created: 1,
                        delivery: Delivery::Queue,
                    })),
                    window,
                    cx,
                );
                let slot = &workspace.effort_preferences.runtimes[&key.runtime];
                assert_eq!(slot.state.revision, revision, "late success admits no edit");
                assert_eq!(
                    slot.state.next_snapshot, snapshots,
                    "late success starts no write"
                );
                assert!(slot.state.dirty.is_empty());
                assert!(slot.state.writing.is_none());
                assert!(slot.io.is_none() && slot.task.is_none());
                assert!(!workspace.effort_preferences.has_failed_writes());
                assert_eq!(slot.state.values["p/model"].as_deref(), Some("low"));
                assert!(
                    workspace
                        .effort_preferences
                        .take_user_send(&key.runtime, &id)
                        .is_none()
                );
                assert!(
                    workspace.take_effort_close().is_none(),
                    "no new drain appeared"
                );
                workspace.cancel_effort_close();
                assert!(!workspace.effort_preferences.is_closing());
            });
        })
        .expect("the test workspace remains open");
    }

    #[gpui_kit::test]
    fn only_successful_initial_user_sends_record_their_bound_effort(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::AppContext;
        use openchamber_native_core::ids::SessionId;
        use openchamber_native_transport::prompt::{Delivery, PromptReceipt};

        use crate::send_attempt::{Admission, SendOrigin, SessionKey, tests as attempts};

        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.send_catalog.settings = Some(ServerSettings::default());
                let sending = Admission::Sending { observed: false };
                let cases = [
                    (SendOrigin::Existing, sending, true, true, Some("high")),
                    (SendOrigin::Created, sending, true, true, None),
                    (SendOrigin::Existing, sending, false, true, None),
                    (SendOrigin::Existing, Admission::Uncertain, true, true, None),
                    (SendOrigin::Deleted, sending, true, true, None),
                    (SendOrigin::Existing, sending, true, false, None),
                ];
                for (index, (origin, admission, bound, success, expected)) in cases.into_iter().enumerate() {
                    let key = SessionKey {
                        runtime: workspace.runtime.clone(),
                        session: SessionId::new(format!("ses_effort_{index}")),
                    };
                    let id = workspace.message_ids.next().0;
                    let model = ModelChoice {
                        provider_id: "provider".to_owned(),
                        model_id: "model".to_owned(),
                        variant: if index == 0 { Some("high".to_owned()) }
                            else if index == 1 { None } else { Some("low".to_owned()) },
                    };
                    let mut attempt = attempts::attempt(&key);
                    attempt.origin = origin;
                    attempt.admission = admission;
                    attempt.prompt.message_id = id.clone();
                    attempt.prompt.model = Some(model.clone());
                    attempt.optimistic.info.id = id.clone();
                    assert!(workspace.send_attempts.begin(key.clone(), attempt));
                    if bound {
                        let capture = workspace.capture_user_effort(&model);
                        workspace.effort_preferences.retain_user_send(&key.runtime, &id, capture);
                    }
                    let receipt = success.then(|| Ok(PromptReceipt {
                        session_id: key.session.clone(), message_id: id.clone(),
                        created: 1, delivery: Delivery::Queue,
                    }));
                    workspace.finish_send(&key, &id, receipt, window, cx);
                    let map = workspace.send_catalog.settings.as_ref().expect("settings")
                        .last_used_efforts.as_ref().expect("the first send records its effort");
                    assert_eq!(map.get("provider/model"), Some(&expected.map(str::to_owned)),
                        "origin {origin:?}, admission {admission:?}, bound {bound}, success {success}");
                    assert!(workspace.effort_preferences.take_user_send(&key.runtime, &id).is_none());
                }
            });
        }).expect("the test workspace remains open");
    }

    #[gpui_kit::test]
    fn only_explicit_supported_picker_efforts_change_profile_memory(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::AppContext;
        use openchamber_native_core::composer_selection::Effort;
        use openchamber_native_core::draft::{DraftKey, DraftTarget};
        use openchamber_native_core::ids::DraftId;
        use openchamber_native_core::send_config::{AgentEntry, CatalogModel, CatalogProvider};

        use crate::send_catalog::{DirectoryCatalog, Load};

        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, cx| {
                let project = workspace.topology.projects[0].clone();
                let directory = project.path.clone();
                workspace.send_catalog.settings = Some(ServerSettings {
                    projects: vec![project],
                    default_model: Some("p/model".to_owned()),
                    ..settings(&[("p/other", Some("low"))])
                });
                workspace
                    .draft_projects
                    .insert(workspace.runtime.clone(), directory.clone());
                workspace.composer_key = Some(
                    DraftKey::new(
                        workspace.runtime.clone(),
                        &directory,
                        DraftTarget::NewSession(DraftId::from("effort-draft")),
                    )
                    .expect("registered draft directory"),
                );
                let mut catalog = DirectoryCatalog::default();
                catalog.agents = Load::Ready(vec![AgentEntry {
                    name: "build".to_owned(),
                    display_name: None,
                    mode: None,
                    hidden: false,
                    model: None,
                    color: None,
                }]);
                catalog.providers = Load::Ready(vec![CatalogProvider {
                    id: "p".to_owned(),
                    name: None,
                    models: vec![CatalogModel {
                        id: "model".to_owned(),
                        provider_model_id: None,
                        name: None,
                        variants: vec!["low".to_owned(), "high".to_owned(), "default".to_owned()],
                        context_limit: None,
                    }],
                }]);
                catalog.opencode = Load::Ready(Default::default());
                workspace
                    .send_catalog
                    .insert_test_directory(directory, catalog);
                workspace.refresh_composer_choices(cx);
                let owner = workspace.selection_owner().expect("the draft has an owner");
                let model = ModelChoice {
                    provider_id: "p".to_owned(),
                    model_id: "model".to_owned(),
                    variant: None,
                };
                workspace.choose_composer_model(&owner, model, cx);
                workspace.choose_composer_effort(&owner, Effort::Inherit, cx);
                workspace.choose_composer_effort(&owner, Effort::Named("retired".to_owned()), cx);
                assert!(
                    workspace.effort_preferences.runtimes.is_empty(),
                    "model picks, inheritance and rejected effort create no profile writes"
                );
                let old_read = workspace.begin_effort_settings_read();
                for effort in [
                    Effort::Named("high".to_owned()),
                    Effort::Named("default".to_owned()),
                    Effort::Default,
                ] {
                    let expected = effort.resolve(None);
                    workspace.choose_composer_effort(&owner, effort, cx);
                    let effective = workspace
                        .send_catalog
                        .settings
                        .as_ref()
                        .and_then(|settings| settings.last_used_efforts.as_ref())
                        .expect("effective profile map");
                    assert_eq!(effective.get("p/model"), Some(&expected));
                    assert_eq!(effective.get("p/other"), Some(&Some("low".to_owned())));
                    assert!(
                        workspace.effort_preferences.has_failed_writes(),
                        "without a connected client the choice is retryable, never reported saved"
                    );
                }
                let fresh_read = workspace.begin_effort_settings_read();
                let mut fresh = settings(&[("p/other", Some("high")), ("p/model", Some("low"))]);
                workspace.reconcile_effort_settings(&fresh_read, &mut fresh);
                let mut stale = settings(&[("p/other", Some("low")), ("p/model", Some("low"))]);
                workspace.reconcile_effort_settings(&old_read, &mut stale);
                assert_eq!(
                    stale
                        .last_used_efforts
                        .as_ref()
                        .expect("reconciled map")
                        .get("p/other"),
                    Some(&Some("high".to_owned())),
                    "an older read cannot undo a newer accepted profile snapshot"
                );
                workspace.send_catalog.settings = Some(stale);
                let captured = workspace.capture_user_effort(&ModelChoice {
                    provider_id: "p".to_owned(),
                    model_id: "model".to_owned(),
                    variant: Some("low".to_owned()),
                });
                workspace.choose_composer_effort(&owner, Effort::Default, cx);
                workspace.record_user_effort(captured, cx);
                assert_eq!(
                    workspace
                        .send_catalog
                        .settings
                        .as_ref()
                        .expect("settings")
                        .last_used_efforts
                        .as_ref()
                        .expect("map")
                        .get("p/model"),
                    Some(&None),
                    "a repeat Default supersedes the older user send"
                );
            });
        })
        .expect("the test workspace remains open");
    }
}
