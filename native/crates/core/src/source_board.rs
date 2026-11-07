//! The issues and pull requests board's model: what a repository's items
//! are, which account reads them, and the searched list with its highlighted
//! row.
//!
//! Ports the repository source of `components/references`
//! (`referencePickerItems.ts`, `useReferenceBrowser.ts`, `referenceCache.ts`)
//! and the read-context choice of `lib/source-control/identity.ts`
//! (`getBoundSourceControlReadContexts`, `resolveSourceControlTarget`). The
//! server's answers are the authority: a failed read is never an empty list,
//! and only the newest read of a query applies.

/// What the board lists: a repository's issues or its change requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RepositoryKind {
    #[default]
    Issue,
    Pull,
}

impl RepositoryKind {
    /// The wire name (`kind=issue|pull`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::Pull => "pull",
        }
    }

    /// The filter states this kind offers, in menu order.
    pub fn states(self) -> &'static [StateFilter] {
        match self {
            Self::Issue => &[StateFilter::Open, StateFilter::Closed, StateFilter::All],
            Self::Pull => &[
                StateFilter::Open,
                StateFilter::Merged,
                StateFilter::Closed,
                StateFilter::All,
            ],
        }
    }

    /// Whose items this kind can be narrowed to, in menu order.
    pub fn people(self) -> &'static [PeopleFilter] {
        match self {
            Self::Issue => &[
                PeopleFilter::Any,
                PeopleFilter::Assigned,
                PeopleFilter::Created,
            ],
            Self::Pull => &[
                PeopleFilter::Any,
                PeopleFilter::Assigned,
                PeopleFilter::Created,
                PeopleFilter::ReviewRequested,
            ],
        }
    }
}

/// Which items by state a list shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum StateFilter {
    #[default]
    Open,
    Closed,
    Merged,
    All,
}

impl StateFilter {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Merged => "merged",
            Self::All => "all",
        }
    }

    /// `REPOSITORY_STATE_LABEL_KEYS`.
    pub fn label_key(self) -> &'static str {
        match self {
            Self::Open => "references.picker.filter.open",
            Self::Closed => "references.picker.filter.state.closed",
            Self::Merged => "references.picker.filter.state.merged",
            Self::All => "references.picker.filter.state.all",
        }
    }
}

/// Whose items a list shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PeopleFilter {
    #[default]
    Any,
    Assigned,
    Created,
    ReviewRequested,
}

impl PeopleFilter {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Assigned => "assigned",
            Self::Created => "created",
            Self::ReviewRequested => "reviewRequested",
        }
    }

    /// `REPOSITORY_PEOPLE_LABEL_KEYS`.
    pub fn label_key(self) -> &'static str {
        match self {
            Self::Any => "references.picker.filter.tracker.people.any",
            Self::Assigned => "references.picker.filter.assigned",
            Self::Created => "references.picker.filter.created",
            Self::ReviewRequested => "references.picker.filter.reviewRequested",
        }
    }
}

/// A list's state and people filter; open items by anyone by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ListFilter {
    pub state: StateFilter,
    pub people: PeopleFilter,
}

/// Everything a list read asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ListQuery {
    pub kind: RepositoryKind,
    pub filter: ListFilter,
    /// The search text, trimmed; empty lists without searching.
    pub text: String,
}

/// An issue's or change request's state as the host reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemState {
    Open,
    /// An issue closed as done.
    Completed,
    /// An issue closed as not planned.
    NotPlanned,
    /// A change request closed without merging.
    Closed,
    Merged,
}

/// A label with its colour when the host gave a six-digit hex one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    /// `rrggbb`, lowercase, without `#`.
    pub color: Option<String>,
}

impl Label {
    /// `labelColor`: GitHub's bare hex, or `#`-prefixed; anything else none.
    pub fn new(name: String, color: Option<&str>) -> Self {
        let color = color
            .map(|color| color.strip_prefix('#').unwrap_or(color))
            .filter(|hex| hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()))
            .map(str::to_ascii_lowercase);
        Self { name, color }
    }
}

/// What only a change request has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullFields {
    pub draft: bool,
    pub head: String,
    pub base: String,
    pub head_sha: String,
}

