//! A page an agent showed with `openchamber_render`, in place of its tool
//! row (`HtmlRenderFrame.tsx`).
//!
//! The page runs in a frame shim's `srcdoc` iframe sandboxed without
//! `allow-same-origin`, in the content profile, so its script has an opaque
//! origin and reaches neither the desktop nor the network through its frame.
//! The bootstrap reports the page's height, so the frame takes it and the
//! chat scrolls the page as one of its rows; vertical wheel turns over the
//! page go to the chat. A theme switch is posted to the page instead of
//! rebuilding it, so its script state survives.
//!
//! A page that waits for an answer sends it with `openchamber.submit`. The
//! desktop checks again what the page's own script may skip: the call still
//! waits, a real press in the page came first, and the summary is present.

use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Theme, WindowExt};
use gpui_kit::{
    Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled, Subscription,
    WeakEntity, Window, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::ids::{PartId, RuntimeId, SessionId};
use openchamber_native_core::rendered_page::{Ask, RenderedPage};
use serde_json::{Value, json};

use crate::baseline::CssTextExt;
use crate::context_panel::{XS_LINE, XS_SIZE};
use crate::html_render_document::{self, AnswerState, Metrics, PageTheme};
use crate::locale;
use crate::network::NetworkTask;
use crate::theme::Palette;
use crate::web_policy::{self, ViewPolicy};
use crate::web_shim::{self, FrameSource};
use crate::webview_host::{HostElement, HostedWebView, Load, ViewSpec, WebViewEvent, new_nonce};
use crate::webview_platform::Profile;
use crate::workspace::Workspace;

const SIZE_MESSAGE: &str = "openchamber:html-render:size";
const OPEN_LINK_MESSAGE: &str = "openchamber:html-render:open-link";
const POINTER_MESSAGE: &str = "openchamber:html-render:pointer";
/// The frame's height before the page reports its own.
const MIN_HEIGHT: f32 = 48.0;

/// The call a page belongs to: where its answer goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PageCall {
    pub runtime: RuntimeId,
    pub session: SessionId,
    pub call: PartId,
}

/// A submission the page sent: its summary and data.
#[derive(Clone, Debug, PartialEq)]
struct Submission {
    summary: String,
    data: Value,
}

pub(crate) struct HtmlAnswerView {
    call: PageCall,
    owner: WeakEntity<Workspace>,
    page: RenderedPage,
    host: Option<Entity<HostedWebView>>,
    /// The page cannot be shown in this session.
    unavailable: bool,
    height: f32,
    theme: Option<PageTheme>,
    answer: AnswerState,
    /// What this client sent, shown until the call records the answer.
    sent: Option<Submission>,
    _events: Option<Subscription>,
}

/// The theme a page styles against, from the app's palette, sizes and fonts.
fn page_theme(cx: &gpui_kit::App) -> Option<PageTheme> {
    let palette = cx.global::<Palette>();
    let theme = Theme::global(cx);
    let rem = crate::appearance::root_font_size(cx);
    let metrics = Metrics {
        rem,
        text: rem * 0.875,
        sans: format!("\"{}\", sans-serif", theme.font_family),
        mono: format!("\"{}\", monospace", theme.mono_font_family),
    };
    PageTheme::of(&palette.id, &metrics)
}

impl HtmlAnswerView {
    pub(crate) fn new(call: PageCall, owner: WeakEntity<Workspace>, page: RenderedPage) -> Self {
        let answer = answer_state(&page.ask, None);
        Self {
            call,
            owner,
            page,
            host: None,
            unavailable: false,
            height: MIN_HEIGHT,
            theme: None,
            answer,
            sent: None,
            _events: None,
        }
    }

    /// Takes the call's newest state; a changed answer reaches the page as
    /// a message, a changed page rebuilds the frame.
    pub(crate) fn update_page(&mut self, page: RenderedPage, cx: &mut Context<Self>) {
        if page == self.page {
            return;
        }
        if page.html != self.page.html {
            self.host = None;
            self._events = None;
        }
        self.page = page;
        self.sync_answer(cx);
        cx.notify();
    }

