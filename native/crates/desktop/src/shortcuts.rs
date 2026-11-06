//! Configurable application shortcuts: the user's bindings in GPUI's keymap,
//! the commands they run, and the writes that save them.
//!
//! The schema and binding rules live in `openchamber_native_core::shortcuts`.
//! Only actions with a native command are bound; the rest keep their keys
//! free, so a press reaches whatever would have taken it without the
//! binding. The bindings this module adds carry [`OWNED`] in their meta, so a
//! changed set replaces exactly them, and the terminal's shadowing bindings
//! are rebuilt from the new keymap.

use gpui_kit::{App, Context, Global, KeyBinding, KeyBindingMetaIndex, Window};
use openchamber_native_core::shortcuts::{self, Overrides, Platform};
use openchamber_native_transport::client::SettingsChanges;

use crate::appearance::ThemeMode;
use crate::appearance_view::AppearanceOwner;
use crate::workspace::Workspace;
use crate::workspace_view::KEY_CONTEXT;

/// Marks the keymap entries [`bind`] owns. Index 0 keeps the precedence of
/// unmarked bindings.
pub(crate) const OWNED: KeyBindingMetaIndex = KeyBindingMetaIndex(0);

/// Runs one schema action.
#[derive(Clone, PartialEq, serde::Deserialize, gpui_kit::Action)]
#[action(namespace = openchamber, no_json)]
pub(crate) struct RunShortcut {
    pub(crate) id: String,
}

/// The actions this client has a command for.
pub(crate) const NATIVE_ACTIONS: &[&str] = &[
    "new_chat",
    "focus_input",
    "toggle_permission_auto_accept",
    "toggle_terminal",
    "toggle_sidebar",
    "toggle_prompt_navigator",
    "open_settings",
    "cycle_theme",
];

/// The bindings in effect and the state of saving them.
#[derive(Debug, Default)]
pub(crate) struct ShortcutBindings {
    /// What the keymap holds: the user's choice, saved or not.
    pub(crate) overrides: Overrides,
    /// The settings document's map as last read.
    server: Option<Overrides>,
    /// A save is in flight; another is wanted after it.
    writing: bool,
    dirty: bool,
    pub(crate) failed: bool,
}

impl Global for ShortcutBindings {}

impl ShortcutBindings {
    /// Takes the server's map unless the user's own change is still saving.
    /// Returns whether the bindings changed.
    fn adopt(&mut self, server: &Overrides) -> bool {
        if self.server.as_ref() == Some(server) {
            return false;
        }
        self.server = Some(server.clone());
        if self.writing || self.dirty || self.overrides == *server {
            return false;
        }
        self.overrides = server.clone();
        self.failed = false;
        true
    }
}

/// The keystrokes GPUI matches for a normalized binding; `None` for a
/// binding with nothing to press.
pub(crate) fn keystrokes(combo: &str, platform: Platform) -> Option<String> {
    let chords = shortcuts::parse(combo)?;
    let mut parts = Vec::with_capacity(chords.len());
    for chord in chords {
        if chord.key.is_empty() {
            return None;
        }
        let m = chord.modifiers;
        let mut keys: Vec<&str> = Vec::new();
        if m.primary {
            keys.push(if platform == Platform::MacOs {
                "cmd"
            } else {
                "ctrl"
            });
        }
        if m.ctrl && !(m.primary && platform != Platform::MacOs) {
            keys.push("ctrl");
        }
        if m.super_key {
            keys.push("super");
        }
        if m.alt {
            keys.push("alt");
        }
        if m.shift {
            keys.push("shift");
        }
        let key = match chord.key.as_str() {
            "arrowup" => "up",
            "arrowdown" => "down",
            "arrowleft" => "left",
            "arrowright" => "right",
            "comma" => ",",
            "period" => ".",
            "plus" => "+",
            "minus" => "-",
            other => other,
        };
        keys.push(key);
        parts.push(keys.join("-"));
    }
    Some(parts.join(" "))
}

fn native_bindings(overrides: &Overrides, platform: Platform) -> Vec<KeyBinding> {
    NATIVE_ACTIONS
        .iter()
        .filter_map(|id| {
            let combo = shortcuts::effective_combo(id, overrides);
            let keys = keystrokes(&combo, platform)?;
            Some(
                KeyBinding::new(
                    &keys,
                    RunShortcut {
                        id: (*id).to_owned(),
                    },
                    Some(KEY_CONTEXT),
                )
                .with_meta(OWNED),
            )
        })
        .collect()
}

/// The first chords the app keeps while a terminal has focus: the terminal
/// toggle, as `handleTerminalShortcutCapture` lets it through.
pub(crate) fn terminal_chords(overrides: &Overrides, platform: Platform) -> Vec<String> {
    let combo = shortcuts::effective_combo("toggle_terminal", overrides);
    keystrokes(&combo, platform)
        .and_then(|keys| keys.split(' ').next().map(str::to_owned))
        .into_iter()
        .collect()
}

