//! File and agent-mention parts (`MessageFilesDisplay` in
//! `FileAttachment.tsx`, compact).
//!
//! A message's files lead its bubble: other files as name chips, images as
//! 64 px thumbnails. A thumbnail draws a `data:` image the prompt carried
//! inline. Any other address shows the image's failed state, as the web's
//! `<img>` does for a `file:` URL it cannot load.

use std::sync::Arc;

use gpui_kit::{
    AnyElement, Image, ImageFormat, InteractiveElement, IntoElement, ObjectFit, ParentElement,
    Role, SharedString, StatefulInteractiveElement, Styled, StyledImage, div, img,
    prelude::FluentBuilder, px, rems,
};
use openchamber_native_core::data_url;
use openchamber_native_core::transcript::{Message, PartContent};

use crate::chrome::glyph;
use crate::locale::{Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::part_state::PartState;
use crate::theme::Palette;
use crate::transcript_view::META_SIZE;

/// `h-16 w-16` at the desktop breakpoint.
const THUMBNAIL: f32 = 64.0;

/// The files of a message, or `None` when it carries none.
pub(crate) fn attachments(
    message: &Message,
    parts: &PartState,
    palette: &Palette,
    direction: Direction,
    strings: &Strings,
) -> Option<AnyElement> {
    let mut chips = Vec::new();
    let mut thumbnails = Vec::new();
    for part in &message.parts {
        let PartContent::File {
            filename,
            mime,
            url,
        } = &part.content
        else {
            continue;
        };
        let name = display_name(filename, url.as_deref());
        if mime.starts_with("image/") && url.is_some() {
            let image = parts.attachment_image(&part.id, || url.as_deref().and_then(decoded_image));
            thumbnails.push(thumbnail(name, image, palette, strings));
        } else {
            chips.push(chip(name, mime, palette, direction));
        }
    }
    if chips.is_empty() && thumbnails.is_empty() {
        return None;
    }
    Some(
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .when(!chips.is_empty(), |this| {
                this.child(
                    div()
                        .flex()
                        .flex_row_from(direction)
                        .flex_wrap()
                        .gap(px(6.0))
                        .children(chips),
                )
            })
            .when(!thumbnails.is_empty(), |this| {
                this.child(
                    div()
                        .flex()
                        .flex_row_from(direction)
                        .flex_wrap()
                        .gap_2()
                        .children(thumbnails),
                )
            })
            .into_any_element(),
    )
}

/// The file name of a path or URL (`resolveDisplayName`).
fn display_name(filename: &str, url: Option<&str>) -> SharedString {
    let path = if filename.is_empty() {
        url.filter(|url| !url.starts_with("data:"))
            .unwrap_or_default()
    } else {
        filename
    };
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    SharedString::from(if name.is_empty() { path } else { name }.to_owned())
}

fn decoded_image(url: &str) -> Option<Arc<Image>> {
    let decoded = data_url::decode(url)?;
    let format = match decoded.mime.as_str() {
        "image/png" => ImageFormat::Png,
        "image/jpeg" | "image/jpg" => ImageFormat::Jpeg,
        "image/webp" => ImageFormat::Webp,
        "image/gif" => ImageFormat::Gif,
        "image/svg+xml" => ImageFormat::Svg,
        "image/bmp" => ImageFormat::Bmp,
        _ => return None,
    };
    Some(Arc::new(Image::from_bytes(format, decoded.bytes)))
}

fn chip(name: SharedString, mime: &str, palette: &Palette, direction: Direction) -> AnyElement {
    div()
        .flex()
        .flex_row_from(direction)
        .items_center()
        .gap_1()
        .px_2()
        .py(px(2.0))
        .rounded(px(8.0))
        .bg(palette.muted.opacity(0.3))
        .border_1()
        .border_color(palette.border.opacity(0.6))
        .text_size(rems(META_SIZE))
        .text_color(palette.foreground)
        .child(glyph(
            if mime.contains("pdf") {
                "file-pdf"
            } else {
                "file-text"
            },
            14.0,
            palette.muted_foreground,
        ))
        .child(div().max_w(px(140.0)).truncate().child(name))
        .into_any_element()
}

fn thumbnail(
    name: SharedString,
    image: Option<Arc<Image>>,
    palette: &Palette,
    strings: &Strings,
) -> AnyElement {
    let frame = div()
        .id(gpui_kit::ElementId::Name(
            format!("attachment:{name}").into(),
        ))
        .role(Role::Image)
        .accessibility_id(format!("attachment:{name}"))
        .aria_label(name)
        .flex_none()
        .size(px(THUMBNAIL))
        .rounded(px(8.0))
        .overflow_hidden()
        .border_1()
        .border_color(palette.border.opacity(0.6))
        .bg(palette.muted.opacity(0.1));
    match image {
        Some(image) => frame
            .child(img(image).size_full().object_fit(ObjectFit::Cover))
            .into_any_element(),
        None => frame
            .flex()
            .items_center()
            .justify_center()
            .aria_description(strings.get("common.image.loadFailed"))
            .child(glyph("image", 20.0, palette.muted_foreground))
            .into_any_element(),
    }
}

pub(crate) fn file(filename: &str, palette: &Palette) -> AnyElement {
    div()
        .text_size(rems(META_SIZE))
        .text_color(palette.muted_foreground)
        .child(filename.to_owned())
        .into_any_element()
}

pub(crate) fn agent_mention(name: &str, palette: &Palette) -> AnyElement {
    div()
        .text_size(rems(META_SIZE))
        .text_color(palette.muted_foreground)
        .child(format!("@{name}"))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_come_from_the_file_name_or_its_address() {
        assert_eq!(
            display_name("shots/screen.png", None).as_ref(),
            "screen.png"
        );
        assert_eq!(
            display_name("", Some("file:///tmp/a/gradient.png")).as_ref(),
            "gradient.png"
        );
        assert_eq!(
            display_name("", Some("data:image/png;base64,AAAA")).as_ref(),
            ""
        );
    }

    #[test]
    fn only_inline_images_decode() {
        assert!(decoded_image("data:image/png;base64,iVBORw0KGgo=").is_some());
        assert!(decoded_image("file:///tmp/a.png").is_none());
        assert!(decoded_image("data:text/plain;base64,aGk=").is_none());
    }
}
