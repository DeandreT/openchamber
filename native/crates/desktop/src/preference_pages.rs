//! Settings' Chat and Sessions pages: the reference controls whose features
//! native implements. Values, writes and failures live in `GeneralSettings`;
//! code line wrap keeps the workspace's own writer, shared with code cards.

use std::collections::HashMap;

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    AnyElement, App, Context, Entity, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, Role, ScrollAnchor, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Subscription, TestSupportExt, Toggled, WeakEntity, Window,
    div, prelude::FluentBuilder, px,
};
use openchamber_native_core::message_queue::FollowUpBehavior;
use openchamber_native_core::permission_mode::PermissionMode;

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::code_card::CodeWrap;
use crate::general_settings::{
    Choice, DELETION_DIALOG, DRAFT_STARTERS, FOLLOW_UP, GeneralSettings, PERMISSION_DEFAULT,
    PROMPT_NAVIGATOR, SHOW_CHATS, WORK_ENABLED,
};
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::settings_controls::{field_row, info_hint, section};
use crate::settings_navigation::Page;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// The Chat page's tabs, kept while Settings stays open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ChatTab {
    #[default]
    Messages,
    Composer,
    Assistance,
}

impl ChatTab {
    const ALL: [Self; 3] = [Self::Messages, Self::Composer, Self::Assistance];

    fn label(self) -> &'static str {
        match self {
            Self::Messages => "settings.layout.chat.tab.messages",
            Self::Composer => "settings.layout.chat.tab.composer",
            Self::Assistance => "settings.layout.chat.tab.assistance",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Messages => "messages",
            Self::Composer => "composer",
            Self::Assistance => "assistance",
        }
    }

    /// The tab that holds a search target (`chatSettingsTabForSearchTarget`).
    pub(crate) fn for_search_target(item: &str) -> Option<Self> {
        match item {
            "chat.prompt-navigator" | "chat.code-block-line-wrap" => Some(Self::Messages),
            "chat.follow-up-behavior" => Some(Self::Composer),
            "chat.draft-starters-visible" => Some(Self::Assistance),
            _ => None,
        }
    }
}

/// The search target each control answers to, and its focus target.
const CONTROLS: [&str; 8] = [
    "chat.prompt-navigator",
    "chat.code-block-line-wrap",
    "chat.follow-up-behavior",
    "chat.draft-starters-visible",
    "sessions.permission-default",
    "sessions.show-chats",
    "sessions.work-enabled",
    "sessions.deletion-dialog",
];

const FOLLOW_UPS: [(FollowUpBehavior, &str, &str); 2] = [
    (
        FollowUpBehavior::Steer,
        "steer",
        "settings.openchamber.visual.option.followUpBehavior.steer.label",
    ),
    (
        FollowUpBehavior::Queue,
        "queue",
        "settings.openchamber.visual.option.followUpBehavior.queue.label",
    ),
];

const MODES: [(PermissionMode, &str, &str, &str); 3] = [
    (
        PermissionMode::Ask,
        "ask",
        "settings.sessions.permissions.mode.ask",
        "shield-user",
    ),
    (
        PermissionMode::Safety,
        "safety",
        "settings.sessions.permissions.mode.safety",
        "shield-star",
    ),
    (
        PermissionMode::Auto,
        "auto",
        "settings.sessions.permissions.mode.auto",
        "shield-check",
    ),
];

/// One chip of a short exclusive choice.
struct Chip {
    id: SharedString,
    label: SharedString,
    icon: Option<(&'static str, gpui_kit::Hsla)>,
    tooltip: Option<SharedString>,
    selected: bool,
    disabled: bool,
    choice: Choice,
}

/// One boolean setting: its search target, text keys, value and writer.
struct CheckboxRow {
    item: &'static str,
    label: &'static str,
    aria: &'static str,
    info: Option<&'static str>,
    checked: bool,
    enabled: bool,
    on_change: fn(&mut PreferencePage, bool, &mut Context<PreferencePage>),
}

pub(crate) struct PreferencePage {
    owner: WeakEntity<Workspace>,
    general: Entity<GeneralSettings>,
    page: Page,
    tab: ChatTab,
    scroll: ScrollHandle,
    anchors: HashMap<&'static str, ScrollAnchor>,
    focus: HashMap<SharedString, FocusHandle>,
    _subscriptions: Vec<Subscription>,
}

impl PreferencePage {
    pub(crate) fn new(
        owner: WeakEntity<Workspace>,
        general: Entity<GeneralSettings>,
        page: Page,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = vec![cx.observe(&general, |_, _, cx| cx.notify())];
        if let Some(workspace) = owner.upgrade() {
            subscriptions.push(cx.observe(&workspace, |_, _, cx| cx.notify()));
        }
        let scroll = ScrollHandle::new();
        let anchors = CONTROLS
            .iter()
            .map(|item| (*item, ScrollAnchor::for_handle(scroll.clone())))
            .collect();
        Self {
            owner,
            general,
            page,
            tab: ChatTab::default(),
            scroll,
            anchors,
            focus: HashMap::new(),
            _subscriptions: subscriptions,
        }
    }

