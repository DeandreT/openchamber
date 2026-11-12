//! The new-session draft's Chat target: which runtimes have one open, the
//! folder its session will be created in, and the read of the Chats root.
//!
//! `draft_projects` keeps owning project targets. A runtime's draft targets
//! Chat when it has an entry here and none there, so a project chosen by any
//! path wins. The folder is named by the client once the server names its
//! root and is created on the server by the first send.

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};

use chrono::Datelike;
use gpui_kit::{App, Context, Task, Window};
use openchamber_native_core::chat_draft::{
    self, CHAT_DRAFT_ID, CHAT_PROJECT_ID, ChatDate, DraftDestination, OpenRequest, RootsStatus,
};
use openchamber_native_core::chat_roots::ChatRoots;
use openchamber_native_core::draft::{DraftKey, DraftTarget};
use openchamber_native_core::ids::RuntimeId;

use crate::connection::ConnectionState;
use crate::ui_memory::{self, UiMemory};
use crate::workspace::Workspace;

/// The failure shown when a Chat cannot be created for want of its root.
const NOT_SENT: &str = "chat.nativeRecovery.sendFailed";

/// Which entry point opened the new-session draft.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NewSession {
    /// The titlebar button and the command palette: the side last worked on.
    Plain,
    /// The shortcut and the menu, which forward the selected session's
    /// directory: its project, or another Chat from a chat session.
    FromSelected,
    /// The Chats header's "+".
    Chat,
}

#[derive(Default)]
pub(crate) struct ChatDrafts {
    /// The runtimes whose draft targets Chat, with the folder the session
    /// will be created in; `None` until the server names its Chats root.
    open: HashMap<RuntimeId, Option<String>>,
    /// The runtimes whose first draft already took its default destination.
    defaulted: HashSet<RuntimeId>,
    /// Whether the last read of the current runtime's Chats root failed.
    roots_failed: bool,
    /// The read a failed one is retried with; dropping it cancels the read.
    roots_read: Option<Task<()>>,
    sequence: u64,
}

impl ChatDrafts {
    /// The folder of `runtime`'s Chat draft, when it has one.
    fn folder(&self, runtime: &RuntimeId) -> Option<&str> {
        self.open.get(runtime)?.as_deref()
    }

    /// Stops `runtime`'s draft targeting Chat. Its text stays stored under
    /// its folder and comes back when Chat is targeted again.
    pub(crate) fn close(&mut self, runtime: &RuntimeId) {
        self.open.remove(runtime);
    }

    /// A runtime switch starts a new read of the new runtime's root.
    pub(crate) fn switch_runtime(&mut self) {
        self.roots_failed = false;
        self.roots_read = None;
    }
}

impl Workspace {
    /// Whether the current runtime's new-session draft targets Chat.
    pub(crate) fn chat_draft_open(&self) -> bool {
        !self.draft_projects.contains_key(&self.runtime)
            && self.chat_drafts.open.contains_key(&self.runtime)
    }

    /// The Chat draft's key once its folder is named.
    pub(crate) fn chat_draft_key(&self) -> Option<DraftKey> {
        if !self.chat_draft_open() {
            return None;
        }
        DraftKey::new(
            self.runtime.clone(),
            self.chat_drafts.folder(&self.runtime)?,
            DraftTarget::NewSession(CHAT_DRAFT_ID.into()),
        )
    }

    /// The directory a Chat draft's catalogs, panels and terminal use: the
    /// Chats root. `None` while it is unknown, never a project's directory.
    pub(crate) fn chat_draft_directory(&self) -> Option<String> {
        chat_draft::effective_directory(self.topology.chat_roots.as_ref())
    }

    pub(crate) fn chat_roots_status(&self) -> RootsStatus {
        RootsStatus::of(
            self.topology.chat_roots.as_ref(),
            self.session_admission.roots_ready,
            self.chat_drafts.roots_failed,
        )
    }

