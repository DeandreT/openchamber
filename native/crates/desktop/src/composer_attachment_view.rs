//! Captured composer file/context rows and explicit recovery actions.

use std::collections::HashSet;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{Disableable, Sizable, Size};
use gpui_kit::{
    AnyElement, Context, ElementId, InteractiveElement, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window, div, px,
};
use openchamber_native_core::draft::DraftKey;
use openchamber_native_core::draft_content::DraftFileContent;
use openchamber_native_core::ids::{AttachmentId, ContextItemId};

use crate::attachment_storage::AttachmentStorageStatus;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::composer_document::{ComposerReferenceKind, ComposerSourceKey};
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

impl Workspace {
    pub(crate) fn render_composer_attachments(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let owner = self.composer_key.as_ref()?;
        let strings = locale::strings(cx);
        let editable = self.composer_editable() && !self.close_in_progress();
        let rows: Vec<_> = self.attachment_storage.rows(owner).collect();
        let represented: HashSet<_> = rows.iter().filter_map(|row| row.id.as_ref()).collect();
        let mut elements = Vec::new();
        if let Some(draft) = self.drafts.get(owner) {
            for file in &draft.content.files {
                if represented.contains(&file.id) {
                    continue;
                }
                let name = file
                    .name
                    .clone()
                    .unwrap_or_else(|| strings.get("chat.fileAttachment.fileFallback").to_string());
                let prepared = matches!(file.content, DraftFileContent::Prepared);
                let ready = !prepared || self.prepared_attachment_ready(owner, &file.id);
                let busy = prepared && self.attachment_staging.stages_id(&file.id);
                let status = if ready {
                    SharedString::default()
                } else {
                    strings.get(if busy {
                        "common.loading"
                    } else {
                        "chat.fileAttachment.status.notReady"
                    })
                };
                let captured_owner = owner.clone();
                let captured_id = file.id.clone();
                let remove = Button::new(format!("attachment-{}-remove", file.id))
                    .accessibility_id(format!("attachment-{}-remove", file.id))
                    .ghost()
                    .with_size(Size::Small)
                    .w(px(24.0))
                    .p_0()
                    .accessibility_label(strings.format(
                        "chat.fileAttachment.actions.removeNamed",
                        &[("name", &name)],
                    ))
                    .disabled(!editable)
                    .child(glyph("close", 16.0, palette.muted_foreground))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.remove_composer_attachment(&captured_owner, &captured_id, window, cx);
                    }))
                    .into_any_element();
                let mut actions = Vec::new();
                if prepared && !ready {
                    let captured_owner = owner.clone();
                    let captured_id = file.id.clone();
                    actions.push(
                        Button::new(format!("attachment-{}-retry", file.id))
                            .accessibility_id(format!("attachment-{}-retry", file.id))
                            .ghost()
                            .with_size(Size::Small)
                            .w(px(24.0))
                            .p_0()
                            .accessibility_label(strings.get("chat.fileAttachment.actions.retry"))
                            .disabled(!editable || busy)
                            .child(glyph("refresh", 16.0, palette.muted_foreground))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.stage_prepared_attachment(
                                    captured_owner.clone(),
                                    captured_id.clone(),
                                    window,
                                    cx,
                                );
                            }))
                            .into_any_element(),
                    );
                }
                actions.push(remove);
                elements.push(attachment_row(
                    format!("attachment-{}", file.id),
                    name,
                    status,
                    actions,
                    palette,
                    direction,
                    cx,
                ));
            }
            for context in &draft.content.context {
                let name = context
                    .description
                    .as_deref()
                    .filter(|text| !text.trim().is_empty())
                    .unwrap_or(&context.text)
                    .chars()
                    .take(160)
                    .collect::<String>();
                let captured_owner = owner.clone();
                let captured_id = context.id.clone();
                let remove = Button::new(format!("context-{}-remove", context.id))
                    .accessibility_id(format!("context-{}-remove", context.id))
                    .ghost()
                    .with_size(Size::Small)
                    .w(px(24.0))
                    .p_0()
                    .accessibility_label(strings.get("chat.chatInput.contextPreview.remove"))
                    .disabled(!editable)
                    .child(glyph("close", 16.0, palette.muted_foreground))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.remove_composer_context(&captured_owner, &captured_id, window, cx);
                    }))
                    .into_any_element();
                elements.push(attachment_row(
                    format!("composer-context-{}", context.id),
                    name,
                    SharedString::default(),
                    vec![remove],
                    palette,
                    direction,
                    cx,
                ));
            }
        }
        let companion_editable = editable && self.composer_document_ready(owner);
        for (key, attachment) in self.composer_companion_attachments(owner) {
            let ComposerSourceKey::Companion {
                operation,
                companion,
                item,
            } = key
            else {
                continue;
            };
            let kind = match attachment.kind {
                ComposerReferenceKind::File => "file",
                ComposerReferenceKind::FileReference => "file-reference",
                #[cfg(test)]
                ComposerReferenceKind::Agent | ComposerReferenceKind::Skill => continue,
            };
            let id = format!(
                "composer-reference:{}:{}:{item}:{kind}:{}",
                operation.as_str(),
                companion.as_str(),
                attachment.ordinal
            );
            let captured_owner = owner.clone();
            let captured_key = key.clone();
            let captured_kind = attachment.kind;
            let ordinal = attachment.ordinal;
            let remove = Button::new(format!("{id}-remove"))
                .accessibility_id(format!("{id}-remove"))
                .ghost()
                .with_size(Size::Small)
                .w(px(24.0))
                .p_0()
                .accessibility_label(strings.format(
                    "chat.fileAttachment.actions.removeNamed",
                    &[("name", &attachment.name)],
                ))
                .disabled(!companion_editable)
                .child(glyph("close", 16.0, palette.muted_foreground))
                .on_click(cx.listener(move |this, _, window, cx| {
                    if this.composer_key.as_ref() != Some(&captured_owner)
                        || !this.composer_editable()
                    {
                        return;
                    }
                    if this
                        .remove_composer_reference(
                            &captured_owner,
                            &captured_key,
                            captured_kind,
                            ordinal,
                            window,
                            cx,
                        )
                        .is_ok()
                    {
                        this.composer
                            .update(cx, |composer, cx| composer.focus(window, cx));
                    }
                }))
                .into_any_element();
            elements.push(attachment_row(
                id,
                attachment.name.clone(),
                SharedString::default(),
                vec![remove],
                palette,
                direction,
                cx,
            ));
        }
        for row in rows {
            let failed = matches!(row.status, AttachmentStorageStatus::Failed(_));
            let status = match &row.status {
                AttachmentStorageStatus::Failed(error) => {
                    crate::attachment_copy::storage_failure(error, cx)
                }
                _ => strings.get("common.loading"),
            };
            let mut actions = Vec::new();
            if let Some(id) = &row.id {
                if failed {
                    let id = id.clone();
                    actions.push(
                        Button::new(format!("attachment-input-{}-retry", row.ticket))
                            .accessibility_id(format!("attachment-input-{}-retry", row.ticket))
                            .ghost()
                            .with_size(Size::Small)
                            .w(px(24.0))
                            .p_0()
                            .accessibility_label(strings.get("chat.fileAttachment.actions.retry"))
                            .disabled(!editable)
                            .child(glyph("refresh", 16.0, palette.muted_foreground))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.retry_attachment_storage(&id, window, cx);
                            }))
                            .into_any_element(),
                    );
                }
                let id = id.clone();
                actions.push(
                    Button::new(format!("attachment-input-{}-remove", row.ticket))
                        .accessibility_id(format!("attachment-input-{}-remove", row.ticket))
                        .ghost()
                        .with_size(Size::Small)
                        .w(px(24.0))
                        .p_0()
                        .accessibility_label(strings.format(
                            "chat.fileAttachment.actions.removeNamed",
                            &[("name", &row.filename)],
                        ))
                        .disabled(!editable)
                        .child(glyph("close", 16.0, palette.muted_foreground))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.discard_attachment_capture(&id, window, cx);
                        }))
                        .into_any_element(),
                );
            } else {
                let ticket = row.ticket;
                actions.push(
                    Button::new(format!("attachment-input-{}-remove", ticket))
                        .accessibility_id(format!("attachment-input-{}-remove", ticket))
                        .ghost()
                        .with_size(Size::Small)
                        .w(px(24.0))
                        .p_0()
                        .accessibility_label(strings.format(
                            "chat.fileAttachment.actions.removeNamed",
                            &[("name", &row.filename)],
                        ))
                        .disabled(!editable)
                        .child(glyph("close", 16.0, palette.muted_foreground))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.cancel_attachment_input(ticket, window, cx);
                        }))
                        .into_any_element(),
                );
            }
            elements.push(attachment_row(
                format!("attachment-input-{}", row.ticket),
                row.filename.clone(),
                status,
                actions,
                palette,
                direction,
                cx,
            ));
        }
        // A restart keeps incomplete pre-put ownership without reading unknown
        // bytes. Explicit removal retires that intent; adding the file starts a
        // new capture. Never pretend a filename or payload can be reconstructed.
        for hold in self
            .attachment_preparations
            .iter()
            .filter(|hold| &hold.owner == owner && !represented.contains(&hold.id))
        {
            let id = hold.id.clone();
            let remove =
                Button::new(format!("attachment-hold-{}-remove", id))
                    .accessibility_id(format!("attachment-hold-{}-remove", id))
                    .ghost()
                    .with_size(Size::Small)
                    .w(px(24.0))
                    .p_0()
                    .accessibility_label(strings.get("chat.chatInput.contextPreview.remove"))
                    .disabled(!editable)
                    .child(glyph("close", 16.0, palette.muted_foreground))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.retire_preparation(&id, window, cx)
                    }))
                    .into_any_element();
            elements.push(attachment_row(
                format!("attachment-hold-{}", hold.id),
                strings.get("chat.fileAttachment.fileFallback").to_string(),
                strings.get("chat.fileAttachment.status.notReady"),
                vec![remove],
                palette,
                direction,
                cx,
            ));
        }
        if elements.is_empty() {
            return None;
        }
        Some(
            div()
                .id("composer-attachments")
                .max_h(px(128.0))
                .overflow_y_scroll()
                .min_w_0()
                .px(px(12.0))
                .pt(px(8.0))
                .flex_row_from(direction)
                .flex_wrap()
                .gap(px(8.0))
                .children(elements)
                .into_any_element(),
        )
    }

    pub(crate) fn remove_composer_attachment(
        &mut self,
        owner: &DraftKey,
        id: &AttachmentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.composer_key.as_ref() != Some(owner)
            || !self.composer_editable()
            || self.close_in_progress()
        {
            return;
        }
        self.save_composer_draft(cx);
        let group = self
            .drafts
            .get(owner)
            .and_then(|draft| draft.content.files.iter().find(|file| &file.id == id))
            .map(|file| file.source_document_id.clone());
        let Some(group) = group else { return };
        if let Some(group) = &group {
            self.cancel_attachment_group(owner, group, window, cx);
        }
        let Some(mut draft) = self.drafts.take(owner) else {
            return;
        };
        let mut removed = Vec::new();
        draft.content.files.retain(|file| {
            let remove = if let Some(group) = &group {
                file.source_document_id.as_ref() == Some(group)
            } else {
                &file.id == id
            };
            if remove {
                removed.push(file.id.clone());
            }
            !remove
        });
        for id in &removed {
            self.invalidate_attachment_staging(owner, Some(id), cx);
        }
        self.save_draft_preview(owner, &draft, cx);
        self.drafts.save(owner.clone(), draft);
        self.queue_persistence(cx);
        for id in removed {
            self.retire_attachment(id, window, cx);
        }
        cx.notify();
    }

    fn remove_composer_context(
        &mut self,
        owner: &DraftKey,
        id: &ContextItemId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.composer_key.as_ref() != Some(owner)
            || !self.composer_editable()
            || self.close_in_progress()
        {
            return;
        }
        self.save_composer_draft(cx);
        let Some(mut draft) = self.drafts.take(owner) else {
            return;
        };
        draft.content.context.retain(|context| &context.id != id);
        self.save_draft_preview(owner, &draft, cx);
        self.drafts.save(owner.clone(), draft);
        self.queue_persistence(cx);
        cx.notify();
    }
}

fn attachment_row(
    id: String,
    name: String,
    status: SharedString,
    actions: Vec<AnyElement>,
    palette: &Palette,
    direction: Direction,
    cx: &Context<Workspace>,
) -> AnyElement {
    div()
        .id(ElementId::Name(id.clone().into()))
        .accessibility_id(id)
        .role(Role::Group)
        .aria_label(format!("{name}. {status}"))
        .test_support()
        .min_w_0()
        .max_w(px(280.0))
        .rounded(px(6.0))
        .px(px(8.0))
        .py(px(4.0))
        .bg(palette.subtle)
        .flex_row_from(direction)
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .flex_col()
                .child(
                    div()
                        .min_w_0()
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(4.0))
                        .css_text(0.75, 1.0, cx)
                        .text_color(palette.foreground)
                        .child(glyph("file-text", 14.0, palette.foreground))
                        .child(div().min_w_0().truncate().child(name)),
                )
                .child(
                    div()
                        .h(px(16.0))
                        .truncate()
                        .css_text(0.75, 1.0, cx)
                        .text_color(palette.muted_foreground)
                        .child(status),
                ),
        )
        .children(actions)
        .into_any_element()
}
