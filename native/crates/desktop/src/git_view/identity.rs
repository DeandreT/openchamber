//! The Git header's identity menu (`IdentityDropdown` in `GitHeader.tsx`):
//! the identity the repository commits as, and the ones it can switch to.
//!
//! The identities and the machine's author are read when the surface opens,
//! the repository's author with every status read. Choosing one writes its
//! author into the repository (`setGitIdentity`), then reads the author
//! again; the System identity removes the repository's own author.
//! Applying the identity's account and transfer credentials to the
//! repository's binding (`applyIdentityToRepository`) is not ported.

use gpui_kit::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Role, SharedString,
    StatefulInteractiveElement, Styled, Task, TestSupportExt, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::git_identity::{self, Identity};

use super::GitView;
use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{CONTROL_RADIUS, glyph};
use crate::context_panel::{ROW_LINE, ROW_SIZE, XS_LINE, XS_SIZE};
use crate::files_view::{Notice, PANEL_ROW};
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;

/// The surface's identity reads.
#[derive(Default)]
pub(super) struct Identities {
    profiles: Vec<Identity>,
    system: Option<Identity>,
    /// The repository's author, once read.
    author: Option<(Option<String>, Option<String>)>,
    open: bool,
    applying: bool,
    lists: Option<Task<()>>,
    author_read: Option<Task<()>>,
    apply: Option<Task<()>>,
}

fn display_name(identity: &Identity, strings: &Strings) -> SharedString {
    if identity.is_system() {
        strings.get("gitView.identity.system")
    } else {
        identity.name.clone().into()
    }
}

