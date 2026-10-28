//! A stable identifier for this installation.
//!
//! A server keeps one device record per install: logging in again replaces
//! the record whose dedupe key carries this ID instead of adding another.
//! The ID identifies nothing about the user and is not a credential.

use std::collections::hash_map::RandomState;
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;

const FILE_NAME: &str = "install-id";
const LENGTH: usize = 32;

/// The install ID, created on first use. `None` when no configuration
/// directory is writable; logins then carry no dedupe key.
pub fn load_or_create() -> Option<String> {
    let path = config_directory()?.join(FILE_NAME);
    if let Ok(existing) = fs::read_to_string(&path) {
        let existing = existing.trim();
        if is_valid(existing) {
            return Some(existing.to_owned());
        }
    }
    let id = generate();
    fs::create_dir_all(path.parent()?).ok()?;
    // Write then rename, so a crash never leaves a partial ID behind.
    let partial = path.with_extension("partial");
    fs::write(&partial, &id).ok()?;
    fs::rename(&partial, &path).ok()?;
    Some(id)
}

/// The per-user configuration directory of the native desktop.
pub(crate) fn config_directory() -> Option<PathBuf> {
    let directory = if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?).join("OpenChamber Native")
    } else if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?)
            .join("Library")
            .join("Application Support")
            .join("OpenChamber Native")
    } else {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        base.join("openchamber-native")
    };
    Some(directory)
}

fn is_valid(id: &str) -> bool {
    id.len() == LENGTH && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// 128 bits from the standard library's per-process random hasher keys.
fn generate() -> String {
    let mut id = String::with_capacity(LENGTH);
    for half in 0u8..2 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u8(half);
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default(),
        );
        id.push_str(&format!("{:016x}", hasher.finish()));
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_valid_and_distinct() {
        let first = generate();
        let second = generate();
        assert!(is_valid(&first), "{first} is 32 hexadecimal digits");
        assert_ne!(first, second);
        assert!(!is_valid("not-an-id"));
    }
}
