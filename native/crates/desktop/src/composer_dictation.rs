//! The composer's dictation (`ComposerDictation.tsx`, `useDictation.ts`):
//! idle, recording, transcribing (`uploading`) and failed. The microphone
//! streams one-second chunks to the server while the user speaks; Insert or
//! Insert and send stops it and waits for the transcript, which lands in the
//! draft that was on screen when recording started.
//!
//! A failed transcription keeps every chunk, so Retry sends them again on a
//! new stream, and keeps the server's partial text, which Insert accepts.

use std::time::{Duration, Instant};

use gpui_kit::component::WindowExt;
use gpui_kit::component::notification::Notification;
use gpui_kit::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Role,
    StatefulInteractiveElement, Styled, Task, Window, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::dictation::{append_inline_text, format_duration};
use openchamber_native_core::draft::{Draft, DraftKey};
use openchamber_native_core::ids::RuntimeId;
use openchamber_native_transport::dictation::{DictationStream, StartOptions, StreamEvent, Timing};

use crate::audio_capture::{Capture, CaptureEvent};
use crate::chrome::{ButtonTone, IconButton, glyph};
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Status {
    #[default]
    Idle,
    Recording,
    /// Waiting for the transcript.
    Uploading,
    Failed,
}

/// What a confirmed transcript does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Action {
    #[default]
    Insert,
    Send,
}

/// One dictation's state.
#[derive(Default)]
pub(crate) struct Dictation {
    pub(crate) status: Status,
    started: Option<Instant>,
    /// The failure shown while failed, or the start's warning while recording.
    pub(crate) error: Option<String>,
    /// The local model is still downloading; the start retries.
    pub(crate) downloading: bool,
    /// The text of the segments the server already transcribed.
    pub(crate) partial: String,
    /// Every chunk of the recording, kept for Retry.
    chunks: Vec<String>,
    pub(crate) level: f32,
    action: Action,
    /// The draft the transcript belongs to, and its runtime.
    origin: Option<DraftKey>,
    runtime: Option<RuntimeId>,
    capture: Option<Capture>,
    stream: Option<DictationStream>,
    capture_task: Option<Task<()>>,
    stream_task: Option<Task<()>>,
    clock: Option<Task<()>>,
    /// Bumped by every start, retry and cancel; older tasks' results drop.
    generation: u64,
}

impl Dictation {
    pub(crate) fn seconds(&self) -> u64 {
        self.started
            .map_or(0, |started| started.elapsed().as_secs())
    }

    fn dictation_id() -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        format!(
            "dic_{:x}{:08x}",
            now.as_millis(),
            now.subsec_nanos() ^ std::process::id()
        )
    }

    /// Back to idle; drops the microphone and the stream, which cancels it.
    fn reset(&mut self) {
        self.generation += 1;
        self.status = Status::Idle;
        self.started = None;
        self.error = None;
        self.downloading = false;
        self.partial.clear();
        self.chunks.clear();
        self.level = 0.0;
        self.origin = None;
        self.runtime = None;
        self.capture = None;
        self.stream = None;
        self.capture_task = None;
        self.stream_task = None;
        self.clock = None;
    }
}

impl Workspace {
    /// `dictationEnabled`, absent meaning on, and a server to transcribe.
    pub(crate) fn dictation_available(&self) -> bool {
        let enabled = self
            .send_catalog
            .settings
            .as_ref()
            .and_then(|settings| settings.dictation_enabled)
            .unwrap_or(true);
        enabled
            && !self.is_synthetic()
            && self
                .connection
                .as_ref()
                .is_some_and(|connection| connection.client.is_some())
    }

    fn dictation_options(&self) -> StartOptions {
        let Some(settings) = self.send_catalog.settings.as_ref() else {
            return StartOptions::default();
        };
        StartOptions {
            provider: settings.stt_provider.unwrap_or_default(),
            language: settings.stt_language.clone().unwrap_or_default(),
            correct: settings.stt_correct_transcript.unwrap_or(false),
            local_model: settings.stt_local_model.clone().unwrap_or_default(),
            server_url: settings
                .stt_server_url
                .clone()
                .unwrap_or_else(|| "http://localhost:8001/v1".into()),
            model: settings
                .stt_model
                .clone()
                .unwrap_or_else(|| "deepdml/faster-whisper-large-v3-turbo-ct2".into()),
            endpoint_path: settings.stt_endpoint_path.clone().unwrap_or_default(),
        }
    }

    fn auto_send(&self) -> bool {
        self.send_catalog
            .settings
            .as_ref()
            .and_then(|settings| settings.dictation_auto_send)
            .unwrap_or(false)
    }

