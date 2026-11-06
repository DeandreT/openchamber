//! The issues and pull requests board (`sourceBoard/SourceBoardView.tsx`):
//! a page over the chat that lists a project's repository issues or change
//! requests on whichever provider hosts it, previews the highlighted one and
//! acts on it.
//!
//! The board keeps its own project: it opens on the one it was left on, else
//! the active project, and switching it changes nothing else in the app. The
//! folder is read through the server only. Which account reads it comes from
//! the repository binding and the hosts' current accounts
//! (`core::source_board::read_contexts`); the server validates that context
//! and holds every credential. Only the newest read of a list or an item
//! applies, a failed read is never shown as an empty list, and every write
//! carries a fresh idempotency key for the click that sent it.
//!
//! Not ported: trackers (Linear), discussions, the repository picker of a
//! folder holding several repositories and the list across them, links
//! another surface asks to show, labels and reviewers editing, merge and
//! ready for review, review drafts and threads, check runs, stacks, comment
//! attachments, starting a session or a worktree on an item, the narrow
//! dropdown layout and the list's resize handle.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::{
    AppContext, Context, Entity, ScrollHandle, SharedString, Subscription, Task, WeakEntity, Window,
};
use openchamber_native_core::ids::RuntimeId;
use openchamber_native_core::source_board::{
    BoardList, Detail, Identity, ListFilter, ListQuery, PeopleFilter, ProviderTerms, PullStatus,
    ReadContext, Reference, RepositoryKind, ReviewVerdict, StateFilter, TargetState,
    first_by_order, read_contexts, repository_hosts, terms_of,
};
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::source_control::{
    BindingLookup, SourceControlError, WriteTarget,
};

use crate::files_view::Notice;
use crate::locale;
use crate::network::NetworkTask;
use crate::workspace::Workspace;

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);
const DETAIL_DEBOUNCE: Duration = Duration::from_millis(250);

/// A registered project the board can read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BoardProject {
    pub path: String,
    pub label: SharedString,
}

/// The account and provider the project's items are read with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Reader {
    pub context: ReadContext,
    pub terms: ProviderTerms,
}

/// Where the project's repository source stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProjectSource {
    Resolving,
    NoProject,
    /// No remote on a host a provider serves, or no repository at all.
    NoSource,
    /// The provider serves no board reads.
    Unsupported,
    /// A remote is on a known host, but no account there can read it.
    Unauthenticated {
        provider: String,
        term: String,
    },
    Failed(String),
    Ready(Box<Reader>),
}

/// The previewed item's detail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DetailLoad {
    Idle,
    Loading,
    Ready(Detail),
    Failed(String),
}

/// A write the board is waiting on; its controls stay disabled meanwhile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Busy {
    Comment,
    Review(ReviewVerdict),
    State,
}

/// An open popup of the toolbar or the preview.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Menu {
    Project,
    Filter,
    State,
}

