//! Descending session identities matching @opencode/schema 2.2.0.

use std::collections::hash_map::RandomState;
use std::time::{SystemTime, UNIX_EPOCH};

use openchamber_native_core::ids::SessionId;

#[derive(Default)]
pub(crate) struct SessionIds {
    timestamp: u64,
    counter: u64,
}

impl SessionIds {
    pub(crate) fn next(&mut self) -> SessionId {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        self.at(u64::try_from(now.as_millis()).unwrap_or(u64::MAX))
    }

    fn at(&mut self, timestamp: u64) -> SessionId {
        use std::hash::{BuildHasher, Hasher};

        if self.timestamp != timestamp {
            self.timestamp = timestamp;
            self.counter = 0;
        }
        self.counter += 1;
        let current = timestamp.wrapping_mul(4096).wrapping_add(self.counter);
        let time = !current & 0xffff_ffff_ffff;
        let mut id = format!("ses_{time:012x}");
        // Matches message_id's randomized uniqueness suffix. IDs confer no authority.
        const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
        for half in 0..2 {
            let mut hasher = RandomState::new().build_hasher();
            hasher.write_u64(timestamp);
            hasher.write_u64(self.counter);
            hasher.write_u8(half);
            for byte in hasher.finish().to_le_bytes().iter().take(7) {
                id.push(char::from(CHARS[usize::from(*byte) % CHARS.len()]));
            }
        }
        SessionId::new(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_match_2_2_descending_unix_millisecond_layout() {
        let mut ids = SessionIds::default();
        let first = ids.at(1_700_000_000_000);
        let second = ids.at(1_700_000_000_000);
        let later = ids.at(1_700_000_000_001);
        assert_eq!(&first.as_str()[4..16], "4301a97ffffe");
        assert_eq!(&second.as_str()[4..16], "4301a97ffffd");
        assert_eq!(&later.as_str()[4..16], "4301a97feffe");
        assert!(later.as_str() < second.as_str());
        assert!(second.as_str() < first.as_str());
        assert_eq!(first.as_str().len(), 30);
        assert!(
            first.as_str()[16..]
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric())
        );
    }

    #[test]
    fn session_ids_use_current_epoch_and_randomize_across_generators() {
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Unix epoch")
            .as_millis();
        let id = SessionIds::default().next();
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Unix epoch")
            .as_millis();
        let prefix = u64::from_str_radix(&id.as_str()[4..16], 16).expect("hex prefix");
        // The protocol wraps the timestamp to 36 bits, not a custom epoch.
        let decoded = (!prefix & 0xffff_ffff_ffff) >> 12;
        assert!((before..=after).any(|millis| (millis as u64 & 0xf_ffff_ffff) == decoded));
        assert_ne!(
            SessionIds::default().at(1_793_209_852_000),
            SessionIds::default().at(1_793_209_852_000)
        );
    }
}