    /// The mic button: opens the microphone and the stream.
    pub(crate) fn start_dictation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dictation.status != Status::Idle
            || !self.dictation_available()
            || !self.composer_editable()
        {
            return;
        }
        let started = match Capture::start() {
            Ok(started) => started,
            Err(error) => {
                tracing::warn!(%error, "the microphone did not open");
                return;
            }
        };
        let crate::audio_capture::Started {
            capture,
            mut events,
            opened,
        } = started;
        self.save_composer_draft(cx);
        let dictation = &mut self.dictation;
        dictation.reset();
        dictation.status = Status::Recording;
        dictation.started = Some(Instant::now());
        dictation.origin = self.composer_key.clone();
        dictation.runtime = Some(self.runtime.clone());
        dictation.capture = Some(capture);
        let generation = dictation.generation;
        self.dictation.capture_task = Some(cx.spawn_in(window, async move |this, cx| {
            // As the web client does: a microphone that does not open puts
            // the composer back as it was.
            match opened.await {
                Ok(Ok(())) => {}
                outcome => {
                    if let Ok(Err(error)) = &outcome {
                        tracing::warn!(%error, "the microphone did not open");
                    }
                    let _ = this.update(cx, |this, cx| {
                        if this.dictation.generation == generation {
                            this.dictation.reset();
                            cx.notify();
                        }
                    });
                    return;
                }
            }
            let _ = this.update_in(cx, |this, window, cx| {
                if this.dictation.generation == generation {
                    this.open_dictation_stream(window, cx);
                }
            });
            while let Some(first) = events.recv().await {
                // Everything that arrived meanwhile goes in one update, so a
                // device that calls back often cannot flood the window.
                let mut batch = vec![first];
                while let Ok(event) = events.try_recv() {
                    batch.push(event);
                }
                let alive = this.update(cx, |this, cx| {
                    if this.dictation.generation != generation {
                        return false;
                    }
                    for event in batch {
                        match event {
                            CaptureEvent::Chunk(chunk) => {
                                if let Some(stream) = &this.dictation.stream {
                                    stream.send_chunk(chunk.clone());
                                }
                                this.dictation.chunks.push(chunk);
                            }
                            CaptureEvent::Level(level) => this.dictation.level = level,
                        }
                    }
                    cx.notify();
                    true
                });
                if !matches!(alive, Ok(true)) {
                    return;
                }
            }
            // The microphone stopped and every chunk is in.
            let _ = this.update_in(cx, |this, window, cx| {
                if this.dictation.generation == generation {
                    this.capture_finished(window, cx);
                }
            });
        }));
        self.dictation.clock = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let ticking = this.update(cx, |this, cx| {
                    cx.notify();
                    this.dictation.generation == generation
                        && this.dictation.status == Status::Recording
                });
                if !matches!(ticking, Ok(true)) {
                    return;
                }
            }
        }));
        cx.notify();
    }

    /// A stream on the current connection, fed every chunk so far.
    fn open_dictation_stream(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let options = self.dictation_options();
        let handle = connection.network.handle();
        let (stream, mut events) = {
            let _runtime = handle.enter();
            DictationStream::open(
                client,
                options,
                Dictation::dictation_id(),
                Timing::default(),
            )
        };
        for chunk in &self.dictation.chunks {
            stream.send_chunk(chunk.clone());
        }
        self.dictation.stream = Some(stream);
        let generation = self.dictation.generation;
        self.dictation.stream_task = Some(cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = events.recv().await {
                let done = matches!(event, StreamEvent::Final(_) | StreamEvent::Failed(_));
                let _ = this.update_in(cx, |this, window, cx| {
                    if this.dictation.generation == generation {
                        this.dictation_event(event, window, cx);
                    }
                });
                if done {
                    return;
                }
            }
        }));
    }

    fn dictation_event(&mut self, event: StreamEvent, window: &mut Window, cx: &mut Context<Self>) {
        let dictation = &mut self.dictation;
        match event {
            StreamEvent::Started => {
                dictation.error = None;
                dictation.downloading = false;
            }
            StreamEvent::StartFailed(error) => {
                dictation.downloading = error.model_downloading();
                dictation.error = Some(error.message);
            }
            StreamEvent::Partial(text) => dictation.partial = text,
            StreamEvent::Final(text) => {
                let text = if text.trim().is_empty() {
                    dictation.partial.clone()
                } else {
                    text
                };
                self.deliver_transcript(text, window, cx);
            }
            StreamEvent::Failed(error) => {
                dictation.stream = None;
                dictation.capture = None;
                dictation.clock = None;
                if dictation.chunks.is_empty() {
                    self.dictation.reset();
                } else {
                    dictation.status = Status::Failed;
                    dictation.downloading = false;
                    dictation.error = Some(error.message);
                }
            }
        }
        cx.notify();
    }

    fn capture_finished(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dictation.status != Status::Uploading {
            return;
        }
        if self.dictation.chunks.is_empty() {
            self.deliver_transcript(String::new(), window, cx);
            return;
        }
        match &self.dictation.stream {
            Some(stream) => stream.finish(),
            None => self.dictation_event(
                StreamEvent::Failed(openchamber_native_transport::dictation::DictationError {
                    message: locale::strings(cx).get("chat.dictation.failed").to_string(),
                    retryable: true,
                    reason_code: None,
                }),
                window,
                cx,
            ),
        }
    }

    /// Insert, or Insert and send: stops the microphone and transcribes.
    pub(crate) fn confirm_dictation(&mut self, action: Action, cx: &mut Context<Self>) {
        if self.dictation.status != Status::Recording {
            return;
        }
        self.dictation.action = action;
        self.dictation.status = Status::Uploading;
        self.dictation.error = None;
        self.dictation.clock = None;
        if let Some(capture) = self.dictation.capture.as_mut() {
            capture.stop();
        }
        cx.notify();
    }

    /// Enter while recording: the auto-send setting decides.
    pub(crate) fn confirm_dictation_default(&mut self, cx: &mut Context<Self>) {
        let action = if self.auto_send() {
            Action::Send
        } else {
            Action::Insert
        };
        self.confirm_dictation(action, cx);
    }

    /// Cancel while recording or transcribing, Discard once failed.
    pub(crate) fn cancel_dictation(&mut self, cx: &mut Context<Self>) {
        if self.dictation.status == Status::Idle {
            return;
        }
        self.dictation.reset();
        cx.notify();
    }

    /// Retry after a failure: every chunk again, on a new stream.
    pub(crate) fn retry_dictation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dictation.status != Status::Failed || self.dictation.chunks.is_empty() {
            return;
        }
        self.dictation.generation += 1;
        self.dictation.status = Status::Uploading;
        self.dictation.action = Action::Insert;
        self.dictation.error = None;
        self.open_dictation_stream(window, cx);
        if let Some(stream) = &self.dictation.stream {
            stream.finish();
        }
        cx.notify();
    }

    /// Insert after a failure: the text the server transcribed before it.
    pub(crate) fn accept_partial_dictation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dictation.status != Status::Failed {
            return;
        }
        self.dictation.action = Action::Insert;
        let text = self.dictation.partial.clone();
        self.deliver_transcript(text, window, cx);
    }

    /// The transcript goes into the draft recording started on; when that
    /// draft is no longer on screen it is kept there (`useDictationOrigin`).
    fn deliver_transcript(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let action = self.dictation.action;
        let origin = self.dictation.origin.clone();
        self.dictation.reset();
        let text = text.trim().to_owned();
        if text.is_empty() {
            cx.notify();
            return;
        }
        if let Some(origin) = origin
            && self.composer_key.as_ref() != Some(&origin)
        {
            let mut draft = self
                .drafts
                .get(&origin)
                .cloned()
                .unwrap_or_else(|| Draft::new(""));
            let next = append_inline_text(&draft.text, &text);
            let end = next.len();
            draft.edit_text(next, end..end);
            self.drafts.save(origin, draft);
            window.push_notification(
                Notification::info(
                    locale::strings(cx).get("chat.chatInput.toast.dictationKeptForOriginalSession"),
                ),
                cx,
            );
            cx.notify();
            return;
        }
        self.composer.update(cx, |composer, cx| {
            let current = composer.value().to_string();
            let next = append_inline_text(&current, &text);
            let end = current.len();
            composer.set_selected_range(end..end, cx);
            composer.replace(next[end..].to_owned(), window, cx);
            let caret = next.len();
            composer.set_selected_range(caret..caret, cx);
        });
        self.save_composer_draft(cx);
        if action == Action::Send {
            self.submit(window, cx);
        } else {
            self.focus_composer(window, cx);
        }
        cx.notify();
    }

    /// The overlay over the composer while dictation is active.
    pub(crate) fn render_dictation_overlay(
        &self,
        palette: &Palette,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let dictation = &self.dictation;
        if dictation.status == Status::Idle {
            return None;
        }
        let strings = locale::strings(cx);
        let direction = strings.direction();
        let placeholder = match dictation.status {
            Status::Uploading => strings.get("chat.dictation.processing"),
            Status::Recording if dictation.downloading => {
                strings.get("chat.dictation.downloadingModel")
            }
            _ => strings.get("chat.dictation.listening"),
        };
        let failed = dictation.status == Status::Failed;
        let mut text = div()
            .id("dictation-text")
            .flex_1()
            .min_h_0()
            .px(px(12.0))
            .pt(px(20.0))
            .pb(px(8.0))
            .overflow_y_scroll();
        if failed && !dictation.partial.is_empty() {
            text = text.child(div().child(dictation.partial.clone()));
        } else if !failed {
            text = text.child(
                div()
                    .text_color(palette.muted_foreground)
                    .child(placeholder),
            );
        }
        if failed {
            let error = dictation
                .error
                .clone()
                .unwrap_or_else(|| strings.get("chat.dictation.failed").to_string());
            text = text.child(
                div()
                    .mt(px(4.0))
                    .text_size(gpui_kit::rems(0.75))
                    .text_color(palette.status_error)
                    .child(error),
            );
        } else if dictation.status == Status::Recording
            && !dictation.downloading
            && let Some(error) = &dictation.error
        {
            text = text.child(
                div()
                    .mt(px(4.0))
                    .text_size(gpui_kit::rems(0.75))
                    .text_color(palette.status_warning)
                    .child(error.clone()),
            );
        }
        let button = |id: &'static str, icon: &'static str, key: &str| {
            IconButton::new(id, icon, strings.get(key))
                .size(32.0, 18.0)
                .tone(ButtonTone::Quiet)
        };
        let mut actions = div()
            .flex_shrink_0()
            .flex_row_from(direction)
            .items_center()
            .gap(px(12.0));
        let mut meter = div()
            .flex_1()
            .min_w_0()
            .flex_row_from(direction)
            .items_center()
            .gap(px(12.0));
        match dictation.status {
            Status::Recording => {
                let level = dictation.level;
                meter = meter
                    .child(
                        div()
                            .size(px(6.0))
                            .rounded(px(3.0))
                            .flex_shrink_0()
                            .bg(palette.status_error),
                    )
                    .child(
                        div()
                            .id("dictation-level")
                            .role(Role::ProgressIndicator)
                            .aria_label(strings.get("chat.dictation.overlayAria"))
                            .flex_1()
                            .h(px(4.0))
                            .rounded(px(2.0))
                            .bg(palette.border)
                            .child(
                                div()
                                    .h_full()
                                    .rounded(px(2.0))
                                    .w(gpui_kit::relative(level.clamp(0.02, 1.0)))
                                    .bg(palette.muted_foreground),
                            ),
                    )
                    .child(
                        div()
                            .text_size(gpui_kit::rems(0.75))
                            .text_color(palette.muted_foreground)
                            .child(format_duration(dictation.seconds())),
                    );
                actions = actions
                    .child(
                        button("dictation-cancel", "close", "chat.dictation.cancel").render(
                            palette,
                            cx,
                            |this, _, cx| this.cancel_dictation(cx),
                        ),
                    )
                    .child(
                        button("dictation-insert", "check", "chat.dictation.insert").render(
                            palette,
                            cx,
                            |this, _, cx| this.confirm_dictation(Action::Insert, cx),
                        ),
                    )
                    .child(
                        button(
                            "dictation-insert-and-send",
                            "send-plane-2",
                            "chat.dictation.insertAndSend",
                        )
                        .tone(ButtonTone::Active)
                        .render(palette, cx, |this, _, cx| {
                            this.confirm_dictation(Action::Send, cx)
                        }),
                    );
            }
            Status::Uploading => {
                meter = meter.child(glyph("loader-4", 16.0, palette.muted_foreground));
                actions = actions.child(
                    button("dictation-cancel", "close", "chat.dictation.cancel").render(
                        palette,
                        cx,
                        |this, _, cx| this.cancel_dictation(cx),
                    ),
                );
            }
            Status::Failed => {
                actions = actions
                    .child(
                        button("dictation-discard", "close", "chat.dictation.discard").render(
                            palette,
                            cx,
                            |this, _, cx| this.cancel_dictation(cx),
                        ),
                    )
                    .child(
                        button("dictation-retry", "refresh", "chat.dictation.retry").render(
                            palette,
                            cx,
                            |this, window, cx| this.retry_dictation(window, cx),
                        ),
                    )
                    .when(!dictation.partial.trim().is_empty(), |actions| {
                        actions.child(
                            button("dictation-accept", "check", "chat.dictation.insert").render(
                                palette,
                                cx,
                                |this, window, cx| this.accept_partial_dictation(window, cx),
                            ),
                        )
                    });
            }
            Status::Idle => {}
        }
        Some(
            div()
                .id("dictation-overlay")
                .accessibility_id("dictation-overlay")
                .role(Role::Dialog)
                .aria_label(strings.get("chat.dictation.overlayAria"))
                .absolute()
                .inset_0()
                .rounded(px(crate::composer_view::BOX_RADIUS))
                .bg(palette.elevated)
                .text_color(palette.elevated_foreground)
                .flex()
                .flex_col()
                .child(text)
                .child(
                    div()
                        .flex_shrink_0()
                        .h(px(crate::composer_view::TOOLBAR_HEIGHT))
                        .px(px(crate::composer_view::TOOLBAR_PADDING_X))
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(12.0))
                        .child(meter)
                        .child(actions),
                )
                .into_any_element(),
        )
    }
}
