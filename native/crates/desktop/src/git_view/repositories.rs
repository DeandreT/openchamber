//! The repository the Git surface works in when its directory holds others
//! (`useNestedGitDirectory.ts`, `NestedRepoPicker.tsx`).
//!
//! The repositories under the directory are listed when the surface opens
//! and again on Refresh, except in the home directory. A directory that is no
//! repository opens the first one not under a dot-directory; a repository
//! root keeps working in itself and lists the others in the picker. Choosing
//! one reads everything again for it. A selection whose repository is gone
//! falls back to the directory.

use gpui_kit::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Role, SharedString,
    StatefulInteractiveElement, Styled, Task, TestSupportExt, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::git_changes::{ChangesRead, StatusLoads};
use openchamber_native_core::nested_repositories;

use super::GitView;
use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{CONTROL_RADIUS, glyph};
use crate::context_panel::{ROW_LINE, ROW_SIZE};
use crate::files_view::{PANEL_ROW, PanelScope};
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;

#[derive(Default)]
pub(super) struct Repositories {
    /// The chosen repository; `None` works in the surface's directory.
    selected: Option<String>,
    /// The repositories under the directory, once listed.
    discovered: Vec<String>,
    /// Whether the directory itself is a repository, once its status said.
    root_is_repository: Option<bool>,
    open: bool,
    listing: Option<Task<()>>,
}

impl GitView {
    /// The repository every Git request goes to.
    pub(super) fn git_directory(&self) -> String {
        self.repositories
            .selected
            .clone()
            .unwrap_or_else(|| self.scope.directory.clone())
    }

    /// The scope with the repository as its directory, for the diff pane.
    pub(super) fn git_scope(&self) -> PanelScope {
        let mut scope = self.scope.clone();
        scope.directory = self.git_directory();
        scope
    }

