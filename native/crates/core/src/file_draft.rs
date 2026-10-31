//! One open file in the Files panel: what was loaded, the draft, its save
//! and the poll that notices external changes.
//!
//! Ports `fileEditorContent.ts`, `fileStatChange.ts`, `fileContentPoller.ts`
//! and the save rules of `FilesView.tsx`. The server's write is
//! last-writer-wins, so this client protects a dirty draft itself: a poll
//! compares size and mtime, never replaces a dirty draft, and swaps a clean
//! file's content in place once two reads agree. A save writes one snapshot
//! of the draft, so edits made while it runs stay unsaved.

/// Files above this many characters open as a preview first.
pub const LARGE_FILE_CHARS: usize = 200_000;
/// A clean file this large or smaller is swapped in place on a change.
pub const MAX_CONTENT_POLL_BYTES: u64 = 200_000;
/// `MIN_MTIME_CHANGE_MS`: jitter below this is not a change.
const MIN_MTIME_CHANGE_MS: f64 = 1.0;

/// The line ending a file was loaded with, restored on save.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineEnding {
    #[default]
    Lf,
    Crlf,
}

/// The editor's text of `content`, with `\n` line ends, and the ending the
/// file mostly used.
pub fn prepare(content: &str) -> (String, LineEnding) {
    let bytes = content.as_bytes();
    let (mut crlf, mut lf) = (0usize, 0usize);
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        if index > 0 && bytes[index - 1] == b'\r' {
            crlf += 1;
        } else {
            lf += 1;
        }
    }
    let ending = if crlf > lf {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    };
    (normalize(content), ending)
}

/// The text to write for `draft` in a file with `ending`.
pub fn serialize(draft: &str, ending: LineEnding) -> String {
    let normalized = normalize(draft);
    match ending {
        LineEnding::Lf => normalized,
        LineEnding::Crlf => normalized.replace('\n', "\r\n"),
    }
}

fn normalize(content: &str) -> String {
    content.replace("\r\n", "\n").replace('\r', "\n")
}

/// What a stat reports about a file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FileStat {
    pub size: u64,
    pub mtime_ms: Option<f64>,
}

impl FileStat {
    /// `hasFileStatChanged`.
    pub fn differs_from(&self, previous: &FileStat) -> bool {
        self.size != previous.size
            || matches!((self.mtime_ms, previous.mtime_ms), (Some(latest), Some(before))
                if (latest - before).abs() >= MIN_MTIME_CHANGE_MS)
    }
}

/// One poll's stat answer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PollStat {
    Found(FileStat),
    Missing,
    /// The stat failed for another reason; it proves nothing.
    Failed,
}

/// What the poll asks the caller to do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollAction {
    Nothing,
    /// The file changed and is clean and small: read it and hand the text to
    /// [`OpenFile::observe_content`].
    ReadContent {
        revision: u64,
    },
    /// The file came back after it was missing: load it again.
    Reload,
}

/// Why the open file shows no content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadFailure {
    NotFound,
    PermissionDenied,
    Failed(Option<String>),
}

#[derive(Clone, Debug, PartialEq)]
enum Content {
    Loading,
    Loaded {
        /// The last loaded or saved text, with `\n` line ends.
        saved: String,
        ending: LineEnding,
        /// The stat the content corresponds to; `None` until a stat after
        /// the load or save records it.
        baseline: Option<FileStat>,
    },
    Failed(ReadFailure),
}

/// What started a save.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveTrigger {
    /// The shortcut or the Save button.
    Explicit,
    /// The 1.5 s debounce after typing stops.
    Auto,
}

/// A write the caller performs, then hands back to [`OpenFile::save_finished`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use = "a save names a write the caller must perform"]
pub struct SaveRequest {
    /// The bytes to write, in the file's own line endings.
    pub content: String,
    snapshot: String,
    revision: u64,
}

