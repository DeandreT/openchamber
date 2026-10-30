//! Captured request ownership, reads and explicit mutations for the native window.

use std::collections::{BTreeMap, BTreeSet};

use gpui_kit::{Context, Task, Window};
use openchamber_native_core::form::{FormAnswer, FormState};
use openchamber_native_core::ids::SessionId;
use openchamber_native_core::pending_requests::{
    PendingRequest, PendingRequests, ReplyFailure, RequestId, RequestKey, RequestKind,
    ResponseState,
};
use openchamber_native_core::permission::PermissionReply;
use openchamber_native_core::request_owner::{RequestOwner, RequestScope};
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::event::BlockingRequestEvent;
use openchamber_native_transport::requests::RequestError;

use crate::connection::ConnectionState;
use crate::form_input::FormDraft;
use crate::workspace::Workspace;

#[derive(Default)]
pub(crate) struct BlockingRequests {
    pub seeds: BTreeMap<
        openchamber_native_core::ids::RuntimeId,
        Vec<openchamber_native_transport::client::PendingRequestStub>,
    >,
    pub stores: BTreeMap<RequestScope, PendingRequests>,
    reads: BTreeMap<RequestScope, Task<()>>,
    actions: BTreeMap<RequestKey, Task<()>>,
    pub drafts: BTreeMap<RequestKey, FormDraft>,
    pub errors: BTreeSet<RequestKey>,
    pub collapsed: bool,
    pub selected: Option<RequestKey>,
    pub presented: Option<RequestKey>,
}

#[derive(Clone, Debug)]
pub enum RequestReply {
    Permission(PermissionReply),
    Form(FormAnswer),
    Dismiss,
}

pub(crate) async fn exact(
    client: &ServerClient,
    key: &RequestKey,
) -> Result<Option<PendingRequest>, RequestError> {
    match key.id {
        RequestId::Permission(_) => match client.permission_request(key).await {
            Ok(request) => Ok(Some(PendingRequest::Permission(request))),
            Err(RequestError::PermissionNotFound) => Ok(None),
            Err(error) => Err(error),
        },
        RequestId::Form(_) => match client.form_detail(key).await {
            Ok(detail) if detail.state == FormState::Pending => {
                Ok(Some(PendingRequest::Form(detail.form)))
            }
            Ok(_) | Err(RequestError::FormNotFound | RequestError::FormAlreadySettled) => Ok(None),
            Err(error) => Err(error),
        },
    }
}

pub(crate) async fn respond(
    client: &ServerClient,
    key: &RequestKey,
    reply: &RequestReply,
) -> Result<(), RequestError> {
    match reply {
        RequestReply::Permission(decision) => client.reply_permission(key, *decision, None).await,
        RequestReply::Form(answer) => client.reply_form(key, answer).await,
        RequestReply::Dismiss => client.cancel_form(key, None).await,
    }
}

impl Workspace {
    pub(crate) fn request_owner_visible(&self, owner: &RequestOwner, scope: &RequestScope) -> bool {
        if scope.runtime() != &self.runtime {
            return false;
        }
        let Some(selected) = &self.selected else {
            return false;
        };
        self.request_owner_belongs(owner, scope, selected)
    }

    fn request_owner_belongs(
        &self,
        owner: &RequestOwner,
        scope: &RequestScope,
        selected: &SessionId,
    ) -> bool {
        if matches!(owner, RequestOwner::Location) {
            return self
                .sessions
                .get(selected)
                .and_then(|session| session.resolved_directory())
                .is_some_and(|directory| directory == scope.directory());
        }
        let RequestOwner::Session(owner) = owner else {
            return false;
        };
        if self
            .sessions
            .get(owner)
            .and_then(|session| session.resolved_directory())
            .is_some_and(|directory| directory != scope.directory())
        {
            return false;
        }
        let mut current = Some(owner);
        let mut visited = BTreeSet::new();
        while let Some(id) = current {
            if id == selected {
                return true;
            }
            if !visited.insert(id) {
                break;
            }
            current = self
                .sessions
                .get(id)
                .and_then(|session| session.parent_id.as_ref());
        }
        false
    }

    pub(crate) fn requests_for_send(
        &self,
        key: &crate::send_attempt::SessionKey,
    ) -> Vec<RequestKey> {
        self.blocking
            .stores
            .iter()
            .filter(|(scope, _)| scope.runtime() == &key.runtime)
            .flat_map(|(scope, store)| {
                store
                    .entries()
                    .filter(|(request, _)| {
                        // The session index belongs to the visible runtime. After
                        // a switch, retain all captured-runtime blockers rather
                        // than resolving owners against another runtime's index.
                        key.runtime != self.runtime
                            || (self.request_owner_belongs(&request.owner, scope, &key.session)
                                && !self.request_held(&request.owner, &request.id))
                    })
                    .map(|(key, _)| key.clone())
            })
            .collect()
    }

