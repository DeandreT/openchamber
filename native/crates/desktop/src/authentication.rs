//! Password entry, saved-token recovery and authentication ownership.

use std::time::{Duration, Instant};

use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::{AppContext, Context, Entity, FocusHandle, Task, Window};
use openchamber_native_transport::authentication::{AuthenticationStatus, LoginTrust};
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::identity::{GLOBAL_EVENTS_CAPABILITY, ServerIdentity};

use crate::connection::{ConnectionFailure, ConnectionState, ServerConfig};
use crate::credential_store::{CredentialOwner, CredentialStore, StoreError};
use crate::workspace::Workspace;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SignInMethod {
    Password,
    Pairing,
}

pub(crate) struct Authentication {
    pub(crate) password: Entity<InputState>,
    pub(crate) trust_device: bool,
    pub(crate) visible: bool,
    pub(crate) method: SignInMethod,
    pub(crate) submitting: bool,
    pub(crate) failure: Option<ConnectionFailure>,
    pub(crate) storage_error: Option<StoreError>,
    pub(crate) store: CredentialStore,
    pub(crate) remembered: Option<(CredentialOwner, Credential)>,
    pub(crate) task: Option<Task<()>>,
    pub(crate) unlock_focus: FocusHandle,
    pub(crate) trust_focus: FocusHandle,
    pub(crate) retry_not_before: Option<Instant>,
    retry_task: Option<Task<()>>,
}

impl Authentication {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let placeholder = crate::locale::strings(cx).get("sessionAuth.password.placeholder");
        Self {
            password: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder(placeholder)
            }),
            trust_device: false,
            visible: false,
            method: SignInMethod::Password,
            submitting: false,
            failure: None,
            storage_error: None,
            store: CredentialStore::default(),
            remembered: None,
            task: None,
            unlock_focus: cx.focus_handle(),
            trust_focus: cx.focus_handle(),
            retry_not_before: None,
            retry_task: None,
        }
    }
}

pub(crate) struct VerifiedConnection {
    pub(crate) client: ServerClient,
    pub(crate) identity: ServerIdentity,
    pub(crate) credential: Credential,
}

pub(crate) enum ConnectOutcome {
    Ready(Box<VerifiedConnection>),
    Locked {
        status: AuthenticationStatus,
        storage_error: Option<StoreError>,
    },
}

fn require_compatible(identity: &ServerIdentity) -> Result<(), TransportError> {
    if !identity.supports(GLOBAL_EVENTS_CAPABILITY) {
        return Err(TransportError::Incompatible(
            "the server has no global event bridge".into(),
        ));
    }
    Ok(())
}

/// Probe identity before reading or sending any remembered credential.
pub(crate) async fn connect_server(
    config: ServerConfig,
    remembered: Option<(CredentialOwner, Credential)>,
    store: CredentialStore,
) -> Result<ConnectOutcome, TransportError> {
    let anonymous = ServerClient::new(config.endpoint.clone(), Credential::None)?;
    let identity = anonymous.probe().await?;
    require_compatible(&identity)?;
    let owner = CredentialOwner::new(&config.endpoint, &identity);
    let mut storage_error = None;
    let credential = if let Some(password) = config.password {
        let credential = anonymous.login(&password, &config.client).await?;
        // The login response itself proves authentication. Retain its token
        // before a later request can fail; the password is never replayed.
        let client =
            ServerClient::new(config.endpoint, credential.clone())?.with_identity(&identity);
        return Ok(ConnectOutcome::Ready(Box::new(VerifiedConnection {
            client,
            identity,
            credential,
        })));
    } else {
        let status = anonymous.authentication_status().await?;
        if status == AuthenticationStatus::Authenticated {
            return Ok(ConnectOutcome::Ready(Box::new(VerifiedConnection {
                client: anonymous.with_identity(&identity),
                identity,
                credential: Credential::None,
            })));
        }
        let cached = remembered.filter(|(cached_owner, _)| Some(cached_owner) == owner.as_ref());
        if let Some((_, credential)) = cached {
            credential
        } else if let Some(owner) = owner.clone() {
            match store.load(owner).await {
                Ok(Some(credential)) => credential,
                result => {
                    storage_error = result.err();
                    return Ok(ConnectOutcome::Locked {
                        status,
                        storage_error,
                    });
                }
            }
        } else {
            return Ok(ConnectOutcome::Locked {
                status,
                storage_error,
            });
        }
    };
    let client = ServerClient::new(config.endpoint, credential.clone())?.with_identity(&identity);
    let status = client.authentication_status().await?;
    if status == AuthenticationStatus::Authenticated {
        Ok(ConnectOutcome::Ready(Box::new(VerifiedConnection {
            client,
            identity,
            credential,
        })))
    } else {
        // A definite rejection retires only this verified owner's saved token.
        if let Some(owner) = owner {
            storage_error = store.remove(owner).await.err();
        }
        Ok(ConnectOutcome::Locked {
            status,
            storage_error,
        })
    }
}

