//! The composer's autocomplete: which picker is open, the catalogs it lists,
//! the `@` search, keyboard and pointer selection, and the edit a choice
//! makes (`ComposerAutocompletePopups.tsx` and the pickers it opens).
//!
//! A choice edits the textarea as typing does, as one undoable replacement,
//! so the draft and every send path see ordinary composer text: a mention,
//! skill or snippet token is prepared at send exactly as one typed by hand.

use std::collections::HashMap;
use std::ops::Range;
use std::time::{Duration, Instant};

use gpui_kit::component::input::CommittedInputEdit;
use gpui_kit::{App, Context, ScrollHandle, Task, Window};
use openchamber_native_core::autocomplete::{
    self, AutocompleteKind, CommandAvailability, CommandItem, FileRow, InputSource, Insertion,
    ServerCommand, SkillItem, SnippetItem, TriggerContext,
};
use openchamber_native_core::ids::RuntimeId;
use openchamber_native_core::search_rank;
use openchamber_native_core::send_config::AgentEntry;
use openchamber_native_core::session::SessionRecord;
use openchamber_native_core::session_reference;
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::composer_preparation::ComposerPreparation;

use crate::connection::ConnectionState;
use crate::locale;
use crate::network::NetworkTask;
use crate::workspace::Workspace;

/// `useDebouncedValue(searchQuery, 180)`.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(180);
/// A catalog read this recently is not read again when a picker opens.
const CATALOG_FRESH: Duration = Duration::from_secs(5);
/// Recent files `@` lists at most.
const RECENT_FILE_LIMIT: usize = 6;

type CatalogKey = (RuntimeId, String);

/// One catalog of one directory.
struct Catalog<T> {
    items: Option<T>,
    loaded_at: Option<Instant>,
    read: Option<Task<()>>,
}

impl<T> Default for Catalog<T> {
    fn default() -> Self {
        Self {
            items: None,
            loaded_at: None,
            read: None,
        }
    }
}

impl<T> Catalog<T> {
    fn wants_read(&self) -> bool {
        self.read.is_none()
            && self
                .loaded_at
                .is_none_or(|at| at.elapsed() >= CATALOG_FRESH)
    }

    fn loading(&self) -> bool {
        self.items.is_none() && self.read.is_some()
    }
}

/// The picker on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OpenPicker {
    pub(crate) kind: AutocompleteKind,
    pub(crate) query: String,
    pub(crate) selected: usize,
}

/// A mention row's identity, which keeps the selection on it when rows change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MentionRowId {
    Agent(String),
    File(String),
    Session(String),
}

/// A session `@` offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionRow {
    pub(crate) id: String,
    pub(crate) title: String,
    /// The project's folder name, and whether the session is archived.
    pub(crate) project: Option<String>,
    pub(crate) archived: bool,
}

/// The rows the `@` list shows, top to bottom, and the search they answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MentionView {
    key: Option<String>,
    pub(crate) agents: Vec<AgentEntry>,
    pub(crate) more_agents: bool,
    pub(crate) recent: Vec<FileRow>,
    pub(crate) results: Vec<FileRow>,
    pub(crate) sessions: Vec<SessionRow>,
}

impl MentionView {
    pub(crate) fn ids(&self) -> Vec<MentionRowId> {
        self.agents
            .iter()
            .map(|agent| MentionRowId::Agent(agent.name.clone()))
            .chain(
                self.recent
                    .iter()
                    .chain(&self.results)
                    .map(|row| MentionRowId::File(row.path.clone())),
            )
            .chain(
                self.sessions
                    .iter()
                    .map(|session| MentionRowId::Session(session.id.clone())),
            )
            .collect()
    }

    pub(crate) fn len(&self) -> usize {
        self.agents.len() + self.recent.len() + self.results.len() + self.sessions.len()
    }
}

/// A settled `@` search: the server's hits for one key.
struct MentionSearch {
    key: String,
    files: Vec<FileRow>,
    folders: Vec<FileRow>,
}

