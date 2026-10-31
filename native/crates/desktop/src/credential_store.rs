//! OS-backed client-token storage. No token is written to application settings.

use std::fmt;
use std::sync::Arc;

use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::endpoint::Endpoint;
use openchamber_native_transport::identity::ServerIdentity;
use tokio::sync::Mutex;

const SERVICE: &str = "ai.openchamber.native";

/// A token belongs to the canonical endpoint and the identity it proved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CredentialOwner(String);

impl CredentialOwner {
    pub(crate) fn new(endpoint: &Endpoint, identity: &ServerIdentity) -> Option<Self> {
        let server = identity
            .server_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())?;
        let name = serde_json::to_string(&[endpoint.as_str(), server])
            .expect("strings serialize as a JSON array");
        Some(Self(name))
    }
}

/// Errors are intentionally detached from backend errors, which may contain
/// secret bytes in BadEncoding, BadDataFormat or ambiguous-entry diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StoreError {
    Unavailable,
    InvalidData,
    Ambiguous,
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
}

/// One ordered worker boundary for reads, saves and deletion. The mutex guard
/// moves into the blocking task, so cancellation cannot let a newer write pass it.
#[derive(Clone)]
pub(crate) struct CredentialStore {
    order: Arc<Mutex<()>>,
    backend: Arc<dyn CredentialBackend>,
}

impl Default for CredentialStore {
    fn default() -> Self {
        Self {
            order: Arc::default(),
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
    async fn operate<T: Send + 'static>(
        &self,
        owner: CredentialOwner,
        action: impl FnOnce(&dyn CredentialBackend, &CredentialOwner) -> Result<T, StoreError>
        + Send
        + 'static,
    ) -> Result<T, StoreError> {
        let guard = self.order.clone().lock_owned().await;
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

    pub(crate) async fn save(
        &self,
        owner: CredentialOwner,
        credential: Credential,
    ) -> Result<(), StoreError> {
        self.operate(owner, move |backend, owner| {
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

    #[cfg(test)]
    pub(crate) fn with_backend(backend: Arc<dyn CredentialBackend>) -> Self {
        Self {
            order: Arc::default(),
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
}
