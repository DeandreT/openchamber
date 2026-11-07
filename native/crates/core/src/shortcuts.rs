//! Application keyboard shortcuts: the declared actions, their default
//! bindings, the user's overrides and the rules that compare bindings.
//!
//! A port of `packages/ui/src/lib/shortcuts` (`config.ts`, `schema.ts`,
//! `bindings.ts`) and of the recording rules in
//! `ShortcutRecordingDialog.tsx`. A binding is one chord or two
//! space-separated chords such as `mod+k p`. `mod` is Command on macOS and
//! Control elsewhere; `super` is the Windows or Super key off macOS.
//! Overrides persist as the settings document's `shortcutOverrides`, a map
//! from action ID to normalized binding.

use std::collections::BTreeMap;

/// The stored value that removes an action's binding.
pub const UNASSIGNED: &str = "__unassigned__";

/// The longest a recorder waits for a second chord.
pub const SECOND_CHORD_TIMEOUT_MS: u64 = 3000;

/// How many keys a recorded in-app chord may hold at once.
pub const MAX_RECORDED_KEYS: usize = 3;

/// Where an action is listed in Settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    Session,
    Models,
    Panels,
    Navigation,
    Application,
    /// OS-level global shortcuts the desktop shell registers; never listed.
    Desktop,
}

impl Category {
    /// The categories Settings lists, in order.
    pub const LISTED: [Self; 5] = [
        Self::Session,
        Self::Models,
        Self::Panels,
        Self::Navigation,
        Self::Application,
    ];

    /// The Settings section title's locale key.
    pub fn title_key(self) -> &'static str {
        match self {
            Self::Session => "settings.openchamber.keyboardShortcuts.category.session",
            Self::Models => "settings.openchamber.keyboardShortcuts.category.models",
            Self::Panels => "settings.openchamber.keyboardShortcuts.category.panels",
            Self::Navigation => "settings.openchamber.keyboardShortcuts.category.navigation",
            Self::Application => "settings.openchamber.keyboardShortcuts.category.application",
            // Never listed; the reference has no title for it either.
            Self::Desktop => "",
        }
    }
}

/// One application command and its default binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShortcutAction {
    pub id: &'static str,
    pub category: Category,
    pub default_binding: &'static str,
    /// The Settings label's locale key, for actions users can rebind.
    pub label_key: Option<&'static str>,
    /// The binding is a modifier prefix completed by a digit.
    pub prefix_style: bool,
}

impl ShortcutAction {
    pub fn customizable(&self) -> bool {
        self.label_key.is_some()
    }
}

const fn custom(
    id: &'static str,
    category: Category,
    default_binding: &'static str,
    label_key: &'static str,
) -> ShortcutAction {
    ShortcutAction {
        id,
        category,
        default_binding,
        label_key: Some(label_key),
        prefix_style: false,
    }
}

const fn fixed(
    id: &'static str,
    category: Category,
    default_binding: &'static str,
) -> ShortcutAction {
    ShortcutAction {
        id,
        category,
        default_binding,
        label_key: None,
        prefix_style: false,
    }
}

const fn prefix(
    id: &'static str,
    category: Category,
    default_binding: &'static str,
    label_key: &'static str,
) -> ShortcutAction {
    ShortcutAction {
        id,
        category,
        default_binding,
        label_key: Some(label_key),
        prefix_style: true,
    }
}

macro_rules! label {
    ($id:literal) => {
        concat!(
            "settings.openchamber.keyboardShortcuts.action.",
            $id,
            ".label"
        )
    };
}

use Category::{Application, Desktop, Models, Navigation, Panels, Session};

