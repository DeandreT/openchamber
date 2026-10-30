//! Pending-send recovery and local-storage notices above the composer.

use gpui_kit::{
    AnyElement, Context, ElementId, InteractiveElement, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window, div,
    prelude::FluentBuilder, px,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::send_attempt::SessionKey;
use crate::theme::Palette;
use crate::workspace::Workspace;
use openchamber_native_core::ids::MessageId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryPhase {
    Preparing,
    Sending,
    Queued,
    Uncertain,
    Checking,
    NotObserved,
    CheckFailed,
    OutOfOrder,
    Refused,
    CreatePreparing,
    Creating,
    CreateUnknown,
    CreateCheckFailed,
    CreateNotObserved,
    CreateMismatch,
    CreateDeleted,
    CreateReady,
}

pub(crate) struct RecoveryRow {
    pub key: SessionKey,
    /// The retained prompt's ID; a session can show several rows.
    pub message: MessageId,
    pub session_label: SharedString,
    pub text: SharedString,
    pub phase: RecoveryPhase,
    pub can_check: bool,
    pub can_retry: bool,
    /// Only for an attempt nothing can settle: it moves the prompt back to
    /// the composer and drops the attempt.
    pub can_restore: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StorageNotice {
    Loading,
    Closing,
    Io,
    Invalid,
    Locked,
    SyncUncertain,
    WriteRejected,
}

impl Workspace {
    /// An empty recovery region has no padding, margin or minimum height.
    pub(crate) fn render_send_recovery(
        &self,
        palette: &Palette,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows = self.recovery_items(cx);
        let notice = self.persistence_notice();
        if rows.is_empty() && notice.is_none() {
            return div().into_any_element();
        }
        let mut region = div()
            .id("send-recovery")
            .accessibility_id("send-recovery")
            .role(Role::Group)
            .aria_label(locale::strings(cx).get("chat.nativeRecovery.title"))
            .w_full()
            .min_w_0()
            .max_h(px(180.0))
            .overflow_y_scroll()
            .flex()
            .flex_col();

        if let Some(notice) = notice {
            let strings = locale::strings(cx);
            let label = strings.get(match notice {
                StorageNotice::Loading => "chat.nativeRecovery.loading",
                StorageNotice::Closing => "chat.nativeRecovery.closing",
                StorageNotice::Io => "chat.nativeRecovery.storageFailed",
                StorageNotice::Invalid => "chat.nativeRecovery.invalidJournal",
                StorageNotice::Locked => "chat.nativeRecovery.profileLocked",
                StorageNotice::SyncUncertain => "chat.nativeRecovery.syncUncertain",
                StorageNotice::WriteRejected => "chat.nativeRecovery.saveRejected",
            });
            let retry_label = strings.get("chat.nativeRecovery.retryStorage");
            let mut notice_row = div()
                .id("recovery-storage-notice")
                .accessibility_id("recovery-storage-notice")
                .role(Role::Group)
                .aria_label(label.clone())
                .min_w_0()
                .min_h(px(36.0))
                .padding_start(direction, px(12.0))
                .padding_end(direction, px(6.0))
                .py(px(4.0))
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .bg(
                    if matches!(notice, StorageNotice::Loading | StorageNotice::Closing) {
                        palette.subtle
                    } else {
                        palette.status_warning_background
                    },
                )
                .text_color(palette.foreground)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .css_text(0.75, 1.0, cx)
                        .when(direction == Direction::RightToLeft, |this| {
                            this.text_right()
                        })
                        .child(label.clone()),
                );
            if !matches!(notice, StorageNotice::Loading | StorageNotice::Closing) {
                notice_row = notice_row.child(recovery_action(
                    RecoveryAction {
                        id: "recovery-retry-storage".into(),
                        label: retry_label.clone(),
                        accessible_label: format!("{retry_label}: {label}").into(),
                        enabled: true,
                    },
                    palette,
                    window,
                    cx,
                    |this, window, cx| this.retry_persistence(window, cx),
                ));
            }
            region = region.child(notice_row);
        }

        for row in rows {
            // Length-prefix the runtime so opaque IDs cannot collide at a separator.
            let id = format!(
                "recovery-{}:{}:{}:{}",
                row.key.runtime.as_str().len(),
                row.key.runtime,
                row.key.session,
                row.message,
            );
            let strings = locale::strings(cx);
            let status = strings.get(match row.phase {
                RecoveryPhase::Preparing => "chat.nativeRecovery.preparing",
                RecoveryPhase::Sending => "chat.nativeRecovery.sending",
                RecoveryPhase::Queued => "chat.nativeRecovery.queued",
                RecoveryPhase::Uncertain => "chat.nativeRecovery.unknown",
                RecoveryPhase::Checking => "chat.nativeRecovery.checking",
                RecoveryPhase::NotObserved => "chat.nativeRecovery.notObserved",
                RecoveryPhase::CheckFailed => "chat.nativeRecovery.checkFailed",
                RecoveryPhase::OutOfOrder => "chat.nativeRecovery.outOfOrder",
                RecoveryPhase::Refused => "chat.nativeRecovery.refused",
                RecoveryPhase::CreatePreparing => "chat.nativeCreation.preparing",
                RecoveryPhase::Creating => "chat.nativeCreation.creating",
                RecoveryPhase::CreateUnknown => "chat.nativeCreation.unknown",
                RecoveryPhase::CreateCheckFailed => "chat.nativeCreation.checkFailed",
                RecoveryPhase::CreateNotObserved => "chat.nativeCreation.notObserved",
                RecoveryPhase::CreateMismatch => "chat.nativeCreation.mismatch",
                RecoveryPhase::CreateDeleted => "chat.nativeCreation.deleted",
                RecoveryPhase::CreateReady => "chat.nativeCreation.ready",
            });
            let check_label = strings.get("chat.nativeRecovery.check");
            let retry_label = strings.get("chat.nativeRecovery.retry");
            let restore_label = strings.get("chat.nativeRecovery.restore");
            let preview: SharedString = row
                .text
                .chars()
                .take(240)
                .map(|character| {
                    if character.is_whitespace() {
                        ' '
                    } else {
                        character
                    }
                })
                .collect::<String>()
                .into();
            let description: SharedString =
                format!("{}: {preview}. {status}", row.session_label).into();
            let check_key = row.key.clone();
            let check_message = row.message.clone();
            let check = recovery_action(
                RecoveryAction {
                    id: format!("{id}-check").into(),
                    label: check_label.clone(),
                    accessible_label: format!("{check_label}: {description}").into(),
                    enabled: row.can_check,
                },
                palette,
                window,
                cx,
                move |this, window, cx| this.check_recovery(&check_key, &check_message, window, cx),
            );
            let restore = row.can_restore.then(|| {
                let key = row.key.clone();
                let message = row.message.clone();
                recovery_action(
                    RecoveryAction {
                        id: format!("{id}-restore").into(),
                        label: restore_label.clone(),
                        accessible_label: format!("{restore_label}: {description}").into(),
                        enabled: true,
                    },
                    palette,
                    window,
                    cx,
                    move |this, window, cx| this.restore_recovery(&key, &message, window, cx),
                )
            });
            let unresolved = matches!(
                row.phase,
                RecoveryPhase::CheckFailed | RecoveryPhase::OutOfOrder | RecoveryPhase::Refused
            );
            let retry = recovery_action(
                RecoveryAction {
                    id: format!("{id}-retry").into(),
                    label: retry_label.clone(),
                    accessible_label: format!("{retry_label}: {description}").into(),
                    enabled: row.can_retry,
                },
                palette,
                window,
                cx,
                move |this, window, cx| this.retry_recovery(&row.key, &row.message, window, cx),
            );
            let id: SharedString = id.into();
            region = region.child(
                div()
                    .id(ElementId::Name(id.clone()))
                    .accessibility_id(id)
                    .role(Role::Group)
                    .aria_label(description)
                    .min_w_0()
                    .padding_start(direction, px(12.0))
                    .padding_end(direction, px(6.0))
                    .py(px(4.0))
                    .bg(palette.subtle)
                    .text_color(palette.foreground)
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .when(direction == Direction::RightToLeft, |this| {
                                this.text_right()
                            })
                            .child(
                                div()
                                    .truncate()
                                    .css_text(0.75, 1.0, cx)
                                    .child(format!("{}: {preview}", row.session_label)),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .css_text(0.75, 1.0, cx)
                                    .text_color(if unresolved {
                                        palette.error
                                    } else {
                                        palette.muted_foreground
                                    })
                                    .child(status),
                            ),
                    )
                    .child(
                        div()
                            .tab_group()
                            .flex_shrink_0()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(2.0))
                            .child(check)
                            .child(retry)
                            .children(restore),
                    ),
            );
        }
        region.into_any_element()
    }
}

