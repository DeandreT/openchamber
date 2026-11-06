//! Native picker and clipboard adapters for captured attachment intake.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::{Disableable, Sizable, Size};
use gpui_kit::{
    Anchor, AnyElement, ClipboardEntry, ClipboardItem, Context, EntityInputHandler, ImageFormat,
    IntoElement, ParentElement, PathPromptOptions, Styled, Task, Window, px,
};
use openchamber_native_core::draft::DraftKey;

use crate::attachment_files::MAX_INLINE_ATTACHMENT_BYTES;
use crate::attachment_intake::{AttachmentInputSource, AttachmentIntake};
use crate::locale;
use crate::locale::Direction;
use crate::theme::Palette;
use crate::workspace::Workspace;

#[derive(Default)]
pub(crate) struct ComposerAttachmentInput {
    picker: Option<(DraftKey, Task<()>)>,
    revisions: HashMap<DraftKey, u64>,
    clipboard_sequence: u64,
}

impl ComposerAttachmentInput {
    pub(crate) fn cancel(&mut self, owner: &DraftKey) {
        let revision = self.revisions.entry(owner.clone()).or_default();
        *revision = revision.wrapping_add(1);
        if self
            .picker
            .as_ref()
            .is_some_and(|(captured, _)| captured == owner)
        {
            self.picker = None;
        }
    }
}

impl Workspace {
    pub(crate) fn render_attachment_menu(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let workspace = cx.weak_entity();
        let label = locale::strings(cx).get("chat.chatInput.actions.addAttachment");
        let file_label = locale::strings(cx).get("chat.chatInput.actions.attachFiles");
        let disabled =
            !self.composer_editable() || !self.persistence_loaded() || self.close_in_progress();
        Button::new("composer-attach")
            .accessibility_id("composer-attach")
            .ghost()
            .with_size(Size::Small)
            .w(px(24.0))
            .p_0()
            .disabled(disabled)
            .accessibility_label(label.clone())
            .tooltip(label)
            .child(crate::chrome::glyph("add-circle", 18.0, palette.foreground))
            .dropdown_menu_with_anchor(
                if direction == Direction::RightToLeft {
                    Anchor::BottomRight
                } else {
                    Anchor::BottomLeft
                },
                move |menu, _, _| {
                    let workspace = workspace.clone();
                    menu.item(PopupMenuItem::new(file_label.clone()).on_click(
                        move |_, window, cx| {
                            let _ = workspace.update(cx, |workspace, cx| {
                                workspace.choose_attachment_paths(window, cx)
                            });
                        },
                    ))
                },
            )
            .into_any_element()
    }

