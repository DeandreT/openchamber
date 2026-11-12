//! Native picker and clipboard adapters for captured attachment intake.
//!
//! A drop or a file paste cites its files in the draft as `[name]` when
//! storage admits them, so the citation is there before anything typed after
//! the drop. A file that is not an image keeps its citation only if it
//! attaches: when its storage row fails or is removed before publication, the
//! citation leaves the draft again with the spaces the insertion added. An
//! insertion waits for an IME composition to end.

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::CommittedInputEdit;
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

/// Where a cited file stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CitedState {
    /// Admitted to storage; its row has neither published nor failed.
    Pending,
    /// Attached, or an image, whose citation stays as the reference's does.
    Kept,
    /// Never attached: its citation leaves the draft once the draft can be
    /// edited.
    Failed,
}

/// One file named by an insertion.
#[derive(Clone, Debug)]
pub(crate) struct CitedFile {
    ticket: u64,
    name: String,
    state: CitedState,
}

impl CitedFile {
    /// A file whose citation depends on its storage row `ticket`. An image's
    /// citation stays whatever happens to the attachment.
    pub(crate) fn new(ticket: u64, name: String) -> Self {
        let state = if is_image_name(&name) {
            CitedState::Kept
        } else {
            CitedState::Pending
        };
        Self {
            ticket,
            name,
            state,
        }
    }
}

/// One insertion of citations and the files it names.
struct CitationGroup {
    owner: DraftKey,
    /// Where the insertion started. Edits before it move it; it tells equal
    /// insertions apart and is checked against the text before any removal.
    start: usize,
    /// The boundary spaces the insertion added around the citations.
    leading: bool,
    trailing: bool,
    /// The files whose citations the insertion still holds.
    files: Vec<CitedFile>,
    /// Waits for an IME composition to end before it enters the draft.
    deferred: bool,
}

impl CitationGroup {
    /// The insertion as it stands in the draft.
    fn text(&self) -> String {
        let names: Vec<&str> = self.files.iter().map(|file| file.name.as_str()).collect();
        with_boundaries(self.leading, &citation_text(&names), self.trailing)
    }

    /// Whether a later storage outcome or a deferred edit still concerns it.
    fn tracked(&self) -> bool {
        self.deferred || self.files.iter().any(|file| file.state != CitedState::Kept)
    }
}