/// One listed issue or change request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    /// The provider it was read from.
    pub provider: String,
    pub kind: RepositoryKind,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub body: String,
    pub body_truncated: bool,
    pub updated_at: Option<String>,
    pub author: Option<String>,
    pub labels: Vec<Label>,
    pub comment_count: u64,
    /// The project it belongs to, which may be a fork's upstream.
    pub owner: String,
    pub repo: String,
    /// It belongs to the upstream of the repository's fork.
    pub upstream: bool,
    pub state: ItemState,
    /// `Some` exactly for a change request.
    pub pull: Option<PullFields>,
}

impl Reference {
    /// `referencePickerItemKey`: provider, project and number, with the kind
    /// marked so issue and change request numbers never collide.
    pub fn key(&self) -> String {
        let marker = match self.kind {
            RepositoryKind::Issue => '#',
            RepositoryKind::Pull => '!',
        };
        format!(
            "{}:{}/{}{marker}{}",
            self.provider,
            self.owner.to_lowercase(),
            self.repo.to_lowercase(),
            self.number
        )
    }

    /// `owner/repo`, or the repository alone for a top-level project.
    pub fn project_path(&self) -> String {
        if self.owner.is_empty() {
            self.repo.clone()
        } else {
            format!("{}/{}", self.owner, self.repo)
        }
    }

    /// The state it changes to from the board, or none for a merged change
    /// request.
    pub fn next_state(&self) -> Option<TargetState> {
        match self.state {
            ItemState::Merged => None,
            ItemState::Open => Some(TargetState::Closed),
            _ => Some(TargetState::Open),
        }
    }

    /// An open change request, which takes a review verdict.
    pub fn open_pull(&self) -> Option<&PullFields> {
        self.pull.as_ref().filter(|_| self.state == ItemState::Open)
    }
}

/// The state a close or reopen asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TargetState {
    Open,
    Closed,
}

impl TargetState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

/// How the provider names and numbers what it lists
/// (`GET /api/source-control/providers`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderTerms {
    pub id: String,
    pub label: String,
    /// `pullRequest`, `mergeRequest` or `change`.
    pub change_request: String,
    pub change_request_prefix: String,
    pub issue_prefix: String,
    /// The provider serves the board's search and detail reads.
    pub reference_search: bool,
}

impl ProviderTerms {
    /// `referenceNumberLabel`: `#12`, or `!12` for a GitLab merge request.
    pub fn number_label(&self, reference: &Reference) -> String {
        let prefix = match reference.kind {
            RepositoryKind::Issue => &self.issue_prefix,
            RepositoryKind::Pull => &self.change_request_prefix,
        };
        format!("{prefix}{}", reference.number)
    }
}

/// The terms of `provider`, or none while the provider list does not name it.
pub fn terms_of<'a>(providers: &'a [ProviderTerms], provider: &str) -> Option<&'a ProviderTerms> {
    providers.iter().find(|terms| terms.id == provider)
}

/// A checks run summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChecksSummary {
    pub state: ChecksState,
    pub total: u64,
    pub success: u64,
    pub failure: u64,
    pub pending: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChecksState {
    Success,
    Failure,
    Pending,
    Unknown,
}

/// A listed change request's checks and mergeability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullStatus {
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub checks: Option<ChecksSummary>,
    pub mergeable: Option<bool>,
    pub mergeable_state: Option<String>,
}

impl PullStatus {
    pub fn is_for(&self, reference: &Reference) -> bool {
        self.number == reference.number
            && self.owner.eq_ignore_ascii_case(&reference.owner)
            && self.repo.eq_ignore_ascii_case(&reference.repo)
    }
}

/// The colour a state takes, from the theme's change request colours.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StateTone {
    Open,
    Draft,
    Merged,
    Closed,
    /// An open change request with failed checks or a conflict.
    Blocked,
}

/// An item's state glyph, colour and label (`repositoryStateLook`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateLook {
    pub icon: &'static str,
    pub tone: StateTone,
    pub label_key: &'static str,
}