pub(crate) struct SourceBoard {
    owner: WeakEntity<Workspace>,
    /// A test's own server, used in place of the workspace's connection.
    #[cfg(test)]
    fixed: Option<(ServerClient, tokio::runtime::Handle)>,
    runtime: RuntimeId,
    pub(crate) projects: Vec<BoardProject>,
    /// The chosen project's path.
    pub(crate) project: Option<String>,
    pub(crate) source: ProjectSource,
    resolve_task: Option<Task<()>>,
    pub(crate) kind: RepositoryKind,
    filters: HashMap<RepositoryKind, ListFilter>,
    pub(crate) menu: Option<Menu>,
    pub(crate) search: Entity<InputState>,
    search_text: String,
    search_task: Option<Task<()>>,
    pub(crate) list: BoardList,
    list_task: Option<Task<()>>,
    more_task: Option<Task<()>>,
    pub(crate) statuses: Vec<PullStatus>,
    status_task: Option<Task<()>>,
    /// The item the detail belongs to.
    detail_key: Option<String>,
    pub(crate) detail: DetailLoad,
    detail_task: Option<Task<()>>,
    pub(crate) refreshing_detail: bool,
    pub(crate) reply: Entity<TextareaState>,
    /// The item the reply's text was typed for.
    reply_for: Option<String>,
    pub(crate) busy: Option<Busy>,
    write_task: Option<Task<()>>,
    pub(crate) notice: Option<Notice>,
    pub(crate) list_scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl SourceBoard {
    pub(crate) fn new(
        owner: WeakEntity<Workspace>,
        runtime: RuntimeId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx));
        let reply = cx.new(|cx| TextareaState::new(window, cx).auto_grow(3, 10));
        let search_subscription = cx.subscribe_in(&search, window, |this, _, event, _, cx| {
            if let InputEvent::Change = event {
                this.schedule_search(cx);
            }
        });
        let reply_subscription = cx.subscribe_in(&reply, window, |this, _, event, window, cx| {
            if let InputEvent::PressEnter {
                secondary: true, ..
            } = event
            {
                this.comment(window, cx);
            }
        });
        Self {
            owner,
            #[cfg(test)]
            fixed: None,
            runtime,
            projects: Vec::new(),
            project: None,
            source: ProjectSource::Resolving,
            resolve_task: None,
            kind: RepositoryKind::Issue,
            filters: HashMap::new(),
            menu: None,
            search,
            search_text: String::new(),
            search_task: None,
            list: BoardList::default(),
            list_task: None,
            more_task: None,
            statuses: Vec::new(),
            status_task: None,
            detail_key: None,
            detail: DetailLoad::Idle,
            detail_task: None,
            refreshing_detail: false,
            reply,
            reply_for: None,
            busy: None,
            write_task: None,
            notice: None,
            list_scroll: ScrollHandle::new(),
            _subscriptions: vec![search_subscription, reply_subscription],
        }
    }

    #[cfg(test)]
    pub(crate) fn with_network(
        mut self,
        client: ServerClient,
        network: tokio::runtime::Handle,
    ) -> Self {
        self.fixed = Some((client, network));
        self
    }

    pub(crate) fn runtime(&self) -> &RuntimeId {
        &self.runtime
    }

    /// The current connection's client and network, read at request time so
    /// a reconnect is picked up and another runtime is never used.
    fn network(&self, cx: &gpui_kit::App) -> Option<(ServerClient, tokio::runtime::Handle)> {
        let workspace = self.owner.upgrade()?;
        let workspace = workspace.read(cx);
        #[cfg(test)]
        if let Some(fixed) = &self.fixed {
            return Some(fixed.clone());
        }
        if workspace.runtime != self.runtime {
            return None;
        }
        let connection = workspace.connection.as_ref()?;
        Some((connection.client.clone()?, connection.network.handle()))
    }

    /// Takes the registered projects as the board opens: the remembered
    /// project while it still exists, else the active one, else the first.
    pub(crate) fn open(
        &mut self,
        projects: Vec<BoardProject>,
        active: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let unchanged = projects == self.projects;
        self.projects = projects;
        let project = self
            .project
            .clone()
            .filter(|path| self.projects.iter().any(|project| &project.path == path))
            .or_else(|| {
                active.filter(|path| self.projects.iter().any(|project| &project.path == path))
            })
            .or_else(|| self.projects.first().map(|project| project.path.clone()));
        // A project that could not be read yet (signed out, failed) is asked
        // again; a ready board keeps its list and preview as they were left.
        if unchanged && project == self.project && matches!(self.source, ProjectSource::Ready(_)) {
            cx.notify();
            return;
        }
        self.select_project(project, cx);
    }

    pub(crate) fn select_project(&mut self, project: Option<String>, cx: &mut Context<Self>) {
        self.menu = None;
        self.project = project;
        self.list = BoardList::default();
        self.list_task = None;
        self.more_task = None;
        self.statuses.clear();
        self.clear_detail();
        self.notice = None;
        self.source = ProjectSource::Resolving;
        // The workspace that opens the board is still being updated; the
        // connection is read once that update ends.
        let board = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = board.update(cx, |board, cx| board.resolve(cx));
        });
        cx.notify();
    }

    /// Finds which account reads the project: the provider list, the
    /// binding, the known instances and each host's current account.
    pub(crate) fn resolve(&mut self, cx: &mut Context<Self>) {
        let Some(directory) = self.project.clone() else {
            self.source = ProjectSource::NoProject;
            self.resolve_task = None;
            cx.notify();
            return;
        };
        let Some((client, network)) = self.network(cx) else {
            self.source = ProjectSource::Failed(String::new());
            cx.notify();
            return;
        };
        self.source = ProjectSource::Resolving;
        let task = NetworkTask::spawn_on(&network, async move {
            resolve_source(&client, &directory).await
        });
        self.resolve_task = Some(cx.spawn(async move |this, cx| {
            let Some(source) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.resolve_task = None;
                this.source = source;
                if matches!(this.source, ProjectSource::Ready(_)) {
                    this.load_list(cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(crate) fn context(&self) -> Option<(&ReadContext, &ProviderTerms)> {
        match &self.source {
            ProjectSource::Ready(reader) => Some((&reader.context, &reader.terms)),
            _ => None,
        }
    }

    pub(crate) fn filter(&self) -> ListFilter {
        self.filters.get(&self.kind).copied().unwrap_or_default()
    }

    fn query(&self) -> ListQuery {
        ListQuery {
            kind: self.kind,
            filter: self.filter(),
            text: self.search_text.trim().to_owned(),
        }
    }

    pub(crate) fn search_text(&self) -> &str {
        &self.search_text
    }

    pub(crate) fn select_kind(&mut self, kind: RepositoryKind, cx: &mut Context<Self>) {
        if self.kind == kind {
            return;
        }
        self.kind = kind;
        self.menu = None;
        self.list.highlight(None);
        self.load_list(cx);
    }

    pub(crate) fn select_state(&mut self, state: StateFilter, cx: &mut Context<Self>) {
        let filter = ListFilter {
            state,
            ..self.filter()
        };
        self.set_filter(filter, cx);
    }

    pub(crate) fn select_people(&mut self, people: PeopleFilter, cx: &mut Context<Self>) {
        let filter = ListFilter {
            people,
            ..self.filter()
        };
        self.set_filter(filter, cx);
    }

    fn set_filter(&mut self, filter: ListFilter, cx: &mut Context<Self>) {
        self.filters.insert(self.kind, filter);
        self.list.highlight(None);
        self.load_list(cx);
    }

    pub(crate) fn toggle_menu(&mut self, menu: Menu, cx: &mut Context<Self>) {
        self.menu = if self.menu == Some(menu) {
            None
        } else {
            Some(menu)
        };
        cx.notify();
    }

    /// Applies the typed search once typing pauses.
    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        let text = self.search.read(cx).value().to_string();
        if text == self.search_text {
            return;
        }
        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                this.search_task = None;
                this.search_text = text;
                this.list.highlight(None);
                this.load_list(cx);
            });
        }));
    }

    pub(crate) fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.search_task = None;
        if !self.search_text.is_empty() {
            self.search_text.clear();
            self.list.highlight(None);
            self.load_list(cx);
        }
    }

    /// Reads the list's first page; the same query again keeps its rows
    /// until the answer.
    pub(crate) fn load_list(&mut self, cx: &mut Context<Self>) {
        let Some((context, _)) = self.context() else {
            return;
        };
        let context = context.clone();
        let query = self.query();
        let ticket = self.list.begin(query.clone());
        self.more_task = None;
        let Some((client, network)) = self.network(cx) else {
            self.list.complete(ticket, Err(String::new()));
            cx.notify();
            return;
        };
        let task = NetworkTask::spawn_on(&network, async move {
            client
                .source_control_references(&context, &query, None)
                .await
                .map_err(|error| error_text(&error))
        });
        self.list_task = Some(cx.spawn(async move |this, cx| {
            let Some(result) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if this.list.complete(ticket, result) {
                    this.list_task = None;
                    this.after_list(cx);
                }
                cx.notify();
            });
        }));
        self.after_highlight(cx);
        cx.notify();
    }

    /// Reads the next page when the list has one.
    pub(crate) fn load_more(&mut self, cx: &mut Context<Self>) {
        let Some((context, _)) = self.context() else {
            return;
        };
        let context = context.clone();
        let Some((ticket, cursor)) = self.list.begin_more() else {
            return;
        };
        let query = self.query();
        let Some((client, network)) = self.network(cx) else {
            self.list.complete_more(ticket, Err(String::new()));
            return;
        };
        let task = NetworkTask::spawn_on(&network, async move {
            client
                .source_control_references(&context, &query, Some(&cursor))
                .await
                .map_err(|error| error_text(&error))
        });
        self.more_task = Some(cx.spawn(async move |this, cx| {
            let Some(result) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if this.list.complete_more(ticket, result) {
                    this.more_task = None;
                    this.load_statuses(cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn after_list(&mut self, cx: &mut Context<Self>) {
        self.load_statuses(cx);
        self.after_highlight(cx);
    }

    /// Reads the checks and mergeability of the listed open change requests.
    fn load_statuses(&mut self, cx: &mut Context<Self>) {
        let Some((context, _)) = self.context() else {
            return;
        };
        let pulls: Vec<Reference> = self
            .list
            .items()
            .iter()
            .filter(|item| item.open_pull().is_some())
            .cloned()
            .collect();
        if pulls.is_empty() {
            return;
        }
        let context = context.clone();
        let Some((client, network)) = self.network(cx) else {
            return;
        };
        let task = NetworkTask::spawn_on(&network, async move {
            client.source_control_pull_statuses(&context, &pulls).await
        });
        self.status_task = Some(cx.spawn(async move |this, cx| {
            // A status that cannot be read leaves the row in its plain colour.
            let Some(Ok(statuses)) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.status_task = None;
                for status in statuses {
                    this.statuses.retain(|known| {
                        !(known.number == status.number
                            && known.owner.eq_ignore_ascii_case(&status.owner)
                            && known.repo.eq_ignore_ascii_case(&status.repo))
                    });
                    this.statuses.push(status);
                }
                cx.notify();
            });
        }));
    }

    pub(crate) fn status_of(&self, reference: &Reference) -> Option<&PullStatus> {
        self.statuses.iter().find(|status| status.is_for(reference))
    }

    /// The list's refresh: the server forgets its lists first, so a change
    /// made on the host's site shows.
    pub(crate) fn refresh_list(&mut self, cx: &mut Context<Self>) {
        let Some((context, _)) = self.context() else {
            self.resolve(cx);
            return;
        };
        let identity = context.identity.clone();
        self.statuses.clear();
        let Some((client, network)) = self.network(cx) else {
            self.load_list(cx);
            return;
        };
        let forget = NetworkTask::spawn_on(&network, async move {
            client.source_control_refresh_reads(&identity, None).await
        });
        self.list_task = Some(cx.spawn(async move |this, cx| {
            let _ = forget.await;
            let _ = this.update(cx, |this, cx| this.load_list(cx));
        }));
        cx.notify();
    }

    pub(crate) fn highlight(&mut self, key: String, cx: &mut Context<Self>) {
        self.list.highlight(Some(key));
        self.menu = None;
        self.after_highlight(cx);
        cx.notify();
    }

    pub(crate) fn move_highlight(&mut self, forward: bool, cx: &mut Context<Self>) {
        let wants_more = self.list.move_highlight(forward);
        if let Some(index) = self
            .list
            .highlighted_key()
            .and_then(|key| self.list.items().iter().position(|item| item.key() == key))
        {
            self.list_scroll.scroll_to_item(index);
        }
        if wants_more {
            self.load_more(cx);
        }
        self.after_highlight(cx);
        cx.notify();
    }

    fn clear_detail(&mut self) {
        self.detail_key = None;
        self.detail = DetailLoad::Idle;
        self.detail_task = None;
        self.refreshing_detail = false;
    }

    /// Reads the highlighted item's detail once the highlight rests on it.
    fn after_highlight(&mut self, cx: &mut Context<Self>) {
        let key = self.list.highlighted_key();
        if key == self.detail_key {
            return;
        }
        self.menu = self.menu.filter(|menu| *menu != Menu::State);
        self.clear_detail();
        self.detail_key = key;
        if self.detail_key.is_some() {
            self.detail = DetailLoad::Loading;
            self.detail_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(DETAIL_DEBOUNCE).await;
                let _ = this.update(cx, |this, cx| this.read_detail(cx));
            }));
        }
    }

    /// Whether the previewed item changed since the reply was last kept,
    /// so its text is cleared (`SourceBoardReply` is keyed by the item).
    pub(crate) fn take_reply_reset(&mut self) -> bool {
        let key = self.list.highlighted_key();
        if key == self.reply_for {
            return false;
        }
        let had_item = self.reply_for.is_some();
        self.reply_for = key;
        had_item
    }

    pub(crate) fn previewed(&self) -> Option<&Reference> {
        self.list.highlighted()
    }

    fn read_detail(&mut self, cx: &mut Context<Self>) {
        let (Some((context, _)), Some(reference)) = (self.context(), self.previewed()) else {
            return;
        };
        let context = context.clone();
        let reference = reference.clone();
        let key = reference.key();
        let Some((client, network)) = self.network(cx) else {
            self.detail = DetailLoad::Failed(String::new());
            cx.notify();
            return;
        };
        let task = NetworkTask::spawn_on(&network, async move {
            client
                .source_control_reference_detail(&context, &reference)
                .await
        });
        self.detail_task = Some(cx.spawn(async move |this, cx| {
            let Some(result) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if this.detail_key.as_deref() != Some(key.as_str()) {
                    return;
                }
                this.detail_task = None;
                this.refreshing_detail = false;
                this.detail = match result {
                    Ok(Some(detail)) => DetailLoad::Ready(detail),
                    // Signed out since the list was read: the list says so.
                    Ok(None) => {
                        this.load_list(cx);
                        DetailLoad::Failed(String::new())
                    }
                    Err(error) => DetailLoad::Failed(error_text(&error)),
                };
                cx.notify();
            });
        }));
    }

    /// The preview's refresh: the server forgets what it read about the
    /// item, then the item and the list are read again.
    pub(crate) fn refresh_item(&mut self, cx: &mut Context<Self>) {
        let (Some((context, _)), Some(reference)) = (self.context(), self.previewed()) else {
            return;
        };
        let identity = context.identity.clone();
        let target = WriteTarget::of(reference);
        let Some((client, network)) = self.network(cx) else {
            return;
        };
        self.refreshing_detail = true;
        let forget = NetworkTask::spawn_on(&network, async move {
            client
                .source_control_refresh_reads(&identity, Some(&target))
                .await
        });
        self.detail_task = Some(cx.spawn(async move |this, cx| {
            let _ = forget.await;
            let _ = this.update(cx, |this, cx| this.reread_item(cx));
        }));
        cx.notify();
    }

    /// After a write: the item's detail and the list read again.
    fn reread_item(&mut self, cx: &mut Context<Self>) {
        self.read_detail(cx);
        self.load_list(cx);
    }

    /// Posts the reply box's text as a comment.
    pub(crate) fn comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let body = self.reply.read(cx).value().to_string();
        if body.trim().is_empty() || self.busy.is_some() {
            return;
        }
        let Some((context, terms, reference)) = self.write_scope() else {
            return;
        };
        let label = terms.number_label(&reference);
        let kind = reference.kind;
        let target = WriteTarget::of(&reference);
        self.write(
            Busy::Comment,
            window,
            cx,
            async move |client, key| {
                client
                    .source_control_comment(&context, kind, &target, &key, &body)
                    .await
                    .map(|()| true)
            },
            move |this, result, window, cx| {
                let strings = locale::strings(cx).clone();
                match result {
                    Ok(_) => {
                        this.reply
                            .update(cx, |input, cx| input.set_value("", window, cx));
                        this.notice = Some(Notice::info(strings.format(
                            "sourceBoard.reply.toast.commented",
                            &[("reference", &label)],
                        )));
                        this.reread_item(cx);
                    }
                    Err(error) => {
                        this.notice = Some(Notice::error(
                            strings.format(
                                "sourceBoard.reply.toast.commentFailed",
                                &[("reference", &label)],
                            ),
                            error.message(),
                        ));
                    }
                }
            },
        );
    }

    /// Approves the previewed open change request, or requests changes with
    /// the reply's text, for the head the preview shows.
    pub(crate) fn review(
        &mut self,
        verdict: ReviewVerdict,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        let text = self.reply.read(cx).value().to_string();
        let body = (!text.trim().is_empty()).then_some(text);
        if verdict == ReviewVerdict::RequestChanges && body.is_none() {
            return;
        }
        let Some((context, terms, reference)) = self.write_scope() else {
            return;
        };
        let Some(pull) = reference.open_pull() else {
            return;
        };
        let target = WriteTarget {
            head_sha: Some(pull.head_sha.clone()),
            ..WriteTarget::of(&reference)
        };
        let label = terms.number_label(&reference);
        let sent_text = body.is_some();
        self.write(
            Busy::Review(verdict),
            window,
            cx,
            async move |client, key| {
                client
                    .source_control_review(&context, &target, &key, verdict, body.as_deref())
                    .await
                    .map(|outcome| outcome.commented)
            },
            move |this, result, window, cx| {
                let strings = locale::strings(cx).clone();
                let done = match verdict {
                    ReviewVerdict::Approve => "sourceBoard.reply.toast.approved",
                    ReviewVerdict::RequestChanges => "sourceBoard.reply.toast.changesRequested",
                };
                match result {
                    Ok(commented) => {
                        let title = strings.format(done, &[("reference", &label)]);
                        if sent_text && !commented {
                            // The verdict stands; its text stays in the box.
                            this.notice = Some(Notice::error(
                                title,
                                Some(&strings.get("sourceBoard.reply.toast.textNotPosted")),
                            ));
                        } else {
                            this.reply
                                .update(cx, |input, cx| input.set_value("", window, cx));
                            this.notice = Some(Notice::info(title));
                        }
                    }
                    Err(error) => {
                        this.notice = Some(Notice::error(
                            strings.format(
                                "sourceBoard.reply.toast.reviewFailed",
                                &[("reference", &label)],
                            ),
                            error.message(),
                        ));
                    }
                }
                // A refused review is most often a push since the preview was read.
                this.reread_item(cx);
            },
        );
    }

    /// Closes the previewed open issue or change request, or reopens a
    /// closed one.
    pub(crate) fn change_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        if self.busy.is_some() {
            return;
        }
        let Some((context, terms, reference)) = self.write_scope() else {
            return;
        };
        let Some(state) = reference.next_state() else {
            return;
        };
        let label = terms.number_label(&reference);
        let kind = reference.kind;
        let target = WriteTarget::of(&reference);
        self.write(
            Busy::State,
            window,
            cx,
            async move |client, key| {
                client
                    .source_control_set_state(&context, kind, &target, &key, state)
                    .await
                    .map(|_| true)
            },
            move |this, result, _, cx| {
                let strings = locale::strings(cx);
                let (done, failed) = match state {
                    TargetState::Closed => {
                        ("sourceBoard.toast.closed", "sourceBoard.toast.closeFailed")
                    }
                    TargetState::Open => (
                        "sourceBoard.toast.reopened",
                        "sourceBoard.toast.reopenFailed",
                    ),
                };
                this.notice = Some(match result {
                    Ok(_) => Notice::info(strings.format(done, &[("reference", &label)])),
                    Err(error) => Notice::error(
                        strings.format(failed, &[("reference", &label)]),
                        error.message(),
                    ),
                });
                this.reread_item(cx);
            },
        );
    }

    fn write_scope(&self) -> Option<(ReadContext, ProviderTerms, Reference)> {
        let (context, terms) = self.context()?;
        Some((context.clone(), terms.clone(), self.previewed()?.clone()))
    }

    /// Runs one write with a fresh idempotency key and reports its outcome;
    /// a write the board loses track of (cancelled) changes nothing here.
    fn write<F>(
        &mut self,
        busy: Busy,
        window: &mut Window,
        cx: &mut Context<Self>,
        send: impl FnOnce(ServerClient, String) -> F + Send + 'static,
        done: impl FnOnce(&mut Self, Result<bool, SourceControlError>, &mut Window, &mut Context<Self>)
        + 'static,
    ) where
        F: Future<Output = Result<bool, SourceControlError>> + Send + 'static,
    {
        let Some((client, network)) = self.network(cx) else {
            self.notice = Some(Notice::error(
                locale::strings(cx).get("gitView.operation.state.unavailable"),
                None,
            ));
            cx.notify();
            return;
        };
        self.busy = Some(busy);
        self.notice = None;
        let key = idempotency_key();
        let task = NetworkTask::spawn_on(&network, send(client, key));
        self.write_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                this.write_task = None;
                if let Some(result) = result {
                    done(this, result, window, cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
}

impl Workspace {
    /// The sidebar's Issues and PRs button opens the board and closes it
    /// again; opening it leaves the archive page.
    pub(crate) fn toggle_source_board(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.navigation_revision = self.navigation_revision.wrapping_add(1);
        self.source_board_open = !self.source_board_open;
        if !self.source_board_open {
            self.focus_composer(window, cx);
            cx.notify();
            return;
        }
        self.archive_open = false;
        if self
            .source_board
            .as_ref()
            .is_some_and(|board| board.read(cx).runtime() != &self.runtime)
        {
            self.source_board = None;
        }
        let runtime = self.runtime.clone();
        let owner = cx.entity().downgrade();
        let board = self
            .source_board
            .get_or_insert_with(|| cx.new(|cx| SourceBoard::new(owner, runtime, window, cx)))
            .clone();
        let projects = self.board_projects();
        let active = self
            .send_catalog
            .settings
            .as_ref()
            .and_then(|settings| settings.active_project_path())
            .map(str::to_owned);
        board.update(cx, |board, cx| board.open(projects, active, cx));
        let search = board.read(cx).search.clone();
        search.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// The registered projects with their labels.
    fn board_projects(&self) -> Vec<BoardProject> {
        self.send_catalog
            .settings
            .as_ref()
            .map(|settings| {
                settings
                    .projects
                    .iter()
                    .map(|project| BoardProject {
                        path: project.path.clone(),
                        label: self.project_label(&project.path),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The open board, only for the runtime it reads: after a runtime switch
    /// the chat shows until the button opens a board for the new one.
    pub(crate) fn render_source_board(&self, cx: &gpui_kit::App) -> Option<gpui_kit::AnyElement> {
        use gpui_kit::IntoElement;
        let board = self
            .source_board
            .clone()
            .filter(|board| board.read(cx).runtime() == &self.runtime)?;
        Some(board.into_any_element())
    }
}

/// The server's explanation, else the transport's.
pub(crate) fn error_text(error: &SourceControlError) -> String {
    error
        .message()
        .map(str::to_owned)
        .unwrap_or_else(|| error.to_string())
}

/// A fresh key per user action: a new click never reuses one.
fn idempotency_key() -> String {
    let seed = std::collections::hash_map::RandomState::new();
    let mut first = seed.build_hasher();
    first.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default(),
    );
    let mut second = seed.build_hasher();
    second.write_u64(first.finish());
    format!("native-{:016x}{:016x}", first.finish(), second.finish())
}

/// Resolves the project's source as `useRepositoryReadContext` and
/// `resolveBoardTab` do for its repository.
async fn resolve_source(client: &ServerClient, directory: &str) -> ProjectSource {
    let failed = |error: SourceControlError| ProjectSource::Failed(error_text(&error));
    let providers = match client.source_control_providers().await {
        Ok(providers) => providers,
        Err(error) => return failed(error),
    };
    let read = match client.source_control_binding(directory).await {
        Ok(BindingLookup::Repository(read)) => read,
        Ok(BindingLookup::NotRepository) => return ProjectSource::NoSource,
        Err(error) => return failed(error),
    };
    let known = match client.source_control_instances().await {
        Ok(known) => known,
        Err(error) => return failed(error),
    };
    let mut hosts = repository_hosts(&read, &known);
    if hosts.is_empty() {
        return ProjectSource::NoSource;
    }
    let mut accounts: Vec<(Identity, Option<String>)> = Vec::new();
    for host in &hosts {
        match client.source_control_auth_status(host).await {
            Ok(status) => accounts.push((host.clone(), status.current_account)),
            Err(error) => return failed(error),
        }
    }
    let contexts = read_contexts(&read, directory, &known, |identity| {
        accounts
            .iter()
            .find(|(host, _)| host == identity)
            .and_then(|(_, account)| account.clone())
    });
    if let Some(context) = first_by_order(&contexts, &providers) {
        let Some(terms) = terms_of(&providers, &context.identity.provider) else {
            return ProjectSource::Unsupported;
        };
        if !terms.reference_search {
            return ProjectSource::Unsupported;
        }
        return ProjectSource::Ready(Box::new(Reader {
            context: context.clone(),
            terms: terms.clone(),
        }));
    }
    // The host the provider list ranks first names the sign-in to make.
    hosts.sort_by_key(|host| {
        providers
            .iter()
            .position(|terms| terms.id == host.provider)
            .unwrap_or(usize::MAX)
    });
    let host = &hosts[0];
    match terms_of(&providers, &host.provider) {
        Some(terms) => ProjectSource::Unauthenticated {
            provider: terms.label.clone(),
            term: terms.change_request.clone(),
        },
        None => ProjectSource::Unsupported,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_keys_differ_per_action() {
        let first = idempotency_key();
        let second = idempotency_key();
        assert_ne!(first, second);
        assert!(first.starts_with("native-"));
        assert_eq!(first.len(), "native-".len() + 32);
    }
}
