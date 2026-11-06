//! The issues and pull requests board's routes, as
//! `packages/web/src/api/source-control.ts` and `repository-references.ts`
//! call them: the provider list, the known instances, a host's accounts,
//! the repository binding, searched lists, an item's detail, listed change
//! requests' statuses, and comment, close or reopen, and review.
//!
//! Every provider read and write names a read context: provider, instance,
//! the repository and binding revision, the primary remote and the account
//! ID. The server validates it before any credential is looked up and keeps
//! every token itself; nothing here carries one. A provider's 401 (a refused
//! or missing provider account) is that route's answer, never the server's
//! sign-in failing. No call retries: a mutation's idempotency key belongs to
//! the user's action, so the caller decides whether to send it again.

use openchamber_native_core::source_board::{
    BindingRead, BindingRemote, BoundProvider, ChecksState, ChecksSummary, Comment, CommentReview,
    Detail, Identity, ItemState, KnownInstance, Label, ListQuery, Page, ProviderTerms, PullDetail,
    PullFields, PullStatus, ReadContext, Reference, RepositoryKind, ReviewDecision, ReviewVerdict,
    TargetState,
};
use reqwest::{Method, StatusCode, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::client::ServerClient;
use crate::error::TransportError;
use crate::request::RequestBuilder;

/// Why a source-control request did not succeed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SourceControlError {
    /// The server or the provider declined, with the server's stable code
    /// and message when it sent them.
    #[error("the server refused the source control request (HTTP {status})")]
    Refused {
        status: u16,
        code: Option<String>,
        message: Option<String>,
    },
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl SourceControlError {
    /// The server's explanation, when it gave one.
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Refused { message, .. } => message.as_deref(),
            Self::Transport(_) => None,
        }
    }

    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Refused { code, .. } => code.as_deref(),
            Self::Transport(_) => None,
        }
    }
}

/// What the binding route says about a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindingLookup {
    Repository(BindingRead),
    /// The folder is no repository the server can bind.
    NotRepository,
}

/// A host's sign-in state (`GET /{provider}/auth/status`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthStatus {
    /// The account reads use when none is bound: the current one while the
    /// host is connected.
    pub current_account: Option<String>,
    /// The server could not reach the host; the account state is unknown.
    pub unreachable: bool,
}

