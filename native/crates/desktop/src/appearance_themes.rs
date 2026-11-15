//! Runtime-owned custom themes. Reads publish only at their captured revision;
//! imports and deletes publish only after the host confirms the mutation.

use std::collections::{HashMap, HashSet};

use gpui_kit::{Context, Task};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

use super::{AppearancePreferences, AppearanceState, ThemeMode};
use crate::network::{Network, NetworkTask};
use crate::settings_controls::{ServerTarget, SettingsWrites, same_target};
use crate::theme::{self, Appearance, Palette, ThemeOption};

pub(crate) type ThemeOperation = oneshot::Receiver<Result<ThemeOption, String>>;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct ThemePreferences {
    mode: ThemeMode,
    light_theme_id: String,
    dark_theme_id: String,
}

/// Explicit user edits awaiting authoritative hydration or a confirmed save.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThemeEdits {
    mode: bool,
    light: bool,
    dark: bool,
}

impl ThemeEdits {
    fn include(&mut self, previous: &ThemePreferences, next: &ThemePreferences) {
        self.mode |= previous.mode != next.mode;
        self.light |= previous.light_theme_id != next.light_theme_id;
        self.dark |= previous.dark_theme_id != next.dark_theme_id;
    }
}

impl ThemePreferences {
    fn for_runtime(preferences: &AppearancePreferences, runtime: &str, first: bool) -> Self {
        preferences
            .runtime_themes
            .get(runtime)
            .cloned()
            .unwrap_or_else(|| {
                if first && preferences.runtime_themes.is_empty() {
                    Self::capture(preferences)
                } else {
                    Self::capture(&AppearancePreferences::default())
                }
            })
    }
    pub(super) fn capture(preferences: &AppearancePreferences) -> Self {
        Self {
            mode: preferences.mode,
            light_theme_id: preferences.light_theme_id.clone(),
            dark_theme_id: preferences.dark_theme_id.clone(),
        }
    }
    pub(super) fn valid(&self) -> bool {
        [&self.light_theme_id, &self.dark_theme_id]
            .iter()
            .all(|id| !id.trim().is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
    }
    fn apply(&self, preferences: &mut AppearancePreferences) {
        preferences.mode = self.mode;
        preferences.light_theme_id = self.light_theme_id.clone();
        preferences.dark_theme_id = self.dark_theme_id.clone();
    }
    fn wire(&self, system: Appearance) -> openchamber_native_transport::themes::ThemePreferences {
        let light = match self.mode {
            ThemeMode::Light => true,
            ThemeMode::Dark => false,
            ThemeMode::System => system == Appearance::Light,
        };
        openchamber_native_transport::themes::ThemePreferences {
            use_system_theme: Some(self.mode == ThemeMode::System),
            theme_variant: Some(if light { "light" } else { "dark" }.into()),
            theme_id: Some(
                if light {
                    &self.light_theme_id
                } else {
                    &self.dark_theme_id
                }
                .clone(),
            ),
            light_theme_id: Some(self.light_theme_id.clone()),
            dark_theme_id: Some(self.dark_theme_id.clone()),
        }
    }

    fn hydrate(
        &mut self,
        wire: openchamber_native_transport::themes::ThemePreferences,
        edits: &ThemeEdits,
    ) -> Result<(), ()> {
        if !edits.mode {
            if wire.use_system_theme == Some(true) {
                self.mode = ThemeMode::System;
            } else if wire.use_system_theme == Some(false) {
                match wire.theme_variant.as_deref() {
                    Some("light") => self.mode = ThemeMode::Light,
                    Some("dark") => self.mode = ThemeMode::Dark,
                    None => {}
                    _ => return Err(()),
                }
            }
        }
        if !edits.light
            && let Some(id) = wire.light_theme_id.filter(|id| !id.trim().is_empty())
        {
            self.light_theme_id = id.trim().into();
        }
        if !edits.dark
            && let Some(id) = wire.dark_theme_id.filter(|id| !id.trim().is_empty())
        {
            self.dark_theme_id = id.trim().into();
        }
        self.valid().then_some(()).ok_or(())
    }
}

#[derive(Default)]
pub(super) struct ThemeLibrary {
    target: Option<ServerTarget>,
    runtime: Option<String>,
    generation: u64,
    request: u64,
    pub(super) revision: u64,
    custom: Vec<ThemeOption>,
    unsupported: HashSet<String>,
    loading: bool,
    error: Option<&'static str>,
    read: Option<Task<()>>,
    pub(super) writes: SettingsWrites,
    pub(super) network: Option<Network>,
    intent: u64,
    preferences_read: Option<Task<()>>,
    preferences_hydrated: bool,
    preferences_error: bool,
    failed_preferences: HashSet<String>,
    preference_revisions: HashMap<String, u64>,
}

impl AppearanceState {
    /// Called when the workspace's authenticated runtime changes. A same-host
    /// reconnect keeps the last library while the new read is pending.
    pub(crate) fn set_theme_runtime(
        &mut self,
        target: Option<ServerTarget>,
        cx: &mut Context<Self>,
    ) {
        if same_target(self.themes.target.as_ref(), target.as_ref()) {
            return;
        }
        let next_runtime = target
            .as_ref()
            .map(|target| target.runtime.as_str().to_owned());
        let changed_runtime = next_runtime
            .as_ref()
            .is_some_and(|runtime| self.themes.runtime.as_ref() != Some(runtime));
        self.themes.generation += 1;
        self.themes.request += 1;
        self.themes.read = None;
        self.themes.preferences_read = None;
        self.themes.preferences_hydrated = false;
        self.themes.loading = false;
        self.themes.target = target;
        if changed_runtime {
            let preferences = ThemePreferences::for_runtime(
                &self.preferences,
                next_runtime.as_deref().expect("changed runtime"),
                self.themes.runtime.is_none(),
            );
            self.themes.runtime = next_runtime;
            self.themes.custom.clear();
            self.themes.unsupported.clear();
            self.themes.error = None;
            self.themes.preferences_error = false;
            self.themes.revision += 1;
            // Seeding is local migration only; it grants no server write intent.
            let _ = self.adopt_theme_preferences(preferences, cx);
            self.apply(cx);
        }
        self.reload_theme_preferences(cx);
        self.reload_custom_themes(cx);
        cx.notify();
    }