/// Every application command, in the order Settings lists them.
pub const SCHEMA: &[ShortcutAction] = &[
    custom(
        "add_selection_to_chat",
        Session,
        "mod+l",
        label!("add_selection_to_chat"),
    ),
    custom("focus_input", Session, "mod+i", label!("focus_input")),
    custom(
        "search_prompt_history",
        Session,
        "mod+arrowup",
        label!("search_prompt_history"),
    ),
    custom(
        "open_timeline_dialog",
        Session,
        "mod+k t",
        label!("open_timeline_dialog"),
    ),
    custom("new_chat", Session, "mod+n", label!("new_chat")),
    custom(
        "switch_session_previous",
        Session,
        "mod+alt+arrowleft",
        label!("switch_session_previous"),
    ),
    custom(
        "switch_session_next",
        Session,
        "mod+alt+arrowright",
        label!("switch_session_next"),
    ),
    custom(
        "session_history_back",
        Session,
        "mod+[",
        label!("session_history_back"),
    ),
    custom(
        "session_history_forward",
        Session,
        "mod+]",
        label!("session_history_forward"),
    ),
    custom(
        "rename_current_session",
        Session,
        "mod+k r",
        label!("rename_current_session"),
    ),
    custom(
        "archive_current_session",
        Session,
        "mod+k backspace",
        label!("archive_current_session"),
    ),
    custom(
        "toggle_permission_auto_accept",
        Session,
        "mod+k a",
        label!("toggle_permission_auto_accept"),
    ),
    custom(
        "close_session_tab",
        Session,
        "alt+w",
        label!("close_session_tab"),
    ),
    custom(
        "open_draft_project_picker",
        Session,
        "mod+k p",
        label!("open_draft_project_picker"),
    ),
    custom(
        "open_draft_worktree_picker",
        Session,
        "mod+k g",
        label!("open_draft_worktree_picker"),
    ),
    custom(
        "open_session_list",
        Session,
        "mod+k l",
        label!("open_session_list"),
    ),
    custom(
        "new_chat_worktree",
        Session,
        "mod+shift+n",
        label!("new_chat_worktree"),
    ),
    custom(
        "new_mini_chat",
        Session,
        "mod+alt+n",
        label!("new_mini_chat"),
    ),
    custom(
        "expand_input",
        Session,
        "mod+shift+e",
        label!("expand_input"),
    ),
    custom(
        "toggle_dictation",
        Session,
        "mod+alt+v",
        label!("toggle_dictation"),
    ),
    fixed("abort_run", Session, "escape"),
    custom(
        "background_session_work",
        Session,
        "mod+shift+b",
        label!("background_session_work"),
    ),
    custom(
        "open_model_selector",
        Models,
        "mod+shift+m",
        label!("open_model_selector"),
    ),
    fixed("cycle_thinking_variant", Models, "mod+shift+t"),
    custom("cycle_agent", Models, "tab", label!("cycle_agent")),
    custom(
        "cycle_favorite_model_forward",
        Models,
        "ctrl+alt+]",
        label!("cycle_favorite_model_forward"),
    ),
    custom(
        "cycle_favorite_model_backward",
        Models,
        "ctrl+alt+[",
        label!("cycle_favorite_model_backward"),
    ),
    custom(
        "toggle_terminal",
        Panels,
        "mod+j",
        label!("toggle_terminal"),
    ),
    custom(
        "toggle_context_panel_expanded",
        Panels,
        "mod+shift+f",
        label!("toggle_context_panel_expanded"),
    ),
    custom("toggle_sidebar", Panels, "mod+b", label!("toggle_sidebar")),
    custom(
        "toggle_source_board",
        Panels,
        "mod+k b",
        label!("toggle_source_board"),
    ),
    custom(
        "toggle_prompt_navigator",
        Panels,
        "mod+k n",
        label!("toggle_prompt_navigator"),
    ),
    prefix(
        "switch_session_tab",
        Panels,
        "mod",
        label!("switch_session_tab"),
    ),
    prefix(
        "switch_context_surface",
        Panels,
        "mod+alt",
        label!("switch_context_surface"),
    ),
    custom(
        "toggle_services_menu",
        Panels,
        "mod+k i",
        label!("toggle_services_menu"),
    ),
    fixed("save_file", Navigation, "mod+s"),
    fixed("find_in_file", Navigation, "mod+f"),
    custom(
        "open_go_to_line",
        Navigation,
        "alt+g",
        label!("open_go_to_line"),
    ),
    custom(
        "open_document_symbols",
        Navigation,
        "mod+shift+o",
        label!("open_document_symbols"),
    ),
    custom(
        "open_command_palette",
        Application,
        "mod+p",
        label!("open_command_palette"),
    ),
    custom(
        "open_settings",
        Application,
        "mod+comma",
        label!("open_settings"),
    ),
    custom("open_help", Application, "mod+k h", label!("open_help")),
    custom(
        "open_project_actions",
        Application,
        "mod+alt+r",
        label!("open_project_actions"),
    ),
    custom("cycle_theme", Application, "mod+k c", label!("cycle_theme")),
    custom(
        "mini_chat_global",
        Desktop,
        UNASSIGNED,
        label!("mini_chat_global"),
    ),
    custom(
        "quake_mode_global",
        Desktop,
        UNASSIGNED,
        label!("quake_mode_global"),
    ),
];

/// The declared action with this ID.
pub fn action(id: &str) -> Option<&'static ShortcutAction> {
    SCHEMA.iter().find(|action| action.id == id)
}

/// The modifiers of one chord.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    /// Command on macOS, Control elsewhere.
    pub primary: bool,
    pub ctrl: bool,
    /// The Windows or Super key; macOS has none of its own.
    pub super_key: bool,
    pub shift: bool,
    pub alt: bool,
}