    fn focus_handle(&mut self, id: &SharedString, cx: &mut Context<Self>) -> FocusHandle {
        self.focus
            .entry(id.clone())
            .or_insert_with(|| cx.focus_handle().tab_index(0).tab_stop(true))
            .clone()
    }

    /// Opens a search target: its tab first, then scroll and focus once laid out.
    pub(crate) fn reveal(&mut self, item: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = CONTROLS.iter().copied().find(|control| *control == item) else {
            return;
        };
        if let Some(tab) = ChatTab::for_search_target(item) {
            self.tab = tab;
        }
        cx.notify();
        cx.on_next_frame(window, move |this, window, cx| {
            if let Some(anchor) = this.anchors.get(item) {
                anchor.scroll_to(window, cx);
            }
            let focus = this.focus_target(item, cx);
            if let Some(focus) = this.focus.get(&focus) {
                window.focus(focus, cx);
            }
        });
    }

    /// A chip group focuses its selected chip, as a radio group does.
    fn focus_target(&self, item: &str, cx: &App) -> SharedString {
        let general = self.general.read(cx);
        match item {
            "chat.follow-up-behavior" => {
                let Choice::FollowUp(current) = general.current_choice(FOLLOW_UP) else {
                    unreachable!("the follow-up field holds a follow-up choice");
                };
                let name = FOLLOW_UPS
                    .iter()
                    .find(|(behavior, ..)| *behavior == current)
                    .map_or("queue", |(_, name, _)| *name);
                format!("{item}-{name}").into()
            }
            "sessions.permission-default" => {
                let name = MODES
                    .iter()
                    .find(|(mode, ..)| *mode == self.shown_mode(cx))
                    .map_or("ask", |(_, name, ..)| *name);
                format!("{item}-{name}").into()
            }
            _ => item.into(),
        }
    }

    fn safety_available(&self, cx: &App) -> bool {
        self.owner
            .upgrade()
            .is_some_and(|owner| owner.read(cx).send_catalog.safety_net)
    }

    /// A stored Safety reads as Ask while the safety net cannot run.
    fn shown_mode(&self, cx: &App) -> PermissionMode {
        let Choice::Mode(mode) = self.general.read(cx).current_choice(PERMISSION_DEFAULT) else {
            unreachable!("the permission field holds a mode");
        };
        mode.displayed(self.safety_available(cx))
    }

    fn change(&mut self, field: usize, choice: Choice, cx: &mut Context<Self>) {
        self.general
            .update(cx, |general, cx| general.change(field, choice, cx));
    }

    fn set_code_wrap(&mut self, wrap: bool, cx: &mut Context<Self>) {
        if let Some(owner) = self.owner.upgrade() {
            owner.update(cx, |owner, cx| owner.set_code_wrap(wrap, cx));
        }
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        self.general.update(cx, |general, cx| general.retry(cx));
        if self.page == Page::Chat
            && let Some(owner) = self.owner.upgrade()
        {
            owner.update(cx, |owner, cx| owner.retry_code_wrap(cx));
        }
    }

    fn select_tab(&mut self, tab: ChatTab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.scroll.set_offset(gpui_kit::point(px(0.0), px(0.0)));
        cx.notify();
    }