#[derive(Deserialize)]
struct ProvidersBody {
    providers: Vec<ProviderBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderBody {
    id: String,
    label: String,
    terminology: TerminologyBody,
    #[serde(default)]
    features: FeaturesBody,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TerminologyBody {
    change_request: String,
    change_request_prefix: String,
    issue_prefix: String,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FeaturesBody {
    #[serde(default)]
    reference_search: bool,
}

#[derive(Deserialize)]
struct InstancesBody {
    instances: Vec<InstanceBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstanceBody {
    provider: String,
    instance: String,
    instance_ssh_url: Option<String>,
}

#[derive(Deserialize)]
struct AuthStatusBody {
    status: Option<String>,
    #[serde(default)]
    connected: bool,
    #[serde(default)]
    accounts: Vec<AccountBody>,
}

#[derive(Deserialize)]
struct AccountBody {
    id: String,
    #[serde(default)]
    current: bool,
    status: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BindingBody {
    repository: RepositoryBody,
    revision: u64,
    binding: Option<BoundBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryBody {
    repository_id: String,
    #[serde(default)]
    remotes: Vec<RemoteBody>,
}

#[derive(Deserialize)]
struct RemoteBody {
    name: String,
    fetch: EndpointBody,
    push: EndpointBody,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EndpointBody {
    display_url: String,
    #[serde(default)]
    fingerprint: String,
}

#[derive(Deserialize)]
struct BoundBody {
    #[serde(default)]
    providers: Vec<BoundProviderBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BoundProviderBody {
    provider: String,
    instance: String,
    account_id: String,
    primary_remote: String,
    readiness: String,
    endpoint: Option<FingerprintBody>,
}

#[derive(Deserialize)]
struct FingerprintBody {
    fingerprint: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageBody {
    connected: bool,
    repo: Option<Value>,
    #[serde(default)]
    items: Vec<ReferenceBody>,
    cursor: Option<String>,
    #[serde(default)]
    has_more: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReferenceBody {
    kind: String,
    number: u64,
    title: String,
    url: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    body_truncated: bool,
    updated_at: Option<String>,
    author: Option<PersonBody>,
    #[serde(default)]
    labels: Vec<LabelBody>,
    #[serde(default)]
    comment_count: u64,
    source_repo: SourceRepoBody,
    state: String,
    #[serde(default)]
    draft: bool,
    head: Option<String>,
    base: Option<String>,
    head_sha: Option<String>,
}

#[derive(Deserialize)]
struct PersonBody {
    login: String,
}

#[derive(Deserialize)]
struct LabelBody {
    name: String,
    color: Option<String>,
}

#[derive(Deserialize)]
struct SourceRepoBody {
    owner: String,
    repo: String,
    source: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailResultBody {
    connected: bool,
    detail: Option<DetailBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailBody {
    #[serde(default)]
    comments: Vec<CommentBody>,
    #[serde(default)]
    comment_total: u64,
    pull: Option<PullDetailBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentBody {
    author: Option<PersonBody>,
    #[serde(default)]
    body: String,
    created_at: Option<String>,
    #[serde(default)]
    url: String,
    path: Option<String>,
    review: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullDetailBody {
    review_decision: Option<String>,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
    #[serde(default)]
    changed_files: u64,
    checks: Option<ChecksBody>,
}

#[derive(Deserialize)]
struct ChecksBody {
    state: String,
    total: u64,
    success: u64,
    failure: u64,
    pending: u64,
}

#[derive(Deserialize)]
struct StatusesBody {
    connected: bool,
    #[serde(default)]
    statuses: Vec<StatusBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusBody {
    owner: String,
    repo: String,
    number: u64,
    checks: Option<ChecksBody>,
    mergeable: Option<bool>,
    mergeable_state: Option<String>,
}

#[derive(Deserialize)]
struct ReceiptBody {
    status: String,
    target: ReceiptTargetBody,
    result: Value,
}

#[derive(Deserialize)]
struct ReceiptTargetBody {
    number: Option<u64>,
}

#[derive(Deserialize)]
struct RefusalBody {
    error: Option<String>,
    code: Option<String>,
    reason: Option<String>,
}

/// The project and item a write names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteTarget {
    pub owner: String,
    pub repo: String,
    pub number: u64,
    /// The head a review is for; a push since makes the server refuse it.
    pub head_sha: Option<String>,
}

impl WriteTarget {
    pub fn of(reference: &Reference) -> Self {
        Self {
            owner: reference.owner.clone(),
            repo: reference.repo.clone(),
            number: reference.number,
            head_sha: None,
        }
    }
}

/// What a review submission reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReviewOutcome {
    /// The review's text was posted with it.
    pub commented: bool,
}

impl ServerClient {
    fn source_url(&self, route: &str, query: &[(&str, &str)]) -> Url {
        let mut url = self.endpoint().url(route);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        url
    }

    fn provider_url(&self, context: &ReadContext, route: &str, query: &[(&str, &str)]) -> Url {
        let revision = context.binding_revision.to_string();
        let mut pairs = vec![
            ("instance", context.identity.instance.as_str()),
            ("directory", context.directory.as_str()),
            ("repositoryId", context.repository_id.as_str()),
            ("accountId", context.account_id.as_str()),
            ("bindingRevision", revision.as_str()),
            ("primaryRemote", context.primary_remote.as_str()),
        ];
        pairs.extend_from_slice(query);
        self.source_url(
            &format!("api/source-control/{}{route}", context.identity.provider),
            &pairs,
        )
    }

    /// Every provider the server serves, in registry order.
    pub async fn source_control_providers(&self) -> Result<Vec<ProviderTerms>, SourceControlError> {
        let body: ProvidersBody = provider_json(self.authorized(
            Method::GET,
            self.source_url("api/source-control/providers", &[]),
        ))
        .await?;
        Ok(body
            .providers
            .into_iter()
            .map(|provider| ProviderTerms {
                id: provider.id,
                label: provider.label,
                change_request: provider.terminology.change_request,
                change_request_prefix: provider.terminology.change_request_prefix,
                issue_prefix: provider.terminology.issue_prefix,
                reference_search: provider.features.reference_search,
            })
            .collect())
    }

    /// The instances remotes are recognised on: each provider's defaults and
    /// the ones it already knows.
    pub async fn source_control_instances(&self) -> Result<Vec<KnownInstance>, SourceControlError> {
        let body: InstancesBody = provider_json(self.authorized(
            Method::GET,
            self.source_url("api/source-control/instances", &[]),
        ))
        .await?;
        Ok(body
            .instances
            .into_iter()
            .map(|instance| KnownInstance {
                identity: Identity {
                    provider: instance.provider,
                    instance: instance.instance,
                },
                ssh_url: instance.instance_ssh_url,
            })
            .collect())
    }

    /// A host's current account, as `repository-binding.ts` asks for it.
    pub async fn source_control_auth_status(
        &self,
        identity: &Identity,
    ) -> Result<AuthStatus, SourceControlError> {
        let url = self.source_url(
            &format!("api/source-control/{}/auth/status", identity.provider),
            &[("instance", identity.instance.as_str())],
        );
        let body: AuthStatusBody = provider_json(self.authorized(Method::GET, url)).await?;
        let unreachable = matches!(
            body.status.as_deref(),
            Some("unreachable" | "temporarily-unavailable")
        );
        let connected = body.connected && body.status.as_deref() == Some("connected");
        Ok(AuthStatus {
            current_account: connected
                .then(|| {
                    body.accounts
                        .into_iter()
                        .find(|account| {
                            account.current && account.status.as_deref() != Some("invalid")
                        })
                        .map(|account| account.id)
                })
                .flatten(),
            unreachable,
        })
    }

    /// The folder's repository, remotes and provider associations.
    pub async fn source_control_binding(
        &self,
        directory: &str,
    ) -> Result<BindingLookup, SourceControlError> {
        let url = self.source_url("api/source-control/binding", &[("directory", directory)]);
        let body: BindingBody = match provider_json(self.authorized(Method::GET, url)).await {
            Ok(body) => body,
            Err(SourceControlError::Refused {
                status: 422, code, ..
            }) if code.as_deref() == Some("UNSUPPORTED_SOURCE_CONTROL_REPOSITORY") => {
                return Ok(BindingLookup::NotRepository);
            }
            Err(error) => return Err(error),
        };
        Ok(BindingLookup::Repository(BindingRead {
            repository_id: body.repository.repository_id,
            revision: body.revision,
            remotes: body
                .repository
                .remotes
                .into_iter()
                .map(|remote| BindingRemote {
                    name: remote.name,
                    fetch_url: remote.fetch.display_url,
                    fetch_fingerprint: remote.fetch.fingerprint,
                    push_url: remote.push.display_url,
                })
                .collect(),
            providers: body
                .binding
                .map(|binding| binding.providers)
                .unwrap_or_default()
                .into_iter()
                .map(|provider| BoundProvider {
                    identity: Identity {
                        provider: provider.provider,
                        instance: provider.instance,
                    },
                    account_id: provider.account_id,
                    primary_remote: provider.primary_remote,
                    readiness: provider.readiness,
                    endpoint_fingerprint: provider
                        .endpoint
                        .and_then(|endpoint| endpoint.fingerprint),
                })
                .collect(),
        }))
    }

    /// One page of the repository's issues or change requests.
    pub async fn source_control_references(
        &self,
        context: &ReadContext,
        query: &ListQuery,
        cursor: Option<&str>,
    ) -> Result<Page, SourceControlError> {
        let mut pairs = vec![
            ("kind", query.kind.as_str()),
            ("state", query.filter.state.as_str()),
            ("people", query.filter.people.as_str()),
        ];
        let text = query.text.trim();
        if !text.is_empty() {
            pairs.push(("query", text));
        }
        if let Some(cursor) = cursor {
            pairs.push(("cursor", cursor));
        }
        let url = self.provider_url(context, "/references", &pairs);
        let body: PageBody = provider_json(self.authorized(Method::GET, url)).await?;
        page(body, &context.identity.provider, query.kind)
    }

    /// An issue's or change request's comments and, for a change request,
    /// its size, review and checks.
    pub async fn source_control_reference_detail(
        &self,
        context: &ReadContext,
        reference: &Reference,
    ) -> Result<Option<Detail>, SourceControlError> {
        let number = reference.number.to_string();
        let url = self.provider_url(
            context,
            "/references/detail",
            &[
                ("owner", reference.owner.as_str()),
                ("repo", reference.repo.as_str()),
                ("number", number.as_str()),
            ],
        );
        let body: DetailResultBody = provider_json(self.authorized(Method::GET, url)).await?;
        if !body.connected {
            return Ok(None);
        }
        let Some(detail) = body.detail else {
            return Ok(None);
        };
        Ok(Some(Detail {
            comments: detail
                .comments
                .into_iter()
                .map(|comment| Comment {
                    author: comment.author.map(|author| author.login),
                    body: comment.body,
                    created_at: comment.created_at,
                    url: comment.url,
                    path: comment.path,
                    review: comment.review.as_deref().and_then(review_of),
                })
                .collect(),
            comment_total: detail.comment_total,
            pull: detail.pull.map(|pull| PullDetail {
                review_decision: pull.review_decision.as_deref().and_then(
                    |decision| match decision {
                        "approved" => Some(ReviewDecision::Approved),
                        "changes_requested" => Some(ReviewDecision::ChangesRequested),
                        "review_required" => Some(ReviewDecision::ReviewRequired),
                        _ => None,
                    },
                ),
                additions: pull.additions,
                deletions: pull.deletions,
                changed_files: pull.changed_files,
                checks: pull.checks.map(checks),
            }),
        }))
    }

    /// The checks and mergeability of listed change requests.
    pub async fn source_control_pull_statuses(
        &self,
        context: &ReadContext,
        pulls: &[Reference],
    ) -> Result<Vec<PullStatus>, SourceControlError> {
        let list = pulls
            .iter()
            .map(|pull| format!("{}/{}#{}", pull.owner, pull.repo, pull.number))
            .collect::<Vec<_>>()
            .join(",");
        let url = self.provider_url(context, "/references/status", &[("pulls", list.as_str())]);
        let body: StatusesBody = provider_json(self.authorized(Method::GET, url)).await?;
        if !body.connected {
            return Ok(Vec::new());
        }
        Ok(body
            .statuses
            .into_iter()
            .map(|status| PullStatus {
                owner: status.owner,
                repo: status.repo,
                number: status.number,
                checks: status.checks.map(checks),
                mergeable: status.mergeable,
                mergeable_state: status.mergeable_state,
            })
            .collect())
    }

    /// Makes the server forget its short-lived provider answers: the lists
    /// and their statuses, or everything read about one item, so a change
    /// made on the host's own site shows on the next read.
    pub async fn source_control_refresh_reads(
        &self,
        identity: &Identity,
        item: Option<&WriteTarget>,
    ) -> Result<(), SourceControlError> {
        let number = item.map(|item| item.number.to_string());
        let mut pairs = vec![("instance", identity.instance.as_str())];
        match (item, number.as_deref()) {
            (Some(item), Some(number)) => pairs.extend([
                ("scope", "item"),
                ("owner", item.owner.as_str()),
                ("repo", item.repo.as_str()),
                ("number", number),
            ]),
            _ => pairs.push(("scope", "lists")),
        }
        let url = self.source_url(
            &format!(
                "api/source-control/{}/read-cache/refresh",
                identity.provider
            ),
            &pairs,
        );
        let _: Value = provider_json(self.authorized(Method::POST, url)).await?;
        Ok(())
    }

    /// Comments on an issue or change request.
    pub async fn source_control_comment(
        &self,
        context: &ReadContext,
        kind: RepositoryKind,
        target: &WriteTarget,
        idempotency_key: &str,
        body: &str,
    ) -> Result<(), SourceControlError> {
        let route = match kind {
            RepositoryKind::Issue => "/issues/comment",
            RepositoryKind::Pull => "/pr/comment",
        };
        let mut request = mutation_body(context, target, idempotency_key);
        request.insert("body".to_owned(), json!(body));
        let result = self.mutate(context, route, request, target).await?;
        empty_result(&result)
    }

    /// Closes or reopens an issue or change request; answers its new state.
    pub async fn source_control_set_state(
        &self,
        context: &ReadContext,
        kind: RepositoryKind,
        target: &WriteTarget,
        idempotency_key: &str,
        state: TargetState,
    ) -> Result<TargetState, SourceControlError> {
        let route = match kind {
            RepositoryKind::Issue => "/issues/state",
            RepositoryKind::Pull => "/pr/state",
        };
        let mut request = mutation_body(context, target, idempotency_key);
        request.insert("state".to_owned(), json!(state.as_str()));
        let result = self.mutate(context, route, request, target).await?;
        match result.get("state").and_then(Value::as_str) {
            Some("open") if exact_keys(&result, &["state"]) => Ok(TargetState::Open),
            Some("closed") if exact_keys(&result, &["state"]) => Ok(TargetState::Closed),
            _ => Err(malformed("the state change result is invalid")),
        }
    }

    /// Approves an open change request, or requests changes with `body`.
    pub async fn source_control_review(
        &self,
        context: &ReadContext,
        target: &WriteTarget,
        idempotency_key: &str,
        verdict: ReviewVerdict,
        body: Option<&str>,
    ) -> Result<ReviewOutcome, SourceControlError> {
        let mut request = mutation_body(context, target, idempotency_key);
        request.insert("verdict".to_owned(), json!(verdict.as_str()));
        if let Some(body) = body {
            request.insert("body".to_owned(), json!(body));
        }
        let result = self.mutate(context, "/pr/review", request, target).await?;
        match result.get("commented").and_then(Value::as_bool) {
            Some(commented) if exact_keys(&result, &["commented"]) => {
                Ok(ReviewOutcome { commented })
            }
            _ => Err(malformed("the review result is invalid")),
        }
    }

    /// Posts a durable mutation and checks its receipt names the target.
    async fn mutate(
        &self,
        context: &ReadContext,
        route: &str,
        body: Map<String, Value>,
        target: &WriteTarget,
    ) -> Result<Map<String, Value>, SourceControlError> {
        let url = self.source_url(
            &format!("api/source-control/{}{route}", context.identity.provider),
            &[("instance", context.identity.instance.as_str())],
        );
        let receipt: ReceiptBody = provider_json(
            self.authorized(Method::POST, url)
                .json(&Value::Object(body)),
        )
        .await?;
        if receipt.status != "succeeded" || receipt.target.number != Some(target.number) {
            return Err(malformed("the mutation receipt does not match the request"));
        }
        match receipt.result {
            Value::Object(result) => Ok(result),
            _ => Err(malformed("the mutation result is invalid")),
        }
    }
}

/// The mutation's context and target, exactly the fields the server's
/// `mutationBody` sends.
fn mutation_body(
    context: &ReadContext,
    target: &WriteTarget,
    idempotency_key: &str,
) -> Map<String, Value> {
    let mut item = Map::new();
    item.insert(
        "project".to_owned(),
        json!({ "owner": target.owner, "name": target.repo }),
    );
    item.insert("number".to_owned(), json!(target.number));
    if let Some(head_sha) = &target.head_sha {
        item.insert("headSha".to_owned(), json!(head_sha));
    }
    let mut body = Map::new();
    body.insert("provider".to_owned(), json!(context.identity.provider));
    body.insert("instance".to_owned(), json!(context.identity.instance));
    body.insert("directory".to_owned(), json!(context.directory));
    body.insert("repositoryId".to_owned(), json!(context.repository_id));
    body.insert("accountId".to_owned(), json!(context.account_id));
    body.insert(
        "bindingRevision".to_owned(),
        json!(context.binding_revision),
    );
    body.insert("primaryRemote".to_owned(), json!(context.primary_remote));
    body.insert("idempotencyKey".to_owned(), json!(idempotency_key));
    body.insert("target".to_owned(), Value::Object(item));
    body
}

fn exact_keys(value: &Map<String, Value>, keys: &[&str]) -> bool {
    value.len() == keys.len() && keys.iter().all(|key| value.contains_key(*key))
}

fn empty_result(result: &Map<String, Value>) -> Result<(), SourceControlError> {
    if result.is_empty() {
        Ok(())
    } else {
        Err(malformed("the mutation result is invalid"))
    }
}

fn malformed(message: &str) -> SourceControlError {
    TransportError::Malformed(message.to_owned()).into()
}

fn review_of(value: &str) -> Option<CommentReview> {
    match value {
        "approved" => Some(CommentReview::Approved),
        "changes_requested" => Some(CommentReview::ChangesRequested),
        "commented" => Some(CommentReview::Commented),
        "dismissed" => Some(CommentReview::Dismissed),
        _ => None,
    }
}

fn checks(body: ChecksBody) -> ChecksSummary {
    ChecksSummary {
        state: match body.state.as_str() {
            "success" => ChecksState::Success,
            "failure" => ChecksState::Failure,
            "pending" => ChecksState::Pending,
            _ => ChecksState::Unknown,
        },
        total: body.total,
        success: body.success,
        failure: body.failure,
        pending: body.pending,
    }
}

/// A page: disconnected, no repository, or its items, each checked against
/// the kind asked for.
fn page(body: PageBody, provider: &str, kind: RepositoryKind) -> Result<Page, SourceControlError> {
    if !body.connected {
        return Ok(Page::Disconnected);
    }
    if body.repo.as_ref().is_none_or(Value::is_null) {
        return Ok(Page::NoRepository);
    }
    let items = body
        .items
        .into_iter()
        .map(|item| reference(item, provider, kind))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Page::Items {
        items,
        cursor: body.cursor.filter(|cursor| !cursor.is_empty()),
        has_more: body.has_more,
    })
}

fn reference(
    body: ReferenceBody,
    provider: &str,
    kind: RepositoryKind,
) -> Result<Reference, SourceControlError> {
    let state = match (kind, body.kind.as_str(), body.state.as_str()) {
        (RepositoryKind::Issue, "issue", "open") | (RepositoryKind::Pull, "pull", "open") => {
            ItemState::Open
        }
        (RepositoryKind::Issue, "issue", "completed") => ItemState::Completed,
        (RepositoryKind::Issue, "issue", "not_planned") => ItemState::NotPlanned,
        (RepositoryKind::Pull, "pull", "closed") => ItemState::Closed,
        (RepositoryKind::Pull, "pull", "merged") => ItemState::Merged,
        _ => return Err(malformed("a listed item has an unexpected kind or state")),
    };
    let pull = match kind {
        RepositoryKind::Issue => None,
        RepositoryKind::Pull => Some(PullFields {
            draft: body.draft,
            head: body
                .head
                .ok_or_else(|| malformed("a change request has no head"))?,
            base: body
                .base
                .ok_or_else(|| malformed("a change request has no base"))?,
            head_sha: body
                .head_sha
                .ok_or_else(|| malformed("a change request has no head revision"))?,
        }),
    };
    Ok(Reference {
        provider: provider.to_owned(),
        kind,
        number: body.number,
        title: body.title,
        url: body.url,
        body: body.body,
        body_truncated: body.body_truncated,
        updated_at: body.updated_at,
        author: body.author.map(|author| author.login),
        labels: body
            .labels
            .into_iter()
            .map(|label| Label::new(label.name, label.color.as_deref()))
            .collect(),
        comment_count: body.comment_count,
        owner: body.source_repo.owner,
        repo: body.source_repo.repo,
        upstream: body.source_repo.source == "upstream",
        state,
        pull,
    })
}

/// Sends, then reads the answer: the parsed body of a success, the
/// server's refusal otherwise. Redirects and the server's own sign-in
/// failures stay transport errors.
async fn provider_json<T: DeserializeOwned>(
    request: RequestBuilder,
) -> Result<T, SourceControlError> {
    let answer = request
        .send_for_provider()
        .await
        .map_err(TransportError::from_request)?;
    if answer.status.is_redirection() {
        return Err(TransportError::Redirected.into());
    }
    if answer.server_lock {
        return Err(TransportError::AuthenticationRequired.into());
    }
    let refusal: Option<RefusalBody> = answer
        .json
        .then(|| serde_json::from_slice(&answer.bytes).ok())
        .flatten();
    if answer.status == StatusCode::FORBIDDEN
        && refusal.as_ref().is_none_or(|body| body.error.is_none())
    {
        return Err(TransportError::AuthenticationRequired.into());
    }
    if answer.status.is_success() {
        return serde_json::from_slice(&answer.bytes)
            .map_err(|error| TransportError::Malformed(error.to_string()).into());
    }
    let refusal = refusal.unwrap_or(RefusalBody {
        error: None,
        code: None,
        reason: None,
    });
    Err(SourceControlError::Refused {
        status: answer.status.as_u16(),
        code: refusal.code.or(refusal.reason),
        message: refusal.error,
    })
}

#[cfg(test)]
#[path = "source_control_tests.rs"]
mod tests;
