//! Draws the scheduled tasks page, its task cards and the task editor.
//! Every control dispatches to `ScheduledTasks` or `TaskEditor`; nothing
//! here decides what the server stored.

use chrono::{Local, TimeZone as _, Timelike};
use gpui_kit::component::WindowExt;
use gpui_kit::component::dialog::Dialog;
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::{
    AnyElement, App, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Render,
    Role, SharedString, StatefulInteractiveElement, Styled, TestSupportExt, WeakEntity, Window,
    div, prelude::FluentBuilder, px, rems,
};
use openchamber_native_core::message_time::{LocalDate, LocalDateTime};
use openchamber_native_core::schedule::{
    self, CRON_EXAMPLES, CronExpression, RunStatus, ScheduleKind, SessionMode, StatusTone,
    UNKNOWN_DATE,
};
use openchamber_native_core::time_zone;
use openchamber_native_transport::scheduled_tasks::{LoopApproval, ScheduledTask};

use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::panel_controls::{
    HEADER_LINE, HEADER_SIZE, LABEL_LINE, LABEL_SIZE, META_LINE, META_SIZE, MICRO_LINE, MICRO_SIZE,
    Variant, button, checkbox, option_row, popup, select_trigger, text,
};
use crate::scheduled_tasks::{EditorMenu, Eligibility, ScheduledTasks, TaskEditor, TaskList};
use crate::theme::Palette;
use crate::workspace::Workspace;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// `formatTimeForPreference`: a local clock time in the user's clock.
pub(crate) fn clock_time<V>(at_ms: i64, cx: &Context<V>) -> String {
    let Some(moment) = Local.timestamp_millis_opt(at_ms).single() else {
        return String::new();
    };
    let date = moment.date_naive();
    let clock = cx
        .try_global::<crate::scheduled_tasks_view::ClockSetting>()
        .map(|setting| setting.0)
        .unwrap_or_default();
    locale::strings(cx).dates().time(
        &LocalDateTime {
            date: LocalDate {
                year: chrono::Datelike::year(&date),
                month: chrono::Datelike::month(&date),
                day: chrono::Datelike::day(&date),
            },
            hour: moment.hour(),
            minute: moment.minute(),
        },
        clock,
    )
}

/// The user's clock preference, mirrored from settings for these views.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ClockSetting(pub openchamber_native_core::message_time::ClockPreference);

impl gpui_kit::Global for ClockSetting {}

fn label(strings: &Strings, label: &schedule::Label) -> SharedString {
    let params: Vec<(&str, String)> = label
        .params
        .iter()
        .map(|(name, value)| {
            let value = if value == UNKNOWN_DATE {
                strings
                    .get("sessions.scheduledTasks.dialog.schedule.unknownDate")
                    .to_string()
            } else {
                value.clone()
            };
            (*name, value)
        })
        .collect();
    let borrowed: Vec<(&str, &str)> = params
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    strings.format(label.key, &borrowed)
}

/// "Nov 9, 9:00 AM" on the zone's own clock, for the cron preview.
fn zoned_moment(strings: &Strings, zone: &time_zone::TimeZone, at: i64, cx: &App) -> String {
    let local = at + i64::from(zone.offset_at(at));
    let (year, month, day) = time_zone::civil_from_days(local.div_euclid(86_400));
    let seconds = local.rem_euclid(86_400);
    let clock = cx
        .try_global::<ClockSetting>()
        .map(|setting| setting.0)
        .unwrap_or_default();
    let time = strings.dates().time(
        &LocalDateTime {
            date: LocalDate {
                year: year as i32,
                month,
                day,
            },
            hour: (seconds / 3600) as u32,
            minute: ((seconds % 3600) / 60) as u32,
        },
        clock,
    );
    let months = &strings.dates().months;
    let name = months
        .get(month.saturating_sub(1) as usize)
        .map_or("", String::as_str);
    format!("{name} {day}, {time}")
}

fn tone_color(tone: StatusTone, palette: &Palette) -> gpui_kit::Hsla {
    match tone {
        StatusTone::Success => palette.status_success,
        StatusTone::Error => palette.status_error,
        StatusTone::Warning => palette.status_warning,
        StatusTone::Muted => palette.muted_foreground,
    }
}

/// The delete confirmation for one task.
pub(crate) fn delete_dialog(
    dialog: Dialog,
    description: SharedString,
    page: WeakEntity<ScheduledTasks>,
    task_id: String,
    cx: &mut App,
) -> Dialog {
    let palette = cx.global::<Palette>().clone();
    let strings = locale::strings(cx).clone();
    let direction = strings.direction();
    let small = |id: &'static str, label: SharedString, destructive: bool| {
        div()
            .id(id)
            .role(Role::Button)
            .accessibility_id(id)
            .aria_label(label.clone())
            .h(px(32.0))
            .px(px(12.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .cursor_pointer()
            .text_color(if destructive {
                palette.status_error
            } else {
                palette.foreground
            })
            .when(destructive, |this| this.bg(palette.status_error_background))
            .when(!destructive, |this| {
                this.border_1().border_color(palette.border)
            })
            .child(div().text_size(rems(LABEL_SIZE)).child(label))
            .test_support()
    };
    let confirm = page.clone();
    dialog
        .w(px(384.0))
        .close_button(false)
        .child(
            div()
                .text_size(rems(LABEL_SIZE))
                .text_color(palette.muted_foreground)
                .child(description),
        )
        .footer(
            div()
                .w_full()
                .flex_row_from(direction)
                .justify_end()
                .gap(px(8.0))
                .child(
                    small(
                        "scheduled-task-delete-cancel",
                        strings.get("sessions.scheduledTasks.editor.actions.cancel"),
                        false,
                    )
                    .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child(
                    small(
                        "scheduled-task-delete-confirm",
                        strings.get("sessions.sidebar.bulkActions.delete"),
                        true,
                    )
                    .on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        let id = task_id.clone();
                        let _ = confirm.update(cx, |page, cx| page.delete(&id, window, cx));
                    }),
                ),
        )
}

impl Workspace {
    pub(crate) fn render_scheduled_tasks(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let page = self.scheduled_tasks.clone()?;
        if page.read(cx).runtime() != &self.runtime {
            return None;
        }
        let clock = self
            .send_catalog
            .settings
            .as_ref()
            .map(|settings| settings.clock)
            .unwrap_or_default();
        cx.set_global(ClockSetting(clock));
        Some(page.into_any_element())
    }
}

