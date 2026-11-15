//! OS-backed client-token storage. No token is written to application settings.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex as StateMutex, OnceLock};

use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::endpoint::Endpoint;
use openchamber_native_transport::identity::ServerIdentity;
use tokio::sync::Mutex;

const SERVICE: &str = "ai.openchamber.native";

/// A token belongs to the canonical endpoint and the identity it proved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CredentialOwner(String, String);

impl CredentialOwner {
    pub(crate) fn new(endpoint: &Endpoint, identity: &ServerIdentity) -> Option<Self> {
        let server = identity
            .server_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())?;
        let name = serde_json::to_string(&[endpoint.as_str(), server])
            .expect("strings serialize as a JSON array");
        Some(Self(name, endpoint.as_str().to_owned()))
    }
}

/// Errors are intentionally detached from backend errors, which may contain
/// secret bytes in BadEncoding, BadDataFormat or ambiguous-entry diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StoreError {
    Unavailable,
    InvalidData,
    Ambiguous,
    Superseded,
}

impl From<keyring::Error> for StoreError {
    fn from(error: keyring::Error) -> Self {
        match error {
            keyring::Error::BadEncoding(_) | keyring::Error::BadDataFormat(..) => Self::InvalidData,
            keyring::Error::Ambiguous(_) => Self::Ambiguous,
            _ => Self::Unavailable,
        }
    }
}

/// The OS boundary is injectable so tests never read the user's credential store.
pub(crate) trait CredentialBackend: Send + Sync {
    fn load(&self, owner: &CredentialOwner) -> Result<Option<String>, StoreError>;
    fn save(&self, owner: &CredentialOwner, token: &str) -> Result<(), StoreError>;
    fn remove(&self, owner: &CredentialOwner) -> Result<(), StoreError>;
    fn forget_endpoint(&self, endpoint: &str) -> Result<(), StoreError>;
}

struct NativeBackend;

