//! The Files surface's tree column: the virtualized tree with its keyboard
//! navigation and inline rename, and the search results that replace it
//! while a query is typed.
//!
//! Rows are tree items with their level, position and expanded state, so an
//! accessibility client walks the same structure the pointer sees. As in
//! the web's virtualized tree, only mounted rows are reachable; the arrow
//! keys scroll the selection into view.

use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::{
    AnyElement, AppContext, Context, Entity, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Role, ScrollStrategy, SharedString, StatefulInteractiveElement, Styled,
    Subscription, TestSupportExt, Window, div, prelude::FluentBuilder, px, uniform_list,
};
use openchamber_native_core::file_tree::{EntryKind, ListingCause, ListingFailure, TreeRow};
use openchamber_native_core::path;
use openchamber_native_transport::files::FilesError;

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{CONTROL_RADIUS, glyph};
use crate::files_view::{FilesView, META_LINE, META_SIZE, Notice, PANEL_ROW, file_name};
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::sidebar_view;
use crate::theme::Palette;

/// `FILE_TREE_BASE_PADDING_PX` and `FILE_TREE_INDENT_PX`.
const BASE_PADDING: f32 = 8.0;
const INDENT: f32 = 12.0;
/// The tree's own inset (`px-1.5 py-1.5`).
const TREE_INSET: f32 = 6.0;
/// The room the scroller reserves for its thumb, past the inset.
const THUMB_GUTTER: f32 = 6.0;

/// A rename typed into the tree.
pub(crate) struct TreeRename {
    pub(crate) path: String,
    pub(crate) input: Entity<InputState>,
    _subscription: Subscription,
}

impl FilesView {
    pub(crate) fn render_tree(
        &mut self,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        if let Some(failure) = self.tree.root_failure() {
            return crate::files_view::empty_state(listing_failure_text(failure, cx), palette, cx);
        }
        if self.tree.root_loading() {
            return crate::files_view::empty_state(
                strings.get("filesView.state.loading"),
                palette,
                cx,
            );
        }
        let rows = self.tree.rows();
        let count = rows.len();
        let palette = palette.clone();
        let list = uniform_list(
            "files-tree-rows",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                let rows = this.tree.rows();
                range
                    .filter_map(|index| rows.get(index).cloned())
                    .map(|row| this.render_row(row, &palette, direction, cx))
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.tree_scroll)
        .size_full();
        div()
            .id("files-tree")
            .role(Role::Tree)
            .accessibility_id("files-tree")
            .aria_label(strings.get("contextPanel.mode.files"))
            .track_focus(&self.tree_focus)
            .on_key_down(cx.listener(Self::tree_key))
            .size_full()
            .py(px(TREE_INSET))
            .child(list)
            .into_any_element()
    }

    fn render_row(
        &mut self,
        row: TreeRow,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = format!("files-row:{}", row.path);
        let selected = self.tree.selected() == Some(row.path.as_str());
        let is_open = self
            .open
            .as_ref()
            .is_some_and(|open| open.file.path() == row.path);
        let is_dir = row.kind == EntryKind::Directory;
        let active = selected || is_open;
        let hover = palette.hover;
        let renaming = self
            .rename
            .as_ref()
            .filter(|rename| rename.path == row.path)
            .map(|rename| rename.input.clone());
        let label: AnyElement = match renaming {
            Some(input) => div()
                .flex_1()
                .min_w_0()
                .child(Input::new(&input).appearance(false).bordered(false))
                .into_any_element(),
            None => div()
                .flex_1()
                .min_w_0()
                .truncate()
                .css_text(META_SIZE, META_LINE, cx)
                .text_color(if active {
                    palette.selection_foreground
                } else {
                    // `text-foreground/90` at rest.
                    palette.foreground.opacity(0.9)
                })
                .child(SharedString::from(row.name.clone()))
                .into_any_element(),
        };
        let failure = row
            .failure
            .as_ref()
            .map(|failure| listing_failure_text(failure, cx));
        let path = row.path.clone();
        let pinned = (!is_dir).then(|| row.path.clone());
        // A folder leads with its chevron; a file with its glyph, 2 px in
        // (`ms-0.5`).
        let lead = if is_dir {
            sidebar_view::chevron(!row.expanded, direction, palette).into_any_element()
        } else {
            div()
                .margin_start(direction, px(2.0))
                .flex_shrink_0()
                .child(glyph("file-text", 14.0, palette.muted_foreground))
                .into_any_element()
        };
        let element = div()
            .id(gpui_kit::ElementId::Name(id.clone().into()))
            .role(Role::TreeItem)
            .accessibility_id(SharedString::from(id))
            .aria_label(SharedString::from(row.name.clone()))
            .aria_level(row.depth + 1)
            .aria_position_in_set(row.position)
            .aria_size_of_set(row.siblings)
            .aria_selected(selected)
            .when(is_dir, |this| this.aria_expanded(row.expanded))
            .h(px(PANEL_ROW))
            .w_full()
            .padding_start(direction, px(BASE_PADDING + row.depth as f32 * INDENT))
            .padding_end(direction, px(8.0))
            .rounded(px(CONTROL_RADIUS))
            .flex_row_from(direction)
            .items_center()
            .gap(px(6.0))
            .cursor_pointer()
            .when(active, |this| this.bg(palette.selection))
            .when(!active, |this| this.hover(move |style| style.bg(hover)))
            // A double click keeps the preview tab the first click opened.
            .when_some(pinned, |this, pinned| {
                this.on_click(
                    cx.listener(move |this, event: &gpui_kit::ClickEvent, _, cx| {
                        if event.click_count() >= 2 {
                            this.pin_file(Some(&pinned), cx);
                        }
                    }),
                )
            })
            .child(lead)
            .child(label)
            .children(failure.map(|failure| {
                div()
                    .flex_shrink_0()
                    .max_w(px(120.0))
                    .truncate()
                    .css_text(META_SIZE, META_LINE, cx)
                    .text_color(palette.status_error)
                    .child(failure)
            }));
        let element = on_activate(element, cx, move |this: &mut FilesView, window, cx| {
            this.tree_focus.focus(window, cx);
            this.activate_row(&path, is_dir, window, cx);
        })
        .test_support();
        // The list lays each item out as a root, which sizes it to its
        // content and drops its margins; a full-width box holds the insets.
        div()
            .w_full()
            .padding_start(direction, px(TREE_INSET))
            .padding_end(direction, px(TREE_INSET + THUMB_GUTTER))
            .child(element)
            .into_any_element()
    }

