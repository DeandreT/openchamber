//! The session actions menu behind the More button beside the header's
//! title, and the title's inline rename (`Header.tsx`, the single-session
//! header's `DropdownMenu`).
//!
//! The menu lists, in the reference's order: Rename, AI rename, Copy ID,
//! then Export Markdown, Move to worktree and Move session after a
//! separator, then Archive and Delete after another. Every item runs the
//! action the sidebar row menu runs, so confirmations, notices and failures
//! are the same. Rename and Archive show their shortcut at the line's end.
//! The reference's Open in Mini Chat and extension items have no native
//! feature and are left out.
//!
//! What the menu lists is decided at each draw from the session's live
//! state, so an AI rename check that answers, a turn that starts or a rename
//! that settles shows at once.

use std::time::Duration;

use gpui_kit::base::actions::{Cancel, Confirm, SelectDown, SelectFirst, SelectLast, SelectUp};
use gpui_kit::component::input::{Escape, Input};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    AccessibleAction, Anchor, AnyElement, App, ClickEvent, Context, Edges, EntityInputHandler,
    FocusHandle, Hsla, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    Pixels, Point, Role, SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window,
    anchored, deferred, div, prelude::FluentBuilder, px, rems,
};
use openchamber_native_core::chat_roots::is_chat_directory;
use openchamber_native_core::ids::SessionId;
use openchamber_native_core::permission_mode::is_space_directory;
use openchamber_native_core::session::LiveStatus;
use openchamber_native_core::session_actions::{AiRenameState, step_highlight_by};
use openchamber_native_core::shortcuts::{self, Overrides, Platform};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{CONTROL_RADIUS, glyph};
use crate::elevation;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::session_menu::MENU_CONTEXT;
use crate::shortcuts::ShortcutBindings;
use crate::theme::Palette;
use crate::transcript_view::META_SIZE;
use crate::workspace::Workspace;

/// The row key the header's rename and menu dismissal are recorded under;
/// no sidebar row has it.
pub(crate) const ROW_KEY: &str = "header-session";
/// Accessibility identity of the More button.
pub(crate) const TRIGGER_ID: &str = "session-actions";
/// Accessibility identity of the open menu.
pub(crate) const MENU_ID: &str = "header-session-menu";
/// Accessibility identity of the rename field's group.
pub(crate) const RENAME_ID: &str = "header-session-rename";
/// `min-w-[190px]`.
const MENU_MIN_WIDTH: f32 = 190.0;
/// The More button: `h-[1.125rem] w-6`.
const TRIGGER_WIDTH: f32 = 24.0;
const TRIGGER_HEIGHT: f32 = 18.0;
/// `typography-ui-label`.
const LABEL_SIZE: f32 = 0.843_75;
const LABEL_LINE: f32 = 1.25;
/// The header's title size, which the rename field keeps.
const TITLE_SIZE: f32 = 0.875;
/// The width of the rename field: an HTML text input's 20 characters at the
/// title's size.
const RENAME_WIDTH: f32 = 168.0;
/// `SessionMenuItemHint`: resting on an item explains it.
const HINT_DELAY: Duration = Duration::from_millis(500);

/// One action of the header's session menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum HeaderAction {
    Rename,
    AiRename,
    CopyId,
    ExportMarkdown,
    /// Creates a worktree from the session's branch and moves its tree there.
    MoveToWorktree,
    /// Opens the dialog that moves the conversation to another project.
    MoveSession,
    Archive,
    Delete,
}

impl HeaderAction {
    /// A stable name for accessibility identities and automation.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Rename => "rename",
            Self::AiRename => "ai-rename",
            Self::CopyId => "copy-id",
            Self::ExportMarkdown => "export-markdown",
            Self::MoveToWorktree => "move-to-worktree",
            Self::MoveSession => "move-session",
            Self::Archive => "archive",
            Self::Delete => "delete",
        }
    }

    fn label_key(self) -> &'static str {
        match self {
            Self::Rename => "sessions.sidebar.session.menu.rename",
            Self::AiRename => "sessions.aiRename.action",
            Self::CopyId => "sessions.sidebar.session.menu.copyId",
            Self::ExportMarkdown => "sessions.sidebar.session.menu.exportMarkdown",
            Self::MoveToWorktree => "sessions.sidebar.session.menu.moveToWorktree",
            Self::MoveSession => "sessions.moveSession.menu",
            Self::Archive => "sessions.sidebar.bulkActions.archive",
            Self::Delete => "sessions.sidebar.bulkActions.delete",
        }
    }

    /// The sprite glyph drawn before the label. The header draws both move
    /// items with `folder-shared`.
    fn icon(self) -> &'static str {
        match self {
            Self::Rename => "pencil-ai",
            Self::AiRename => "ai-generate-2",
            Self::CopyId => "file-copy",
            Self::ExportMarkdown => "download",
            Self::MoveToWorktree | Self::MoveSession => "folder-shared",
            Self::Archive => "inbox-archive",
            Self::Delete => "delete-bin",
        }
    }

    /// `ai-generate-2` fills more of its box than its neighbours, so the
    /// reference draws it a pixel smaller.
    fn icon_size(self) -> f32 {
        if self == Self::AiRename { 15.0 } else { 16.0 }
    }

    fn is_destructive(self) -> bool {
        self == Self::Delete
    }

    /// The configurable shortcut shown at the line's end.
    fn shortcut_id(self) -> Option<&'static str> {
        match self {
            Self::Rename => Some("rename_current_session"),
            Self::Archive => Some("archive_current_session"),
            _ => None,
        }
    }

    /// The explanation shown after the pointer rests on the item.
    fn hint_key(self, context: &HeaderContext) -> &'static str {
        match self {
            Self::Rename => "sessions.sidebar.session.menuHint.rename",
            Self::AiRename => match context.ai_rename {
                AiRenameState::Pending => "sessions.aiRename.generating",
                AiRenameState::NoSmallModel => "sessions.aiRename.noSmallModel",
                AiRenameState::Checking => "sessions.aiRename.checking",
                AiRenameState::Empty => "sessions.aiRename.noCompletedTurns",
                AiRenameState::Ready | AiRenameState::Unknown => {
                    "sessions.sidebar.session.menuHint.aiRename"
                }
            },
            Self::CopyId => "sessions.sidebar.session.menuHint.copyId",
            Self::ExportMarkdown => "sessions.sidebar.session.menuHint.exportMarkdown",
            Self::MoveToWorktree if context.moving => {
                "sessions.sidebar.session.moveToWorktree.tooltipMoving"
            }
            Self::MoveToWorktree if context.busy => {
                "sessions.sidebar.session.moveToWorktree.tooltipBusy"
            }
            Self::MoveToWorktree => "sessions.sidebar.session.moveToWorktree.tooltip",
            Self::MoveSession if context.busy => {
                "sessions.sidebar.session.moveToWorktree.tooltipBusy"
            }
            Self::MoveSession => "sessions.moveSession.hint",
            Self::Archive => "sessions.sidebar.session.menuHint.archive",
            Self::Delete => "sessions.sidebar.session.menuHint.delete",
        }
    }

    /// Whether the action needs a server. Rename opens its field and Copy ID
    /// writes the clipboard without one.
    fn needs_server(self) -> bool {
        !matches!(self, Self::Rename | Self::CopyId)
    }
}