/// `repositoryStateLook` for issues and change requests: an open change
/// request turns blocked on failed checks or a conflict once its status has
/// arrived (`prVisualStateOf`).
pub fn state_look(reference: &Reference, status: Option<&PullStatus>) -> StateLook {
    let look = |icon, tone, label_key| StateLook {
        icon,
        tone,
        label_key,
    };
    match (reference.kind, reference.state) {
        (RepositoryKind::Issue, ItemState::Open) => look(
            "record-circle",
            StateTone::Open,
            "references.picker.state.open",
        ),
        (RepositoryKind::Issue, ItemState::NotPlanned) => look(
            "close-circle",
            StateTone::Closed,
            "references.picker.state.notPlanned",
        ),
        (RepositoryKind::Issue, _) => look(
            "checkbox-circle",
            StateTone::Merged,
            "references.picker.state.completed",
        ),
        (RepositoryKind::Pull, ItemState::Merged) => look(
            "git-merge",
            StateTone::Merged,
            "references.picker.state.merged",
        ),
        (RepositoryKind::Pull, ItemState::Open) => {
            if reference.pull.as_ref().is_some_and(|pull| pull.draft) {
                return look(
                    "git-pr-draft",
                    StateTone::Draft,
                    "references.picker.state.draft",
                );
            }
            let blocked = status.is_some_and(|status| {
                status
                    .checks
                    .is_some_and(|checks| checks.state == ChecksState::Failure)
                    || status.mergeable == Some(false)
                    || status.mergeable_state.as_deref() == Some("dirty")
            });
            look(
                "git-pull-request",
                if blocked {
                    StateTone::Blocked
                } else {
                    StateTone::Open
                },
                "references.picker.state.open",
            )
        }
        (RepositoryKind::Pull, _) => look(
            "git-close-pull-request",
            StateTone::Closed,
            "references.picker.state.closed",
        ),
    }
}

/// A review's verdict on a comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommentReview {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
}

/// One comment of an item's thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    pub author: Option<String>,
    pub body: String,
    pub created_at: Option<String>,
    pub url: String,
    /// The file a review comment is on.
    pub path: Option<String>,
    pub review: Option<CommentReview>,
}

/// A change request's overall review state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

impl ReviewDecision {
    /// `REVIEW_KEYS` of `ReferencePreview.tsx`.
    pub fn label_key(self) -> &'static str {
        match self {
            Self::Approved => "references.picker.preview.review.approved",
            Self::ChangesRequested => "references.picker.preview.review.changesRequested",
            Self::ReviewRequired => "references.picker.preview.review.required",
        }
    }
}

/// What a change request's detail adds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullDetail {
    pub review_decision: Option<ReviewDecision>,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    pub checks: Option<ChecksSummary>,
}

/// An item's thread and, for a change request, its size and review.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detail {
    pub comments: Vec<Comment>,
    /// How many comments there are in all; the newest are listed.
    pub comment_total: u64,
    pub pull: Option<PullDetail>,
}

/// The verdict of a review the board submits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReviewVerdict {
    Approve,
    RequestChanges,
}

impl ReviewVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::RequestChanges => "request-changes",
        }
    }
}

/// A provider and the instance it answers on (`github.com`,
/// `https://gitlab.com`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Identity {
    pub provider: String,
    pub instance: String,
}

/// An instance remotes are recognised on, with its separate SSH address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownInstance {
    pub identity: Identity,
    pub ssh_url: Option<String>,
}

/// One remote of the repository as the binding reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingRemote {
    pub name: String,
    pub fetch_url: String,
    pub fetch_fingerprint: String,
    pub push_url: String,
}

/// A provider association of the binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundProvider {
    pub identity: Identity,
    pub account_id: String,
    pub primary_remote: String,
    pub readiness: String,
    pub endpoint_fingerprint: Option<String>,
}

/// The repository's binding read (`GET /api/source-control/binding`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingRead {
    pub repository_id: String,
    pub revision: u64,
    pub remotes: Vec<BindingRemote>,
    pub providers: Vec<BoundProvider>,
}

/// The account and binding a repository's items are read and changed with.
/// Every field goes to the server, which validates it before any credential
/// is looked up; nothing here is a credential.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReadContext {
    pub directory: String,
    pub identity: Identity,
    pub repository_id: String,
    pub account_id: String,
    pub binding_revision: u64,
    pub primary_remote: String,
}

/// The host a remote URL names, with its port when it carries one:
/// `https://host:8443/o/r.git` and `git@host:o/r.git` alike.
fn remote_authority(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    if let Some((scheme, rest)) = url.split_once("://") {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        let host = host.to_lowercase();
        // The URL standard drops a special scheme's default port.
        let host = match scheme.to_lowercase().as_str() {
            "https" => host.strip_suffix(":443").map(str::to_owned).unwrap_or(host),
            "http" => host.strip_suffix(":80").map(str::to_owned).unwrap_or(host),
            _ => host,
        };
        return (!host.is_empty()).then_some(host);
    }
    // scp-like `[user@]host:path`
    let (head, _) = url.split_once(':')?;
    let host = head.rsplit_once('@').map_or(head, |(_, host)| host);
    (!host.is_empty() && !host.contains('/')).then(|| host.to_lowercase())
}