struct RecoveryAction {
    id: SharedString,
    label: SharedString,
    accessible_label: SharedString,
    enabled: bool,
}

/// Retained focus uses the action's runtime/session/message identity, never its row position.
fn recovery_action(
    action: RecoveryAction,
    palette: &Palette,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    activate: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) -> AnyElement {
    let RecoveryAction {
        id,
        label,
        accessible_label,
        enabled,
    } = action;
    let focus = window
        .use_keyed_state(
            ElementId::Name(format!("{id}-focus").into()),
            cx,
            |_, cx| cx.focus_handle().tab_index(0).tab_stop(true),
        )
        .read(cx)
        .clone();
    let hover = palette.hover;
    let button = div()
        .id(ElementId::Name(id.clone()))
        .accessibility_id(id)
        .role(Role::Button)
        .aria_label(accessible_label)
        .h(px(28.0))
        .px(px(6.0))
        .flex_shrink_0()
        .rounded(px(6.0))
        .flex()
        .items_center()
        .justify_center()
        .text_color(palette.foreground)
        .when(!enabled, |this| {
            this.opacity(0.4)
                .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
        })
        .child(div().css_text(0.75, 1.0, cx).child(label));
    if enabled {
        on_activate(
            button
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .focus(move |style| style.bg(hover)),
            cx,
            activate,
        )
        .test_support()
        .track_focus(&focus)
        .into_any_element()
    } else {
        button.test_support().into_any_element()
    }
}
