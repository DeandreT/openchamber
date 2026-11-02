//! One live appearance owner. Views request changes without replacing retained inputs.

use gpui_kit::component::Theme;
use gpui_kit::{App, Context, Global, Subscription, Task, Window, WindowAppearance, px};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::appearance_storage::{AppearanceError, AppearanceSnapshot, AppearanceStore};
use crate::fonts::{self, GenericFamilies};
use crate::locale::{self, Strings};
use crate::theme::{self, Appearance, Palette};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct AppearancePreferences {
    pub(crate) mode: ThemeMode,
    pub(crate) light_theme_id: String,
    pub(crate) dark_theme_id: String,
    pub(crate) locale: String,
    pub(crate) ui_font: String,
    pub(crate) mono_font: String,
    pub(crate) font_size: u16,
}

impl Default for AppearancePreferences {
    fn default() -> Self {
        Self {
            mode: ThemeMode::System,
            light_theme_id: Palette::default_for(Appearance::Light).id,
            dark_theme_id: Palette::default_for(Appearance::Dark).id,
            locale: "en".to_owned(),
            ui_font: "system".to_owned(),
            mono_font: "system-mono".to_owned(),
            font_size: 100,
        }
    }
}

impl AppearancePreferences {
    /// Reapplies only local edits to a freshly read snapshot on explicit retry.
    fn merge_edits(&self, base: &Self, mut fresh: Self) -> Self {
        if self.mode != base.mode {
            fresh.mode = self.mode;
        }
        if self.light_theme_id != base.light_theme_id {
            fresh.light_theme_id = self.light_theme_id.clone();
        }
        if self.dark_theme_id != base.dark_theme_id {
            fresh.dark_theme_id = self.dark_theme_id.clone();
        }
        if self.locale != base.locale {
            fresh.locale = self.locale.clone();
        }
        if self.ui_font != base.ui_font {
            fresh.ui_font = self.ui_font.clone();
        }
        if self.mono_font != base.mono_font {
            fresh.mono_font = self.mono_font.clone();
        }
        if self.font_size != base.font_size {
            fresh.font_size = self.font_size;
        }
        fresh
    }

    pub(crate) fn validate(&self) -> Result<(), AppearanceError> {
        let correct_variant = |id: &str, variant| {
            Palette::by_id(id).is_some_and(|palette| palette.appearance == variant)
        };
        if !correct_variant(&self.light_theme_id, Appearance::Light)
            || !correct_variant(&self.dark_theme_id, Appearance::Dark)
            || !locale::supported_locales().contains(&self.locale)
            || !fonts::valid_ui_font(&self.ui_font)
            || !fonts::valid_mono_font(&self.mono_font)
            || !(50..=200).contains(&self.font_size)
        {
            return Err(AppearanceError::Invalid);
        }
        Ok(())
    }

    fn theme_id(&self, system: Appearance) -> &str {
        let appearance = match self.mode {
            ThemeMode::System => system,
            ThemeMode::Light => Appearance::Light,
            ThemeMode::Dark => Appearance::Dark,
        };
        match appearance {
            Appearance::Light => &self.light_theme_id,
            Appearance::Dark => &self.dark_theme_id,
        }
    }