/// Why no write was needed or allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveRefusal {
    /// Nothing differs from the file; a save counts as done.
    Clean,
    ReadOnly,
    NotLoaded,
    InProgress,
    /// Autosave never writes an empty draft over a non-empty file.
    WouldEmpty,
}

/// The open file.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenFile {
    path: String,
    read_only: bool,
    content: Content,
    draft: String,
    /// Bumps on every load, external swap and finished save, so a read that
    /// started before one of them is stale.
    revision: u64,
    saving: bool,
    /// This poll already saw the file missing.
    saw_missing: bool,
    /// The text of the first of two agreeing reads of an external change.
    pending_change: Option<String>,
}

impl OpenFile {
    pub fn new(path: impl Into<String>, read_only: bool) -> Self {
        Self {
            path: path.into(),
            read_only,
            content: Content::Loading,
            draft: String::new(),
            revision: 0,
            saving: false,
            saw_missing: false,
            pending_change: None,
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// The file moved to `path`; content, draft and save state move with it.
    pub fn moved(&mut self, path: impl Into<String>) {
        self.path = path.into();
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    pub fn draft(&self) -> &str {
        &self.draft
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_loading(&self) -> bool {
        matches!(self.content, Content::Loading)
    }

    pub fn is_saving(&self) -> bool {
        self.saving
    }

    pub fn failure(&self) -> Option<&ReadFailure> {
        match &self.content {
            Content::Failed(failure) => Some(failure),
            _ => None,
        }
    }

    pub fn line_ending(&self) -> Option<LineEnding> {
        match &self.content {
            Content::Loaded { ending, .. } => Some(*ending),
            _ => None,
        }
    }

    /// Whether the draft differs from the loaded text. A failed file with a
    /// kept draft counts as dirty, so saving writes it back.
    pub fn is_dirty(&self) -> bool {
        match &self.content {
            Content::Loaded { saved, .. } => *saved != self.draft,
            Content::Failed(_) => !self.draft.is_empty(),
            Content::Loading => false,
        }
    }

    /// Whether the file opens as a preview first.
    pub fn is_large(&self) -> bool {
        self.draft.chars().count() > LARGE_FILE_CHARS
    }

    /// A read of the whole file finished. Returns the editor's new text.
    pub fn loaded(&mut self, content: &str) -> &str {
        let (text, ending) = prepare(content);
        self.draft = text.clone();
        self.content = Content::Loaded {
            saved: text,
            ending,
            baseline: None,
        };
        self.revision += 1;
        self.saw_missing = false;
        self.pending_change = None;
        &self.draft
    }

    /// The first read failed.
    pub fn load_failed(&mut self, failure: ReadFailure) {
        self.content = Content::Failed(failure);
        self.draft.clear();
        self.revision += 1;
    }

    /// The editor's text changed.
    pub fn edit(&mut self, text: &str) {
        if self.draft != text {
            self.draft.clear();
            self.draft.push_str(text);
        }
    }

    /// Takes one snapshot of the draft to write.
    pub fn begin_save(&mut self, trigger: SaveTrigger) -> Result<SaveRequest, SaveRefusal> {
        if self.read_only {
            return Err(SaveRefusal::ReadOnly);
        }
        if self.saving {
            return Err(SaveRefusal::InProgress);
        }
        let (saved, ending) = match &self.content {
            Content::Loaded { saved, ending, .. } => (saved.as_str(), *ending),
            // A file that went missing with a kept draft writes it back.
            Content::Failed(_) if !self.draft.is_empty() => ("", LineEnding::Lf),
            _ => return Err(SaveRefusal::NotLoaded),
        };
        if *saved == self.draft && matches!(self.content, Content::Loaded { .. }) {
            return Err(SaveRefusal::Clean);
        }
        if trigger == SaveTrigger::Auto && self.draft.is_empty() && !saved.is_empty() {
            return Err(SaveRefusal::WouldEmpty);
        }
        self.saving = true;
        Ok(SaveRequest {
            content: serialize(&self.draft, ending),
            snapshot: self.draft.clone(),
            revision: self.revision,
        })
    }

    /// The write finished. The written snapshot becomes the saved text, so
    /// edits made meanwhile stay dirty; a failure keeps the draft.
    pub fn save_finished(&mut self, request: SaveRequest, written: bool) {
        self.saving = false;
        if !written {
            return;
        }
        let ending = self.line_ending().unwrap_or_default();
        self.content = Content::Loaded {
            saved: request.snapshot,
            ending,
            baseline: None,
        };
        self.revision = self.revision.max(request.revision) + 1;
        self.saw_missing = false;
        self.pending_change = None;
    }

    /// Applies one poll's stat answer (`openFilePollStep` and the metadata
    /// comparison before any content read).
    pub fn poll(&mut self, stat: PollStat) -> PollAction {
        let shows_failure = matches!(self.content, Content::Failed(_));
        let dirty = self.is_dirty();
        match stat {
            PollStat::Failed => PollAction::Nothing,
            PollStat::Missing => {
                if !dirty && !self.saving && !shows_failure {
                    self.saw_missing = true;
                    self.content = Content::Failed(ReadFailure::NotFound);
                    self.draft.clear();
                    self.revision += 1;
                }
                PollAction::Nothing
            }
            PollStat::Found(latest) => {
                if shows_failure {
                    return if self.saw_missing {
                        PollAction::Reload
                    } else {
                        PollAction::Nothing
                    };
                }
                let Content::Loaded { baseline, .. } = &mut self.content else {
                    return PollAction::Nothing;
                };
                let Some(previous) = baseline else {
                    *baseline = Some(latest);
                    return PollAction::Nothing;
                };
                if !latest.differs_from(previous) || dirty || self.saving {
                    // A dirty draft keeps the old baseline, so the change is
                    // still noticed once the draft is saved or discarded.
                    return PollAction::Nothing;
                }
                if latest.size > MAX_CONTENT_POLL_BYTES {
                    *baseline = Some(latest);
                    return PollAction::Reload;
                }
                PollAction::ReadContent {
                    revision: self.revision,
                }
            }
        }
    }

    /// A content read the poll asked for. Returns the editor's new text when
    /// the change was applied. The first read of a change is held until a
    /// second read agrees, so a half-written file is never shown; a dirty
    /// draft or a newer load or save drops the read. `stat` is the stat the
    /// read followed, recorded once the content is observed.
    pub fn observe_content(
        &mut self,
        revision: u64,
        content: &str,
        stat: FileStat,
    ) -> Option<ContentStep> {
        if revision != self.revision || self.is_dirty() || self.saving {
            self.pending_change = None;
            return None;
        }
        let Content::Loaded {
            saved, baseline, ..
        } = &mut self.content
        else {
            return None;
        };
        let (text, ending) = prepare(content);
        if text == *saved {
            *baseline = Some(stat);
            self.pending_change = None;
            return Some(ContentStep::Unchanged);
        }
        if self.pending_change.as_deref() != Some(content) {
            self.pending_change = Some(content.to_owned());
            return Some(ContentStep::Confirm);
        }
        self.pending_change = None;
        self.draft = text.clone();
        self.content = Content::Loaded {
            saved: text,
            ending,
            baseline: Some(stat),
        };
        self.revision += 1;
        Some(ContentStep::Applied)
    }

    /// Records the stat after this client's own write, so the poll does not
    /// read it as an external change.
    pub fn record_stat(&mut self, revision: u64, stat: FileStat) {
        if revision != self.revision {
            return;
        }
        if let Content::Loaded { baseline, .. } = &mut self.content {
            *baseline = Some(stat);
        }
    }
}

/// What an observed content read did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentStep {
    /// The file holds what the editor shows.
    Unchanged,
    /// The file changed; read it once more to confirm.
    Confirm,
    /// The new content replaced the clean draft.
    Applied,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(size: u64, mtime_ms: f64) -> FileStat {
        FileStat {
            size,
            mtime_ms: Some(mtime_ms),
        }
    }

    fn loaded(content: &str) -> OpenFile {
        let mut file = OpenFile::new("/p/a.txt", false);
        file.loaded(content);
        file
    }

    #[test]
    fn line_endings_follow_the_majority_and_return_on_save() {
        assert_eq!(
            prepare("a\r\nb\r\nc\n"),
            ("a\nb\nc\n".to_owned(), LineEnding::Crlf)
        );
        assert_eq!(prepare("a\nb\r\n").1, LineEnding::Lf, "a tie is LF");
        assert_eq!(prepare("a\rb").0, "a\nb");
        assert_eq!(serialize("x\ny\n", LineEnding::Crlf), "x\r\ny\r\n");
        let mut file = loaded("first\r\nsecond\r\n");
        assert_eq!(file.draft(), "first\nsecond\n");
        file.edit("first\nsecond\nthird\n");
        let save = file.begin_save(SaveTrigger::Explicit).expect("dirty");
        assert_eq!(save.content, "first\r\nsecond\r\nthird\r\n");
    }

    #[test]
    fn stat_changes_ignore_sub_millisecond_jitter() {
        assert!(!stat(4, 10.0).differs_from(&stat(4, 10.4)));
        assert!(stat(4, 11.0).differs_from(&stat(4, 10.0)));
        assert!(stat(5, 10.0).differs_from(&stat(4, 10.0)));
        let unknown = FileStat {
            size: 4,
            mtime_ms: None,
        };
        assert!(!unknown.differs_from(&stat(4, 10.0)));
    }

    #[test]
    fn edits_during_a_save_stay_dirty() {
        let mut file = loaded("a\n");
        file.edit("ab\n");
        let save = file.begin_save(SaveTrigger::Explicit).expect("dirty");
        assert_eq!(
            file.begin_save(SaveTrigger::Explicit),
            Err(SaveRefusal::InProgress)
        );
        file.edit("abc\n");
        file.save_finished(save, true);
        assert!(file.is_dirty(), "the newer edit was not written");
        let next = file.begin_save(SaveTrigger::Auto).expect("still dirty");
        assert_eq!(next.content, "abc\n");
        file.save_finished(next, true);
        assert!(!file.is_dirty());
        assert_eq!(
            file.begin_save(SaveTrigger::Explicit),
            Err(SaveRefusal::Clean)
        );
    }

    #[test]
    fn a_failed_save_keeps_the_draft_dirty() {
        let mut file = loaded("a\n");
        file.edit("b\n");
        let save = file.begin_save(SaveTrigger::Explicit).expect("dirty");
        file.save_finished(save, false);
        assert!(file.is_dirty());
        assert_eq!(file.draft(), "b\n");
        assert!(!file.is_saving());
    }

    #[test]
    fn autosave_never_empties_a_file_and_read_only_never_saves() {
        let mut file = loaded("content\n");
        file.edit("");
        assert_eq!(
            file.begin_save(SaveTrigger::Auto),
            Err(SaveRefusal::WouldEmpty)
        );
        assert!(file.begin_save(SaveTrigger::Explicit).is_ok());
        let mut outside = OpenFile::new("/etc/hostname", true);
        outside.loaded("host\n");
        outside.edit("other\n");
        assert_eq!(
            outside.begin_save(SaveTrigger::Explicit),
            Err(SaveRefusal::ReadOnly)
        );
        let mut loading = OpenFile::new("/p/b", false);
        assert_eq!(
            loading.begin_save(SaveTrigger::Explicit),
            Err(SaveRefusal::NotLoaded)
        );
    }

    #[test]
    fn a_clean_change_is_swapped_in_after_two_agreeing_reads() {
        let mut file = loaded("one\n");
        assert_eq!(
            file.poll(PollStat::Found(stat(4, 1.0))),
            PollAction::Nothing
        );
        let PollAction::ReadContent { revision } = file.poll(PollStat::Found(stat(4, 9.0))) else {
            panic!("a changed clean file is read");
        };
        assert_eq!(
            file.observe_content(revision, "two\n", stat(4, 9.0)),
            Some(ContentStep::Confirm)
        );
        assert_eq!(
            file.observe_content(revision, "two\n", stat(4, 9.0)),
            Some(ContentStep::Applied)
        );
        assert_eq!(file.draft(), "two\n");
        assert!(!file.is_dirty());
        assert_eq!(
            file.poll(PollStat::Found(stat(4, 9.0))),
            PollAction::Nothing,
            "the observed stat is the new baseline"
        );
    }

    #[test]
    fn a_dirty_draft_is_never_replaced() {
        let mut file = loaded("one\n");
        let _ = file.poll(PollStat::Found(stat(4, 1.0)));
        let PollAction::ReadContent { revision } = file.poll(PollStat::Found(stat(4, 2.0))) else {
            panic!("read");
        };
        file.edit("mine\n");
        assert_eq!(
            file.observe_content(revision, "theirs\n", stat(6, 2.0)),
            None
        );
        assert_eq!(
            file.poll(PollStat::Found(stat(6, 3.0))),
            PollAction::Nothing
        );
        assert_eq!(file.draft(), "mine\n");
        let save = file.begin_save(SaveTrigger::Explicit).expect("dirty");
        assert_eq!(save.content, "mine\n", "saving writes the draft");
    }

    #[test]
    fn a_read_from_before_a_save_is_dropped() {
        let mut file = loaded("one\n");
        let _ = file.poll(PollStat::Found(stat(4, 1.0)));
        let PollAction::ReadContent { revision } = file.poll(PollStat::Found(stat(4, 2.0))) else {
            panic!("read");
        };
        file.edit("two\n");
        let save = file.begin_save(SaveTrigger::Explicit).expect("dirty");
        file.save_finished(save, true);
        assert_eq!(file.observe_content(revision, "old\n", stat(4, 2.0)), None);
        assert_eq!(file.draft(), "two\n");
    }

    #[test]
    fn a_deleted_file_shows_missing_then_reloads_when_it_returns() {
        let mut file = loaded("one\n");
        assert_eq!(file.poll(PollStat::Missing), PollAction::Nothing);
        assert_eq!(file.failure(), Some(&ReadFailure::NotFound));
        assert_eq!(file.poll(PollStat::Failed), PollAction::Nothing);
        assert_eq!(file.poll(PollStat::Found(stat(3, 5.0))), PollAction::Reload);
        file.loaded("back\n");
        assert_eq!(file.failure(), None);
        assert_eq!(file.draft(), "back\n");
    }

    #[test]
    fn a_deleted_file_with_edits_keeps_them() {
        let mut file = loaded("one\n");
        file.edit("kept\n");
        assert_eq!(file.poll(PollStat::Missing), PollAction::Nothing);
        assert_eq!(file.failure(), None, "the draft stays on screen");
        assert_eq!(file.draft(), "kept\n");
    }

    #[test]
    fn a_failed_first_read_never_reloads_in_a_loop() {
        let mut file = OpenFile::new("/p/x", false);
        file.load_failed(ReadFailure::PermissionDenied);
        assert_eq!(
            file.poll(PollStat::Found(stat(1, 1.0))),
            PollAction::Nothing
        );
    }

    #[test]
    fn a_large_change_reloads_instead_of_swapping() {
        let mut file = loaded("one\n");
        let _ = file.poll(PollStat::Found(stat(4, 1.0)));
        assert_eq!(
            file.poll(PollStat::Found(stat(MAX_CONTENT_POLL_BYTES + 1, 2.0))),
            PollAction::Reload
        );
    }

    #[test]
    fn large_files_open_as_a_preview() {
        let file = loaded(&"x".repeat(LARGE_FILE_CHARS + 1));
        assert!(file.is_large());
        assert!(!loaded(&"x".repeat(LARGE_FILE_CHARS)).is_large());
    }
}