/// What the header menu depends on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HeaderContext {
    /// A server can take the change; false for synthetic data or while
    /// disconnected.
    pub(crate) available: bool,
    /// An archive or delete of the session is in flight.
    pub(crate) removing: bool,
    /// A rename of the session is in flight.
    pub(crate) renaming: bool,
    /// The session's directory is known.
    pub(crate) has_directory: bool,
    /// The session has a parent.
    pub(crate) subtask: bool,
    /// The session lives in an isolated space.
    pub(crate) in_space: bool,
    /// The session is a managed chat, outside every project.
    pub(crate) in_chats: bool,
    /// A turn of the session is running.
    pub(crate) busy: bool,
    /// A move of the session to a worktree or project is in flight.
    pub(crate) moving: bool,
    pub(crate) ai_rename: AiRenameState,
}

/// One line of the header menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeaderEntry {
    Item { action: HeaderAction, enabled: bool },
    Separator,
}

impl HeaderEntry {
    pub(crate) fn action(self) -> Option<HeaderAction> {
        match self {
            Self::Item { action, .. } => Some(action),
            Self::Separator => None,
        }
    }

    pub(crate) fn is_enabled(self) -> bool {
        matches!(self, Self::Item { enabled: true, .. })
    }
}

/// The menu in the reference's order. Move to worktree is listed for a root
/// session outside Chats, Move session for a root session outside a space;
/// both wait for a running turn and a move in flight.
pub(crate) fn header_entries(context: &HeaderContext) -> Vec<HeaderEntry> {
    let enabled = |action: HeaderAction| {
        let blocked = match action {
            HeaderAction::Rename => context.renaming,
            HeaderAction::AiRename => {
                !context.has_directory
                    || matches!(
                        context.ai_rename,
                        AiRenameState::Checking
                            | AiRenameState::Empty
                            | AiRenameState::NoSmallModel
                            | AiRenameState::Pending
                    )
            }
            HeaderAction::MoveToWorktree | HeaderAction::MoveSession => {
                !context.has_directory || context.busy || context.moving
            }
            _ => false,
        };
        let removing = context.removing && action != HeaderAction::CopyId;
        (context.available || !action.needs_server()) && !blocked && !removing
    };
    let item = |action| HeaderEntry::Item {
        action,
        enabled: enabled(action),
    };
    let mut entries = vec![
        item(HeaderAction::Rename),
        item(HeaderAction::AiRename),
        item(HeaderAction::CopyId),
        HeaderEntry::Separator,
        item(HeaderAction::ExportMarkdown),
    ];
    if !context.in_chats && !context.subtask {
        entries.push(item(HeaderAction::MoveToWorktree));
    }
    if !context.subtask && !context.in_space {
        entries.push(item(HeaderAction::MoveSession));
    }
    entries.push(HeaderEntry::Separator);
    entries.push(item(HeaderAction::Archive));
    entries.push(item(HeaderAction::Delete));
    entries
}

/// The enabled action `step` lines from `from`, wrapping around.
fn step_action(
    entries: &[HeaderEntry],
    from: Option<HeaderAction>,
    step: isize,
) -> Option<HeaderAction> {
    let at = from.and_then(|action| {
        entries
            .iter()
            .position(|entry| entry.action() == Some(action))
    });
    step_highlight_by(entries.len(), |index| entries[index].is_enabled(), at, step)
        .and_then(|index| entries[index].action())
}

/// The first or last enabled action.
fn edge_action(entries: &[HeaderEntry], last: bool) -> Option<HeaderAction> {
    let mut enabled = entries.iter().filter(|entry| entry.is_enabled());
    let entry = if last {
        enabled.next_back()
    } else {
        enabled.next()
    };
    entry.and_then(|entry| entry.action())
}