impl CredentialBackend for NativeBackend {
    fn load(&self, owner: &CredentialOwner) -> Result<Option<String>, StoreError> {
        let entry = keyring::Entry::new(SERVICE, &owner.0).map_err(StoreError::from)?;
        match entry.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn save(&self, owner: &CredentialOwner, token: &str) -> Result<(), StoreError> {
        keyring::Entry::new(SERVICE, &owner.0)
            .map_err(StoreError::from)?
            .set_password(token)
            .map_err(StoreError::from)
    }

    fn remove(&self, owner: &CredentialOwner) -> Result<(), StoreError> {
        let entry = keyring::Entry::new(SERVICE, &owner.0).map_err(StoreError::from)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn forget_endpoint(&self, endpoint: &str) -> Result<(), StoreError> {
        keyring::Entry::store_status()
            .as_ref()
            .map_err(|_| StoreError::Unavailable)?;
        let query = HashMap::from([("service", SERVICE)]);
        let entries = keyring_core::Entry::search(&query).map_err(StoreError::from)?;
        for entry in entries {
            let attributes = entry.get_attributes().map_err(StoreError::from)?;
            if attributes.get("service").map(String::as_str) != Some(SERVICE) {
                continue;
            }
            let Some(account) = attributes.get("username") else {
                continue;
            };
            let Ok([saved_endpoint, _server_id]) = serde_json::from_str::<[String; 2]>(account)
            else {
                continue;
            };
            if saved_endpoint == endpoint {
                match entry.delete_credential() {
                    Ok(()) | Err(keyring::Error::NoEntry) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
    }
}

/// One ordered worker boundary for reads, saves and deletion. The mutex guard
/// moves into the blocking task, so cancellation cannot let a newer write pass it.
#[derive(Clone)]
pub(crate) struct CredentialStore {
    state: Arc<StoreState>,
    backend: Arc<dyn CredentialBackend>,
}

#[derive(Default)]
struct StoreState {
    order: Arc<Mutex<()>>,
    epochs: StateMutex<HashMap<String, u64>>,
}

#[derive(Clone, Debug)]
pub(crate) struct CredentialLease {
    endpoint: String,
    revision: u64,
}

impl Default for CredentialStore {
    fn default() -> Self {
        static STATE: OnceLock<Arc<StoreState>> = OnceLock::new();
        Self {
            state: STATE
                .get_or_init(|| Arc::new(StoreState::default()))
                .clone(),
            backend: Arc::new(NativeBackend),
        }
    }
}

impl fmt::Debug for CredentialStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialStore")
    }
}

impl CredentialStore {
    pub(crate) fn capture(&self, endpoint: &str) -> Option<CredentialLease> {
        let epochs = self.state.epochs.lock().ok()?;
        Some(CredentialLease {
            endpoint: endpoint.to_owned(),
            revision: epochs.get(endpoint).copied().unwrap_or(0),
        })
    }

    pub(crate) fn is_current(&self, lease: &CredentialLease) -> bool {
        self.capture(&lease.endpoint)
            .is_some_and(|current| current.revision == lease.revision)
    }

    pub(crate) fn invalidate_endpoint(
        &self,
        endpoint: &str,
    ) -> Result<CredentialLease, StoreError> {
        let mut epochs = self
            .state
            .epochs
            .lock()
            .map_err(|_| StoreError::Unavailable)?;
        let revision = epochs.entry(endpoint.to_owned()).or_default();
        *revision = revision.checked_add(1).ok_or(StoreError::Unavailable)?;
        Ok(CredentialLease {
            endpoint: endpoint.to_owned(),
            revision: *revision,
        })
    }

    async fn operate<T: Send + 'static>(
        &self,
        owner: CredentialOwner,
        action: impl FnOnce(&dyn CredentialBackend, &CredentialOwner) -> Result<T, StoreError>
        + Send
        + 'static,
    ) -> Result<T, StoreError> {
        let guard = self.state.order.clone().lock_owned().await;
        let backend = self.backend.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            action(backend.as_ref(), &owner)
        })
        .await
        .map_err(|_| StoreError::Unavailable)?
    }

    pub(crate) async fn load(
        &self,
        owner: CredentialOwner,
    ) -> Result<Option<Credential>, StoreError> {
        self.operate(owner, |backend, owner| match backend.load(owner)? {
            Some(token) => Credential::from_client_token(token)
                .map(Some)
                .ok_or(StoreError::InvalidData),
            None => Ok(None),
        })
        .await
    }

    #[cfg(test)]
    pub(crate) async fn save(
        &self,
        owner: CredentialOwner,
        credential: Credential,
    ) -> Result<(), StoreError> {
        let lease = self.capture(&owner.1).ok_or(StoreError::Unavailable)?;
        self.save_for_lease(owner, credential, lease).await
    }

    pub(crate) async fn save_for_lease(
        &self,
        owner: CredentialOwner,
        credential: Credential,
        lease: CredentialLease,
    ) -> Result<(), StoreError> {
        let state = self.clone();
        self.operate(owner, move |backend, owner| {
            if lease.endpoint != owner.1 || !state.is_current(&lease) {
                return Err(StoreError::Superseded);
            }
            let Credential::ClientToken(token) = credential else {
                return Err(StoreError::InvalidData);
            };
            // Validate without retaining an extra token copy after the write.
            let Some(Credential::ClientToken(token)) = Credential::from_client_token(token) else {
                return Err(StoreError::InvalidData);
            };
            backend.save(owner, &token)
        })
        .await
    }

    pub(crate) async fn remove(&self, owner: CredentialOwner) -> Result<(), StoreError> {
        self.operate(owner, |backend, owner| backend.remove(owner))
            .await
    }