impl ScheduledTasks {
    fn render_scope(
        &self,
        id: &str,
        label: SharedString,
        chats: bool,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.selected.as_deref() == Some(id);
        let hover = palette.hover;
        let scope = id.to_owned();
        let element_id = format!("scheduled-tasks-scope:{id}");
        crate::activation::on_activate(
            div()
                .id(SharedString::from(element_id.clone()))
                .role(Role::Tab)
                .accessibility_id(element_id)
                .aria_label(label.clone())
                .aria_selected(selected)
                .w_full()
                .min_w_0()
                .px(px(8.0))
                .py(px(6.0))
                .rounded(px(6.0))
                .flex_row_from(direction)
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .when(selected, |this| this.bg(palette.selection))
                .when(!selected, |this| this.hover(move |style| style.bg(hover)))
                .child(glyph(
                    if chats { "chat-4" } else { "folder" },
                    14.0,
                    palette.muted_foreground,
                ))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .css_text(LABEL_SIZE, LABEL_LINE, cx)
                        .text_color(if selected {
                            palette.foreground
                        } else {
                            palette.muted_foreground
                        })
                        .child(label),
                ),
            cx,
            move |this: &mut Self, _, cx| this.select_scope(Some(scope.clone()), cx),
        )
        .test_support()
        .into_any_element()
    }

    fn render_task(
        &self,
        task: &ScheduledTask,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.mutating.as_deref() == Some(task.id.as_str());
        let dim = if task.enabled { 1.0 } else { 0.6 };
        let summary = label(
            strings,
            &schedule::schedule_label(&task.schedule, |day| {
                strings.get(schedule::weekday_short_key(day)).to_string()
            }),
        );
        let model = if task.execution.use_defaults
            || task.execution.provider_id.is_none()
            || task.execution.model_id.is_none()
        {
            strings.get("sessions.scheduledTasks.dialog.usesDefaults")
        } else {
            let mut parts = vec![task.execution.model_id.clone().unwrap_or_default()];
            if let Some(variant) = task.execution.variant.as_ref() {
                parts.push(variant.clone());
            }
            let mut line = parts.join(" · ");
            if let Some(agent) = task.execution.agent.as_ref() {
                line.push_str(" · ");
                line.push_str(agent);
            }
            line.into()
        };
        let status = task.state.last_status;
        let (tone, icon) = status.look();
        let now = now_ms();
        let id = task.id.clone();
        let meta = (MICRO_SIZE, MICRO_LINE);
        let next = match task.state.next_run_at {
            Some(at) => format!(
                "{} · {}",
                label(strings, &schedule::relative_label(at, now)),
                clock_time(at, cx)
            ),
            None => "\u{2014}".to_owned(),
        };
        let last: AnyElement = if status == RunStatus::Running {
            div()
                .flex_row_from(direction)
                .items_center()
                .gap(px(4.0))
                .child(glyph("loader-4", 14.0, palette.status_warning))
                .child(text(
                    strings.get("sessions.scheduledTasks.dialog.lastRun.runningNow"),
                    meta,
                    palette.status_warning,
                    cx,
                ))
                .into_any_element()
        } else if let Some(at) = task.state.last_run_at {
            div()
                .flex_row_from(direction)
                .items_center()
                .gap(px(4.0))
                .when(tone != StatusTone::Muted, |this| {
                    this.child(glyph(icon, 14.0, tone_color(tone, palette)))
                        .child(text(
                            strings.get(status.label_key()),
                            meta,
                            tone_color(tone, palette),
                            cx,
                        ))
                        .child(text("·", meta, palette.muted_foreground, cx))
                })
                .child(text(
                    label(strings, &schedule::relative_label(at, now)),
                    meta,
                    palette.muted_foreground,
                    cx,
                ))
                .into_any_element()
        } else {
            text(
                strings.get("sessions.scheduledTasks.dialog.lastRun.never"),
                meta,
                palette.muted_foreground,
                cx,
            )
            .into_any_element()
        };
        let toggle_label = if task.enabled {
            strings.get("sessions.scheduledTasks.dialog.taskToggle.enabled")
        } else {
            strings.get("sessions.scheduledTasks.dialog.taskToggle.paused")
        };
        let element_id = format!("scheduled-task:{}", task.id);
        div()
            .id(SharedString::from(element_id.clone()))
            .role(Role::Group)
            .accessibility_id(element_id)
            .aria_label(SharedString::from(task.name.clone()))
            .test_support()
            .p(px(16.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(palette.border)
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .opacity(dim)
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .css_text(HEADER_SIZE, HEADER_LINE, cx)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(palette.foreground)
                            .child(task.name.clone()),
                    )
                    .child(text(summary, meta, palette.muted_foreground, cx).truncate())
                    .child(text(model, meta, palette.muted_foreground, cx).truncate())
                    .when_some(task.target_session_id.as_ref(), |this, target| {
                        let title = self
                            .owner_session_title(target, cx)
                            .unwrap_or_else(|| target.clone());
                        this.child(
                            text(
                                format!(
                                    "{}: {title}",
                                    strings.get("sessions.scheduledTasks.editor.targetSession")
                                ),
                                (META_SIZE, META_LINE),
                                palette.muted_foreground,
                                cx,
                            )
                            .truncate(),
                        )
                    })
                    .when_some(task.loop_file.as_ref(), |this, file| {
                        this.child(
                            text(
                                strings.format(
                                    "sessions.scheduledTasks.dialog.loopFile.note",
                                    &[("file", file)],
                                ),
                                meta,
                                palette.muted_foreground,
                                cx,
                            )
                            .truncate(),
                        )
                    }),
            )
            .child(
                div()
                    .opacity(dim)
                    .flex_row_from(direction)
                    .flex_wrap()
                    .items_center()
                    .gap(px(20.0))
                    .child(
                        div()
                            .id(SharedString::from(format!("scheduled-task-next:{id}")))
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(6.0))
                            .child(glyph("timer", 14.0, palette.muted_foreground))
                            .child(text(
                                strings.get("sessions.scheduledTasks.dialog.nextRun.label"),
                                meta,
                                palette.foreground,
                                cx,
                            ))
                            .child(text(next, meta, palette.muted_foreground, cx)),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("scheduled-task-last:{id}")))
                            .role(Role::Status)
                            .accessibility_id(format!("scheduled-task-last:{id}"))
                            .aria_label(strings.get(status.label_key()))
                            .test_support()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(6.0))
                            .child(glyph("history", 14.0, palette.muted_foreground))
                            .child(text(
                                strings.get("sessions.scheduledTasks.dialog.lastRun.label"),
                                meta,
                                palette.foreground,
                                cx,
                            ))
                            .child(last),
                    )
                    .when(task.state.last_run_isolated, |this| {
                        let text_value = match &task.state.last_run_branch {
                            Some(branch) => strings.format(
                                "sessions.scheduledTasks.dialog.lastRun.isolatedBranch",
                                &[("branch", branch)],
                            ),
                            None => strings.get("sessions.scheduledTasks.dialog.lastRun.isolated"),
                        };
                        this.child(
                            div()
                                .flex_row_from(direction)
                                .items_center()
                                .gap(px(6.0))
                                .child(glyph("git-branch", 14.0, palette.muted_foreground))
                                .child(text(text_value, meta, palette.muted_foreground, cx)),
                        )
                    }),
            )
            .when_some(task.state.last_error.as_ref(), |this, error| {
                let element_id = format!("scheduled-task-error:{id}");
                this.child(
                    div()
                        .id(SharedString::from(element_id.clone()))
                        .role(Role::Alert)
                        .accessibility_id(element_id)
                        .aria_label(SharedString::from(error.clone()))
                        .test_support()
                        .opacity(dim)
                        .p(px(8.0))
                        .rounded(px(6.0))
                        .bg(palette.status_error_background)
                        .flex_row_from(direction)
                        .gap(px(8.0))
                        .child(glyph("error-warning", 14.0, palette.status_error))
                        .child(text(error.clone(), meta, palette.status_error, cx)),
                )
            })
            .child(
                div()
                    .flex_row_from(direction)
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(4.0))
                            .child({
                                let id = id.clone();
                                checkbox(
                                    format!("scheduled-task-toggle:{id}"),
                                    toggle_label,
                                    task.enabled,
                                    busy,
                                    palette,
                                    direction,
                                    cx,
                                    move |this: &mut Self, window, cx| {
                                        this.toggle_enabled(&id, window, cx)
                                    },
                                )
                            })
                            .when_some(task.loop_approval, |this, reason| {
                                let (icon, key) = match reason {
                                    LoopApproval::Outdated => (
                                        "file-edit",
                                        "sessions.scheduledTasks.dialog.loopFile.changedSinceEnabled",
                                    ),
                                    LoopApproval::Required => (
                                        "git-branch",
                                        "sessions.scheduledTasks.dialog.loopFile.enableOnThisComputer",
                                    ),
                                };
                                this.child(
                                    div()
                                        .id(SharedString::from(format!("scheduled-task-loop:{id}")))
                                        .role(Role::Image)
                                        .aria_label(strings.get(key))
                                        .child(glyph(icon, 14.0, palette.muted_foreground)),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_row_from(direction)
                            .flex_wrap()
                            .items_center()
                            .gap(px(6.0))
                            .when(task.state.last_session_id.is_some(), |this| {
                                let id = id.clone();
                                this.child(button(
                                    format!("scheduled-task-open:{id}"),
                                    Some("chat-1"),
                                    Some(strings.get("sessions.scheduledTasks.dialog.actions.openSession")),
                                    strings.format(
                                        "sessions.scheduledTasks.dialog.actions.openSessionAria",
                                        &[("taskName", &task.name)],
                                    ),
                                    Variant::Ghost,
                                    false,
                                    palette,
                                    direction,
                                    cx,
                                    move |this: &mut Self, window, cx| {
                                        this.open_last_run(&id, window, cx)
                                    },
                                ))
                            })
                            .child({
                                let id = id.clone();
                                button(
                                    format!("scheduled-task-run:{id}"),
                                    Some("play"),
                                    Some(strings.get("sessions.scheduledTasks.dialog.actions.runNow")),
                                    strings.get("sessions.scheduledTasks.dialog.actions.runNow"),
                                    Variant::Outline,
                                    busy,
                                    palette,
                                    direction,
                                    cx,
                                    move |this: &mut Self, window, cx| this.run_now(&id, window, cx),
                                )
                            })
                            .child({
                                let id = id.clone();
                                button(
                                    format!("scheduled-task-edit:{id}"),
                                    Some("edit-2"),
                                    Some(strings.get("sessions.scheduledTasks.dialog.actions.edit")),
                                    strings.format(
                                        "sessions.scheduledTasks.dialog.actions.editAria",
                                        &[("taskName", &task.name)],
                                    ),
                                    Variant::Outline,
                                    busy || task.loop_file.is_some(),
                                    palette,
                                    direction,
                                    cx,
                                    move |this: &mut Self, window, cx| {
                                        this.open_editor(Some(&id), window, cx)
                                    },
                                )
                            })
                            .child({
                                let id = id.clone();
                                button(
                                    format!("scheduled-task-delete:{id}"),
                                    Some("delete-bin"),
                                    None,
                                    strings.format(
                                        "sessions.scheduledTasks.dialog.actions.deleteAria",
                                        &[("taskName", &task.name)],
                                    ),
                                    Variant::Destructive,
                                    busy,
                                    palette,
                                    direction,
                                    cx,
                                    move |this: &mut Self, window, cx| {
                                        this.request_delete(&id, window, cx)
                                    },
                                )
                            }),
                    ),
            )
            .into_any_element()
    }

    fn owner_session_title(&self, session: &str, cx: &App) -> Option<String> {
        let workspace = self.owner_entity()?;
        let workspace = workspace.read(cx);
        workspace
            .sessions
            .get(&openchamber_native_core::ids::SessionId::new(session.to_owned()))
            .map(|record| record.title.clone())
            .filter(|title| !title.is_empty())
    }
}