/// What the open picker lists.
pub(crate) enum PickerRows {
    Commands(Vec<CommandItem>),
    Skills { rows: Vec<SkillItem>, loading: bool },
    Snippets(Vec<SnippetItem>),
    Mentions { view: MentionView, pending: bool },
}

impl PickerRows {
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Commands(rows) => rows.len(),
            Self::Skills { rows, .. } => rows.len(),
            Self::Snippets(rows) => rows.len(),
            Self::Mentions { view, .. } => view.len(),
        }
    }
}

/// The keys an open picker takes from the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PickerKey {
    Up,
    Down,
    /// Enter or Tab.
    Choose,
    Escape,
}

/// The composer's autocomplete state, one per window.
#[derive(Default)]
pub(crate) struct ComposerAutocomplete {
    pub(crate) open: Option<OpenPicker>,
    /// Mentions a choice completed in the current text: the `@` through the
    /// name, never the space after it.
    completed: Vec<Range<usize>>,
    /// The mention the next committed edit writes.
    pending_completion: Option<Range<usize>>,
    /// The next committed edit is a text paste.
    paste: bool,
    commands: HashMap<CatalogKey, Catalog<Vec<ServerCommand>>>,
    skills: HashMap<CatalogKey, Catalog<Vec<SkillItem>>>,
    snippets: HashMap<CatalogKey, Catalog<Vec<SnippetItem>>>,
    search: Option<MentionSearch>,
    search_task: Option<(String, Task<()>)>,
    /// The `@` rows last shown, held while the next query searches.
    held: MentionView,
    pub(crate) scroll: ScrollHandle,
}

impl ComposerAutocomplete {
    /// Marks the next committed edit as a paste of text.
    pub(crate) fn note_paste(&mut self) {
        self.paste = true;
    }

    /// Keeps completed mentions in step with an edit: one an edit touches
    /// becomes an unfinished query again; the rest move with the text.
    fn map_completed(&mut self, edit: &CommittedInputEdit) {
        let mut mentions = std::mem::take(&mut self.completed);
        for replacement in &edit.replacements {
            let range = &replacement.range;
            let delta = replacement.text.len() as isize - range.len() as isize;
            mentions.retain_mut(|mention| {
                if range.start <= mention.end && range.end > mention.start {
                    return false;
                }
                if range.end <= mention.start {
                    mention.start = mention.start.saturating_add_signed(delta);
                    mention.end = mention.end.saturating_add_signed(delta);
                }
                true
            });
        }
        mentions.extend(self.pending_completion.take());
        self.completed = mentions;
    }
}

impl Workspace {
    pub(crate) fn close_autocomplete(&mut self, cx: &mut Context<Self>) {
        if self.autocomplete.open.take().is_some() {
            self.autocomplete.search_task = None;
            cx.notify();
        }
    }

    /// Forgets the completed mentions of a composer the text left.
    pub(crate) fn reset_autocomplete(&mut self, cx: &mut Context<Self>) {
        self.autocomplete.completed.clear();
        self.autocomplete.pending_completion = None;
        self.autocomplete.paste = false;
        self.close_autocomplete(cx);
    }

    /// After a committed edit: what the caret asks for now
    /// (`updateAutocompleteState`). IME preedit never reaches here.
    pub(crate) fn autocomplete_after_edit(
        &mut self,
        edit: &CommittedInputEdit,
        cx: &mut Context<Self>,
    ) {
        self.autocomplete.map_completed(edit);
        let source = if std::mem::take(&mut self.autocomplete.paste) {
            InputSource::Paste
        } else {
            InputSource::Typed
        };
        let inserted: String = edit
            .replacements
            .iter()
            .map(|replacement| replacement.text.as_ref())
            .collect();
        let text = edit.text.to_string();
        self.update_autocomplete(&text, edit.selection.start, source, &inserted, cx);
    }