/// An instance's host and port: `github.com` reads as `https://github.com`.
fn instance_authority(instance: &str) -> String {
    if instance.contains("://") {
        remote_authority(instance).unwrap_or_else(|| instance.to_lowercase())
    } else {
        remote_authority(&format!("https://{instance}")).unwrap_or_else(|| instance.to_lowercase())
    }
}

/// An SSH remote's host and port: `ssh://` with its port (22 by default),
/// or the scp-like form on 22.
fn ssh_endpoint(url: &str) -> Option<(String, u16)> {
    let url = url.trim();
    if let Some(rest) = url.strip_prefix("ssh://") {
        let authority = rest.split('/').next().unwrap_or_default();
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        let (host, port) = match host.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().ok()?),
            None => (host, 22),
        };
        return Some((host.trim_end_matches('.').to_lowercase(), port));
    }
    if url.contains("://") {
        return None;
    }
    remote_authority(url).map(|host| (host.trim_end_matches('.').to_owned(), 22))
}

/// `resolveSourceControlIdentity` for one remote: the known instance its
/// push URL, else its fetch URL, points at.
pub fn remote_identity<'a>(
    remote: &BindingRemote,
    known: &'a [KnownInstance],
) -> Option<&'a Identity> {
    let url = if remote.push_url.trim().is_empty() {
        &remote.fetch_url
    } else {
        &remote.push_url
    };
    let authority = remote_authority(url)?;
    let ssh = ssh_endpoint(url);
    known
        .iter()
        .find(|candidate| {
            instance_authority(&candidate.identity.instance) == authority
                || candidate
                    .ssh_url
                    .as_deref()
                    .and_then(ssh_endpoint)
                    .is_some_and(|instance| ssh.as_ref().is_some_and(|remote| remote == &instance))
        })
        .map(|candidate| &candidate.identity)
}

fn same_host(left: &Identity, right: &Identity) -> bool {
    left.provider == right.provider
        && instance_authority(&left.instance) == instance_authority(&right.instance)
}

/// `repositoryHostsOf`: every host the repository lives on, bound or named
/// by a remote, whether or not an account there works.
pub fn repository_hosts(read: &BindingRead, known: &[KnownInstance]) -> Vec<Identity> {
    let mut hosts: Vec<Identity> = Vec::new();
    let mut add = |identity: &Identity| {
        if !hosts.iter().any(|host| same_host(host, identity)) {
            hosts.push(identity.clone());
        }
    };
    for provider in &read.providers {
        add(&provider.identity);
    }
    for remote in &read.remotes {
        if let Some(identity) = remote_identity(remote, known) {
            add(identity);
        }
    }
    hosts
}

/// `getBoundSourceControlReadContexts`: ready bound providers whose endpoint
/// still matches their primary remote, then, for each remote on a host no
/// usable binding covers (origin first), the current account of that host.
pub fn read_contexts(
    read: &BindingRead,
    directory: &str,
    known: &[KnownInstance],
    current_account: impl Fn(&Identity) -> Option<String>,
) -> Vec<ReadContext> {
    let context = |identity: &Identity, account_id: String, remote: &str| ReadContext {
        directory: directory.to_owned(),
        identity: identity.clone(),
        repository_id: read.repository_id.clone(),
        account_id,
        binding_revision: read.revision,
        primary_remote: remote.to_owned(),
    };
    let mut contexts: Vec<ReadContext> = read
        .providers
        .iter()
        .filter(|provider| {
            provider.readiness == "ready"
                && provider.endpoint_fingerprint.as_deref()
                    == read
                        .remotes
                        .iter()
                        .find(|remote| remote.name == provider.primary_remote)
                        .map(|remote| remote.fetch_fingerprint.as_str())
        })
        .map(|provider| {
            context(
                &provider.identity,
                provider.account_id.clone(),
                &provider.primary_remote,
            )
        })
        .collect();
    let mut covered: Vec<Identity> = read
        .providers
        .iter()
        .filter(|provider| provider.readiness != "account-unavailable")
        .map(|provider| provider.identity.clone())
        .collect();
    let mut remotes: Vec<&BindingRemote> = read.remotes.iter().collect();
    remotes.sort_by_key(|remote| remote.name != "origin");
    for remote in remotes {
        let Some(identity) = remote_identity(remote, known) else {
            continue;
        };
        if covered.iter().any(|host| same_host(host, identity)) {
            continue;
        }
        let Some(account_id) = current_account(identity) else {
            continue;
        };
        covered.push(identity.clone());
        contexts.push(context(identity, account_id, &remote.name));
    }
    contexts
}