    fn checkbox(&mut self, row: CheckboxRow, cx: &mut Context<Self>) -> AnyElement {
        let CheckboxRow {
            item,
            label,
            aria,
            info,
            checked,
            enabled,
            on_change,
        } = row;
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let focus = self.focus_handle(&item.into(), cx);
        let weak = cx.weak_entity();
        let id: SharedString = item.replace('.', "-").into();
        div()
            .flex_row_from(strings.direction())
            .items_start()
            .gap(px(6.0))
            .child(
                gpui_kit::base::Checkbox::new(id.clone())
                    .accessibility_id(id)
                    .accessibility_label(strings.get(aria))
                    .checked(checked)
                    .disabled(!enabled)
                    .track_focus(&focus)
                    .anchor_scroll(self.anchors.get(item).cloned())
                    .flex_row_from(strings.direction())
                    .items_start()
                    .gap(px(8.0))
                    .py(px(5.0))
                    .when(!enabled, |row| row.opacity(0.5))
                    .on_change(move |state, _, _, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            on_change(this, state == gpui_kit::base::CheckboxState::Checked, cx)
                        });
                    })
                    .child(glyph(
                        if checked {
                            "checkbox"
                        } else {
                            "checkbox-blank"
                        },
                        16.0,
                        palette.foreground,
                    ))
                    .child(div().min_w_0().child(strings.get(label))),
            )
            .children(info.map(|info| info_hint(item, strings.get(info), cx)))
            .into_any_element()
    }

    fn chips(
        &mut self,
        item: &'static str,
        label: &'static str,
        info: Option<SharedString>,
        chips: Vec<Chip>,
        field: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let hover = palette.hover;
        let focus: Vec<FocusHandle> = chips
            .iter()
            .map(|chip| self.focus_handle(&chip.id, cx))
            .collect();
        let enabled: Vec<usize> = chips
            .iter()
            .enumerate()
            .filter(|(_, chip)| !chip.disabled)
            .map(|(index, _)| index)
            .collect();
        let choices: Vec<Choice> = chips.iter().map(|chip| chip.choice.clone()).collect();
        let mut group = div()
            .id(SharedString::from(format!(
                "{}-group",
                item.replace('.', "-")
            )))
            .role(Role::Group)
            .aria_label(strings.get(label))
            .flex_row_from(direction)
            .items_center()
            .gap(px(2.0))
            .p(px(2.0))
            .h(gpui_kit::rems(2.0))
            .rounded(px(7.0))
            .bg(palette.elevated);
        for (index, chip) in chips.into_iter().enumerate() {
            let choice = chip.choice.clone();
            let disabled = chip.disabled;
            let enabled = enabled.clone();
            let choices = choices.clone();
            let focus_handles = focus.clone();
            let mut element = div()
                .id(chip.id.clone())
                .accessibility_id(chip.id.clone())
                .role(Role::Button)
                .aria_label(chip.label.clone())
                .aria_toggled(if chip.selected {
                    Toggled::True
                } else {
                    Toggled::False
                })
                // A disabled chip takes no focus or activation.
                .when(!disabled, |chip| {
                    chip.track_focus(&focus[index]).tab_index(0)
                })
                .h_full()
                .px(px(10.0))
                .rounded(px(5.0))
                .flex_row_from(direction)
                .items_center()
                .gap(px(6.0))
                .when(chip.selected, |chip| {
                    chip.bg(palette.background).font_weight(FontWeight::MEDIUM)
                })
                .when(disabled, |chip| {
                    chip.opacity(0.5).text_color(palette.muted_foreground)
                })
                .when(!disabled && !chip.selected, |chip| {
                    chip.cursor_pointer().hover(move |style| style.bg(hover))
                })
                .children(chip.icon.map(|(icon, color)| glyph(icon, 14.0, color)))
                .child(chip.label.clone());
            if let Some(tooltip) = chip.tooltip.clone() {
                element = element
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx));
            }
            let element = if disabled {
                element.test_support().into_any_element()
            } else {
                on_activate(element, cx, move |this, _, cx| {
                    this.change(field, choice.clone(), cx)
                })
                .on_action(cx.listener({
                    let choice = chip.choice.clone();
                    move |this, action: &gpui_kit::base::actions::Confirm, _, cx| {
                        if !action.secondary {
                            this.change(field, choice.clone(), cx);
                            cx.stop_propagation();
                        }
                    }
                }))
                .on_key_down(cx.listener(
                    move |this, event: &gpui_kit::KeyDownEvent, window, cx| {
                        let key = &event.keystroke;
                        if key.modifiers.control || key.modifiers.alt || key.modifiers.platform {
                            return;
                        }
                        let step = match (key.key.as_str(), direction) {
                            ("right", Direction::LeftToRight)
                            | ("left", Direction::RightToLeft) => 1,
                            ("left", Direction::LeftToRight)
                            | ("right", Direction::RightToLeft) => -1,
                            _ => return,
                        };
                        let Some(position) = enabled.iter().position(|chip| *chip == index) else {
                            return;
                        };
                        let next = enabled[(position as isize + step)
                            .rem_euclid(enabled.len() as isize)
                            as usize];
                        this.change(field, choices[next].clone(), cx);
                        window.focus(&focus_handles[next], cx);
                        cx.stop_propagation();
                    },
                ))
                .test_support()
                .into_any_element()
            };
            group = group.child(element);
        }
        field_row(
            item,
            strings.get(label),
            info,
            group.into_any_element(),
            self.anchors.get(item).cloned(),
            cx,
        )
    }

    fn chat(&mut self, enabled: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let general = self.general.read(cx);
        let boolean = |field| matches!(general.current_choice(field), Choice::Boolean(true));
        let prompt_navigator = boolean(PROMPT_NAVIGATOR);
        let starters = boolean(DRAFT_STARTERS);
        let Choice::FollowUp(follow_up) = general.current_choice(FOLLOW_UP) else {
            unreachable!("the follow-up field holds a follow-up choice");
        };
        let wrap = cx.try_global::<CodeWrap>().copied().unwrap_or_default().0;
        let wrap_enabled = self
            .owner
            .upgrade()
            .is_some_and(|owner| !owner.read(cx).close_in_progress());
        let mut sections = vec![self.tabs(cx)];
        match self.tab {
            ChatTab::Messages => {
                let navigator = self.checkbox(
                    CheckboxRow {
                        item: "chat.prompt-navigator",
                        label: "settings.openchamber.visual.field.promptNavigatorEnabled",
                        aria: "settings.openchamber.visual.field.promptNavigatorEnabledAria",
                        info: None,
                        checked: prompt_navigator,
                        enabled,
                        on_change: |this, value, cx| {
                            this.change(PROMPT_NAVIGATOR, Choice::Boolean(value), cx)
                        },
                    },
                    cx,
                );
                sections.push(section(
                    "settings.layout.chat.section.reading",
                    true,
                    vec![navigator],
                    cx,
                ));
                let wrap = self.checkbox(
                    CheckboxRow {
                        item: "chat.code-block-line-wrap",
                        label: "settings.openchamber.visual.field.codeBlockLineWrap",
                        aria: "settings.openchamber.visual.field.codeBlockLineWrapAria",
                        info: None,
                        checked: wrap,
                        enabled: wrap_enabled,
                        on_change: |this, value, cx| this.set_code_wrap(value, cx),
                    },
                    cx,
                );
                sections.push(section(
                    "settings.layout.chat.section.formatting",
                    false,
                    vec![wrap],
                    cx,
                ));
            }
            ChatTab::Composer => {
                let strings = locale::strings(cx).clone();
                let chips = FOLLOW_UPS
                    .iter()
                    .map(|(behavior, name, label)| Chip {
                        id: format!("chat.follow-up-behavior-{name}").into(),
                        label: strings.get(label),
                        icon: None,
                        tooltip: None,
                        selected: *behavior == follow_up,
                        disabled: !enabled,
                        choice: Choice::FollowUp(*behavior),
                    })
                    .collect();
                let row = self.chips(
                    "chat.follow-up-behavior",
                    "settings.openchamber.visual.section.followUpBehavior",
                    None,
                    chips,
                    FOLLOW_UP,
                    cx,
                );
                sections.push(section(
                    "settings.layout.chat.section.sending",
                    true,
                    vec![row],
                    cx,
                ));
            }
            ChatTab::Assistance => {
                let row = self.checkbox(
                    CheckboxRow {
                        item: "chat.draft-starters-visible",
                        label: "settings.openchamber.visual.field.draftStartersVisible",
                        aria: "settings.openchamber.visual.field.draftStartersVisibleAria",
                        info: None,
                        checked: starters,
                        enabled,
                        on_change: |this, value, cx| {
                            this.change(DRAFT_STARTERS, Choice::Boolean(value), cx)
                        },
                    },
                    cx,
                );
                sections.push(section(
                    "settings.openchamber.visual.section.sessionAssistance",
                    true,
                    vec![row],
                    cx,
                ));
            }
        }
        sections
    }

    fn sessions(&mut self, enabled: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let general = self.general.read(cx);
        let boolean = |field| matches!(general.current_choice(field), Choice::Boolean(true));
        let show_chats = boolean(SHOW_CHATS);
        let work = boolean(WORK_ENABLED);
        let deletion = boolean(DELETION_DIALOG);
        let safety = self.safety_available(cx);
        let shown = self.shown_mode(cx);
        let explain = |mode: PermissionMode| match mode {
            PermissionMode::Safety if !safety => {
                "settings.sessions.permissions.explain.safetyUnavailable"
            }
            PermissionMode::Safety => "settings.sessions.permissions.explain.safety",
            PermissionMode::Auto => "settings.sessions.permissions.explain.auto",
            PermissionMode::Ask => "settings.sessions.permissions.explain.ask",
        };
        let chips = MODES
            .iter()
            .map(|(mode, name, label, icon)| Chip {
                id: format!("sessions.permission-default-{name}").into(),
                label: strings.get(label),
                icon: Some((
                    icon,
                    match mode {
                        PermissionMode::Ask => palette.foreground,
                        PermissionMode::Safety => palette.status_success,
                        PermissionMode::Auto => palette.status_info,
                    },
                )),
                tooltip: Some(strings.get(explain(*mode))),
                selected: *mode == shown,
                disabled: !enabled || (*mode == PermissionMode::Safety && !safety),
                choice: Choice::Mode(*mode),
            })
            .collect();
        let info: SharedString = format!(
            "{}\n\n{}",
            strings.get(explain(shown)),
            strings.get("settings.sessions.permissions.defaultModeInfo")
        )
        .into();
        let permission = self.chips(
            "sessions.permission-default",
            "settings.sessions.permissions.defaultMode",
            Some(info),
            chips,
            PERMISSION_DEFAULT,
            cx,
        );
        let show_chats = self.checkbox(
            CheckboxRow {
                item: "sessions.show-chats",
                label: "settings.openchamber.defaults.field.showChats",
                aria: "settings.openchamber.defaults.field.showChats",
                info: None,
                checked: show_chats,
                enabled,
                on_change: |this, value, cx| this.change(SHOW_CHATS, Choice::Boolean(value), cx),
            },
            cx,
        );
        let work = self.checkbox(
            CheckboxRow {
                item: "sessions.work-enabled",
                label: "settings.openchamber.sessionWork.field.enabled",
                aria: "settings.openchamber.sessionWork.field.enabled",
                info: Some("settings.openchamber.sessionWork.info"),
                checked: work,
                enabled,
                on_change: |this, value, cx| this.change(WORK_ENABLED, Choice::Boolean(value), cx),
            },
            cx,
        );
        let deletion = self.checkbox(
            CheckboxRow {
                item: "sessions.deletion-dialog",
                label: "settings.openchamber.defaults.field.showDeletionDialog",
                aria: "settings.openchamber.defaults.field.showDeletionDialogAria",
                info: None,
                checked: deletion,
                enabled,
                on_change: |this, value, cx| {
                    this.change(DELETION_DIALOG, Choice::Boolean(value), cx)
                },
            },
            cx,
        );
        vec![
            section(
                "settings.openchamber.defaults.newSessionsTitle",
                true,
                vec![permission],
                cx,
            ),
            section(
                "settings.layout.sessions.sidebar.title",
                false,
                vec![show_chats, work],
                cx,
            ),
            section(
                "settings.openchamber.sessionRetention.title",
                false,
                vec![deletion],
                cx,
            ),
        ]
    }

    fn tabs(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let hover = palette.hover;
        let mut strip = div()
            .id("chat-settings-tabs")
            .role(Role::TabList)
            .mb(px(32.0))
            .max_w(px(448.0))
            .h(gpui_kit::rems(2.0))
            .p(px(2.0))
            .gap(px(2.0))
            .rounded(px(7.0))
            .bg(palette.elevated)
            .flex_row_from(direction);
        for tab in ChatTab::ALL {
            let id: SharedString = format!("chat-settings-tab-{}", tab.id()).into();
            let focus = self.focus_handle(&id, cx);
            let selected = self.tab == tab;
            strip = strip.child(
                on_activate(
                    div()
                        .id(id.clone())
                        .accessibility_id(id)
                        .role(Role::Tab)
                        .aria_label(strings.get(tab.label()))
                        .aria_selected(selected)
                        .track_focus(&focus)
                        .tab_index(0)
                        .flex_1()
                        .h_full()
                        .rounded(px(5.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .when(selected, |tab| {
                            tab.bg(palette.background).font_weight(FontWeight::MEDIUM)
                        })
                        .when(!selected, |tab| tab.hover(move |style| style.bg(hover)))
                        .child(strings.get(tab.label())),
                    cx,
                    move |this, _, cx| this.select_tab(tab, cx),
                )
                .on_action(cx.listener(
                    move |this, action: &gpui_kit::base::actions::Confirm, _, cx| {
                        if !action.secondary {
                            this.select_tab(tab, cx);
                            cx.stop_propagation();
                        }
                    },
                ))
                .test_support(),
            );
        }
        strip.into_any_element()
    }

    fn status(&self, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let general = self.general.read(cx);
        let (mut key, mut retryable) = general.status(false);
        let closing = general.is_closing();
        let wrap_failed = self.page == Page::Chat
            && self
                .owner
                .upgrade()
                .is_some_and(|owner| owner.read(cx).code_wrap_save_failed());
        if wrap_failed && !retryable {
            key = Some("settings.common.status.saveFailed");
            retryable = true;
        }
        let prefix = self.page.id();
        let retry_id: SharedString = format!("{prefix}-retry").into();
        let retry = on_activate(
            gpui_kit::base::Button::new(retry_id.clone())
                .disabled(closing)
                .accessibility_id(retry_id)
                .accessibility_label(strings.get("desktopHostSwitcher.actions.retry"))
                .h(gpui_kit::rems(1.75))
                .px(px(8.0))
                .rounded(px(7.0))
                .child(strings.get("desktopHostSwitcher.actions.retry")),
            cx,
            |this, _, cx| this.retry(cx),
        );
        div()
            .id(SharedString::from(format!("{prefix}-save-status")))
            .accessibility_id(format!("{prefix}-save-status"))
            .role(Role::Status)
            .aria_label(key.map(|key| strings.get(key)).unwrap_or_default())
            .h(px(36.0))
            .mt(px(16.0))
            .flex_row_from(strings.direction())
            .items_center()
            .gap(px(8.0))
            .text_color(palette.muted_foreground)
            .children(key.map(|key| strings.get(key)))
            .when(retryable, |view| view.child(retry))
            .into_any_element()
    }
}

impl Render for PreferencePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled = self.general.read(cx).editable();
        let sections = match self.page {
            Page::Chat => self.chat(enabled, cx),
            _ => self.sessions(enabled, cx),
        };
        let status = self.status(cx);
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        div()
            .id(SharedString::from(format!(
                "{}-settings-page",
                self.page.id()
            )))
            .role(Role::Group)
            .aria_label(strings.get(self.page.title()))
            .size_full()
            .min_w_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px(px(48.0))
            .py(px(32.0))
            .css_text(0.875, 1.25, cx)
            .text_color(palette.foreground)
            .when(strings.direction() == Direction::RightToLeft, |view| {
                view.text_right()
            })
            .child(crate::settings_view::page_header(self.page, cx))
            .children(sections)
            .child(status)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, AppContext as _, TestAppContext};
    use openchamber_native_transport::client::{ServerClient, ServerSettings};
    use openchamber_native_transport::credential::Credential;
    use openchamber_native_transport::endpoint::Endpoint;
    use serde_json::{Value, json};

    use super::*;
    use crate::connection::{Connection, ServerConfig};

    /// A loopback settings document: reads return it, writes merge into it
    /// unless `fail_writes` is set.
    struct Server {
        endpoint: Endpoint,
        address: std::net::SocketAddr,
        saved: Arc<Mutex<serde_json::Map<String, Value>>>,
        writes: Arc<Mutex<Vec<Value>>>,
        fail_writes: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl Server {
        fn new(initial: Value) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("owned loopback listener");
            let address = listener.local_addr().expect("listener address");
            let saved = Arc::new(Mutex::new(
                initial.as_object().expect("settings object").clone(),
            ));
            let writes = Arc::new(Mutex::new(Vec::new()));
            let fail_writes = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let (document, recorded, failing, stopped) = (
                Arc::clone(&saved),
                Arc::clone(&writes),
                Arc::clone(&fail_writes),
                Arc::clone(&stop),
            );
            let worker = thread::spawn(move || {
                for stream in listener.incoming() {
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(mut stream) = stream else { continue };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                    let Some((method, target, body)) = read_request(&mut stream) else {
                        continue;
                    };
                    let (status, response) = if !target.starts_with("/api/config/settings") {
                        (404, json!({"error": "not in this fixture"}))
                    } else if method == "PUT" {
                        recorded.lock().expect("writes").push(body.clone());
                        if failing.load(Ordering::Acquire) {
                            (500, json!({"error": "fixture write failure"}))
                        } else {
                            let mut saved = document.lock().expect("saved");
                            saved.extend(body.as_object().expect("settings object").clone());
                            (200, Value::Object(saved.clone()))
                        }
                    } else {
                        (200, Value::Object(document.lock().expect("saved").clone()))
                    };
                    let response = response.to_string();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                        response.len()
                    );
                }
            });
            Self {
                endpoint: Endpoint::parse(&format!("http://{address}")).expect("endpoint"),
                address,
                saved,
                writes,
                fail_writes,
                stop,
                worker: Some(worker),
            }
        }

        fn writes(&self) -> Vec<Value> {
            self.writes.lock().expect("writes").clone()
        }

        fn saved(&self, key: &str) -> Option<Value> {
            self.saved.lock().expect("saved").get(key).cloned()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            let _ = TcpStream::connect(self.address);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn read_request(stream: &mut TcpStream) -> Option<(String, String, Value)> {
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).ok()?;
            headers.push(byte[0]);
        }
        let headers = String::from_utf8(headers).ok()?;
        let mut line = headers.lines().next()?.split_whitespace();
        let method = line.next()?.to_owned();
        let target = line.next()?.to_owned();
        let length: usize = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse().ok())
            .unwrap_or(0);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).ok()?;
        let body = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).ok()?
        };
        Some((method, target, body))
    }

    /// A workspace connected to `server`, with Settings open on `page`.
    fn open(
        server: &Server,
        page: Page,
        safety_net: bool,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<Workspace>) {
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let mut connection =
                    Connection::new(ServerConfig::for_endpoint(server.endpoint.clone()))
                        .expect("network");
                connection.client = Some(
                    ServerClient::new(server.endpoint.clone(), Credential::None)
                        .expect("fixture client"),
                );
                workspace.connection = Some(connection);
                workspace.send_catalog.settings = Some(ServerSettings::default());
                workspace.send_catalog.safety_net = safety_net;
                workspace.show_settings(Some(page), window, cx);
            });
        })
        .expect("test window");
        settle(window, cx);
        (window, workspace)
    }

    /// Lets loopback requests finish and their completions render.
    fn settle(window: AnyWindowHandle, cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        for _ in 0..20 {
            cx.run_until_parked();
            thread::sleep(Duration::from_millis(10));
        }
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .expect("test window");
    }

    fn click(window: AnyWindowHandle, id: &'static str, cx: &mut TestAppContext) {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        })
        .expect("test window");
        settle(window, cx);
    }

    fn settings(workspace: &Entity<Workspace>, cx: &TestAppContext) -> ServerSettings {
        cx.read(|cx| {
            workspace
                .read(cx)
                .send_catalog
                .settings
                .clone()
                .expect("workspace settings")
        })
    }

    fn status(workspace: &Entity<Workspace>, cx: &TestAppContext) -> Option<&'static str> {
        cx.read(|cx| {
            let settings = workspace
                .read(cx)
                .settings
                .clone()
                .expect("settings dialog");
            settings.read(cx).general.read(cx).status(false).0
        })
    }

    #[gpui_kit::test]
    fn sessions_page_reads_saves_and_applies_each_choice(cx: &mut TestAppContext) {
        let server = Server::new(json!({
            "permissionDefaultMode": "auto",
            "sidebarShowChatsSection": false,
        }));
        let (window, workspace) = open(&server, Page::Sessions, false, cx);
        cx.read(|cx| {
            let settings = workspace
                .read(cx)
                .settings
                .clone()
                .expect("settings dialog");
            let general = settings.read(cx).general.read(cx);
            assert_eq!(
                general.current_choice(PERMISSION_DEFAULT),
                Choice::Mode(PermissionMode::Auto),
                "the page shows the server's stored default"
            );
            assert_eq!(general.current_choice(SHOW_CHATS), Choice::Boolean(false));
        });

        click(window, "sessions-work-enabled", cx);
        click(window, "sessions-show-chats", cx);
        click(window, "sessions-deletion-dialog", cx);
        click(window, "sessions.permission-default-ask", cx);
        assert_eq!(
            server.writes(),
            vec![
                json!({"sessionWorkEnabled": false}),
                json!({"sidebarShowChatsSection": true}),
                json!({"showDeletionDialog": false}),
                json!({"permissionDefaultMode": "ask"}),
            ],
            "each choice writes only its own server field"
        );
        let applied = settings(&workspace, cx);
        assert_eq!(applied.work_enabled, Some(false));
        assert_eq!(applied.show_chats, Some(true));
        assert_eq!(applied.show_deletion_dialog, Some(false));
        assert_eq!(applied.permission_default_mode, Some(PermissionMode::Ask));
        assert!(
            cx.read(|cx| !workspace.read(cx).shows_deletion_dialog()),
            "archive and delete follow the saved choice"
        );
        assert_eq!(status(&workspace, cx), None, "success is silent");
    }

    #[gpui_kit::test]
    fn safety_net_default_is_offered_only_while_it_can_run(cx: &mut TestAppContext) {
        let server = Server::new(json!({"permissionDefaultMode": "safety"}));
        let (window, workspace) = open(&server, Page::Sessions, false, cx);
        let sessions = cx.read(|cx| {
            workspace
                .read(cx)
                .settings
                .clone()
                .expect("settings dialog")
                .read(cx)
                .sessions
                .clone()
        });
        assert_eq!(
            cx.read(|cx| sessions.read(cx).shown_mode(cx)),
            PermissionMode::Ask,
            "a stored Safety reads as Ask while the safety net cannot run"
        );
        click(window, "sessions.permission-default-safety", cx);
        assert!(
            server.writes().is_empty(),
            "the disabled chip saves nothing"
        );

        cx.update(|cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.send_catalog.safety_net = true;
                cx.notify();
            })
        });
        settle(window, cx);
        assert_eq!(
            cx.read(|cx| sessions.read(cx).shown_mode(cx)),
            PermissionMode::Safety
        );
        click(window, "sessions.permission-default-auto", cx);
        click(window, "sessions.permission-default-safety", cx);
        assert_eq!(
            server.writes(),
            vec![
                json!({"permissionDefaultMode": "auto"}),
                json!({"permissionDefaultMode": "safety"}),
            ]
        );
    }

    #[gpui_kit::test]
    fn chat_page_keeps_a_failed_choice_applied_until_retry_saves_it(cx: &mut TestAppContext) {
        let server = Server::new(json!({}));
        let (window, workspace) = open(&server, Page::Chat, false, cx);
        server.fail_writes.store(true, Ordering::Release);
        click(window, "chat-prompt-navigator", cx);
        assert_eq!(
            settings(&workspace, cx).prompt_navigator_enabled,
            Some(false),
            "the live app keeps the choice whose save failed"
        );
        assert_eq!(
            status(&workspace, cx),
            Some("settings.common.status.saveFailed")
        );
        assert!(cx.read(|cx| {
            let settings = workspace
                .read(cx)
                .settings
                .clone()
                .expect("settings dialog");
            settings.read(cx).general.read(cx).has_failed_writes()
        }));

        // A newer server read must not replace the unconfirmed choice.
        let mut reread = ServerSettings {
            prompt_navigator_enabled: Some(true),
            ..ServerSettings::default()
        };
        cx.read(|cx| {
            workspace
                .read(cx)
                .overlay_unsettled_settings(&mut reread, cx)
        });
        assert_eq!(reread.prompt_navigator_enabled, Some(false));

        server.fail_writes.store(false, Ordering::Release);
        click(window, "chat-retry", cx);
        assert_eq!(server.saved("promptNavigatorEnabled"), Some(json!(false)));
        assert_eq!(status(&workspace, cx), None, "retry clears the failure");
        assert_eq!(
            server.writes().len(),
            2,
            "retry sends the failed choice once"
        );
    }

    #[gpui_kit::test]
    fn chat_tabs_hold_their_controls_and_search_opens_the_right_tab(cx: &mut TestAppContext) {
        let server = Server::new(json!({"followUpBehavior": "queue"}));
        let (window, workspace) = open(&server, Page::Chat, false, cx);
        let present = |id: &'static str, cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.try_find(id).is_some()
            })
            .expect("test window")
        };
        assert!(present("chat-prompt-navigator", cx));
        assert!(!present("chat.follow-up-behavior-steer", cx));

        click(window, "chat-settings-tab-composer", cx);
        assert!(present("chat.follow-up-behavior-steer", cx));
        click(window, "chat.follow-up-behavior-steer", cx);
        assert_eq!(server.writes(), vec![json!({"followUpBehavior": "steer"})]);
        assert_eq!(
            cx.read(|cx| workspace.read(cx).follow_up_behavior()),
            FollowUpBehavior::Steer,
            "Enter routing follows the new choice"
        );

        let chat = cx.read(|cx| {
            workspace
                .read(cx)
                .settings
                .clone()
                .expect("settings dialog")
                .read(cx)
                .chat
                .clone()
        });
        cx.update_window(window, |_, window, cx| {
            chat.update(cx, |page, cx| {
                page.reveal("chat.draft-starters-visible", window, cx)
            });
        })
        .expect("test window");
        settle(window, cx);
        assert!(present("chat-draft-starters-visible", cx));
        click(window, "chat-draft-starters-visible", cx);
        assert_eq!(settings(&workspace, cx).draft_starters_visible, Some(false));
        assert!(cx.read(|cx| !workspace.read(cx).starters_visible()));
    }

    #[gpui_kit::test]
    fn code_wrap_uses_the_shared_writer_and_reports_its_failure(cx: &mut TestAppContext) {
        let server = Server::new(json!({"codeBlockLineWrap": true}));
        let (window, workspace) = open(&server, Page::Chat, false, cx);
        let before = cx.read(|cx| cx.try_global::<CodeWrap>().copied().unwrap_or_default().0);
        server.fail_writes.store(true, Ordering::Release);
        click(window, "chat-code-block-line-wrap", cx);
        assert_eq!(
            cx.read(|cx| cx.try_global::<CodeWrap>().copied().unwrap_or_default().0),
            !before,
            "code cards follow the choice at once"
        );
        assert_eq!(server.writes(), vec![json!({"codeBlockLineWrap": !before})]);
        assert!(cx.read(|cx| workspace.read(cx).code_wrap_save_failed()));
        server.fail_writes.store(false, Ordering::Release);
        click(window, "chat-retry", cx);
        assert_eq!(server.saved("codeBlockLineWrap"), Some(json!(!before)));
        assert!(cx.read(|cx| !workspace.read(cx).code_wrap_save_failed()));
    }

    #[gpui_kit::test]
    fn a_newer_never_ask_toggle_wins_over_a_failed_settings_choice(cx: &mut TestAppContext) {
        let server = Server::new(json!({"showDeletionDialog": true}));
        let (window, workspace) = open(&server, Page::Sessions, false, cx);
        server.fail_writes.store(true, Ordering::Release);
        click(window, "sessions-deletion-dialog", cx);
        assert_eq!(settings(&workspace, cx).show_deletion_dialog, Some(false));

        server.fail_writes.store(false, Ordering::Release);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_deletion_dialog(window, cx)
            });
        })
        .expect("test window");
        settle(window, cx);
        assert_eq!(server.saved("showDeletionDialog"), Some(json!(true)));
        assert_eq!(
            settings(&workspace, cx).show_deletion_dialog,
            Some(true),
            "the dialog's confirmed choice is newer than the failed one"
        );
    }
}