    /// Tells a loaded page when its answer state changed.
    fn sync_answer(&mut self, cx: &Context<Self>) {
        let next = answer_state(&self.page.ask, self.sent.as_ref());
        if next != self.answer {
            self.answer = next;
            self.post(&self.answer.message(), cx);
        }
    }

    fn ensure_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.host.is_some() || self.unavailable {
            return;
        }
        let Some(theme) = page_theme(cx) else {
            self.unavailable = true;
            return;
        };
        let nonce = new_nonce();
        let document =
            html_render_document::build_document(&self.page.html, &theme, false, &self.answer);
        let title = if self.page.title.is_empty() {
            locale::strings(cx)
                .get("chat.htmlRender.untitled")
                .to_string()
        } else {
            self.page.title.clone()
        };
        let spec = ViewSpec {
            policy: ViewPolicy::HtmlAnswer {
                shim: web_policy::shim_url(&nonce),
            },
            profile: Profile::Content,
            load: Load::Shim {
                nonce,
                document: web_shim::shim_document(
                    &FrameSource::Inline(document),
                    web_shim::PAGE_SANDBOX,
                    &title,
                ),
            },
            forward_wheel: true,
            developer_tools: false,
        };
        match HostedWebView::build(spec, window, cx) {
            Ok(host) => {
                self._events =
                    Some(
                        cx.subscribe_in(&host, window, |this, _, event, window, cx| {
                            this.page_event(event, window, cx);
                        }),
                    );
                self.host = Some(host);
                self.theme = Some(theme);
            }
            Err(error) => {
                tracing::warn!(%error, "an agent's page cannot be shown");
                self.unavailable = true;
            }
        }
    }

    fn post(&self, message: &Value, cx: &Context<Self>) {
        if let Some(host) = &self.host {
            host.read(cx).post_message(message);
        }
    }

    fn page_event(&mut self, event: &WebViewEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            WebViewEvent::Message(message) => self.page_message(message, window, cx),
            WebViewEvent::PointerOver(over) => {
                self.post(&json!({ "type": POINTER_MESSAGE, "over": over }), cx);
            }
            WebViewEvent::Crashed => {
                self.host = None;
                self._events = None;
                cx.notify();
            }
            _ => {}
        }
    }

    fn page_message(&mut self, message: &Value, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((id, submission)) = read_submission(message) {
            self.submit(id, submission, window, cx);
            return;
        }
        match message.get("type").and_then(Value::as_str) {
            Some(SIZE_MESSAGE) => {
                let Some(height) = message
                    .get("height")
                    .and_then(Value::as_f64)
                    .filter(|height| *height > 0.0)
                else {
                    return;
                };
                let height = html_render_document::clamp_height(height);
                if height != self.height {
                    self.height = height;
                    cx.notify();
                }
            }
            Some(OPEN_LINK_MESSAGE) => {
                let url = message
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if ViewPolicy::Browser.allows_load(url) && url != web_policy::BLANK_URL {
                    cx.open_url(url);
                }
            }
            _ => {}
        }
    }

    fn submit(
        &mut self,
        id: Value,
        submission: Submission,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let refuse = |this: &Self, reason: &str, cx: &Context<Self>| {
            this.post(&submit_reply(&id, Some(reason)), cx)
        };
        if !self.answer.waiting || self.sent.is_some() {
            return refuse(self, "This page is not waiting for an answer", cx);
        }
        // The page's own check can be skipped by the page; GTK only reports
        // real presses.
        let gesture = self
            .host
            .as_ref()
            .is_some_and(|host| host.read(cx).has_recent_gesture());
        if !gesture {
            return refuse(self, "openchamber.submit works only from a click", cx);
        }
        if submission.summary.is_empty() {
            return refuse(self, "summary is required: the choice in a few words", cx);
        }
        let Some(owner) = self.owner.upgrade() else {
            return refuse(self, "The answer was not delivered", cx);
        };
        let workspace = owner.read(cx);
        let connection = workspace
            .connection
            .as_ref()
            .filter(|_| workspace.runtime == self.call.runtime);
        let Some((client, network)) = connection
            .and_then(|connection| Some((connection.client.clone()?, connection.network.handle())))
        else {
            return refuse(self, "The answer was not delivered", cx);
        };
        let session = self.call.session.to_string();
        let call = self.call.call.to_string();
        let summary = submission.summary.clone();
        let data = submission.data.clone();
        let request = NetworkTask::spawn_on(&network, async move {
            client
                .answer_rendered_page(&session, &call, &summary, data)
                .await
                .map_err(|error| error.to_string())
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = request
                .await
                .unwrap_or_else(|| Err("The answer was not delivered".to_owned()));
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(()) => {
                    this.post(&submit_reply(&id, None), cx);
                    this.sent = Some(submission);
                    this.sync_answer(cx);
                    cx.notify();
                }
                Err(message) => {
                    this.post(&submit_reply(&id, Some(&message)), cx);
                    let title = locale::strings(cx).get("chat.htmlRender.answerFailed");
                    window.push_notification(Notification::error(message.clone()).title(title), cx);
                }
            });
        })
        .detach();
    }

    fn status(&self, cx: &Context<Self>) -> Option<SharedString> {
        let strings = locale::strings(cx);
        let summary = match (&self.page.ask, &self.sent) {
            (Ask::Answered { summary, .. }, _) => Some(summary.clone()),
            (_, Some(sent)) => Some(sent.summary.clone()),
            _ => None,
        };
        if let Some(summary) = summary {
            // A form's summary has a line per field; on one line they need a
            // separator.
            let line = summary
                .trim()
                .split('\n')
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" \u{b7} ");
            return Some(strings.format("chat.htmlRender.answered", &[("summary", &line)]));
        }
        let key = match self.page.ask {
            Ask::Waiting => "chat.htmlRender.waiting",
            Ask::AnsweredInChat => "chat.htmlRender.answeredInChat",
            Ask::ChatSentEarlier => "chat.htmlRender.chatSentEarlier",
            Ask::Unanswered => "chat.htmlRender.unanswered",
            Ask::NotDrawn => "chat.htmlRender.notDrawn",
            Ask::Show | Ask::Answered { .. } => return None,
        };
        Some(strings.get(key))
    }
}

