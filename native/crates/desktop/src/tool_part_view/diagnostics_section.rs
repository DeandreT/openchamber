//! The LSP errors of the file a file-changing call changed
//! (`renderDiagnosticsSection` in `ToolPart.tsx`): the file, its first five
//! errors at their positions and a count of the rest, on the error tint.

use gpui_kit::{
    AnyElement, FontWeight, InteractiveElement, IntoElement, ParentElement, Role, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, div, prelude::FluentBuilder, px, rems,
};
use openchamber_native_core::ids::PartId;
use openchamber_native_core::tool_shells::diagnostics::DiagnosticSection;

use super::{MICRO_SIZE, tile_inset};
use crate::locale::{Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::panel_controls::LABEL_SIZE;
use crate::theme::Palette;
use crate::transcript_view::META_SIZE;

pub(super) fn diagnostics_section(
    part: &PartId,
    section: &DiagnosticSection<'_>,
    strings: &Strings,
    palette: &Palette,
    direction: Direction,
) -> AnyElement {
    let id: SharedString = format!("tool:{part}:diagnostics").into();
    let mut errors = div().flex().flex_col().gap_1();
    for diagnostic in section.diagnostics {
        errors = errors.child(
            div()
                .px_2()
                .py_1()
                .rounded(px(6.0))
                .bg(tile_inset(palette))
                .flex()
                .flex_row_from(direction)
                .items_start()
                .gap_2()
                .min_w_0()
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(rems(MICRO_SIZE))
                        .text_color(palette.status_error)
                        .child(format!("[{}:{}]", diagnostic.line, diagnostic.character)),
                )
                .child(
                    div()
                        .min_w_0()
                        .text_size(rems(META_SIZE))
                        .text_color(palette.foreground)
                        .whitespace_normal()
                        .child(diagnostic.message.clone()),
                ),
        );
    }
    div()
        .id(gpui_kit::ElementId::Name(id.clone()))
        .role(Role::Group)
        .accessibility_id(id)
        .aria_label(strings.get("chat.toolPart.lspErrors"))
        .p_2()
        .rounded(px(12.0))
        .bg(palette.status_error_background)
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_size(rems(META_SIZE))
                .font_weight(FontWeight::MEDIUM)
                .text_color(palette.status_error)
                .child(strings.get("chat.toolPart.lspErrors")),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(file_path(&section.display_path, palette))
                .child(errors)
                .when_some(
                    (section.remaining > 0).then(|| {
                        strings.format(
                            "chat.toolPart.moreErrors",
                            &[("count", &section.remaining.to_string())],
                        )
                    }),
                    |this, more| {
                        this.child(
                            div()
                                .text_size(rems(MICRO_SIZE))
                                .text_color(palette.muted_foreground)
                                .child(more),
                        )
                    },
                ),
        )
        .test_support()
        .into_any_element()
}

/// A path with its folder muted and its file name plain
/// (`renderPathLikeGitChanges`). Paths read left to right in every locale.
fn file_path(path: &str, palette: &Palette) -> AnyElement {
    let (folder, name) = match path.rsplit_once('/') {
        Some((folder, name)) => (Some(folder), name),
        None => (None, path),
    };
    div()
        .flex()
        .items_baseline()
        .min_w_0()
        .overflow_hidden()
        .text_size(rems(LABEL_SIZE))
        .when_some(folder, |this, folder| {
            this.child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(palette.muted_foreground)
                    .child(format!("{folder}/")),
            )
        })
        .child(
            div()
                .flex_shrink_0()
                .text_color(palette.foreground)
                .child(name.to_owned()),
        )
        .into_any_element()
}