    fn update_autocomplete(
        &mut self,
        text: &str,
        cursor: usize,
        source: InputSource,
        inserted: &str,
        cx: &mut Context<Self>,
    ) {
        let trigger = {
            let current = self.selected.as_ref().map(|id| id.as_str());
            let completed = &self.autocomplete.completed;
            // Asked only while an `@` query holds a space.
            let continues = |query: &str| {
                self.sessions_listable()
                    && autocomplete::has_session_mention(
                        &self.mentionable_sessions(),
                        query,
                        current,
                    )
            };
            let is_completed = |start: usize| {
                completed
                    .iter()
                    .any(|mention| mention.start <= start && start < mention.end)
            };
            autocomplete::resolve_trigger(
                text,
                cursor,
                &TriggerContext {
                    mentions_enabled: !self.btw_takes_composer(),
                    source,
                    inserted: Some(inserted),
                    continues_past_space: &continues,
                    is_completed: &is_completed,
                },
            )
        };
        let Some(trigger) = trigger else {
            self.close_autocomplete(cx);
            return;
        };
        let previous = self.autocomplete.open.take();
        let same_kind = previous
            .as_ref()
            .is_some_and(|open| open.kind == trigger.kind);
        // A new list starts at its first row. The `@` list keeps the row the
        // keyboard is on while its rows are held for the next answer.
        let selected = match previous {
            Some(open)
                if same_kind
                    && (open.query == trigger.query
                        || trigger.kind == AutocompleteKind::Mention) =>
            {
                open.selected
            }
            _ => 0,
        };
        if !same_kind {
            self.autocomplete.held = MentionView::default();
        }
        let kind = trigger.kind;
        let query = trigger.query;
        match kind {
            AutocompleteKind::Command => self.load_commands(cx),
            AutocompleteKind::Skill => self.load_skills(cx),
            AutocompleteKind::Snippet => self.load_snippets(cx),
            AutocompleteKind::Mention => self.search_mentions(&query, cx),
        }
        self.autocomplete.open = Some(OpenPicker {
            kind,
            query,
            selected,
        });
        self.refresh_mention_view(cx);
        cx.notify();
    }

    /// Settles the `@` rows: while a query searches the last rows stay;
    /// a new answer starts at its best match, and new rows for the same
    /// search keep the row the user is on.
    fn refresh_mention_view(&mut self, cx: &App) {
        let Some(open) = self
            .autocomplete
            .open
            .as_ref()
            .filter(|open| open.kind == AutocompleteKind::Mention)
        else {
            return;
        };
        if self.mention_pending(&open.query) {
            return;
        }
        let live = self.mention_view(&open.query, cx);
        if live == self.autocomplete.held {
            return;
        }
        let held = std::mem::replace(&mut self.autocomplete.held, live);
        let next = &self.autocomplete.held;
        let same_search = held.key.is_some() && held.key == next.key;
        let selected =
            autocomplete::carried_selection(&held.ids(), open.selected, &next.ids(), same_search);
        if let Some(open) = self.autocomplete.open.as_mut() {
            open.selected = selected;
        }
    }

    /// Whether the query's search has not answered yet, debounce included.
    /// Without a server nothing searches, so nothing is pending.
    fn mention_pending(&self, query: &str) -> bool {
        let wanted = self.mention_search_key(query);
        wanted.is_some()
            && self.autocomplete_client().is_some()
            && self.autocomplete.search.as_ref().map(|search| &search.key) != wanted.as_ref()
    }

    /// A catalog arrived: a list built from it starts at its first row.
    fn catalog_arrived(&mut self, kind: AutocompleteKind, cx: &mut Context<Self>) {
        if let Some(open) = self.autocomplete.open.as_mut()
            && open.kind == kind
        {
            open.selected = 0;
        }
        cx.notify();
    }

    /// The live server connection the pickers read through.
    fn autocomplete_client(&self) -> Option<(ServerClient, tokio::runtime::Handle)> {
        let connection = self
            .connection
            .as_ref()
            .filter(|connection| matches!(connection.state, ConnectionState::Connected))?;
        Some((connection.client.clone()?, connection.network.handle()))
    }

    fn catalog_key(&self) -> Option<CatalogKey> {
        Some((self.runtime.clone(), self.config_directory()?))
    }