impl Modifiers {
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }
}

/// One chord: modifiers and at most one key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chord {
    pub modifiers: Modifiers,
    /// A normalized key token (`k`, `comma`, `arrowup`); empty for a bare
    /// modifier prefix.
    pub key: String,
}

fn modifier_token(part: &str) -> Option<fn(&mut Modifiers)> {
    Some(match part {
        "mod" | "meta" | "cmd" | "command" => |m: &mut Modifiers| m.primary = true,
        "shift" => |m: &mut Modifiers| m.shift = true,
        "alt" | "option" => |m: &mut Modifiers| m.alt = true,
        "ctrl" => |m: &mut Modifiers| m.ctrl = true,
        "super" | "win" => |m: &mut Modifiers| m.super_key = true,
        _ => return None,
    })
}

fn is_unassigned(combo: &str) -> bool {
    combo.trim().eq_ignore_ascii_case(UNASSIGNED)
}

fn parse_chord(chord: &str) -> Chord {
    let mut modifiers = Modifiers::default();
    let mut key = String::new();
    for part in chord
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let part = part.to_lowercase();
        let part = match part.as_str() {
            "," => "comma".to_owned(),
            "." => "period".to_owned(),
            _ => part,
        };
        match modifier_token(&part) {
            Some(set) => set(&mut modifiers),
            None => key = part,
        }
    }
    Chord { modifiers, key }
}

fn chord_string(chord: &Chord) -> String {
    let m = chord.modifiers;
    let mut parts: Vec<&str> = [
        (m.primary, "mod"),
        (m.ctrl, "ctrl"),
        (m.super_key, "super"),
        (m.shift, "shift"),
        (m.alt, "alt"),
    ]
    .into_iter()
    .filter_map(|(held, name)| held.then_some(name))
    .collect();
    if !chord.key.is_empty() {
        parts.push(&chord.key);
    }
    parts.join("+")
}

