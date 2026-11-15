//! The open autocomplete picker, over the composer's start edge
//! (`CommandAutocomplete`, `SkillAutocomplete`, `SnippetAutocomplete` and
//! `FileMentionAutocomplete`): one box `max-w-[560px]` for `@`, 420 px for
//! the others, at most 256 px tall, its rows and a keyboard hint below.
//!
//! The web places the box at the caret once it has measured it; until then
//! it anchors to the composer's edge, which is where this box stays.

use gpui_kit::{
    AnyElement, Context, Focusable, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Role, SharedString, StatefulInteractiveElement, Styled, TestSupportExt, div,
    prelude::FluentBuilder, px, relative,
};
use openchamber_native_core::autocomplete::{AutocompleteKind, FileRow, ListState, RowScope};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::composer_autocomplete::PickerRows;
use crate::elevation;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// `typography-ui-label` and `typography-meta`, in rems.
const LABEL_SIZE: f32 = 0.843_75;
const LABEL_LINE: f32 = 1.25;
const META_SIZE: f32 = 0.781_25;
const META_LINE: f32 = 1.125;
/// The scope badge's `text-[0.625rem]`.
const BADGE_SIZE: f32 = 0.625;
const MENTION_WIDTH: f32 = 560.0;
const LIST_WIDTH: f32 = 420.0;
const MAX_HEIGHT: f32 = 256.0;
/// `truncatePathMiddle(path, { maxLength: 60 })`.
const PATH_LENGTH: usize = 60;

/// One row's parts: what it is, its icon and the rest after its name.
struct Row {
    id: String,
    icon: Option<(&'static str, gpui_kit::Hsla)>,
    name: SharedString,
    /// Paths read left to right in every language.
    path: bool,
    detail: Option<String>,
    scope: Option<RowScope>,
}

/// `truncatePathMiddle`: a long path keeps the segments nearest its file
/// name behind `…/`; in trees full of `index.md` the parent tells them apart.
fn truncate_path_middle(path: &str, limit: usize) -> String {
    let limit = limit.max(16);
    let length = |text: &str| text.encode_utf16().count();
    if length(path) <= limit {
        return path.to_owned();
    }
    let mut segments: Vec<&str> = path.split('/').collect();
    if segments.len() <= 1 {
        return path.to_owned();
    }
    let file = segments.pop().unwrap_or_default();
    if file.is_empty() {
        return path.to_owned();
    }
    let mut suffix = file.to_owned();
    for segment in segments.iter().rev().filter(|segment| !segment.is_empty()) {
        let candidate = format!("{segment}/{suffix}");
        if length(&candidate) + 2 > limit {
            break;
        }
        suffix = candidate;
    }
    format!("…/{suffix}")
}

fn file_icon(row: &FileRow, palette: &Palette) -> (&'static str, gpui_kit::Hsla) {
    if row.directory {
        return ("folder-3-fill", palette.primary.opacity(0.6));
    }
    let extension = row
        .path
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, extension)| extension.to_ascii_lowercase());
    match extension.as_deref() {
        Some("ts" | "tsx" | "js" | "jsx") => ("code", palette.status_info),
        Some("json") => ("code", palette.status_warning),
        Some("md" | "mdx") => ("file", palette.muted_foreground),
        Some("png" | "jpg" | "jpeg" | "gif" | "svg") => ("file-image", palette.status_success),
        _ => ("file-pdf", palette.muted_foreground),
    }
}