    pub(crate) async fn forget_endpoint(&self, lease: CredentialLease) -> Result<(), StoreError> {
        let guard = self.state.order.clone().lock_owned().await;
        let backend = self.backend.clone();
        let state = self.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let result = backend.forget_endpoint(&lease.endpoint);
            state.invalidate_endpoint(&lease.endpoint)?;
            result
        })
        .await
        .map_err(|_| StoreError::Unavailable)?
    }

    #[cfg(test)]
    pub(crate) fn with_backend(backend: Arc<dyn CredentialBackend>) -> Self {
        Self {
            state: Arc::default(),
            backend,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    use super::*;

    #[derive(Default)]
    struct MemoryBackend(StdMutex<HashMap<String, String>>);

    impl CredentialBackend for MemoryBackend {
        fn load(&self, owner: &CredentialOwner) -> Result<Option<String>, StoreError> {
            Ok(self.0.lock().expect("test store").get(&owner.0).cloned())
        }

        fn save(&self, owner: &CredentialOwner, token: &str) -> Result<(), StoreError> {
            self.0
                .lock()
                .expect("test store")
                .insert(owner.0.clone(), token.to_owned());
            Ok(())
        }

        fn remove(&self, owner: &CredentialOwner) -> Result<(), StoreError> {
            self.0.lock().expect("test store").remove(&owner.0);
            Ok(())
        }
        fn forget_endpoint(&self, endpoint: &str) -> Result<(), StoreError> {
            self.0.lock().expect("test store").retain(|account, _| {
                serde_json::from_str::<[String; 2]>(account)
                    .map_or(true, |[saved, _]| saved != endpoint)
            });
            Ok(())
        }
    }

    fn identity(id: Option<&str>) -> ServerIdentity {
        ServerIdentity {
            openchamber_version: "2.5.3".into(),
            runtime: "web".into(),
            server_id: id.map(str::to_owned),
            api_version: 1,
            capabilities: vec![],
        }
    }

    #[test]
    fn ownership_requires_identity_and_keeps_endpoint_paths_distinct() {
        let first = Endpoint::parse("https://example.test/one/").expect("endpoint");
        let second = Endpoint::parse("https://example.test/two/").expect("endpoint");
        assert!(CredentialOwner::new(&first, &identity(None)).is_none());
        assert_ne!(
            CredentialOwner::new(&first, &identity(Some("server"))),
            CredentialOwner::new(&second, &identity(Some("server")))
        );
        assert_ne!(
            CredentialOwner::new(&first, &identity(Some("server"))),
            CredentialOwner::new(&first, &identity(Some("replacement")))
        );
    }

    #[test]
    fn backend_errors_cannot_disclose_secret_bytes() {
        let error = StoreError::from(keyring::Error::BadEncoding(b"private-token".to_vec()));
        assert_eq!(error, StoreError::InvalidData);
        assert!(!format!("{error:?}").contains("private-token"));
    }

    #[tokio::test]
    async fn credentials_round_trip_only_under_the_verified_owner() {
        let backend = Arc::new(MemoryBackend::default());
        let store = CredentialStore::with_backend(backend.clone());
        let endpoint = Endpoint::parse("https://example.test/").expect("endpoint");
        let owner = CredentialOwner::new(&endpoint, &identity(Some("first"))).expect("identity");
        let other = CredentialOwner::new(&endpoint, &identity(Some("second"))).expect("identity");
        let credential = Credential::ClientToken("oc_client_fixture".into());
        assert_eq!(store.load(owner.clone()).await, Ok(None));
        store
            .save(owner.clone(), credential.clone())
            .await
            .expect("save");
        assert_eq!(store.load(owner.clone()).await, Ok(Some(credential)));
        assert_eq!(store.load(other).await, Ok(None));
        store.remove(owner.clone()).await.expect("remove");
        assert_eq!(store.load(owner.clone()).await, Ok(None));
        backend
            .0
            .lock()
            .expect("store")
            .insert(owner.0.clone(), "not a token".into());
        assert_eq!(store.load(owner).await, Err(StoreError::InvalidData));
    }

    #[tokio::test]
    async fn unavailable_storage_is_an_error_without_a_fallback() {
        struct Unavailable;
        impl CredentialBackend for Unavailable {
            fn load(&self, _: &CredentialOwner) -> Result<Option<String>, StoreError> {
                Err(StoreError::Unavailable)
            }
            fn save(&self, _: &CredentialOwner, _: &str) -> Result<(), StoreError> {
                Err(StoreError::Unavailable)
            }
            fn remove(&self, _: &CredentialOwner) -> Result<(), StoreError> {
                Err(StoreError::Unavailable)
            }
            fn forget_endpoint(&self, _: &str) -> Result<(), StoreError> {
                Err(StoreError::Unavailable)
            }
        }
        let owner = CredentialOwner::new(
            &Endpoint::parse("https://example.test/").expect("endpoint"),
            &identity(Some("one")),
        )
        .expect("identity");
        let store = CredentialStore::with_backend(Arc::new(Unavailable));
        assert_eq!(
            store.load(owner.clone()).await,
            Err(StoreError::Unavailable)
        );
        assert_eq!(
            store
                .save(
                    owner.clone(),
                    Credential::ClientToken("oc_client_fixture".into())
                )
                .await,
            Err(StoreError::Unavailable)
        );
        assert_eq!(store.remove(owner).await, Err(StoreError::Unavailable));
    }

    #[tokio::test]
    async fn forgetting_an_endpoint_removes_every_identity_but_keeps_other_hosts() {
        let backend = Arc::new(MemoryBackend::default());
        let store = CredentialStore::with_backend(backend);
        let first = Endpoint::parse("https://example.test/one/").expect("endpoint");
        let second = Endpoint::parse("https://example.test/two/").expect("endpoint");
        let a = CredentialOwner::new(&first, &identity(Some("old"))).expect("identity");
        let b = CredentialOwner::new(&first, &identity(Some("new"))).expect("identity");
        let c = CredentialOwner::new(&second, &identity(Some("old"))).expect("identity");
        for owner in [a.clone(), b.clone(), c.clone()] {
            store
                .save(owner, Credential::ClientToken("oc_client_fixture".into()))
                .await
                .expect("saved");
        }
        let forget = store
            .invalidate_endpoint(first.as_str())
            .expect("invalidation");
        store.forget_endpoint(forget).await.expect("forgotten");
        assert_eq!(store.load(a).await, Ok(None));
        assert_eq!(store.load(b).await, Ok(None));
        assert!(store.load(c).await.expect("loaded").is_some());
    }

    #[tokio::test]
    async fn a_login_captured_before_forgetting_cannot_restore_saved_credentials() {
        let store = CredentialStore::with_backend(Arc::new(MemoryBackend::default()));
        let endpoint = Endpoint::parse("https://example.test/").expect("endpoint");
        let owner = CredentialOwner::new(&endpoint, &identity(Some("one"))).expect("owner");
        let login = store.capture(endpoint.as_str()).expect("lease");
        let deletion = store
            .invalidate_endpoint(endpoint.as_str())
            .expect("invalidation");
        let during_deletion = store.capture(endpoint.as_str()).expect("lease");
        store.forget_endpoint(deletion).await.expect("forgotten");
        for lease in [login, during_deletion] {
            assert_eq!(
                store
                    .save_for_lease(
                        owner.clone(),
                        Credential::ClientToken("oc_client_late".into()),
                        lease
                    )
                    .await,
                Err(StoreError::Superseded)
            );
        }
        assert_eq!(store.load(owner.clone()).await, Ok(None));
        let next = store.capture(endpoint.as_str()).expect("fresh lease");
        store
            .save_for_lease(
                owner.clone(),
                Credential::ClientToken("oc_client_new".into()),
                next,
            )
            .await
            .expect("a new explicit sign-in may save");
        assert!(store.load(owner).await.expect("loaded").is_some());
    }
}