/// Splits a raw binding into its chords, joining `a + b` spellings.
fn raw_chords(combo: &str) -> Vec<String> {
    let mut joined = String::new();
    let mut pending_plus = false;
    for token in combo.split_whitespace() {
        let starts_plus = token.starts_with('+');
        if !joined.is_empty() && !pending_plus && !starts_plus {
            joined.push(' ');
        }
        joined.push_str(token);
        pending_plus = token.ends_with('+') && token.len() > 1 || token == "+";
    }
    joined
        .split(' ')
        .filter(|chord| !chord.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The canonical spelling of a binding: modifiers in a fixed order and at
/// most two chords. An empty string is no valid binding.
pub fn normalize(combo: &str) -> String {
    if is_unassigned(combo) {
        return UNASSIGNED.to_owned();
    }
    let chords = raw_chords(combo.trim());
    if chords.is_empty() || chords.len() > 2 {
        return String::new();
    }
    chords
        .iter()
        .map(|chord| chord_string(&parse_chord(chord)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The chords of a binding; `None` for nothing parseable or the unassigned
/// value.
pub fn parse(combo: &str) -> Option<Vec<Chord>> {
    let normalized = normalize(combo);
    if normalized.is_empty() || normalized == UNASSIGNED {
        return None;
    }
    Some(normalized.split(' ').map(parse_chord).collect())
}

/// Whether a binding names a key in every chord, or is the unassigned value.
pub fn is_valid(combo: &str) -> bool {
    if is_unassigned(combo) {
        return true;
    }
    parse(combo).is_some_and(|chords| chords.iter().all(|chord| !chord.key.is_empty()))
}

/// How two bindings collide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conflict {
    Exact,
    /// One is a single chord that starts the other's sequence.
    Prefix,
}

/// The conflict between two bindings, if any.
pub fn conflict(left: &str, right: &str) -> Option<Conflict> {
    let left = normalize(left);
    let right = normalize(right);
    if !is_valid(&left) || !is_valid(&right) || left == UNASSIGNED || right == UNASSIGNED {
        return None;
    }
    if left == right {
        return Some(Conflict::Exact);
    }
    let left_chords: Vec<&str> = left.split(' ').collect();
    let right_chords: Vec<&str> = right.split(' ').collect();
    (left_chords[0] == right_chords[0] && left_chords.len() != right_chords.len())
        .then_some(Conflict::Prefix)
}

/// The user's bindings by action ID, as the settings document stores them.
pub type Overrides = BTreeMap<String, String>;

/// The binding an action uses: its valid override, else its default. An empty
/// string means unbound.
pub fn effective_combo(id: &str, overrides: &Overrides) -> String {
    let Some(action) = action(id) else {
        return String::new();
    };
    let default = if action.default_binding == UNASSIGNED {
        ""
    } else {
        action.default_binding
    };
    if !action.customizable() {
        return default.to_owned();
    }
    if let Some(value) = overrides.get(id) {
        let normalized = normalize(value);
        if normalized == UNASSIGNED {
            return String::new();
        }
        if is_valid(&normalized) {
            return normalized;
        }
    }
    default.to_owned()
}

/// The modifier prefix a prefix-style action uses; [`UNASSIGNED`] when the
/// user removed it.
pub fn effective_prefix(id: &str, overrides: &Overrides) -> String {
    let Some(action) = action(id) else {
        return String::new();
    };
    if !action.customizable() {
        return action.default_binding.to_owned();
    }
    if let Some(value) = overrides.get(id).filter(|value| !value.trim().is_empty()) {
        let normalized = normalize(value);
        if normalized == UNASSIGNED {
            return UNASSIGNED.to_owned();
        }
        let first = normalized.split(' ').next().map(parse_chord);
        if first.is_some_and(|chord| !chord.modifiers.is_empty() || !chord.key.is_empty()) {
            return normalized;
        }
    }
    action.default_binding.to_owned()
}

/// The binding an action effectively holds for conflict checks.
fn held_binding(action: &ShortcutAction, overrides: &Overrides) -> String {
    if action.prefix_style {
        effective_prefix(action.id, overrides)
    } else {
        effective_combo(action.id, overrides)
    }
}

/// Every other action `combo` would collide with if `id` took it.
pub fn binding_conflicts(
    id: &str,
    combo: &str,
    overrides: &Overrides,
) -> Vec<(&'static ShortcutAction, Conflict)> {
    if action(id).is_none() {
        return Vec::new();
    }
    SCHEMA
        .iter()
        .filter(|candidate| candidate.id != id)
        .filter_map(|candidate| {
            conflict(combo, &held_binding(candidate, overrides)).map(|kind| (candidate, kind))
        })
        .collect()
}

/// Reads the settings document's `shortcutOverrides`: trimmed keys with
/// nonempty trimmed string values, as `parseShortcutOverrides` keeps them.
pub fn parse_overrides<'a>(entries: impl IntoIterator<Item = (&'a str, &'a str)>) -> Overrides {
    entries
        .into_iter()
        .filter_map(|(key, value)| {
            let key = key.trim();
            let value = value.trim();
            (!key.is_empty() && !value.is_empty()).then(|| (key.to_owned(), value.to_owned()))
        })
        .collect()
}

/// The keyboard conventions a binding is shown with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Windows,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        match std::env::consts::OS {
            "macos" => Self::MacOs,
            "windows" => Self::Windows,
            _ => Self::Other,
        }
    }
}

fn key_label(key: &str) -> String {
    match key {
        "comma" => ",".to_owned(),
        "period" => ".".to_owned(),
        "enter" => "Enter".to_owned(),
        "escape" => "Esc".to_owned(),
        "tab" => "Tab".to_owned(),
        "space" => "Space".to_owned(),
        "backspace" => "⌫".to_owned(),
        "delete" => "⌦".to_owned(),
        "arrowup" => "↑".to_owned(),
        "arrowdown" => "↓".to_owned(),
        "arrowleft" => "←".to_owned(),
        "arrowright" => "→".to_owned(),
        "home" => "Home".to_owned(),
        "end" => "End".to_owned(),
        "pageup" => "Page Up".to_owned(),
        "pagedown" => "Page Down".to_owned(),
        other => other.to_uppercase(),
    }
}

fn modifier_labels(platform: Platform) -> [&'static str; 5] {
    // primary, ctrl, super, shift, alt
    match platform {
        Platform::MacOs => ["⌘", "⌃", "Super", "⇧", "⌥"],
        Platform::Windows => ["Ctrl", "Ctrl", "Win", "Shift", "Alt"],
        Platform::Other => ["Ctrl", "Ctrl", "Super", "Shift", "Alt"],
    }
}