/// The answer state a page loads with and is told about.
fn answer_state(ask: &Ask, sent: Option<&Submission>) -> AnswerState {
    match (ask, sent) {
        (Ask::Answered { data, .. }, _) => AnswerState {
            waiting: false,
            answer: serde_json::from_str(data).ok(),
        },
        (_, Some(sent)) => AnswerState {
            waiting: false,
            answer: Some(sent.data.clone()),
        },
        (ask, None) => AnswerState {
            waiting: *ask == Ask::Waiting,
            answer: None,
        },
    }
}

/// An MCP Apps `ui/message` request (`uiMessageSchema`): its id and the
/// summary and data it carries.
fn read_submission(message: &Value) -> Option<(Value, Submission)> {
    if message.get("jsonrpc")?.as_str()? != "2.0"
        || message.get("method")?.as_str()? != "ui/message"
    {
        return None;
    }
    let id = message.get("id")?;
    let valid_id = id.is_number() || id.as_str().is_some_and(|id| id.chars().count() <= 64);
    let params = message.get("params")?;
    if !valid_id || params.get("role")?.as_str()? != "user" {
        return None;
    }
    let content = params.get("content")?.as_array()?;
    if content.is_empty() {
        return None;
    }
    let mut texts = Vec::with_capacity(content.len());
    for block in content {
        if block.get("type")?.as_str()? != "text" {
            return None;
        }
        texts.push(block.get("text")?.as_str()?);
    }
    let data = params
        .get("_meta")
        .and_then(|meta| meta.get("openchamber/data"))
        .cloned()
        .unwrap_or(Value::Null);
    Some((
        id.clone(),
        Submission {
            summary: texts.join("\n").trim().to_owned(),
            data,
        },
    ))
}