    /// Lists the repositories under the directory; a failed list keeps the
    /// previous one.
    pub(super) fn discover_repositories(&mut self, cx: &mut Context<Self>) {
        let home = self.scope.owner.as_ref().and_then(|owner| {
            owner
                .upgrade()?
                .read(cx)
                .send_catalog
                .settings
                .as_ref()?
                .home_directory
                .clone()
        });
        let root = self.scope.directory.clone();
        if !nested_repositories::searches_nested(&root, home.as_deref()) {
            return;
        }
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let task = self
            .scope
            .run(async move { client.git_directories(&root).await });
        self.repositories.listing = Some(cx.spawn(async move |this, cx| {
            let Some(result) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.repositories.listing = None;
                match result {
                    Ok(found) => {
                        let root = this.scope.directory.clone();
                        this.repositories.discovered =
                            found.into_iter().filter(|path| *path != root).collect();
                        if let Some(selected) = this.repositories.selected.clone()
                            && !this.repositories.discovered.contains(&selected)
                        {
                            this.select_repository(None, cx);
                        }
                        this.pick_when_root_is_plain(cx);
                    }
                    Err(error) => {
                        tracing::debug!(%error, "nested repositories could not be listed")
                    }
                }
                cx.notify();
            });
        }));
    }

    /// A status read of the directory itself finished.
    pub(super) fn root_status_read(&mut self, cx: &mut Context<Self>) {
        if self.repositories.selected.is_some() {
            return;
        }
        match self.status.current() {
            Some(ChangesRead::NotRepository) => {
                self.repositories.root_is_repository = Some(false);
                self.pick_when_root_is_plain(cx);
            }
            Some(ChangesRead::Repository(_)) => self.repositories.root_is_repository = Some(true),
            None => {}
        }
    }

    fn pick_when_root_is_plain(&mut self, cx: &mut Context<Self>) {
        if self.repositories.root_is_repository != Some(false)
            || self.repositories.selected.is_some()
        {
            return;
        }
        let root = self.scope.directory.clone();
        if let Some(pick) =
            nested_repositories::auto_pick(&root, &self.repositories.discovered).map(str::to_owned)
        {
            self.select_repository(Some(pick), cx);
        }
    }

    fn select_repository(&mut self, repository: Option<String>, cx: &mut Context<Self>) {
        let repository = repository.filter(|repository| *repository != self.scope.directory);
        self.repositories.open = false;
        if self.repositories.selected == repository {
            cx.notify();
            return;
        }
        self.repositories.selected = repository;
        self.status = StatusLoads::default();
        self.status_task = None;
        self.selected = None;
        self.pane.clear(cx);
        self.branches = None;
        self.branches_open = false;
        self.notice = None;
        self.refresh(cx);
        cx.notify();
    }

    /// The picker's button, when there is a choice.
    pub(super) fn render_repository_button(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let root = &self.scope.directory;
        nested_repositories::options(
            root,
            self.repositories.root_is_repository == Some(true),
            &self.repositories.discovered,
            self.repositories.selected.as_deref(),
        )?;
        let strings = locale::strings(cx);
        let current = self.git_directory();
        let label = SharedString::from(nested_repositories::label(root, &current).to_owned());
        let hover = palette.hover;
        Some(
            on_activate(
                div()
                    .id("git-repository")
                    .role(Role::Button)
                    .accessibility_id("git-repository")
                    .aria_label(SharedString::from(format!(
                        "{}: {label}",
                        strings.get("gitView.empty.selectRepositoryPlaceholder")
                    )))
                    .aria_expanded(self.repositories.open)
                    // `SelectTrigger` with `max-w-[13rem] gap-1.5 px-2`.
                    .h(px(28.0))
                    .max_w(px(208.0))
                    .px(px(8.0))
                    .rounded(px(CONTROL_RADIUS))
                    .bg(palette.elevated)
                    .shadow(crate::elevation::control(palette))
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(6.0))
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .child(glyph("folder", 16.0, palette.muted_foreground))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .css_text(
                                crate::panel_controls::LABEL_SIZE,
                                crate::panel_controls::LABEL_LINE,
                                cx,
                            )
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .text_color(palette.foreground)
                            .child(label),
                    )
                    .child(glyph("arrow-down-s", 16.0, palette.muted_foreground)),
                cx,
                |this: &mut Self, _, cx| {
                    this.repositories.open = !this.repositories.open;
                    cx.notify();
                },
            )
            .test_support()
            .into_any_element(),
        )
    }

    /// The repositories to switch to, under the header while open.
    pub(super) fn render_repositories(
        &self,
        palette: &Palette,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !self.repositories.open {
            return None;
        }
        let root = self.scope.directory.clone();
        let options = nested_repositories::options(
            &root,
            self.repositories.root_is_repository == Some(true),
            &self.repositories.discovered,
            self.repositories.selected.as_deref(),
        )?;
        let strings = locale::strings(cx);
        let current = self.git_directory();
        let hover = palette.hover;
        let rows = options.into_iter().map(|repository| {
            let selected = repository == current;
            let label =
                SharedString::from(nested_repositories::label(&root, &repository).to_owned());
            let id = SharedString::from(format!("git-repository:{repository}"));
            on_activate(
                div()
                    .id(id.clone())
                    .role(Role::MenuItem)
                    .accessibility_id(id)
                    .aria_label(label.clone())
                    .aria_selected(selected)
                    .h(px(PANEL_ROW))
                    .mx(px(4.0))
                    .px(px(8.0))
                    .rounded(px(CONTROL_RADIUS))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .when(selected, |this| this.bg(palette.selection))
                    .when(!selected, |this| this.hover(move |style| style.bg(hover)))
                    .child(
                        div()
                            .truncate()
                            .css_text(ROW_SIZE, ROW_LINE, cx)
                            .text_color(palette.foreground)
                            .child(label),
                    ),
                cx,
                move |this: &mut Self, _, cx| this.select_repository(Some(repository.clone()), cx),
            )
            .test_support()
            .into_any_element()
        });
        Some(
            div()
                .id("git-repositories")
                .role(Role::Menu)
                .accessibility_id("git-repositories")
                .aria_label(strings.get("gitView.empty.selectRepositoryPlaceholder"))
                .py(px(4.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .children(rows)
                .into_any_element(),
        )
    }
}