    /// A new session is not indexed yet, so its location-owned forms need the
    /// explicit creation directory. Capture this only at the user's activation.
    pub(crate) fn requests_for_creation(
        &self,
        key: &crate::send_attempt::SessionKey,
        directory: &str,
    ) -> Vec<RequestKey> {
        let mut requests = self.requests_for_send(key);
        for (scope, store) in &self.blocking.stores {
            if scope.runtime() != &key.runtime || scope.directory() != directory {
                continue;
            }
            for (request, _) in store.entries() {
                if request.owner == RequestOwner::Location && !requests.contains(request) {
                    requests.push(request.clone());
                }
            }
        }
        requests
    }

    pub(crate) fn prepare_supersession(
        &mut self,
        keys: &[RequestKey],
    ) -> Result<Vec<openchamber_native_core::pending_requests::RequestAttempt>, ()> {
        // Preflight all cards before taking any lock. A lost response requires Check status.
        for key in keys {
            if let Some(entry) = self
                .blocking
                .stores
                .get(&key.scope)
                .and_then(|store| store.get(key))
                && (!entry.current
                    || entry.response != ResponseState::Pending
                    || self.blocking.actions.contains_key(key))
            {
                return Err(());
            }
        }
        let mut attempts = Vec::new();
        for key in keys {
            if let Some(store) = self.blocking.stores.get_mut(&key.scope)
                && store.get(key).is_some()
            {
                attempts.push(store.begin_reply(key).map_err(|_| ())?);
            }
        }
        Ok(attempts)
    }

    pub(crate) fn visible_requests(&self) -> Vec<RequestKey> {
        let mut keys = Vec::new();
        for (scope, store) in &self.blocking.stores {
            if scope.runtime() != &self.runtime {
                continue;
            }
            for (key, _) in store.entries() {
                if self.request_owner_visible(&key.owner, scope)
                    && !self.request_held(&key.owner, &key.id)
                {
                    keys.push(key.clone());
                }
            }
        }
        keys.sort_by_key(|key| (key.id.kind(), matches!(key.owner, RequestOwner::Location)));
        keys
    }

    pub(crate) fn active_request(&self) -> Option<RequestKey> {
        let keys = self.visible_requests();
        let kind = keys.first()?.id.kind();
        self.blocking
            .selected
            .as_ref()
            .filter(|key| key.id.kind() == kind && keys.contains(key))
            .cloned()
            .or_else(|| keys.into_iter().next())
    }

    pub(crate) fn request_actionable(&self, key: &RequestKey) -> bool {
        key.scope.runtime() == &self.runtime
            && match &key.owner {
                RequestOwner::Session(id) => self
                    .sessions
                    .get(id)
                    .and_then(|session| session.resolved_directory())
                    .is_none_or(|directory| directory == key.scope.directory()),
                RequestOwner::Location => true,
            }
            && !self.blocking.actions.contains_key(key)
            && self
                .connection
                .as_ref()
                .is_some_and(|connection| connection.state == ConnectionState::Connected)
            && self
                .blocking
                .stores
                .get(&key.scope)
                .and_then(|store| store.get(key))
                .is_some_and(|entry| entry.current && entry.response == ResponseState::Pending)
    }

