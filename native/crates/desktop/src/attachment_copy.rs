//! Localized, content-free explanations for attachment preparation failures.

use gpui_kit::{App, SharedString};

use crate::attachment_images::ImageConversionError;
use crate::attachment_intake::AttachmentIntakeError;
use crate::attachment_storage::AttachmentStorageFailure;
use crate::clipboard_attachment::ClipboardImageError;
use crate::document_attachments::DocumentExtractionError;
use crate::heic_worker::HeicWorkerError;
use crate::locale;

pub(crate) fn storage_failure(error: &AttachmentStorageFailure, cx: &App) -> SharedString {
    locale::strings(cx).get(failure_key(error))
}

fn failure_key(error: &AttachmentStorageFailure) -> &'static str {
    match error {
        AttachmentStorageFailure::Capacity => "chat.fileAttachment.failure.capacity",
        AttachmentStorageFailure::Unavailable | AttachmentStorageFailure::IdentityExhausted => {
            "chat.fileAttachment.failure.unavailable"
        }
        AttachmentStorageFailure::Persistence
        | AttachmentStorageFailure::Put(_)
        | AttachmentStorageFailure::SpoolUncertain => {
            "chat.fileAttachment.failure.storageUnconfirmed"
        }
        AttachmentStorageFailure::Referenced => "chat.fileAttachment.failure.referenced",
        AttachmentStorageFailure::Intake(error) => match error {
            AttachmentIntakeError::InputTooLarge => "chat.fileAttachment.failure.inputTooLarge",
            AttachmentIntakeError::NotRegular => "chat.fileAttachment.failure.notRegular",
            AttachmentIntakeError::Read(_) => "chat.fileAttachment.failure.readFailed",
            AttachmentIntakeError::Unsupported => "chat.fileAttachment.failure.unsupported",
            AttachmentIntakeError::InvalidFilename => "chat.fileAttachment.failure.invalidFilename",
            AttachmentIntakeError::MalformedHar => "chat.fileAttachment.failure.malformedHar",
            AttachmentIntakeError::OutputTooLarge => "chat.fileAttachment.failure.outputTooLarge",
            AttachmentIntakeError::SerializationUnavailable => {
                "chat.fileAttachment.failure.unavailable"
            }
            AttachmentIntakeError::Clipboard(error) => match error {
                ClipboardImageError::InputTooLarge => "chat.fileAttachment.failure.inputTooLarge",
                ClipboardImageError::OutputTooLarge => "chat.fileAttachment.failure.outputTooLarge",
                ClipboardImageError::Limit => "chat.fileAttachment.failure.imageLimit",
                ClipboardImageError::Unsupported => "chat.fileAttachment.failure.unsupported",
                ClipboardImageError::Invalid
                | ClipboardImageError::Allocation
                | ClipboardImageError::Encode => "chat.fileAttachment.failure.imageFailed",
            },
            AttachmentIntakeError::Document(error) => match error {
                DocumentExtractionError::UnsupportedFormat => {
                    "chat.fileAttachment.failure.unsupported"
                }
                DocumentExtractionError::InputTooLarge => {
                    "chat.fileAttachment.failure.inputTooLarge"
                }
                DocumentExtractionError::TooManyEntries
                | DocumentExtractionError::EntryTooLarge
                | DocumentExtractionError::XmlTooLarge
                | DocumentExtractionError::ExpandedArchiveTooLarge
                | DocumentExtractionError::XmlComplexityLimit
                | DocumentExtractionError::ReferenceWorkLimit => {
                    "chat.fileAttachment.failure.documentLimit"
                }
                DocumentExtractionError::InvalidArchive
                | DocumentExtractionError::UnsafeArchivePath
                | DocumentExtractionError::DuplicateArchivePath
                | DocumentExtractionError::UnsupportedArchiveEntry
                | DocumentExtractionError::InvalidXml
                | DocumentExtractionError::MissingDocumentPart
                | DocumentExtractionError::InvalidDocumentStructure => {
                    "chat.fileAttachment.failure.documentFailed"
                }
            },
            AttachmentIntakeError::Image(error) => match error {
                HeicWorkerError::Timeout => "chat.fileAttachment.failure.heicTimeout",
                HeicWorkerError::InputTooLarge
                | HeicWorkerError::Conversion(ImageConversionError::InputTooLarge) => {
                    "chat.fileAttachment.failure.inputTooLarge"
                }
                HeicWorkerError::InvalidLimits
                | HeicWorkerError::UnsupportedPlatform
                | HeicWorkerError::ContainmentUnavailable
                | HeicWorkerError::SerializationUnavailable
                | HeicWorkerError::LaunchFailed => "chat.fileAttachment.failure.heicUnavailable",
                HeicWorkerError::Conversion(
                    ImageConversionError::MetadataLimit
                    | ImageConversionError::PixelLimit
                    | ImageConversionError::OutputTooLarge,
                ) => "chat.fileAttachment.failure.imageLimit",
                HeicWorkerError::PipeFailed
                | HeicWorkerError::Protocol
                | HeicWorkerError::WorkerFailed
                | HeicWorkerError::AllocationFailed
                | HeicWorkerError::Conversion(_) => "chat.fileAttachment.failure.heicFailed",
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconfirmed_storage_and_worker_exits_do_not_claim_definite_data_loss_or_oom() {
        assert_eq!(
            failure_key(&AttachmentStorageFailure::SpoolUncertain),
            "chat.fileAttachment.failure.storageUnconfirmed"
        );
        assert_eq!(
            failure_key(&AttachmentStorageFailure::Intake(
                AttachmentIntakeError::Image(HeicWorkerError::WorkerFailed)
            )),
            "chat.fileAttachment.failure.heicFailed"
        );
        assert_eq!(
            failure_key(&AttachmentStorageFailure::Intake(
                AttachmentIntakeError::Image(HeicWorkerError::Timeout)
            )),
            "chat.fileAttachment.failure.heicTimeout"
        );
    }
}
