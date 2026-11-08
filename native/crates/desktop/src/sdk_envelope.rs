//! The host half of the extension SDK's frame protocol
//! (`packages/sdk/src/protocol.ts`, `api-version.ts`).
//!
//! An extension panel posts `{ channel, v, type, id?, payload? }` to its
//! parent and waits for a `result` with the same `id`. The native host
//! answers the `hello` handshake with `ready` and every request with an
//! explicit `UNSUPPORTED` failure: it implements none of the host services
//! yet, so nothing an extension asks for reaches the workspace, the server
//! or the machine. The guest reads a failed result as a rejected call.

use serde_json::{Value, json};

pub(crate) const CHANNEL: &str = "openchamber.sdk";
pub(crate) const VERSION: u64 = 1;

/// A message an extension frame posted.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum GuestMessage {
    /// The guest connected and waits for `ready`.
    Hello,
    /// A request that expects a `result`.
    Request { kind: String, id: String },
    /// A notice that expects no answer, such as `badge`.
    Notice,
}

/// Reads an extension frame's message: the envelope and discriminant only.
/// Anything without the SDK's channel and version is not one.
pub(crate) fn read_guest_message(message: &Value) -> Option<GuestMessage> {
    if message.get("channel")?.as_str()? != CHANNEL || message.get("v")?.as_u64()? != VERSION {
        return None;
    }
    let kind = message.get("type")?.as_str()?;
    if kind == "hello" {
        return Some(GuestMessage::Hello);
    }
    match message
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        Some(id) => Some(GuestMessage::Request {
            kind: kind.to_owned(),
            id: id.to_owned(),
        }),
        None => Some(GuestMessage::Notice),
    }
}

/// The colours, fonts and radius an extension draws with (`ready.theme`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GuestTheme {
    pub dark: bool,
    /// Every required token of `themeTokensSchema`, as CSS values.
    pub tokens: Vec<(&'static str, String)>,
}

/// What `ready` tells a panel about where it runs.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ReadyContext {
    pub theme: GuestTheme,
    pub locale: String,
    pub directory: Option<String>,
    pub connected: bool,
}

pub(crate) fn ready_message(context: &ReadyContext) -> Value {
    let tokens: serde_json::Map<String, Value> = context
        .theme
        .tokens
        .iter()
        .map(|(name, value)| ((*name).to_owned(), Value::String(value.clone())))
        .collect();
    json!({
        "channel": CHANNEL,
        "v": VERSION,
        "type": "ready",
        "payload": {
            "theme": { "mode": if context.theme.dark { "dark" } else { "light" }, "tokens": tokens },
            "locale": context.locale,
            "directory": context.directory,
            "session": null,
            "surface": "panel",
            // Access tokens never appear here.
            "connection": { "connected": context.connected, "account": "" },
            "settings": {},
            "item": null,
        },
    })
}

/// The failed `result` of a request the native host does not provide.
pub(crate) fn unsupported_result(id: &str, kind: &str) -> Value {
    json!({
        "channel": CHANNEL,
        "v": VERSION,
        "type": "result",
        "id": id,
        "ok": false,
        "error": format!("{kind} is not available in this OpenChamber client"),
        "code": "UNSUPPORTED",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_only_the_sdk_envelope() {
        assert_eq!(
            read_guest_message(&json!({ "channel": CHANNEL, "v": 1, "type": "hello" })),
            Some(GuestMessage::Hello)
        );
        assert_eq!(
            read_guest_message(
                &json!({ "channel": CHANNEL, "v": 1, "type": "compose", "id": "r1", "payload": { "text": "x" } })
            ),
            Some(GuestMessage::Request {
                kind: "compose".to_owned(),
                id: "r1".to_owned()
            })
        );
        assert_eq!(
            read_guest_message(
                &json!({ "channel": CHANNEL, "v": 1, "type": "badge", "payload": { "count": 2 } })
            ),
            Some(GuestMessage::Notice)
        );
        for other in [
            json!({ "channel": CHANNEL, "v": 2, "type": "hello" }),
            json!({ "channel": "other", "v": 1, "type": "hello" }),
            json!("hello"),
        ] {
            assert!(read_guest_message(&other).is_none(), "{other}");
        }
    }

    #[test]
    fn ready_carries_no_credential_and_failures_are_explicit() {
        let ready = ready_message(&ReadyContext {
            theme: GuestTheme {
                dark: true,
                tokens: vec![("background", "#000".to_owned())],
            },
            locale: "en".to_owned(),
            directory: Some("/work".to_owned()),
            connected: true,
        });
        assert_eq!(
            ready["payload"]["connection"],
            json!({ "connected": true, "account": "" })
        );
        assert_eq!(ready["payload"]["theme"]["mode"], "dark");
        let text = ready.to_string();
        assert!(!text.contains("token\":") && !text.contains("Bearer"));
        let failed = unsupported_result("r1", "compose");
        assert_eq!(failed["ok"], false);
        assert_eq!(failed["code"], "UNSUPPORTED");
        assert_eq!(failed["id"], "r1");
    }
}