    pub(crate) fn choose_attachment_paths(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.composer_editable()
            || !self.persistence_loaded()
            || self.close_in_progress()
            || self.attachment_input.picker.is_some()
        {
            return;
        }
        let Some(owner) = self.composer_key.clone() else {
            return;
        };
        let revision = *self
            .attachment_input
            .revisions
            .entry(owner.clone())
            .or_default();
        let answer = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(locale::strings(cx).get("chat.chatInput.actions.attachFiles")),
        });
        let captured = owner.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = answer.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.attachment_input.revisions.get(&captured) != Some(&revision) {
                    return;
                }
                this.attachment_input.picker = None;
                if this.close_in_progress() {
                    return;
                }
                match result {
                    Ok(Ok(Some(paths))) => {
                        this.attach_selected_paths(captured.clone(), paths, window, cx)
                    }
                    Ok(Ok(None)) => {}
                    _ => this.notify_mutation_failure(
                        "chat.chatInput.toast.attachFileFailed",
                        window,
                        cx,
                    ),
                }
                if this.composer_key.as_ref() == Some(&captured) {
                    this.focus_composer(window, cx);
                }
            });
        });
        self.attachment_input.picker = Some((owner, task));
    }

    /// The path payload comes from an explicit native selection, never from a
    /// server file-tree entry or text resembling a path.
    pub(crate) fn attach_selected_paths(
        &mut self,
        owner: DraftKey,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.persistence_loaded() || self.close_in_progress() {
            return;
        }
        let mut inputs = Vec::new();
        for path in paths {
            let Some(filename) = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
            else {
                self.notify_mutation_failure("chat.chatInput.toast.attachFileFailed", window, cx);
                continue;
            };
            inputs.push(AttachmentIntake {
                source: AttachmentInputSource::LocalPath(path),
                filename,
                declared_media_type: None,
                reserved_filenames: Vec::new(),
            });
        }
        self.ingest_attachments(owner, inputs, window, cx);
    }

    /// Consumes file and image payloads so their display names never fall through
    /// to ordinary text paste. Text-only clipboard input stays with the editor.
    pub(crate) fn paste_attachments(
        &mut self,
        item: &ClipboardItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let has_paths = item
            .entries()
            .iter()
            .any(|entry| matches!(entry, ClipboardEntry::ExternalPaths(_)));
        let has_images = item
            .entries()
            .iter()
            .any(|entry| matches!(entry, ClipboardEntry::Image(_)));
        if !has_paths && !has_images {
            return false;
        }
        if !self.composer_editable() || !self.persistence_loaded() || self.close_in_progress() {
            return true;
        }
        let Some(owner) = self.composer_key.clone() else {
            return true;
        };
        let mut paths = Vec::new();
        for entry in item.entries() {
            match entry {
                ClipboardEntry::ExternalPaths(selected) => {
                    paths.extend_from_slice(selected.paths())
                }
                ClipboardEntry::Image(image) => {
                    let (extension, media_type) = clipboard_format(image.format());
                    if image.bytes().len() > MAX_INLINE_ATTACHMENT_BYTES {
                        self.notify_mutation_failure(
                            "chat.chatInput.toast.clipboardAttachFailed",
                            window,
                            cx,
                        );
                        continue;
                    }
                    self.attachment_input.clipboard_sequence =
                        self.attachment_input.clipboard_sequence.wrapping_add(1);
                    let input = AttachmentIntake {
                        source: match image.format() {
                            ImageFormat::Bmp
                            | ImageFormat::Tiff
                            | ImageFormat::Ico
                            | ImageFormat::Pnm => AttachmentInputSource::ClipboardImage {
                                bytes: image.bytes().to_vec(),
                                format: image.format(),
                            },
                            _ => AttachmentInputSource::Bytes(image.bytes().to_vec()),
                        },
                        filename: format!(
                            "pasted-image-{}.{extension}",
                            self.attachment_input.clipboard_sequence
                        ),
                        declared_media_type: Some(media_type.to_owned()),
                        reserved_filenames: Vec::new(),
                    };
                    self.ingest_attachments(owner.clone(), vec![input], window, cx);
                }
                // File-list producers often also publish a filename string.
                ClipboardEntry::String(text) if !has_paths => {
                    self.composer.update(cx, |composer, cx| {
                        composer.replace_text_in_range(None, &text.text, window, cx)
                    });
                }
                ClipboardEntry::String(_) => {}
            }
        }
        self.attach_selected_paths(owner, paths, window, cx);
        true
    }
}

fn clipboard_format(format: ImageFormat) -> (&'static str, &'static str) {
    match format {
        ImageFormat::Png => ("png", "image/png"),
        ImageFormat::Jpeg => ("jpg", "image/jpeg"),
        ImageFormat::Gif => ("gif", "image/gif"),
        ImageFormat::Webp => ("webp", "image/webp"),
        ImageFormat::Svg => ("svg", "image/svg+xml"),
        ImageFormat::Bmp => ("bmp", "image/bmp"),
        ImageFormat::Tiff => ("tiff", "image/tiff"),
        ImageFormat::Ico => ("ico", "image/x-icon"),
        ImageFormat::Pnm => ("pnm", "image/x-portable-anymap"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_images_keep_their_real_encoding() {
        assert_eq!(clipboard_format(ImageFormat::Png), ("png", "image/png"));
        assert_eq!(clipboard_format(ImageFormat::Jpeg), ("jpg", "image/jpeg"));
        assert_eq!(clipboard_format(ImageFormat::Bmp), ("bmp", "image/bmp"));
        assert_eq!(clipboard_format(ImageFormat::Tiff), ("tiff", "image/tiff"));
    }
}