    pub(crate) fn theme_target(&self) -> Option<ServerTarget> {
        self.themes.target.clone()
    }
    pub(crate) fn theme_generation(&self) -> u64 {
        self.themes.generation
    }
    pub(crate) fn theme_revision(&self) -> u64 {
        self.themes.revision
    }
    pub(crate) fn custom_themes_loading(&self) -> bool {
        self.themes.loading
    }
    pub(crate) fn theme_error(&self) -> Option<&'static str> {
        if self
            .themes
            .runtime
            .as_ref()
            .is_some_and(|runtime| self.themes.failed_preferences.contains(runtime))
        {
            Some("settings.common.status.saveFailed")
        } else if self.themes.preferences_error {
            Some("settings.appearance.native.error.themeLoad")
        } else {
            self.themes.error
        }
    }
    pub(crate) fn is_custom_theme(&self, id: &str) -> bool {
        self.themes.custom.iter().any(|theme| theme.id == id)
    }

    pub(crate) fn available_themes(&self) -> Vec<ThemeOption> {
        let mut themes = self.themes.custom.clone();
        let custom: HashSet<_> = themes.iter().map(|theme| theme.id.clone()).collect();
        themes.extend(
            [Appearance::Light, Appearance::Dark]
                .into_iter()
                .flat_map(theme::options)
                .filter(|theme| !custom.contains(&theme.id)),
        );
        themes.sort_by(|a, b| a.name.cmp(&b.name));
        themes
    }