/// The host's reply to a page's `ui/message` (`htmlRenderSubmitReply`).
fn submit_reply(id: &Value, error: Option<&str>) -> Value {
    match error {
        None => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        Some(message) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32000, "message": message },
        }),
    }
}

impl Render for HtmlAnswerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_host(window, cx);
        let theme = page_theme(cx);
        if self.theme.is_some() && theme != self.theme {
            if let Some(theme) = &theme {
                self.post(&theme.message(), cx);
            }
            self.theme = theme;
        }
        let palette = cx.global::<Palette>().clone();
        let status = self.status(cx);
        let waiting = self.page.ask == Ask::Waiting && self.sent.is_none();
        div()
            .my(px(8.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(self.height))
                    .when_some(self.host.clone(), |this, host| {
                        this.child(HostElement::new(host))
                    }),
            )
            .when_some(status, |this, status| {
                this.child(
                    div()
                        .mt(px(4.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .css_text(XS_SIZE, XS_LINE, cx)
                        .text_color(palette.muted_foreground)
                        .when(waiting, |this| {
                            this.child(
                                div()
                                    .size(px(6.0))
                                    .flex_shrink_0()
                                    .rounded_full()
                                    .bg(palette.primary),
                            )
                        })
                        .child(div().min_w_0().truncate().child(status)),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submissions_follow_the_mcp_apps_message() {
        let message = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "ui/message",
            "params": {
                "role": "user",
                "content": [{ "type": "text", "text": " Pick A " }, { "type": "text", "text": "and B" }],
                "_meta": { "openchamber/data": { "pick": "a" } }
            }
        });
        let (id, submission) = read_submission(&message).expect("a submission");
        assert_eq!(id, json!(3));
        assert_eq!(submission.summary, "Pick A \nand B");
        assert_eq!(submission.data, json!({ "pick": "a" }));
        for refused in [
            json!({ "jsonrpc": "2.0", "id": 1, "method": "ui/other", "params": {} }),
            json!({ "jsonrpc": "2.0", "id": "x".repeat(65), "method": "ui/message", "params": { "role": "user", "content": [{ "type": "text", "text": "a" }] } }),
            json!({ "jsonrpc": "2.0", "id": 1, "method": "ui/message", "params": { "role": "assistant", "content": [{ "type": "text", "text": "a" }] } }),
            json!({ "jsonrpc": "2.0", "id": 1, "method": "ui/message", "params": { "role": "user", "content": [] } }),
        ] {
            assert!(read_submission(&refused).is_none(), "{refused}");
        }
    }

    #[test]
    fn replies_carry_the_reason_a_submit_failed() {
        assert_eq!(
            submit_reply(&json!(1), None),
            json!({ "jsonrpc": "2.0", "id": 1, "result": {} })
        );
        assert_eq!(
            submit_reply(&json!("a"), Some("no")),
            json!({ "jsonrpc": "2.0", "id": "a", "error": { "code": -32000, "message": "no" } })
        );
    }

    #[test]
    fn a_sent_answer_ends_the_wait() {
        let waiting = answer_state(&Ask::Waiting, None);
        assert!(waiting.waiting);
        let sent = Submission {
            summary: "A".to_owned(),
            data: json!({ "pick": 1 }),
        };
        let after = answer_state(&Ask::Waiting, Some(&sent));
        assert!(!after.waiting);
        assert_eq!(after.answer, Some(json!({ "pick": 1 })));
        let recorded = answer_state(
            &Ask::Answered {
                summary: "A".to_owned(),
                data: "{\"pick\":2}".to_owned(),
            },
            Some(&sent),
        );
        assert_eq!(recorded.answer, Some(json!({ "pick": 2 })));
    }
}