    fn activate_row(
        &mut self,
        path: &str,
        is_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if is_dir {
            self.toggle_folder(path, cx);
        } else {
            self.open_file(path, window, cx);
        }
    }

    /// Arrows move and fold, Enter opens, Home and End jump, F2 renames.
    fn tree_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.rename.is_some() {
            return;
        }
        let rows = self.tree.rows();
        if rows.is_empty() {
            return;
        }
        let index = self
            .tree
            .selected()
            .and_then(|selected| rows.iter().position(|row| row.path == selected));
        let rtl = locale::strings(cx).direction() == Direction::RightToLeft;
        let (forward, back) = if rtl {
            ("left", "right")
        } else {
            ("right", "left")
        };
        let key = event.keystroke.key.as_str();
        let target = match key {
            "down" => Some(index.map_or(0, |index| (index + 1).min(rows.len() - 1))),
            "up" => Some(index.map_or(0, |index| index.saturating_sub(1))),
            "home" => Some(0),
            "end" => Some(rows.len() - 1),
            _ => None,
        };
        if let Some(target) = target {
            self.tree.select(Some(rows[target].path.clone()));
            self.tree_scroll
                .scroll_to_item(target, ScrollStrategy::Nearest);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let Some(index) = index else {
            return;
        };
        let row = &rows[index];
        let is_dir = row.kind == EntryKind::Directory;
        if key == forward && is_dir {
            if row.expanded {
                if let Some(child) = rows.get(index + 1).filter(|next| next.depth > row.depth) {
                    self.tree.select(Some(child.path.clone()));
                }
            } else {
                self.toggle_folder(&row.path.clone(), cx);
            }
        } else if key == back {
            if is_dir && row.expanded {
                self.tree.collapse(&row.path);
                self.remember_tree(cx);
            } else if let Some(parent) = rows[..index]
                .iter()
                .rev()
                .find(|candidate| candidate.depth < row.depth)
            {
                self.tree.select(Some(parent.path.clone()));
            }
        } else if key == "enter" || key == "space" {
            let path = row.path.clone();
            self.activate_row(&path, is_dir, window, cx);
        } else if key == "f2" {
            let path = row.path.clone();
            self.start_rename(&path, window, cx);
        } else {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(crate) fn start_rename(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let name = file_name(path);
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx);
            state.set_value(name, window, cx);
            state
        });
        let subscription =
            cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                InputEvent::PressEnter { .. } => this.commit_rename(window, cx),
                InputEvent::Blur => this.cancel_rename(window, cx),
                _ => {}
            });
        input.update(cx, |input, cx| input.focus(window, cx));
        self.rename = Some(TreeRename {
            path: path.to_owned(),
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    pub(crate) fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.rename.take().is_some() {
            self.tree_focus.focus(window, cx);
            cx.notify();
        }
    }

    /// Renames to the typed name in the same folder. An existing name is
    /// refused by the server and replaces nothing.
    pub(crate) fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.rename.as_ref().is_some_and(|rename| {
            let target = path::parent(&rename.path).map(|parent| {
                format!(
                    "{}/{}",
                    parent.trim_end_matches('/'),
                    rename.input.read(cx).value().trim()
                )
            });
            let exact_retry = target.as_ref().is_some_and(|target| {
                self.uncertain_renames.get(&rename.path) == Some(target)
                    && self.renaming.iter().all(|active| {
                        !path::is_within(&rename.path, active)
                            && !path::is_within(active, &rename.path)
                    })
                    && self.uncertain_renames.keys().all(|other| {
                        other == &rename.path
                            || (!path::is_within(&rename.path, other)
                                && !path::is_within(other, &rename.path))
                    })
            });
            self.save_blocks_rename(&rename.path) && !exact_retry
        }) {
            self.notice = Some(Notice::error(
                locale::strings(cx).get("filesView.editor.saving"),
                None,
            ));
            cx.notify();
            return;
        }
        let Some(rename) = self.rename.take() else {
            return;
        };
        self.tree_focus.focus(window, cx);
        let name = rename.input.read(cx).value().trim().to_owned();
        let strings = locale::strings(cx);
        if name.is_empty() {
            self.notice = Some(Notice::error(
                strings.get("sidebarFilesTree.toast.nameRequired"),
                None,
            ));
            cx.notify();
            return;
        }
        let Some(parent) = path::parent(&rename.path) else {
            return;
        };
        let new_path = format!("{}/{name}", parent.trim_end_matches('/'));
        if new_path == rename.path {
            cx.notify();
            return;
        }
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let directory = self.scope.directory.clone();
        let old_path = rename.path.clone();
        let target = new_path.clone();
        let task = self
            .scope
            .run(async move { client.rename_path(&directory, &old_path, &target).await });
        let old_path = rename.path;
        self.renaming.push(old_path.clone());
        let revision = self.begin_rename_operation(&old_path);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.finish_rename(old_path, new_path, revision, result, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn begin_rename_operation(&mut self, old: &str) -> u64 {
        self.next_rename_revision += 1;
        self.rename_revisions
            .insert(old.to_owned(), self.next_rename_revision);
        self.next_rename_revision
    }

    pub(crate) fn finish_rename(
        &mut self,
        old: String,
        new: String,
        revision: u64,
        result: Option<Result<(), FilesError>>,
        cx: &mut Context<Self>,
    ) {
        if self.rename_revisions.get(&old) != Some(&revision) {
            return;
        }
        let strings = locale::strings(cx);
        match result {
            Some(Ok(())) => {
                self.notice = Some(Notice::info(
                    strings.get("sidebarFilesTree.toast.renamedSuccessfully"),
                ));
                self.path_moved(&old, &new, cx);
            }
            Some(Err(FilesError::AlreadyExists)) if !self.uncertain_renames.contains_key(&old) => {
                self.notice = Some(Notice::error(
                    strings.format(
                        "sidebarFilesTree.toast.nameTaken",
                        &[
                            (
                                "folder",
                                &path::parent(&old)
                                    .map_or_else(String::new, |parent| file_name(&parent)),
                            ),
                            ("name", &file_name(&new)),
                        ],
                    ),
                    None,
                ));
            }
            _ => {
                self.check_rename(old, new, cx);
                return;
            }
        }
        self.rename_finished(&old, cx);
        if let Some(parent) = path::parent(&old)
            && let Some(ticket) = self.tree.request(&parent, ListingCause::Mutation)
        {
            self.list(ticket, cx);
        }
        cx.notify();
    }

    /// A lost response does not establish whether the server moved the path.
    /// Refresh repeats these reads while writes remain blocked.
    pub(crate) fn check_rename(&mut self, old: String, new: String, cx: &mut Context<Self>) {
        let revision = match self.rename_revisions.get(&old) {
            Some(revision) => *revision,
            None => self.begin_rename_operation(&old),
        };
        self.renaming.retain(|active| active != &old);
        self.uncertain_renames.insert(old.clone(), new.clone());
        self.notice = Some(Notice::error(
            locale::strings(cx).get("filesView.rename.unresolved"),
            None,
        ));
        cx.notify();
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let directory = self.scope.directory.clone();
        let source = old.clone();
        let target = new.clone();
        let task = self.scope.run(async move {
            let source = client.path_kind(&directory, &source).await;
            let target = client.path_kind(&directory, &target).await;
            (source, target)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.rename_revisions.get(&old) != Some(&revision)
                    || this.uncertain_renames.get(&old) != Some(&new)
                {
                    return;
                }
                match result {
                    Some((Ok(None), Ok(Some(_)))) => {
                        this.path_moved(&old, &new, cx);
                    }
                    // A source that still exists does not prove a timed-out
                    // rename stopped. A late commit could otherwise recreate
                    // the old path through autosave. Only confirmed movement
                    // or an explicit same-operation retry can settle it.
                    _ => return,
                }
                this.rename_finished(&old, cx);
                this.notice = None;
                if let Some(parent) = path::parent(&old)
                    && let Some(ticket) = this.tree.request(&parent, ListingCause::Mutation)
                {
                    this.list(ticket, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Escape on the rename field cancels it.
    pub(crate) fn on_rename_escape(
        &mut self,
        _: &Escape,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.rename.is_some() {
            cx.stop_propagation();
            self.cancel_rename(window, cx);
        }
    }
}

fn listing_failure_text(failure: &ListingFailure, cx: &gpui_kit::App) -> SharedString {
    let strings = locale::strings(cx);
    match failure {
        ListingFailure::Failed(Some(message)) => SharedString::from(message.clone()),
        _ => strings.get("sidebarFilesTree.toast.operationFailed"),
    }
}
