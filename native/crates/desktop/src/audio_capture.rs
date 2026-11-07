//! Microphone capture for dictation through cpal (ALSA on Linux, which
//! PipeWire and PulseAudio serve): the default input device's stream, mixed
//! to mono and cut by the core's `Chunker` into one-second 16 kHz PCM16
//! chunks, plus the waveform level.
//!
//! cpal's stream is not `Send` on every platform, so one thread owns it from
//! open to stop. Stopping flushes the partial last chunk before the event
//! channel closes, so the recording knows every chunk has arrived.

use std::sync::mpsc;
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use openchamber_native_core::dictation::{Chunker, downmix, level, pcm16_base64};

/// Why the microphone did not open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CaptureError {
    /// No input device exists.
    NoDevice,
    /// The system refused access to the device.
    PermissionDenied,
    Failed(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDevice => formatter.write_str("no microphone"),
            Self::PermissionDenied => formatter.write_str("microphone access denied"),
            Self::Failed(error) => formatter.write_str(error),
        }
    }
}

fn capture_error(error: &cpal::Error) -> CaptureError {
    match error.kind() {
        cpal::ErrorKind::PermissionDenied => CaptureError::PermissionDenied,
        cpal::ErrorKind::DeviceNotAvailable => CaptureError::NoDevice,
        _ => CaptureError::Failed(error.to_string()),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CaptureEvent {
    /// A base64 PCM16 chunk, in order.
    Chunk(String),
    /// The waveform's 0..1 level.
    Level(f32),
}

/// A running capture; dropping it stops the microphone.
#[derive(Debug)]
pub(crate) struct Capture {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Opens the default input device. Blocks until it is open or refused.
    pub(crate) fn start()
    -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<CaptureEvent>), CaptureError> {
        let (events, received) = tokio::sync::mpsc::unbounded_channel();
        let (opened, open_result) = mpsc::channel::<Result<(), CaptureError>>();
        let (stop, stopped) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("dictation-capture".into())
            .spawn(move || run(events, opened, stopped))
            .map_err(|error| CaptureError::Failed(error.to_string()))?;
        match open_result.recv() {
            Ok(Ok(())) => Ok((
                Self {
                    stop: Some(stop),
                    thread: Some(thread),
                },
                received,
            )),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => Err(CaptureError::Failed("the capture thread ended".into())),
        }
    }

    /// Stops the microphone; the last chunk and the end of the channel follow.
    pub(crate) fn stop(&mut self) {
        self.stop.take();
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.take();
        // The thread only drops its stream and flushes; it ends promptly.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    events: tokio::sync::mpsc::UnboundedSender<CaptureEvent>,
    opened: mpsc::Sender<Result<(), CaptureError>>,
    stopped: mpsc::Receiver<()>,
) {
    let host = cpal::default_host();
    let Some(device) = host.default_input_device() else {
        let _ = opened.send(Err(CaptureError::NoDevice));
        return;
    };
    let config = match device.default_input_config() {
        Ok(config) => config,
        Err(error) => {
            let _ = opened.send(Err(capture_error(&error)));
            return;
        }
    };
    let channels = usize::from(config.channels());
    let rate = config.sample_rate();
    let chunker = std::sync::Arc::new(std::sync::Mutex::new(Chunker::new(rate)));
    let feed = {
        let chunker = std::sync::Arc::clone(&chunker);
        let events = events.clone();
        move |mono: Vec<f32>| {
            let _ = events.send(CaptureEvent::Level(level(&mono)));
            let Ok(mut chunker) = chunker.lock() else {
                return;
            };
            for chunk in chunker.push(&mono) {
                let _ = events.send(CaptureEvent::Chunk(pcm16_base64(&chunk)));
            }
        }
    };
    let on_error = |error: cpal::Error| tracing::warn!(%error, "microphone stream error");
    let stream_config: cpal::StreamConfig = config.clone().into();
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            stream_config,
            move |data: &[f32], _: &_| feed(downmix(data, channels)),
            on_error,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            stream_config,
            move |data: &[i16], _: &_| {
                let floats: Vec<f32> = data.iter().map(|s| f32::from(*s) / 32768.0).collect();
                feed(downmix(&floats, channels))
            },
            on_error,
            None,
        ),
        cpal::SampleFormat::I32 => device.build_input_stream(
            stream_config,
            move |data: &[i32], _: &_| {
                let floats: Vec<f32> = data.iter().map(|s| *s as f32 / 2_147_483_648.0).collect();
                feed(downmix(&floats, channels))
            },
            on_error,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            stream_config,
            move |data: &[u16], _: &_| {
                let floats: Vec<f32> = data
                    .iter()
                    .map(|s| (f32::from(*s) - 32768.0) / 32768.0)
                    .collect();
                feed(downmix(&floats, channels))
            },
            on_error,
            None,
        ),
        other => {
            let _ = opened.send(Err(CaptureError::Failed(format!(
                "unsupported sample format {other}"
            ))));
            return;
        }
    };
    let stream = match stream.and_then(|stream| stream.play().map(|()| stream)) {
        Ok(stream) => stream,
        Err(error) => {
            let _ = opened.send(Err(capture_error(&error)));
            return;
        }
    };
    let _ = opened.send(Ok(()));
    // Waits for stop: a message or the sender's drop.
    let _ = stopped.recv();
    drop(stream);
    let tail = chunker.lock().ok().and_then(|mut chunker| chunker.finish());
    if let Some(tail) = tail {
        let _ = events.send(CaptureEvent::Chunk(pcm16_base64(&tail)));
    }
    let _ = events.send(CaptureEvent::Level(0.0));
}