    fn initial_overrides(&mut self) {
        if let Ok(id) = std::env::var("OPENCHAMBER_NATIVE_THEME")
            && let Some(palette) = Palette::by_id(&id)
        {
            match palette.appearance {
                Appearance::Light => {
                    self.light_theme_id = id;
                    self.mode = ThemeMode::Light;
                }
                Appearance::Dark => {
                    self.dark_theme_id = id;
                    self.mode = ThemeMode::Dark;
                }
            }
        }
        if let Ok(locale) = std::env::var("OPENCHAMBER_NATIVE_LOCALE") {
            self.locale = locale::normalize(&locale);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SaveStatus {
    Saved,
    Saving,
    Failed(AppearanceError),
}

#[derive(Clone)]
enum WriteOutcome {
    Loaded(Result<Option<AppearanceSnapshot>, AppearanceError>),
    Written {
        preferences: AppearancePreferences,
        result: Result<AppearanceSnapshot, AppearanceError>,
    },
}

struct TextScale(u16);
impl Global for TextScale {}

pub(crate) fn root_font_size(cx: &App) -> f32 {
    16.0 * f32::from(cx.try_global::<TextScale>().map_or(100, |scale| scale.0)) / 100.0
}

pub(crate) struct AppearanceState {
    preferences: AppearancePreferences,
    edit_base: AppearancePreferences,
    store: AppearanceStore,
    baseline: Result<Option<AppearanceSnapshot>, AppearanceError>,
    generic_fonts: GenericFamilies,
    system: Appearance,
    subscription: Option<Subscription>,
    task: Option<Task<()>>,
    pending_io: Option<Task<WriteOutcome>>,
    dirty: bool,
    saving: bool,
    closing: bool,
    error: Option<AppearanceError>,
}

impl AppearanceState {
    /// `loaded` must be read off the UI thread before creating the workspace.
    pub(crate) fn new(
        store: AppearanceStore,
        loaded: Result<Option<AppearanceSnapshot>, AppearanceError>,
        generic_fonts: GenericFamilies,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut preferences = loaded
            .as_ref()
            .ok()
            .and_then(|snapshot| snapshot.as_ref())
            .map(|snapshot| snapshot.preferences.clone())
            .unwrap_or_else(|| AppearancePreferences {
                locale: locale::requested_locale(),
                ..AppearancePreferences::default()
            });
        preferences.initial_overrides();
        let state = Self {
            edit_base: preferences.clone(),
            preferences,
            store,
            error: loaded.as_ref().err().copied(),
            baseline: loaded,
            generic_fonts,
            system: system_appearance(cx.window_appearance()),
            subscription: None,
            task: None,
            pending_io: None,
            dirty: false,
            saving: false,
            closing: false,
        };
        state.apply(cx);
        state
    }

    /// Retains the system-appearance observer for this window's lifetime.
    pub(crate) fn attach_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.set_rem_size(px(root_font_size(cx)));
        self.system = system_appearance(window.appearance());
        self.apply(cx);
        self.subscription = Some(cx.observe_window_appearance(window, |this, window, cx| {
            let appearance = system_appearance(window.appearance());
            if this.system == appearance {
                return;
            }
            this.system = appearance;
            if this.preferences.mode == ThemeMode::System {
                this.apply(cx);
                cx.notify();
            }
        }));
    }

    pub(crate) fn preferences(&self) -> &AppearancePreferences {
        &self.preferences
    }

    pub(crate) fn save_status(&self) -> SaveStatus {
        if let Some(error) = self.error {
            SaveStatus::Failed(error)
        } else if self.saving || self.dirty {
            SaveStatus::Saving
        } else {
            SaveStatus::Saved
        }
    }

    fn change(
        &mut self,
        edit: impl FnOnce(&mut AppearancePreferences),
        cx: &mut Context<Self>,
    ) -> Result<(), AppearanceError> {
        if self.closing {
            return Err(AppearanceError::Locked);
        }
        let mut next = self.preferences.clone();
        edit(&mut next);
        next.validate()?;
        if next == self.preferences {
            return Ok(());
        }
        self.preferences = next;
        self.dirty = true;
        self.apply(cx);
        self.start_save(false, cx);
        cx.notify();
        Ok(())
    }

    pub(crate) fn set_mode(
        &mut self,
        mode: ThemeMode,
        cx: &mut Context<Self>,
    ) -> Result<(), AppearanceError> {
        self.change(|p| p.mode = mode, cx)
    }
    pub(crate) fn set_light_theme(
        &mut self,
        id: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), AppearanceError> {
        self.change(|p| p.light_theme_id = id.to_owned(), cx)
    }
    pub(crate) fn set_dark_theme(
        &mut self,
        id: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), AppearanceError> {
        self.change(|p| p.dark_theme_id = id.to_owned(), cx)
    }
    pub(crate) fn set_locale(
        &mut self,
        locale: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), AppearanceError> {
        self.change(|p| p.locale = locale.to_owned(), cx)
    }
    pub(crate) fn set_ui_font(
        &mut self,
        id: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), AppearanceError> {
        self.change(|p| p.ui_font = id.to_owned(), cx)
    }
    pub(crate) fn set_mono_font(
        &mut self,
        id: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), AppearanceError> {
        self.change(|p| p.mono_font = id.to_owned(), cx)
    }
    pub(crate) fn set_font_size(
        &mut self,
        percent: u16,
        cx: &mut Context<Self>,
    ) -> Result<(), AppearanceError> {
        self.change(|p| p.font_size = percent, cx)
    }

    fn apply(&self, cx: &mut Context<Self>) {
        let palette = Palette::by_id(self.preferences.theme_id(self.system))
            .expect("validated theme IDs resolve");
        theme::apply(palette, cx);
        fonts::apply_preferences(
            &self.generic_fonts,
            &self.preferences.ui_font,
            &self.preferences.mono_font,
            cx,
        );
        if cx
            .try_global::<Strings>()
            .is_none_or(|strings| strings.locale() != self.preferences.locale)
        {
            cx.set_global(Strings::load(&self.preferences.locale));
        }
        cx.set_global(TextScale(self.preferences.font_size));
        // Kit's root installs this metric before every frame's layout.
        let size = px(root_font_size(cx));
        Theme::update(cx, |theme| theme.font_size = size);
        cx.defer(|cx| cx.refresh_windows());
    }

    /// Starts one ordered writer; subsequent edits coalesce behind its current write.
    pub(crate) fn save(&mut self, cx: &mut Context<Self>) {
        self.start_save(true, cx);
    }

    fn start_save(&mut self, recover_read: bool, cx: &mut Context<Self>) {
        if self.saving || (!self.dirty && self.baseline.is_ok()) {
            return;
        }
        if !recover_read && let Err(error) = &self.baseline {
            self.error = Some(*error);
            cx.notify();
            return;
        }
        let reread = recover_read && (self.baseline.is_err() || self.error.is_some());
        self.error = None;
        self.saving = true;
        let ready = self.start_io(reread, cx);
        self.task = Some(cx.spawn(async move |this, cx| {
            let mut ready = Some(ready);
            while let Some(receipt) = ready.take() {
                let outcome = receipt
                    .await
                    .unwrap_or(WriteOutcome::Loaded(Err(AppearanceError::Unavailable)));
                ready = this
                    .update(cx, |this, cx| this.finish_io(outcome, cx))
                    .ok()
                    .flatten();
            }
        }));
        cx.notify();
    }

    /// Disk work starts before a foreground callback, so quit can await it directly.
    fn start_io(&mut self, reread: bool, cx: &Context<Self>) -> oneshot::Receiver<WriteOutcome> {
        let store = self.store.clone();
        let baseline = self.baseline.clone();
        let preferences = self.preferences.clone();
        let (completed, receipt) = oneshot::channel();
        self.pending_io = Some(cx.background_executor().spawn(async move {
            let outcome = if reread {
                WriteOutcome::Loaded(store.load())
            } else {
                let result = match baseline {
                    Ok(expected) => store.save(expected.as_ref(), &preferences),
                    Err(error) => Err(error),
                };
                WriteOutcome::Written {
                    preferences,
                    result,
                }
            };
            let _ = completed.send(outcome.clone());
            outcome
        }));
        receipt
    }

    fn finish_io(
        &mut self,
        outcome: WriteOutcome,
        cx: &mut Context<Self>,
    ) -> Option<oneshot::Receiver<WriteOutcome>> {
        self.pending_io = None;
        match outcome {
            WriteOutcome::Loaded(Ok(snapshot)) => {
                let fresh = snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.preferences.clone())
                    .unwrap_or_else(|| self.edit_base.clone());
                let merged = if self.dirty {
                    self.preferences.merge_edits(&self.edit_base, fresh.clone())
                } else {
                    fresh.clone()
                };
                self.dirty = merged != fresh;
                self.edit_base = fresh;
                self.preferences = merged;
                self.baseline = Ok(snapshot);
                self.apply(cx);
                if self.dirty {
                    return Some(self.start_io(false, cx));
                }
                self.error = None;
            }
            WriteOutcome::Written {
                preferences,
                result: Ok(saved),
            } => {
                self.baseline = Ok(Some(saved));
                self.edit_base = preferences.clone();
                self.dirty = self.preferences != preferences;
                if self.dirty {
                    return Some(self.start_io(false, cx));
                }
                self.error = None;
            }
            WriteOutcome::Loaded(Err(error))
            | WriteOutcome::Written {
                result: Err(error), ..
            } => self.error = Some(error),
        }
        self.saving = false;
        self.task = None;
        cx.notify();
        None
    }

    /// Quit blocks GPUI's foreground session. Drain disk work without any entity updates.
    pub(crate) fn take_quit_write(&mut self, cx: &Context<Self>) -> Option<Task<()>> {
        self.closing = true;
        self.task = None;
        let pending = self.pending_io.take()?;
        let store = self.store.clone();
        let preferences = self.preferences.clone();
        let edit_base = self.edit_base.clone();
        let dirty = self.dirty;
        Some(cx.background_executor().spawn(async move {
            let (expected, next) = match pending.await {
                WriteOutcome::Loaded(Ok(snapshot)) => {
                    let fresh = snapshot
                        .as_ref()
                        .map(|snapshot| snapshot.preferences.clone())
                        .unwrap_or_else(|| edit_base.clone());
                    let merged = if dirty {
                        preferences.merge_edits(&edit_base, fresh.clone())
                    } else {
                        fresh.clone()
                    };
                    if merged == fresh {
                        return;
                    }
                    (snapshot, merged)
                }
                WriteOutcome::Written {
                    preferences: written,
                    result: Ok(saved),
                } => {
                    if preferences == written {
                        return;
                    }
                    (Some(saved), preferences)
                }
                // Failed writes still require explicit retry, including during quit.
                WriteOutcome::Loaded(Err(_)) | WriteOutcome::Written { result: Err(_), .. } => {
                    return;
                }
            };
            let _ = store.save(expected.as_ref(), &next);
        }))
    }

    /// Freezes changes before the parent awaits the final write on normal close.
    pub(crate) fn take_pending_write(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        self.closing = true;
        // Failures require an explicit retry; closing must not silently resubmit.
        if self.error.is_none() {
            self.start_save(false, cx);
        }
        self.task.take()
    }

    /// A failed save keeps the window and all live edits available for retry.
    pub(crate) fn finish_close(&mut self) -> bool {
        let ready = !self.saving && !self.dirty;
        if !ready {
            self.closing = false;
        }
        ready
    }

    /// Resumes appearance edits if a later host or draft-journal close barrier fails.
    pub(crate) fn cancel_close(&mut self) {
        self.closing = false;
    }
}

fn system_appearance(appearance: WindowAppearance) -> Appearance {
    match appearance {
        WindowAppearance::Dark | WindowAppearance::VibrantDark => Appearance::Dark,
        WindowAppearance::Light | WindowAppearance::VibrantLight => Appearance::Light,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (std::path::PathBuf, AppearanceStore) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = std::path::PathBuf::from("/tmp/opencode")
            .join(format!("appearance-owner-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("owned fixture directory");
        let store = AppearanceStore::new(&directory);
        (directory, store)
    }

    enum QuitPath {
        Application,
        ClosingWindow,
    }

    fn quit_with_pending_writes(cx: &mut gpui_kit::TestAppContext, path: QuitPath) {
        use gpui_kit::AppContext as _;
        let (directory, store) = fixture();
        let initial = store
            .save(None, &AppearancePreferences::default())
            .expect("initial snapshot");
        let (window, workspace) = crate::workspace_tests::open(cx);
        let owner = cx.update(|cx| {
            let owner = cx.new(|cx| {
                AppearanceState::new(store.clone(), store.load(), GenericFamilies::default(), cx)
            });
            cx.set_global(crate::appearance_view::AppearanceOwner(owner.clone()));
            owner
        });
        owner.update(cx, |state, cx| {
            state.set_font_size(125, cx).expect("first accepted write");
            state.set_font_size(137, cx).expect("newer coalesced size");
            state.set_locale("ar", cx).expect("coalesced language");
            assert!(
                state.pending_io.is_some(),
                "an independent write is pending"
            );
        });
        if matches!(path, QuitPath::ClosingWindow) {
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| workspace.request_close(window, cx));
            })
            .expect("normal close starts before app quit");
        }
        assert_eq!(
            store.load(),
            Ok(Some(initial)),
            "the deterministic executor has not run the queued disk operation"
        );
        cx.quit();
        let saved = store.load().expect("read snapshot").expect("saved");
        assert_eq!(
            saved.preferences.font_size, 137,
            "quit returns only after the newest accepted size is durable"
        );
        assert_eq!(
            saved.preferences.locale, "ar",
            "quit drains coalesced fields"
        );
        std::fs::remove_dir_all(directory).expect("remove owned fixture");
    }

