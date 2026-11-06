//! Linux HEIC containment and a private, versioned byte-only worker protocol.
//!
//! The launcher blocks and must run off the UI thread. One process-local lock
//! permits one worker at a time. Each worker is the current executable, entered
//! before desktop initialization, with an empty environment and no profile,
//! window, network, filename or filesystem input. Linux prlimit applies a hard
//! and soft 512 MiB address-space ceiling and disables core dumps before exec.
//! The parent owns the 30-second wall deadline, bounded pipes, kill and reap.
//!
//! Address space and elapsed time contain decoder allocations, aborts and work
//! that geometry accounting cannot measure. A failed exit does not establish
//! OOM. No failure retries or falls back to decoding inside the desktop process.

use std::io::{self, Read, Write};

use crate::attachment_images::{self, HeicConversionLimits, ImageConversionError};

pub(crate) const WORKER_ARGUMENT: &str = "--attachment-heic-worker";
pub(crate) const MAX_INPUT_BYTES: usize = 10 * 1024 * 1024;
pub(crate) const MAX_OUTPUT_BYTES: usize = 20 * 1024 * 1024;
/// Structural upper bound on the pinned metadata preflight's charged slots.
/// Its table encodings charge fewer than 4 slots per input byte. This is not a
/// memory estimate; the OS address-space limit enforces actual containment.
pub(crate) const MAX_METADATA_ENTRIES: usize = MAX_INPUT_BYTES * 4;
pub(crate) const MAX_PIXELS: u64 = heic_rs::DEFAULT_MAX_PIXELS;

#[cfg(target_os = "linux")]
const ADDRESS_SPACE_BYTES: u64 = 536_870_912;
const REQUEST_MAGIC: &[u8; 8] = b"OCHHEIC\0";
const RESPONSE_MAGIC: &[u8; 8] = b"OCHJPEG\0";
const VERSION: u8 = 1;
const REQUEST_HEADER_BYTES: usize = 56;
const RESPONSE_HEADER_BYTES: usize = 16;

/// Content-free failures for one file. WorkerFailed includes OOM, decoder abort,
/// loader failure and other unsuccessful exits whose cause is not established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeicWorkerError {
    InputTooLarge,
    InvalidLimits,
    UnsupportedPlatform,
    ContainmentUnavailable,
    SerializationUnavailable,
    LaunchFailed,
    PipeFailed,
    Protocol,
    Timeout,
    WorkerFailed,
    AllocationFailed,
    Conversion(ImageConversionError),
}

/// Returns only complete JPEG bytes. The caller retains and normalizes the
/// original display filename; no filename or path enters the worker protocol.
/// content must be the complete attachment, never a prefix from a capped read.
/// Limits are explicit and transmitted unchanged to the decoder. Pixel ceilings
/// cannot exceed the pinned decoder's published default; coded-area accounting
/// remains a separate caller budget and is not a total-work or memory estimate.
pub(crate) fn convert_heic_isolated(
    content: &[u8],
    limits: HeicConversionLimits,
) -> Result<Vec<u8>, HeicWorkerError> {
    validate_limits(limits)?;
    if content.len() > limits.max_input_bytes {
        return Err(HeicWorkerError::InputTooLarge);
    }
    #[cfg(target_os = "linux")]
    {
        linux::convert(content, limits)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(HeicWorkerError::UnsupportedPlatform)
    }
}

/// Call as the first statement in main, before diagnostics, fonts, configuration
/// or GPUI. Some means the private worker request was handled and main must exit.
/// A misplaced or repeated worker argument also exits without starting the UI.
pub(crate) fn run_if_requested() -> Option<i32> {
    let mut arguments = std::env::args_os().skip(1);
    let first = arguments.next();
    if first.as_deref() != Some(std::ffi::OsStr::new(WORKER_ARGUMENT)) {
        let reserved = |argument: &std::ffi::OsStr| {
            argument
                .as_encoded_bytes()
                .starts_with(b"--attachment-heic")
        };
        return (first.as_deref().is_some_and(reserved)
            || arguments.any(|argument| reserved(&argument)))
        .then_some(64);
    }
    if arguments.next().is_some() {
        return Some(64);
    }
    // The worker is a distinct process. Suppress decoder panic diagnostics;
    // ordinary errors use protocol kinds and allocator aborts remain contained.
    std::panic::set_hook(Box::new(|_| {}));
    let mut stdout = io::stdout().lock();
    #[cfg(target_os = "linux")]
    let contained = linux::worker_limits_are_active();
    #[cfg(not(target_os = "linux"))]
    let contained = false;
    let result = if !contained {
        #[cfg(target_os = "linux")]
        let error = HeicWorkerError::ContainmentUnavailable;
        #[cfg(not(target_os = "linux"))]
        let error = HeicWorkerError::UnsupportedPlatform;
        write_response(&mut stdout, &Response::Unavailable(error))
    } else {
        serve_once(&mut io::stdin().lock(), &mut stdout)
    };
    Some(if result.is_ok() { 0 } else { 74 })
}