impl Render for ScheduledTasks {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let direction = strings.direction();
        let palette = cx.global::<Palette>().clone();
        let scopes: Vec<AnyElement> = self
            .scopes
            .clone()
            .into_iter()
            .map(|scope| {
                self.render_scope(
                    &scope.id,
                    scope.label.clone(),
                    scope.path.is_none(),
                    &palette,
                    direction,
                    cx,
                )
            })
            .collect();
        let state = |id: &'static str, value: SharedString, error: bool, cx: &Context<Self>| {
            div()
                .id(id)
                .role(Role::Status)
                .accessibility_id(id)
                .aria_label(value.clone())
                .test_support()
                .p(px(16.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(if error {
                    palette.status_error
                } else {
                    palette.border
                })
                .css_text(META_SIZE, META_LINE, cx)
                .text_color(if error {
                    palette.status_error
                } else {
                    palette.muted_foreground
                })
                .child(value)
                .into_any_element()
        };
        let list: AnyElement = match &self.list {
            TaskList::Loading => div()
                .id("scheduled-tasks-loading")
                .role(Role::Status)
                .accessibility_id("scheduled-tasks-loading")
                .aria_label(strings.get("sessions.scheduledTasks.dialog.loading"))
                .test_support()
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .child(glyph("loader-4", 16.0, palette.muted_foreground))
                .child(text(
                    strings.get("sessions.scheduledTasks.dialog.loading"),
                    (META_SIZE, META_LINE),
                    palette.muted_foreground,
                    cx,
                ))
                .into_any_element(),
            TaskList::Failed(error) => {
                let value = if error.is_empty() {
                    strings.get("sessions.scheduledTasks.dialog.toast.loadFailed")
                } else {
                    error.clone().into()
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(state("scheduled-tasks-failed", value, true, cx))
                    .when_some(self.selected.clone(), |this, scope| {
                        this.child(button(
                            "scheduled-tasks-retry",
                            Some("refresh"),
                            Some(strings.get("sessions.scheduledTasks.editor.targetRetry")),
                            strings.get("sessions.scheduledTasks.editor.targetRetry"),
                            Variant::Outline,
                            false,
                            &palette,
                            direction,
                            cx,
                            move |this: &mut Self, _, cx| this.reload(scope.clone(), false, cx),
                        ))
                    })
                    .into_any_element()
            }
            TaskList::Ready(tasks) if tasks.is_empty() => state(
                "scheduled-tasks-empty",
                strings.get(if self.selected.is_some() {
                    "sessions.scheduledTasks.dialog.empty.noTasks"
                } else {
                    "sessions.scheduledTasks.dialog.empty.selectProject"
                }),
                false,
                cx,
            ),
            TaskList::Ready(tasks) => {
                let cards: Vec<AnyElement> = tasks
                    .clone()
                    .iter()
                    .map(|task| self.render_task(task, &strings, &palette, direction, cx))
                    .collect();
                div()
                    .id("scheduled-tasks-list")
                    .role(Role::List)
                    .accessibility_id("scheduled-tasks-list")
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .children(cards)
                    .into_any_element()
            }
        };
        div()
            .id("scheduled-tasks-page")
            .role(Role::Region)
            .accessibility_id("scheduled-tasks-page")
            .test_support()
            .aria_label(strings.get("sessions.sidebar.header.actions.scheduledTasks"))
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(palette.background)
            .flex_row_from(direction)
            .child(
                div()
                    .id("scheduled-tasks-scopes")
                    .role(Role::TabList)
                    .accessibility_id("scheduled-tasks-scopes")
                    .aria_label(strings.get("sessions.scheduledTasks.dialog.project.label"))
                    .w(px(240.0))
                    .flex_shrink_0()
                    .h_full()
                    .overflow_y_scroll()
                    .p(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .border_color(palette.divider)
                    .map(|this| match direction {
                        Direction::LeftToRight => this.border_r_1(),
                        Direction::RightToLeft => this.border_l_1(),
                    })
                    .children(scopes),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px(px(24.0))
                            .pt(px(12.0))
                            .flex_row_from(direction)
                            .items_center()
                            .child(button(
                                "scheduled-tasks-new",
                                Some("add"),
                                Some(strings.get("sessions.scheduledTasks.dialog.actions.newTask")),
                                strings.get("sessions.scheduledTasks.dialog.actions.newTask"),
                                Variant::Primary,
                                self.selected.is_none(),
                                &palette,
                                direction,
                                cx,
                                |this: &mut Self, window, cx| this.open_editor(None, window, cx),
                            )),
                    )
                    .child(
                        div()
                            .id("scheduled-tasks-body")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .px(px(24.0))
                            .py(px(16.0))
                            .child(
                                div()
                                    .w_full()
                                    .max_w(px(768.0))
                                    .mx_auto()
                                    .min_h(px(280.0))
                                    .child(list),
                            ),
                    ),
            )
    }
}

impl TaskEditor {
    fn field(
        &self,
        label_text: SharedString,
        required: bool,
        body: AnyElement,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .flex_row_from(direction)
                    .gap(px(2.0))
                    .child(
                        text(label_text, (META_SIZE, META_LINE), palette.foreground, cx)
                            .font_weight(FontWeight::MEDIUM),
                    )
                    .when(required, |this| {
                        this.child(text("*", (META_SIZE, META_LINE), palette.status_error, cx))
                    }),
            )
            .child(body)
            .into_any_element()
    }

