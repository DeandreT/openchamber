//! Desktop notifications, as Electron's `maybeShowNativeNotification` shows
//! them: the server's `openchamber:notification` events, shown through the
//! freedesktop notification service (`org.freedesktop.Notifications` on the
//! session bus). A click brings the window forward and opens the session on
//! the runtime that raised it.
//!
//! The server owns which events notify (the master switch and the event
//! toggles); this module owns the window's side: `requireHidden` while the
//! window has focus, the five-second duplicate claim, the title fallback and
//! the click's owner.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui_kit::component::WindowExt;
use gpui_kit::component::notification::Notification;
use gpui_kit::{Context, Task, Window};
use openchamber_native_core::ids::{RuntimeId, SessionId};
use openchamber_native_transport::endpoint::Endpoint;
use openchamber_native_transport::notification::UiNotification;
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::zvariant::Value;

use crate::locale;
use crate::workspace::Workspace;

/// `NATIVE_NOTIFICATION_DEDUPE_TTL_MS`.
const CLAIM_TTL: Duration = Duration::from_secs(5);
const SERVICE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const DEFAULT_ACTION: &str = "default";

/// What a notification shows. The text is the server's, as Electron's
/// desktop branch shows it; nothing here logs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Shown {
    pub(crate) title: String,
    pub(crate) body: String,
    /// The label of the click action, for servers that draw actions.
    pub(crate) action: String,
}

/// The session a click opens, and the runtime that owns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) runtime: RuntimeId,
    pub(crate) endpoint: Option<Endpoint>,
    pub(crate) session: SessionId,
}

/// `claimNativeNotification`: one notification per key within the window.
#[derive(Debug, Default)]
pub(crate) struct Claims {
    claimed: HashMap<String, Instant>,
}

impl Claims {
    /// The tag, else the session, kind, title and body joined.
    fn key(notification: &UiNotification) -> String {
        if let Some(tag) = &notification.tag {
            return tag.clone();
        }
        [
            notification.session_id.as_ref().map(SessionId::as_str),
            notification.kind.as_deref(),
            notification.title.as_deref(),
            notification.body.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("|")
    }

    pub(crate) fn claim(&mut self, notification: &UiNotification, now: Instant) -> bool {
        let key = Self::key(notification);
        if key.is_empty() {
            return true;
        }
        self.claimed
            .retain(|_, claimed| now.duration_since(*claimed) <= CLAIM_TTL);
        if self.claimed.contains_key(&key) {
            return false;
        }
        self.claimed.insert(key, now);
        true
    }
}

/// What the window does with a notification event.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// Nothing: the window has focus and the notification is background-only,
    /// or the same notification was shown moments ago.
    Skip,
    Show(Shown),
}

/// `maybeShowNativeNotification`'s decision, apart from the toast a focused
/// window shows for a plugin's `showWhenFocused` notice.
pub(crate) fn delivery(
    notification: &UiNotification,
    focused: bool,
    claims: &mut Claims,
    now: Instant,
    action: &str,
) -> Delivery {
    if notification.require_hidden && focused {
        return Delivery::Skip;
    }
    if !claims.claim(notification, now) {
        return Delivery::Skip;
    }
    Delivery::Show(Shown {
        title: notification
            .title
            .clone()
            .unwrap_or_else(|| "OpenChamber".to_owned()),
        body: notification.body.clone().unwrap_or_default(),
        action: action.to_owned(),
    })
}

/// An event from the notification service about one of our notifications.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServiceEvent {
    Clicked(u32),
    Closed(u32),
}

/// One connection to the notification service: a thread that sends
/// `Notify` calls and a thread that reads its signals.
#[derive(Debug)]
pub(crate) struct Notifier {
    requests: std::sync::mpsc::Sender<(Shown, tokio::sync::oneshot::Sender<Option<u32>>)>,
}