    #[gpui_kit::test]
    fn app_quit_drains_pending_and_coalesced_writes(cx: &mut gpui_kit::TestAppContext) {
        quit_with_pending_writes(cx, QuitPath::Application);
    }

    #[gpui_kit::test]
    fn app_quit_drains_writes_already_waited_by_normal_close(cx: &mut gpui_kit::TestAppContext) {
        quit_with_pending_writes(cx, QuitPath::ClosingWindow);
    }

    #[gpui_kit::test]
    fn app_quit_drains_pending_explicit_recovery(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        let (directory, store) = fixture();
        let foreign = AppearancePreferences {
            font_size: 110,
            ..AppearancePreferences::default()
        };
        store.save(None, &foreign).expect("repaired snapshot");
        let (_, _workspace) = crate::workspace_tests::open(cx);
        cx.update(|cx| {
            let owner = cx.new(|cx| {
                AppearanceState::new(
                    store.clone(),
                    Err(AppearanceError::Unavailable),
                    GenericFamilies::default(),
                    cx,
                )
            });
            cx.set_global(crate::appearance_view::AppearanceOwner(owner.clone()));
            owner.update(cx, |state, cx| {
                state.set_locale("ar", cx).expect("pending local edit");
                state.save(cx);
            });
        });
        cx.quit();
        let saved = store.load().expect("read snapshot").expect("saved");
        assert_eq!(
            saved.preferences.locale, "ar",
            "quit drains explicit recovery"
        );
        assert_eq!(
            saved.preferences.font_size, 110,
            "recovery still preserves another window's unedited field"
        );
        std::fs::remove_dir_all(directory).expect("remove owned fixture");
    }

