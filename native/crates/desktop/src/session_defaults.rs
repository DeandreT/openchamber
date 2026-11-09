//! The Sessions page's OpenCode-backed sections: the prompt cache
//! (`SessionWarmingModeField.tsx`) and compaction (`CompactionSettings.tsx`).
//! Both show what OpenCode reports, write one field at a time, and put a
//! field back when its newest write fails.

use std::collections::{HashMap, HashSet};

use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::{
    AnyElement, AppContext, Context, Entity, FocusHandle, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, Role, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Task, TestSupportExt, Toggled, WeakEntity, Window, div, prelude::FluentBuilder,
    px,
};
use openchamber_native_transport::opencode_config::{
    Compaction, CompactionBuffer, CompactionChange, EffectiveConfig,
};
use openchamber_native_transport::session_warming::WarmingSnapshot;
use serde_json::Value;

use crate::activation::on_activate;
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::network::{Network, NetworkTask};
use crate::settings_controls::{
    Load, ServerTarget, SettingsWrites, checkbox_row, field_row, focus_handle, helper, load_failed,
    same_target, section_with_info,
};
use crate::theme::Palette;
use crate::workspace::Workspace;

/// OpenCode's warming when it is not tuned: 30 minutes after the last turn.
const OPENCODE_DEFAULT_MINUTES: u32 = 30;
/// The warm durations the server accepts.
const WARMING_STEPS: [u32; 4] = [30, 60, 120, 180];
/// OpenCode's `compaction.keep.tokens` when the config leaves it unset.
const OPENCODE_DEFAULT_KEEP_TOKENS: u64 = 15_000;
const BUFFER_PERCENT_STEPS: [u32; 7] = [5, 10, 20, 25, 30, 40, 50];
const KEEP_STEPS: [u64; 5] = [5_000, 10_000, 30_000, 50_000, 100_000];
const DEFAULT_OPTION: &str = "default";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WarmingMode {
    Off,
    All,
    Session,
}

impl WarmingMode {
    fn wire(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::All => "all",
            Self::Session => "session",
        }
    }
}

/// OpenCode's own warming switch for every session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GlobalWarming {
    on: bool,
    minutes: Option<u32>,
}

/// OpenCode's warm duration in minutes, or `None` when it is not one
/// OpenChamber can read (`readGlobalMinutes`): `"1800000 millis"` and a
/// hand-written `"30 minutes"` both read.
fn global_warming(config: &EffectiveConfig) -> GlobalWarming {
    let minutes = |duration: &str| -> Option<u32> {
        let duration = duration.trim();
        let split = duration.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
        let (number, unit) = duration.split_at(split);
        let number: f64 = number.parse().ok()?;
        let per_minute = match unit.trim().to_ascii_lowercase().as_str() {
            "ms" | "millis" | "millisecond" | "milliseconds" => 1.0 / 60_000.0,
            "s" | "second" | "seconds" => 1.0 / 60.0,
            "m" | "minute" | "minutes" => 1.0,
            "h" | "hour" | "hours" => 60.0,
            _ => return None,
        };
        Some((number * per_minute).round() as u32)
    };
    match config.get("warming") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => GlobalWarming {
            on: false,
            minutes: None,
        },
        Some(Value::Object(warming)) => GlobalWarming {
            on: true,
            minutes: match warming.get("duration").and_then(Value::as_str) {
                Some(duration) => minutes(duration),
                None => Some(OPENCODE_DEFAULT_MINUTES),
            },
        },
        Some(_) => GlobalWarming {
            on: true,
            minutes: Some(OPENCODE_DEFAULT_MINUTES),
        },
    }
}

/// The per-session warming the server offers, or that it has none.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PerSession {
    Loading,
    Ready { enabled: bool, minutes: u32 },
    Unavailable,
}

#[derive(Clone, PartialEq, Eq)]
struct ValueOption {
    value: String,
    label: SharedString,
}

impl SearchableListItem for ValueOption {
    type Value = String;
    fn title(&self) -> SharedString {
        self.label.clone()
    }
    fn value(&self) -> &String {
        &self.value
    }
}

type Picker = Entity<SelectState<SearchableVec<ValueOption>>>;

/// Which compaction field a write changed, for restoring it on failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Field {
    Auto,
    Buffer,
    Keep,
    Background,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum WriteField {
    Mode,
    Minutes,
    Compaction(Field),
}

fn field_of(change: &CompactionChange) -> Field {
    match change {
        CompactionChange::Auto(_) => Field::Auto,
        CompactionChange::Buffer(_) => Field::Buffer,
        CompactionChange::KeepTokens(_) => Field::Keep,
        CompactionChange::Background(_) => Field::Background,
    }
}

fn apply(values: &mut Compaction, change: &CompactionChange) {
    match change {
        CompactionChange::Auto(auto) => values.auto = *auto,
        CompactionChange::Buffer(buffer) => values.buffer = buffer.clone(),
        CompactionChange::KeepTokens(tokens) => values.keep_tokens = *tokens,
        CompactionChange::Background(background) => values.background = *background,
    }
}

fn restore(values: &mut Compaction, saved: &Compaction, field: Field) {
    match field {
        Field::Auto => values.auto = saved.auto,
        Field::Buffer => values.buffer = saved.buffer.clone(),
        Field::Keep => values.keep_tokens = saved.keep_tokens,
        Field::Background => values.background = saved.background,
    }
}

fn buffer_value(buffer: &CompactionBuffer) -> String {
    match buffer {
        CompactionBuffer::Share(share) => share.clone(),
        CompactionBuffer::Tokens(tokens) => tokens.to_string(),
    }
}