    fn input(&self, id: &'static str, state: &gpui_kit::Entity<gpui_kit::component::input::InputState>, aria: SharedString, width: f32) -> AnyElement {
        div()
            .w(px(width))
            .max_w_full()
            .child(
                Input::new(state)
                    .text_size(rems(LABEL_SIZE))
                    .accessibility_id(id)
                    .aria_label(aria),
            )
            .into_any_element()
    }

    /// A select: its trigger and, when open, its options under it.
    #[allow(clippy::too_many_arguments)]
    fn select(
        &self,
        id: &'static str,
        menu: EditorMenu,
        value: SharedString,
        aria: SharedString,
        options: Vec<AnyElement>,
        disabled: bool,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let open = self.menu == Some(menu);
        div()
            .relative()
            .child(select_trigger(
                id,
                value,
                aria,
                open,
                disabled,
                palette,
                direction,
                cx,
                move |this: &mut Self, window, cx| this.toggle_menu(menu, window, cx),
            ))
            .when(open, |this| {
                this.child(popup("scheduled-task-editor-menu", options, 320.0, palette))
            })
            .into_any_element()
    }

    fn timezone_select(&self, strings: &Strings, palette: &Palette, direction: Direction, cx: &mut Context<Self>) -> AnyElement {
        let filter = self.filter.read(cx).value().to_lowercase();
        let mut options: Vec<AnyElement> = vec![
            div()
                .p(px(4.0))
                .child(
                    Input::new(&self.filter)
                        .text_size(rems(LABEL_SIZE))
                        .accessibility_id("scheduled-task-timezone-filter")
                        .aria_label(strings.get("sessions.scheduledTasks.editor.timezone.label")),
                )
                .into_any_element(),
        ];
        options.extend(
            crate::time_zones::zone_names()
                .iter()
                .filter(|zone| filter.is_empty() || zone.to_lowercase().contains(&filter))
                .take(200)
                .map(|zone| {
                    let value = zone.clone();
                    option_row(
                        format!("scheduled-task-timezone:{zone}"),
                        zone.clone().into(),
                        *zone == self.draft.timezone,
                        false,
                        palette,
                        direction,
                        cx,
                        move |this: &mut Self, _, cx| {
                            this.draft.timezone = value.clone();
                            this.menu = None;
                            cx.notify();
                        },
                    )
                }),
        );
        self.field(
            strings.get("sessions.scheduledTasks.editor.timezone.label"),
            false,
            self.select(
                "scheduled-task-timezone",
                EditorMenu::Timezone,
                self.draft.timezone.clone().into(),
                strings.get("sessions.scheduledTasks.editor.timezone.label"),
                options,
                false,
                palette,
                direction,
                cx,
            ),
            palette,
            direction,
            cx,
        )
    }