    #[gpui_kit::test]
    fn app_quit_does_not_retry_a_failed_pending_write(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        let (directory, store) = fixture();
        let initial = store
            .save(None, &AppearancePreferences::default())
            .expect("initial snapshot");
        let (_, _workspace) = crate::workspace_tests::open(cx);
        cx.update(|cx| {
            let owner = cx.new(|cx| {
                AppearanceState::new(store.clone(), store.load(), GenericFamilies::default(), cx)
            });
            cx.set_global(crate::appearance_view::AppearanceOwner(owner.clone()));
            owner.update(cx, |state, cx| {
                state.set_font_size(125, cx).expect("first accepted write");
                state.set_font_size(137, cx).expect("newer coalesced size");
            });
        });
        let foreign = AppearancePreferences {
            font_size: 150,
            ..initial.preferences.clone()
        };
        let foreign = store.save(Some(&initial), &foreign).expect("foreign edit");
        cx.quit();
        assert_eq!(
            store.load(),
            Ok(Some(foreign)),
            "a pending CAS failure stops the drain without rebasing or retrying"
        );
        std::fs::remove_dir_all(directory).expect("remove owned fixture");
    }

    #[gpui_kit::test]
    fn explicit_conflict_retry_merges_only_local_edits_and_includes_new_pending_edits(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::AppContext as _;
        let (directory, store) = fixture();
        let initial = store
            .save(None, &AppearancePreferences::default())
            .expect("initial snapshot");
        let owner = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.new(|cx| {
                AppearanceState::new(store.clone(), store.load(), GenericFamilies::default(), cx)
            })
        });
        let foreign = AppearancePreferences {
            locale: "fr".to_owned(),
            mode: ThemeMode::Dark,
            font_size: 110,
            mono_font: "fira-code".to_owned(),
            ..initial.preferences.clone()
        };
        let other_saved = store
            .save(Some(&initial), &foreign)
            .expect("another window's edit");
        owner.update(cx, |state, cx| {
            state.set_locale("ar", cx).expect("local choice");
        });
        cx.run_until_parked();
        assert_eq!(
            store.load(),
            Ok(Some(other_saved)),
            "automatic writes cannot rebase conflicts"
        );
        owner.update(cx, |state, cx| {
            assert_eq!(
                state.save_status(),
                SaveStatus::Failed(AppearanceError::Changed)
            );
            assert_eq!(state.preferences().locale, "ar");
            assert!(state.take_pending_write(cx).is_none());
            assert!(
                !state.finish_close(),
                "unpersisted local choices block close"
            );
            state.save(cx);
            state
                .set_font_size(125, cx)
                .expect("new edit while retry reads");
        });
        cx.run_until_parked();
        let saved = store.load().expect("read snapshot").expect("saved");
        assert_eq!(
            saved.preferences.locale, "ar",
            "explicit retry retains a conflicting local choice"
        );
        assert_eq!(
            saved.preferences.font_size, 125,
            "edits made during retry survive"
        );
        assert_eq!(
            saved.preferences.mode,
            ThemeMode::Dark,
            "unmodified local fields take the other window's value"
        );
        assert_eq!(saved.preferences.mono_font, "fira-code");
        owner.update(cx, |state, _| {
            assert_eq!(
                state.preferences(),
                &saved.preferences,
                "merged choices apply live"
            );
            assert_eq!(state.save_status(), SaveStatus::Saved);
        });
        std::fs::remove_dir_all(directory).expect("remove owned fixture");
    }

    #[gpui_kit::test]
    fn clean_read_recovery_applies_snapshot_without_rewriting_it_and_can_cancel_close(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::AppContext as _;
        let (directory, store) = fixture();
        let preferences = AppearancePreferences {
            locale: "de".to_owned(),
            ..AppearancePreferences::default()
        };
        let saved = store.save(None, &preferences).expect("repaired snapshot");
        let owner = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.new(|cx| {
                AppearanceState::new(
                    store.clone(),
                    Err(AppearanceError::Unavailable),
                    GenericFamilies::default(),
                    cx,
                )
            })
        });
        owner.update(cx, |state, cx| state.save(cx));
        cx.run_until_parked();
        assert_eq!(
            store.load(),
            Ok(Some(saved)),
            "clean recovery does not rewrite the snapshot"
        );
        owner.update(cx, |state, cx| {
            assert_eq!(state.preferences().locale, "de");
            assert_eq!(locale::strings(cx).locale(), "de");
            assert_eq!(state.save_status(), SaveStatus::Saved);
            assert!(state.take_pending_write(cx).is_none());
            assert!(state.finish_close());
            assert_eq!(state.set_locale("de", cx), Err(AppearanceError::Locked));
            state.cancel_close();
            assert_eq!(
                state.set_locale("de", cx),
                Ok(()),
                "a later barrier failure releases the freeze"
            );
        });
        std::fs::remove_dir_all(directory).expect("remove owned fixture");
    }

    #[gpui_kit::test]
    fn repaired_read_preserves_pending_edits_and_close_drains_them(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::AppContext as _;
        let (directory, store) = fixture();
        std::fs::write(directory.join("appearance.json"), b"broken").expect("invalid fixture");
        let owner = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.new(|cx| {
                AppearanceState::new(store.clone(), store.load(), GenericFamilies::default(), cx)
            })
        });
        owner.update(cx, |state, cx| {
            state.set_locale("ar", cx).expect("live edit");
        });
        assert_eq!(store.load(), Err(AppearanceError::Invalid));
        std::fs::remove_file(directory.join("appearance.json")).expect("repair fixture");
        let barrier = owner.update(cx, |state, cx| {
            state.save(cx);
            state
                .set_font_size(125, cx)
                .expect("edit while recovery is pending");
            let barrier = state.take_pending_write(cx);
            assert!(
                barrier.is_some(),
                "close owns the pending recovery and save"
            );
            assert_eq!(state.set_locale("en", cx), Err(AppearanceError::Locked));
            barrier
        });
        // Holding the barrier keeps the writer alive while the deterministic executor drains.
        cx.run_until_parked();
        assert!(owner.update(cx, |state, _| state.finish_close()));
        let saved = store
            .load()
            .expect("recovered read")
            .expect("durable preferences");
        assert_eq!(saved.preferences.locale, "ar");
        assert_eq!(saved.preferences.font_size, 125);
        drop(barrier);
        std::fs::remove_dir_all(directory).expect("remove owned fixture");
    }

    #[gpui_kit::test]
    fn failed_write_keeps_edits_and_explicit_retry_succeeds(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        let (directory, store) = fixture();
        let lock = std::fs::File::create(directory.join("appearance.lock")).expect("fixture lock");
        lock.try_lock().expect("hold lock");
        let owner = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.new(|cx| {
                AppearanceState::new(store.clone(), Ok(None), GenericFamilies::default(), cx)
            })
        });
        owner.update(cx, |state, cx| {
            state.set_locale("he", cx).expect("valid edit");
        });
        cx.run_until_parked();
        owner.update(cx, |state, cx| {
            assert_eq!(
                state.save_status(),
                SaveStatus::Failed(AppearanceError::Locked)
            );
            assert_eq!(state.preferences().locale, "he");
            assert!(state.take_pending_write(cx).is_none());
            assert!(!state.finish_close(), "failure keeps close blocked");
        });
        assert_eq!(
            store.load(),
            Ok(None),
            "failed writes leave the snapshot absent"
        );
        drop(lock);
        owner.update(cx, |state, cx| state.save(cx));
        cx.run_until_parked();
        assert_eq!(
            cx.read(|cx| owner.read(cx).save_status()),
            SaveStatus::Saved
        );
        assert_eq!(
            store
                .load()
                .expect("read")
                .expect("saved")
                .preferences
                .locale,
            "he"
        );
        std::fs::remove_dir_all(directory).expect("remove owned fixture");
    }

    #[gpui_kit::test]
    fn live_changes_apply_and_failed_storage_retains_edits(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        cx.update(|cx| {
            gpui_kit::init(cx);
            let owner = cx.new(|cx| {
                AppearanceState::new(
                    AppearanceStore::new(std::path::Path::new("/tmp/opencode/appearance-no-io")),
                    Err(AppearanceError::Invalid),
                    GenericFamilies::default(),
                    cx,
                )
            });
            owner.update(cx, |state, cx| {
                assert!(state.take_pending_write(cx).is_none());
                assert!(
                    state.finish_close(),
                    "an untouched invalid file is safe to leave unchanged on close"
                );
                state.cancel_close();
                state.set_mode(ThemeMode::Dark, cx).expect("valid mode");
                state.set_locale("ar", cx).expect("supported locale");
                state.set_font_size(125, cx).expect("valid text scale");
                assert_eq!(cx.global::<Palette>().id, state.preferences.dark_theme_id);
                assert_eq!(
                    locale::strings(cx).direction(),
                    locale::Direction::RightToLeft
                );
                assert_eq!(root_font_size(cx), 20.0);
                assert_eq!(
                    Theme::global(cx).font_size,
                    px(20.0),
                    "Kit's root must retain the live size when it prepares the next frame"
                );
                assert_eq!(
                    state.save_status(),
                    SaveStatus::Failed(AppearanceError::Invalid)
                );
                assert_eq!(
                    state.preferences().locale,
                    "ar",
                    "failed saves retain live edits"
                );
                assert!(!state.finish_close(), "dirty state must block normal close");
                let before = state.preferences().clone();
                assert_eq!(state.set_font_size(201, cx), Err(AppearanceError::Invalid));
                assert_eq!(
                    state.preferences(),
                    &before,
                    "invalid changes apply nothing"
                );
            });
        });
    }

    #[test]
    fn separate_theme_preferences_follow_mode_and_system() {
        let mut preferences = AppearancePreferences::default();
        assert_eq!(
            preferences.theme_id(Appearance::Light),
            preferences.light_theme_id
        );
        assert_eq!(
            preferences.theme_id(Appearance::Dark),
            preferences.dark_theme_id
        );
        preferences.mode = ThemeMode::Light;
        assert_eq!(
            preferences.theme_id(Appearance::Dark),
            preferences.light_theme_id
        );
        preferences.mode = ThemeMode::Dark;
        assert_eq!(
            preferences.theme_id(Appearance::Light),
            preferences.dark_theme_id
        );
        preferences
            .validate()
            .expect("default choices remain valid in explicit mode");
    }
}