fn validate_limits(limits: HeicConversionLimits) -> Result<(), HeicWorkerError> {
    if limits.max_input_bytes > MAX_INPUT_BYTES
        || limits.max_output_bytes > MAX_OUTPUT_BYTES
        || limits.max_metadata_entries > MAX_METADATA_ENTRIES
        || limits.max_pixels > MAX_PIXELS
        || limits.max_decoded_pixels > MAX_PIXELS
    {
        return Err(HeicWorkerError::InvalidLimits);
    }
    Ok(())
}

enum Response {
    Jpeg(Vec<u8>),
    Rejected(ImageConversionError),
    Unavailable(HeicWorkerError),
}

fn serve_once(input: &mut impl Read, output: &mut impl Write) -> Result<(), HeicWorkerError> {
    let response = match read_request(input) {
        Ok((content, limits)) => {
            match attachment_images::convert_heic_to_jpeg("attachment.heic", &content, limits) {
                Ok(prepared) => Response::Jpeg(prepared.content),
                Err(error) => Response::Rejected(error),
            }
        }
        Err(_) => Response::Unavailable(HeicWorkerError::Protocol),
    };
    write_response(output, &response)
}

#[cfg(any(target_os = "linux", test))]
fn request_header(length: usize, limits: HeicConversionLimits) -> [u8; REQUEST_HEADER_BYTES] {
    // The caller validates the fixed ceilings before writing any header.
    let mut header = [0; REQUEST_HEADER_BYTES];
    header[..8].copy_from_slice(REQUEST_MAGIC);
    header[8] = VERSION;
    header[12..16].copy_from_slice(&(length as u32).to_be_bytes());
    for (index, value) in [
        limits.max_input_bytes as u64,
        limits.max_output_bytes as u64,
        limits.max_metadata_entries as u64,
        limits.max_pixels,
        limits.max_decoded_pixels,
    ]
    .into_iter()
    .enumerate()
    {
        let start = 16 + index * 8;
        header[start..start + 8].copy_from_slice(&value.to_be_bytes());
    }
    header
}

fn read_request(input: &mut impl Read) -> Result<(Vec<u8>, HeicConversionLimits), HeicWorkerError> {
    let mut header = [0; REQUEST_HEADER_BYTES];
    input
        .read_exact(&mut header)
        .map_err(|_| HeicWorkerError::PipeFailed)?;
    if &header[..8] != REQUEST_MAGIC || header[8] != VERSION || header[9..12] != [0; 3] {
        return Err(HeicWorkerError::Protocol);
    }
    let length =
        u32::from_be_bytes(header[12..16].try_into().expect("fixed header field")) as usize;
    let field = |index: usize| -> Result<u64, HeicWorkerError> {
        let start = 16 + index * 8;
        Ok(u64::from_be_bytes(
            header[start..start + 8]
                .try_into()
                .map_err(|_| HeicWorkerError::Protocol)?,
        ))
    };
    let size = |value| usize::try_from(value).map_err(|_| HeicWorkerError::InvalidLimits);
    let limits = HeicConversionLimits {
        max_input_bytes: size(field(0)?)?,
        max_output_bytes: size(field(1)?)?,
        max_metadata_entries: size(field(2)?)?,
        max_pixels: field(3)?,
        max_decoded_pixels: field(4)?,
    };
    validate_limits(limits)?;
    if length > limits.max_input_bytes {
        return Err(HeicWorkerError::InputTooLarge);
    }
    let content = read_payload(input, length)?;
    require_eof(input)?;
    Ok((content, limits))
}