    fn schedule_fields(&self, strings: &Strings, palette: &Palette, direction: Direction, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut fields = Vec::new();
        match self.draft.kind {
            ScheduleKind::Cron => {
                let expression = self.draft.cron.trim().to_owned();
                let parsed = (!expression.is_empty()).then(|| CronExpression::parse(&expression));
                let mut body = div().flex().flex_col().gap(px(4.0)).child(self.input(
                    "scheduled-task-cron",
                    &self.cron,
                    strings.get("sessions.scheduledTasks.editor.cronExpression.label"),
                    320.0,
                ));
                if matches!(parsed, Some(Err(_))) {
                    body = body.child(
                        div()
                            .id("scheduled-task-cron-invalid")
                            .role(Role::Alert)
                            .accessibility_id("scheduled-task-cron-invalid")
                            .aria_label(strings.get("sessions.scheduledTasks.editor.validation.cronInvalid"))
                            .test_support()
                            .child(text(
                                strings.get("sessions.scheduledTasks.editor.validation.cronInvalid"),
                                (MICRO_SIZE, MICRO_LINE),
                                palette.status_error,
                                cx,
                            )),
                    );
                }
                fields.push(self.field(
                    strings.get("sessions.scheduledTasks.editor.cronExpression.label"),
                    true,
                    body.into_any_element(),
                    palette,
                    direction,
                    cx,
                ));
                if let Some(Ok(cron)) = parsed {
                    let zone = crate::time_zones::zone(&self.draft.timezone)
                        .unwrap_or_else(time_zone::TimeZone::utc);
                    let runs: Vec<String> = cron
                        .next_runs(&zone, chrono::Utc::now().timestamp(), 4)
                        .into_iter()
                        .map(|at| zoned_moment(strings, &zone, at, cx))
                        .collect();
                    if !runs.is_empty() {
                        fields.push(
                            div()
                                .id("scheduled-task-next-runs")
                                .role(Role::Status)
                                .accessibility_id("scheduled-task-next-runs")
                                .aria_label(SharedString::from(runs.join(", ")))
                                .test_support()
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .child(text(
                                    strings.get("sessions.scheduledTasks.editor.cronExpression.nextRuns"),
                                    (META_SIZE, META_LINE),
                                    palette.muted_foreground,
                                    cx,
                                ))
                                .child(text(runs.join(", "), (MICRO_SIZE, MICRO_LINE), palette.foreground, cx))
                                .into_any_element(),
                        );
                    }
                }
                let examples: Vec<AnyElement> = CRON_EXAMPLES
                    .iter()
                    .map(|(expression, key)| {
                        let value = (*expression).to_owned();
                        button(
                            format!("scheduled-task-cron-example:{expression}"),
                            None,
                            Some(SharedString::from(format!("{expression}  {}", strings.get(key)))),
                            strings.get(key),
                            Variant::Outline,
                            false,
                            palette,
                            direction,
                            cx,
                            move |this: &mut Self, window, cx| this.set_cron(&value, window, cx),
                        )
                    })
                    .collect();
                fields.push(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(text(
                            strings.get("sessions.scheduledTasks.editor.cronExpression.examples"),
                            (META_SIZE, META_LINE),
                            palette.muted_foreground,
                            cx,
                        ))
                        .child(div().flex_row_from(direction).flex_wrap().gap(px(6.0)).children(examples))
                        .into_any_element(),
                );
            }
            ScheduleKind::Once => {
                fields.push(self.field(
                    strings.get("sessions.scheduledTasks.editor.date.label"),
                    false,
                    self.input(
                        "scheduled-task-date",
                        &self.once_date,
                        strings.get("sessions.scheduledTasks.editor.date.label"),
                        160.0,
                    ),
                    palette,
                    direction,
                    cx,
                ));
                fields.push(self.field(
                    strings.get("sessions.scheduledTasks.editor.time.label"),
                    false,
                    self.input(
                        "scheduled-task-time",
                        &self.once_time,
                        strings.get("sessions.scheduledTasks.editor.time.label"),
                        120.0,
                    ),
                    palette,
                    direction,
                    cx,
                ));
            }
            ScheduleKind::Daily | ScheduleKind::Weekly => {
                if self.draft.kind == ScheduleKind::Weekly {
                    let days: Vec<AnyElement> = [1, 2, 3, 4, 5, 6, 0]
                        .into_iter()
                        .map(|day| {
                            checkbox(
                                format!("scheduled-task-weekday:{day}"),
                                strings.get(schedule::weekday_short_key(day)),
                                self.draft.weekdays.contains(&day),
                                false,
                                palette,
                                direction,
                                cx,
                                move |this: &mut Self, _, cx| {
                                    this.draft.toggle_weekday(day);
                                    this.error = None;
                                    cx.notify();
                                },
                            )
                        })
                        .collect();
                    fields.push(self.field(
                        strings.get("sessions.scheduledTasks.editor.weekdays.label"),
                        false,
                        div()
                            .flex_row_from(direction)
                            .flex_wrap()
                            .gap(px(12.0))
                            .children(days)
                            .into_any_element(),
                        palette,
                        direction,
                        cx,
                    ));
                }
                let several = self.times.len() > 1;
                let rows: Vec<AnyElement> = self
                    .times
                    .iter()
                    .enumerate()
                    .map(|(index, input)| {
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div().w(px(120.0)).child(
                                    Input::new(input)
                                        .text_size(rems(LABEL_SIZE))
                                        .accessibility_id(format!("scheduled-task-time:{index}"))
                                        .aria_label(strings.get("sessions.scheduledTasks.editor.time.label")),
                                ),
                            )
                            .when(several, |this| {
                                this.child(button(
                                    format!("scheduled-task-time-remove:{index}"),
                                    Some("close"),
                                    None,
                                    strings.get("sessions.scheduledTasks.editor.times.removeAria"),
                                    Variant::Ghost,
                                    false,
                                    palette,
                                    direction,
                                    cx,
                                    move |this: &mut Self, window, cx| this.remove_time(index, window, cx),
                                ))
                            })
                            .into_any_element()
                    })
                    .collect();
                fields.push(self.field(
                    strings.get("sessions.scheduledTasks.editor.times.label"),
                    false,
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .children(rows)
                        .child(button(
                            "scheduled-task-time-add",
                            Some("add"),
                            Some(strings.get("sessions.scheduledTasks.editor.times.add")),
                            strings.get("sessions.scheduledTasks.editor.times.add"),
                            Variant::Outline,
                            false,
                            palette,
                            direction,
                            cx,
                            |this: &mut Self, window, cx| this.add_time(window, cx),
                        ))
                        .into_any_element(),
                    palette,
                    direction,
                    cx,
                ));
            }
        }
        fields.push(self.timezone_select(strings, palette, direction, cx));
        fields
    }

    fn model_fields(&self, strings: &Strings, palette: &Palette, direction: Direction, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut fields = Vec::new();
        let filter = self.filter.read(cx).value().to_lowercase();
        let mut models: Vec<AnyElement> = vec![
            div()
                .p(px(4.0))
                .child(
                    Input::new(&self.filter)
                        .text_size(rems(LABEL_SIZE))
                        .accessibility_id("scheduled-task-model-filter")
                        .aria_label(strings.get("sessions.scheduledTasks.editor.model.label")),
                )
                .into_any_element(),
        ];
        for provider in &self.catalog.providers {
            for model in &provider.models {
                let shown = model.name.clone().unwrap_or_else(|| model.id.clone());
                let full = format!("{} / {shown}", provider.name.as_deref().unwrap_or(&provider.id));
                if !filter.is_empty() && !full.to_lowercase().contains(&filter) {
                    continue;
                }
                let (provider_id, model_id) = (provider.id.clone(), model.id.clone());
                models.push(option_row(
                    format!("scheduled-task-model:{}/{}", provider.id, model.id),
                    full.into(),
                    provider.id == self.draft.provider_id && model.id == self.draft.model_id,
                    false,
                    palette,
                    direction,
                    cx,
                    move |this: &mut Self, _, cx| {
                        this.set_model(provider_id.clone(), model_id.clone(), cx)
                    },
                ));
            }
        }
        let model_value: SharedString = if self.draft.model_id.is_empty() {
            strings.get("sessions.scheduledTasks.editor.model.label")
        } else {
            self.draft.model_id.clone().into()
        };
        fields.push(self.field(
            strings.get("sessions.scheduledTasks.editor.model.label"),
            true,
            self.select(
                "scheduled-task-model",
                EditorMenu::Model,
                model_value,
                strings.get("sessions.scheduledTasks.editor.model.label"),
                models,
                false,
                palette,
                direction,
                cx,
            ),
            palette,
            direction,
            cx,
        ));
        let variants = self.variants();
        let default_label = strings.get("sessions.scheduledTasks.editor.thinkingLevel.default");
        let mut variant_options = vec![option_row(
            "scheduled-task-variant:default",
            default_label.clone(),
            self.draft.variant.is_empty(),
            false,
            palette,
            direction,
            cx,
            |this: &mut Self, _, cx| {
                this.draft.variant.clear();
                this.menu = None;
                cx.notify();
            },
        )];
        for variant in &variants {
            let value = variant.clone();
            variant_options.push(option_row(
                format!("scheduled-task-variant:{variant}"),
                variant.clone().into(),
                *variant == self.draft.variant,
                false,
                palette,
                direction,
                cx,
                move |this: &mut Self, _, cx| {
                    this.draft.variant = value.clone();
                    this.menu = None;
                    cx.notify();
                },
            ));
        }
        let variant_value = if variants.contains(&self.draft.variant) {
            self.draft.variant.clone().into()
        } else {
            default_label
        };
        fields.push(self.field(
            strings.get("sessions.scheduledTasks.editor.thinkingLevel.label"),
            false,
            self.select(
                "scheduled-task-variant",
                EditorMenu::Variant,
                variant_value,
                strings.get("sessions.scheduledTasks.editor.thinkingLevel.label"),
                variant_options,
                variants.is_empty(),
                palette,
                direction,
                cx,
            ),
            palette,
            direction,
            cx,
        ));
        let mut agents = vec![option_row(
            "scheduled-task-agent:none",
            strings.get("sessions.scheduledTasks.editor.thinkingLevel.default"),
            self.draft.agent.is_empty(),
            false,
            palette,
            direction,
            cx,
            |this: &mut Self, _, cx| {
                this.draft.agent.clear();
                this.menu = None;
                cx.notify();
            },
        )];
        for agent in &self.catalog.agents {
            let value = agent.name.clone();
            agents.push(option_row(
                format!("scheduled-task-agent:{}", agent.name),
                agent.display_name.clone().unwrap_or_else(|| agent.name.clone()).into(),
                agent.name == self.draft.agent,
                false,
                palette,
                direction,
                cx,
                move |this: &mut Self, _, cx| {
                    this.draft.agent = value.clone();
                    this.menu = None;
                    cx.notify();
                },
            ));
        }
        let agent_value: SharedString = if self.draft.agent.is_empty() {
            strings.get("sessions.scheduledTasks.editor.thinkingLevel.default")
        } else {
            self.draft.agent.clone().into()
        };
        fields.push(self.field(
            strings.get("sessions.scheduledTasks.editor.agent.label"),
            false,
            self.select(
                "scheduled-task-agent",
                EditorMenu::Agent,
                agent_value,
                strings.get("sessions.scheduledTasks.editor.agent.label"),
                agents,
                false,
                palette,
                direction,
                cx,
            ),
            palette,
            direction,
            cx,
        ));
        fields
    }
}