#[derive(Default)]
pub(crate) struct ComposerAttachmentInput {
    picker: Option<(DraftKey, Task<()>)>,
    revisions: HashMap<DraftKey, u64>,
    clipboard_sequence: u64,
    citations: Vec<CitationGroup>,
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
        // The draft goes with its text.
        self.citations.retain(|group| &group.owner != owner);
    }

    /// The file behind `ticket` attached: its citation stays.
    pub(crate) fn settle_citation(&mut self, ticket: u64) {
        for group in &mut self.citations {
            for file in &mut group.files {
                if file.ticket == ticket && file.state == CitedState::Pending {
                    file.state = CitedState::Kept;
                }
            }
        }
        self.citations.retain(CitationGroup::tracked);
    }

    /// Moves the insertions of `owner` with an edit before them. An edit that
    /// starts at an insertion leaves its start, which keeps the insertion's
    /// own edit and typing in front of it from moving it twice.
    pub(crate) fn map_citations(&mut self, owner: &DraftKey, edit: &CommittedInputEdit) {
        for group in &mut self.citations {
            if &group.owner != owner || group.deferred {
                continue;
            }
            for replacement in &edit.replacements {
                let range = &replacement.range;
                if range.end < group.start || (range.end == group.start && range.start < range.end)
                {
                    group.start =
                        (group.start + replacement.text.len()).saturating_sub(range.len());
                }
            }
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
            .child(crate::chrome::glyph("add-circle", 16.0, palette.foreground))
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
                        this.attach_selected_paths(captured.clone(), paths, window, cx);
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
    /// server file-tree entry or text resembling a path. Answers the storage
    /// ticket and name of each admitted path, in selection order.
    pub(crate) fn attach_selected_paths(
        &mut self,
        owner: DraftKey,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<(u64, String)> {
        let mut admitted = Vec::new();
        if !self.persistence_loaded() || self.close_in_progress() {
            return admitted;
        }
        for path in paths {
            let Some(filename) = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
            else {
                self.notify_mutation_failure("chat.chatInput.toast.attachFileFailed", window, cx);
                continue;
            };
            let input = AttachmentIntake {
                source: AttachmentInputSource::LocalPath(path),
                filename: filename.clone(),
                declared_media_type: None,
                reserved_filenames: Vec::new(),
            };
            if let Some(ticket) = self.ingest_attachment(owner.clone(), input, window, cx) {
                admitted.push((ticket, filename));
            }
        }
        admitted
    }

    /// A drop or a file paste attaches the paths and cites each admitted file
    /// in the draft as `[name]`, as the reference's `attachFilesWithCitation`
    /// does. The picker attaches without a citation.
    pub(crate) fn attach_cited_paths(
        &mut self,
        owner: DraftKey,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let files = self
            .attach_selected_paths(owner.clone(), paths, window, cx)
            .into_iter()
            .map(|(ticket, name)| CitedFile::new(ticket, name))
            .collect();
        self.cite_attachments(&owner, files, window, cx);
    }

    /// Cites `files` in the composer as one undo step that leaves the caret
    /// after the citations. A draft that is no longer in the composer gets
    /// none, as in the reference. During an IME composition the insertion
    /// waits for the composition to end, so the preedit stays whole.
    pub(crate) fn cite_attachments(
        &mut self,
        owner: &DraftKey,
        files: Vec<CitedFile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if files.is_empty() || self.composer_key.as_ref() != Some(owner) {
            return;
        }
        let mut group = CitationGroup {
            owner: owner.clone(),
            start: 0,
            leading: false,
            trailing: false,
            files,
            deferred: true,
        };
        if !self.composer.read(cx).is_composing() {
            self.insert_citations(&mut group, window, cx);
        }
        if group.tracked() {
            self.attachment_input.citations.push(group);
        }
    }

    /// Replaces the committed selection with the group's citations. The
    /// boundary spaces come from the text around the range that is replaced.
    fn insert_citations(
        &mut self,
        group: &mut CitationGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        group.leading = false;
        group.trailing = false;
        let cited = group.text();
        let (start, leading, trailing) = self.composer.update(cx, |composer, cx| {
            let selection = composer.selected_range();
            composer.set_selected_range(selection, cx);
            let replaced = composer.selected_range();
            let value = composer.value().to_string();
            let (leading, trailing) =
                boundary_spaces(&cited, &value[..replaced.start], &value[replaced.end..]);
            composer.replace(with_boundaries(leading, &cited, trailing), window, cx);
            (replaced.start, leading, trailing)
        });
        group.start = start;
        group.leading = leading;
        group.trailing = trailing;
        group.deferred = false;
        self.save_composer_draft(cx);
    }

    /// The storage row `ticket` failed or left before it published: its
    /// file's citation leaves the draft.
    pub(crate) fn retract_citation(
        &mut self,
        ticket: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut found = false;
        for group in &mut self.attachment_input.citations {
            if group.deferred {
                // Nothing entered the draft for it yet.
                let before = group.files.len();
                group
                    .files
                    .retain(|file| file.ticket != ticket || file.state != CitedState::Pending);
                found |= group.files.len() != before;
                continue;
            }
            for file in &mut group.files {
                if file.ticket == ticket && file.state == CitedState::Pending {
                    file.state = CitedState::Failed;
                    found = true;
                }
            }
        }
        if found {
            self.flush_citations(window, cx);
        }
    }

    /// Does the citation work that waited: insertions held back by an IME
    /// composition, and removals a composition or a compound document held
    /// back.
    pub(crate) fn flush_citations(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.attachment_input.citations.is_empty() {
            return;
        }
        let groups = std::mem::take(&mut self.attachment_input.citations);
        let mut kept = Vec::with_capacity(groups.len());
        for mut group in groups {
            let shown = self.composer_key.as_ref() == Some(&group.owner);
            if group.deferred {
                if !shown || group.files.is_empty() {
                    // The draft left the composer, or every file failed.
                    continue;
                }
                if !self.composer.read(cx).is_composing() {
                    self.insert_citations(&mut group, window, cx);
                }
            } else {
                let failed: Vec<u64> = group
                    .files
                    .iter()
                    .filter(|file| file.state == CitedState::Failed)
                    .map(|file| file.ticket)
                    .collect();
                for ticket in failed {
                    self.remove_citation(&mut group, ticket, window, cx);
                }
            }
            if group.tracked() {
                kept.push(group);
            }
        }
        // Nothing above adds a group; an edit's handlers run after this.
        self.attachment_input.citations = kept;
    }

    /// Takes the failed file `ticket` out of the group's insertion. Answers
    /// without a change while the draft cannot be edited.
    fn remove_citation(
        &mut self,
        group: &mut CitationGroup,
        ticket: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shown = self.composer_key.as_ref() == Some(&group.owner);
        let text = if shown {
            let composer = self.composer.read(cx);
            if composer.is_composing() {
                return;
            }
            composer.value().to_string()
        } else if self.has_composer_document(&group.owner) {
            return;
        } else {
            match self.drafts.get(&group.owner) {
                Some(draft) => draft.text.clone(),
                None => String::new(),
            }
        };
        let before = group.text();
        let Some(index) = group
            .files
            .iter()
            .position(|file| file.ticket == ticket && file.state == CitedState::Failed)
        else {
            return;
        };
        let file = group.files.remove(index);
        let (range, replacement) = match nearest(&text, &before, group.start) {
            // The insertion is intact: it is written again without the file.
            Some(at) => {
                group.start = at;
                (at..at + before.len(), group.text())
            }
            None => {
                // The user edited the insertion; the file's own citation goes.
                let token = citation_text(&[file.name.as_str()]);
                match nearest(&text, &token, group.start) {
                    Some(at) => (bare_citation(&text, at..at + token.len()), String::new()),
                    // The user removed it already.
                    None => return,
                }
            }
        };
        if shown {
            self.composer.update(cx, |composer, cx| {
                let caret = composer.selected_range();
                composer.set_selected_range(range.clone(), cx);
                composer.replace(replacement.clone(), window, cx);
                let start = moved(caret.start, &range, replacement.len());
                let end = moved(caret.end, &range, replacement.len());
                composer.set_selected_range(start..end, cx);
            });
            self.save_composer_draft(cx);
        } else if let Some(mut draft) = self.drafts.take(&group.owner) {
            let mut text = draft.text.clone();
            text.replace_range(range.clone(), &replacement);
            let start = moved(draft.selection.start, &range, replacement.len());
            let end = moved(draft.selection.end, &range, replacement.len());
            self.record_native_composer_change(&group.owner);
            draft.edit_text(text, start..end);
            self.save_draft_preview(&group.owner, &draft, cx);
            self.drafts.save(group.owner.clone(), draft);
            self.queue_persistence(cx);
        }
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
        self.attach_cited_paths(owner, paths, window, cx);
        true
    }
}

/// `[a.txt] [b.png]`: the reference's `buildAttachmentCitationText`.
fn citation_text(filenames: &[&str]) -> String {
    let cited: Vec<String> = filenames
        .iter()
        .map(|filename| format!("[{filename}]"))
        .collect();
    cited.join(" ")
}

/// Whether a file of this name is an image to the reference, which reads the
/// browser's `image/*` type: the extensions of its `IMAGE_MIME_EXTENSIONS`
/// and their common spellings.
fn is_image_name(filename: &str) -> bool {
    let extension = filename
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        extension.as_str(),
        "avif"
            | "bmp"
            | "gif"
            | "heic"
            | "heif"
            | "ico"
            | "jpg"
            | "jpeg"
            | "png"
            | "svg"
            | "tif"
            | "tiff"
            | "webp"
    )
}

/// What JavaScript's `\s` matches, which the reference's boundary rules use.
fn is_space(character: char) -> bool {
    (character.is_whitespace() && character != '\u{85}') || character == '\u{feff}'
}

/// The reference's `withInlineInsertionBoundaries`: whether a space goes
/// before and after `content` between `before` and `after`. A space separates
/// the insertion from text on either side, except after an opening bracket
/// and before a closing one or sentence punctuation.
fn boundary_spaces(content: &str, before: &str, after: &str) -> (bool, bool) {
    if content.is_empty() {
        return (false, false);
    }
    let leading = before.chars().next_back().is_some_and(|last| {
        !is_space(last) && !matches!(last, '(' | '[' | '{') && !content.starts_with(is_space)
    });
    let trailing = after.chars().next().is_some_and(|first| {
        !is_space(first)
            && !matches!(first, ']' | ')' | '}' | '.' | ',' | ';' | ':' | '!' | '?')
            && !content.ends_with(is_space)
    });
    (leading, trailing)
}

/// `content` with its boundary spaces; nothing for no content.
fn with_boundaries(leading: bool, content: &str, trailing: bool) -> String {
    if content.is_empty() {
        return String::new();
    }
    format!(
        "{}{content}{}",
        if leading { " " } else { "" },
        if trailing { " " } else { "" }
    )
}

/// The start of the occurrence of `needle` in `text` closest to `hint`.
fn nearest(text: &str, needle: &str, hint: usize) -> Option<usize> {
    text.match_indices(needle)
        .map(|(at, _)| at)
        .min_by_key(|at| at.abs_diff(hint))
}

/// The range to delete for a lone citation at `range`: with one of the two
/// spaces around it when it stands between spaces.
fn bare_citation(text: &str, range: Range<usize>) -> Range<usize> {
    if text[..range.start].ends_with(' ') && text[range.end..].starts_with(' ') {
        range.start - 1..range.end
    } else {
        range
    }
}

/// Where `offset` is after `range` became `length` bytes long.
fn moved(offset: usize, range: &Range<usize>, length: usize) -> usize {
    if offset <= range.start {
        offset
    } else if offset >= range.end {
        offset - range.len() + length
    } else {
        range.start + length
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
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, AppContext, Entity, TestAppContext};
    use openchamber_native_core::draft::DraftTarget;
    use openchamber_native_core::ids::{DraftId, RuntimeId};

    use super::*;
    use crate::workspace_tests::open;

    /// Files with tickets 1, 2, ... in order.
    fn files(names: &[&str]) -> Vec<CitedFile> {
        names
            .iter()
            .zip(1..)
            .map(|(name, ticket)| CitedFile::new(ticket, (*name).to_owned()))
            .collect()
    }

    fn update<T>(
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) -> T,
    ) -> T {
        let result = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |this, cx| f(this, window, cx))
            })
            .expect("window is open");
        cx.run_until_parked();
        result
    }

    /// The composer's text and selection.
    fn composer(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> (String, Range<usize>) {
        cx.read(|cx| {
            let composer = workspace.read(cx).composer.read(cx);
            (composer.value().to_string(), composer.selected_range())
        })
    }

    /// Selects a root session and puts `text` in its composer with
    /// `selection`. Answers the composer's draft owner.
    fn compose(
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        text: &str,
        selection: Range<usize>,
        cx: &mut TestAppContext,
    ) -> DraftKey {
        update(window, workspace, cx, |this, window, cx| {
            let session = this
                .sessions
                .active()
                .iter()
                .find(|record| record.parent_id.is_none() && record.directory.is_some())
                .expect("a root session")
                .id
                .clone();
            this.select_session(session, window, cx);
            // A test that composes again starts without earlier insertions.
            this.attachment_input.citations.clear();
            this.composer.update(cx, |composer, cx| {
                composer.set_value(text.to_owned(), window, cx);
                composer.set_selected_range(selection, cx);
            });
            this.save_composer_draft(cx);
            this.composer_key
                .clone()
                .expect("the selected session has a draft owner")
        })
    }

    /// `compose`, then cites `names` under tickets 1, 2, ... there.
    fn cite(
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        text: &str,
        selection: Range<usize>,
        names: &[&str],
        cx: &mut TestAppContext,
    ) -> DraftKey {
        let owner = compose(window, workspace, text, selection, cx);
        let cited = files(names);
        update(window, workspace, cx, |this, window, cx| {
            this.cite_attachments(&owner, cited, window, cx)
        });
        owner
    }

    fn retract(
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        ticket: u64,
        cx: &mut TestAppContext,
    ) {
        update(window, workspace, cx, |this, window, cx| {
            this.retract_citation(ticket, window, cx)
        });
    }

    fn other_owner() -> DraftKey {
        DraftKey::new(
            RuntimeId::from("captured-runtime"),
            "/captured/project",
            DraftTarget::NewSession(DraftId::from("elsewhere")),
        )
        .expect("valid synthetic owner")
    }

    #[test]
    fn citations_are_bracketed_names_joined_by_one_space() {
        assert_eq!(citation_text(&["second.txt"]), "[second.txt]");
        assert_eq!(citation_text(&["a.txt", "b c.png"]), "[a.txt] [b c.png]");
        assert_eq!(citation_text(&[]), "");
    }

    #[test]
    fn insertion_boundaries_follow_the_reference() {
        let cited = "[a.txt]";
        let inserted = |before: &str, after: &str| {
            let (leading, trailing) = boundary_spaces(cited, before, after);
            with_boundaries(leading, cited, trailing)
        };
        assert_eq!(inserted("", ""), "[a.txt]");
        assert_eq!(inserted("see", ""), " [a.txt]");
        assert_eq!(inserted("see ", ""), "[a.txt]");
        assert_eq!(inserted("see\n", ""), "[a.txt]");
        assert_eq!(inserted("(", ")"), "[a.txt]");
        assert_eq!(inserted("", "next"), "[a.txt] ");
        assert_eq!(inserted("", " next"), "[a.txt]");
        for after in ["]", ")", "}", ".", ",", ";", ":", "!", "?"] {
            assert_eq!(inserted("", after), "[a.txt]", "no space before {after:?}");
        }
        assert_eq!(inserted("a", "b"), " [a.txt] ");
        assert_eq!(boundary_spaces("", "a", "b"), (false, false));
        assert_eq!(with_boundaries(true, "", true), "");
    }

    #[test]
    fn images_are_told_by_their_extension() {
        for name in [
            "shot.PNG",
            "photo.jpeg",
            "scan.tif",
            "icon.svg",
            "live.heic",
        ] {
            assert!(is_image_name(name), "{name}");
        }
        for name in ["notes.txt", "png", "archive.png.zip", "folder"] {
            assert!(!is_image_name(name), "{name}");
        }
    }

    #[test]
    fn a_lone_citation_takes_one_of_two_spaces_with_it() {
        assert_eq!(bare_citation("a [x] b", 2..5), 1..5);
        assert_eq!(bare_citation("[x] b", 0..3), 0..3);
        assert_eq!(bare_citation("a [x]", 2..5), 2..5);
        assert_eq!(bare_citation("a[x] b", 1..4), 1..4);
    }

    #[test]
    fn offsets_follow_a_replaced_range() {
        let range = 4..10;
        assert_eq!(moved(2, &range, 0), 2);
        assert_eq!(moved(4, &range, 0), 4);
        assert_eq!(moved(7, &range, 0), 4);
        assert_eq!(moved(7, &range, 3), 7);
        assert_eq!(moved(10, &range, 0), 4);
        assert_eq!(moved(15, &range, 2), 11);
    }

    #[gpui_kit::test]
    fn one_dropped_file_is_cited_in_an_empty_draft(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        cite(window, &workspace, "", 0..0, &["second.txt"], cx);
        assert_eq!(
            composer(&workspace, cx),
            ("[second.txt]".to_owned(), 12..12),
            "the caret follows the citation"
        );
    }

    #[gpui_kit::test]
    fn several_dropped_files_are_cited_in_one_insertion(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        cite(window, &workspace, "compare", 7..7, &["a.txt", "b.png"], cx);
        assert_eq!(
            composer(&workspace, cx),
            ("compare [a.txt] [b.png]".to_owned(), 23..23)
        );
    }

    #[gpui_kit::test]
    fn a_citation_replaces_the_selection_and_keeps_the_caret_after_it(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        cite(
            window,
            &workspace,
            "read this file",
            5..9,
            &["notes.md"],
            cx,
        );
        assert_eq!(
            composer(&workspace, cx),
            ("read [notes.md] file".to_owned(), 15..15)
        );
    }

    #[gpui_kit::test]
    fn a_citation_in_the_middle_of_a_word_gets_both_spaces(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        cite(window, &workspace, "ab", 1..1, &["x.rs"], cx);
        assert_eq!(composer(&workspace, cx), ("a [x.rs] b".to_owned(), 9..9));
    }

    #[gpui_kit::test]
    fn one_undo_takes_the_whole_citation_back(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        cite(window, &workspace, "compare", 7..7, &["a.txt", "b.png"], cx);
        assert_eq!(composer(&workspace, cx).0, "compare [a.txt] [b.png]");
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.focus_composer(window, cx));
            window.render_frame(cx);
            window.press("ctrl-z", cx);
        })
        .expect("window is open");
        cx.run_until_parked();
        assert_eq!(
            composer(&workspace, cx).0,
            "compare",
            "one undo removes every citation of the drop"
        );
    }

    #[gpui_kit::test]
    fn another_drafts_composer_gets_no_citation(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        compose(window, &workspace, "typed", 5..5, cx);
        update(window, &workspace, cx, |this, window, cx| {
            this.cite_attachments(&other_owner(), files(&["late.txt"]), window, cx);
            assert!(this.attachment_input.citations.is_empty());
        });
        assert_eq!(composer(&workspace, cx).0, "typed");
    }

    #[gpui_kit::test]
    fn a_failed_file_takes_its_citation_and_boundary_spaces_back(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        cite(window, &workspace, "ab", 1..1, &["x.rs"], cx);
        assert_eq!(composer(&workspace, cx).0, "a [x.rs] b");
        retract(window, &workspace, 1, cx);
        assert_eq!(
            composer(&workspace, cx),
            ("ab".to_owned(), 1..1),
            "the text and the caret are as before the drop"
        );
        assert!(cx.read(|cx| workspace.read(cx).attachment_input.citations.is_empty()));
    }

    /// The parity scenario's order: the drop, then typing, then the outcome.
    #[gpui_kit::test]
    fn text_typed_after_the_drop_survives_either_outcome(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        for fails in [false, true] {
            cite(window, &workspace, "", 0..0, &["second.txt"], cx);
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |this, cx| this.focus_composer(window, cx));
                window.render_frame(cx);
                window.input(" with attachment", cx);
            })
            .expect("window is open");
            cx.run_until_parked();
            assert_eq!(composer(&workspace, cx).0, "[second.txt] with attachment");
            if fails {
                retract(window, &workspace, 1, cx);
                assert_eq!(
                    composer(&workspace, cx),
                    (" with attachment".to_owned(), 16..16),
                    "the typed text and the caret after it stay"
                );
            } else {
                update(window, &workspace, cx, |this, _, _| {
                    this.attachment_input.settle_citation(1)
                });
                retract(window, &workspace, 1, cx);
                assert_eq!(
                    composer(&workspace, cx).0,
                    "[second.txt] with attachment",
                    "an attached file keeps its citation"
                );
            }
        }
    }

    #[gpui_kit::test]
    fn one_failure_among_several_keeps_the_others_joined_by_one_space(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        for (ticket, expected) in [
            (1, "see [b.md] [c.md] now"),
            (2, "see [a.md] [c.md] now"),
            (3, "see [a.md] [b.md] now"),
        ] {
            cite(
                window,
                &workspace,
                "see now",
                3..3,
                &["a.md", "b.md", "c.md"],
                cx,
            );
            assert_eq!(composer(&workspace, cx).0, "see [a.md] [b.md] [c.md] now");
            retract(window, &workspace, ticket, cx);
            assert_eq!(composer(&workspace, cx).0, expected);
        }
    }

    #[gpui_kit::test]
    fn every_file_failing_leaves_the_draft_as_it_was(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        cite(window, &workspace, "see now", 3..3, &["a.md", "b.md"], cx);
        retract(window, &workspace, 2, cx);
        retract(window, &workspace, 1, cx);
        assert_eq!(composer(&workspace, cx).0, "see now");
    }

    #[gpui_kit::test]
    fn an_image_keeps_its_citation_when_its_row_fails(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        cite(window, &workspace, "", 0..0, &["shot.png", "notes.txt"], cx);
        retract(window, &workspace, 1, cx);
        retract(window, &workspace, 2, cx);
        assert_eq!(composer(&workspace, cx).0, "[shot.png]");
    }

    #[gpui_kit::test]
    fn an_edited_insertion_loses_only_the_failed_citation(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        cite(window, &workspace, "", 0..0, &["a.md", "b.md"], cx);
        // The user writes between the two citations.
        update(window, &workspace, cx, |this, window, cx| {
            this.composer.update(cx, |composer, cx| {
                composer.set_selected_range(6..6, cx);
                composer.replace(" and", window, cx);
            });
        });
        assert_eq!(composer(&workspace, cx).0, "[a.md] and [b.md]");
        retract(window, &workspace, 2, cx);
        assert_eq!(composer(&workspace, cx).0, "[a.md] and ");
        retract(window, &workspace, 1, cx);
        assert_eq!(composer(&workspace, cx).0, " and ");
    }

    #[gpui_kit::test]
    fn equal_citations_lose_the_one_that_failed(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let owner = cite(window, &workspace, "", 0..0, &["a.md"], cx);
        // Text in front of it, then the same file dropped at the end.
        update(window, &workspace, cx, |this, window, cx| {
            this.composer.update(cx, |composer, cx| {
                composer.set_selected_range(0..0, cx);
                composer.replace("first ", window, cx);
                composer.set_selected_range(12..12, cx);
                composer.replace(" then", window, cx);
                composer.set_selected_range(17..17, cx);
            });
        });
        update(window, &workspace, cx, |this, window, cx| {
            let second = vec![CitedFile::new(2, "a.md".to_owned())];
            this.cite_attachments(&owner, second, window, cx);
        });
        assert_eq!(composer(&workspace, cx).0, "first [a.md] then [a.md]");
        retract(window, &workspace, 1, cx);
        assert_eq!(
            composer(&workspace, cx).0,
            "first  then [a.md]",
            "the first drop's citation goes; the spaces around it are the user's"
        );
    }

    #[gpui_kit::test]
    fn a_draft_that_left_the_composer_is_cleaned_where_it_is_stored(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let owner = cite(window, &workspace, "see", 3..3, &["gone.txt"], cx);
        update(window, &workspace, cx, |this, window, cx| {
            this.show_new_session(window, cx);
            assert_ne!(this.composer_key.as_ref(), Some(&owner));
            assert_eq!(
                this.drafts.get(&owner).expect("the draft is stored").text,
                "see [gone.txt]"
            );
        });
        let shown = composer(&workspace, cx).0;
        retract(window, &workspace, 1, cx);
        assert_eq!(
            cx.read(|cx| workspace
                .read(cx)
                .drafts
                .get(&owner)
                .map(|draft| draft.text.clone())),
            Some("see".to_owned())
        );
        assert_eq!(
            composer(&workspace, cx).0,
            shown,
            "the composer on screen is another draft's"
        );
    }

    #[gpui_kit::test]
    fn a_drop_during_composition_waits_for_the_commit(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let owner = compose(window, &workspace, "typed", 5..5, cx);
        update(window, &workspace, cx, |this, window, cx| {
            this.composer.update(cx, |composer, cx| {
                composer.replace_and_mark_text_in_range(None, "に", Some(1..1), window, cx);
            });
            assert!(this.composer.read(cx).is_composing());
            this.cite_attachments(&owner, files(&["a.txt"]), window, cx);
            assert!(
                this.composer.read(cx).is_composing(),
                "the drop leaves the composition open"
            );
            assert_eq!(
                this.composer.read(cx).value().as_ref(),
                "typedに",
                "the preedit is whole and nothing is inserted yet"
            );
        });
        // The input method commits its text.
        update(window, &workspace, cx, |this, window, cx| {
            this.composer.update(cx, |composer, cx| {
                composer.replace_text_in_range(None, "日本", window, cx);
            });
        });
        assert_eq!(
            composer(&workspace, cx),
            ("typed日本 [a.txt]".to_owned(), 19..19),
            "the citation follows the committed text, after one space"
        );
        retract(window, &workspace, 1, cx);
        assert_eq!(composer(&workspace, cx).0, "typed日本");
    }

    #[gpui_kit::test]
    fn a_file_that_fails_during_composition_is_never_cited(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let owner = compose(window, &workspace, "typed", 5..5, cx);
        update(window, &workspace, cx, |this, window, cx| {
            this.composer.update(cx, |composer, cx| {
                composer.replace_and_mark_text_in_range(None, "に", Some(1..1), window, cx);
            });
            this.cite_attachments(&owner, files(&["a.txt", "b.txt"]), window, cx);
            this.retract_citation(1, window, cx);
        });
        // The composition is cancelled.
        update(window, &workspace, cx, |this, window, cx| {
            this.composer.update(cx, |composer, cx| {
                composer.replace_and_mark_text_in_range(None, "", Some(0..0), window, cx);
            });
        });
        assert_eq!(composer(&workspace, cx).0, "typed [b.txt]");
    }

    #[gpui_kit::test]
    fn a_removal_during_composition_waits_and_keeps_the_preedit(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        cite(window, &workspace, "", 0..0, &["a.txt"], cx);
        update(window, &workspace, cx, |this, window, cx| {
            this.composer.update(cx, |composer, cx| {
                composer.replace_and_mark_text_in_range(None, "に", Some(1..1), window, cx);
            });
            this.retract_citation(1, window, cx);
            assert_eq!(this.composer.read(cx).value().as_ref(), "[a.txt]に");
            assert!(this.composer.read(cx).is_composing());
        });
        update(window, &workspace, cx, |this, window, cx| {
            this.composer.update(cx, |composer, cx| {
                composer.replace_text_in_range(None, "日本", window, cx);
            });
        });
        assert_eq!(composer(&workspace, cx).0, "日本");
    }

    #[gpui_kit::test]
    fn a_discarded_draft_forgets_its_citations(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let owner = cite(window, &workspace, "", 0..0, &["a.txt"], cx);
        update(window, &workspace, cx, |this, window, cx| {
            this.attachment_input.cancel(&owner);
            assert!(this.attachment_input.citations.is_empty());
            this.retract_citation(1, window, cx);
        });
        assert_eq!(composer(&workspace, cx).0, "[a.txt]");
    }

    #[test]
    fn clipboard_images_keep_their_real_encoding() {
        assert_eq!(clipboard_format(ImageFormat::Png), ("png", "image/png"));
        assert_eq!(clipboard_format(ImageFormat::Jpeg), ("jpg", "image/jpeg"));
        assert_eq!(clipboard_format(ImageFormat::Bmp), ("bmp", "image/bmp"));
        assert_eq!(clipboard_format(ImageFormat::Tiff), ("tiff", "image/tiff"));
    }
}