/// The context whose provider comes first in the registry's order; one the
/// registry does not list comes last, and the earlier context wins a tie.
pub fn first_by_order<'a>(
    contexts: &'a [ReadContext],
    order: &[ProviderTerms],
) -> Option<&'a ReadContext> {
    let rank = |context: &ReadContext| {
        order
            .iter()
            .position(|terms| terms.id == context.identity.provider)
            .unwrap_or(usize::MAX)
    };
    let mut first: Option<&ReadContext> = None;
    for context in contexts {
        if first.is_none_or(|current| rank(context) < rank(current)) {
            first = Some(context);
        }
    }
    first
}

/// One page of a list read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Page {
    /// The account the context names is not signed in any more.
    Disconnected,
    /// The folder's remotes name no project the provider knows.
    NoRepository,
    Items {
        items: Vec<Reference>,
        cursor: Option<String>,
        has_more: bool,
    },
}

/// Why a list has nothing to show that is not an empty result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Unavailable {
    Disconnected,
    NoRepository,
}

/// Where the list's first page stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListStatus {
    Loading,
    Ready,
    Unavailable(Unavailable),
    /// The first page failed; nothing is listed.
    Failed(String),
}

/// A read the list started; only the newest one applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListTicket(u64);

/// The searched list (`useCachedList`): the first page, more pages, a
/// refresh that keeps the rows, and the highlighted row.
#[derive(Clone, Debug, Default)]
pub struct BoardList {
    query: Option<ListQuery>,
    status: Option<ListStatus>,
    items: Vec<Reference>,
    cursor: Option<String>,
    has_more: bool,
    refreshing: bool,
    loading_more: bool,
    /// A refresh failed while rows were listed; they stay.
    refresh_error: Option<String>,
    more_error: Option<String>,
    ticket: u64,
    more_ticket: Option<u64>,
    highlighted: Option<String>,
}

impl BoardList {
    pub fn query(&self) -> Option<&ListQuery> {
        self.query.as_ref()
    }

    pub fn status(&self) -> &ListStatus {
        self.status.as_ref().unwrap_or(&ListStatus::Loading)
    }

    pub fn items(&self) -> &[Reference] {
        &self.items
    }

    pub fn has_more(&self) -> bool {
        self.has_more
    }

    pub fn is_refreshing(&self) -> bool {
        self.refreshing
    }

    pub fn is_loading_more(&self) -> bool {
        self.loading_more
    }

    pub fn refresh_error(&self) -> Option<&str> {
        self.refresh_error.as_deref().or(self.more_error.as_deref())
    }

    /// Starts reading `query`'s first page. A new query starts empty; the
    /// same query is a refresh that keeps its rows until the answer.
    pub fn begin(&mut self, query: ListQuery) -> ListTicket {
        self.ticket += 1;
        self.more_ticket = None;
        self.loading_more = false;
        if self.query.as_ref() == Some(&query) && self.status.is_some() {
            self.refreshing = true;
        } else {
            self.query = Some(query);
            self.status = Some(ListStatus::Loading);
            self.items.clear();
            self.cursor = None;
            self.has_more = false;
            self.refreshing = false;
            self.refresh_error = None;
            self.more_error = None;
        }
        ListTicket(self.ticket)
    }

    /// Applies a first-page answer; false when a newer read superseded it.
    pub fn complete(&mut self, ticket: ListTicket, result: Result<Page, String>) -> bool {
        if ticket.0 != self.ticket {
            return false;
        }
        let refreshed = self.refreshing;
        self.refreshing = false;
        match result {
            Ok(Page::Items {
                items,
                cursor,
                has_more,
            }) => {
                self.items = items;
                self.cursor = cursor;
                self.has_more = has_more;
                self.status = Some(ListStatus::Ready);
                self.refresh_error = None;
                self.more_error = None;
            }
            Ok(Page::Disconnected) => self.unavailable(Unavailable::Disconnected),
            Ok(Page::NoRepository) => self.unavailable(Unavailable::NoRepository),
            Err(error) => {
                if refreshed && self.status == Some(ListStatus::Ready) {
                    self.refresh_error = Some(error);
                } else {
                    self.items.clear();
                    self.has_more = false;
                    self.status = Some(ListStatus::Failed(error));
                }
            }
        }
        true
    }