impl Render for TaskEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let direction = strings.direction();
        let palette = cx.global::<Palette>().clone();
        let mut fields: Vec<AnyElement> = vec![text(
            strings.get("sessions.scheduledTasks.editor.description"),
            (META_SIZE, META_LINE),
            palette.muted_foreground,
            cx,
        )
        .into_any_element()];
        if self.scopes.len() > 1 {
            let options: Vec<AnyElement> = self
                .scopes
                .clone()
                .into_iter()
                .map(|scope| {
                    let id = scope.id.clone();
                    option_row(
                        format!("scheduled-task-scope:{}", scope.id),
                        scope.label.clone(),
                        scope.id == self.scope,
                        false,
                        &palette,
                        direction,
                        cx,
                        move |this: &mut Self, window, cx| this.set_scope(id.clone(), window, cx),
                    )
                })
                .collect();
            let value = self
                .scopes
                .iter()
                .find(|scope| scope.id == self.scope)
                .map(|scope| scope.label.clone())
                .unwrap_or_default();
            fields.push(self.field(
                strings.get("sessions.scheduledTasks.dialog.project.label"),
                false,
                self.select(
                    "scheduled-task-scope",
                    EditorMenu::Scope,
                    value,
                    strings.get("sessions.scheduledTasks.dialog.project.label"),
                    options,
                    false,
                    &palette,
                    direction,
                    cx,
                ),
                &palette,
                direction,
                cx,
            ));
        }
        let existing = self.draft.session_mode == SessionMode::Existing;
        let mode_label = |mode: SessionMode| {
            strings.get(match mode {
                SessionMode::New => "sessions.scheduledTasks.editor.newSession",
                SessionMode::Existing => "sessions.scheduledTasks.editor.existingSession",
            })
        };
        let modes: Vec<AnyElement> = [SessionMode::New, SessionMode::Existing]
            .into_iter()
            .map(|mode| {
                option_row(
                    format!(
                        "scheduled-task-session-mode:{}",
                        if mode == SessionMode::New { "new" } else { "existing" }
                    ),
                    mode_label(mode),
                    self.draft.session_mode == mode,
                    false,
                    &palette,
                    direction,
                    cx,
                    move |this: &mut Self, _, cx| this.set_session_mode(mode, cx),
                )
            })
            .collect();
        fields.push(self.field(
            strings.get("sessions.scheduledTasks.editor.sessionMode"),
            false,
            self.select(
                "scheduled-task-session-mode",
                EditorMenu::SessionMode,
                mode_label(self.draft.session_mode),
                strings.get("sessions.scheduledTasks.editor.sessionMode"),
                modes,
                false,
                &palette,
                direction,
                cx,
            ),
            &palette,
            direction,
            cx,
        ));
        if existing {
            let options: Vec<AnyElement> = match &self.eligibility {
                Eligibility::Loading => vec![text(
                    strings.get("common.loading"),
                    (META_SIZE, META_LINE),
                    palette.muted_foreground,
                    cx,
                )
                .p(px(8.0))
                .into_any_element()],
                _ => self
                    .target_sessions()
                    .into_iter()
                    .cloned()
                    .map(|(id, _, title)| {
                        let value = id.clone();
                        option_row(
                            format!("scheduled-task-target:{id}"),
                            if title.is_empty() { id.clone() } else { title }.into(),
                            id == self.draft.target_session_id,
                            false,
                            &palette,
                            direction,
                            cx,
                            move |this: &mut Self, _, cx| {
                                this.draft.target_session_id = value.clone();
                                this.menu = None;
                                this.error = None;
                                cx.notify();
                            },
                        )
                    })
                    .collect(),
            };
            let value: SharedString = self
                .catalog
                .sessions
                .iter()
                .find(|(id, _, _)| *id == self.draft.target_session_id)
                .map(|(_, _, title)| title.clone())
                .filter(|title| !title.is_empty())
                .or_else(|| {
                    (!self.draft.target_session_id.is_empty())
                        .then(|| self.draft.target_session_id.clone())
                })
                .map(SharedString::from)
                .unwrap_or_else(|| strings.get("sessions.scheduledTasks.editor.targetSession"));
            let failed = self.eligibility == Eligibility::Failed;
            fields.push(self.field(
                strings.get("sessions.scheduledTasks.editor.targetSession"),
                false,
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(self.select(
                        "scheduled-task-target",
                        EditorMenu::Target,
                        value,
                        strings.get("sessions.scheduledTasks.editor.targetSession"),
                        options,
                        false,
                        &palette,
                        direction,
                        cx,
                    ))
                    .when(failed, |this| {
                        this.child(
                            div()
                                .id("scheduled-task-target-failed")
                                .role(Role::Alert)
                                .accessibility_id("scheduled-task-target-failed")
                                .aria_label(strings.get("sessions.scheduledTasks.editor.targetLoadFailed"))
                                .test_support()
                                .flex_row_from(direction)
                                .items_center()
                                .gap(px(8.0))
                                .child(text(
                                    strings.get("sessions.scheduledTasks.editor.targetLoadFailed"),
                                    (META_SIZE, META_LINE),
                                    palette.muted_foreground,
                                    cx,
                                ))
                                .child(button(
                                    "scheduled-task-target-retry",
                                    None,
                                    Some(strings.get("sessions.scheduledTasks.editor.targetRetry")),
                                    strings.get("sessions.scheduledTasks.editor.targetRetry"),
                                    Variant::Ghost,
                                    false,
                                    &palette,
                                    direction,
                                    cx,
                                    |this: &mut Self, _, cx| this.load_targets(cx),
                                )),
                        )
                    })
                    .into_any_element(),
                &palette,
                direction,
                cx,
            ));
        }
        let kinds: Vec<AnyElement> = ScheduleKind::ALL
            .into_iter()
            .map(|kind| {
                option_row(
                    format!("scheduled-task-kind:{}", kind.as_str()),
                    strings.get(kind.label_key()),
                    self.draft.kind == kind,
                    false,
                    &palette,
                    direction,
                    cx,
                    move |this: &mut Self, window, cx| this.set_kind(kind, window, cx),
                )
            })
            .collect();
        fields.push(
            div()
                .flex_row_from(direction)
                .flex_wrap()
                .gap(px(16.0))
                .child(self.field(
                    strings.get("sessions.scheduledTasks.editor.taskName.label"),
                    true,
                    self.input(
                        "scheduled-task-name",
                        &self.name,
                        strings.get("sessions.scheduledTasks.editor.taskName.label"),
                        220.0,
                    ),
                    &palette,
                    direction,
                    cx,
                ))
                .child(self.field(
                    strings.get("sessions.scheduledTasks.editor.scheduleType.label"),
                    false,
                    self.select(
                        "scheduled-task-kind",
                        EditorMenu::Kind,
                        strings.get(self.draft.kind.label_key()),
                        strings.get("sessions.scheduledTasks.editor.scheduleType.label"),
                        kinds,
                        false,
                        &palette,
                        direction,
                        cx,
                    ),
                    &palette,
                    direction,
                    cx,
                ))
                .into_any_element(),
        );
        fields.extend(self.schedule_fields(&strings, &palette, direction, cx));
        if !existing && self.scope_path().is_some_and(|path| !path.trim().is_empty()) {
            let mut worktree = div().flex().flex_col().gap(px(4.0)).child(checkbox(
                "scheduled-task-worktree",
                strings.get("sessions.scheduledTasks.editor.worktree.label"),
                self.draft.run_in_worktree,
                false,
                &palette,
                direction,
                cx,
                |this: &mut Self, _, cx| {
                    this.draft.run_in_worktree = !this.draft.run_in_worktree;
                    cx.notify();
                },
            ))
            .child(text(
                strings.get("sessions.scheduledTasks.editor.worktree.hint"),
                (META_SIZE, META_LINE),
                palette.muted_foreground,
                cx,
            ));
            if self.draft.run_in_worktree {
                worktree = worktree
                    .child(self.field(
                        strings.get("sessions.scheduledTasks.editor.worktree.baseBranch.label"),
                        false,
                        self.input(
                            "scheduled-task-base-branch",
                            &self.base_branch,
                            strings.get("sessions.scheduledTasks.editor.worktree.baseBranch.label"),
                            220.0,
                        ),
                        &palette,
                        direction,
                        cx,
                    ))
                    .child(checkbox(
                        "scheduled-task-cleanup",
                        strings.get("sessions.scheduledTasks.editor.worktree.cleanup.label"),
                        self.draft.cleanup_worktree,
                        false,
                        &palette,
                        direction,
                        cx,
                        |this: &mut Self, _, cx| {
                            this.draft.cleanup_worktree = !this.draft.cleanup_worktree;
                            cx.notify();
                        },
                    ))
                    .child(text(
                        strings.get(if self.draft.cleanup_worktree {
                            "sessions.scheduledTasks.editor.worktree.cleanup.hint"
                        } else {
                            "sessions.scheduledTasks.editor.worktree.cleanup.keepHint"
                        }),
                        (META_SIZE, META_LINE),
                        palette.muted_foreground,
                        cx,
                    ));
            }
            fields.push(worktree.into_any_element());
        }
        fields.push(checkbox(
            "scheduled-task-use-defaults",
            strings.get("sessions.scheduledTasks.editor.useDefaults.label"),
            self.draft.use_defaults,
            false,
            &palette,
            direction,
            cx,
            |this: &mut Self, _, cx| {
                this.draft.use_defaults = !this.draft.use_defaults;
                this.error = None;
                cx.notify();
            },
        ));
        if self.draft.use_defaults {
            fields.push(
                text(
                    strings.get(if existing {
                        "sessions.scheduledTasks.editor.targetHint"
                    } else {
                        "sessions.scheduledTasks.editor.useDefaults.hint"
                    }),
                    (META_SIZE, META_LINE),
                    palette.muted_foreground,
                    cx,
                )
                .into_any_element(),
            );
        } else {
            fields.extend(self.model_fields(&strings, &palette, direction, cx));
        }
        fields.push(self.field(
            strings.get("sessions.scheduledTasks.editor.prompt.label"),
            true,
            div()
                .min_h(px(120.0))
                .rounded(px(6.0))
                .border_1()
                .border_color(palette.border)
                .child(
                    Textarea::new(&self.prompt)
                        .px(px(8.0))
                        .py(px(6.0))
                        .text_size(rems(META_SIZE))
                        .accessibility_id("scheduled-task-prompt")
                        .aria_label(strings.get("sessions.scheduledTasks.editor.prompt.label")),
                )
                .into_any_element(),
            &palette,
            direction,
            cx,
        ));
        if !existing && self.draft.goal_enabled {
            fields.push(
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(16.0))
                    .child(checkbox(
                        "scheduled-task-goal-budget",
                        strings.get("sessions.scheduledTasks.editor.goal.budgetLabel"),
                        self.draft.goal_token_budget.is_some(),
                        false,
                        &palette,
                        direction,
                        cx,
                        |this: &mut Self, window, cx| {
                            this.draft.goal_token_budget = match this.draft.goal_token_budget {
                                Some(_) => None,
                                None => Some(200_000),
                            };
                            let value = this
                                .draft
                                .goal_token_budget
                                .map(|budget| budget.to_string())
                                .unwrap_or_default();
                            this.budget
                                .update(cx, |input, cx| input.set_value(value, window, cx));
                            cx.notify();
                        },
                    ))
                    .when(self.draft.goal_token_budget.is_some(), |this| {
                        this.child(self.input(
                            "scheduled-task-goal-budget-value",
                            &self.budget,
                            strings.get("sessions.scheduledTasks.editor.goal.budgetAria"),
                            140.0,
                        ))
                    })
                    .into_any_element(),
            );
        }
        let saving = self.saving;
        let footer = div()
            .flex_row_from(direction)
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .child(checkbox(
                "scheduled-task-enabled",
                strings.get("sessions.scheduledTasks.editor.enabled.label"),
                self.draft.enabled,
                false,
                &palette,
                direction,
                cx,
                |this: &mut Self, _, cx| {
                    this.draft.enabled = !this.draft.enabled;
                    cx.notify();
                },
            ))
            .child(
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(8.0))
                    .when(!existing, |this| {
                        this.child(button(
                            "scheduled-task-auto-accept",
                            Some(if self.draft.permission_auto_accept {
                                "shield-check"
                            } else {
                                "shield-user"
                            }),
                            None,
                            strings.get("sessions.scheduledTasks.editor.permissionAutoAccept.aria"),
                            Variant::Ghost,
                            false,
                            &palette,
                            direction,
                            cx,
                            |this: &mut Self, _, cx| {
                                this.draft.permission_auto_accept = !this.draft.permission_auto_accept;
                                cx.notify();
                            },
                        ))
                        .child(button(
                            "scheduled-task-goal",
                            Some(if self.draft.goal_enabled {
                                "target-fill"
                            } else {
                                "target"
                            }),
                            None,
                            strings.get("sessions.scheduledTasks.editor.goal.aria"),
                            Variant::Ghost,
                            false,
                            &palette,
                            direction,
                            cx,
                            |this: &mut Self, _, cx| {
                                this.draft.goal_enabled = !this.draft.goal_enabled;
                                cx.notify();
                            },
                        ))
                    })
                    .child(button(
                        "scheduled-task-cancel",
                        None,
                        Some(strings.get("sessions.scheduledTasks.editor.actions.cancel")),
                        strings.get("sessions.scheduledTasks.editor.actions.cancel"),
                        Variant::Ghost,
                        saving,
                        &palette,
                        direction,
                        cx,
                        |this: &mut Self, window, cx| this.cancel(window, cx),
                    ))
                    .child(button(
                        "scheduled-task-save",
                        None,
                        Some(strings.get(if saving {
                            "sessions.scheduledTasks.editor.actions.saving"
                        } else {
                            "sessions.scheduledTasks.editor.actions.save"
                        })),
                        strings.get("sessions.scheduledTasks.editor.actions.save"),
                        Variant::Primary,
                        saving,
                        &palette,
                        direction,
                        cx,
                        |this: &mut Self, window, cx| this.submit(window, cx),
                    )),
            );
        div()
            .id("scheduled-task-editor")
            .role(Role::Form)
            .accessibility_id("scheduled-task-editor")
            .test_support()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(
                div()
                    .id("scheduled-task-editor-body")
                    .max_h(px(560.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(20.0))
                    .children(fields),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .id("scheduled-task-editor-error")
                        .role(Role::Alert)
                        .accessibility_id("scheduled-task-editor-error")
                        .aria_label(error.clone())
                        .test_support()
                        .child(text(error, (META_SIZE, META_LINE), palette.status_error, cx)),
                )
            })
            .child(footer)
    }
}