pub(crate) struct SessionDefaults {
    owner: WeakEntity<Workspace>,
    network: Option<Network>,
    target: Option<ServerTarget>,
    read_revision: u64,
    reads: Vec<Task<()>>,
    global: Load<GlobalWarming>,
    per_session: PerSession,
    pending_mode: Option<WarmingMode>,
    pending_minutes: Option<u32>,
    warming_writes: u64,
    compaction: Load<Compaction>,
    /// What the config file holds as far as this page knows.
    saved: Option<Compaction>,
    compaction_writes: u64,
    latest_field_write: HashMap<Field, u64>,
    settled_field_write: HashMap<Field, u64>,
    latest_mode_write: u64,
    latest_minutes_write: u64,
    settled_warming_write: u64,
    failed_fields: HashSet<WriteField>,
    writes: SettingsWrites,
    /// A failed write since the last success, shown as Save failed.
    failed: bool,
    duration_picker: Picker,
    buffer_picker: Picker,
    keep_picker: Picker,
    picker_values: HashMap<&'static str, (Vec<ValueOption>, Option<String>)>,
    focus: HashMap<SharedString, FocusHandle>,
    _subscriptions: Vec<Subscription>,
}

impl SessionDefaults {
    pub(crate) fn new(
        owner: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker = |window: &mut Window, cx: &mut Context<Self>| -> Picker {
            cx.new(|cx| SelectState::new(SearchableVec::new(Vec::new()), None, window, cx))
        };
        let duration_picker = picker(window, cx);
        let buffer_picker = picker(window, cx);
        let keep_picker = picker(window, cx);
        let mut subscriptions = vec![
            cx.subscribe_in(&duration_picker, window, |this, _, event, window, cx| {
                if let SelectEvent::Confirm(Some(value)) = event
                    && let Ok(minutes) = value.parse()
                {
                    this.set_minutes(minutes, window, cx);
                }
            }),
            cx.subscribe_in(&buffer_picker, window, |this, _, event, _, cx| {
                if let SelectEvent::Confirm(Some(value)) = event {
                    let buffer = if value == DEFAULT_OPTION {
                        None
                    } else if value.ends_with('%') {
                        Some(CompactionBuffer::Share(value.clone()))
                    } else {
                        value.parse().ok().map(CompactionBuffer::Tokens)
                    };
                    this.save_compaction(CompactionChange::Buffer(buffer), cx);
                }
            }),
            cx.subscribe_in(&keep_picker, window, |this, _, event, _, cx| {
                if let SelectEvent::Confirm(Some(value)) = event {
                    let tokens = (value != DEFAULT_OPTION)
                        .then(|| value.parse().ok())
                        .flatten();
                    this.save_compaction(CompactionChange::KeepTokens(tokens), cx);
                }
            }),
        ];
        if let Some(workspace) = owner.upgrade() {
            subscriptions.push(cx.observe(&workspace, |this, _, cx| {
                let target = ServerTarget::capture(&this.owner, this.network.as_ref(), cx);
                if !same_target(this.target.as_ref(), target.as_ref()) {
                    this.refresh(cx);
                }
            }));
        }
        Self {
            owner,
            network: Network::new().ok(),
            target: None,
            read_revision: 0,
            reads: Vec::new(),
            global: Load::Loading,
            per_session: PerSession::Loading,
            pending_mode: None,
            pending_minutes: None,
            warming_writes: 0,
            compaction: Load::Loading,
            saved: None,
            compaction_writes: 0,
            latest_field_write: HashMap::new(),
            settled_field_write: HashMap::new(),
            latest_mode_write: 0,
            latest_minutes_write: 0,
            settled_warming_write: 0,
            failed_fields: HashSet::new(),
            writes: SettingsWrites::default(),
            failed: false,
            duration_picker,
            buffer_picker,
            keep_picker,
            picker_values: HashMap::new(),
            focus: HashMap::new(),
            _subscriptions: subscriptions,
        }
    }