    fn unavailable(&mut self, reason: Unavailable) {
        self.items.clear();
        self.cursor = None;
        self.has_more = false;
        self.refresh_error = None;
        self.status = Some(ListStatus::Unavailable(reason));
    }

    /// Starts reading the next page: its ticket and cursor, or none while
    /// one is out, the list is not ready or it has no more.
    pub fn begin_more(&mut self) -> Option<(ListTicket, String)> {
        if self.loading_more
            || self.refreshing
            || self.status != Some(ListStatus::Ready)
            || !self.has_more
        {
            return None;
        }
        let cursor = self.cursor.clone()?;
        self.loading_more = true;
        self.more_ticket = Some(self.ticket);
        Some((ListTicket(self.ticket), cursor))
    }

    /// Appends the next page, skipping rows already listed; a failure keeps
    /// the rows and says so.
    pub fn complete_more(&mut self, ticket: ListTicket, result: Result<Page, String>) -> bool {
        if self.more_ticket != Some(ticket.0) || ticket.0 != self.ticket {
            return false;
        }
        self.more_ticket = None;
        self.loading_more = false;
        match result {
            Ok(Page::Items {
                items,
                cursor,
                has_more,
            }) => {
                for item in items {
                    let key = item.key();
                    if !self.items.iter().any(|listed| listed.key() == key) {
                        self.items.push(item);
                    }
                }
                self.cursor = cursor;
                self.has_more = has_more;
                self.more_error = None;
            }
            Ok(_) => self.has_more = false,
            Err(error) => self.more_error = Some(error),
        }
        true
    }

    /// The highlighted row's key: the one picked while it is listed, else
    /// the first row.
    pub fn highlighted_key(&self) -> Option<String> {
        self.highlighted
            .as_ref()
            .filter(|key| self.items.iter().any(|item| &item.key() == *key))
            .cloned()
            .or_else(|| self.items.first().map(Reference::key))
    }

    pub fn highlighted(&self) -> Option<&Reference> {
        let key = self.highlighted_key()?;
        self.items.iter().find(|item| item.key() == key)
    }

    pub fn highlight(&mut self, key: Option<String>) {
        self.highlighted = key;
    }

    /// Moves the highlight one row; true when it neared the end of a list
    /// with more pages, which reads the next one.
    pub fn move_highlight(&mut self, forward: bool) -> bool {
        if self.items.is_empty() {
            return false;
        }
        let current = self.highlighted_key();
        let index = self
            .items
            .iter()
            .position(|item| Some(item.key()) == current)
            .unwrap_or(0);
        let next = if forward {
            (index + 1).min(self.items.len() - 1)
        } else {
            index.saturating_sub(1)
        };
        self.highlighted = Some(self.items[next].key());
        forward && index + 3 >= self.items.len() && self.has_more
    }

    /// Replaces a listed item with the one an item read returned.
    pub fn replace(&mut self, reference: Reference) {
        let key = reference.key();
        if let Some(item) = self.items.iter_mut().find(|item| item.key() == key) {
            *item = reference;
        }
    }
}

/// How long ago an ISO time was, for a row or the preview
/// (`relativeTimeOf`): the key and count of its short form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelativeAge {
    pub key: &'static str,
    pub count: i64,
}