/// The shortcut of `action` as the Keyboard Shortcuts page writes it; `None`
/// for an action without one and for an unassigned binding.
pub(crate) fn shortcut_label(action: HeaderAction, overrides: &Overrides) -> Option<SharedString> {
    let combo = shortcuts::effective_combo(action.shortcut_id()?, overrides);
    let label = shortcuts::format_for_display(&combo, "", Platform::current());
    (!label.is_empty()).then(|| label.into())
}

fn shortcut_overrides(cx: &App) -> Overrides {
    cx.try_global::<ShortcutBindings>()
        .map(|bindings| bindings.overrides.clone())
        .unwrap_or_default()
}

/// The open header menu.
pub(crate) struct HeaderSessionMenu {
    pub(crate) session_id: SessionId,
    pub(crate) highlighted: Option<HeaderAction>,
    pub(crate) focus: FocusHandle,
    /// What had focus before the menu opened.
    return_focus: Option<FocusHandle>,
}

impl Workspace {
    /// What the header menu of `session` depends on now.
    pub(crate) fn header_menu_context(&self, session_id: &SessionId) -> Option<HeaderContext> {
        let session = self.session(session_id)?;
        let key = self.mutation_key(session_id);
        let directory = session.resolved_directory();
        let management = &self.session_management;
        Some(HeaderContext {
            available: self.session_actions_available(),
            removing: management.removing.contains(&key),
            renaming: management.renaming.contains(&key),
            has_directory: directory.is_some(),
            subtask: session.parent_id.is_some(),
            in_space: directory.as_deref().is_some_and(is_space_directory),
            in_chats: is_chat_directory(
                self.topology.chat_roots.as_ref(),
                session.directory.as_deref(),
            ),
            busy: matches!(
                self.live_status.get(session_id),
                Some(LiveStatus::Busy | LiveStatus::Retry)
            ),
            moving: self.worktree_move_pending(session_id),
            ai_rename: self.ai_rename_state(session_id),
        })
    }

    /// Opens the menu for the selected session, or closes it when it is
    /// open. Keyboard opening highlights the first item.
    pub(crate) fn open_header_session_menu(
        &mut self,
        keyboard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.header_session_menu.is_some() {
            self.close_header_session_menu(true, window, cx);
            return;
        }
        let Some(session_id) = self.selected.clone() else {
            return;
        };
        let Some(context) = self.header_menu_context(&session_id) else {
            return;
        };
        // One session menu at a time.
        self.close_session_menu(false, window, cx);
        let return_focus = window.focused(cx);
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.header_session_menu = Some(HeaderSessionMenu {
            highlighted: keyboard
                .then(|| edge_action(&header_entries(&context), false))
                .flatten(),
            session_id: session_id.clone(),
            focus,
            return_focus,
        });
        self.check_ai_rename(&session_id, cx);
        cx.notify();
    }

    pub(crate) fn close_header_session_menu(
        &mut self,
        restore_focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.header_session_menu.take() else {
            return;
        };
        self.stop_ai_rename_check();
        if restore_focus && let Some(focus) = menu.return_focus {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// Before each frame: the menu and the header's rename belong to the
    /// session they were opened for, and leave with it.
    pub(crate) fn settle_header_session_menu(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let stale = |session: &SessionId| {
            self.selected.as_ref() != Some(session) || self.session(session).is_none()
        };
        let menu_stale = self
            .header_session_menu
            .as_ref()
            .is_some_and(|menu| stale(&menu.session_id));
        let rename_stale = self
            .session_rename
            .as_ref()
            .is_some_and(|rename| rename.row_key.as_ref() == ROW_KEY && stale(&rename.session_id));
        if menu_stale {
            self.close_header_session_menu(false, window, cx);
        }
        if rename_stale {
            self.session_rename = None;
            cx.notify();
        }
    }

    /// Runs `action` of the open menu when the session still offers it.
    pub(crate) fn activate_header_action(
        &mut self,
        action: HeaderAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.header_session_menu.as_ref() else {
            return;
        };
        let session_id = menu.session_id.clone();
        let offered = self
            .header_menu_context(&session_id)
            .is_some_and(|context| {
                header_entries(&context)
                    .iter()
                    .any(|entry| entry.action() == Some(action) && entry.is_enabled())
            });
        if !offered {
            return;
        }
        // A rename takes focus into its field; anything else returns it.
        self.close_header_session_menu(action != HeaderAction::Rename, window, cx);
        match action {
            HeaderAction::Rename => self.start_rename(ROW_KEY.into(), session_id, window, cx),
            HeaderAction::AiRename => self.ai_rename_session(&session_id, window, cx),
            HeaderAction::CopyId => self.copy_session_id(&session_id, window, cx),
            HeaderAction::ExportMarkdown => self.export_session_markdown(&session_id, window, cx),
            HeaderAction::MoveToWorktree => {
                self.move_session_to_new_worktree(&session_id, window, cx)
            }
            HeaderAction::MoveSession => self.open_move_session_dialog(&session_id, window, cx),
            HeaderAction::Archive => self.request_removal(&session_id, false, window, cx),
            HeaderAction::Delete => self.request_removal(&session_id, true, window, cx),
        }
    }

    /// The rename shortcut: opens the header's rename field for the selected
    /// session. `false` leaves the key to others.
    pub(crate) fn rename_current_session(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session_id) = self.selected.clone() else {
            return false;
        };
        if self.session(&session_id).is_none() {
            return false;
        }
        self.close_header_session_menu(false, window, cx);
        self.start_rename(ROW_KEY.into(), session_id, window, cx);
        true
    }

    /// The archive shortcut: archives the selected session as the menu's
    /// Archive does. `false` leaves the key to others.
    pub(crate) fn archive_current_session(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session_id) = self.selected.clone() else {
            return false;
        };
        if self.session(&session_id).is_none() {
            return false;
        }
        self.close_header_session_menu(false, window, cx);
        self.request_removal(&session_id, false, window, cx);
        true
    }

