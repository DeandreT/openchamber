//! App-local, non-secret appearance snapshots with locked compare-and-swap writes.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::appearance::AppearancePreferences;

const MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppearanceError {
    Unavailable,
    Invalid,
    Changed,
    Locked,
}

impl From<std::io::Error> for AppearanceError {
    fn from(_: std::io::Error) -> Self {
        Self::Unavailable
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct AppearanceSnapshot {
    version: u32,
    revision: u64,
    pub(crate) preferences: AppearancePreferences,
}

impl AppearanceSnapshot {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, AppearanceError> {
        if bytes.len() as u64 > MAX_BYTES {
            return Err(AppearanceError::Invalid);
        }
        let snapshot: Self = serde_json::from_slice(bytes).map_err(|_| AppearanceError::Invalid)?;
        if snapshot.version != 1 {
            return Err(AppearanceError::Invalid);
        }
        snapshot.preferences.validate()?;
        Ok(snapshot)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AppearanceStore {
    path: PathBuf,
}

impl AppearanceStore {
    pub(crate) fn new(directory: &Path) -> Self {
        Self {
            path: directory.join("appearance.json"),
        }
    }

    /// Missing grants first-write authority; malformed and unreadable files do not.
    pub(crate) fn load(&self) -> Result<Option<AppearanceSnapshot>, AppearanceError> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !file.metadata()?.is_file() {
            return Err(AppearanceError::Invalid);
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        AppearanceSnapshot::parse(&bytes).map(Some)
    }

    pub(crate) fn save(
        &self,
        expected: Option<&AppearanceSnapshot>,
        preferences: &AppearancePreferences,
    ) -> Result<AppearanceSnapshot, AppearanceError> {
        preferences.validate()?;
        let parent = self.path.parent().ok_or(AppearanceError::Unavailable)?;
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
            .open(parent.join("appearance.lock"))?;
        lock.try_lock().map_err(|_| AppearanceError::Locked)?;
        if self.load()?.as_ref() != expected {
            return Err(AppearanceError::Changed);
        }
        let next = AppearanceSnapshot {
            version: 1,
            revision: expected
                .map_or(Some(1), |old| old.revision.checked_add(1))
                .ok_or(AppearanceError::Invalid)?,
            preferences: preferences.clone(),
        };
        let bytes = encode_snapshot(&next, MAX_BYTES as usize)?;
        AppearanceSnapshot::parse(&bytes)?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| AppearanceError::Unavailable)?
            .as_nanos();
        let temporary = parent.join(format!(
            "appearance.json.partial-{}-{nonce}",
            std::process::id()
        ));
        let mut output = private_options().create_new(true).open(&temporary)?;
        let result = (|| {
            output.write_all(&bytes)?;
            output.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            #[cfg(unix)]
            File::open(parent)?.sync_all()?;
            Ok::<_, std::io::Error>(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        Ok(next)
    }
}

/// Refuse oversized encoded output before opening a temporary file.
fn encode_snapshot(
    snapshot: &AppearanceSnapshot,
    limit: usize,
) -> Result<Vec<u8>, AppearanceError> {
    let mut output = SnapshotBuffer {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer_pretty(&mut output, snapshot).map_err(|_| AppearanceError::Invalid)?;
    Ok(output.bytes)
}

struct SnapshotBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for SnapshotBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "appearance snapshot exceeds its encoded size limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
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
    fn byte_budget_includes_json_escaping_and_pretty_formatting() {
        let snapshot = AppearanceSnapshot {
            version: 1,
            revision: 1,
            preferences: AppearancePreferences {
                dark_theme_id: "custom-\"🎨\"".into(),
                ..Default::default()
            },
        };
        let encoded = serde_json::to_vec_pretty(&snapshot).expect("v1 JSON");
        assert_eq!(
            encode_snapshot(&snapshot, encoded.len() - 1),
            Err(AppearanceError::Invalid)
        );
        assert_eq!(encode_snapshot(&snapshot, encoded.len()), Ok(encoded));
    }

    #[test]
    fn saves_valid_runtime_selections_that_together_exceed_16_kib() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("appearance-scopes-{}-{nonce}", std::process::id()));
        let store = AppearanceStore::new(&directory);
        let scope = serde_json::json!({
            "mode": "dark", "lightThemeId": "openchamber-light", "darkThemeId": "imported-dark"
        });
        let scopes: serde_json::Map<_, _> = (0..160)
            .map(|i| (format!("https://runtime-{i}.example.test"), scope.clone()))
            .collect();
        let preferences = AppearancePreferences {
            runtime_themes: serde_json::from_value(scopes.into()).expect("valid runtime map"),
            ..Default::default()
        };
        let saved = store.save(None, &preferences).expect("save all runtimes");
        assert!(fs::metadata(&store.path).expect("written file").len() > 16 * 1024);
        assert_eq!(saved.preferences, preferences);
        assert_eq!(AppearanceStore::new(&directory).load(), Ok(Some(saved)));
        fs::remove_dir_all(directory).expect("remove owned fixture");
    }

    #[test]
    fn validated_v1_roundtrip_rejects_future_versions_and_invalid_fields() {
        let snapshot = AppearanceSnapshot {
            version: 1,
            revision: 1,
            preferences: AppearancePreferences::default(),
        };
        let bytes = serde_json::to_vec(&snapshot).expect("serialize preferences");
        assert_eq!(AppearanceSnapshot::parse(&bytes), Ok(snapshot));
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
        for (pointer, bad) in [
            ("/version", serde_json::json!(2)),
            ("/preferences/mode", serde_json::json!("auto")),
            ("/preferences/locale", serde_json::json!("xx")),
            (
                "/preferences/lightThemeId",
                serde_json::json!("openchamber-dark"),
            ),
            ("/preferences/fontSize", serde_json::json!(201)),
            ("/preferences/uiFont", serde_json::json!("unregistered")),
        ] {
            let mut invalid = value.clone();
            *invalid.pointer_mut(pointer).expect("existing field") = bad;
            assert_eq!(
                AppearanceSnapshot::parse(&serde_json::to_vec(&invalid).expect("JSON")),
                Err(AppearanceError::Invalid),
                "reject {pointer}"
            );
        }
        assert_eq!(
            AppearanceSnapshot::parse(b"{}"),
            Err(AppearanceError::Invalid)
        );
    }

    #[test]
    fn cas_distinguishes_missing_and_refuses_conflicts_and_invalid_snapshots() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("appearance-test-{}-{nonce}", std::process::id()));
        let store = AppearanceStore::new(&directory);
        assert_eq!(store.load(), Ok(None));
        let mut draft = AppearancePreferences::default();
        let saved = store.save(None, &draft).expect("first save");
        assert_eq!(store.load(), Ok(Some(saved.clone())));
        draft.locale = "ar".to_owned();
        assert_eq!(store.save(None, &draft), Err(AppearanceError::Changed));
        assert_eq!(draft.locale, "ar", "CAS failure preserves caller edits");
        assert_eq!(store.load(), Ok(Some(saved.clone())));
        let second = store
            .save(Some(&saved), &draft)
            .expect("ordered second save");
        assert_eq!(second.revision, 2);
        fs::write(&store.path, b"invalid snapshot").expect("write invalid fixture");
        assert_eq!(
            store.save(Some(&second), &draft),
            Err(AppearanceError::Invalid)
        );
        assert_eq!(
            fs::read(&store.path).expect("read fixture"),
            b"invalid snapshot"
        );
        fs::remove_dir_all(directory).expect("remove owned fixture");
    }
}
