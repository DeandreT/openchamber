//! Display-only projection of PermissionRequestContent and permissionSummary.

use gpui_kit::base::text::{TextView, TextViewStyle};
use gpui_kit::component::Theme;
use gpui_kit::{
    AnyElement, App, Context, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, StyleRefinement, Styled, TestSupportExt, Window, div, px, rems,
};
use openchamber_native_core::permission::PermissionRequest;
use serde_json::Value;

use crate::baseline::CssTextExt;
use crate::locale::{self, Strings};
use crate::logical::LogicalStyledExt;
use crate::theme::{Appearance, Palette};
use crate::workspace::Workspace;

pub(super) fn tool(action: &str) -> (&'static str, &str) {
    match action.to_ascii_lowercase().as_str() {
        "" => ("tools", "unknown"),
        "edit" | "patch" | "str_replace" | "str_replace_based_edit_tool" => ("pencil-ai", "edit"),
        "write" | "create" | "file_write" => ("file-edit", "write"),
        "shell" | "bash" | "cmd" | "terminal" | "shell_command" => ("terminal-box", "shell"),
        "webfetch" | "fetch" | "curl" | "wget" => ("global", "webfetch"),
        value if value == "linear" || value.starts_with("linear_") => ("linear", action),
        value
            if value == "cloudflare"
                || value.starts_with("cloudflare_")
                || value == "claudflare"
                || value.starts_with("claudflare_") =>
        {
            ("cloudflare", action)
        }
        _ => ("tools", action),
    }
}

pub(super) fn tool_header(request: &PermissionRequest, palette: &Palette, cx: &App) -> AnyElement {
    let (icon, name) = tool(&request.action);
    div()
        .flex()
        .flex_row_from(locale::strings(cx).direction())
        .items_center()
        .flex_shrink_0()
        .gap(px(6.))
        .text_color(palette.muted_foreground)
        .child(crate::chrome::glyph(icon, 12., palette.muted_foreground))
        .child(
            div()
                .css_text(0.78125, 1.125, cx)
                .font_weight(gpui_kit::FontWeight::MEDIUM)
                .child(name.to_owned()),
        )
        .into_any_element()
}

fn strip_directory_glob(value: &str) -> &str {
    if value == "/*" {
        value
    } else {
        value.strip_suffix("/*").unwrap_or(value)
    }
}

pub(super) fn always_label(request: &PermissionRequest, strings: &Strings) -> SharedString {
    if request.save.iter().all(|value| value == "*") {
        return strings.get("chat.permissionCard.alwaysAllow");
    }
    let patterns: Vec<_> = request
        .save
        .iter()
        .take(2)
        .map(|value| {
            if request.action.eq_ignore_ascii_case("external_directory") {
                strip_directory_glob(value)
            } else {
                value
            }
        })
        .collect();
    let mut shown = patterns.join(", ");
    if request.save.len() > 2 {
        shown.push_str("...");
    }
    strings.format(
        "chat.permissionCard.alwaysAllowPatterns",
        &[("patterns", &shown)],
    )
}