    /// Reads OpenCode's config and the server's warming snapshot.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let target = ServerTarget::capture(&self.owner, self.network.as_ref(), cx);
        if same_target(self.target.as_ref(), target.as_ref()) && self.writes.is_pending() {
            return;
        }
        if !same_target(self.target.as_ref(), target.as_ref()) {
            self.global = Load::Loading;
            self.per_session = PerSession::Loading;
            self.compaction = Load::Loading;
            self.saved = None;
            self.pending_mode = None;
            self.pending_minutes = None;
            self.failed = false;
            self.failed_fields.clear();
            self.latest_field_write.clear();
            self.settled_field_write.clear();
            self.settled_warming_write = 0;
        }
        self.read_revision += 1;
        self.target = target.clone();
        let Some(target) = target else {
            self.reads.clear();
            cx.notify();
            return;
        };
        let revision = self.read_revision;
        let (writes_at_read, warming_at_read) = (self.compaction_writes, self.warming_writes);
        let client = target.client.clone();
        let config = NetworkTask::spawn_on(&target.network, async move {
            client.opencode_config(None).await
        });
        let client = target.client;
        let snapshot =
            NetworkTask::spawn_on(
                &target.network,
                async move { client.session_warming().await },
            );
        self.reads = vec![
            cx.spawn(async move |this, cx| {
                let result = config.await;
                let _ = this.update(cx, |this, cx| {
                    if this.read_revision != revision {
                        return;
                    }
                    match result {
                        Some(Ok(config)) => {
                            // A write since the read started is newer than it.
                            if this.warming_writes == warming_at_read {
                                this.global = Load::Ready(global_warming(&config));
                            }
                            if this.compaction_writes == writes_at_read {
                                let values = Compaction::from_config(&config);
                                this.saved = Some(values.clone());
                                this.compaction = Load::Ready(values);
                            }
                        }
                        _ => {
                            if this.global.ready().is_none() {
                                this.global = Load::Failed;
                            }
                            if this.compaction.ready().is_none() {
                                this.compaction = Load::Failed;
                            }
                        }
                    }
                    cx.notify();
                });
            }),
            cx.spawn(async move |this, cx| {
                let result = snapshot.await;
                let _ = this.update(cx, |this, cx| {
                    if this.read_revision != revision {
                        return;
                    }
                    match result {
                        Some(Ok(snapshot)) if this.warming_writes == warming_at_read => {
                            this.adopt(&snapshot)
                        }
                        Some(Ok(_)) => {}
                        // A server without per-session warming offers the rest.
                        _ if this.per_session == PerSession::Loading => {
                            this.per_session = PerSession::Unavailable;
                        }
                        _ => {}
                    }
                    cx.notify();
                });
            }),
        ];
        cx.notify();
    }

    fn adopt(&mut self, snapshot: &WarmingSnapshot) {
        self.per_session = PerSession::Ready {
            enabled: snapshot.enabled,
            minutes: snapshot.minutes,
        };
    }

    /// The mode shown, `None` while either source is still unknown.
    fn mode(&self) -> Option<WarmingMode> {
        if let Some(mode) = self.pending_mode {
            return Some(mode);
        }
        let global = self.global.ready()?;
        if global.on {
            return Some(WarmingMode::All);
        }
        match self.per_session {
            PerSession::Loading => None,
            PerSession::Ready { enabled: true, .. } => Some(WarmingMode::Session),
            _ => Some(WarmingMode::Off),
        }
    }

    fn minutes(&self) -> Option<u32> {
        self.pending_minutes.or(match self.mode() {
            Some(WarmingMode::All) => self.global.ready().and_then(|global| global.minutes),
            _ => match self.per_session {
                PerSession::Ready { minutes, .. } => Some(minutes),
                _ => None,
            },
        })
    }

    fn set_mode(&mut self, mode: WarmingMode, cx: &mut Context<Self>) {
        if self.writes.closing() {
            return;
        }
        let (Some(current), Some(target)) = (self.mode(), self.write_target(cx)) else {
            return;
        };
        let unavailable = self.per_session == PerSession::Unavailable;
        if mode == current || (mode == WarmingMode::Session && unavailable) {
            return;
        }
        self.pending_mode = Some(mode);
        self.warming_writes += 1;
        let write = self.warming_writes;
        self.latest_mode_write = write;
        let client = target.client.clone();
        let (id, writing) = self.writes.submit(&target, async move {
            if unavailable {
                client
                    .save_opencode_warming(mode == WarmingMode::All)
                    .await
                    .map(|()| None)
            } else {
                client.set_warming_mode(mode.wire()).await.map(Some)
            }
        });
        let completion = cx.spawn(async move |this, cx| {
            let result = writing.await.ok();
            let _ = this.update(cx, |this, cx| {
                this.writes.finish(id);
                if !this.owns(&target, cx) {
                    return;
                }
                match result {
                    Some(Ok(snapshot)) => {
                        if write >= this.settled_warming_write {
                            this.settled_warming_write = write;
                            if let Some(snapshot) = snapshot {
                                this.adopt(&snapshot);
                            }
                            // Turning OpenCode's warming on keeps a duration it had.
                            let minutes = this
                                .global
                                .ready()
                                .and_then(|global| global.minutes)
                                .or(Some(OPENCODE_DEFAULT_MINUTES));
                            this.global = Load::Ready(GlobalWarming {
                                on: mode == WarmingMode::All,
                                minutes,
                            });
                        }
                        if this.latest_mode_write == write {
                            this.failed_fields.remove(&WriteField::Mode);
                        }
                    }
                    _ if this.latest_mode_write == write => {
                        this.failed_fields.insert(WriteField::Mode);
                    }
                    _ => {}
                }
                // Only the newest write settles the chips.
                if this.latest_mode_write == write {
                    this.pending_mode = None;
                }
                this.failed = !this.failed_fields.is_empty();
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
        cx.notify();
    }

    fn set_minutes(&mut self, minutes: u32, _: &mut Window, cx: &mut Context<Self>) {
        if self.writes.closing() {
            return;
        }
        let (Some(mode), Some(target)) = (self.mode(), self.write_target(cx)) else {
            return;
        };
        let scope = match mode {
            WarmingMode::All => "all",
            WarmingMode::Session => "session",
            WarmingMode::Off => return,
        };
        if Some(minutes) == self.minutes() {
            return;
        }
        self.pending_minutes = Some(minutes);
        self.warming_writes += 1;
        let write = self.warming_writes;
        self.latest_minutes_write = write;
        let client = target.client.clone();
        let (id, writing) = self.writes.submit(&target, async move {
            client.set_warming_duration(scope, minutes).await
        });
        let completion = cx.spawn(async move |this, cx| {
            let result = writing.await.ok();
            let _ = this.update(cx, |this, cx| {
                this.writes.finish(id);
                if !this.owns(&target, cx) {
                    return;
                }
                match result {
                    Some(Ok(snapshot)) => {
                        if write >= this.settled_warming_write {
                            this.settled_warming_write = write;
                            this.adopt(&snapshot);
                            if scope == "all" {
                                this.global = Load::Ready(GlobalWarming {
                                    on: true,
                                    minutes: Some(minutes),
                                });
                            }
                        }
                        if this.latest_minutes_write == write {
                            this.failed_fields.remove(&WriteField::Minutes);
                        }
                    }
                    _ if this.latest_minutes_write == write => {
                        this.failed_fields.insert(WriteField::Minutes);
                    }
                    _ => {}
                }
                if this.latest_minutes_write == write {
                    this.pending_minutes = None;
                }
                this.failed = !this.failed_fields.is_empty();
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
        cx.notify();
    }

    /// Writes one field; the value shows at once and goes back if the write fails.
    fn save_compaction(&mut self, change: CompactionChange, cx: &mut Context<Self>) {
        if self.writes.closing() {
            return;
        }
        let Some(target) = self.write_target(cx) else {
            return;
        };
        let Some(values) = self.compaction.ready_mut() else {
            return;
        };
        let mut next = values.clone();
        apply(&mut next, &change);
        if next == *values {
            return;
        }
        *values = next;
        self.compaction_writes += 1;
        let write = self.compaction_writes;
        let field = field_of(&change);
        self.latest_field_write.insert(field, write);
        let client = target.client.clone();
        let sent = change.clone();
        let (id, writing) = self
            .writes
            .submit(&target, async move { client.save_compaction(&sent).await });
        let completion = cx.spawn(async move |this, cx| {
            let result = writing.await.ok();
            let _ = this.update(cx, |this, cx| {
                this.writes.finish(id);
                if !this.owns(&target, cx) {
                    return;
                }
                if matches!(result, Some(Ok(()))) {
                    if this.settled_field_write.get(&field).copied().unwrap_or(0) <= write
                        && let Some(saved) = &mut this.saved
                    {
                        apply(saved, &change);
                        this.settled_field_write.insert(field, write);
                    }
                    if this.failed_fields.contains(&WriteField::Compaction(field))
                        && let (Some(saved), Some(values)) =
                            (this.saved.clone(), this.compaction.ready_mut())
                    {
                        restore(values, &saved, field);
                    }
                    if this.latest_field_write.get(&field) == Some(&write) {
                        this.failed_fields.remove(&WriteField::Compaction(field));
                    }
                } else {
                    if this.latest_field_write.get(&field) == Some(&write)
                        && let (Some(saved), Some(values)) =
                            (this.saved.clone(), this.compaction.ready_mut())
                    {
                        restore(values, &saved, field);
                        this.failed_fields.insert(WriteField::Compaction(field));
                    }
                }
                this.failed = !this.failed_fields.is_empty();
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
        cx.notify();
    }

    fn owns(&self, target: &ServerTarget, cx: &gpui_kit::App) -> bool {
        same_target(self.target.as_ref(), Some(target))
            && same_target(
                ServerTarget::capture(&self.owner, self.network.as_ref(), cx).as_ref(),
                Some(target),
            )
    }

    fn write_target(&mut self, cx: &mut Context<Self>) -> Option<ServerTarget> {
        let target = self.target.clone()?;
        if !self.owns(&target, cx) {
            self.refresh(cx);
            return None;
        }
        Some(target)
    }

    pub(crate) fn take_pending_write(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<tokio::sync::oneshot::Receiver<()>> {
        let pending = self.writes.take_pending_write();
        cx.notify();
        pending
    }

    pub(crate) fn has_failed_writes(&self) -> bool {
        self.failed
    }

    pub(crate) fn cancel_close(&mut self, cx: &mut Context<Self>) {
        self.writes.cancel_close();
        cx.notify();
    }

    pub(crate) fn take_quit_write(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        self.writes.take_quit_write(self.network.take(), cx)
    }

    /// Search targets this section owns.
    pub(crate) fn reveal(&mut self, item: &str, window: &mut Window, cx: &mut Context<Self>) {
        let picker = match item {
            "sessions.warming" => Some(&self.duration_picker),
            "sessions.compaction-buffer" => Some(&self.buffer_picker),
            "sessions.compaction-keep" => Some(&self.keep_picker),
            _ => None,
        };
        if let Some(picker) = picker {
            picker.update(cx, |picker, cx| picker.focus(window, cx));
        } else if let Some(focus) = self.focus.get(item) {
            window.focus(focus, cx);
        }
    }

    /// Gives a picker its options and selection when either changed.
    fn sync_picker(
        &mut self,
        id: &'static str,
        options: Vec<ValueOption>,
        selected: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let applied = (options.clone(), selected.clone());
        if self.picker_values.get(id) == Some(&applied) {
            return;
        }
        self.picker_values.insert(id, applied);
        let picker = match id {
            "duration" => self.duration_picker.clone(),
            "buffer" => self.buffer_picker.clone(),
            _ => self.keep_picker.clone(),
        };
        picker.update(cx, |picker, cx| {
            picker.set_items(SearchableVec::new(options), window, cx);
            match &selected {
                Some(value) => picker.set_selected_value(value, window, cx),
                None => picker.set_selected_index(None, window, cx),
            }
        });
    }

    fn chips(&mut self, mode: Option<WarmingMode>, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let unavailable = self.per_session == PerSession::Unavailable;
        let mut group = div()
            .id("sessions-warming-group")
            .role(Role::Group)
            .aria_label(strings.get("settings.openchamber.defaults.sessionWarming.aria"))
            .flex_row_from(direction)
            .items_center()
            .gap(px(2.0))
            .p(px(2.0))
            .h(gpui_kit::rems(2.0))
            .rounded(px(7.0))
            .bg(palette.elevated);
        for (choice, key) in [
            (
                WarmingMode::Off,
                "settings.openchamber.defaults.sessionWarming.off",
            ),
            (
                WarmingMode::All,
                "settings.openchamber.defaults.sessionWarming.all",
            ),
            (
                WarmingMode::Session,
                "settings.openchamber.defaults.sessionWarming.session",
            ),
        ] {
            let disabled = self.writes.closing()
                || mode.is_none()
                || (choice == WarmingMode::Session && unavailable);
            let selected = mode.unwrap_or(WarmingMode::Off) == choice;
            let id: SharedString = format!("sessions-warming-{}", choice.wire()).into();
            let focus = focus_handle(&mut self.focus, &id, cx);
            let chip = div()
                .id(id.clone())
                .accessibility_id(id)
                .role(Role::Button)
                .aria_label(strings.get(key))
                .aria_toggled(if selected {
                    Toggled::True
                } else {
                    Toggled::False
                })
                .when(!disabled, |chip| chip.track_focus(&focus).tab_index(0))
                .h_full()
                .px(px(10.0))
                .rounded(px(5.0))
                .flex()
                .items_center()
                .when(selected, |chip| {
                    chip.bg(palette.background).font_weight(FontWeight::MEDIUM)
                })
                .when(disabled, |chip| {
                    chip.opacity(0.5).text_color(palette.muted_foreground)
                })
                .when(!disabled && !selected, |chip| chip.cursor_pointer())
                .child(strings.get(key));
            group = group.child(if disabled {
                chip.test_support().into_any_element()
            } else {
                on_activate(chip, cx, move |this, _, cx| this.set_mode(choice, cx))
                    .test_support()
                    .into_any_element()
            });
        }
        group.into_any_element()
    }

    fn select(
        picker: &Picker,
        id: &'static str,
        label: SharedString,
        disabled: bool,
    ) -> AnyElement {
        div()
            .key_context("SettingsPicker")
            .w(px(200.0))
            .max_w_full()
            .h(gpui_kit::rems(2.0))
            .child(
                Select::new(picker)
                    .id(id)
                    .accessibility_label(label)
                    .disabled(disabled)
                    .w_full()
                    .h(gpui_kit::rems(2.0)),
            )
            .into_any_element()
    }
}

impl Render for SessionDefaults {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let tokens = |count: u64| -> SharedString {
            strings.format(
                "settings.openchamber.defaults.compaction.tokens",
                &[("count", &strings.number(&count.to_string()))],
            )
        };
        // Prompt cache.
        let mode = self.mode();
        let minutes = self.minutes();
        let durations = WARMING_STEPS
            .iter()
            .map(|step| ValueOption {
                value: step.to_string(),
                label: strings.get(&format!(
                    "settings.openchamber.defaults.sessionWarming.duration.{step}"
                )),
            })
            .collect();
        // A hand-tuned OpenCode duration outside the steps shows no step.
        let step = minutes
            .filter(|minutes| WARMING_STEPS.contains(minutes))
            .map(|minutes| minutes.to_string());
        self.sync_picker("duration", durations, step, window, cx);
        let duration_disabled = matches!(mode, None | Some(WarmingMode::Off))
            || self.per_session == PerSession::Unavailable
            || self.writes.closing();
        let chips = self.chips(mode, cx);
        let warming_controls = div()
            .flex_row_from(strings.direction())
            .flex_wrap()
            .items_center()
            .gap(px(8.0))
            .child(chips)
            .child(Self::select(
                &self.duration_picker,
                "sessions-warming-duration",
                strings.get("settings.openchamber.defaults.sessionWarming.durationAria"),
                duration_disabled,
            ))
            .into_any_element();
        let warming_row = match self.global {
            Load::Failed => load_failed(
                "sessions-config-load-failed",
                strings.get("settings.openchamber.desktopNetwork.error.loadFailed"),
                strings.get("desktopHostSwitcher.actions.retry"),
                cx,
                |this, cx| this.refresh(cx),
            ),
            _ => field_row(
                "sessions.warming",
                strings.get("settings.openchamber.defaults.field.sessionWarming"),
                Some(strings.get("settings.openchamber.defaults.sessionWarming.info")),
                warming_controls,
                None,
                cx,
            ),
        };

        // Compaction.
        let loading = self.compaction.ready().is_none() || self.writes.closing();
        let values = self.compaction.ready().cloned().unwrap_or(Compaction {
            auto: true,
            buffer: None,
            keep_tokens: None,
            background: false,
        });
        let auto_off = !values.auto;
        let mut keep_steps = KEEP_STEPS.to_vec();
        if let Some(value) = values.keep_tokens
            && !keep_steps.contains(&value)
        {
            keep_steps.push(value);
            keep_steps.sort_unstable();
        }
        let keep_default = strings.format(
            "settings.openchamber.defaults.compaction.keepDefault",
            &[(
                "count",
                &strings.number(&OPENCODE_DEFAULT_KEEP_TOKENS.to_string()),
            )],
        );
        let keep_options = std::iter::once(ValueOption {
            value: DEFAULT_OPTION.into(),
            label: keep_default,
        })
        .chain(keep_steps.into_iter().map(|count| ValueOption {
            value: count.to_string(),
            label: tokens(count),
        }))
        .collect();
        let keep_selected = Some(
            values
                .keep_tokens
                .map_or(DEFAULT_OPTION.to_owned(), |count| count.to_string()),
        );
        self.sync_picker("keep", keep_options, keep_selected, window, cx);
        let mut buffer_steps: Vec<String> = BUFFER_PERCENT_STEPS
            .iter()
            .map(|percent| format!("{percent}%"))
            .collect();
        if let Some(buffer) = &values.buffer
            && !buffer_steps.contains(&buffer_value(buffer))
        {
            buffer_steps.push(buffer_value(buffer));
        }
        let buffer_options = std::iter::once(ValueOption {
            value: DEFAULT_OPTION.into(),
            label: strings.get("settings.openchamber.defaults.compaction.bufferDefault"),
        })
        .chain(buffer_steps.into_iter().map(|value| {
            let label = if value.ends_with('%') {
                strings.number(&value).into()
            } else {
                tokens(value.parse().unwrap_or_default())
            };
            ValueOption { value, label }
        }))
        .collect();
        let buffer_selected = Some(
            values
                .buffer
                .as_ref()
                .map_or(DEFAULT_OPTION.to_owned(), buffer_value),
        );
        self.sync_picker("buffer", buffer_options, buffer_selected, window, cx);
        let keep = field_row(
            "sessions.compaction-keep",
            strings.get("settings.openchamber.defaults.compaction.keep"),
            Some(strings.get("settings.openchamber.defaults.compaction.keepInfo")),
            Self::select(
                &self.keep_picker,
                "sessions-compaction-keep",
                strings.get("settings.openchamber.defaults.compaction.keep"),
                loading,
            ),
            None,
            cx,
        );
        let auto_id: SharedString = "sessions.compaction-auto".into();
        let auto_focus = focus_handle(&mut self.focus, &auto_id, cx);
        let auto = div()
            .flex_row_from(strings.direction())
            .items_start()
            .gap(px(6.0))
            .child(checkbox_row(
                "sessions-compaction-auto".into(),
                strings.get("settings.openchamber.defaults.compaction.auto"),
                strings.get("settings.openchamber.defaults.compaction.auto"),
                values.auto,
                !loading,
                &auto_focus,
                None,
                cx,
                |this: &mut Self, value, _, cx| {
                    this.save_compaction(CompactionChange::Auto(value), cx)
                },
            ))
            .child(crate::settings_controls::info_hint(
                "sessions.compaction-auto",
                strings.get("settings.openchamber.defaults.compaction.autoInfo"),
                cx,
            ))
            .into_any_element();
        let buffer = field_row(
            "sessions.compaction-buffer",
            strings.get("settings.openchamber.defaults.compaction.buffer"),
            Some(strings.get("settings.openchamber.defaults.compaction.bufferInfo")),
            Self::select(
                &self.buffer_picker,
                "sessions-compaction-buffer",
                strings.get("settings.openchamber.defaults.compaction.buffer"),
                loading || auto_off,
            ),
            None,
            cx,
        );
        let background_id: SharedString = "sessions.background-compaction".into();
        let background_focus = focus_handle(&mut self.focus, &background_id, cx);
        let background = div()
            .flex_row_from(strings.direction())
            .items_start()
            .gap(px(6.0))
            .child(checkbox_row(
                "sessions-background-compaction".into(),
                strings.get("settings.openchamber.defaults.compaction.background"),
                strings.get("settings.openchamber.defaults.compaction.background"),
                values.background,
                !loading && !auto_off,
                &background_focus,
                None,
                cx,
                |this: &mut Self, value, _, cx| {
                    this.save_compaction(CompactionChange::Background(value), cx)
                },
            ))
            .child(crate::settings_controls::info_hint(
                "sessions.background-compaction",
                strings.get("settings.openchamber.defaults.compaction.backgroundInfo"),
                cx,
            ))
            .into_any_element();
        let nested = div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .padding_start(strings.direction(), px(24.0))
            .child(buffer)
            .child(background)
            .into_any_element();
        let direction = strings.direction();
        div()
            .id("sessions-opencode-sections")
            .child(section_with_info(
                "settings.openchamber.defaults.promptCacheTitle",
                None,
                false,
                vec![warming_row],
                None,
                cx,
            ))
            .child(section_with_info(
                "settings.openchamber.defaults.compaction.title",
                None,
                false,
                vec![
                    helper(
                        strings.get("settings.openchamber.defaults.compaction.projectNote"),
                        cx,
                    ),
                    keep,
                    auto,
                    nested,
                ],
                None,
                cx,
            ))
            .when(self.failed, |view| {
                view.child(
                    div()
                        .id("sessions-opencode-save-failed")
                        .accessibility_id("sessions-opencode-save-failed")
                        .role(Role::Status)
                        .mt(px(12.0))
                        .flex_row_from(direction)
                        .text_color(palette.status_error)
                        .child(strings.get("settings.common.status.saveFailed")),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use gpui_kit::TestAppContext;
    use serde_json::json;

    use super::*;
    use crate::settings_navigation::Page;
    use crate::settings_test_server::{Server, open, settings, settle};

    /// OpenCode's config documents, a warming snapshot (or none) and the
    /// write routes; a compaction write of `auto` fails.
    fn server(per_session: bool) -> (Server, Arc<Mutex<Value>>) {
        let compaction = Arc::new(Mutex::new(json!({"keep": {"tokens": 30000}})));
        let held = Arc::clone(&compaction);
        let server = Server::new(move |request| {
            let snapshot = |enabled: bool, minutes: u32| json!({"revision": 1, "enabled": enabled, "minutes": minutes, "sessions": []});
            if request.is("GET", "/api/config") {
                return (
                    200,
                    json!([
                        {"type": "document", "info": {"warming": false}},
                        {"type": "document", "info": {"compaction": held.lock().expect("config").clone()}},
                    ]),
                );
            }
            if request.target.starts_with("/api/session-warming") && !per_session {
                return (404, json!({"error": "no per-session warming"}));
            }
            if request.is("GET", "/api/session-warming") {
                return (200, snapshot(true, 60));
            }
            if request.is("PUT", "/api/session-warming/mode") {
                return (200, snapshot(request.body["mode"] == "session", 60));
            }
            if request.is("PUT", "/api/session-warming/duration") {
                return (
                    200,
                    snapshot(true, request.body["minutes"].as_u64().unwrap_or(60) as u32),
                );
            }
            if request.is("PUT", "/api/config/warming") {
                return (200, json!({"success": true}));
            }
            if request.is("PUT", "/api/config/compaction") {
                if request.body.get("auto").is_some() {
                    return (500, json!({"error": "fixture write failure"}));
                }
                return (200, json!({"success": true}));
            }
            if request.is("GET", "/api/config/settings") {
                return (200, json!({}));
            }
            (404, json!({"error": "not in this fixture"}))
        });
        (server, compaction)
    }

    fn defaults(workspace: &Entity<Workspace>, cx: &TestAppContext) -> Entity<SessionDefaults> {
        let settings = settings(workspace, cx);
        cx.read(|cx| {
            settings
                .read(cx)
                .sessions
                .read(cx)
                .defaults
                .clone()
                .expect("the Sessions page has its OpenCode sections")
        })
    }

    #[gpui_kit::test]
    fn review_regression_a_warming_write_cannot_change_a_later_server(cx: &mut TestAppContext) {
        use crate::settings_controls::tests::{ResponseGate, retarget};
        let gate = ResponseGate::new();
        let held = Arc::clone(&gate);
        let first = Server::new(move |request| {
            if request.is("GET", "/api/config") {
                return (200, json!([]));
            }
            if request.is("PUT", "/api/session-warming/mode") {
                held.hold();
            }
            if request.target.starts_with("/api/session-warming") {
                return (
                    200,
                    json!({"revision": 1, "enabled": true, "minutes": 180, "sessions": []}),
                );
            }
            (200, json!({}))
        });
        let (second, _) = server(true);
        let (window, workspace) = open(&first, Page::Sessions, cx);
        let page = defaults(&workspace, cx);
        page.update(cx, |page, cx| page.set_mode(WarmingMode::All, cx));
        gate.entered(cx);
        retarget(&workspace, &second, cx);
        settle(window, cx);
        gate.release();
        settle(window, cx);
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.mode(), Some(WarmingMode::Session));
            assert_eq!(page.minutes(), Some(60));
        });
    }

    #[gpui_kit::test]
    fn delayed_duration_and_compaction_results_stay_with_their_connection(cx: &mut TestAppContext) {
        use crate::settings_controls::tests::{ResponseGate, retarget};
        for duration in [true, false] {
            let gate = ResponseGate::new();
            let held = Arc::clone(&gate);
            let first = Server::new(move |request| {
                if request.is("GET", "/api/config") {
                    return (200, json!([]));
                }
                if request.method == "PUT" {
                    held.hold();
                }
                if request.target.starts_with("/api/session-warming") {
                    return (
                        200,
                        json!({"revision": 1, "enabled": true, "minutes": 180, "sessions": []}),
                    );
                }
                (200, json!({}))
            });
            let second = Server::new(|request| {
                if request.is("GET", "/api/config") {
                    return (
                        200,
                        json!([{"type": "document", "info": {"compaction": {"keep": {"tokens": 30000}}}}]),
                    );
                }
                if request.is("PUT", "/api/config/compaction") {
                    return (500, json!({}));
                }
                if request.target.starts_with("/api/session-warming") {
                    return (
                        200,
                        json!({"revision": 1, "enabled": true, "minutes": 60, "sessions": []}),
                    );
                }
                (200, json!({}))
            });
            let (window, workspace) = open(&first, Page::Sessions, cx);
            let page = defaults(&workspace, cx);
            cx.update_window(window, |_, window, cx| {
                page.update(cx, |page, cx| {
                    if duration {
                        page.set_minutes(120, window, cx);
                    } else {
                        page.save_compaction(CompactionChange::KeepTokens(Some(50_000)), cx);
                    }
                });
            })
            .expect("test window");
            gate.entered(cx);
            retarget(&workspace, &second, cx);
            settle(window, cx);
            gate.release();
            settle(window, cx);
            page.update(cx, |page, cx| {
                page.save_compaction(CompactionChange::KeepTokens(Some(100_000)), cx)
            });
            settle(window, cx);
            cx.read(|cx| {
                let page = page.read(cx);
                assert_eq!(
                    page.minutes(),
                    Some(60),
                    "an old duration never replaces B's value"
                );
                assert_eq!(
                    page.compaction.ready().expect("B compaction").keep_tokens,
                    Some(30_000),
                    "a failed B edit rolls back to B's own acknowledged value"
                );
            });
        }
    }

    #[gpui_kit::test]
    fn app_quit_drains_every_accepted_session_default_write(cx: &mut TestAppContext) {
        use crate::settings_controls::tests::ResponseGate;
        use std::sync::atomic::{AtomicBool, Ordering};
        let gate = ResponseGate::new();
        let held = Arc::clone(&gate);
        let first = AtomicBool::new(true);
        let server = Server::new(move |request| {
            if request.is("GET", "/api/config") {
                return (200, json!([]));
            }
            if request.method == "PUT" && first.swap(false, Ordering::SeqCst) {
                held.hold();
            }
            if request.target.starts_with("/api/session-warming") {
                return (
                    200,
                    json!({"revision": 1, "enabled": true, "minutes": 60, "sessions": []}),
                );
            }
            (200, json!({}))
        });
        let (window, workspace) = open(&server, Page::Sessions, cx);
        let page = defaults(&workspace, cx);
        page.update(cx, |page, cx| page.set_mode(WarmingMode::All, cx));
        gate.entered(cx);
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                page.set_minutes(120, window, cx);
                page.save_compaction(CompactionChange::KeepTokens(Some(50_000)), cx);
            });
        })
        .expect("test window");
        let settings = settings(&workspace, cx);
        let _normal_close = settings.update(cx, |settings, cx| settings.take_pending_write(cx));
        cx.update(|cx| {
            cx.on_app_quit(move |cx| {
                let pending = settings.update(cx, |settings, cx| settings.take_quit_write(cx));
                gate.release();
                async move {
                    if let Some(pending) = pending {
                        pending.await;
                    }
                }
            })
            .detach();
        });
        cx.executor().allow_parking();
        cx.quit();
        let writes = server.writes();
        assert_eq!(
            writes.len(),
            3,
            "quit drains all accepted settings writes after the foreground session stops"
        );
        assert!(writes[0].is("PUT", "/api/session-warming/mode"));
        assert!(writes[1].is("PUT", "/api/session-warming/duration"));
        assert!(writes[2].is("PUT", "/api/config/compaction"));
    }

    #[gpui_kit::test]
    fn compaction_writes_one_field_and_a_failed_write_puts_it_back(cx: &mut TestAppContext) {
        let (server, _) = server(true);
        let (window, workspace) = open(&server, Page::Sessions, cx);
        let sections = defaults(&workspace, cx);
        cx.read(|cx| {
            let page = sections.read(cx);
            let values = page.compaction.ready().expect("compaction read");
            assert_eq!(values.keep_tokens, Some(30_000));
            assert!(values.auto, "absent auto reads as on");
            assert_eq!(page.mode(), Some(WarmingMode::Session));
            assert_eq!(page.minutes(), Some(60));
        });
        sections.update(cx, |page, cx| {
            page.save_compaction(CompactionChange::KeepTokens(Some(50_000)), cx);
            page.save_compaction(CompactionChange::Auto(false), cx);
        });
        cx.read(|cx| {
            let values = sections
                .read(cx)
                .compaction
                .ready()
                .cloned()
                .expect("values");
            assert!(!values.auto, "a choice shows before its write answers");
        });
        settle(window, cx);
        let writes: Vec<Value> = server
            .writes()
            .into_iter()
            .filter(|write| write.is("PUT", "/api/config/compaction"))
            .map(|write| write.body)
            .collect();
        assert_eq!(
            writes,
            vec![json!({"keepTokens": 50000}), json!({"auto": false})]
        );
        cx.read(|cx| {
            let page = sections.read(cx);
            let values = page.compaction.ready().expect("values");
            assert_eq!(values.keep_tokens, Some(50_000));
            assert!(values.auto, "the failed field goes back to the saved value");
            assert!(page.failed, "the failure is shown");
        });
    }

    #[gpui_kit::test]
    fn warming_modes_go_through_the_server_or_opencode_alone(cx: &mut TestAppContext) {
        let (current, _) = server(true);
        let (window, workspace) = open(&current, Page::Sessions, cx);
        let sections = defaults(&workspace, cx);
        sections.update(cx, |page, cx| page.set_mode(WarmingMode::All, cx));
        settle(window, cx);
        assert!(current.writes().iter().any(|write| {
            write.is("PUT", "/api/session-warming/mode") && write.body == json!({"mode": "all"})
        }));
        cx.read(|cx| assert_eq!(sections.read(cx).mode(), Some(WarmingMode::All)));

        let (older, _) = server(false);
        let (window, workspace) = open(&older, Page::Sessions, cx);
        let sections = defaults(&workspace, cx);
        cx.read(|cx| {
            let page = sections.read(cx);
            assert_eq!(page.per_session, PerSession::Unavailable);
            assert_eq!(page.mode(), Some(WarmingMode::Off));
        });
        sections.update(cx, |page, cx| page.set_mode(WarmingMode::Session, cx));
        sections.update(cx, |page, cx| page.set_mode(WarmingMode::All, cx));
        settle(window, cx);
        let writes = older.writes();
        assert_eq!(
            writes.len(),
            1,
            "chosen sessions are not offered without the server"
        );
        assert!(writes[0].is("PUT", "/api/config/warming"));
        assert_eq!(writes[0].body, json!({"enabled": true}));
    }

    #[test]
    fn opencode_warming_reads_its_switch_and_duration() {
        let config = |value: Value| -> EffectiveConfig {
            json!({ "warming": value })
                .as_object()
                .expect("object")
                .clone()
        };
        assert_eq!(
            global_warming(&EffectiveConfig::new()),
            GlobalWarming {
                on: false,
                minutes: None
            }
        );
        assert_eq!(
            global_warming(&config(json!(true))).minutes,
            Some(OPENCODE_DEFAULT_MINUTES)
        );
        assert_eq!(
            global_warming(&config(json!({"duration": "1800000 millis"}))).minutes,
            Some(30)
        );
        assert_eq!(
            global_warming(&config(json!({"duration": "2 hours"}))).minutes,
            Some(120)
        );
        assert_eq!(
            global_warming(&config(json!({"duration": "soon"}))),
            GlobalWarming {
                on: true,
                minutes: None
            },
            "an unreadable duration shows no step"
        );
    }
}