    pub(crate) fn sync_request_drafts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.blocking.drafts.retain(|key, _| {
            self.blocking
                .stores
                .get(&key.scope)
                .is_some_and(|store| store.get(key).is_some())
        });
        let forms: Vec<_> = self
            .blocking
            .stores
            .values()
            .flat_map(|store| store.entries())
            .filter_map(|(key, entry)| match &entry.request {
                PendingRequest::Form(form) => Some((key.clone(), form.clone())),
                _ => None,
            })
            .collect();
        for (key, form) in forms {
            if self
                .blocking
                .drafts
                .get(&key)
                .is_none_or(|draft| !draft.same_fields(&form))
            {
                let draft = FormDraft::new(key.clone(), form, window, cx);
                self.blocking.drafts.insert(key, draft);
            } else if let Some(draft) = self.blocking.drafts.get_mut(&key) {
                draft.form = form;
            }
        }
        self.acknowledge_visible_form(cx);
    }

    pub(crate) fn refresh_blocking_requests(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selected) = self.selected.as_ref() else {
            return;
        };
        let mut scopes = BTreeSet::new();
        if let Some(scope) = self
            .sessions
            .get(selected)
            .and_then(|session| session.resolved_directory())
            .and_then(|directory| RequestScope::new(self.runtime.clone(), &directory))
        {
            scopes.insert(scope);
        }
        // Only pending requests make a descendant directory relevant. Walking
        // every historical child would start idle locations and their MCP servers.
        for key in self.visible_requests() {
            scopes.insert(key.scope);
        }
        for scope in scopes {
            self.load_blocking_scope(scope, window, cx);
        }
        let seeds = self
            .blocking
            .seeds
            .get(&self.runtime)
            .cloned()
            .unwrap_or_default();
        for seed in seeds {
            let directory = match &seed.owner {
                RequestOwner::Session(id) => self
                    .sessions
                    .get(id)
                    .and_then(|session| session.resolved_directory()),
                // Host stubs have no directory. Never guess one for `global`;
                // the selected location's full form list recovers its own forms.
                RequestOwner::Location => None,
            };
            let Some(scope) =
                directory.and_then(|directory| RequestScope::new(self.runtime.clone(), &directory))
            else {
                continue;
            };
            if !self.request_owner_visible(&seed.owner, &scope) {
                continue;
            }
            let key = RequestKey {
                scope: scope.clone(),
                owner: seed.owner,
                id: seed.id,
            };
            let store = self
                .blocking
                .stores
                .entry(scope.clone())
                .or_insert_with(|| PendingRequests::new(scope));
            if store.get(&key).is_none() {
                self.check_request(key, window, cx);
            }
        }
    }

    fn load_blocking_scope(
        &mut self,
        scope: RequestScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.blocking.reads.contains_key(&scope) {
            return;
        }
        let Some(connection) = self
            .connection
            .as_ref()
            .filter(|connection| connection.state == ConnectionState::Connected)
        else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let store = self
            .blocking
            .stores
            .entry(scope.clone())
            .or_insert_with(|| PendingRequests::new(scope.clone()));
        let permissions = store.begin_load(RequestKind::Permission);
        let forms = store.begin_load(RequestKind::Form);
        let captured = scope.clone();
        let read = connection.network.run(async move {
            tokio::join!(
                client.pending_permissions(&captured),
                client.pending_forms(&captured)
            )
        });
        let task_scope = scope.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = read.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.blocking.reads.remove(&scope);
                if let Some((permission_result, form_result)) = result
                    && let Some(store) = this.blocking.stores.get_mut(&scope)
                {
                    if let Ok(list) = permission_result {
                        let _ = store.apply_list(permissions, list.map(PendingRequest::Permission));
                    }
                    if let Ok(list) = form_result {
                        let _ = store.apply_list(forms, list.map(PendingRequest::Form));
                    }
                }
                this.sync_request_drafts(window, cx);
                cx.notify();
            });
        });
        self.blocking.reads.insert(task_scope, task);
    }

    pub(crate) fn blocking_event(
        &mut self,
        directory: Option<String>,
        event: BlockingRequestEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let owner = match &event {
            BlockingRequestEvent::Asked(request) => match request {
                PendingRequest::Permission(request) => {
                    RequestOwner::Session(request.session_id.clone())
                }
                PendingRequest::Form(form) => form.owner.clone(),
            },
            BlockingRequestEvent::Settled { owner, .. } => owner.clone(),
        };
        let directory = directory.or_else(|| match &owner {
            RequestOwner::Session(id) => self
                .sessions
                .get(id)
                .and_then(|session| session.resolved_directory()),
            RequestOwner::Location => None,
        });
        let Some(scope) =
            directory.and_then(|directory| RequestScope::new(self.runtime.clone(), &directory))
        else {
            return;
        };
        // Presentation only: a request the server may answer on its own is
        // held out of sight; the store still owns it and nothing answers it here.
        match &event {
            BlockingRequestEvent::Asked(PendingRequest::Permission(request)) => {
                let shown = self.blocking.stores.get(&scope).is_some_and(|store| {
                    store
                        .get(&RequestKey {
                            scope: scope.clone(),
                            owner: owner.clone(),
                            id: RequestId::Permission(request.id.clone()),
                        })
                        .is_some()
                });
                self.hold_permission_request(&request.session_id, &request.id, shown);
            }
            BlockingRequestEvent::Settled {
                owner: RequestOwner::Session(session),
                id: RequestId::Permission(permission),
            } => self.permission_modes.holds.settled(session, permission),
            _ => {}
        }
        let store = self
            .blocking
            .stores
            .entry(scope.clone())
            .or_insert_with(|| PendingRequests::new(scope.clone()));
        match event {
            BlockingRequestEvent::Asked(request) => {
                store.asked(request);
            }
            BlockingRequestEvent::Settled { owner, id } => {
                let _ = store.settle(&RequestKey { scope, owner, id });
            }
        }
        self.sync_request_drafts(window, cx);
    }

    pub(crate) fn invalidate_blocking(&mut self, directory: Option<&str>) {
        for (scope, store) in &mut self.blocking.stores {
            if scope.runtime() == &self.runtime
                && directory.is_none_or(|directory| scope.directory() == directory)
            {
                store.invalidate();
            }
        }
        self.blocking.reads.retain(|scope, _| {
            scope.runtime() != &self.runtime
                || directory.is_some_and(|directory| scope.directory() != directory)
        });
    }

    pub(crate) fn remove_session_requests(&mut self, id: &SessionId) {
        for (scope, store) in &mut self.blocking.stores {
            if scope.runtime() != &self.runtime {
                continue;
            }
            let keys: Vec<_> = store
                .entries()
                .filter(|(key, _)| key.owner == RequestOwner::Session(id.clone()))
                .map(|(key, _)| key.clone())
                .collect();
            for key in keys {
                let _ = store.settle(&key);
                self.blocking.drafts.remove(&key);
            }
        }
    }

    /// Explicit exact lookup. It never resends the previous decision.
    pub fn check_request(&mut self, key: RequestKey, window: &mut Window, cx: &mut Context<Self>) {
        if key.scope.runtime() != &self.runtime || self.blocking.actions.contains_key(&key) {
            return;
        }
        let Some(connection) = self
            .connection
            .as_ref()
            .filter(|connection| connection.state == ConnectionState::Connected)
        else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let Some(store) = self.blocking.stores.get(&key.scope) else {
            return;
        };
        let check = store.begin_check(&key);
        let captured = key.clone();
        let read = connection
            .network
            .run(async move { exact(&client, &captured).await });
        let task_key = key.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = read.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.blocking.actions.remove(&key);
                match result {
                    Some(Ok(request)) => {
                        let accepted = this
                            .blocking
                            .stores
                            .get_mut(&key.scope)
                            .is_some_and(|store| store.checked(check, request).is_ok());
                        if accepted {
                            if let Some(seeds) = this.blocking.seeds.get_mut(key.scope.runtime()) {
                                seeds.retain(|seed| seed.owner != key.owner || seed.id != key.id);
                            }
                            this.blocking.errors.remove(&key);
                        } else {
                            // A stale read grants no authority to consume the hint.
                            // The next scoped refresh must still discover its owner.
                            this.blocking.errors.insert(key);
                        }
                    }
                    _ => {
                        this.blocking.errors.insert(key);
                    }
                }
                this.sync_request_drafts(window, cx);
                cx.notify();
            });
        });
        self.blocking.actions.insert(task_key, task);
        cx.notify();
    }

    /// One explicit reply captured before I/O. Double activation cannot issue another mutation.
    pub fn reply_request(
        &mut self,
        key: RequestKey,
        reply: RequestReply,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.request_actionable(&key) {
            return;
        }
        let Some(entry) = self
            .blocking
            .stores
            .get(&key.scope)
            .and_then(|store| store.get(&key))
        else {
            return;
        };
        match (&entry.request, &reply) {
            (PendingRequest::Permission(request), RequestReply::Permission(decision))
                if *decision != PermissionReply::Always || request.can_save() => {}
            (PendingRequest::Form(_), RequestReply::Form(_) | RequestReply::Dismiss) => {}
            _ => return,
        }
        let Some(connection) = &self.connection else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let Ok(attempt) = self
            .blocking
            .stores
            .get_mut(&key.scope)
            .expect("entry exists")
            .begin_reply(&key)
        else {
            return;
        };
        let captured = key.clone();
        let request = connection
            .network
            .run(async move { respond(&client, &captured, &reply).await });
        let task_key = key.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.blocking.actions.remove(&key);
                if let Some(store) = this.blocking.stores.get_mut(&key.scope) {
                    match result {
                        Some(Ok(()))
                        | Some(Err(
                            RequestError::FormAlreadySettled
                            | RequestError::FormNotFound
                            | RequestError::PermissionNotFound,
                        )) => {
                            let _ = store.settle(&key);
                            this.blocking.errors.remove(&key);
                        }
                        result => {
                            let failure =
                                if matches!(result, Some(Err(RequestError::FormInvalidAnswer))) {
                                    ReplyFailure::Rejected
                                } else {
                                    ReplyFailure::Unknown
                                };
                            let _ = store.reply_failed(attempt, failure);
                            this.blocking.errors.insert(key);
                        }
                    }
                }
                this.sync_request_drafts(window, cx);
                cx.notify();
            });
        });
        self.blocking.actions.insert(task_key, task);
        cx.notify();
    }
}