/// A binding as the user reads it: `Ctrl + K, P` or `⌘ + ,`.
pub fn format_for_display(combo: &str, unassigned_label: &str, platform: Platform) -> String {
    let Some(chords) = parse(combo) else {
        return unassigned_label.to_owned();
    };
    if chords
        .iter()
        .any(|chord| chord.key.is_empty() && chord.modifiers.is_empty())
    {
        return unassigned_label.to_owned();
    }
    let labels = modifier_labels(platform);
    chords
        .iter()
        .map(|chord| {
            let m = chord.modifiers;
            let mut parts: Vec<String> = [m.primary, m.ctrl, m.super_key, m.shift, m.alt]
                .into_iter()
                .zip(labels)
                .filter(|(held, _)| *held)
                .map(|(_, label)| label.to_owned())
                .collect();
            if !chord.key.is_empty() {
                parts.push(key_label(&chord.key));
            }
            parts.join(" + ")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

const RISKY_BROWSER_KEYS: &[&str] = &[
    "w", "t", "r", "p", "s", "f", "l", "n", "q", "d", "h", "j", "o", "u",
];

/// Whether a browser may take the binding first (closing a tab, printing).
pub fn is_risky_browser_shortcut(combo: &str) -> bool {
    let Some(chords) = parse(combo) else {
        return false;
    };
    chords.iter().any(|chord| {
        let m = chord.modifiers;
        if !m.primary || m.alt {
            return false;
        }
        if m.shift {
            return chord.key == "w" || chord.key == "q";
        }
        RISKY_BROWSER_KEYS.contains(&chord.key.as_str())
    })
}

/// The token of a pressed key's character, as `keyToShortcutToken` names it.
pub fn key_token(key: &str) -> String {
    let lowered = key.to_lowercase();
    let token = match lowered.as_str() {
        "," => "comma",
        "." => "period",
        " " => "space",
        "esc" => "escape",
        "+" => "plus",
        "-" | "_" => "minus",
        "{" => "[",
        "}" => "]",
        ":" => ";",
        "\"" => "'",
        "<" => ",",
        ">" => ".",
        "?" => "/",
        "|" => "\\",
        "~" => "`",
        "!" => "1",
        "@" => "2",
        "#" => "3",
        "$" => "4",
        "%" => "5",
        "^" => "6",
        "&" => "7",
        "*" => "8",
        "(" => "9",
        ")" => "0",
        _ => return lowered,
    };
    token.to_owned()
}

/// One key press or release the recorder sees, with the platform's
/// physical modifier flags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedKey {
    /// The key's token from [`key_token`], or `control`, `alt`, `shift`,
    /// `meta` for a modifier key.
    pub key: String,
    pub control: bool,
    pub meta: bool,
    pub shift: bool,
    pub alt: bool,
    pub repeat: bool,
    pub composing: bool,
}

const MODIFIER_KEYS: [&str; 4] = ["shift", "control", "alt", "meta"];

/// A recording in progress.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recording {
    pub chords: Vec<String>,
    /// The modifiers held without a key yet.
    pub live_preview: Option<String>,
    pub settled: bool,
}

/// What a key event does to a recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyPhase {
    Down,
    Up,
}

fn modifier_parts(key: &RecordedKey, platform: Platform) -> Vec<&'static str> {
    let mut parts = Vec::new();
    if platform == Platform::MacOs {
        if key.meta {
            parts.push("mod");
        }
        if key.control {
            parts.push("ctrl");
        }
    } else {
        if key.control {
            parts.push("mod");
        }
        if key.meta {
            parts.push("super");
        }
    }
    if key.shift {
        parts.push("shift");
    }
    if key.alt {
        parts.push("alt");
    }
    parts
}

fn physical_count(key: &RecordedKey, include_key: bool) -> usize {
    let mut held: Vec<&str> = [
        (key.alt, "alt"),
        (key.control, "control"),
        (key.meta, "meta"),
        (key.shift, "shift"),
    ]
    .into_iter()
    .filter_map(|(held, name)| held.then_some(name))
    .collect();
    if include_key && !held.contains(&key.key.as_str()) {
        held.push(&key.key);
    }
    held.len()
}

fn modifier_preview(key: &RecordedKey, platform: Platform) -> Option<String> {
    if physical_count(key, false) > MAX_RECORDED_KEYS {
        return None;
    }
    let parts = modifier_parts(key, platform);
    (!parts.is_empty()).then(|| normalize(&parts.join("+")))
}

fn chord_from_key(key: &RecordedKey, platform: Platform) -> Option<String> {
    if MODIFIER_KEYS.contains(&key.key.as_str()) || key.key.is_empty() {
        return None;
    }
    if physical_count(key, true) > MAX_RECORDED_KEYS {
        return None;
    }
    let mut parts = modifier_parts(key, platform);
    parts.push(&key.key);
    Some(normalize(&parts.join("+")))
}

impl Recording {
    /// The recorded binding so far.
    pub fn combo(&self) -> String {
        normalize(&self.chords.join(" "))
    }

    /// The second-chord wait ended, or Confirm was pressed.
    pub fn settle(&mut self) {
        if !self.chords.is_empty() {
            self.live_preview = None;
            self.settled = true;
        }
    }

