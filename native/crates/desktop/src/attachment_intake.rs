//! Blocking, per-file attachment admission before IDs, storage or transport.
//!
//! The caller captures draft ownership and generation separately, then invokes this boundary on
//! its background executor. One process-local gate serializes reads and conversion. A result
//! contains complete normalized files only, with the document text first and cited images after
//! it. Multiple returned files share one source document; the caller assigns that association
//! and attachment IDs only after success. Errors cannot alter any other file's outcome.
//!
//! Local paths are explicit user selections. They are opened unchanged, including selected
//! symlinks, and never returned or persisted. The opened handle must identify a regular file.
//! Already-read remote references enter as owned bytes; this module performs no network access.
//! HEIC decoding runs exclusively through the existing contained worker with its production
//! address-space and wall-time limits. There is no in-process decoding fallback.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek};
use std::path::PathBuf;
use std::sync::Mutex;

use gpui_kit::ImageFormat;
use openchamber_native_transport::attachment::{
    AttachmentError, AttachmentUpload, MAX_PREPARED_ATTACHMENT_BYTES,
};

use crate::attachment_files::{
    self, MAX_INLINE_ATTACHMENT_BYTES, NormalizationError, NormalizedAttachmentFile,
    RequiredConversion,
};
use crate::attachment_images::{self, HeicConversionLimits};
use crate::clipboard_attachment::{self, ClipboardImageError};
use crate::document_attachments::{self, DocumentExtractionError};
use crate::heic_worker::{self, HeicWorkerError};

static PREPARATION: Mutex<()> = Mutex::new(());

/// Captured input authority. Memory must contain the complete source, never a capped prefix.
pub(crate) enum AttachmentInputSource {
    /// The exact picker/drop/paste selection, with no workspace-relative interpretation.
    LocalPath(PathBuf),
    /// An owned selected handle. Preparation rewinds it and checks its own metadata.
    #[cfg(test)]
    LocalFile(File),
    /// Complete picker/paste/drop bytes, or a completed remote-reference read.
    Bytes(Vec<u8>),
    /// Complete encoded clipboard bytes requiring bounded PNG conversion.
    ClipboardImage { bytes: Vec<u8>, format: ImageFormat },
}

impl fmt::Debug for AttachmentInputSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LocalPath(_) => formatter.write_str("LocalPath(<redacted>)"),
            #[cfg(test)]
            Self::LocalFile(_) => formatter.write_str("LocalFile(<redacted>)"),
            Self::Bytes(bytes) => formatter
                .debug_struct("Bytes")
                .field("byte_len", &bytes.len())
                .finish_non_exhaustive(),
            Self::ClipboardImage { bytes, format } => formatter
                .debug_struct("ClipboardImage")
                .field("format", format)
                .field("byte_len", &bytes.len())
                .finish_non_exhaustive(),
        }
    }
}

/// One file's captured preparation request, independent of the currently selected draft.
pub(crate) struct AttachmentIntake {
    pub source: AttachmentInputSource,
    pub filename: String,
    pub declared_media_type: Option<String>,
    /// Passed unchanged to document extraction for case-insensitive citation-name reservations.
    pub reserved_filenames: Vec<String>,
}

impl fmt::Debug for AttachmentIntake {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttachmentIntake")
            .field("source", &self.source)
            .field("reserved_names", &self.reserved_filenames.len())
            .finish_non_exhaustive()
    }
}

/// Content-free failures for exactly one input. Underlying diagnostic strings are discarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachmentIntakeError {
    InputTooLarge,
    NotRegular,
    Read(io::ErrorKind),
    Unsupported,
    MalformedHar,
    Image(HeicWorkerError),
    Clipboard(ClipboardImageError),
    Document(DocumentExtractionError),
    InvalidFilename,
    OutputTooLarge,
    SerializationUnavailable,
}

/// Reads and normalizes one complete source on the caller's background executor.
///
/// A successful vector is always nonempty, including for an empty text file. For a document,
/// index 0 is its original filename with text/plain content; later files are the converter's
/// cited images in citation order. Every name and prepared byte length passes the transport's
/// admission policy before any result leaves this function. Unsafe names are refused unchanged.
/// No spool, journal, draft, network, ID allocation or UI work occurs here.
pub(crate) fn prepare_attachment(
    input: AttachmentIntake,
) -> Result<Vec<NormalizedAttachmentFile>, AttachmentIntakeError> {
    let _preparation = PREPARATION
        .lock()
        .map_err(|_| AttachmentIntakeError::SerializationUnavailable)?;
    validate_filename(&input.filename)?;
    let clipboard_format = match &input.source {
        AttachmentInputSource::ClipboardImage { format, .. } => match format {
            ImageFormat::Bmp | ImageFormat::Tiff | ImageFormat::Ico | ImageFormat::Pnm => {
                Some(*format)
            }
            _ => return Err(AttachmentIntakeError::Unsupported),
        },
        _ => None,
    };
    let content = read_source(input.source)?;
    let files = if let Some(format) = clipboard_format {
        vec![
            clipboard_attachment::prepare_clipboard_image(&content, format, input.filename)
                .map_err(AttachmentIntakeError::Clipboard)?,
        ]
    } else {
        let normalized = attachment_files::normalize_attachment_file(
            &input.filename,
            input.declared_media_type.as_deref().unwrap_or_default(),
            &content,
            MAX_INLINE_ATTACHMENT_BYTES,
        );
        match normalized {
            Ok(file) => vec![file],
            Err(NormalizationError::UnsupportedConversion(RequiredConversion::HeicImage)) => {
                let bytes = heic_worker::convert_heic_isolated(&content, heic_limits())
                    .map_err(AttachmentIntakeError::Image)?;
                vec![NormalizedAttachmentFile {
                    filename: attachment_images::jpeg_filename(&input.filename),
                    media_type: "image/jpeg",
                    content: bytes,
                }]
            }
            Err(NormalizationError::UnsupportedConversion(
                RequiredConversion::OfficeDocument | RequiredConversion::OpenDocument,
            )) => {
                let document = document_attachments::extract_document_attachments(
                    &input.filename,
                    &content,
                    &input.reserved_filenames,
                )
                .map_err(|error| match error {
                    DocumentExtractionError::UnsupportedFormat => {
                        AttachmentIntakeError::Unsupported
                    }
                    DocumentExtractionError::InputTooLarge => AttachmentIntakeError::InputTooLarge,
                    _ => AttachmentIntakeError::Document(error),
                })?;
                let mut files = Vec::with_capacity(1 + document.images.len());
                files.push(document.text_file);
                files.extend(document.images);
                files
            }
            Err(NormalizationError::InputTooLarge) => {
                return Err(AttachmentIntakeError::InputTooLarge);
            }
            Err(NormalizationError::UnsupportedContent) => {
                return Err(AttachmentIntakeError::Unsupported);
            }
            Err(NormalizationError::MalformedHar) => {
                return Err(AttachmentIntakeError::MalformedHar);
            }
        }
    };
    for file in &files {
        validate_filename(&file.filename)?;
        if file.content.len() > MAX_PREPARED_ATTACHMENT_BYTES {
            return Err(AttachmentIntakeError::OutputTooLarge);
        }
    }
    Ok(files)
}