    fn load_commands(&mut self, cx: &mut Context<Self>) {
        let (Some(key), Some((client, network))) = (self.catalog_key(), self.autocomplete_client())
        else {
            return;
        };
        let catalog = self.autocomplete.commands.entry(key.clone()).or_default();
        if !catalog.wants_read() {
            return;
        }
        let directory = key.1.clone();
        catalog.read = Some(cx.spawn(async move |this, cx| {
            let result = NetworkTask::spawn_on(&network, async move {
                client.composer_commands(&directory).await
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                let catalog = this.autocomplete.commands.entry(key).or_default();
                catalog.read = None;
                if let Some(Ok(commands)) = result {
                    catalog.items = Some(commands);
                    catalog.loaded_at = Some(Instant::now());
                }
                this.catalog_arrived(AutocompleteKind::Command, cx);
            });
        }));
    }

    fn load_skills(&mut self, cx: &mut Context<Self>) {
        let (Some(key), Some((client, network))) = (self.catalog_key(), self.autocomplete_client())
        else {
            return;
        };
        let catalog = self.autocomplete.skills.entry(key.clone()).or_default();
        if !catalog.wants_read() {
            return;
        }
        let Ok(preparation) = ComposerPreparation::new(&client, &key.1) else {
            return;
        };
        catalog.read = Some(cx.spawn(async move |this, cx| {
            let result =
                NetworkTask::spawn_on(&network, async move { preparation.skill_choices().await })
                    .await;
            let _ = this.update(cx, |this, cx| {
                let catalog = this.autocomplete.skills.entry(key).or_default();
                catalog.read = None;
                if let Some(Ok(skills)) = result {
                    catalog.items = Some(
                        skills
                            .into_iter()
                            .map(|skill| SkillItem {
                                name: skill.name,
                                project: skill.project,
                                description: skill.description,
                            })
                            .collect(),
                    );
                    catalog.loaded_at = Some(Instant::now());
                } else if catalog.items.is_none() {
                    // An empty list after a failed read reads "no skills".
                    catalog.items = Some(Vec::new());
                }
                this.catalog_arrived(AutocompleteKind::Skill, cx);
            });
        }));
    }

    fn load_snippets(&mut self, cx: &mut Context<Self>) {
        let (Some(key), Some((client, network))) = (self.catalog_key(), self.autocomplete_client())
        else {
            return;
        };
        let catalog = self.autocomplete.snippets.entry(key.clone()).or_default();
        if !catalog.wants_read() {
            return;
        }
        let directory = key.1.clone();
        catalog.read = Some(cx.spawn(async move |this, cx| {
            let result = NetworkTask::spawn_on(&network, async move {
                client.composer_snippets(&directory).await
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                let catalog = this.autocomplete.snippets.entry(key).or_default();
                catalog.read = None;
                if let Some(Ok(snippets)) = result {
                    catalog.items = Some(snippets);
                    catalog.loaded_at = Some(Instant::now());
                }
                this.catalog_arrived(AutocompleteKind::Snippet, cx);
            });
        }));
    }

    /// The directory the `@` search and the recent files belong to: the
    /// composer's own.
    fn mention_directory(&self) -> Option<String> {
        self.composer_key
            .as_ref()
            .map(|key| key.directory().to_owned())
            .or_else(|| self.config_directory())
    }

    fn mention_search_key(&self, query: &str) -> Option<String> {
        if !autocomplete::mention_searches(query) {
            return None;
        }
        let directory = self.mention_directory()?;
        Some(format!(
            "{}\u{0}{directory}\u{0}{}",
            self.runtime.as_str(),
            autocomplete::mention_server_query(query)
        ))
    }