    /// Applies one key event, as `updateShortcutRecordingState` does.
    pub fn apply(
        &mut self,
        key: &RecordedKey,
        phase: KeyPhase,
        max_chords: usize,
        platform: Platform,
    ) {
        if key.repeat || key.composing {
            return;
        }
        if phase == KeyPhase::Up {
            self.live_preview = modifier_preview(key, platform);
            return;
        }
        if key.key == "backspace" {
            self.chords.pop();
            self.live_preview = None;
            self.settled = false;
            return;
        }
        if let Some(chord) = chord_from_key(key, platform) {
            if self.settled {
                self.chords = vec![chord];
            } else if self.chords.len() < max_chords {
                self.chords.push(chord);
            }
            self.live_preview = None;
            self.settled = self.chords.len() >= max_chords;
            return;
        }
        self.live_preview = modifier_preview(key, platform);
    }

    /// A prefix-style action records the modifiers released with no key.
    pub fn apply_prefix_release(&mut self, key: &RecordedKey, platform: Platform) -> bool {
        if !self.chords.is_empty() || !MODIFIER_KEYS.contains(&key.key.as_str()) {
            return false;
        }
        if physical_count(key, true) > MAX_RECORDED_KEYS {
            return false;
        }
        let released = RecordedKey {
            control: key.control || key.key == "control",
            meta: key.meta || key.key == "meta",
            shift: key.shift || key.key == "shift",
            alt: key.alt || key.key == "alt",
            ..key.clone()
        };
        let parts = modifier_parts(&released, platform);
        if parts.is_empty() {
            return false;
        }
        *self = Self {
            chords: vec![normalize(&parts.join("+"))],
            live_preview: None,
            settled: true,
        };
        true
    }
}