fn validate_filename(filename: &str) -> Result<(), AttachmentIntakeError> {
    // The transport owns this policy. An empty payload avoids copying complete prepared bytes
    // just to inspect a name; actual payload lengths are checked separately against its constant.
    AttachmentUpload::new(filename, Vec::new())
        .map(|_| ())
        .map_err(|error| match error {
            AttachmentError::InvalidFilename => AttachmentIntakeError::InvalidFilename,
            AttachmentError::FileTooLarge => AttachmentIntakeError::OutputTooLarge,
            _ => AttachmentIntakeError::Unsupported,
        })
}

fn heic_limits() -> HeicConversionLimits {
    HeicConversionLimits {
        max_input_bytes: MAX_INLINE_ATTACHMENT_BYTES,
        max_output_bytes: heic_worker::MAX_OUTPUT_BYTES,
        max_metadata_entries: heic_worker::MAX_METADATA_ENTRIES,
        max_pixels: heic_worker::MAX_PIXELS,
        max_decoded_pixels: heic_worker::MAX_PIXELS,
    }
}

fn read_source(source: AttachmentInputSource) -> Result<Vec<u8>, AttachmentIntakeError> {
    match source {
        AttachmentInputSource::LocalPath(path) => {
            // Check before open so ordinary directory, device and FIFO selections are refused.
            // Selected symlinks retain normal filesystem resolution. The handle check below is
            // authoritative if this metadata becomes stale before open.
            admit_metadata(&fs::metadata(&path).map_err(read_error)?)?;
            let mut options = OpenOptions::new();
            options.read(true);
            #[cfg(target_os = "linux")]
            {
                use std::os::unix::fs::OpenOptionsExt;

                // Linux x86_64/aarch64 O_NONBLOCK prevents a raced-in FIFO from blocking open.
                // It has no effect on regular-file reads. No O_NOFOLLOW policy is imposed.
                options.custom_flags(0x800);
            }
            read_file(options.open(path).map_err(read_error)?)
        }
        #[cfg(test)]
        AttachmentInputSource::LocalFile(file) => read_file(file),
        AttachmentInputSource::Bytes(bytes)
        | AttachmentInputSource::ClipboardImage { bytes, .. } => {
            if bytes.len() > MAX_INLINE_ATTACHMENT_BYTES {
                return Err(AttachmentIntakeError::InputTooLarge);
            }
            Ok(bytes)
        }
    }
}

fn admit_metadata(metadata: &fs::Metadata) -> Result<(), AttachmentIntakeError> {
    if !metadata.is_file() {
        return Err(AttachmentIntakeError::NotRegular);
    }
    if metadata.len() > MAX_INLINE_ATTACHMENT_BYTES as u64 {
        return Err(AttachmentIntakeError::InputTooLarge);
    }
    Ok(())
}

fn read_file(mut file: File) -> Result<Vec<u8>, AttachmentIntakeError> {
    admit_metadata(&file.metadata().map_err(read_error)?)?;
    file.rewind().map_err(read_error)?;
    read_bounded(&mut file)
}

fn read_bounded(reader: &mut impl Read) -> Result<Vec<u8>, AttachmentIntakeError> {
    // Bound the reader before allocating. Metadata never selects a capacity or authorizes a
    // prefix: one extra byte detects files that grow beyond admission after the handle check.
    let mut bounded = reader.take((MAX_INLINE_ATTACHMENT_BYTES + 1) as u64);
    let mut bytes = Vec::new();
    bounded.read_to_end(&mut bytes).map_err(read_error)?;
    if bytes.len() > MAX_INLINE_ATTACHMENT_BYTES {
        return Err(AttachmentIntakeError::InputTooLarge);
    }
    Ok(bytes)
}

fn read_error(error: io::Error) -> AttachmentIntakeError {
    AttachmentIntakeError::Read(error.kind())
}

#[cfg(test)]
#[path = "attachment_intake_tests.rs"]
mod tests;