impl Workspace {
    /// The open picker, or None.
    pub(crate) fn render_autocomplete(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let open = self.autocomplete.open.as_ref()?;
        let strings = locale::strings(cx);
        let rows = self.picker_rows(cx);
        let total = rows.len();
        let selected = if total == 0 { 0 } else { open.selected % total };
        let (list_label, width) = match open.kind {
            AutocompleteKind::Command => ("chat.autocomplete.tabs.commands", LIST_WIDTH),
            AutocompleteKind::Skill => ("chat.draftStarters.sectionSkills", LIST_WIDTH),
            AutocompleteKind::Snippet => ("settings.page.snippets.title", LIST_WIDTH),
            AutocompleteKind::Mention => ("chat.autocomplete.tabs.files", MENTION_WIDTH),
        };
        let mut children: Vec<AnyElement> = Vec::new();
        let mut index = 0;
        let mut push_rows = |rows: Vec<Row>, children: &mut Vec<AnyElement>| {
            for row in rows {
                children.push(self.render_autocomplete_row(
                    row,
                    index,
                    index == selected,
                    palette,
                    direction,
                    cx,
                ));
                index += 1;
            }
        };
        let note = |key: &str, cx: &Context<Self>| {
            div()
                .px(px(8.0))
                .py(px(6.0))
                .text_color(palette.muted_foreground)
                .css_text(LABEL_SIZE, LABEL_LINE, cx)
                .child(locale::strings(cx).get(key))
                .into_any_element()
        };
        let separator = || {
            div()
                .my(px(4.0))
                .h(px(1.0))
                .bg(palette.border)
                .into_any_element()
        };
        let loading = || {
            div()
                .id("autocomplete-loading")
                .role(Role::ProgressIndicator)
                .aria_label(locale::strings(cx).get("common.loading"))
                .px(px(8.0))
                .py(px(6.0))
                .h(px(LABEL_LINE * 16.0 + 12.0))
                .flex()
                .items_center()
                .child(glyph("loader-4", 14.0, palette.muted_foreground))
                .into_any_element()
        };
        match rows {
            PickerRows::Commands(commands) => {
                if commands.is_empty() {
                    children.push(note("chat.commandAutocomplete.empty", cx));
                }
                push_rows(
                    commands
                        .iter()
                        .map(|command| Row {
                            id: format!("command-{}", command.name),
                            icon: None,
                            name: format!("/{}", command.name).into(),
                            path: false,
                            detail: command.description.clone(),
                            scope: command.row_scope(),
                        })
                        .collect(),
                    &mut children,
                );
            }
            PickerRows::Skills {
                rows,
                loading: waiting,
            } => {
                if rows.is_empty() {
                    children.push(if waiting {
                        loading()
                    } else {
                        note("chat.autocomplete.skillsEmpty", cx)
                    });
                }
                push_rows(
                    rows.iter()
                        .map(|skill| Row {
                            id: format!("skill-{}", skill.name),
                            icon: None,
                            name: skill.name.clone().into(),
                            path: false,
                            detail: skill.description.clone(),
                            scope: Some(if skill.project {
                                RowScope::Project
                            } else {
                                RowScope::User
                            }),
                        })
                        .collect(),
                    &mut children,
                );
            }
            PickerRows::Snippets(snippets) => {
                if snippets.is_empty() {
                    children.push(note("chat.snippetAutocomplete.empty", cx));
                }
                push_rows(
                    snippets
                        .iter()
                        .map(|snippet| Row {
                            id: format!("snippet-{}", snippet.name),
                            icon: None,
                            name: format!("#{}", snippet.name).into(),
                            path: false,
                            detail: Some(snippet.preview()),
                            // A global snippet is the user's own.
                            scope: Some(if snippet.project {
                                RowScope::Project
                            } else {
                                RowScope::User
                            }),
                        })
                        .collect(),
                    &mut children,
                );
            }
            PickerRows::Mentions { view, pending } => {
                match openchamber_native_core::autocomplete::list_state(view.len(), pending) {
                    ListState::Loading => children.push(loading()),
                    ListState::Empty => {
                        children.push(note("chat.fileMentionAutocomplete.empty", cx))
                    }
                    ListState::Rows => {}
                }
                push_rows(
                    view.agents
                        .iter()
                        .map(|agent| Row {
                            id: format!("agent-{}", agent.name),
                            icon: None,
                            name: format!("@{}", agent.name).into(),
                            path: false,
                            detail: agent.description.clone(),
                            scope: None,
                        })
                        .collect(),
                    &mut children,
                );
                if view.more_agents {
                    children.push(
                        div()
                            .px(px(8.0))
                            .py(px(4.0))
                            .text_color(palette.muted_foreground)
                            .css_text(META_SIZE, META_LINE, cx)
                            .child(strings.get("chat.fileMentionAutocomplete.searchMoreAgents"))
                            .into_any_element(),
                    );
                }
                let files = !view.recent.is_empty() || !view.results.is_empty();
                if !view.agents.is_empty() && (files || !view.sessions.is_empty()) {
                    children.push(separator());
                }
                let file_row = |file: &FileRow| Row {
                    id: format!("file-{}", file.path),
                    icon: Some(file_icon(file, palette)),
                    name: truncate_path_middle(file.mention_path(), PATH_LENGTH).into(),
                    path: true,
                    detail: None,
                    scope: None,
                };
                push_rows(view.recent.iter().map(file_row).collect(), &mut children);
                if !view.recent.is_empty() && !view.results.is_empty() {
                    children.push(separator());
                }
                push_rows(view.results.iter().map(file_row).collect(), &mut children);
                if !view.sessions.is_empty() && files {
                    children.push(separator());
                }
                let archived = strings.get("chat.fileMentionAutocomplete.sessionArchived");
                push_rows(
                    view.sessions
                        .iter()
                        .map(|session| {
                            let detail = [
                                session.project.clone(),
                                session.archived.then(|| archived.to_string()),
                            ]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>()
                            .join(" · ");
                            Row {
                                id: format!("session-{}", session.id),
                                icon: Some(("chat-3", palette.muted_foreground)),
                                name: session.title.clone().into(),
                                path: false,
                                detail: (!detail.is_empty()).then_some(detail),
                                scope: None,
                            }
                        })
                        .collect(),
                    &mut children,
                );
            }
        }
        let hint_key = if open.kind == AutocompleteKind::Snippet {
            "chat.snippetAutocomplete.footer"
        } else {
            "chat.autocomplete.keyboardHint"
        };
        Some(
            div()
                .id("composer-autocomplete")
                .absolute()
                .bottom(relative(1.0))
                .mb(px(8.0))
                .inset_start(direction, px(0.0))
                .w_full()
                .max_w(px(width))
                .max_h(px(MAX_HEIGHT))
                .flex()
                .flex_col()
                .rounded(px(12.0))
                .bg(palette.elevated)
                .text_color(palette.elevated_foreground)
                .shadow(elevation::floating(palette))
                .occlude()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_autocomplete(cx)))
                .child(
                    div()
                        .id("composer-autocomplete-list")
                        .role(Role::ListBox)
                        .accessibility_id("composer-autocomplete-list")
                        .aria_label(strings.get(list_label))
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .track_scroll(&self.autocomplete.scroll)
                        .p(px(4.0))
                        .children(children),
                )
                .child(
                    div()
                        .px(px(12.0))
                        .py(px(6.0))
                        .border_t_1()
                        .border_color(palette.border)
                        .text_color(palette.muted_foreground)
                        .css_text(META_SIZE, META_LINE, cx)
                        .child(strings.get(hint_key)),
                )
                .test_support()
                .into_any_element(),
        )
    }

    fn render_autocomplete_row(
        &self,
        row: Row,
        index: usize,
        selected: bool,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let detail = row
            .detail
            .as_deref()
            .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|text| !text.is_empty());
        let badge = row.scope.map(|scope| {
            let (key, tone) = match scope {
                RowScope::Project => ("chat.autocomplete.scope.project", Some(palette.status_info)),
                RowScope::User => ("chat.autocomplete.scope.user", Some(palette.status_success)),
                RowScope::OpenCode => ("chat.autocomplete.scope.opencode", None),
                RowScope::OpenChamber => ("chat.autocomplete.scope.openchamber", None),
            };
            let label = strings.get(key).to_uppercase();
            div()
                .flex_shrink_0()
                .max_w(px(128.0))
                .truncate()
                .rounded(px(4.0))
                .border_1()
                .px(px(6.0))
                .py(px(4.0))
                .text_size(gpui_kit::rems(BADGE_SIZE))
                .line_height(relative(1.0))
                .font_weight(FontWeight::BOLD)
                .map(|badge| match tone {
                    Some(tone) => badge
                        .bg(tone.opacity(0.12))
                        .text_color(tone.opacity(0.7))
                        .border_color(tone.opacity(0.24)),
                    None => badge
                        .bg(palette.muted)
                        .text_color(palette.muted_foreground)
                        .border_color(palette.border.opacity(0.6)),
                })
                .child(label)
        });
        let label = [Some(row.name.to_string()), detail.clone()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
        let composer_focus = self.composer.read(cx).focus_handle(cx);
        on_activate(
            div()
                .id(SharedString::from(format!("autocomplete-{}", row.id)))
                .role(Role::ListBoxOption)
                .accessibility_id(format!("autocomplete-{}", row.id))
                .aria_label(label)
                .aria_selected(selected)
                .when(selected, |row| {
                    row.aria_active_descendant_for(&composer_focus)
                })
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .px(px(8.0))
                .py(px(6.0))
                .rounded(px(8.0))
                .cursor_pointer()
                .css_text(LABEL_SIZE, LABEL_LINE, cx)
                .when(selected, |row| row.bg(palette.selection))
                .on_mouse_move(
                    cx.listener(move |this, _, _, cx| this.select_autocomplete_row(index, cx)),
                )
                // A press never moves focus out of the editor.
                .on_mouse_down(gpui_kit::MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .when_some(row.icon, |this, (icon, color)| {
                    this.child(div().flex_shrink_0().child(glyph(icon, 14.0, color)))
                })
                .child(
                    div()
                        .when(row.path, |name| name.flex_1())
                        .when(!row.path, |name| name.flex_shrink_0().max_w(relative(0.6)))
                        .min_w_0()
                        .truncate()
                        .when(!row.path, |name| name.font_weight(FontWeight::SEMIBOLD))
                        .child(row.name),
                )
                .when(!row.path, |this| {
                    this.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(palette.muted_foreground)
                            .children(detail),
                    )
                })
                .children(badge),
            cx,
            move |this, window, cx| this.choose_autocomplete_row(index, window, cx),
        )
        .test_support()
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_paths_keep_the_segments_nearest_their_name() {
        assert_eq!(truncate_path_middle("src/a.ts", 60), "src/a.ts");
        let long = format!("{}/docs/guide/index.md", "x".repeat(60));
        assert_eq!(truncate_path_middle(&long, 60), "…/docs/guide/index.md");
        let folder = format!("{}/", "y".repeat(70));
        assert_eq!(truncate_path_middle(&folder, 60), folder);
    }
}