    fn header_menu_entries(&self) -> Vec<HeaderEntry> {
        self.header_session_menu
            .as_ref()
            .and_then(|menu| self.header_menu_context(&menu.session_id))
            .map(|context| header_entries(&context))
            .unwrap_or_default()
    }

    fn step_header_session_menu(&mut self, step: isize, cx: &mut Context<Self>) {
        let entries = self.header_menu_entries();
        if let Some(menu) = self.header_session_menu.as_mut() {
            menu.highlighted = step_action(&entries, menu.highlighted, step);
            cx.notify();
        }
    }

    fn edge_header_session_menu(&mut self, last: bool, cx: &mut Context<Self>) {
        let entries = self.header_menu_entries();
        if let Some(menu) = self.header_session_menu.as_mut() {
            menu.highlighted = edge_action(&entries, last);
            cx.notify();
        }
    }

    fn confirm_header_session_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let highlighted = self
            .header_session_menu
            .as_ref()
            .and_then(|menu| menu.highlighted);
        if let Some(action) = highlighted {
            self.activate_header_action(action, window, cx);
        }
    }

    /// A press outside the menu closes it. When that press lands on the More
    /// button, its click finds the same press recorded and leaves the menu
    /// closed, as a toggle.
    fn dismiss_header_session_menu(
        &mut self,
        press: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session_management.menu_dismissed = Some((ROW_KEY.into(), press));
        self.close_header_session_menu(false, window, cx);
    }

    /// The More button beside the title, with its menu while open. It sits
    /// at the title line's height when a metadata line runs under the title.
    pub(crate) fn render_header_session_actions(
        &self,
        session_id: &SessionId,
        show_metadata: bool,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let open = self
            .header_session_menu
            .as_ref()
            .filter(|menu| menu.session_id == *session_id);
        let foreground = palette.foreground;
        let workspace = cx.entity().downgrade();
        let button = div()
            .id(TRIGGER_ID)
            .group(TRIGGER_ID)
            .role(Role::Button)
            .accessibility_id(TRIGGER_ID)
            .aria_label(strings.get("header.sessionActions.openAria"))
            .aria_expanded(open.is_some())
            .w(px(TRIGGER_WIDTH))
            .h(px(TRIGGER_HEIGHT))
            .rounded(px(CONTROL_RADIUS))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            // A press here is the button's, never the start of a window move.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                cx.stop_propagation();
                let dismissed = this.session_management.menu_dismissed.take();
                let press = match event {
                    ClickEvent::Mouse(click) => Some(click.down.position),
                    _ => None,
                };
                if dismissed.is_some_and(|(key, at)| key.as_ref() == ROW_KEY && Some(at) == press) {
                    return;
                }
                this.open_header_session_menu(event.is_keyboard(), window, cx);
            }))
            .on_a11y_action(AccessibleAction::Click, move |_, window, cx| {
                let _ = workspace.update(cx, |this, cx| {
                    this.open_header_session_menu(true, window, cx)
                });
            })
            .child(
                glyph("more", 16.0, palette.muted_foreground)
                    .group_hover(TRIGGER_ID, move |style| style.text_color(foreground)),
            )
            .test_support();
        div()
            .relative()
            .h(px(TRIGGER_HEIGHT))
            .flex_shrink_0()
            .when(show_metadata, |this| this.self_start())
            .child(button)
            // The menu's corner sits at the button's bottom end.
            .children(open.map(|menu| {
                div()
                    .absolute()
                    .top(px(TRIGGER_HEIGHT))
                    .inset_end(direction, px(0.0))
                    .child(self.render_header_session_menu(menu, palette, direction, cx))
            }))
            .into_any_element()
    }

    fn render_header_session_menu(
        &self,
        menu: &HeaderSessionMenu,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let context = self
            .header_menu_context(&menu.session_id)
            .unwrap_or_default();
        let overrides = shortcut_overrides(cx);
        let items = header_entries(&context)
            .into_iter()
            .map(|entry| {
                let HeaderEntry::Item { action, enabled } = entry else {
                    return div()
                        .mx(px(-4.0))
                        .my(px(2.0))
                        .h(px(1.0))
                        .bg(palette.border)
                        .into_any_element();
                };
                let row = header_menu_row(
                    HeaderMenuRow {
                        action,
                        label: strings.get(action.label_key()),
                        hint: strings.get(action.hint_key(&context)),
                        shortcut: shortcut_label(action, &overrides),
                        enabled,
                        highlighted: menu.highlighted == Some(action),
                    },
                    palette,
                    direction,
                    cx,
                )
                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                    if enabled
                        && let Some(menu) = this.header_session_menu.as_mut()
                        && menu.highlighted != Some(action)
                    {
                        menu.highlighted = Some(action);
                        cx.notify();
                    }
                }));
                if enabled {
                    on_activate(row, cx, move |this, window, cx| {
                        this.activate_header_action(action, window, cx);
                    })
                    .test_support()
                    .into_any_element()
                } else {
                    row.test_support().into_any_element()
                }
            })
            .collect::<Vec<_>>();
        let popup =
            div()
                .id(MENU_ID)
                .role(Role::Menu)
                .accessibility_id(MENU_ID)
                .aria_label(strings.get("header.sessionActions.openAria"))
                .track_focus(&menu.focus)
                .key_context(MENU_CONTEXT)
                .on_action(
                    cx.listener(|this, _: &SelectDown, _, cx| this.step_header_session_menu(1, cx)),
                )
                .on_action(
                    cx.listener(|this, _: &SelectUp, _, cx| this.step_header_session_menu(-1, cx)),
                )
                .on_action(cx.listener(|this, _: &SelectFirst, _, cx| {
                    this.edge_header_session_menu(false, cx)
                }))
                .on_action(cx.listener(|this, _: &SelectLast, _, cx| {
                    this.edge_header_session_menu(true, cx)
                }))
                .on_action(cx.listener(|this, _: &Confirm, window, cx| {
                    this.confirm_header_session_menu(window, cx)
                }))
                .on_action(cx.listener(|this, _: &Cancel, window, cx| {
                    this.close_header_session_menu(true, window, cx)
                }))
                .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.dismiss_header_session_menu(event.position, window, cx)
                }))
                .occlude()
                .min_w(px(MENU_MIN_WIDTH))
                .p(px(4.0))
                .rounded(px(12.0))
                .bg(palette.elevated)
                .shadow(elevation::floating(palette))
                .flex()
                .flex_col()
                .children(items)
                .test_support();
        // `align="end"`: the menu's end edge under the button's.
        let anchor = match direction {
            Direction::LeftToRight => Anchor::TopRight,
            Direction::RightToLeft => Anchor::TopLeft,
        };
        deferred(
            anchored()
                .anchor(anchor)
                .snap_to_window_with_margin(Edges::all(px(8.0)))
                .child(div().mt(px(4.0)).child(popup)),
        )
        .with_priority(1)
        .into_any_element()
    }

    /// The rename field in the title's place, with Save and Cancel, while
    /// the header renames `session_id`.
    pub(crate) fn render_header_rename(
        &self,
        session_id: &SessionId,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let rename = self.session_rename.as_ref().filter(|rename| {
            rename.row_key.as_ref() == ROW_KEY && rename.session_id == *session_id
        })?;
        let strings = locale::strings(cx);
        let input = rename.input.clone();
        let button = |id: &'static str, icon: &'static str, label: SharedString| {
            let foreground = palette.foreground;
            let tip = label.clone();
            div()
                .id(id)
                .group(id)
                .role(Role::Button)
                .accessibility_id(id)
                .aria_label(label)
                .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                .flex_shrink_0()
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    glyph(icon, 16.0, palette.muted_foreground)
                        .group_hover(id, move |style| style.text_color(foreground)),
                )
        };
        let save = on_activate(
            button(
                "header-session-rename-save",
                "check",
                strings.get("sessions.sidebar.session.rename.save"),
            ),
            cx,
            |this, window, cx| this.save_rename(window, cx),
        )
        .test_support();
        let cancel = on_activate(
            button(
                "header-session-rename-cancel",
                "close",
                strings.get("sessions.sidebar.session.rename.cancel"),
            ),
            cx,
            |this, window, cx| this.cancel_rename(window, cx),
        )
        .test_support();
        let escape_input = input.clone();
        Some(
            div()
                .id(RENAME_ID)
                .accessibility_id(RENAME_ID)
                .role(Role::Group)
                .aria_label(strings.get("sessions.sidebar.session.menu.rename"))
                .min_w_0()
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                // A press in the field edits the title; it never moves the
                // window.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                // Escape cancels unless it ends an IME composition, which
                // the field handles. Either way it stops here, so it never
                // reaches the workspace's Stop.
                .capture_action(cx.listener(move |this, _: &Escape, window, cx| {
                    let composing = escape_input.update(cx, |input, cx| {
                        input.marked_text_range(window, cx).is_some()
                    });
                    if !composing {
                        cx.stop_propagation();
                        this.cancel_rename(window, cx);
                    }
                }))
                .on_action(|_: &Escape, _, cx| cx.stop_propagation())
                // A press outside the field saves, as an outside click does.
                .on_mouse_down_out(cx.listener(|this, _, window, cx| this.save_rename(window, cx)))
                .child(
                    div().w(px(RENAME_WIDTH)).min_w_0().child(
                        Input::new(&input)
                            .appearance(false)
                            .bordered(false)
                            .text_size(rems(TITLE_SIZE)),
                    ),
                )
                .child(save)
                .child(cancel)
                .test_support()
                .into_any_element(),
        )
    }
}