/// What a recorded binding would do on Confirm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordingVerdict {
    /// Nothing recorded yet.
    Empty,
    /// Collides with a binding users cannot change.
    Internal,
    /// Starts or extends another action's sequence.
    Prefix(&'static ShortcutAction),
    /// Saves, taking the binding from another action.
    Replaces(&'static ShortcutAction),
    Saves,
}

impl RecordingVerdict {
    pub fn can_save(&self) -> bool {
        matches!(self, Self::Replaces(_) | Self::Saves)
    }
}

/// Classifies a recorded binding the way the recording dialog does.
pub fn recording_verdict(id: &str, combo: &str, overrides: &Overrides) -> RecordingVerdict {
    if combo.is_empty() {
        return RecordingVerdict::Empty;
    }
    let conflicts = binding_conflicts(id, combo, overrides);
    if conflicts.iter().any(|(action, _)| !action.customizable()) {
        return RecordingVerdict::Internal;
    }
    if let Some((action, _)) = conflicts.iter().find(|(_, kind)| *kind == Conflict::Prefix) {
        return RecordingVerdict::Prefix(action);
    }
    if let Some((action, _)) = conflicts.iter().find(|(_, kind)| *kind == Conflict::Exact) {
        return RecordingVerdict::Replaces(action);
    }
    RecordingVerdict::Saves
}

/// The overrides after saving `combo` for `id`, releasing `replaced` when
/// the binding came from another action.
pub fn save_override(
    overrides: &Overrides,
    id: &str,
    combo: &str,
    replaced: Option<&str>,
) -> Overrides {
    let mut next = overrides.clone();
    next.insert(id.to_owned(), combo.to_owned());
    if let Some(replaced) = replaced {
        next.insert(replaced.to_owned(), UNASSIGNED.to_owned());
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overrides(entries: &[(&str, &str)]) -> Overrides {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn schema_ids_are_unique_and_defaults_valid() {
        let mut ids = std::collections::HashSet::new();
        for action in SCHEMA {
            assert!(ids.insert(action.id), "{} is declared once", action.id);
            if action.default_binding != UNASSIGNED && !action.prefix_style {
                assert!(is_valid(action.default_binding), "{}", action.id);
                assert_eq!(normalize(action.default_binding), action.default_binding);
            }
        }
        assert_eq!(SCHEMA.len(), 46);
    }

    #[test]
    fn no_two_defaults_collide() {
        for (index, left) in SCHEMA.iter().enumerate() {
            for right in &SCHEMA[index + 1..] {
                if left.prefix_style || right.prefix_style {
                    continue;
                }
                assert_eq!(
                    conflict(left.default_binding, right.default_binding),
                    None,
                    "{} and {}",
                    left.id,
                    right.id
                );
            }
        }
    }

    #[test]
    fn normalizes_aliases_order_and_sequences() {
        assert_eq!(normalize("Shift+Cmd+K"), "mod+shift+k");
        assert_eq!(normalize("option+meta+n"), "mod+alt+n");
        assert_eq!(normalize("ctrl + ,"), "ctrl+comma");
        assert_eq!(normalize("win+shift+x"), "super+shift+x");
        assert_eq!(normalize("mod+k  p"), "mod+k p");
        assert_eq!(normalize("mod+k p q"), "");
        assert_eq!(normalize("  "), "");
        assert_eq!(normalize("__UNASSIGNED__"), UNASSIGNED);
    }

    #[test]
    fn conflicts_follow_shared_leaders() {
        assert_eq!(conflict("mod+k", "mod+k p"), Some(Conflict::Prefix));
        assert_eq!(conflict("mod+k p", "mod+k g"), None, "siblings are valid");
        assert_eq!(conflict("Cmd+B", "mod+b"), Some(Conflict::Exact));
        assert_eq!(conflict(UNASSIGNED, "mod+b"), None);
        assert_eq!(
            conflict("mod", "mod+b"),
            None,
            "a bare modifier is no binding"
        );
    }

    #[test]
    fn effective_bindings_honor_valid_overrides_only() {
        let empty = Overrides::new();
        assert_eq!(effective_combo("toggle_sidebar", &empty), "mod+b");
        assert_eq!(
            effective_combo(
                "toggle_sidebar",
                &overrides(&[("toggle_sidebar", "Ctrl+Shift+S")])
            ),
            "ctrl+shift+s"
        );
        assert_eq!(
            effective_combo(
                "toggle_sidebar",
                &overrides(&[("toggle_sidebar", UNASSIGNED)])
            ),
            ""
        );
        assert_eq!(
            effective_combo("toggle_sidebar", &overrides(&[("toggle_sidebar", "mod+")])),
            "mod+b",
            "an override without a key falls back"
        );
        assert_eq!(
            effective_combo("abort_run", &overrides(&[("abort_run", "mod+x")])),
            "escape",
            "internal bindings ignore overrides"
        );
        assert_eq!(effective_combo("mini_chat_global", &empty), "");
        assert_eq!(effective_combo("does_not_exist", &empty), "");
    }

    #[test]
    fn effective_prefixes_match_the_reference() {
        let empty = Overrides::new();
        assert_eq!(
            effective_prefix("switch_context_surface", &empty),
            "mod+alt"
        );
        let custom = overrides(&[("switch_context_surface", "mod+p")]);
        assert_eq!(effective_prefix("switch_context_surface", &custom), "mod+p");
        let modifier = overrides(&[("switch_context_surface", "shift")]);
        assert_eq!(
            effective_prefix("switch_context_surface", &modifier),
            "shift"
        );
        let removed = overrides(&[("switch_context_surface", UNASSIGNED)]);
        assert_eq!(
            effective_prefix("switch_context_surface", &removed),
            UNASSIGNED
        );
        assert_eq!(effective_prefix("does_not_exist", &empty), "");
    }

    #[test]
    fn display_uses_platform_names() {
        assert_eq!(
            format_for_display("mod+k p", "Unassigned", Platform::Other),
            "Ctrl + K, P"
        );
        assert_eq!(
            format_for_display("mod+shift+n", "-", Platform::MacOs),
            "⌘ + ⇧ + N"
        );
        assert_eq!(
            format_for_display("mod+comma", "-", Platform::Windows),
            "Ctrl + ,"
        );
        assert_eq!(
            format_for_display("super+arrowup", "-", Platform::Windows),
            "Win + ↑"
        );
        assert_eq!(
            format_for_display(UNASSIGNED, "None", Platform::Other),
            "None"
        );
        assert_eq!(
            format_for_display("mod+alt", "-", Platform::Other),
            "Ctrl + Alt"
        );
    }

    #[test]
    fn binding_conflicts_cover_internal_and_desktop_actions() {
        let empty = Overrides::new();
        let escape = binding_conflicts("toggle_sidebar", "escape", &empty);
        assert_eq!(escape.len(), 1);
        assert_eq!(escape[0].0.id, "abort_run");
        assert_eq!(
            recording_verdict("toggle_sidebar", "escape", &empty),
            RecordingVerdict::Internal
        );
        assert!(matches!(
            recording_verdict("toggle_sidebar", "mod+k", &empty),
            RecordingVerdict::Prefix(_)
        ));
        let replaces = recording_verdict("toggle_sidebar", "mod+j", &empty);
        assert_eq!(
            replaces,
            RecordingVerdict::Replaces(action("toggle_terminal").expect("declared"))
        );
        let global = overrides(&[("mini_chat_global", "mod+shift+space")]);
        assert!(matches!(
            recording_verdict("toggle_sidebar", "mod+shift+space", &global),
            RecordingVerdict::Replaces(action) if action.id == "mini_chat_global"
        ));
        assert_eq!(
            recording_verdict("toggle_sidebar", "mod+shift+y", &empty),
            RecordingVerdict::Saves
        );
    }

    #[test]
    fn saving_a_replacement_unassigns_the_previous_owner() {
        let next = save_override(
            &Overrides::new(),
            "toggle_sidebar",
            "mod+j",
            Some("toggle_terminal"),
        );
        assert_eq!(effective_combo("toggle_sidebar", &next), "mod+j");
        assert_eq!(effective_combo("toggle_terminal", &next), "");
    }

    #[test]
    fn parsed_overrides_drop_blank_entries() {
        let parsed = parse_overrides([
            (" new_chat ", " mod+m "),
            ("", "mod+x"),
            ("toggle_sidebar", " "),
        ]);
        assert_eq!(parsed, overrides(&[("new_chat", "mod+m")]));
    }

    #[test]
    fn risky_browser_shortcuts_check_every_chord() {
        assert!(is_risky_browser_shortcut("mod+w"));
        assert!(is_risky_browser_shortcut("mod+k mod+t"));
        assert!(is_risky_browser_shortcut("mod+shift+q"));
        assert!(!is_risky_browser_shortcut("mod+shift+t"));
        assert!(!is_risky_browser_shortcut("mod+alt+w"));
    }

    fn press(key: &str, control: bool, shift: bool) -> RecordedKey {
        RecordedKey {
            key: key.to_owned(),
            control,
            meta: false,
            shift,
            alt: false,
            repeat: false,
            composing: false,
        }
    }

    #[test]
    fn recording_takes_two_chords_and_backspace_edits() {
        let mut recording = Recording::default();
        recording.apply(
            &press("control", true, false),
            KeyPhase::Down,
            2,
            Platform::Other,
        );
        assert_eq!(recording.live_preview.as_deref(), Some("mod"));
        recording.apply(&press("k", true, false), KeyPhase::Down, 2, Platform::Other);
        assert_eq!(recording.chords, ["mod+k"]);
        assert!(!recording.settled);
        recording.apply(
            &press("p", false, false),
            KeyPhase::Down,
            2,
            Platform::Other,
        );
        assert_eq!(recording.combo(), "mod+k p");
        assert!(recording.settled);
        recording.apply(&press("x", true, false), KeyPhase::Down, 2, Platform::Other);
        assert_eq!(
            recording.combo(),
            "mod+x",
            "a settled recording starts over"
        );
        recording.apply(
            &press("backspace", false, false),
            KeyPhase::Down,
            2,
            Platform::Other,
        );
        assert!(recording.chords.is_empty());
    }

    #[test]
    fn recording_settles_one_chord_and_caps_held_keys() {
        let mut recording = Recording::default();
        recording.apply(&press("j", true, false), KeyPhase::Down, 2, Platform::Other);
        recording.settle();
        assert!(recording.settled);
        assert_eq!(recording.combo(), "mod+j");
        let mut crowded = Recording::default();
        let mut key = press("x", true, true);
        key.alt = true;
        crowded.apply(&key, KeyPhase::Down, 2, Platform::Other);
        assert!(
            crowded.chords.is_empty(),
            "four keys exceed the in-app limit"
        );
        let mut repeated = Recording::default();
        let mut key = press("j", true, false);
        key.repeat = true;
        repeated.apply(&key, KeyPhase::Down, 2, Platform::Other);
        assert!(repeated.chords.is_empty());
    }

    #[test]
    fn recording_maps_mac_modifiers_separately() {
        let mut recording = Recording::default();
        let mut key = press("n", true, false);
        key.meta = true;
        recording.apply(&key, KeyPhase::Down, 2, Platform::MacOs);
        assert_eq!(recording.combo(), "mod+ctrl+n");
        let mut other = Recording::default();
        other.apply(&key, KeyPhase::Down, 2, Platform::Other);
        assert_eq!(other.combo(), "mod+super+n");
    }

    #[test]
    fn prefix_actions_record_released_modifiers() {
        let mut recording = Recording::default();
        let release = RecordedKey {
            key: "alt".to_owned(),
            control: true,
            meta: false,
            shift: false,
            alt: false,
            repeat: false,
            composing: false,
        };
        assert!(recording.apply_prefix_release(&release, Platform::Other));
        assert_eq!(recording.combo(), "mod+alt");
        assert!(recording.settled);
    }

    #[test]
    fn key_tokens_name_shifted_characters_by_their_base_key() {
        assert_eq!(key_token("{"), "[");
        assert_eq!(key_token(","), "comma");
        assert_eq!(key_token("K"), "k");
        assert_eq!(key_token("ArrowUp"), "arrowup");
    }
}