    /// Searches files and folders for `query` after the debounce. A failed
    /// kind lists nothing without taking the other kind's hits with it.
    fn search_mentions(&mut self, query: &str, cx: &mut Context<Self>) {
        let Some(key) = self.mention_search_key(query) else {
            self.autocomplete.search_task = None;
            return;
        };
        if self
            .autocomplete
            .search
            .as_ref()
            .is_some_and(|search| search.key == key)
            || self
                .autocomplete
                .search_task
                .as_ref()
                .is_some_and(|(pending, _)| *pending == key)
        {
            return;
        }
        let (Some(directory), Some((client, network))) =
            (self.mention_directory(), self.autocomplete_client())
        else {
            return;
        };
        let server_query = autocomplete::mention_server_query(query);
        let task_key = key.clone();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let result = NetworkTask::spawn_on(&network, async move {
                let (files, folders) = tokio::join!(
                    client.find_files(&directory, &server_query, autocomplete::FILE_SEARCH_LIMIT),
                    client.find_folders(
                        &directory,
                        &server_query,
                        autocomplete::FOLDER_SEARCH_LIMIT
                    ),
                );
                let rows = |hits: Result<Vec<_>, _>, folder: bool| -> Vec<FileRow> {
                    hits.unwrap_or_default()
                        .into_iter()
                        .map(
                            |hit: openchamber_native_transport::files::FileHit| FileRow {
                                path: hit.path,
                                relative_path: hit.relative_path,
                                directory: folder,
                            },
                        )
                        .collect()
                };
                (rows(files, false), rows(folders, true))
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .autocomplete
                    .search_task
                    .as_ref()
                    .is_none_or(|(pending, _)| *pending != task_key)
                {
                    return;
                }
                this.autocomplete.search_task = None;
                let (files, folders) = result.unwrap_or_default();
                this.autocomplete.search = Some(MentionSearch {
                    key: task_key,
                    files,
                    folders,
                });
                this.refresh_mention_view(cx);
                cx.notify();
            });
        });
        self.autocomplete.search_task = Some((key, task));
    }

    /// Whether the session list is whole: a server's after its first
    /// complete load, synthetic data always.
    fn sessions_listable(&self) -> bool {
        self.sessions.has_loaded() || self.is_synthetic()
    }

    /// Sessions `@` can name: every loaded session, active and archived.
    fn mentionable_sessions(&self) -> Vec<&SessionRecord> {
        self.sessions
            .active()
            .iter()
            .chain(self.sessions.archived())
            .collect()
    }

    /// The registered project holding the composer's directory, else that
    /// directory: `@` lists its sessions first.
    fn mention_project_root(&self) -> Option<String> {
        let directory = self.mention_directory()?;
        self.topology
            .projects
            .iter()
            .map(|project| project.path.trim_end_matches('/'))
            .filter(|root| {
                directory == *root
                    || directory
                        .strip_prefix(root)
                        .is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|root| root.len())
            .map(str::to_owned)
            .or(Some(directory))
    }

    fn mention_view(&self, query: &str, cx: &App) -> MentionView {
        let sessions_only = query.contains(' ');
        let normalized = query.trim();
        let agents = self
            .config_directory()
            .and_then(|directory| self.send_catalog.directory(&directory))
            .and_then(|catalog| catalog.agents.ready())
            .map(|agents| autocomplete::rank_mention_agents(agents, query))
            .unwrap_or_default();
        let recent: Vec<FileRow> = if sessions_only {
            Vec::new()
        } else {
            self.mention_directory()
                .and_then(|directory| {
                    let path = self.open_file_path(&directory, cx)?;
                    let root = directory.trim_end_matches('/');
                    let relative = path
                        .strip_prefix(root)
                        .and_then(|rest| rest.strip_prefix('/'))
                        .unwrap_or(&path)
                        .to_owned();
                    search_rank::matches_rank_query([Some(relative.as_str())], normalized)
                        .then_some(FileRow {
                            path,
                            relative_path: relative,
                            directory: false,
                        })
                })
                .into_iter()
                .take(RECENT_FILE_LIMIT)
                .collect()
        };
        let key = self.mention_search_key(query);
        let settled = self
            .autocomplete
            .search
            .as_ref()
            .filter(|search| key.as_ref() == Some(&search.key));
        let results = settled
            .map(|search| {
                let files: Vec<FileRow> = search
                    .files
                    .iter()
                    .filter(|hit| !recent.iter().any(|row| row.path == hit.path))
                    .cloned()
                    .collect();
                autocomplete::rank_file_results(&files, &search.folders, normalized)
            })
            .unwrap_or_default();
        let current = self.selected.as_ref().map(|id| id.as_str());
        let sessions = if self.sessions_listable() {
            autocomplete::rank_session_mentions(
                &self.mentionable_sessions(),
                normalized,
                current,
                self.mention_project_root().as_deref(),
            )
            .into_iter()
            .map(|session| SessionRow {
                id: session.id.as_str().to_owned(),
                title: session.title.clone(),
                project: session
                    .directory
                    .as_deref()
                    .and_then(|directory| directory.rsplit('/').find(|part| !part.is_empty()))
                    .map(str::to_owned),
                archived: session.time.archived.is_some(),
            })
            .collect()
        } else {
            Vec::new()
        };
        MentionView {
            key: settled.map(|search| search.key.clone()),
            agents: agents.0,
            more_agents: agents.1,
            recent,
            results,
            sessions,
        }
    }

    /// The rows the open picker lists now.
    pub(crate) fn picker_rows(&self, cx: &App) -> PickerRows {
        let Some(open) = &self.autocomplete.open else {
            return PickerRows::Commands(Vec::new());
        };
        let key = self.catalog_key();
        match open.kind {
            AutocompleteKind::Command => {
                let strings = locale::strings(cx);
                let has_session = self.selected.is_some();
                let built_ins = autocomplete::builtin_commands(CommandAvailability {
                    has_session,
                    can_start_session: has_session || self.composer_key.is_some(),
                    review_handoff: has_session,
                })
                .into_iter()
                .map(|(name, description, open_chamber)| CommandItem {
                    name: name.to_owned(),
                    description: Some(strings.get(description).to_string()),
                    search_aliases: Vec::new(),
                    built_in: !open_chamber,
                    open_chamber,
                    scope: None,
                })
                .collect();
                let server = key
                    .and_then(|key| self.autocomplete.commands.get(&key))
                    .and_then(|catalog| catalog.items.as_ref())
                    .map(|commands| {
                        commands
                            .iter()
                            .filter(|command| autocomplete::offered_in_composer(command))
                            .map(|command| CommandItem {
                                name: command.name.clone(),
                                description: command.description.clone(),
                                search_aliases: Vec::new(),
                                built_in: command.name == "init" || command.name == "review",
                                open_chamber: false,
                                scope: command.scope,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let merged = autocomplete::merge_commands(built_ins, server);
                PickerRows::Commands(autocomplete::rank_commands(&merged, &open.query))
            }
            AutocompleteKind::Skill => {
                let catalog = key.and_then(|key| self.autocomplete.skills.get(&key));
                let rows = catalog
                    .and_then(|catalog| catalog.items.as_ref())
                    .map(|skills| autocomplete::rank_skills(skills, &open.query))
                    .unwrap_or_default();
                // An empty list before the first read is not "no skills".
                let loading =
                    catalog.is_none_or(Catalog::loading) && self.autocomplete_client().is_some();
                PickerRows::Skills { rows, loading }
            }
            AutocompleteKind::Snippet => PickerRows::Snippets(
                key.and_then(|key| self.autocomplete.snippets.get(&key))
                    .and_then(|catalog| catalog.items.as_ref())
                    .map(|snippets| autocomplete::filter_snippets(snippets, &open.query))
                    .unwrap_or_default(),
            ),
            AutocompleteKind::Mention => {
                let pending = self.mention_pending(&open.query);
                let view = if pending {
                    self.autocomplete.held.clone()
                } else {
                    self.mention_view(&open.query, cx)
                };
                PickerRows::Mentions { view, pending }
            }
        }
    }

    /// Up, Down, Enter/Tab and Escape while a picker is open. False when the
    /// key belongs to the editor.
    pub(crate) fn autocomplete_key(
        &mut self,
        key: PickerKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(open) = self.autocomplete.open.clone() else {
            return false;
        };
        if self.composer.read(cx).is_composing() {
            return false;
        }
        if key == PickerKey::Escape {
            self.close_autocomplete(cx);
            return true;
        }
        let total = self.picker_rows(cx).len();
        if total == 0 {
            return true;
        }
        match key {
            PickerKey::Up | PickerKey::Down => {
                let selected = autocomplete::step(open.selected, total, key == PickerKey::Down);
                self.select_autocomplete_row(selected, cx);
                self.autocomplete.scroll.scroll_to_item(selected);
            }
            PickerKey::Choose => self.choose_autocomplete_row(open.selected % total, window, cx),
            PickerKey::Escape => {}
        }
        true
    }

    /// The pointer over a row selects it.
    pub(crate) fn select_autocomplete_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(open) = self.autocomplete.open.as_mut()
            && open.selected != index
        {
            open.selected = index;
            cx.notify();
        }
    }

    /// The row's choice: the edit it makes, then focus back in the editor.
    pub(crate) fn choose_autocomplete_row(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.autocomplete.open.clone() else {
            return;
        };
        let rows = self.picker_rows(cx);
        let (text, cursor) = {
            let composer = self.composer.read(cx);
            (
                composer.value().to_string(),
                composer.selected_range().start,
            )
        };
        let choice = match rows {
            PickerRows::Commands(commands) => commands.get(index).map(|command| {
                if command.name == "btw" && self.selected.is_some() {
                    Choice::SubmitBtw
                } else {
                    Choice::Edit(autocomplete::command_insertion(&text, &command.name), false)
                }
            }),
            PickerRows::Skills { rows, .. } => rows
                .get(index)
                .and_then(|skill| autocomplete::skill_insertion(&text, cursor, &skill.name))
                .map(|insertion| Choice::Edit(insertion, false)),
            PickerRows::Snippets(rows) => rows.get(index).map(|snippet| {
                Choice::Edit(
                    autocomplete::snippet_insertion(
                        &text,
                        cursor,
                        &snippet.trigger(open.query.trim()),
                    ),
                    false,
                )
            }),
            PickerRows::Mentions { view, .. } => {
                let agents = view.agents.len();
                let files = view.recent.len() + view.results.len();
                if let Some(agent) = view.agents.get(index) {
                    Some(Choice::Edit(
                        autocomplete::mention_insertion(&text, cursor, &format!("@{}", agent.name)),
                        true,
                    ))
                } else if let Some(file) =
                    view.recent.iter().chain(&view.results).nth(index - agents)
                {
                    Some(Choice::Edit(
                        autocomplete::mention_insertion(
                            &text,
                            cursor,
                            &format!("@{}", file.mention_path()),
                        ),
                        true,
                    ))
                } else {
                    view.sessions.get(index - agents - files).map(|session| {
                        self.remember_session_reference(session_reference::session_reference(
                            &session.id,
                            &session.title,
                        ));
                        Choice::Edit(
                            autocomplete::mention_insertion(
                                &text,
                                cursor,
                                &session_reference::session_link_text(&session.id),
                            ),
                            false,
                        )
                    })
                }
            }
        };
        self.close_autocomplete(cx);
        match choice {
            Some(Choice::Edit(insertion, mention)) => {
                self.apply_insertion(insertion, mention, window, cx)
            }
            Some(Choice::SubmitBtw) => {
                self.composer.update(cx, |composer, cx| {
                    composer.set_value("/btw", window, cx);
                });
                self.save_composer_draft(cx);
                self.submit(window, cx);
            }
            None => self.focus_composer(window, cx),
        }
    }

    fn apply_insertion(
        &mut self,
        insertion: Insertion,
        mention: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Insertion { range, text, caret } = insertion;
        if mention {
            // The `@` through the name; the trailing space stays outside.
            self.autocomplete.pending_completion = Some(range.start..range.start + text.len() - 1);
        }
        self.composer.update(cx, |composer, cx| {
            composer.set_selected_range(range, cx);
            composer.replace(text, window, cx);
            composer.set_selected_range(caret..caret, cx);
            composer.focus(window, cx);
        });
    }
}

enum Choice {
    Edit(Insertion, bool),
    /// `/btw` with a session open sends at once.
    SubmitBtw,
}

#[cfg(test)]
#[path = "composer_autocomplete_tests.rs"]
mod tests;
