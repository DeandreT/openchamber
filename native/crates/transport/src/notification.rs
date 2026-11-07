//! `openchamber:notification`: a notification the server raised for this
//! user (a finished turn, an error, a question, a plugin's notice). The
//! server already applied the event toggles and the master switch
//! (`lib/notifications/runtime.js`); the client decides only whether the
//! window's focus allows it and how to show it (`handleUiNotificationEvent`).

use openchamber_native_core::ids::SessionId;
use serde::Serialize;
use serde_json::Value;

/// One notification as `event-pipeline.ts` validates it: every field is
/// optional, and a present field of the wrong type rejects the whole event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiNotification {
    pub kind: Option<String>,
    pub session_id: Option<SessionId>,
    pub directory: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub tag: Option<String>,
    /// Shown only while no window of the app has focus.
    pub require_hidden: bool,
    /// A plugin's notice that also shows while the app is in front.
    pub show_when_focused: bool,
}

/// Decodes the event's `properties`; `None` when a field has the wrong type.
pub(crate) fn decode(properties: &Value) -> Option<UiNotification> {
    let properties = properties.as_object()?;
    let text = |key: &str| -> Result<Option<String>, ()> {
        match properties.get(key) {
            None => Ok(None),
            Some(Value::String(value)) => {
                let trimmed = value.trim();
                Ok((!trimmed.is_empty()).then(|| trimmed.to_owned()))
            }
            Some(_) => Err(()),
        }
    };
    let flag = |key: &str| -> Result<bool, ()> {
        match properties.get(key) {
            None => Ok(false),
            Some(Value::Bool(value)) => Ok(*value),
            Some(_) => Err(()),
        }
    };
    // The local-shell markers only matter to a client of a local server,
    // which this one never is; they are validated like the other fields.
    flag("desktopNotificationDelivered").ok()?;
    flag("desktopStdoutActive").ok()?;
    let kind = text("kind").ok()?;
    let plugin = kind.as_deref() == Some("plugin");
    Some(UiNotification {
        session_id: text("sessionId").ok()?.map(SessionId::from),
        directory: text("directory").ok()?,
        title: text("title").ok()?,
        body: text("body").ok()?,
        tag: text("tag").ok()?,
        // A plugin's notification never interrupts a focused window natively.
        require_hidden: plugin || flag("requireHidden").ok()?,
        show_when_focused: flag("showWhenFocused").ok()?,
        kind,
    })
}

/// One event's custom text; an empty field uses the server's default for
/// the interface language.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct NotificationTemplate {
    pub title: String,
    pub message: String,
}

/// `notificationTemplates`, written whole as the web client writes it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct NotificationTemplates {
    pub completion: NotificationTemplate,
    pub error: NotificationTemplate,
    pub question: NotificationTemplate,
    pub subtask: NotificationTemplate,
}

impl NotificationTemplates {
    /// `parseNotificationTemplates`: an object with at least one valid event;
    /// a missing event takes the empty default, a non-string field is empty.
    pub(crate) fn parse(value: &Value) -> Option<Self> {
        let record = value.as_object()?;
        let read = |key: &str| {
            let entry = record.get(key)?.as_object()?;
            let text = |field: &str| {
                entry
                    .get(field)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            Some(NotificationTemplate {
                title: text("title"),
                message: text("message"),
            })
        };
        let (completion, error, question, subtask) = (
            read("completion"),
            read("error"),
            read("question"),
            read("subtask"),
        );
        if completion.is_none() && error.is_none() && question.is_none() && subtask.is_none() {
            return None;
        }
        Some(Self {
            completion: completion.unwrap_or_default(),
            error: error.unwrap_or_default(),
            question: question.unwrap_or_default(),
            subtask: subtask.unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn decodes_trimmed_fields_and_treats_plugins_as_background_only() {
        let notification = decode(&json!({
            "kind": "completion",
            "sessionId": " ses_1 ",
            "directory": "/repo",
            "title": "Done",
            "body": "",
            "tag": "t1",
            "requireHidden": true,
            "desktopNotificationDelivered": false,
        }))
        .expect("valid notification");
        assert_eq!(notification.session_id, Some(SessionId::from("ses_1")));
        assert_eq!(notification.body, None, "an empty body is no body");
        assert!(notification.require_hidden);
        assert!(!notification.show_when_focused);

        let plugin = decode(&json!({"kind": "plugin", "title": "Build", "requireHidden": false}))
            .expect("plugin notice");
        assert!(
            plugin.require_hidden,
            "the web client forces plugins hidden"
        );
    }

    #[test]
    fn templates_parse_like_the_settings_registry() {
        assert_eq!(NotificationTemplates::parse(&json!({})), None);
        assert_eq!(NotificationTemplates::parse(&json!({"other": {}})), None);
        let parsed = NotificationTemplates::parse(&json!({
            "error": {"title": "Failed {session_name}", "message": 4},
        }))
        .expect("one valid event");
        assert_eq!(parsed.error.title, "Failed {session_name}");
        assert_eq!(parsed.error.message, "", "a non-string field is empty");
        assert_eq!(parsed.completion, NotificationTemplate::default());
        assert_eq!(
            serde_json::to_value(&parsed).expect("serializes")["question"],
            json!({"title": "", "message": ""})
        );
    }

    #[test]
    fn a_field_of_the_wrong_type_rejects_the_event() {
        assert_eq!(decode(&json!({"title": 3})), None);
        assert_eq!(decode(&json!({"requireHidden": "yes"})), None);
        assert_eq!(decode(&json!({"desktopStdoutActive": 1})), None);
        assert_eq!(decode(&json!("text")), None);
        assert_eq!(decode(&json!({})), Some(UiNotification::default()));
    }
}