impl GitView {
    /// Reads the stored identities and the machine's author; a failed read
    /// keeps what was read before.
    pub(super) fn read_identities(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let task = self.scope.run(async move {
            (
                client.git_identities().await,
                client.system_identity().await,
            )
        });
        self.identities.lists = Some(cx.spawn(async move |this, cx| {
            let Some((profiles, system)) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.identities.lists = None;
                match profiles {
                    Ok(profiles) => this.identities.profiles = profiles,
                    Err(error) => tracing::debug!(%error, "the identities could not be read"),
                }
                match system {
                    Ok(system) => this.identities.system = Some(system),
                    Err(error) => tracing::debug!(%error, "the machine's author could not be read"),
                }
                cx.notify();
            });
        }));
    }

    /// Reads the author the repository commits as.
    pub(super) fn read_author(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let directory = self.git_directory();
        let task = self
            .scope
            .run(async move { client.current_author(&directory).await });
        self.identities.author_read = Some(cx.spawn(async move |this, cx| {
            let Some(result) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.identities.author_read = None;
                match result {
                    Ok(author) => this.identities.author = Some(author),
                    Err(error) => {
                        tracing::debug!(%error, "the repository's author could not be read")
                    }
                }
                cx.notify();
            });
        }));
    }

    fn active_identity(&self) -> Option<Identity> {
        let author = self.identities.author.as_ref()?;
        git_identity::active(
            &self.identities.profiles,
            self.identities.system.as_ref(),
            (author.0.as_deref(), author.1.as_deref()),
        )
    }

    fn toggle_identities(&mut self, cx: &mut Context<Self>) {
        self.identities.open = !self.identities.open;
        if self.identities.open {
            // Identities can change in Settings while the surface is open.
            self.read_identities(cx);
        }
        cx.notify();
    }

    fn apply_identity(&mut self, identity: Identity, cx: &mut Context<Self>) {
        self.identities.open = false;
        if self.identities.applying {
            return;
        }
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let directory = self.git_directory();
        let id = identity.id.clone();
        let task = self
            .scope
            .run(async move { client.set_identity(&directory, &id).await });
        self.identities.applying = true;
        self.identities.apply = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.identities.applying = false;
                this.identities.apply = None;
                let strings = locale::strings(cx);
                match result {
                    Some(Ok(())) => {
                        this.notice = Some(Notice::info(strings.format(
                            "gitView.toast.appliedIdentity",
                            &[("name", &display_name(&identity, strings))],
                        )));
                    }
                    Some(Err(error)) => {
                        this.notice = Some(Notice::error(
                            strings.get("gitView.toast.applyIdentityFailed"),
                            error.message(),
                        ));
                    }
                    None => {}
                }
                this.read_author(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The state row's identity control (`IdentityDropdown iconOnly`): the
    /// icon and a chevron; the label names who commits.
    pub(super) fn render_identity_button(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let label = self
            .active_identity()
            .map(|identity| display_name(&identity, strings))
            .unwrap_or_else(|| strings.get("gitView.header.noIdentity"));
        let offered =
            git_identity::offered(&self.identities.profiles, self.identities.system.as_ref());
        let disabled = self.identities.applying || offered.is_empty();
        let hover = palette.hover;
        on_activate(
            div()
                .id("git-identity")
                .role(Role::Button)
                .accessibility_id("git-identity")
                .aria_label(SharedString::from(format!(
                    "{}: {label}",
                    strings.get("gitView.header.identityTooltip")
                )))
                .aria_expanded(self.identities.open)
                .h(px(28.0))
                .px(px(8.0))
                .flex_shrink_0()
                .rounded(px(CONTROL_RADIUS))
                .flex_row_from(direction)
                .items_center()
                .gap(px(6.0))
                .when(!disabled, |this| {
                    this.cursor_pointer().hover(move |style| style.bg(hover))
                })
                .when(disabled, |this| this.opacity(0.6))
                .child(glyph("user", 16.0, palette.status_info))
                .child(glyph("arrow-down-s", 16.0, palette.muted_foreground)),
            cx,
            move |this: &mut Self, _, cx| {
                if !disabled {
                    this.toggle_identities(cx)
                }
            },
        )
        .test_support()
        .into_any_element()
    }

    /// The identities to switch to, under the header while open.
    pub(super) fn render_identities(
        &self,
        palette: &Palette,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !self.identities.open {
            return None;
        }
        let strings = locale::strings(cx);
        let active = self.active_identity().map(|identity| identity.id);
        let offered =
            git_identity::offered(&self.identities.profiles, self.identities.system.as_ref());
        let hover = palette.hover;
        let rows = offered.into_iter().map(|identity| {
            let selected = active.as_deref() == Some(identity.id.as_str());
            let name = display_name(&identity, strings);
            let detail: SharedString = if identity.user_email.is_empty() {
                strings.get("gitView.identity.systemNoAuthor")
            } else {
                identity.user_email.clone().into()
            };
            let id = SharedString::from(format!("git-identity:{}", identity.id));
            on_activate(
                div()
                    .id(id.clone())
                    .role(Role::MenuItem)
                    .accessibility_id(id)
                    .aria_label(name.clone())
                    .aria_selected(selected)
                    .min_h(px(PANEL_ROW))
                    .mx(px(4.0))
                    .px(px(8.0))
                    .py(px(2.0))
                    .rounded(px(CONTROL_RADIUS))
                    .flex()
                    .flex_col()
                    .justify_center()
                    .cursor_pointer()
                    .when(selected, |this| this.bg(palette.selection))
                    .when(!selected, |this| this.hover(move |style| style.bg(hover)))
                    .child(
                        div()
                            .truncate()
                            .css_text(ROW_SIZE, ROW_LINE, cx)
                            .text_color(palette.foreground)
                            .child(name),
                    )
                    .child(
                        div()
                            .truncate()
                            .css_text(XS_SIZE, XS_LINE, cx)
                            .text_color(palette.muted_foreground)
                            .child(detail),
                    ),
                cx,
                move |this: &mut Self, _, cx| this.apply_identity(identity.clone(), cx),
            )
            .test_support()
            .into_any_element()
        });
        Some(
            div()
                .id("git-identities")
                .role(Role::Menu)
                .accessibility_id("git-identities")
                .aria_label(strings.get("gitView.header.identityTooltip"))
                .py(px(4.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .children(rows)
                .into_any_element(),
        )
    }
}
