//! Non-secret native host profiles with conflict-checked atomic persistence.
//!
//! Tokens belong to `credential_store`; passwords and request headers are never
//! accepted in this format. A malformed profile file grants no write authority.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use openchamber_native_transport::endpoint::Endpoint;
use serde::{Deserialize, Serialize};

const MAX_BYTES: u64 = 512 * 1024;
const MAX_HOSTS: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostProfile {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) endpoint: Endpoint,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HostProfiles {
    pub(crate) revision: u64,
    pub(crate) hosts: Vec<HostProfile>,
    pub(crate) default_host: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProfilesError {
    Unavailable,
    Invalid,
    InvalidLabel,
    InvalidEndpoint,
    Duplicate,
    Limit,
    Changed,
    Locked,
}

impl From<std::io::Error> for ProfilesError {
    fn from(_: std::io::Error) -> Self {
        Self::Unavailable
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ProfileStore {
    path: PathBuf,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Document {
    version: u32,
    revision: u64,
    hosts: Vec<StoredHost>,
    default_host: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredHost {
    id: String,
    label: String,
    url: String,
}

impl HostProfiles {
    pub(crate) fn default_profile(&self) -> Option<&HostProfile> {
        let id = self.default_host.as_ref()?;
        self.hosts.iter().find(|host| &host.id == id)
    }

    pub(crate) fn upsert(&mut self, id: &str, label: &str, url: &str) -> Result<(), ProfilesError> {
        let profile = HostProfile::parse(id, label, url)?;
        if self
            .hosts
            .iter()
            .any(|host| host.id != id && host.endpoint == profile.endpoint)
        {
            return Err(ProfilesError::Duplicate);
        }
        if let Some(existing) = self.hosts.iter_mut().find(|host| host.id == id) {
            *existing = profile;
        } else {
            if self.hosts.len() >= MAX_HOSTS {
                return Err(ProfilesError::Limit);
            }
            self.hosts.push(profile);
        }
        Ok(())
    }

    pub(crate) fn remove(&mut self, id: &str) {
        self.hosts.retain(|host| host.id != id);
        if self.default_host.as_deref() == Some(id) {
            self.default_host = None;
        }
    }

    pub(crate) fn set_default(&mut self, id: Option<&str>) -> Result<(), ProfilesError> {
        if id.is_some_and(|id| !self.hosts.iter().any(|host| host.id == id)) {
            return Err(ProfilesError::Invalid);
        }
        self.default_host = id.map(str::to_owned);
        Ok(())
    }

    fn document(&self) -> Document {
        Document {
            version: 1,
            revision: self.revision,
            default_host: self.default_host.clone(),
            hosts: self
                .hosts
                .iter()
                .map(|host| StoredHost {
                    id: host.id.clone(),
                    label: host.label.clone(),
                    url: host.endpoint.as_str().to_owned(),
                })
                .collect(),
        }
    }

    fn parse(bytes: &[u8]) -> Result<Self, ProfilesError> {
        let document: Document =
            serde_json::from_slice(bytes).map_err(|_| ProfilesError::Invalid)?;
        if document.version != 1 || document.hosts.len() > MAX_HOSTS {
            return Err(ProfilesError::Invalid);
        }
        let mut result = Self {
            revision: document.revision,
            ..Self::default()
        };
        for host in document.hosts {
            if result.hosts.iter().any(|saved| saved.id == host.id) {
                return Err(ProfilesError::Invalid);
            }
            result
                .upsert(&host.id, &host.label, &host.url)
                .map_err(|_| ProfilesError::Invalid)?;
        }
        result.set_default(document.default_host.as_deref())?;
        Ok(result)
    }
}

impl HostProfile {
    fn parse(id: &str, label: &str, url: &str) -> Result<Self, ProfilesError> {
        let label = label.trim();
        if !id.starts_with("host_")
            || id.len() > 64
            || id.len() <= 5
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(ProfilesError::Invalid);
        }
        if label.is_empty() || label.chars().count() > 256 || label.chars().any(char::is_control) {
            return Err(ProfilesError::InvalidLabel);
        }
        if url.len() > 2048 {
            return Err(ProfilesError::InvalidEndpoint);
        }
        Ok(Self {
            id: id.to_owned(),
            label: label.to_owned(),
            endpoint: Endpoint::parse(url).map_err(|_| ProfilesError::InvalidEndpoint)?,
        })
    }
}

impl ProfileStore {
    pub(crate) fn new(directory: &Path) -> Self {
        Self {
            path: directory.join("hosts.json"),
        }
    }

    pub(crate) fn load(&self) -> Result<HostProfiles, ProfilesError> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(HostProfiles::default());
            }
            Err(error) => return Err(error.into()),
        };
        if !file.metadata()?.is_file() {
            return Err(ProfilesError::Invalid);
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(ProfilesError::Invalid);
        }
        HostProfiles::parse(&bytes)
    }

    /// Reload under an exclusive lock and reject another window's newer edit.
    pub(crate) fn save(
        &self,
        expected: &HostProfiles,
        next: &HostProfiles,
    ) -> Result<HostProfiles, ProfilesError> {
        let parent = self.path.parent().ok_or(ProfilesError::Unavailable)?;
        let mut directories = fs::DirBuilder::new();
        directories.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directories.mode(0o700);
        }
        directories.create(parent)?;
        let lock = private_options()
            .create(true)
            .truncate(false)
            .open(parent.join("hosts.lock"))?;
        lock.try_lock().map_err(|_| ProfilesError::Locked)?;
        if self.load()? != *expected {
            return Err(ProfilesError::Changed);
        }
        let mut next = next.clone();
        next.revision = expected
            .revision
            .checked_add(1)
            .ok_or(ProfilesError::Invalid)?;
        let bytes =
            serde_json::to_vec_pretty(&next.document()).map_err(|_| ProfilesError::Invalid)?;
        // Validate again at the write boundary, including callers that edited fields.
        if bytes.len() as u64 > MAX_BYTES || HostProfiles::parse(&bytes)? != next {
            return Err(ProfilesError::Invalid);
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ProfilesError::Unavailable)?
            .as_nanos();
        let temporary = parent.join(format!("hosts.json.partial-{}-{nonce}", std::process::id()));
        let mut output = private_options().create_new(true).open(&temporary)?;
        let written = (|| {
            output.write_all(&bytes)?;
            output.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            #[cfg(unix)]
            File::open(parent)?.sync_all()?;
            Ok::<_, std::io::Error>(())
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        written?;
        Ok(next)
    }
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_reject_credentials_unknown_fields_and_unowned_defaults() {
        for bad in [
            r#"{"version":1,"revision":0,"hosts":[],"defaultHost":"host_missing"}"#,
            r#"{"version":2,"revision":0,"hosts":[],"defaultHost":null}"#,
            r#"{"version":1,"revision":0,"hosts":[{"id":"host_1","label":"A","url":"http://u:p@localhost"}],"defaultHost":null}"#,
            r#"{"version":1,"revision":0,"hosts":[{"id":"host_1","label":"A","url":"http://localhost","clientToken":"secret"}],"defaultHost":null}"#,
        ] {
            assert_eq!(
                HostProfiles::parse(bad.as_bytes()),
                Err(ProfilesError::Invalid)
            );
        }
    }

    #[test]
    fn normalized_endpoints_are_unique_and_deletion_clears_only_its_default() {
        let mut profiles = HostProfiles::default();
        profiles
            .upsert("host_1", " A ", "https://one.test/prefix")
            .expect("valid profile");
        assert_eq!(
            profiles.upsert("host_2", "B", "https://one.test/prefix/"),
            Err(ProfilesError::Duplicate)
        );
        profiles
            .upsert("host_2", "B", "https://two.test/")
            .expect("valid profile");
        profiles.set_default(Some("host_1")).expect("listed host");
        profiles.remove("host_2");
        assert_eq!(
            profiles.default_profile().map(|host| host.label.as_str()),
            Some("A")
        );
        profiles.remove("host_1");
        assert!(profiles.default_host.is_none());
    }

    #[test]
    fn atomic_saves_preserve_other_windows_and_refuse_malformed_files() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = PathBuf::from("/tmp/opencode").join(format!(
            "native-host-profiles-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("test directory");
        let store = ProfileStore::new(&directory);
        let empty = store.load().expect("absent profile is empty");
        let mut draft = empty.clone();
        draft
            .upsert("host_one", "One", "https://one.test/api/")
            .expect("profile");
        let saved = store.save(&empty, &draft).expect("initial save");
        assert_eq!(store.load(), Ok(saved.clone()));
        draft
            .upsert("host_two", "Two", "https://two.test/")
            .expect("profile");
        assert_eq!(store.save(&empty, &draft), Err(ProfilesError::Changed));
        assert_eq!(store.load(), Ok(saved));
        fs::write(directory.join("hosts.json"), b"broken document")
            .expect("write malformed fixture");
        assert_eq!(store.save(&empty, &draft), Err(ProfilesError::Invalid));
        assert_eq!(
            fs::read(directory.join("hosts.json")).expect("fixture preserved"),
            b"broken document"
        );
        fs::remove_dir_all(directory).expect("remove owned test directory");
    }
}