fn read_payload(input: &mut impl Read, length: usize) -> Result<Vec<u8>, HeicWorkerError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| HeicWorkerError::AllocationFailed)?;
    bytes.resize(length, 0);
    input
        .read_exact(&mut bytes)
        .map_err(|_| HeicWorkerError::PipeFailed)?;
    Ok(bytes)
}

fn require_eof(input: &mut impl Read) -> Result<(), HeicWorkerError> {
    let mut extra = [0];
    loop {
        match input.read(&mut extra) {
            Ok(0) => return Ok(()),
            Ok(_) => return Err(HeicWorkerError::Protocol),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(HeicWorkerError::PipeFailed),
        }
    }
}

fn write_response(output: &mut impl Write, response: &Response) -> Result<(), HeicWorkerError> {
    let mut header = [0; RESPONSE_HEADER_BYTES];
    header[..8].copy_from_slice(RESPONSE_MAGIC);
    header[8] = VERSION;
    let bytes = match response {
        Response::Jpeg(bytes) if bytes.len() <= MAX_OUTPUT_BYTES && is_jpeg(bytes) => {
            bytes.as_slice()
        }
        Response::Jpeg(_) => return Err(HeicWorkerError::Protocol),
        Response::Rejected(error) => {
            header[9] = 1;
            header[10] = image_error_code(*error);
            &[]
        }
        Response::Unavailable(error) => {
            header[9] = 2;
            header[10] = match error {
                HeicWorkerError::Protocol => 1,
                HeicWorkerError::ContainmentUnavailable => 2,
                HeicWorkerError::UnsupportedPlatform => 3,
                _ => return Err(HeicWorkerError::Protocol),
            };
            &[]
        }
    };
    header[12..16].copy_from_slice(&(bytes.len() as u32).to_be_bytes());
    output
        .write_all(&header)
        .and_then(|()| output.write_all(bytes))
        .and_then(|()| output.flush())
        .map_err(|_| HeicWorkerError::PipeFailed)
}

#[cfg(any(target_os = "linux", test))]
fn read_response(
    input: &mut impl Read,
    max_output_bytes: usize,
) -> Result<Response, HeicWorkerError> {
    let mut header = [0; RESPONSE_HEADER_BYTES];
    input
        .read_exact(&mut header)
        .map_err(|_| HeicWorkerError::PipeFailed)?;
    if &header[..8] != RESPONSE_MAGIC || header[8] != VERSION || header[11] != 0 {
        return Err(HeicWorkerError::Protocol);
    }
    let length =
        u32::from_be_bytes(header[12..16].try_into().expect("fixed header field")) as usize;
    let response = match (header[9], header[10], length) {
        (0, 0, length) if length <= max_output_bytes && length <= MAX_OUTPUT_BYTES => {
            let bytes = read_payload(input, length)?;
            if !is_jpeg(&bytes) {
                return Err(HeicWorkerError::Protocol);
            }
            Response::Jpeg(bytes)
        }
        (1, code, 0) => Response::Rejected(image_error_from_code(code)?),
        (2, code, 0) => Response::Unavailable(match code {
            1 => HeicWorkerError::Protocol,
            2 => HeicWorkerError::ContainmentUnavailable,
            3 => HeicWorkerError::UnsupportedPlatform,
            _ => return Err(HeicWorkerError::Protocol),
        }),
        _ => return Err(HeicWorkerError::Protocol),
    };
    require_eof(input)?;
    Ok(response)
}

fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && bytes.starts_with(&[0xff, 0xd8]) && bytes.ends_with(&[0xff, 0xd9])
}

fn image_error_code(error: ImageConversionError) -> u8 {
    match error {
        ImageConversionError::InputTooLarge => 1,
        ImageConversionError::MetadataLimit => 2,
        ImageConversionError::PixelLimit => 3,
        ImageConversionError::InvalidDimensions => 4,
        ImageConversionError::InvalidImage => 5,
        ImageConversionError::UnsupportedImage => 6,
        ImageConversionError::InvalidDecodedImage => 7,
        ImageConversionError::OutputTooLarge => 8,
        ImageConversionError::AllocationFailed => 9,
        ImageConversionError::EncodeFailed => 10,
    }
}