/// Replaces the keymap's shortcut bindings with `overrides`' effective ones.
pub(crate) fn bind(overrides: &Overrides, cx: &mut App) {
    let platform = Platform::current();
    let kept: Vec<KeyBinding> = cx
        .key_bindings()
        .borrow()
        .bindings()
        .filter(|binding| binding.meta() != Some(OWNED))
        .cloned()
        .collect();
    cx.clear_key_bindings();
    cx.bind_keys(kept);
    cx.bind_keys(native_bindings(overrides, platform));
    crate::terminal_view::bind_keys(&terminal_chords(overrides, platform), cx);
}

fn cycle_theme(cx: &mut App) {
    let Some(owner) = cx
        .try_global::<AppearanceOwner>()
        .map(|owner| owner.0.clone())
    else {
        return;
    };
    owner.update(cx, |appearance, cx| {
        let next = match appearance.preferences().mode {
            ThemeMode::Light => ThemeMode::Dark,
            ThemeMode::Dark => ThemeMode::System,
            ThemeMode::System => ThemeMode::Light,
        };
        if let Err(error) = appearance.set_mode(next, cx) {
            tracing::warn!(?error, "the theme mode could not be saved");
        }
    });
}

impl Workspace {
    /// Runs a shortcut's command. `false` leaves the key to others.
    pub(crate) fn run_shortcut(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if NATIVE_ACTIONS.contains(&id) {
            // A shortcut acts on the window, as a native menu closes when
            // anything else takes the keyboard.
            self.close_app_menu(window, cx);
        }
        match id {
            "new_chat" => self.show_new_session(window, cx),
            "focus_input" => self.focus_composer(window, cx),
            "toggle_permission_auto_accept" => {
                if !self.permission_mode_interactive() {
                    return false;
                }
                self.cycle_permission_mode(window, cx);
            }
            "toggle_terminal" => self.toggle_terminal(window, cx),
            "toggle_sidebar" => self.toggle_sidebar(cx),
            "toggle_prompt_navigator" => self.toggle_prompt_navigator(window, cx),
            "open_settings" => self.open_settings(window, cx),
            "cycle_theme" => cycle_theme(cx),
            _ => return false,
        }
        true
    }

    /// Follows the settings document's bindings once they are read.
    pub(crate) fn sync_shortcut_bindings(&self, cx: &mut Context<Self>) {
        let Some(server) = self
            .send_catalog
            .settings
            .as_ref()
            .map(|settings| settings.shortcut_overrides.clone().unwrap_or_default())
        else {
            return;
        };
        let changed = cx.default_global::<ShortcutBindings>().adopt(&server);
        if changed {
            let overrides = cx.global::<ShortcutBindings>().overrides.clone();
            cx.defer(move |cx| bind(&overrides, cx));
        }
    }

    /// Applies the user's bindings at once and saves them to the server,
    /// one write at a time; a failed write keeps the choice and says so.
    pub(crate) fn save_shortcut_overrides(&mut self, overrides: Overrides, cx: &mut Context<Self>) {
        {
            let bindings = cx.default_global::<ShortcutBindings>();
            bindings.overrides = overrides.clone();
            bindings.dirty = true;
            bindings.failed = false;
        }
        bind(&overrides, cx);
        self.write_shortcut_overrides(cx);
        cx.notify();
    }

