//! The window layout of the frameless desktop window: the sidebar column at
//! the inline start; the main column with its header, the chat, the work
//! status panel and the panel rail.

use gpui_kit::{
    Context, InteractiveElement, IntoElement, KeyBinding, MouseButton, ParentElement, Render, Role,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window, actions, canvas, div,
    prelude::FluentBuilder, px,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{self, ButtonTone, CONTROL_RADIUS, IconButton, TITLE_ROW_HEIGHT, glyph};
use crate::diagnostics;
use crate::fonts;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

actions!(
    openchamber,
    [
        /// Selects the next session row.
        SelectNextSession,
        /// Selects the previous session row.
        SelectPreviousSession,
        /// Stops the selected session's response.
        StopGenerating,
        /// Turns line wrapping in code blocks on or off.
        ToggleCodeWrap,
        /// Moves the open composer picker's selection down.
        PickerNext,
        /// Moves the open composer picker's selection up.
        PickerPrevious,
    ]
);

/// The key context of the composer while its autocomplete picker is open.
/// Deeper than the workspace's, so the picker's Ctrl+N/P win over shortcuts.
pub(crate) const PICKER_CONTEXT: &str = "ComposerPicker";

pub(crate) const KEY_CONTEXT: &str = "OpenChamberWorkspace";
/// The header's session title: 14/17.5 px.
const TITLE_SIZE: f32 = 0.875;
const TITLE_LINE: f32 = 1.093_75;

/// Registers the workspace's keyboard shortcuts.
pub fn bind_keys(cx: &mut gpui_kit::App) {
    let menu = Some(crate::session_menu::MENU_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("escape", StopGenerating, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-n", PickerNext, Some(PICKER_CONTEXT)),
        KeyBinding::new("ctrl-p", PickerPrevious, Some(PICKER_CONTEXT)),
        KeyBinding::new(
            "ctrl-n",
            gpui_kit::base::actions::SelectDown,
            Some("PopupMenu"),
        ),
        KeyBinding::new(
            "ctrl-p",
            gpui_kit::base::actions::SelectUp,
            Some("PopupMenu"),
        ),
        KeyBinding::new(
            "ctrl-n",
            gpui_kit::base::actions::SelectDown,
            Some("ProjectChooser > List"),
        ),
        KeyBinding::new("down", gpui_kit::base::actions::SelectDown, menu),
        KeyBinding::new("ctrl-n", gpui_kit::base::actions::SelectDown, menu),
        KeyBinding::new("up", gpui_kit::base::actions::SelectUp, menu),
        KeyBinding::new("ctrl-p", gpui_kit::base::actions::SelectUp, menu),
        KeyBinding::new("right", gpui_kit::base::actions::SelectRight, menu),
        KeyBinding::new("left", gpui_kit::base::actions::SelectLeft, menu),
        KeyBinding::new("home", gpui_kit::base::actions::SelectFirst, menu),
        KeyBinding::new("end", gpui_kit::base::actions::SelectLast, menu),
        KeyBinding::new(
            "enter",
            gpui_kit::base::actions::Confirm { secondary: false },
            menu,
        ),
        KeyBinding::new(
            "space",
            gpui_kit::base::actions::Confirm { secondary: false },
            menu,
        ),
        KeyBinding::new("escape", gpui_kit::base::actions::Cancel, menu),
        KeyBinding::new(
            "ctrl-n",
            gpui_kit::NoAction,
            Some(crate::prompt_rail::LIST_CONTEXT),
        ),
        KeyBinding::new(
            "ctrl-p",
            gpui_kit::NoAction,
            Some(crate::prompt_rail::LIST_CONTEXT),
        ),
        KeyBinding::new(
            "ctrl-n",
            gpui_kit::NoAction,
            Some(crate::git_view::UPDATE_PICKER_CONTEXT),
        ),
        KeyBinding::new(
            "ctrl-p",
            gpui_kit::NoAction,
            Some(crate::git_view::UPDATE_PICKER_CONTEXT),
        ),
        KeyBinding::new(
            "escape",
            gpui_kit::NoAction,
            Some(crate::git_view::UPDATE_PICKER_CONTEXT),
        ),
        KeyBinding::new("tab", gpui_kit::base::actions::Cancel, menu),
        KeyBinding::new("shift-tab", gpui_kit::base::actions::Cancel, menu),
        KeyBinding::new(
            "ctrl-p",
            gpui_kit::base::actions::SelectUp,
            Some("ProjectChooser > List"),
        ),
    ]);
    crate::shortcuts_settings::bind_keys(cx);
    // The configurable shortcuts and, after them, the terminal's `NoAction`
    // bindings: a later `NoAction` cancels a pending chord.
    crate::shortcuts::bind(&Default::default(), cx);
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let markers = self.take_frame_markers();
        self.release_retired_networks();
        if let Some(initial_size) = &self.initial_size
            && !initial_size.hold(window)
        {
            self.initial_size = None;
        }
        self.sync_composer_placeholder(window, cx);
        if self.host_recovery.is_some() {
            let palette = cx.global::<Palette>().clone();
            return self
                .render_host_recovery(&palette, window, cx)
                .child(frame_markers(markers));
        }
        if self.managed_failure_visible() {
            let palette = cx.global::<Palette>().clone();
            return self
                .render_managed_failure(&palette, window, cx)
                .child(frame_markers(markers));
        }
        if let Some(variant) = self.connection_recovery() {
            let palette = cx.global::<Palette>().clone();
            return self
                .render_connection_recovery(variant, &palette, window, cx)
                .child(frame_markers(markers));
        }
        if self.authentication.visible {
            let palette = cx.global::<Palette>().clone();
            let direction = locale::strings(cx).direction();
            return self
                .render_authentication(&palette, direction, window, cx)
                .child(frame_markers(markers));
        }
        if let Some(gate) =
            self.render_compatibility_gate(&cx.global::<Palette>().clone(), window, cx)
        {
            return gate.child(frame_markers(markers));
        }
        if self.prompt_panel.is_some()
            && (self
                .send_catalog
                .settings
                .as_ref()
                .and_then(|settings| settings.prompt_navigator_enabled)
                == Some(false)
                || self
                    .selected
                    .as_ref()
                    .and_then(|id| self.open_sessions.get(id))
                    .is_none_or(|open| crate::prompt_rail::prompt_rows(&open.transcript).len() < 2))
        {
            self.prompt_panel = None;
            self.prompt_hover_task = None;
        }
        if self.work_status_fits(window) {
            self.work_status_overlay = false;
        }
        self.ensure_send_catalog(cx);
        self.sync_shortcut_bindings(cx);
        self.ensure_provider_logos(cx);
        self.ensure_project_readouts(cx);
        self.ensure_project_action_runs(cx);
        self.ensure_space_queue(cx);
        self.ensure_project_starters(cx);
        self.ensure_goal_reads(window, cx);
        self.ensure_space_auto_reviews(cx);
        let palette = cx.global::<Palette>().clone();
        let direction = locale::strings(cx).direction();
        let source_board = if self.source_board_open {
            self.render_source_board(cx)
        } else if self.scheduled_tasks_open {
            self.render_scheduled_tasks(cx)
        } else {
            None
        };
        // A full-page surface hides the work status card with the chat.
        let chat_covered = source_board.is_some();
        let chat = if self.archive_open {
            self.render_archive_page(&palette, direction, window, cx)
        } else if let Some(board) = source_board {
            board
        } else if self.selected.is_some() {
            div()
                .id("conversation")
                .relative()
                .flex_1()
                .min_w_0()
                .h_full()
                .bg(palette.chat_background)
                .child(self.render_transcript(&palette, direction, window, cx))
                .child(self.render_composer(&palette, direction, window, cx))
                .into_any_element()
        } else {
            self.render_startup(&palette, direction, window, cx)
                .into_any_element()
        };
        let side_panel = self
            .render_side_panel(&palette, direction, window, cx)
            .or_else(|| self.render_terminal_panel(window, cx));
        let work_status = side_panel.is_none() && !chat_covered;
        div()
            .id("workspace")
            .role(Role::Group)
            .accessibility_id("workspace")
            // Glyphs the interface face lacks come from the stack's
            // `sans-serif` family first, as in Chromium.
            .map(|mut this| {
                this.text_style().font_fallbacks = cx
                    .try_global::<fonts::UiFontFallbacks>()
                    .and_then(|fallbacks| fallbacks.0.clone());
                this
            })
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .capture_key_down(cx.listener(Self::permission_shortcut))
            .on_mouse_move(cx.listener(Self::drag_pane))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::release_pane))
            .on_action(cx.listener(|this, _: &SelectNextSession, window, cx| {
                this.select_adjacent(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectPreviousSession, window, cx| {
                this.select_adjacent(-1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &StopGenerating, window, cx| {
                if this.prompt_panel.is_some() {
                    this.close_prompt_panel(window, cx);
                } else if this.escape_btw(cx) {
                    // An open side conversation collapses first.
                } else {
                    this.escape_to_stop(window, cx);
                }
            }))
            .on_action(
                cx.listener(|this, action: &crate::shortcuts::RunShortcut, window, cx| {
                    if !this.run_shortcut(&action.id, window, cx) {
                        cx.propagate();
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &ToggleCodeWrap, _, cx| {
                this.toggle_code_wrap(cx);
            }))
            .on_action(cx.listener(
                |_, action: &crate::project_chooser::ChooseDraftProject, window, cx| {
                    let directory = action.directory.clone();
                    window.dispatch_action(Box::new(gpui_kit::base::actions::Cancel), cx);
                    cx.defer_in(window, move |this, window, cx| {
                        this.choose_draft_project(&directory, window, cx);
                    });
                },
            ))
            .size_full()
            .flex_row_from(direction)
            .bg(palette.background)
            .text_color(palette.foreground)
            .when(self.sidebar_open, |this| {
                this.child(self.render_sidebar(&palette, direction, cx))
            })
            .child(
                div()
                    .id("main")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(palette.background)
                    .child(self.render_header(&palette, direction, window, cx))
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .flex_row_from(direction)
                            .child(chat)
                            .children(side_panel)
                            .when(work_status && self.work_status_inline(window), |this| {
                                this.child(self.render_work_status_panel(&palette, direction, cx))
                            })
                            .when(work_status && self.work_status_overlay, |this| {
                                this.child(
                                    div()
                                        .absolute()
                                        .top_0()
                                        .bottom_0()
                                        .inset_end(direction, px(crate::context_panel::RAIL_WIDTH))
                                        .child(
                                            self.render_work_status_panel(&palette, direction, cx),
                                        ),
                                )
                            })
                            .child(self.render_rail(&palette, direction, cx)),
                    ),
            )
            .child(self.render_session_notice())
            .children(self.render_app_menu(window, direction))
            .child(frame_markers(markers))
    }
}

fn frame_markers(
    markers: Vec<(
        diagnostics::Marker,
        Option<std::time::Instant>,
        Option<usize>,
    )>,
) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |_, _, _, _| {
            for (marker, started, count) in markers {
                diagnostics::mark(marker, started.map(|started| started.elapsed()), count);
            }
        },
    )
    .size_0()
}

impl Workspace {
    /// The selected session's title and its actions, at the header's start.
    fn render_session_title(
        &self,
        palette: &Palette,
        direction: Direction,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<impl IntoElement> {
        let session = self.selected_session()?;
        let strings = locale::strings(cx);
        let title: SharedString = if session.title.is_empty() {
            strings.get("sessions.sidebar.session.untitled")
        } else {
            session.title.clone().into()
        };
        let foreground = palette.foreground;
        let metadata = (!self.work_status_inline(window))
            .then(|| self.header_project_meta())
            .flatten();
        let show_metadata = metadata.is_some();
        let actions = on_activate(
            div()
                .id("session-actions")
                .group("session-actions")
                .role(Role::Button)
                .accessibility_id("session-actions")
                .aria_label(strings.get("header.sessionActions.openAria"))
                .w(px(24.0))
                .h(px(18.0))
                .when(show_metadata, |this| this.self_start())
                .flex_shrink_0()
                .rounded(px(CONTROL_RADIUS))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    glyph("more", 16.0, palette.muted_foreground)
                        .group_hover("session-actions", move |style| style.text_color(foreground)),
                ),
            cx,
            |_, _, _| {},
        )
        .test_support();
        Some(
            div()
                .min_w_0()
                .margin_end(direction, px(12.0))
                .py(px(2.0))
                .flex_row_from(direction)
                .items_center()
                .gap(px(2.0))
                .child(
                    div()
                        .id("session-title")
                        .accessibility_id("session-title")
                        .role(Role::Heading)
                        .aria_label(title.clone())
                        .min_w_0()
                        .px(px(4.0))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .truncate()
                                .css_text(TITLE_SIZE, TITLE_LINE, cx)
                                .text_color(palette.foreground)
                                .child(title),
                        )
                        .children(metadata.map(|meta| {
                            div()
                                .min_w_0()
                                .flex_row_from(direction)
                                .items_center()
                                .gap(px(6.0))
                                .css_text(0.65625, 0.9375, cx)
                                .text_color(palette.muted_foreground)
                                .children(meta.project.map(|label| div().truncate().child(label)))
                                .children(meta.branch.map(|branch| {
                                    div()
                                        .min_w_0()
                                        .flex_row_from(direction)
                                        .items_center()
                                        .gap(px(2.0))
                                        .child(glyph("git-branch", 12.0, palette.muted_foreground))
                                        .child(div().truncate().child(branch))
                                }))
                        })),
                )
                .child(actions),
        )
    }

    /// The main column's title row. Window controls keep their physical
    /// order and the right edge in every language, as in the web client.
    fn render_header(
        &self,
        palette: &Palette,
        direction: Direction,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        div()
            .id("header")
            .h(px(TITLE_ROW_HEIGHT))
            .flex_shrink_0()
            .flex()
            .bg(palette.background)
            .child(
                chrome::drag_region("header-drag", cx)
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex_row_from(direction)
                    .items_center()
                    .when(!self.sidebar_open, |this| {
                        this.padding_start(direction, px(12.0))
                            .child(self.render_title_controls(palette, direction, cx))
                    })
                    .child(div().w(px(12.0)).flex_shrink_0())
                    .when_some(
                        self.render_session_title(palette, direction, window, cx),
                        |this, title| this.child(title),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(8.0))
                            .children(self.render_project_action(palette, direction, cx))
                            .child(
                                IconButton::new(
                                    "mini-chat",
                                    "picture-in-picture-2",
                                    strings.get("header.actions.newMiniChatAria"),
                                )
                                .render(palette, cx, |_, _, _| {}),
                            )
                            .child(
                                IconButton::new(
                                    "work-status-toggle",
                                    "list-indefinite",
                                    strings.get("header.workStatusPanel.toggleAria"),
                                )
                                .tone(
                                    if self.work_status_inline(window) || self.work_status_overlay {
                                        ButtonTone::Active
                                    } else {
                                        ButtonTone::Quiet
                                    },
                                )
                                .render(
                                    palette,
                                    cx,
                                    |this, window, cx| this.toggle_work_status(window, cx),
                                ),
                            ),
                    )
                    .child(div().w(px(12.0)).flex_shrink_0()),
            )
            .child(chrome::window_controls(window, &strings, palette, cx))
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{KeyContext, Keystroke, TestAppContext};
    use openchamber_native_core::shortcuts::{self as shortcut_schema, Overrides, Platform};

    use super::{KEY_CONTEXT, StopGenerating, bind_keys};
    use crate::git_view::UPDATE_PICKER_CONTEXT;
    use crate::shortcuts::{self, RunShortcut};

    #[gpui_kit::test]
    fn update_picker_keymap_shadows_ancestor_actions_only_while_open(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            bind_keys(cx);
            for overrides in [
                Overrides::new(),
                Overrides::from([("new_chat".to_owned(), "mod+p".to_owned())]),
            ] {
                shortcuts::bind(&overrides, cx);
                let contexts = |open: bool| {
                    let mut names = vec!["Root", KEY_CONTEXT];
                    if open {
                        names.push(UPDATE_PICKER_CONTEXT);
                    }
                    names.push("Input");
                    names
                        .into_iter()
                        .map(|name| KeyContext::parse(name).expect("valid application context"))
                        .collect::<Vec<_>>()
                };
                let bindings = cx.key_bindings();
                let bindings = bindings.borrow();
                for key in ["ctrl-n", "ctrl-p", "escape"] {
                    let key = Keystroke::parse(key).expect("valid picker key");
                    let (actions, pending) = bindings.bindings_for_input(&[key], &contexts(true));
                    assert!(!pending, "picker keys must not start an ancestor chord");
                    assert!(
                        actions.iter().all(|binding| {
                            !binding.action().as_any().is::<StopGenerating>()
                                && !binding.action().as_any().is::<RunShortcut>()
                        }),
                        "an open picker must suppress ancestor Stop and configurable actions"
                    );
                }
                let escape = Keystroke::parse("escape").expect("valid Escape key");
                let (actions, _) = bindings.bindings_for_input(&[escape], &contexts(false));
                assert!(
                    actions
                        .iter()
                        .any(|binding| binding.action().as_any().is::<StopGenerating>()),
                    "closing the picker must retain the workspace Escape action"
                );
                let combo = shortcut_schema::effective_combo("new_chat", &overrides);
                let key = shortcuts::keystrokes(&combo, Platform::current())
                    .expect("New session has a native binding");
                let key = Keystroke::parse(&key).expect("valid New session key");
                let (actions, _) = bindings.bindings_for_input(&[key], &contexts(false));
                assert!(
                    actions.iter().any(|binding| {
                        binding
                            .action()
                            .as_any()
                            .downcast_ref::<RunShortcut>()
                            .is_some_and(|action| action.id == "new_chat")
                    }),
                    "closing the picker must retain the effective New session binding"
                );
            }
        });
    }
}