    pub(super) fn effective_palette(&self) -> Option<Palette> {
        let id = self.preferences.theme_id(self.system);
        let variant = match self.preferences.mode {
            ThemeMode::Light => Appearance::Light,
            ThemeMode::Dark => Appearance::Dark,
            ThemeMode::System => self.system,
        };
        if self.themes.unsupported.contains(id) {
            return None;
        }
        self.themes
            .custom
            .iter()
            .find(|theme| theme.id == id && theme.palette.appearance == variant)
            .map(|theme| theme.palette.clone())
            .or_else(|| Palette::by_id(id).filter(|palette| palette.appearance == variant))
            .or_else(|| {
                self.themes
                    .custom
                    .iter()
                    .find(|theme| theme.palette.appearance == variant)
                    .map(|theme| theme.palette.clone())
                    .or_else(|| Some(Palette::default_for(variant)))
            })
    }

    pub(crate) fn reload_custom_themes(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.themes.target.clone() else {
            return;
        };
        self.themes.request += 1;
        let request = self.themes.request;
        let generation = self.themes.generation;
        self.themes.loading = true;
        let work = NetworkTask::spawn_on(&target.network, async move {
            target.client.custom_themes().await.map(|definitions| {
                definitions
                    .into_iter()
                    .map(|definition| {
                        let id = definition
                            .pointer("/metadata/id")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        theme::from_definition(&definition).map_err(|code| (id, code))
                    })
                    .collect::<Vec<_>>()
            })
        });
        self.themes.read = Some(cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if this.themes.generation != generation || this.themes.request != request {
                    return;
                }
                this.themes.loading = false;
                this.themes.read = None;
                if let Some(Ok(definitions)) = result {
                    let mut themes = Vec::new();
                    let mut unsupported = HashSet::new();
                    for definition in definitions {
                        match definition {
                            Ok(theme)
                                if !themes
                                    .iter()
                                    .any(|other: &ThemeOption| other.id == theme.id) =>
                            {
                                themes.push(theme)
                            }
                            Err((Some(id), code)) if code == "unsupported" => {
                                unsupported.insert(id.clone());
                                if let Some(prior) =
                                    this.themes.custom.iter().find(|theme| theme.id == id)
                                {
                                    themes.push(prior.clone());
                                }
                            }
                            _ => {}
                        }
                    }
                    this.themes.error = (!unsupported.is_empty())
                        .then_some("settings.appearance.native.error.themeColor");
                    this.themes.custom = themes;
                    this.themes.unsupported = unsupported;
                    this.themes.revision += 1;
                    this.apply(cx);
                } else {
                    this.themes.error = Some("settings.appearance.native.error.themeLoad");
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn reload_theme_preferences(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.themes.target.clone() else {
            return;
        };
        let generation = self.themes.generation;
        self.themes.preferences_hydrated = false;
        let captured_edits = self
            .preferences
            .runtime_theme_pending
            .get(target.runtime.as_str())
            .cloned()
            .unwrap_or_default();
        let work = NetworkTask::spawn_on(&target.network, async move {
            target.client.theme_preferences().await
        });
        self.themes.preferences_read = Some(cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if this.themes.generation != generation {
                    return;
                }
                this.finish_theme_preferences_read(result.and_then(Result::ok), captured_edits, cx);
            });
        }));
    }

    fn finish_theme_preferences_read(
        &mut self,
        wire: Option<openchamber_native_transport::themes::ThemePreferences>,
        captured_edits: ThemeEdits,
        cx: &mut Context<Self>,
    ) {
        self.themes.preferences_read = None;
        self.themes.preferences_error = false;
        if let Some(wire) = wire {
            let mut next = ThemePreferences::capture(&self.preferences);
            let mut edits = self
                .themes
                .runtime
                .as_ref()
                .and_then(|runtime| self.preferences.runtime_theme_pending.get(runtime))
                .cloned()
                .unwrap_or_default();
            edits.mode |= captured_edits.mode;
            edits.light |= captured_edits.light;
            edits.dark |= captured_edits.dark;
            self.themes.preferences_error = next.hydrate(wire, &edits).is_err()
                || self.adopt_theme_preferences(next, cx).is_err();
        } else {
            self.themes.preferences_error = true;
        }
        self.themes.preferences_hydrated = !self.themes.preferences_error;
        if let Some(runtime) = &self.themes.runtime
            && self.preferences.runtime_theme_pending.contains_key(runtime)
        {
            if self.themes.preferences_hydrated {
                self.write_theme_preferences(cx);
            } else {
                self.themes.failed_preferences.insert(runtime.clone());
            }
        }
        cx.notify();
    }

    fn adopt_theme_preferences(
        &mut self,
        preferences: ThemePreferences,
        cx: &mut Context<Self>,
    ) -> Result<(), crate::appearance_storage::AppearanceError> {
        if self.baseline.is_ok() || self.dirty {
            return self.apply_change(|current| preferences.apply(current), cx);
        }
        // Automatic migration and host hydration have no local write authority
        // after a failed read. Genuine edits still use the dirty/save path.
        let mut next = self.preferences.clone();
        preferences.apply(&mut next);
        self.remember_theme_preferences(&mut next);
        next.validate()?;
        self.preferences = next;
        self.apply(cx);
        cx.notify();
        Ok(())
    }

    pub(super) fn remember_theme_preferences(&mut self, next: &mut AppearancePreferences) {
        if let Some(runtime) = &self.themes.runtime {
            let preferences = ThemePreferences::capture(next);
            next.runtime_themes.insert(runtime.clone(), preferences);
        }
    }

    pub(super) fn restore_runtime_theme_preferences(&mut self) {
        if let Some(preferences) = self
            .themes
            .runtime
            .as_ref()
            .and_then(|runtime| self.preferences.runtime_themes.get(runtime))
            .cloned()
        {
            preferences.apply(&mut self.preferences);
        }
    }

    pub(super) fn publish_theme_preferences(
        &mut self,
        previous: ThemePreferences,
        cx: &mut Context<Self>,
    ) {
        let next = ThemePreferences::capture(&self.preferences);
        if next == previous {
            return;
        }
        let Some(runtime) = self.themes.runtime.clone() else {
            return;
        };
        *self
            .themes
            .preference_revisions
            .entry(runtime.clone())
            .or_default() += 1;
        self.preferences
            .runtime_theme_pending
            .entry(runtime)
            .or_default()
            .include(&previous, &next);
        self.dirty = true;
        self.start_save(false, cx);
        self.write_theme_preferences(cx);
    }

    fn write_theme_preferences(&mut self, cx: &mut Context<Self>) {
        let Some(runtime) = self.themes.runtime.clone() else {
            return;
        };
        // A local seed is continuity, not authority to overwrite an unread host.
        if !self.themes.preferences_hydrated {
            if self.themes.preferences_read.is_none() {
                self.themes.failed_preferences.insert(runtime);
            }
            cx.notify();
            return;
        }
        let Some(target) = self.theme_write_target() else {
            self.themes.failed_preferences.insert(runtime);
            cx.notify();
            return;
        };
        let revision = self
            .themes
            .preference_revisions
            .entry(runtime.clone())
            .or_default();
        *revision += 1;
        let revision = *revision;
        let next = ThemePreferences::capture(&self.preferences);
        let mut wire = next.wire(self.system);
        if let Some(palette) = self.effective_palette() {
            wire.theme_id = Some(palette.id);
            wire.theme_variant = Some(
                if palette.appearance == Appearance::Light {
                    "light"
                } else {
                    "dark"
                }
                .into(),
            );
        }
        let client = target.client.clone();
        let (write, ready) = self.themes.writes.submit(&target, async move {
            client.save_theme_preferences(&wire).await
        });
        let task = cx.spawn(async move |this, cx| {
            let success = matches!(ready.await, Ok(Ok(_)));
            let _ = this.update(cx, |this, cx| {
                this.finish_preferences_write(&runtime, revision, success, cx);
                this.themes.writes.finish(write);
                cx.notify();
            });
        });
        self.themes.writes.observe(write, task);
    }

    fn finish_preferences_write(
        &mut self,
        runtime: &str,
        revision: u64,
        success: bool,
        cx: &mut Context<Self>,
    ) {
        if self.themes.preference_revisions.get(runtime) != Some(&revision) {
            return;
        }
        if success {
            self.preferences.runtime_theme_pending.remove(runtime);
            self.dirty = true;
            self.start_save(false, cx);
            self.themes.failed_preferences.remove(runtime);
        } else {
            self.themes.failed_preferences.insert(runtime.to_owned());
        }
    }

    pub(super) fn retry_theme_preferences(&mut self, cx: &mut Context<Self>) {
        if self
            .themes
            .runtime
            .as_ref()
            .is_some_and(|runtime| self.preferences.runtime_theme_pending.contains_key(runtime))
            && self.themes.preferences_hydrated
        {
            self.write_theme_preferences(cx);
        } else {
            self.reload_theme_preferences(cx);
        }
        self.reload_custom_themes(cx);
    }

    pub(super) fn theme_close_ready(&self) -> bool {
        self.themes.failed_preferences.is_empty()
            && self.preferences.runtime_theme_pending.is_empty()
    }

    /// Accepted writes have their own executor, retained by the appearance
    /// owner through dialog dismissal, runtime changes and the close barrier.
    fn theme_write_target(&mut self) -> Option<ServerTarget> {
        let mut target = self.themes.target.clone()?;
        if self.themes.network.is_none() {
            self.themes.network = Network::new().ok();
        }
        target.network = self.themes.network.as_ref()?.handle();
        Some(target)
    }

    pub(crate) fn import_theme(
        &mut self,
        definition: Value,
        activate: bool,
        cx: &mut Context<Self>,
    ) -> ThemeOperation {
        let (send, receive) = oneshot::channel();
        let Some(target) = self.theme_write_target().filter(|_| !self.closing) else {
            let _ = send.send(Err("connection".into()));
            return receive;
        };
        self.themes.intent += 1;
        let intent = self.themes.intent;
        let generation = self.themes.generation;
        let preferences = ThemePreferences::capture(&self.preferences);
        let client = target.client.clone();
        let (write, ready) =
            self.themes.writes.submit(
                &target,
                async move { client.import_theme(&definition).await },
            );
        let task = cx.spawn(async move |this, cx| {
            let result = ready.await;
            let _ = this.update(cx, |this, cx| {
                let result = if generation != this.themes.generation {
                    Err("connection".into())
                } else {
                    match result {
                        Ok(Ok(definition)) => theme::from_definition(&definition),
                        Ok(Err(error)) => Err(error.code().to_owned()),
                        Err(_) => Err("save".into()),
                    }
                };
                if let Ok(theme) = &result {
                    this.themes.request += 1;
                    this.themes.read = None;
                    this.themes.loading = false;
                    this.themes.custom.retain(|item| item.id != theme.id);
                    this.themes.custom.push(theme.clone());
                    this.themes.unsupported.remove(&theme.id);
                    this.themes.revision += 1;
                    if activate
                        && intent == this.themes.intent
                        && preferences == ThemePreferences::capture(&this.preferences)
                    {
                        let id = theme.id.clone();
                        let variant = theme.palette.appearance;
                        let _ = this.apply_change(
                            |p| match variant {
                                Appearance::Light => {
                                    p.mode = ThemeMode::Light;
                                    p.light_theme_id = id;
                                }
                                Appearance::Dark => {
                                    p.mode = ThemeMode::Dark;
                                    p.dark_theme_id = id;
                                }
                            },
                            cx,
                        );
                        this.publish_theme_preferences(preferences, cx);
                    }
                }
                this.themes.writes.finish(write);
                let _ = send.send(result);
                cx.notify();
            });
        });
        self.themes.writes.observe(write, task);
        receive
    }

    pub(crate) fn delete_theme(
        &mut self,
        id: &str,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<(), String>> {
        let (send, receive) = oneshot::channel();
        if !self.is_custom_theme(id) || self.closing {
            let _ = send.send(Err("delete".into()));
            return receive;
        }
        let Some(target) = self.theme_write_target() else {
            let _ = send.send(Err("connection".into()));
            return receive;
        };
        let generation = self.themes.generation;
        let id = id.to_owned();
        let deleting = id.clone();
        let client = target.client.clone();
        let (write, ready) = self
            .themes
            .writes
            .submit(&target, async move { client.delete_theme(&deleting).await });
        let task = cx.spawn(async move |this, cx| {
            let result = ready.await;
            let _ = this.update(cx, |this, cx| {
                let result = if generation != this.themes.generation {
                    Err("connection".into())
                } else if matches!(result, Ok(Ok(()))) {
                    Ok(())
                } else {
                    Err("delete".into())
                };
                if result.is_ok() {
                    this.themes.request += 1;
                    this.themes.read = None;
                    this.themes.loading = false;
                    this.themes.custom.retain(|theme| theme.id != id);
                    this.themes.unsupported.remove(&id);
                    this.themes.revision += 1;
                    let defaults = AppearancePreferences::default();
                    let previous = ThemePreferences::capture(&this.preferences);
                    let _ = this.apply_change(
                        |p| {
                            if p.light_theme_id == id {
                                p.light_theme_id = defaults.light_theme_id;
                            }
                            if p.dark_theme_id == id {
                                p.dark_theme_id = defaults.dark_theme_id;
                            }
                        },
                        cx,
                    );
                    this.apply(cx);
                    this.publish_theme_preferences(previous, cx);
                }
                this.themes.writes.finish(write);
                let _ = send.send(result);
                cx.notify();
            });
        });
        self.themes.writes.observe(write, task);
        receive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui_kit::test]
    fn connecting_after_failed_local_read_does_not_dirty_an_untouched_app(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::AppContext;
        use openchamber_native_transport::{
            client::ServerClient, credential::Credential, endpoint::Endpoint,
        };
        let network = Network::new().expect("network");
        for failure in [
            crate::appearance_storage::AppearanceError::Invalid,
            crate::appearance_storage::AppearanceError::Unavailable,
        ] {
            let (directory, store) = super::super::tests::fixture();
            let owner = cx.update(|cx| {
                gpui_kit::init(cx);
                cx.new(|cx| {
                    AppearanceState::new(
                        store.clone(),
                        Err(failure),
                        crate::fonts::GenericFamilies::default(),
                        cx,
                    )
                })
            });
            owner.update(cx, |state, cx| {
                state.set_theme_runtime(
                    Some(ServerTarget {
                        runtime: "first-runtime".into(),
                        generation: 1,
                        client: ServerClient::new(
                            Endpoint::parse("http://127.0.0.1:1/").expect("endpoint"),
                            Credential::None,
                        )
                        .expect("client"),
                        network: network.handle(),
                    }),
                    cx,
                );
                assert!(
                    !state.dirty,
                    "migration after {failure:?} is not a user edit"
                );
                assert!(state.take_pending_write(cx).is_none());
                assert!(
                    state.finish_close(),
                    "untouched app can close after {failure:?}"
                );
                state.cancel_close();
                // Complete the same parsed-settings callback as a successful
                // GET, with host values that differ from the startup seed.
                state.finish_theme_preferences_read(
                    Some(openchamber_native_transport::themes::ThemePreferences {
                        use_system_theme: Some(false),
                        theme_variant: Some("dark".into()),
                        theme_id: Some("host-dark".into()),
                        light_theme_id: Some("host-light".into()),
                        dark_theme_id: Some("host-dark".into()),
                    }),
                    ThemeEdits::default(),
                    cx,
                );
                assert_eq!(state.preferences.mode, ThemeMode::Dark);
                assert_eq!(state.preferences.dark_theme_id, "host-dark");
                assert!(state.themes.preferences_hydrated);
                assert!(
                    !state.dirty,
                    "passive hydration has no local writer after {failure:?}"
                );
                assert!(state.take_pending_write(cx).is_none());
                assert!(
                    state.finish_close(),
                    "successful host hydration must not trap an untouched app"
                );
                state.cancel_close();
                state.set_locale("ar", cx).expect("locale");
                assert!(state.dirty, "genuine user edits still need persistence");
                assert!(!state.finish_close());
            });
            assert_eq!(
                store.load(),
                Ok(None),
                "connection never obtained local write authority"
            );
            std::fs::remove_dir_all(directory).expect("remove fixture");
        }
    }

    #[gpui_kit::test]
    fn older_equal_value_success_cannot_acknowledge_a_newer_failed_choice(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::AppContext;
        let (directory, store) = super::super::tests::fixture();
        let owner = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.new(|cx| {
                AppearanceState::new(
                    store.clone(),
                    Ok(None),
                    crate::fonts::GenericFamilies::default(),
                    cx,
                )
            })
        });
        owner.update(cx, |state, cx| {
            state.themes.runtime = Some("A".into());
            state.set_mode(ThemeMode::Dark, cx).expect("theme mode");
            let dark_first = state.themes.preference_revisions["A"];
            state.set_mode(ThemeMode::Light, cx).expect("theme mode");
            let light = state.themes.preference_revisions["A"];
            state.set_mode(ThemeMode::Dark, cx).expect("theme mode");
            let dark_last = state.themes.preference_revisions["A"];
            state.finish_preferences_write("A", dark_first, true, cx);
            assert!(state.preferences.runtime_theme_pending.contains_key("A"));
            state.finish_preferences_write("A", light, true, cx);
            state.finish_preferences_write("A", dark_last, false, cx);
            assert!(!state.finish_close());
            state.retry_theme_preferences(cx);
            let mut retry = ThemePreferences::capture(&state.preferences);
            retry
                .hydrate(
                    openchamber_native_transport::themes::ThemePreferences {
                        use_system_theme: Some(false),
                        theme_variant: Some("light".into()),
                        ..Default::default()
                    },
                    &state.preferences.runtime_theme_pending["A"],
                )
                .expect("test fixture");
            assert_eq!(
                retry.mode,
                ThemeMode::Dark,
                "retry retains the latest explicit choice over the server's Light"
            );
            state.finish_preferences_write("A", dark_last, true, cx);
            assert!(state.preferences.runtime_theme_pending.is_empty());
            assert!(state.themes.failed_preferences.is_empty());
        });
        cx.run_until_parked();
        assert_eq!(
            store
                .load()
                .expect("load preferences")
                .expect("preferences")
                .preferences
                .mode,
            ThemeMode::Dark
        );
        std::fs::remove_dir_all(directory).expect("remove fixture");
    }

    #[test]
    fn legacy_seed_is_retained_once_and_scoped_selections_roundtrip() {
        let mut legacy = AppearancePreferences {
            mode: ThemeMode::Dark,
            light_theme_id: "import-a-light".into(),
            dark_theme_id: "import-a-dark".into(),
            ..AppearancePreferences::default()
        };
        let first = ThemePreferences::for_runtime(&legacy, "A", true);
        assert_eq!(first, ThemePreferences::capture(&legacy));
        legacy.runtime_themes.insert("A".into(), first.clone());
        assert_eq!(ThemePreferences::for_runtime(&legacy, "A", false), first);
        assert_eq!(
            ThemePreferences::for_runtime(&legacy, "B", false),
            ThemePreferences::capture(&AppearancePreferences::default())
        );
        assert_eq!(
            ThemePreferences::for_runtime(&legacy, "B", true),
            ThemePreferences::capture(&AppearancePreferences::default())
        );
        legacy.runtime_theme_pending.insert(
            "A".into(),
            ThemeEdits {
                mode: true,
                ..ThemeEdits::default()
            },
        );
        let bytes = serde_json::to_vec(&legacy).expect("serialize preferences");
        let restored: AppearancePreferences =
            serde_json::from_slice(&bytes).expect("restore preferences");
        assert_eq!(restored, legacy);
        assert!(restored.validate().is_ok());
    }

    #[test]
    fn mode_edit_before_hydration_keeps_both_host_selections() {
        let previous = ThemePreferences::capture(&AppearancePreferences::default());
        let mut next = previous.clone();
        next.mode = ThemeMode::Dark;
        let mut edits = ThemeEdits::default();
        edits.include(&previous, &next);
        next.hydrate(
            openchamber_native_transport::themes::ThemePreferences {
                use_system_theme: Some(true),
                theme_variant: Some("light".into()),
                theme_id: Some("host-light".into()),
                light_theme_id: Some("host-light".into()),
                dark_theme_id: Some("host-dark".into()),
            },
            &edits,
        )
        .expect("test fixture");
        assert_eq!(next.mode, ThemeMode::Dark);
        assert_eq!(next.light_theme_id, "host-light");
        assert_eq!(next.dark_theme_id, "host-dark");
        let wire =
            serde_json::to_value(next.wire(Appearance::Light)).expect("serialize preferences");
        assert_eq!(
            wire,
            serde_json::json!({"useSystemTheme":false,"themeVariant":"dark","themeId":"host-dark","lightThemeId":"host-light","darkThemeId":"host-dark"})
        );
    }

    #[test]
    fn theme_edit_during_hydration_only_overlays_its_own_field() {
        let previous = ThemePreferences::capture(&AppearancePreferences::default());
        let mut next = previous.clone();
        next.light_theme_id = "picked-light".into();
        let mut edits = ThemeEdits::default();
        edits.include(&previous, &next);
        next.hydrate(
            openchamber_native_transport::themes::ThemePreferences {
                use_system_theme: Some(false),
                theme_variant: Some("dark".into()),
                light_theme_id: Some("host-light".into()),
                dark_theme_id: Some("host-dark".into()),
                ..Default::default()
            },
            &edits,
        )
        .expect("test fixture");
        assert_eq!(next.mode, ThemeMode::Dark);
        assert_eq!(next.light_theme_id, "picked-light");
        assert_eq!(next.dark_theme_id, "host-dark");
    }

    #[gpui_kit::test]
    fn failed_remote_save_blocks_close_and_unfreezes_retry(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext;
        let (directory, store) = super::super::tests::fixture();
        let owner = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.new(|cx| {
                AppearanceState::new(
                    store.clone(),
                    Ok(None),
                    crate::fonts::GenericFamilies::default(),
                    cx,
                )
            })
        });
        owner.update(cx, |state, cx| {
            state.themes.runtime = Some("A".into());
            state.themes.failed_preferences.insert("A".into());
            assert!(state.take_pending_write(cx).is_none());
            assert!(
                !state.finish_close(),
                "a settled failed remote save still blocks close"
            );
            assert!(!state.closing, "retry must remain available");
            assert!(!state.themes.writes.closing());
            state.themes.failed_preferences.clear();
            state.preferences.runtime_theme_pending.insert(
                "A".into(),
                ThemeEdits {
                    mode: true,
                    ..Default::default()
                },
            );
            assert!(
                !state.finish_close(),
                "an unhydrated pending edit cannot be lost on close"
            );
            state.preferences.runtime_theme_pending.clear();
            assert!(state.finish_close());
        });
        std::fs::remove_dir_all(directory).expect("remove fixture");
    }
}