/// `relativeTimeOf` from milliseconds: just now under a minute, then
/// minutes, hours, days, weeks and years.
pub fn relative_age(now_ms: i64, at_ms: i64) -> RelativeAge {
    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const YEAR: i64 = 365 * DAY;
    let elapsed = now_ms.saturating_sub(at_ms).max(0);
    let (key, unit) = if elapsed < MINUTE {
        ("common.relative.justNow", 1)
    } else if elapsed < HOUR {
        ("common.relative.minutesAgoShort", MINUTE)
    } else if elapsed < DAY {
        ("common.relative.hoursAgoShort", HOUR)
    } else if elapsed < WEEK {
        ("common.relative.daysAgoShort", DAY)
    } else if elapsed < YEAR {
        ("common.relative.weeksAgoShort", WEEK)
    } else {
        ("common.relative.yearsAgoShort", YEAR)
    };
    RelativeAge {
        key,
        count: elapsed / unit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(number: u64) -> Reference {
        Reference {
            provider: "github".into(),
            kind: RepositoryKind::Issue,
            number,
            title: format!("Item {number}"),
            url: format!("https://github.com/o/r/issues/{number}"),
            body: String::new(),
            body_truncated: false,
            updated_at: None,
            author: None,
            labels: Vec::new(),
            comment_count: 0,
            owner: "O".into(),
            repo: "R".into(),
            upstream: false,
            state: ItemState::Open,
            pull: None,
        }
    }

    fn page(numbers: &[u64], has_more: bool) -> Page {
        Page::Items {
            items: numbers.iter().copied().map(reference).collect(),
            cursor: has_more.then(|| "next".to_owned()),
            has_more,
        }
    }

    fn github() -> KnownInstance {
        KnownInstance {
            identity: Identity {
                provider: "github".into(),
                instance: "github.com".into(),
            },
            ssh_url: None,
        }
    }

    fn remote(name: &str, url: &str) -> BindingRemote {
        BindingRemote {
            name: name.into(),
            fetch_url: url.into(),
            fetch_fingerprint: format!("fp-{name}"),
            push_url: url.into(),
        }
    }

    #[test]
    fn remotes_resolve_to_their_host_over_https_and_ssh() {
        let known = [
            github(),
            KnownInstance {
                identity: Identity {
                    provider: "gitlab".into(),
                    instance: "https://gitlab.example:8443".into(),
                },
                ssh_url: Some("ssh://git@ssh.gitlab.example:2222".into()),
            },
        ];
        let provider = |url: &str| {
            remote_identity(&remote("origin", url), &known).map(|id| id.provider.clone())
        };
        assert_eq!(
            provider("https://github.com/o/r.git").as_deref(),
            Some("github")
        );
        assert_eq!(
            provider("git@GitHub.com:o/r.git").as_deref(),
            Some("github")
        );
        assert_eq!(
            provider("https://user@gitlab.example:8443/o/r.git").as_deref(),
            Some("gitlab")
        );
        assert_eq!(
            provider("ssh://git@ssh.gitlab.example:2222/o/r.git").as_deref(),
            Some("gitlab")
        );
        assert_eq!(provider("https://gitlab.example/o/r.git"), None);
        assert_eq!(provider("/srv/git/r.git"), None);
    }

    #[test]
    fn a_ready_binding_answers_before_the_hosts_current_account() {
        let identity = github().identity;
        let read = BindingRead {
            repository_id: "repo".into(),
            revision: 3,
            remotes: vec![
                remote("upstream", "https://github.com/up/r.git"),
                remote("origin", "https://github.com/o/r.git"),
            ],
            providers: Vec::new(),
        };
        let contexts = read_contexts(&read, "/p", &[github()], |_| Some("acct".into()));
        // One context per host; origin is read first.
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].primary_remote, "origin");
        assert_eq!(contexts[0].account_id, "acct");
        assert_eq!(contexts[0].binding_revision, 3);

        let bound = BindingRead {
            providers: vec![BoundProvider {
                identity: identity.clone(),
                account_id: "bound".into(),
                primary_remote: "upstream".into(),
                readiness: "ready".into(),
                endpoint_fingerprint: Some("fp-upstream".into()),
            }],
            ..read.clone()
        };
        let contexts = read_contexts(&bound, "/p", &[github()], |_| Some("acct".into()));
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].account_id, "bound");

        // A binding whose endpoint moved grants nothing, and still covers its host.
        let moved = BindingRead {
            providers: vec![BoundProvider {
                endpoint_fingerprint: Some("old".into()),
                ..bound.providers[0].clone()
            }],
            ..read.clone()
        };
        assert!(read_contexts(&moved, "/p", &[github()], |_| Some("acct".into())).is_empty());
        // No account on the host: no context, but the host is still listed.
        assert!(read_contexts(&read, "/p", &[github()], |_| None).is_empty());
        assert_eq!(repository_hosts(&read, &[github()]), vec![identity]);
    }

    #[test]
    fn a_new_query_starts_empty_and_a_refresh_keeps_its_rows() {
        let mut list = BoardList::default();
        let query = ListQuery::default();
        let first = list.begin(query.clone());
        assert_eq!(list.status(), &ListStatus::Loading);
        assert!(list.complete(first, Ok(page(&[1, 2], false))));
        assert_eq!(list.highlighted_key().as_deref(), Some("github:o/r#1"));

        let refresh = list.begin(query.clone());
        assert!(list.is_refreshing());
        assert_eq!(list.items().len(), 2);
        assert!(list.complete(refresh, Err("offline".into())));
        assert_eq!(list.status(), &ListStatus::Ready);
        assert_eq!(list.items().len(), 2);
        assert_eq!(list.refresh_error(), Some("offline"));

        let other = list.begin(ListQuery {
            text: "bug".into(),
            ..query
        });
        assert!(list.items().is_empty());
        assert!(list.complete(other, Err("offline".into())));
        assert_eq!(list.status(), &ListStatus::Failed("offline".into()));
    }

    #[test]
    fn only_the_newest_read_applies() {
        let mut list = BoardList::default();
        let older = list.begin(ListQuery::default());
        let newer = list.begin(ListQuery {
            kind: RepositoryKind::Pull,
            ..ListQuery::default()
        });
        assert!(!list.complete(older, Ok(page(&[9], false))));
        assert!(list.complete(newer, Ok(Page::Disconnected)));
        assert_eq!(
            list.status(),
            &ListStatus::Unavailable(Unavailable::Disconnected)
        );
    }

    #[test]
    fn more_pages_append_once_and_the_highlight_reads_ahead() {
        let mut list = BoardList::default();
        let ticket = list.begin(ListQuery::default());
        list.complete(ticket, Ok(page(&[1, 2, 3], true)));
        assert!(!list.move_highlight(false));
        // Moving within three rows of the end asks for the next page.
        assert!(list.move_highlight(true));
        let (more, cursor) = list.begin_more().expect("a next page");
        assert_eq!(cursor, "next");
        assert!(list.begin_more().is_none());
        assert!(list.complete_more(more, Ok(page(&[3, 4], false))));
        let numbers: Vec<u64> = list.items().iter().map(|item| item.number).collect();
        assert_eq!(numbers, [1, 2, 3, 4]);
        assert!(!list.has_more());
        assert_eq!(list.highlighted().map(|item| item.number), Some(2));
    }

    #[test]
    fn a_refresh_that_drops_the_highlighted_row_falls_back_to_the_first() {
        let mut list = BoardList::default();
        let ticket = list.begin(ListQuery::default());
        list.complete(ticket, Ok(page(&[1, 2], false)));
        list.highlight(Some(reference(2).key()));
        let ticket = list.begin(ListQuery::default());
        list.complete(ticket, Ok(page(&[1], false)));
        assert_eq!(list.highlighted().map(|item| item.number), Some(1));
    }

    #[test]
    fn open_pulls_turn_blocked_on_failed_checks_or_a_conflict() {
        let mut pull = reference(5);
        pull.kind = RepositoryKind::Pull;
        pull.pull = Some(PullFields {
            draft: false,
            head: "h".into(),
            base: "main".into(),
            head_sha: "abc".into(),
        });
        let status = |mergeable| PullStatus {
            owner: "o".into(),
            repo: "r".into(),
            number: 5,
            checks: None,
            mergeable,
            mergeable_state: None,
        };
        assert_eq!(state_look(&pull, None).tone, StateTone::Open);
        assert_eq!(
            state_look(&pull, Some(&status(Some(false)))).tone,
            StateTone::Blocked
        );
        assert!(status(None).is_for(&pull));
        pull.state = ItemState::Merged;
        assert_eq!(pull.next_state(), None);
        assert_eq!(state_look(&pull, None).icon, "git-merge");
        let issue = reference(1);
        assert_eq!(issue.next_state(), Some(TargetState::Closed));
    }

    #[test]
    fn ages_follow_the_web_buckets() {
        let minute = 60_000;
        assert_eq!(
            relative_age(minute * 10, minute * 10).key,
            "common.relative.justNow"
        );
        let age = relative_age(minute * 125, 0);
        assert_eq!((age.key, age.count), ("common.relative.hoursAgoShort", 2));
    }

    #[test]
    fn labels_keep_only_hex_colours() {
        assert_eq!(
            Label::new("a".into(), Some("FF00aa")).color.as_deref(),
            Some("ff00aa")
        );
        assert_eq!(
            Label::new("a".into(), Some("#00ff00")).color.as_deref(),
            Some("00ff00")
        );
        assert_eq!(Label::new("a".into(), Some("red")).color, None);
    }
}
