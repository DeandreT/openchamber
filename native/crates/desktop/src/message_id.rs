//! Client message identities using the pinned OpenCode identifier layout.

use std::collections::hash_map::RandomState;
use std::time::{SystemTime, UNIX_EPOCH};

use openchamber_native_core::ids::MessageId;

#[derive(Default)]
pub(crate) struct MessageIds {
    timestamp: u64,
    counter: u64,
}

impl MessageIds {
    pub(crate) fn next(&mut self) -> (MessageId, i64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let millis = u64::try_from(now.as_millis()).unwrap_or(u64::MAX);
        (self.at(millis), i64::try_from(millis).unwrap_or(i64::MAX))
    }

    fn at(&mut self, timestamp: u64) -> MessageId {
        use std::hash::{BuildHasher, Hasher};

        if self.timestamp != timestamp {
            self.timestamp = timestamp;
            self.counter = 0;
        }
        self.counter += 1;
        let time = timestamp.wrapping_mul(4096).wrapping_add(self.counter) & 0xffff_ffff_ffff;
        let mut id = format!("msg_{time:012x}");
        // Like install_id, use std's randomized hash seeds for uniqueness.
        // This is a correlation identifier, never a credential.
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
        MessageId::new(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_match_the_pinned_identifier_layout() {
        let mut ids = MessageIds::default();
        let first = ids.at(1_700_000_000_000);
        let second = ids.at(1_700_000_000_000);
        assert_eq!(first.as_str().len(), 30);
        assert_eq!(&first.as_str()[4..16], "bcfe56800001");
        assert_eq!(&second.as_str()[4..16], "bcfe56800002");
        assert!(
            first.as_str()[16..]
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric())
        );
        assert_ne!(first, second);
    }
}