#[cfg(any(target_os = "linux", test))]
fn image_error_from_code(code: u8) -> Result<ImageConversionError, HeicWorkerError> {
    Ok(match code {
        1 => ImageConversionError::InputTooLarge,
        2 => ImageConversionError::MetadataLimit,
        3 => ImageConversionError::PixelLimit,
        4 => ImageConversionError::InvalidDimensions,
        5 => ImageConversionError::InvalidImage,
        6 => ImageConversionError::UnsupportedImage,
        7 => ImageConversionError::InvalidDecodedImage,
        8 => ImageConversionError::OutputTooLarge,
        9 => ImageConversionError::AllocationFailed,
        10 => ImageConversionError::EncodeFailed,
        _ => return Err(HeicWorkerError::Protocol),
    })
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{self, Read, Write};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::{Mutex, mpsc};
    use std::thread::{self, ScopedJoinHandle};
    use std::time::{Duration, Instant};

    use crate::attachment_images::HeicConversionLimits;

    use super::{ADDRESS_SPACE_BYTES, HeicWorkerError, Response, WORKER_ARGUMENT};

    const PRLIMIT: &str = "/usr/bin/prlimit";
    const WALL_TIMEOUT: Duration = Duration::from_secs(30);
    const STATUS_INTERVAL: Duration = Duration::from_millis(10);
    static CONVERSION: Mutex<()> = Mutex::new(());

    pub(super) fn convert(
        content: &[u8],
        limits: HeicConversionLimits,
    ) -> Result<Vec<u8>, HeicWorkerError> {
        let _owner = CONVERSION
            .lock()
            .map_err(|_| HeicWorkerError::SerializationUnavailable)?;
        launch(content, limits, WALL_TIMEOUT)
    }

    fn containment_program_available() -> bool {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(PRLIMIT).is_ok_and(|metadata| {
            metadata.is_file()
                && metadata.uid() == 0
                && metadata.mode() & 0o6022 == 0
                && metadata.mode() & 0o111 != 0
        })
    }

    fn launch(
        content: &[u8],
        limits: HeicConversionLimits,
        timeout: Duration,
    ) -> Result<Vec<u8>, HeicWorkerError> {
        if !containment_program_available() {
            return Err(HeicWorkerError::ContainmentUnavailable);
        }
        let executable = std::env::current_exe().map_err(|_| HeicWorkerError::LaunchFailed)?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(HeicWorkerError::Timeout)?;
        let child = Command::new(PRLIMIT)
            .args(["--as=536870912:536870912", "--core=0:0", "--"])
            .arg(executable)
            .arg(WORKER_ARGUMENT)
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| HeicWorkerError::LaunchFailed)?;

        thread::scope(|scope| {
            // This guard lives inside the scope. Even unwinding kills/reaps the
            // child before scope's automatic joins can wait on blocked pipes.
            let mut child = OwnedChild {
                child,
                reaped: false,
            };
            let mut stdin = child
                .child
                .stdin
                .take()
                .ok_or(HeicWorkerError::PipeFailed)?;
            let mut stdout = child
                .child
                .stdout
                .take()
                .ok_or(HeicWorkerError::PipeFailed)?;
            let (sender, receiver) = mpsc::sync_channel(2);
            let written = sender.clone();
            let writer = thread::Builder::new()
                .name("heic-input".to_owned())
                .spawn_scoped(scope, move || {
                    let result = stdin
                        .write_all(&super::request_header(content.len(), limits))
                        .and_then(|()| stdin.write_all(content))
                        .and_then(|()| stdin.flush())
                        .map_err(|_| HeicWorkerError::PipeFailed);
                    drop(stdin);
                    let _ = written.send(Event::Written(result));
                })
                .map_err(|_| HeicWorkerError::PipeFailed)?;
            let reader = thread::Builder::new()
                .name("heic-output".to_owned())
                .spawn_scoped(scope, move || {
                    let response = super::read_response(&mut stdout, limits.max_output_bytes);
                    drop(stdout);
                    let _ = sender.send(Event::Read(response));
                })
                .map_err(|_| HeicWorkerError::PipeFailed)?;
            let result = supervise(&mut child.child, &receiver, deadline);
            let settled = child.finish(result.is_err());
            // If wait itself failed, Drop retries kill/reap before any join.
            drop(child);
            let joined = join_pipes(writer, reader);
            match (result, settled, joined) {
                (Err(HeicWorkerError::PipeFailed), Ok(status), _) if !status.success() => {
                    Err(HeicWorkerError::WorkerFailed)
                }
                (Err(error), _, _) => Err(error),
                (_, Err(error), _) | (_, _, Err(error)) => Err(error),
                (Ok(response), Ok(status), Ok(())) if status.success() => match response {
                    Response::Jpeg(bytes) => Ok(bytes),
                    Response::Rejected(error) => Err(HeicWorkerError::Conversion(error)),
                    Response::Unavailable(error) => Err(error),
                },
                _ => Err(HeicWorkerError::WorkerFailed),
            }
        })
    }

    enum Event {
        Written(Result<(), HeicWorkerError>),
        Read(Result<Response, HeicWorkerError>),
    }

    fn supervise(
        child: &mut Child,
        receiver: &mpsc::Receiver<Event>,
        deadline: Instant,
    ) -> Result<Response, HeicWorkerError> {
        let mut written = false;
        let mut response = None;
        let mut exited = false;
        loop {
            if Instant::now() >= deadline {
                return Err(HeicWorkerError::Timeout);
            }
            match child
                .try_wait()
                .map_err(|_| HeicWorkerError::WorkerFailed)?
            {
                Some(status) if !status.success() => return Err(HeicWorkerError::WorkerFailed),
                Some(_) => exited = true,
                None => {}
            }
            if written
                && exited
                && let Some(response) = response.take()
            {
                return Ok(response);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match receiver.recv_timeout(remaining.min(STATUS_INTERVAL)) {
                Ok(Event::Written(result)) => {
                    result?;
                    written = true;
                }
                Ok(Event::Read(result)) => response = Some(result?),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) if written && response.is_some() => {
                    // Both bounded pipe threads finished; only process exit remains.
                    thread::park_timeout(remaining.min(STATUS_INTERVAL));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(HeicWorkerError::PipeFailed);
                }
            }
        }
    }

    struct OwnedChild {
        child: Child,
        reaped: bool,
    }

    impl OwnedChild {
        fn finish(&mut self, stop: bool) -> Result<ExitStatus, HeicWorkerError> {
            if stop {
                let _ = self.child.kill();
            }
            let status = self
                .child
                .wait()
                .map_err(|_| HeicWorkerError::WorkerFailed)?;
            self.reaped = true;
            Ok(status)
        }
    }

    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if !self.reaped {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    fn join_pipes(
        writer: ScopedJoinHandle<'_, ()>,
        reader: ScopedJoinHandle<'_, ()>,
    ) -> Result<(), HeicWorkerError> {
        // Join both, including when the first thread panicked.
        let written = writer.join();
        let read = reader.join();
        if written.is_err() || read.is_err() {
            return Err(HeicWorkerError::PipeFailed);
        }
        Ok(())
    }

    pub(super) fn worker_limits_are_active() -> bool {
        // No libc dependency or unsafe ABI declaration is needed. This fixed
        // kernel file describes this process, not an application profile.
        let Ok(mut file) = std::fs::File::open("/proc/self/limits") else {
            return false;
        };
        let mut bytes = [0; 8193];
        let mut length = 0;
        while length < bytes.len() {
            match file.read(&mut bytes[length..]) {
                Ok(0) => break,
                Ok(count) => length += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
        length < bytes.len() && limits_text_is_contained(&bytes[..length])
    }

    fn limits_text_is_contained(bytes: &[u8]) -> bool {
        let Ok(text) = std::str::from_utf8(bytes) else {
            return false;
        };
        let mut address_space = false;
        let mut core = false;
        for line in text.lines() {
            let mut fields = line.split_ascii_whitespace();
            match (fields.next(), fields.next(), fields.next()) {
                (Some("Max"), Some("address"), Some("space")) => {
                    address_space = fields.next().and_then(|s| s.parse::<u64>().ok())
                        == Some(ADDRESS_SPACE_BYTES)
                        && fields.next().and_then(|s| s.parse::<u64>().ok())
                            == Some(ADDRESS_SPACE_BYTES)
                        && fields.next() == Some("bytes")
                        && fields.next().is_none();
                }
                (Some("Max"), Some("core"), Some("file")) if fields.next() == Some("size") => {
                    core = fields.next() == Some("0")
                        && fields.next() == Some("0")
                        && fields.next() == Some("bytes")
                        && fields.next().is_none();
                }
                _ => {}
            }
        }
        address_space && core
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn kernel_limit_verification_requires_both_exact_hard_and_soft_limits() {
            let text = b"Limit Soft Limit Hard Limit Units\nMax address space 536870912 536870912 bytes\nMax core file size 0 0 bytes\n";
            assert!(limits_text_is_contained(text), "both OS limits are active");
            for text in [
                b"Max address space unlimited unlimited bytes\nMax core file size 0 0 bytes\n".as_slice(),
                b"Max address space 536870912 unlimited bytes\nMax core file size 0 0 bytes\n",
                b"Max address space 536870912 536870912 bytes\nMax core file size 0 unlimited bytes\n",
                b"Max address space 536870912 536870912 bytes\n",
            ] {
                assert!(!limits_text_is_contained(text), "missing containment fails closed");
            }
        }

        #[test]
        fn joining_pipe_threads_drains_both_even_if_the_writer_panics() {
            let (sender, receiver) = mpsc::sync_channel(1);
            let joined = thread::scope(|scope| {
                let writer = scope.spawn(|| panic!("synthetic pipe writer failure"));
                let reader = scope.spawn(move || {
                    sender.send(()).expect("test receiver exists");
                });
                join_pipes(writer, reader)
            });
            assert_eq!(
                joined,
                Err(HeicWorkerError::PipeFailed),
                "a pipe panic fails this file"
            );
            assert_eq!(
                receiver.try_recv(),
                Ok(()),
                "the other pipe thread was also joined"
            );
        }

        #[test]
        fn zero_deadline_kills_reaps_and_drains_the_owned_child() {
            // The unit-test executable cannot enter desktop startup. This case
            // exercises the real launcher/kill/join path without claiming a
            // successful decoder subprocess. The built app needs its own run.
            let _owner = CONVERSION
                .lock()
                .expect("test serialization lock is healthy");
            let result = launch(
                b"synthetic input",
                super::super::tests::limits(),
                Duration::ZERO,
            );
            assert_eq!(
                result,
                Err(HeicWorkerError::Timeout),
                "the forced deadline cleans up the child"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn limits() -> HeicConversionLimits {
        HeicConversionLimits {
            max_input_bytes: MAX_INPUT_BYTES,
            max_output_bytes: MAX_OUTPUT_BYTES,
            max_metadata_entries: MAX_METADATA_ENTRIES,
            max_pixels: MAX_PIXELS,
            max_decoded_pixels: MAX_PIXELS,
        }
    }

    fn request(bytes: &[u8]) -> Vec<u8> {
        let mut message = request_header(bytes.len(), limits()).to_vec();
        message.extend_from_slice(bytes);
        message
    }

    #[test]
    fn request_round_trips_complete_bytes_and_the_same_explicit_limits() {
        let bytes = b"\0synthetic-input\xff";
        let (content, received) =
            read_request(&mut request(bytes).as_slice()).expect("complete frame");
        assert_eq!(
            content, bytes,
            "request decoding does not truncate or reinterpret bytes"
        );
        assert_eq!(
            received,
            limits(),
            "the exact caller budgets reach the converter"
        );
    }

    #[test]
    fn request_refuses_unknown_versions_truncation_trailing_bytes_and_oversized_lengths() {
        let valid = request(b"synthetic");
        for end in 0..valid.len() {
            assert!(
                read_request(&mut &valid[..end]).is_err(),
                "an incomplete request is rejected"
            );
        }
        for index in [0, 8, 9] {
            let mut bytes = valid.clone();
            bytes[index] = 255;
            assert!(
                read_request(&mut bytes.as_slice()).is_err(),
                "magic/version/reserved bits are strict"
            );
        }
        let mut bytes = valid.clone();
        bytes.push(0);
        assert!(
            read_request(&mut bytes.as_slice()).is_err(),
            "extra request bytes are rejected"
        );
        let mut header = request_header(MAX_INPUT_BYTES + 1, limits());
        assert!(
            read_request(&mut header.as_slice()).is_err(),
            "declared input limit is checked before allocation"
        );
        header[16..24].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(
            read_request(&mut header.as_slice()).is_err(),
            "unrepresentable or excessive limits fail closed"
        );
    }

    #[test]
    fn response_bounds_are_checked_before_reading_any_declared_payload() {
        let mut frame = [0; RESPONSE_HEADER_BYTES];
        frame[..8].copy_from_slice(RESPONSE_MAGIC);
        frame[8] = VERSION;
        frame[12..16].copy_from_slice(&((MAX_OUTPUT_BYTES + 1) as u32).to_be_bytes());
        assert!(
            matches!(
                read_response(&mut frame.as_slice(), MAX_OUTPUT_BYTES),
                Err(HeicWorkerError::Protocol)
            ),
            "an oversized output length is rejected before a payload allocation or read"
        );
        frame[12..16].copy_from_slice(&4u32.to_be_bytes());
        let mut bytes = frame.to_vec();
        bytes.extend_from_slice(&[0xff, 0xd8, 0xff, 0xd9, 0]);
        assert!(
            matches!(
                read_response(&mut bytes.as_slice(), MAX_OUTPUT_BYTES),
                Err(HeicWorkerError::Protocol)
            ),
            "one excess stdout byte cannot become a truncated success"
        );
    }

    #[test]
    fn every_decoder_error_has_a_content_free_known_wire_code() {
        for code in 1..=10 {
            let error = image_error_from_code(code).expect("known decoder error class");
            let mut bytes = Vec::new();
            write_response(&mut bytes, &Response::Rejected(error)).expect("bounded error frame");
            assert_eq!(
                bytes.len(),
                RESPONSE_HEADER_BYTES,
                "errors carry no payload"
            );
            assert!(
                matches!(read_response(&mut bytes.as_slice(), 0), Ok(Response::Rejected(received)) if received == error),
                "the error class round trips without paths or diagnostic strings"
            );
        }
        assert_eq!(
            image_error_from_code(255),
            Err(HeicWorkerError::Protocol),
            "unknown classes are rejected"
        );
    }

    #[test]
    fn pipe_errors_and_truncated_responses_never_become_a_success() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
        }
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
        }
        assert!(
            matches!(
                read_response(&mut Broken, MAX_OUTPUT_BYTES),
                Err(HeicWorkerError::PipeFailed)
            ),
            "a failed stdout read remains a pipe failure"
        );
        assert_eq!(
            write_response(
                &mut Broken,
                &Response::Rejected(ImageConversionError::InvalidImage)
            ),
            Err(HeicWorkerError::PipeFailed),
            "a failed protocol write is not reported as delivered"
        );
        let mut response = Vec::new();
        write_response(
            &mut response,
            &Response::Rejected(ImageConversionError::InvalidImage),
        )
        .expect("synthetic error response");
        for end in 0..response.len() {
            assert!(
                read_response(&mut &response[..end], MAX_OUTPUT_BYTES).is_err(),
                "every incomplete response is rejected"
            );
        }
        response[10] = 255;
        assert!(
            matches!(
                read_response(&mut response.as_slice(), MAX_OUTPUT_BYTES),
                Err(HeicWorkerError::Protocol)
            ),
            "an unknown worker error class cannot pass as a known conversion failure"
        );
    }

    #[test]
    fn source_fixture_crosses_the_actual_decoder_and_bounded_protocol() {
        let content = attachment_images::heic_fixture_for_worker_test();
        let mut output = Vec::new();
        serve_once(&mut request(&content).as_slice(), &mut output).expect("valid source fixture");
        let Response::Jpeg(bytes) = read_response(&mut output.as_slice(), MAX_OUTPUT_BYTES)
            .expect("complete JPEG response")
        else {
            panic!("the real source fixture must convert")
        };
        let jpeg = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)
            .expect("the actual image decoder accepts the JPEG");
        assert_eq!(
            (jpeg.width(), jpeg.height()),
            (16, 16),
            "source fixture dimensions survive protocol conversion"
        );
    }

    #[test]
    fn malformed_content_reports_a_file_error_without_payload_or_success() {
        let mut output = Vec::new();
        serve_once(
            &mut request(b"private-invalid-image").as_slice(),
            &mut output,
        )
        .expect("bounded failure response");
        assert_eq!(
            output.len(),
            RESPONSE_HEADER_BYTES,
            "failure output contains no input bytes"
        );
        assert!(
            matches!(
                read_response(&mut output.as_slice(), MAX_OUTPUT_BYTES),
                Ok(Response::Rejected(ImageConversionError::InvalidImage))
            ),
            "malformed input remains a failed file"
        );
    }
}