    /// Whether `key` is a Chat draft whose folder the known roots hold.
    pub(crate) fn is_chat_creation(&self, key: &DraftKey) -> bool {
        matches!(key.target(), DraftTarget::NewSession(id) if id.as_str() == CHAT_DRAFT_ID)
            && self
                .topology
                .chat_roots
                .as_ref()
                .is_some_and(|roots| chat_draft::is_session_folder(roots, key.directory()))
    }

    /// Opens the new-session draft on the destination `origin` asks for.
    /// Synthetic data has no server to create a Chat on and keeps its draft.
    pub(crate) fn open_new_session(
        &mut self,
        origin: NewSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_synthetic() {
            self.show_new_session(window, cx);
            return;
        }
        let directory = match origin {
            NewSession::FromSelected => self
                .selected_session()
                .and_then(|session| session.directory.clone()),
            NewSession::Plain | NewSession::Chat => None,
        };
        let last = self.last_draft_destination(cx);
        let destination = chat_draft::resolve_destination(
            OpenRequest {
                chat: origin == NewSession::Chat,
                directory: directory.as_deref(),
                last: last.as_ref(),
            },
            self.topology.chat_roots.as_ref(),
            |directory| self.draft_destination_project(directory).is_some(),
        );
        self.show_new_session(window, cx);
        self.retarget_draft(&destination, false, window, cx);
        self.focus_composer(window, cx);
        if destination == DraftDestination::Chat {
            self.retry_failed_chat_roots(window, cx);
        }
    }

    /// The target picker's choice for the open draft: a registered
    /// directory, or `CHAT_PROJECT_ID` for Chat. The draft stays the same
    /// one, so its text goes along to the new destination.
    pub(crate) fn move_draft_to(
        &mut self,
        choice: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected.is_some() {
            return;
        }
        let destination = if choice == CHAT_PROJECT_ID {
            DraftDestination::Chat
        } else if self.draft_destination_project(choice).is_some() {
            DraftDestination::Project(choice.to_owned())
        } else {
            return;
        };
        self.retarget_draft(&destination, true, window, cx);
        self.focus_composer(window, cx);
        if destination == DraftDestination::Chat {
            self.retry_failed_chat_roots(window, cx);
        }
    }

    /// The one writer of the draft's destination. With `carry`, text typed
    /// under the previous destination moves to the new one.
    fn retarget_draft(
        &mut self,
        destination: &DraftDestination,
        carry: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let on_draft = self.selected.is_none();
        if on_draft {
            self.save_composer_draft(cx);
        }
        let before = self.new_session_draft_key();
        let runtime = self.runtime.clone();
        match destination {
            DraftDestination::Chat => {
                self.draft_projects.remove(&runtime);
                self.chat_drafts.open.entry(runtime).or_insert(None);
                self.name_chat_folder();
            }
            DraftDestination::Project(directory) => {
                self.chat_drafts.close(&runtime);
                self.draft_projects.insert(runtime, directory.clone());
            }
        }
        self.remember_draft_destination(destination, cx);
        let after = self.new_session_draft_key();
        if carry {
            self.carry_draft_text(before.as_ref(), after.as_ref(), cx);
        }
        if on_draft {
            self.navigation_revision = self.navigation_revision.wrapping_add(1);
            self.composer_key = after;
            self.restore_composer_draft(window, cx);
            self.ensure_send_catalog(cx);
        }
        cx.notify();
    }