fn meta(metadata: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| match &metadata[*key] {
            Value::String(value) if !value.is_empty() => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

fn code(id: &str, source: &str, language: &str, palette: &Palette) -> AnyElement {
    // A longer fence preserves commands containing Markdown fences verbatim.
    let longest = source.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(3.max(longest + 1));
    let markdown = format!("{fence}{language}\n{source}\n{fence}");
    div()
        .id(SharedString::from(format!("{id}-block")))
        .test_support()
        .min_w_0()
        .child(
            TextView::markdown(SharedString::from(id.to_owned()), markdown)
                .selectable(true)
                .style(
                    TextViewStyle::default()
                        .with_foreground(palette.syntax_foreground)
                        .with_selection(palette.selection)
                        .with_dark(palette.appearance == Appearance::Dark)
                        .with_paragraph_gap(rems(0.))
                        .with_code_block(
                            StyleRefinement::default()
                                .p(px(8.))
                                .rounded(px(4.))
                                .border_0()
                                .bg(palette.muted.opacity(0.3))
                                .text_size(rems(0.78125))
                                .line_height(rems(1.25)),
                        ),
                )
                .code_block_actions(|_, _, _| div()),
        )
        .into_any_element()
}

pub(super) fn content(
    request: &PermissionRequest,
    palette: &Palette,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let strings = locale::strings(cx).clone();
    let metadata: Value = request
        .metadata_display
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or(Value::Null);
    let action = request.action.to_ascii_lowercase();
    let shell = matches!(action.as_str(), "shell" | "bash" | "shell_command");
    let title = match action.as_str() {
        "external_directory" => "chat.permissionCard.summary.externalDirectory",
        "read" => "chat.permissionCard.summary.read",
        "edit" | "write" | "patch" => "chat.permissionCard.summary.edit",
        "shell" | "bash" | "shell_command" => "chat.permissionCard.summary.shell",
        "glob" => "chat.permissionCard.summary.glob",
        "grep" => "chat.permissionCard.summary.grep",
        "webfetch" => "chat.permissionCard.summary.webfetch",
        "websearch" => "chat.permissionCard.summary.websearch",
        "skill" => "chat.permissionCard.summary.skill",
        _ => "chat.permissionCard.summary.tool",
    };
    let mut content = div().flex().flex_col().min_w_0();
    let text = |value: SharedString| div().css_text(0.78125, 1.125, cx).child(value);
    if let Some(message) = &request.message {
        content = content.child(text(message.clone().into()).mb(px(8.)));
    }
    let mut summary = div().mb(px(8.)).child(
        text(strings.format(title, &[("tool", &request.action)]))
            .id(SharedString::from(format!(
                "permission-{}-summary",
                request.id.as_str()
            )))
            .aria_label(strings.format(title, &[("tool", &request.action)]))
            .test_support()
            .font_weight(gpui_kit::FontWeight::MEDIUM),
    );
    if matches!(action.as_str(), "glob" | "grep") {
        let path = meta(&metadata, &["path"]);
        if !path.is_empty() && path != "." {
            summary = summary.child(
                text(strings.format("chat.permissionCard.summary.inPath", &[("path", &path)]))
                    .text_color(palette.muted_foreground),
            );
        }
    }
    let mut command = meta(&metadata, &["command", "cmd", "script"]);
    if shell && command.is_empty() {
        command = request.resources.join("\n");
    }
    let previews: Vec<_> = metadata["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|file| {
            file["file"].is_string()
                && file["patch"]
                    .as_str()
                    .is_some_and(|patch| !patch.is_empty())
        })
        .collect();
    let has_previews = matches!(tool(&request.action).1, "edit" | "write") && !previews.is_empty();
    let webfetch = matches!(action.as_str(), "webfetch" | "fetch" | "curl" | "wget");
    let displayed_url = if webfetch {
        meta(&metadata, &["url", "uri", "endpoint"])
    } else {
        String::new()
    };
    let mut targets = div().mt(px(4.)).flex().flex_col().gap(px(2.));
    let mut count = 0;
    for (index, resource) in request.resources.iter().enumerate() {
        // Metadata is display-only. Hide a target only when the same target
        // is actually shown by the matching preview below.
        if (has_previews
            && previews
                .iter()
                .any(|file| file["file"].as_str() == Some(resource.as_str())))
            || (shell && !command.is_empty() && command.split('\n').any(|line| line == resource))
            || (webfetch && !displayed_url.is_empty() && resource == &displayed_url)
            || (!shell
                && !webfetch
                && action != "external_directory"
                && (resource.is_empty() || resource == "*"))
        {
            continue;
        }
        let mut value = if action == "external_directory" {
            strip_directory_glob(resource).to_owned()
        } else {
            resource.clone()
        };
        if action == "external_directory" {
            let file = meta(&metadata, &["filepath"]);
            let parent = meta(&metadata, &["parentDir"]);
            if !file.is_empty()
                && ((!parent.is_empty() && strip_directory_glob(&parent) == value)
                    || (parent.is_empty() && file.starts_with(&format!("{value}/"))))
                && let Some(name) = file.rsplit(['/', '\\']).find(|part| !part.is_empty())
            {
                value.push('/');
                value.push_str(name);
            }
        }
        targets = targets.child(
            text(value.into())
                .id(SharedString::from(format!(
                    "permission-{}-target-{index}",
                    request.id.as_str()
                )))
                .aria_label(resource.clone())
                .test_support()
                .font_weight(gpui_kit::FontWeight::NORMAL)
                .font_family(Theme::global(cx).mono_font_family.clone()),
        );
        count += 1;
    }
    if count > 0 {
        summary = summary.child(targets);
    }
    content = content.child(summary);
    let id = format!("permission-{}-code", request.id.as_str());
    if shell {
        let description = meta(&metadata, &["description"]);
        if !description.is_empty() {
            content = content.child(
                text(description.into())
                    .mb(px(8.))
                    .text_color(palette.muted_foreground),
            );
        }
        for (key, value) in [
            (
                "chat.permissionCard.workingDirectory",
                meta(
                    &metadata,
                    &["cwd", "working_directory", "directory", "path"],
                ),
            ),
            (
                "chat.permissionCard.timeout",
                metadata["timeout"]
                    .as_f64()
                    .filter(|n| *n != 0.)
                    .map(|_| format!("{}ms", metadata["timeout"]))
                    .unwrap_or_default(),
            ),
        ] {
            if !value.is_empty() {
                content = content.child(
                    text(format!("{} {value}", strings.get(key)).into())
                        .mb(px(8.))
                        .text_color(palette.muted_foreground),
                );
            }
        }
        if !command.is_empty() {
            content = content.child(code(&id, &command, "bash", palette));
        }
    } else if has_previews {
        for (index, file) in previews.iter().enumerate() {
            let path = meta(file, &["file"]);
            let patch = meta(file, &["patch"]);
            if !patch.is_empty() {
                content = content.child(text(path.into())).child(code(
                    &format!("{id}-{index}"),
                    &patch,
                    "diff",
                    palette,
                ));
            }
        }
    } else {
        if matches!(
            action.as_str(),
            "edit" | "patch" | "multiedit" | "str_replace" | "str_replace_based_edit_tool"
        ) && ["replace_all", "replaceAll"]
            .iter()
            .any(|key| metadata[*key] == Value::Bool(true))
        {
            content = content.child(
                text(strings.get("chat.permissionCard.replaceAll"))
                    .mb(px(8.))
                    .text_color(palette.muted_foreground),
            );
        }
        let description = meta(&metadata, &["description"]);
        let has_description = !description.is_empty();
        if !description.is_empty() {
            content = content.child(
                text(description.into())
                    .mb(px(8.))
                    .text_color(palette.muted_foreground),
            );
        }
        let (source, language) = match action.as_str() {
            "edit" | "patch" | "multiedit" | "str_replace" | "str_replace_based_edit_tool" => {
                (meta(&metadata, &["changes", "diff"]), "diff")
            }
            "write" | "create" | "file_write" => {
                (meta(&metadata, &["content", "text", "data"]), "text")
            }
            _ => (
                meta(&metadata, &["command", "content", "action", "operation"]),
                "text",
            ),
        };
        if webfetch {
            if !displayed_url.is_empty() {
                let method = meta(&metadata, &["method"]);
                content = content
                    .child(text(strings.get("chat.permissionCard.request")))
                    .child(
                        text(
                            format!(
                                "{} {displayed_url}",
                                if method.is_empty() { "GET" } else { &method }
                            )
                            .into(),
                        )
                        .id(SharedString::from(format!(
                            "permission-{}-url",
                            request.id.as_str()
                        )))
                        .aria_label(displayed_url.clone())
                        .test_support()
                        .mb(px(8.)),
                    );
            }
            if metadata["headers"]
                .as_object()
                .is_some_and(|headers| !headers.is_empty())
            {
                content = content
                    .child(text(strings.get("chat.permissionCard.headers")))
                    .child(code(
                        &format!("{id}-headers"),
                        &serde_json::to_string_pretty(&metadata["headers"]).expect("JSON value"),
                        "json",
                        palette,
                    ));
            }
            let body = meta(&metadata, &["body", "data", "payload"]);
            if !body.is_empty() {
                content = content
                    .child(text(strings.get("chat.permissionCard.body")))
                    .child(code(&format!("{id}-body"), &body, "text", palette));
            }
        }
        if !source.is_empty() {
            if !matches!(tool(&request.action).1, "edit" | "write" | "webfetch") {
                content = content.child(
                    text(strings.get("chat.permissionCard.action"))
                        .mb(px(4.))
                        .text_color(palette.muted_foreground),
                );
            }
            content = content.child(code(&id, &source, language, palette));
        }
        if title == "chat.permissionCard.summary.tool"
            && source.is_empty()
            && !has_description
            && metadata
                .as_object()
                .is_some_and(|object| !object.is_empty())
            && !matches!(tool(&request.action).1, "edit" | "write" | "webfetch")
        {
            let detail_id = format!("permission-{}-details", request.id.as_str());
            let expanded = window.use_keyed_state(
                gpui_kit::ElementId::Name(detail_id.clone().into()),
                cx,
                |_, _| false,
            );
            let shown = *expanded.read(cx);
            content = content.child(super::button(
                detail_id,
                strings.get("chat.permissionCard.showDetails"),
                true,
                super::Control::Details(shown),
                palette,
                (window, cx),
                move |_, _, cx| {
                    expanded.update(cx, |value, _| *value = !*value);
                    cx.notify();
                },
            ));
            if shown {
                content = content.child(code(
                    &format!("{id}-details"),
                    &serde_json::to_string_pretty(&metadata).expect("JSON value"),
                    "json",
                    palette,
                ));
            }
        }
    }
    content.into_any_element()
}