/// What one line of the header menu shows.
struct HeaderMenuRow {
    action: HeaderAction,
    label: SharedString,
    hint: SharedString,
    shortcut: Option<SharedString>,
    enabled: bool,
    highlighted: bool,
}

/// One 28 px line: icon, label, then the shortcut at the end.
fn header_menu_row(
    row: HeaderMenuRow,
    palette: &Palette,
    direction: Direction,
    cx: &App,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let destructive = row.action.is_destructive();
    let (text, icon): (Hsla, Hsla) = if destructive {
        (palette.error, palette.error)
    } else {
        (palette.elevated_foreground, palette.muted_foreground)
    };
    let id = format!("header-session-menu-item:{}", row.action.id());
    let hint = row.hint.clone();
    div()
        .id(SharedString::from(id.clone()))
        .role(Role::MenuItem)
        .accessibility_id(id)
        .aria_label(row.label.clone())
        .aria_description(row.hint)
        .when(!row.enabled, |this| {
            this.opacity(0.5)
                .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
        })
        .tooltip(move |window, cx| Tooltip::new(hint.clone()).build(window, cx))
        .tooltip_show_delay(HINT_DELAY)
        .h(px(28.0))
        .px(px(8.0))
        .rounded(px(8.0))
        .flex_row_from(direction)
        .items_center()
        .gap(px(8.0))
        .when(row.enabled, |this| this.cursor_pointer())
        .when(row.highlighted, |this| {
            this.bg(if destructive {
                palette.error.opacity(0.1)
            } else {
                palette.hover
            })
        })
        .child(
            div()
                .size(px(16.0))
                .flex()
                .items_center()
                .justify_center()
                .margin_end(direction, px(4.0))
                .child(glyph(row.action.icon(), row.action.icon_size(), icon)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .css_text(LABEL_SIZE, LABEL_LINE, cx)
                .text_color(text)
                .child(row.label),
        )
        .children(row.shortcut.map(|shortcut| {
            div()
                .flex_shrink_0()
                .padding_start(direction, px(12.0))
                .text_size(rems(META_SIZE))
                .text_color(palette.muted_foreground)
                .child(shortcut)
        }))
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, ElementId, Entity, TestAppContext};

    use super::*;
    use crate::workspace_tests::open;

    fn ready() -> HeaderContext {
        HeaderContext {
            available: true,
            has_directory: true,
            ai_rename: AiRenameState::Ready,
            ..HeaderContext::default()
        }
    }

    fn actions(entries: &[HeaderEntry]) -> Vec<Option<HeaderAction>> {
        entries.iter().map(|entry| entry.action()).collect()
    }

    fn enabled(entries: &[HeaderEntry]) -> Vec<HeaderAction> {
        entries
            .iter()
            .filter(|entry| entry.is_enabled())
            .filter_map(|entry| entry.action())
            .collect()
    }

    fn name(id: impl Into<String>) -> ElementId {
        ElementId::Name(SharedString::from(id.into()))
    }

    fn update<T>(
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) -> T,
    ) -> T {
        cx.update_window(window, |_, window, cx| {
            let result = workspace.update(cx, |workspace, cx| f(workspace, window, cx));
            window.render_frame(cx);
            result
        })
        .expect("window is open")
    }

    /// Selects a root session with a directory and returns it.
    fn select_root(
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
    ) -> SessionId {
        update(window, workspace, cx, |this, window, cx| {
            let session = this
                .sessions
                .active()
                .iter()
                .find(|record| record.parent_id.is_none() && record.directory.is_some())
                .expect("a root session")
                .id
                .clone();
            this.select_session(session.clone(), window, cx);
            session
        })
    }

    #[test]
    fn the_menu_follows_the_reference_order() {
        use HeaderAction::*;
        assert_eq!(
            actions(&header_entries(&ready())),
            [
                Some(Rename),
                Some(AiRename),
                Some(CopyId),
                None,
                Some(ExportMarkdown),
                Some(MoveToWorktree),
                Some(MoveSession),
                None,
                Some(Archive),
                Some(Delete),
            ]
        );
    }

    #[test]
    fn chats_subtasks_and_spaces_drop_the_moves_the_reference_drops() {
        use HeaderAction::*;
        let chat = actions(&header_entries(&HeaderContext {
            in_chats: true,
            ..ready()
        }));
        assert!(!chat.contains(&Some(MoveToWorktree)));
        assert!(chat.contains(&Some(MoveSession)));
        let space = actions(&header_entries(&HeaderContext {
            in_space: true,
            ..ready()
        }));
        assert!(space.contains(&Some(MoveToWorktree)));
        assert!(!space.contains(&Some(MoveSession)));
        let subtask = actions(&header_entries(&HeaderContext {
            subtask: true,
            ..ready()
        }));
        assert_eq!(
            subtask,
            [
                Some(Rename),
                Some(AiRename),
                Some(CopyId),
                None,
                Some(ExportMarkdown),
                None,
                Some(Archive),
                Some(Delete),
            ]
        );
    }

    #[test]
    fn items_are_disabled_only_while_they_cannot_run() {
        use HeaderAction::*;
        let offline = header_entries(&HeaderContext {
            available: false,
            ..ready()
        });
        assert_eq!(enabled(&offline), [Rename, CopyId], "no server, no other");
        let removing = header_entries(&HeaderContext {
            removing: true,
            ..ready()
        });
        assert_eq!(enabled(&removing), [CopyId]);
        let renaming = header_entries(&HeaderContext {
            renaming: true,
            ..ready()
        });
        assert!(!enabled(&renaming).contains(&Rename));
        assert!(enabled(&renaming).contains(&Archive));
        for context in [
            HeaderContext {
                busy: true,
                ..ready()
            },
            HeaderContext {
                moving: true,
                ..ready()
            },
            HeaderContext {
                has_directory: false,
                ..ready()
            },
        ] {
            let entries = enabled(&header_entries(&context));
            assert!(!entries.contains(&MoveToWorktree), "{context:?}");
            assert!(!entries.contains(&MoveSession), "{context:?}");
            assert!(entries.contains(&ExportMarkdown), "{context:?}");
        }
        for state in [
            AiRenameState::Checking,
            AiRenameState::Empty,
            AiRenameState::NoSmallModel,
            AiRenameState::Pending,
        ] {
            let entries = header_entries(&HeaderContext {
                ai_rename: state,
                ..ready()
            });
            assert!(!enabled(&entries).contains(&AiRename), "{state:?}");
        }
        let unknown = header_entries(&HeaderContext {
            ai_rename: AiRenameState::Unknown,
            ..ready()
        });
        assert!(enabled(&unknown).contains(&AiRename), "the run reports");
    }

    #[test]
    fn move_hints_explain_a_running_turn_and_a_move_in_flight() {
        let busy = HeaderContext {
            busy: true,
            ..ready()
        };
        let moving = HeaderContext {
            moving: true,
            busy: true,
            ..ready()
        };
        assert_eq!(
            HeaderAction::MoveToWorktree.hint_key(&ready()),
            "sessions.sidebar.session.moveToWorktree.tooltip"
        );
        assert_eq!(
            HeaderAction::MoveToWorktree.hint_key(&busy),
            "sessions.sidebar.session.moveToWorktree.tooltipBusy"
        );
        assert_eq!(
            HeaderAction::MoveToWorktree.hint_key(&moving),
            "sessions.sidebar.session.moveToWorktree.tooltipMoving"
        );
        assert_eq!(
            HeaderAction::MoveSession.hint_key(&ready()),
            "sessions.moveSession.hint"
        );
        assert_eq!(
            HeaderAction::MoveSession.hint_key(&busy),
            "sessions.sidebar.session.moveToWorktree.tooltipBusy"
        );
    }

    #[test]
    fn stepping_skips_separators_and_disabled_items_and_wraps() {
        use HeaderAction::*;
        let entries = header_entries(&HeaderContext {
            available: false,
            ..ready()
        });
        assert_eq!(step_action(&entries, None, 1), Some(Rename));
        assert_eq!(step_action(&entries, Some(Rename), 1), Some(CopyId));
        assert_eq!(step_action(&entries, Some(CopyId), 1), Some(Rename));
        assert_eq!(step_action(&entries, Some(Rename), -1), Some(CopyId));
        assert_eq!(edge_action(&entries, true), Some(CopyId));
        let all = header_entries(&ready());
        assert_eq!(edge_action(&all, true), Some(Delete));
        assert_eq!(step_action(&all, Some(CopyId), 1), Some(ExportMarkdown));
    }

    #[test]
    fn rename_and_archive_show_their_effective_shortcuts() {
        let defaults = Overrides::new();
        let shown = |combo: &str| -> SharedString {
            shortcuts::format_for_display(combo, "", Platform::current()).into()
        };
        assert_eq!(
            shortcut_label(HeaderAction::Rename, &defaults),
            Some(shown("mod+k r"))
        );
        assert_eq!(
            shortcut_label(HeaderAction::Archive, &defaults),
            Some(shown("mod+k backspace"))
        );
        assert_eq!(shortcut_label(HeaderAction::Delete, &defaults), None);
        let rebound = shortcuts::save_override(&defaults, "rename_current_session", "alt+r", None);
        assert_eq!(
            shortcut_label(HeaderAction::Rename, &rebound),
            Some(shown("alt+r"))
        );
        let unassigned = shortcuts::save_override(
            &defaults,
            "archive_current_session",
            shortcuts::UNASSIGNED,
            None,
        );
        assert_eq!(shortcut_label(HeaderAction::Archive, &unassigned), None);
    }

    #[gpui_kit::test]
    fn the_more_button_opens_and_closes_the_menu(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let session = select_root(window, &workspace, cx);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(name(TRIGGER_ID), cx);
            window.render_frame(cx);
            assert!(window.try_find(name(MENU_ID)).is_some());
            for id in [
                "rename",
                "ai-rename",
                "copy-id",
                "export-markdown",
                "archive",
                "delete",
            ] {
                assert!(
                    window
                        .try_find(name(format!("header-session-menu-item:{id}")))
                        .is_some(),
                    "{id} is listed"
                );
            }
            assert!(
                window
                    .try_find(name("header-session-menu-item:mini-chat"))
                    .is_none(),
                "an item without a native feature is not listed"
            );
        })
        .expect("window is open");
        cx.read(|cx| {
            let state = workspace.read(cx);
            let menu = state
                .header_session_menu
                .as_ref()
                .expect("the menu is open");
            assert_eq!(menu.session_id, session);
            assert_eq!(
                menu.highlighted, None,
                "a pointer opens without a highlight"
            );
            let context = state.header_menu_context(&session).expect("selected");
            assert_eq!(
                enabled(&header_entries(&context)),
                [HeaderAction::Rename, HeaderAction::CopyId],
                "synthetic data has no server to change"
            );
        });
        cx.update_window(window, |_, window, cx| {
            let focused = workspace
                .read(cx)
                .header_session_menu
                .as_ref()
                .map(|menu| menu.focus.is_focused(window));
            assert_eq!(focused, Some(true), "the open menu takes focus");
            // A second click on the button closes it again.
            window.click(name(TRIGGER_ID), cx);
            window.render_frame(cx);
            assert!(window.try_find(name(MENU_ID)).is_none());
        })
        .expect("window is open");
        assert!(cx.read(|cx| workspace.read(cx).header_session_menu.is_none()));
    }

    #[gpui_kit::test]
    fn keyboard_opening_highlights_and_steps_over_disabled_items(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        select_root(window, &workspace, cx);
        update(window, &workspace, cx, |this, window, cx| {
            this.open_header_session_menu(true, window, cx);
        });
        let highlighted = |cx: &mut TestAppContext| {
            cx.read(|cx| {
                workspace
                    .read(cx)
                    .header_session_menu
                    .as_ref()
                    .and_then(|menu| menu.highlighted)
            })
        };
        assert_eq!(highlighted(cx), Some(HeaderAction::Rename));
        update(window, &workspace, cx, |this, _, cx| {
            this.step_header_session_menu(1, cx);
        });
        assert_eq!(
            highlighted(cx),
            Some(HeaderAction::CopyId),
            "AI rename has no server to run on"
        );
        update(window, &workspace, cx, |this, window, cx| {
            this.close_header_session_menu(true, window, cx);
        });
        assert_eq!(highlighted(cx), None);
    }

    #[gpui_kit::test]
    fn a_disabled_item_runs_nothing(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        select_root(window, &workspace, cx);
        update(window, &workspace, cx, |this, window, cx| {
            this.open_header_session_menu(false, window, cx);
            this.activate_header_action(HeaderAction::Delete, window, cx);
        });
        cx.read(|cx| {
            let state = workspace.read(cx);
            assert!(
                state.header_session_menu.is_some(),
                "the menu stays open on an item that cannot run"
            );
            assert!(state.session_management.confirm.is_none());
        });
    }

    #[gpui_kit::test]
    fn rename_opens_the_field_in_the_title_and_cancel_restores_it(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let session = select_root(window, &workspace, cx);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(name(TRIGGER_ID), cx);
            window.render_frame(cx);
            window.click(name("header-session-menu-item:rename"), cx);
            window.render_frame(cx);
            assert!(window.try_find(name(MENU_ID)).is_none());
            assert!(window.try_find(name(RENAME_ID)).is_some());
            assert!(
                window.try_find(name(TRIGGER_ID)).is_none(),
                "the More button leaves while the title is renamed"
            );
            assert!(window.try_find(name("session-title")).is_none());
        })
        .expect("window is open");
        cx.read(|cx| {
            let state = workspace.read(cx);
            let rename = state.session_rename.as_ref().expect("renaming");
            assert_eq!(rename.row_key.as_ref(), ROW_KEY);
            assert_eq!(rename.session_id, session);
            let title = state.session(&session).expect("listed").title.clone();
            if !title.is_empty() {
                assert_eq!(rename.input.read(cx).value().to_string(), title);
            }
        });
        cx.update_window(window, |_, window, cx| {
            window.click(name("header-session-rename-cancel"), cx);
            window.render_frame(cx);
            assert!(window.try_find(name(RENAME_ID)).is_none());
            assert!(window.try_find(name("session-title")).is_some());
            assert!(window.try_find(name(TRIGGER_ID)).is_some());
        })
        .expect("window is open");
    }

    #[gpui_kit::test]
    fn the_menu_and_the_rename_leave_with_their_session(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let session = select_root(window, &workspace, cx);
        update(window, &workspace, cx, |this, window, cx| {
            this.open_header_session_menu(false, window, cx);
        });
        let other = update(window, &workspace, cx, |this, window, cx| {
            let other = this
                .sessions
                .active()
                .iter()
                .find(|record| {
                    record.id != session && record.parent_id.is_none() && record.directory.is_some()
                })
                .expect("a second root session")
                .id
                .clone();
            this.select_session(other.clone(), window, cx);
            other
        });
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .expect("window is open");
        assert!(cx.read(|cx| workspace.read(cx).header_session_menu.is_none()));
        update(window, &workspace, cx, |this, window, cx| {
            assert!(this.rename_current_session(window, cx));
        });
        update(window, &workspace, cx, |this, window, cx| {
            this.select_session(session.clone(), window, cx);
        });
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .expect("window is open");
        cx.read(|cx| {
            let state = workspace.read(cx);
            assert_eq!(state.selected.as_ref(), Some(&session));
            assert_ne!(session, other);
            assert!(
                state.session_rename.is_none(),
                "a rename of the session left behind closes unsaved"
            );
        });
    }

    #[gpui_kit::test]
    fn copy_id_reports_its_outcome_and_closes_the_menu(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        select_root(window, &workspace, cx);
        update(window, &workspace, cx, |this, window, cx| {
            this.open_header_session_menu(false, window, cx);
            this.activate_header_action(HeaderAction::CopyId, window, cx);
        });
        cx.read(|cx| {
            let state = workspace.read(cx);
            assert!(state.header_session_menu.is_none());
            assert!(
                state.session_management.notice.is_some(),
                "copied or not, the outcome is said"
            );
        });
    }

    #[gpui_kit::test]
    fn the_shortcuts_need_a_selected_session(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        update(window, &workspace, cx, |this, window, cx| {
            assert!(!this.rename_current_session(window, cx));
            assert!(!this.archive_current_session(window, cx));
        });
        let session = select_root(window, &workspace, cx);
        update(window, &workspace, cx, |this, window, cx| {
            assert!(this.archive_current_session(window, cx));
            assert!(
                this.session_management.confirm.is_none()
                    && !this
                        .session_management
                        .removing
                        .contains(&this.mutation_key(&session)),
                "synthetic data has no server to archive on"
            );
        });
    }
}