impl Workspace {
    pub(crate) fn watch_authentication(
        &mut self,
        generation: u64,
        mut failures: tokio::sync::watch::Receiver<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let endpoint = connection.config.endpoint.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            loop {
                if *failures.borrow_and_update() > 0 {
                    let _ = this.update_in(cx, |this, window, cx| {
                        let Some(connection) = this.connection.as_mut().filter(|connection| {
                            connection.generation == generation
                                && connection.config.endpoint == endpoint
                        }) else {
                            return;
                        };
                        connection.restart();
                        connection.retry_task = None;
                        connection.state =
                            ConnectionState::Failed(ConnectionFailure::AuthenticationRequired);
                        this.show_authentication(
                            ConnectionFailure::AuthenticationRequired,
                            None,
                            window,
                            cx,
                        );
                        this.invalidate_terminal_client(cx);
                        cx.notify();
                    });
                    break;
                }
                if failures.changed().await.is_err() {
                    break;
                }
            }
        });
        if let Some(connection) = self.connection.as_mut() {
            connection.authentication_task = Some(task);
        }
    }

    pub(crate) fn on_authentication_input(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { .. } => self.submit_authentication(window, cx),
            InputEvent::Change => {
                if self.authentication.retry_not_before.is_none() {
                    self.authentication.failure = None;
                }
                cx.notify();
            }
            _ => {}
        }
    }

    pub(crate) fn show_authentication(
        &mut self,
        failure: ConnectionFailure,
        storage_error: Option<StoreError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.authentication.visible = true;
        self.authentication.method = if failure == ConnectionFailure::ClientAuthenticationRequired {
            SignInMethod::Pairing
        } else {
            SignInMethod::Password
        };
        self.authentication.submitting = false;
        self.authentication.failure = Some(failure);
        self.authentication.storage_error = storage_error;
        self.authentication.remembered = None;
        self.authentication.retry_task = None;
        self.authentication.retry_not_before = None;
        if let ConnectionFailure::AuthenticationRateLimited {
            retry_after_seconds: Some(seconds),
        } = failure
            && let Some(deadline) = Instant::now().checked_add(Duration::from_secs(seconds))
        {
            self.authentication.retry_not_before = Some(deadline);
            self.authentication.retry_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_secs(seconds))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if this.authentication.retry_not_before == Some(deadline) {
                        this.authentication.retry_not_before = None;
                        this.authentication.retry_task = None;
                        cx.notify();
                    }
                });
            }));
        }
        self.authentication
            .password
            .update(cx, |input, cx| input.focus(window, cx));
    }

    pub(crate) fn submit_authentication(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.authentication.visible
            || self.authentication.submitting
            || self
                .authentication
                .retry_not_before
                .is_some_and(|deadline| deadline > Instant::now())
            || self.authentication.method == SignInMethod::Pairing
        {
            return;
        }
        let password = self.authentication.password.read(cx).value().to_string();
        if password.is_empty() {
            return;
        }
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        // The UI replaces any launch-time password; later reconnects use the
        // exact-owner token in memory or the secure store.
        connection.config.password = None;
        connection.retry_task = None;
        let generation = connection.restart();
        let config = connection.config.clone();
        let target_endpoint = config.endpoint.clone();
        let trust = if self.authentication.trust_device {
            LoginTrust::Device
        } else {
            LoginTrust::Session
        };
        let store = self.authentication.store.clone();
        self.authentication.submitting = true;
        self.authentication.failure = None;
        let task = connection.network.run(async move {
            let anonymous = ServerClient::new(config.endpoint.clone(), Credential::None)?;
            let identity = anonymous.probe().await?;
            require_compatible(&identity)?;
            let credential = anonymous
                .login_with_trust(&password, &config.client, trust)
                .await?;
            let client = ServerClient::new(config.endpoint.clone(), credential.clone())?
                .with_identity(&identity);
            let storage_error = match CredentialOwner::new(&config.endpoint, &identity) {
                Some(owner) if trust == LoginTrust::Device => {
                    store.save(owner, credential.clone()).await.err()
                }
                Some(owner) => store.remove(owner).await.err(),
                None if trust == LoginTrust::Device => Some(StoreError::Unavailable),
                None => None,
            };
            Ok::<_, TransportError>((
                VerifiedConnection {
                    client,
                    identity,
                    credential,
                },
                storage_error,
            ))
        });
        self.authentication.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.connection.as_ref().is_none_or(|connection| {
                    connection.generation != generation
                        || connection.config.endpoint != target_endpoint
                }) {
                    return;
                }
                this.authentication.submitting = false;
                this.authentication.task = None;
                match result {
                    Some(Ok((verified, storage_error))) => {
                        this.authentication.storage_error = storage_error;
                        this.finish_connect(
                            generation,
                            Some(Ok(ConnectOutcome::Ready(Box::new(verified)))),
                            window,
                            cx,
                        );
                        if storage_error.is_some() {
                            this.notify_mutation_failure(
                                "sessionAuth.error.credentialUpdateFailed",
                                window,
                                cx,
                            );
                        }
                    }
                    failure => {
                        let error = failure
                            .and_then(Result::err)
                            .unwrap_or(TransportError::Cancelled);
                        let failure = ConnectionFailure::from_error(&error);
                        if let Some(connection) = this.connection.as_mut() {
                            connection.state = ConnectionState::Failed(failure);
                        }
                        this.show_authentication(failure, None, window, cx);
                    }
                }
                cx.notify();
            });
        }));
        self.invalidate_terminal_client(cx);
        cx.notify();
    }
}