impl Notifier {
    /// Connects through `connection`; events arrive on the returned channel.
    pub(crate) fn start(
        connection: Connection,
    ) -> zbus::Result<(Self, tokio::sync::mpsc::UnboundedReceiver<ServiceEvent>)> {
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(SERVICE)?
            .path(PATH)?
            .build();
        let signals = MessageIterator::for_match_rule(rule, &connection, None)?;
        let (events, received) = tokio::sync::mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("notification-signals".into())
            .spawn(move || {
                for message in signals {
                    let Ok(message) = message else { continue };
                    let header = message.header();
                    let event = match header.member().map(|member| member.as_str()) {
                        Some("ActionInvoked") => message
                            .body()
                            .deserialize::<(u32, String)>()
                            .ok()
                            .filter(|(_, action)| action == DEFAULT_ACTION)
                            .map(|(id, _)| ServiceEvent::Clicked(id)),
                        Some("NotificationClosed") => message
                            .body()
                            .deserialize::<(u32, u32)>()
                            .ok()
                            .map(|(id, _)| ServiceEvent::Closed(id)),
                        _ => None,
                    };
                    if let Some(event) = event
                        && events.send(event).is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|error| zbus::Error::InputOutput(std::sync::Arc::new(error)))?;
        let (requests, queue) =
            std::sync::mpsc::channel::<(Shown, tokio::sync::oneshot::Sender<Option<u32>>)>();
        std::thread::Builder::new()
            .name("notification-requests".into())
            .spawn(move || {
                let Ok(proxy) = Proxy::new(&connection, SERVICE, PATH, SERVICE) else {
                    return;
                };
                for (shown, reply) in queue {
                    let hints: HashMap<&str, Value<'_>> = HashMap::new();
                    let id = proxy
                        .call::<_, _, u32>(
                            "Notify",
                            &(
                                "OpenChamber",
                                0_u32,
                                "",
                                shown.title.as_str(),
                                shown.body.as_str(),
                                vec![DEFAULT_ACTION, shown.action.as_str()],
                                hints,
                                -1_i32,
                            ),
                        )
                        .inspect_err(|error| {
                            tracing::warn!(%error, "the notification service refused a notification")
                        })
                        .ok();
                    let _ = reply.send(id);
                }
            })
            .map_err(|error| zbus::Error::InputOutput(std::sync::Arc::new(error)))?;
        Ok((Self { requests }, received))
    }

    /// Shows one notification; resolves to its service id, `None` when the
    /// service refused it or is gone.
    pub(crate) fn show(&self, shown: Shown) -> tokio::sync::oneshot::Receiver<Option<u32>> {
        let (reply, receipt) = tokio::sync::oneshot::channel();
        let _ = self.requests.send((shown, reply));
        receipt
    }
}

/// The window's notification state.
#[derive(Default)]
pub(crate) struct DesktopNotifications {
    claims: Claims,
    /// Started on the first notification; a session bus that cannot be
    /// reached leaves it `Err` and logs once.
    notifier: Option<Result<Notifier, ()>>,
    /// What each shown notification opens, by service id, until it closes.
    targets: HashMap<u32, Target>,
    listener: Option<Task<()>>,
    /// A click for a session of another saved host, opened once that host's
    /// session list arrives.
    pub(crate) awaiting_host: Option<Target>,
}

impl Workspace {
    /// Starts the connection to the session bus's notification service.
    fn notifier(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<&Notifier> {
        if self.desktop_notifications.notifier.is_none() {
            let started = Connection::session().and_then(Notifier::start);
            self.desktop_notifications.notifier = Some(match started {
                Ok((notifier, events)) => {
                    self.listen_for_notification_clicks(events, window, cx);
                    Ok(notifier)
                }
                Err(error) => {
                    tracing::warn!(%error, "no desktop notification service");
                    Err(())
                }
            });
        }
        self.desktop_notifications.notifier.as_ref()?.as_ref().ok()
    }

    pub(crate) fn listen_for_notification_clicks(
        &mut self,
        mut events: tokio::sync::mpsc::UnboundedReceiver<ServiceEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.desktop_notifications.listener = Some(cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = events.recv().await {
                if this
                    .update_in(cx, |this, window, cx| match event {
                        ServiceEvent::Clicked(id) => this.notification_clicked(id, window, cx),
                        ServiceEvent::Closed(id) => {
                            this.desktop_notifications.targets.remove(&id);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    /// `handleUiNotificationEvent` and the desktop shell's delivery.
    pub(crate) fn deliver_notification(
        &mut self,
        notification: UiNotification,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focused = window.is_window_active();
        if notification.show_when_focused && focused {
            let text = match (&notification.title, &notification.body) {
                (Some(title), Some(body)) => format!("{title}\n{body}"),
                (Some(text), None) | (None, Some(text)) => text.clone(),
                (None, None) => String::new(),
            };
            if !text.is_empty() {
                window.push_notification(Notification::info(text), cx);
            }
        }
        let action = locale::strings(cx).get("chat.toast.opencodeRestartInterrupted.openSession");
        let Delivery::Show(shown) = delivery(
            &notification,
            focused,
            &mut self.desktop_notifications.claims,
            Instant::now(),
            &action,
        ) else {
            return;
        };
        let target = notification.session_id.map(|session| Target {
            runtime: self.runtime.clone(),
            endpoint: self
                .connection
                .as_ref()
                .map(|connection| connection.config.endpoint.clone()),
            session,
        });
        self.show_desktop_notification(shown, target, window, cx)
            .detach();
    }

    /// Shows a notification now; Settings' test notification has no target.
    /// The task answers whether the notification service took it.
    pub(crate) fn show_desktop_notification(
        &mut self,
        shown: Shown,
        target: Option<Target>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<bool> {
        let Some(notifier) = self.notifier(window, cx) else {
            return Task::ready(false);
        };
        let receipt = notifier.show(shown);
        cx.spawn(async move |this, cx| {
            let Ok(Some(id)) = receipt.await else {
                return false;
            };
            if let Some(target) = target {
                let _ = this.update(cx, |this, _| {
                    this.desktop_notifications.targets.insert(id, target);
                });
            }
            true
        })
    }

    /// The notification's click: the window comes forward and the session
    /// opens on its runtime, reconnecting to its saved host when the window
    /// shows another one now.
    pub(crate) fn notification_clicked(
        &mut self,
        id: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.activate_window();
        cx.activate(true);
        let Some(target) = self.desktop_notifications.targets.remove(&id) else {
            return;
        };
        self.open_notification_target(target, window, cx);
    }

    fn open_notification_target(
        &mut self,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if target.runtime == self.runtime {
            if self.session(&target.session).is_some() {
                self.select_session(target.session, window, cx);
            }
            return;
        }
        // Another runtime: only a host the user still has saved is opened,
        // as `resolveHostEntryForRuntimeKey` finds nothing for a removed one.
        let Some(endpoint) = target.endpoint.clone() else {
            return;
        };
        let saved = self
            .host_manager
            .as_ref()
            .is_some_and(|manager| manager.read(cx).has_saved_endpoint(&endpoint));
        if !saved {
            return;
        }
        self.desktop_notifications.awaiting_host = Some(target);
        self.connect_to_host(endpoint, window, cx);
    }

    /// After a host's session list arrives: opens the session a click on
    /// another host's notification asked for.
    pub(crate) fn open_awaited_notification(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.desktop_notifications.awaiting_host.as_ref() else {
            return;
        };
        if target.endpoint.as_ref()
            != self
                .connection
                .as_ref()
                .map(|connection| &connection.config.endpoint)
        {
            self.desktop_notifications.awaiting_host = None;
            return;
        }
        let Some(target) = self.desktop_notifications.awaiting_host.take() else {
            return;
        };
        if self.session(&target.session).is_some() {
            self.select_session(target.session, window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    fn notification(tag: Option<&str>, require_hidden: bool) -> UiNotification {
        UiNotification {
            kind: Some("completion".into()),
            session_id: Some(SessionId::from("ses_1")),
            title: Some("Ready".into()),
            body: Some("Done".into()),
            tag: tag.map(str::to_owned),
            require_hidden,
            ..UiNotification::default()
        }
    }

    #[test]
    fn a_focused_window_skips_background_only_notifications() {
        let mut claims = Claims::default();
        let now = Instant::now();
        assert_eq!(
            delivery(&notification(None, true), true, &mut claims, now, "Open"),
            Delivery::Skip
        );
        assert!(
            matches!(
                delivery(&notification(None, true), false, &mut claims, now, "Open"),
                Delivery::Show(_)
            ),
            "a skipped notification claims nothing"
        );
        assert!(matches!(
            delivery(
                &notification(Some("a"), false),
                true,
                &mut claims,
                now,
                "Open"
            ),
            Delivery::Show(_)
        ));
    }

    #[test]
    fn duplicates_within_five_seconds_show_once() {
        let mut claims = Claims::default();
        let start = Instant::now();
        let first = notification(Some("tag-1"), false);
        assert!(claims.claim(&first, start));
        assert!(!claims.claim(&first, start + Duration::from_secs(4)));
        assert!(claims.claim(&first, start + Duration::from_secs(6)));
        // Without a tag the text identifies it.
        let untagged = notification(None, false);
        assert!(claims.claim(&untagged, start));
        let mut other = untagged.clone();
        other.body = Some("Other".into());
        assert!(claims.claim(&other, start));
        assert!(!claims.claim(&untagged, start));
        assert!(
            claims.claim(&UiNotification::default(), start)
                && claims.claim(&UiNotification::default(), start),
            "a notification without any key is never deduplicated"
        );
    }

    #[test]
    fn the_title_falls_back_to_the_product_name() {
        let mut bare = notification(None, false);
        bare.title = None;
        bare.body = None;
        let Delivery::Show(shown) =
            delivery(&bare, false, &mut Claims::default(), Instant::now(), "Open")
        else {
            panic!("shown");
        };
        assert_eq!(shown.title, "OpenChamber");
        assert_eq!(shown.body, "");
    }

    /// A notification service on a private peer connection.
    #[derive(Clone, Default)]
    struct FakeService {
        shown: Arc<Mutex<Vec<(String, String, Vec<String>)>>>,
    }

    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl FakeService {
        #[allow(clippy::too_many_arguments)]
        fn notify(
            &self,
            _app_name: &str,
            _replaces_id: u32,
            _app_icon: &str,
            summary: &str,
            body: &str,
            actions: Vec<String>,
            _hints: HashMap<String, zbus::zvariant::OwnedValue>,
            _expire_timeout: i32,
        ) -> u32 {
            let mut shown = self.shown.lock().expect("shown");
            shown.push((summary.to_owned(), body.to_owned(), actions));
            shown.len() as u32
        }
    }

    #[test]
    fn notify_calls_and_click_signals_round_trip_over_dbus() {
        let (server, client) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        let service = FakeService::default();
        let shown = Arc::clone(&service.shown);
        let guid = zbus::Guid::generate();
        let server = std::thread::spawn(move || {
            zbus::blocking::connection::Builder::async_io_unix_stream(server)
                .server(guid)
                .expect("server guid")
                .p2p()
                .serve_at(PATH, service)
                .expect("serve")
                .build()
                .expect("server connection")
        });
        let client = zbus::blocking::connection::Builder::async_io_unix_stream(client)
            .p2p()
            .build()
            .expect("client connection");
        let server = server.join().expect("server thread");
        let (notifier, mut events) = Notifier::start(client).expect("notifier");
        let id = notifier
            .show(Shown {
                title: "Ready".into(),
                body: "Done".into(),
                action: "Open session".into(),
            })
            .blocking_recv()
            .expect("reply");
        assert_eq!(id, Some(1));
        assert_eq!(
            shown.lock().expect("shown").as_slice(),
            [(
                "Ready".to_owned(),
                "Done".to_owned(),
                vec!["default".to_owned(), "Open session".to_owned()]
            )]
        );
        server
            .emit_signal(
                None::<()>,
                PATH,
                SERVICE,
                "ActionInvoked",
                &(9_u32, "other"),
            )
            .expect("signal");
        server
            .emit_signal(
                None::<()>,
                PATH,
                SERVICE,
                "ActionInvoked",
                &(1_u32, "default"),
            )
            .expect("signal");
        server
            .emit_signal(
                None::<()>,
                PATH,
                SERVICE,
                "NotificationClosed",
                &(1_u32, 2_u32),
            )
            .expect("signal");
        assert_eq!(
            events.blocking_recv(),
            Some(ServiceEvent::Clicked(1)),
            "only the default action is a click"
        );
        assert_eq!(events.blocking_recv(), Some(ServiceEvent::Closed(1)));
    }
}