    /// The first draft of a runtime opens on the side last worked on, once
    /// the registered projects are known. Later reads change nothing, and a
    /// destination the user already picked is kept.
    pub(crate) fn open_default_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_synthetic() || !self.chat_drafts.defaulted.insert(self.runtime.clone()) {
            return;
        }
        if self.draft_projects.contains_key(&self.runtime)
            || self.chat_drafts.open.contains_key(&self.runtime)
        {
            return;
        }
        let last = self.last_draft_destination(cx);
        let destination = chat_draft::resolve_destination(
            OpenRequest {
                last: last.as_ref(),
                ..OpenRequest::default()
            },
            self.topology.chat_roots.as_ref(),
            |directory| self.draft_destination_project(directory).is_some(),
        );
        // Text typed before the destination was known belongs to this draft.
        self.retarget_draft(&destination, true, window, cx);
    }

    /// Names the Chat draft's folder when the roots allow it: the folder a
    /// stored Chat draft was saved under, else a new one. A folder outside
    /// the current roots is replaced, since the server would refuse it.
    fn name_chat_folder(&mut self) {
        let runtime = self.runtime.clone();
        if !self.chat_drafts.open.contains_key(&runtime) {
            return;
        }
        let Some(roots) = self.topology.chat_roots.clone() else {
            return;
        };
        if self
            .chat_drafts
            .folder(&runtime)
            .is_some_and(|folder| chat_draft::is_session_folder(&roots, folder))
        {
            return;
        }
        let stored = self
            .drafts
            .iter()
            .map(|(key, _)| key)
            .filter(|key| {
                key.runtime() == &runtime
                    && matches!(
                        key.target(),
                        DraftTarget::NewSession(id) if id.as_str() == CHAT_DRAFT_ID
                    )
                    && chat_draft::is_session_folder(&roots, key.directory())
            })
            .map(|key| key.directory().to_owned())
            .min();
        let folder = stored.or_else(|| self.new_chat_folder(&roots));
        self.chat_drafts.open.insert(runtime, folder);
    }

    fn new_chat_folder(&mut self, roots: &ChatRoots) -> Option<String> {
        let today = chrono::Local::now().date_naive();
        self.chat_drafts.sequence = self.chat_drafts.sequence.wrapping_add(1);
        let mut id = String::with_capacity(32);
        for half in 0..2_u8 {
            // Uniqueness only: the name confers no authority.
            let mut hasher = RandomState::new().build_hasher();
            hasher.write_u64(self.chat_drafts.sequence);
            hasher.write_u8(half);
            id.push_str(&format!("{:016x}", hasher.finish()));
        }
        chat_draft::session_directory(
            roots,
            ChatDate {
                year: today.year(),
                month: today.month(),
                day: today.day(),
            },
            &id,
        )
    }

    /// Follows an answer about the Chats root: a Chat draft waiting for its
    /// folder gets one, and what was typed meanwhile moves under it.
    pub(crate) fn chat_roots_read(
        &mut self,
        succeeded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.chat_drafts.roots_failed = !succeeded;
        if succeeded {
            self.settle_chat_folder(window, cx);
        }
        cx.notify();
    }

    /// Also runs after stored drafts load, which can name the folder a Chat
    /// draft was saved under before the restart.
    pub(crate) fn settle_chat_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.chat_draft_open() {
            return;
        }
        let before = self.new_session_draft_key();
        self.name_chat_folder();
        let after = self.new_session_draft_key();
        if before == after {
            return;
        }
        let on_draft = self.selected.is_none() && self.composer_key == before;
        if on_draft {
            self.save_composer_draft(cx);
        }
        self.carry_draft_text(before.as_ref(), after.as_ref(), cx);
        if on_draft {
            self.composer_key = after;
            self.restore_composer_draft(window, cx);
            self.ensure_send_catalog(cx);
        }
    }

    /// After a Chat's session exists its folder is that session's: the
    /// draft takes a new one, with whatever was typed since the send.
    pub(crate) fn renew_chat_folder(
        &mut self,
        runtime: &RuntimeId,
        used: &DraftKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.chat_drafts.folder(runtime) != Some(used.directory()) {
            return;
        }
        if runtime != &self.runtime {
            // Its roots are read again when that runtime is back.
            self.chat_drafts.open.insert(runtime.clone(), None);
            return;
        }
        let folder = self
            .topology
            .chat_roots
            .clone()
            .and_then(|roots| self.new_chat_folder(&roots));
        self.chat_drafts.open.insert(runtime.clone(), folder);
        if !self.chat_draft_open() {
            return;
        }
        let after = self.new_session_draft_key();
        let on_draft = self.selected.is_none() && self.composer_key.as_ref() == Some(used);
        if on_draft {
            self.save_composer_draft(cx);
        }
        self.carry_draft_text(Some(used), after.as_ref(), cx);
        if on_draft {
            self.composer_key = after;
            self.restore_composer_draft(window, cx);
        }
    }

    /// Moves a draft's text from one key to another, after whatever the
    /// destination already kept. Files, context and mentions name the old
    /// directory and stay there. A compound document is left where it is.
    fn carry_draft_text(
        &mut self,
        from: Option<&DraftKey>,
        to: Option<&DraftKey>,
        cx: &mut Context<Self>,
    ) {
        let (Some(from), Some(to)) = (from, to) else {
            return;
        };
        if from == to
            || self.composer_documents.contains(from)
            || self.composer_documents.contains(to)
        {
            return;
        }
        let Some(mut left) = self.drafts.take(from) else {
            return;
        };
        let outgoing = left.text.clone();
        if outgoing.is_empty() {
            self.drafts.save(from.clone(), left);
            return;
        }
        left.edit_text(String::new(), 0..0);
        self.record_native_composer_change(from);
        self.save_draft_preview(from, &left, cx);
        self.drafts.save(from.clone(), left);
        let mut kept = self.drafts.take(to).unwrap_or_default();
        let text = chat_draft::carried_text(&kept.text, &outgoing);
        let end = text.len();
        kept.edit_text(text, end..end);
        self.record_native_composer_change(to);
        self.save_draft_preview(to, &kept, cx);
        self.drafts.save(to.clone(), kept);
        self.queue_persistence(cx);
    }

    /// Whether a send from the Chat draft has to stop for want of a folder.
    /// The draft and its text stay; a failed root read is tried again and
    /// the failure is shown, so the message is never created elsewhere.
    pub(crate) fn chat_send_blocked(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.selected.is_some() || !self.chat_draft_open() || self.chat_draft_key().is_some() {
            return false;
        }
        if self.composer_has_sendable_content(cx)
            && matches!(
                self.chat_roots_status(),
                RootsStatus::Failed | RootsStatus::Missing
            )
        {
            self.notify_mutation_failure(NOT_SENT, window, cx);
            self.retry_failed_chat_roots(window, cx);
        }
        true
    }

    /// Reads the Chats root again after a failed read.
    pub(crate) fn retry_failed_chat_roots(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.chat_roots_status() != RootsStatus::Failed || self.chat_drafts.roots_read.is_some()
        {
            return;
        }
        let Some(connection) = self
            .connection
            .as_ref()
            .filter(|connection| matches!(connection.state, ConnectionState::Connected))
        else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let runtime = self.runtime.clone();
        let read = connection
            .network
            .run(async move { client.chat_roots().await });
        self.chat_drafts.roots_read = Some(cx.spawn_in(window, async move |this, cx| {
            let result = read.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.chat_drafts.roots_read = None;
                if this.runtime != runtime {
                    return;
                }
                match result {
                    Some(Ok(roots)) => {
                        this.topology.chat_roots = roots;
                        this.session_admission.roots_ready = true;
                        this.chat_roots_read(true, window, cx);
                        this.rebuild_sidebar();
                    }
                    Some(Err(error)) => {
                        tracing::warn!(%error, "the Chats roots could not be read");
                        this.chat_roots_read(false, window, cx);
                    }
                    None => {}
                }
            });
        }));
    }

    /// The side of the target picker last worked on, for this runtime.
    fn last_draft_destination(&self, cx: &App) -> Option<DraftDestination> {
        cx.try_global::<UiMemory>()?
            .draft_destination(self.runtime.as_str())
    }

    pub(crate) fn remember_draft_destination(
        &self,
        destination: &DraftDestination,
        cx: &mut Context<Self>,
    ) {
        if self.is_synthetic() {
            return;
        }
        ui_memory::remember_draft_destination(self.runtime.as_str(), destination, cx);
    }
}

#[cfg(test)]
#[path = "chat_draft_tests.rs"]
mod tests;