    fn write_shortcut_overrides(&mut self, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_ref() else {
            // Without a server the choice lives for this run only.
            cx.default_global::<ShortcutBindings>().dirty = false;
            return;
        };
        let Some(client) = connection.client.clone() else {
            cx.default_global::<ShortcutBindings>().failed = true;
            return;
        };
        let overrides = {
            let bindings = cx.default_global::<ShortcutBindings>();
            if bindings.writing || !bindings.dirty {
                return;
            }
            bindings.writing = true;
            bindings.dirty = false;
            bindings.overrides.clone()
        };
        let runtime = self.runtime.clone();
        let changes = SettingsChanges {
            shortcut_overrides: Some(overrides),
            ..SettingsChanges::default()
        };
        let task = connection
            .network
            .run(async move { client.save_settings(&changes).await });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let saved = matches!(result, Some(Ok(_)));
                {
                    let bindings = cx.default_global::<ShortcutBindings>();
                    bindings.writing = false;
                    bindings.failed = !saved && !bindings.dirty;
                }
                if let Some(Ok(settings)) = result
                    && this.runtime == runtime
                    && let Some(current) = this.send_catalog.settings.as_mut()
                {
                    current.shortcut_overrides = settings.shortcut_overrides;
                }
                if !saved {
                    tracing::warn!("the shortcut bindings could not be saved");
                }
                this.write_shortcut_overrides(cx);
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keystrokes_follow_gpui_names() {
        assert_eq!(
            keystrokes("mod+k p", Platform::Other).as_deref(),
            Some("ctrl-k p")
        );
        assert_eq!(
            keystrokes("mod+k p", Platform::MacOs).as_deref(),
            Some("cmd-k p")
        );
        assert_eq!(
            keystrokes("mod+alt+arrowleft", Platform::Other).as_deref(),
            Some("ctrl-alt-left")
        );
        assert_eq!(
            keystrokes("mod+comma", Platform::Windows).as_deref(),
            Some("ctrl-,")
        );
        assert_eq!(
            keystrokes("ctrl+alt+]", Platform::Other).as_deref(),
            Some("ctrl-alt-]")
        );
        assert_eq!(
            keystrokes("mod+ctrl+x", Platform::Other).as_deref(),
            Some("ctrl-x")
        );
        assert_eq!(
            keystrokes("mod+ctrl+x", Platform::MacOs).as_deref(),
            Some("cmd-ctrl-x")
        );
        assert_eq!(
            keystrokes("mod+shift+n", Platform::Other).as_deref(),
            Some("ctrl-shift-n")
        );
        assert_eq!(keystrokes("mod+alt", Platform::Other), None);
        assert_eq!(keystrokes(shortcuts::UNASSIGNED, Platform::Other), None);
        for action in shortcuts::SCHEMA {
            if let Some(keys) = keystrokes(action.default_binding, Platform::current()) {
                for keystroke in keys.split(' ') {
                    gpui_kit::Keystroke::parse(keystroke).expect("GPUI parses every default");
                }
            }
        }
    }

    #[test]
    fn every_native_action_is_declared_and_customizable() {
        for id in NATIVE_ACTIONS {
            let action = shortcuts::action(id).expect("declared in the schema");
            assert!(action.customizable(), "{id}");
        }
    }

    #[test]
    fn terminal_keeps_the_effective_toggle() {
        let empty = Overrides::new();
        assert_eq!(terminal_chords(&empty, Platform::Other), ["ctrl-j"]);
        let rebound = shortcuts::save_override(&empty, "toggle_terminal", "mod+k t", None);
        assert_eq!(terminal_chords(&rebound, Platform::Other), ["ctrl-k"]);
        let removed =
            shortcuts::save_override(&empty, "toggle_terminal", shortcuts::UNASSIGNED, None);
        assert!(terminal_chords(&removed, Platform::Other).is_empty());
    }

    #[test]
    fn server_maps_wait_for_the_users_pending_change() {
        let mut bindings = ShortcutBindings::default();
        let server = shortcuts::save_override(&Overrides::new(), "new_chat", "mod+m", None);
        assert!(bindings.adopt(&server));
        assert_eq!(bindings.overrides, server);
        assert!(
            !bindings.adopt(&server),
            "an unchanged read rebinds nothing"
        );
        bindings.overrides = Overrides::new();
        bindings.dirty = true;
        let other = shortcuts::save_override(&Overrides::new(), "new_chat", "mod+y", None);
        assert!(
            !bindings.adopt(&other),
            "a pending change keeps the user's choice"
        );
        assert!(bindings.overrides.is_empty());
    }

    #[gpui_kit::test]
    fn rebinding_replaces_only_owned_bindings(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            crate::workspace_view::bind_keys(cx);
            let count = |cx: &App, keys: &str| {
                cx.key_bindings()
                    .borrow()
                    .bindings()
                    .filter(|binding| {
                        binding.meta() == Some(OWNED)
                            && !gpui_kit::is_no_action(binding.action())
                            && binding
                                .keystrokes()
                                .iter()
                                .map(|keystroke| keystroke.unparse())
                                .collect::<Vec<_>>()
                                .join(" ")
                                == keys
                    })
                    .count()
            };
            let total = cx.key_bindings().borrow().bindings().count();
            assert_eq!(count(cx, "ctrl-b"), 1);
            let rebound =
                shortcuts::save_override(&Overrides::new(), "toggle_sidebar", "mod+shift+y", None);
            bind(&rebound, cx);
            assert_eq!(count(cx, "ctrl-b"), 0, "the old binding is gone");
            assert_eq!(count(cx, "ctrl-shift-y"), 1);
            assert_eq!(
                cx.key_bindings().borrow().bindings().count(),
                total,
                "one shortcut and its terminal shadow moved; nothing else changed"
            );
            bind(&Overrides::new(), cx);
            assert_eq!(cx.key_bindings().borrow().bindings().count(), total);
        });
    }
}
