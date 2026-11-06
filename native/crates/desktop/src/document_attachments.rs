//! Bounded, in-memory Office and OpenDocument attachment extraction.
//!
//! Matches `packages/ui/src/sync/document-attachments.ts` for the six supported extensions.
//! Generated headings and omission markers are attachment content, not application UI copy.
//! The caller supplies a complete bounded input and owns background execution, per-file error
//! presentation, transport and draft identity. No source bytes fall back to plain text on failure.
//! XML is UTF-8 XML 1.0; DTDs and custom entities are refused. Embedded images require
//! a matching PNG/JPEG/GIF/WebP signature and readable, nonzero dimensions. Pixel data is never
//! decoded. Decoder construction can inflate PNG metadata, so successful and failed inspections
//! reserve an aggregate validation budget and a separate per-decoder allocation allowance.
//! Parser work is capped at 100 MiB of XML reads, 250,000 nodes per part, 128 levels and 128
//! attributes per element. Repeated references and string-table reuse cannot bypass these bounds.
//! Reference-key comparisons have a 100 MiB aggregate allowance. Image inspection reserves at
//! most 100 MiB in total, charging encoded bytes plus a 1 MiB decoder allowance for each attempt.
//! Entry construction, decompression and owned-entry cleanup share a physical-read allowance
//! equal to the compressed input length. Seeking or opening another entry cannot replenish it.
//! Footer discovery uses the admitted EOF record with its unused ZIP comment removed, preventing
//! signature retries and repeated ZIP64 metadata allocation before the entry reader is created.

mod archive;
mod compressed_reads;
mod formats;
mod xml;
mod zip_admission;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Cursor;

use image::{ImageFormat, ImageReader, Limits};

use crate::attachment_files::NormalizedAttachmentFile;

use self::archive::Archive;

const MAX_ARCHIVE_BYTES: usize = 20 * 1024 * 1024;
const MAX_UNCOMPRESSED_BYTES: u64 = 100 * 1024 * 1024;
const MAX_ENTRY_BYTES: u64 = 25 * 1024 * 1024;
const MAX_XML_ENTRY_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 5_000;
const MAX_EMBEDDED_IMAGES: usize = 50;
const MAX_EMBEDDED_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_EMBEDDED_IMAGES_BYTES: usize = 40 * 1024 * 1024;
const MAX_IMAGE_VALIDATION_BYTES: usize = 100 * 1024 * 1024;
const IMAGE_DECODER_ALLOCATION_BYTES: usize = 1024 * 1024;
const MAX_REFERENCE_WORK_BYTES: usize = 100 * 1024 * 1024;
const MAX_TEXT_UNITS: usize = 500_000;
const TRUNCATION_NOTICE: &str = "\n\n[Document text truncated by OpenChamber]\n";

/// One converted document attachment followed by images in first-citation order.
#[derive(PartialEq, Eq)]
pub(crate) struct DocumentAttachments {
    pub text_file: NormalizedAttachmentFile,
    pub images: Vec<NormalizedAttachmentFile>,
}

impl fmt::Debug for DocumentAttachments {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DocumentAttachments")
            .field("text_bytes", &self.text_file.content.len())
            .field("images", &self.images.len())
            .finish_non_exhaustive()
    }
}

/// Content-free errors, scoped to exactly one input document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DocumentExtractionError {
    UnsupportedFormat,
    InputTooLarge,
    InvalidArchive,
    UnsafeArchivePath,
    DuplicateArchivePath,
    UnsupportedArchiveEntry,
    TooManyEntries,
    EntryTooLarge,
    XmlTooLarge,
    ExpandedArchiveTooLarge,
    InvalidXml,
    XmlComplexityLimit,
    ReferenceWorkLimit,
    MissingDocumentPart,
    InvalidDocumentStructure,
}

/// Converts complete ZIP bytes to a text/plain attachment retaining the original filename.
///
/// `reserved_filenames` contains other attachments' names and is compared case-insensitively.
/// The 500,000 text ceiling includes the truncation notice and counts UTF-16 code units, as
/// JavaScript does. A supplementary Unicode character is retained whole or omitted whole.
/// Only images with complete citations in the final bounded text are returned. This function
/// performs no filesystem, network, process, environment or global-state access.
pub(crate) fn extract_document_attachments(
    filename: &str,
    content: &[u8],
    reserved_filenames: &[String],
) -> Result<DocumentAttachments, DocumentExtractionError> {
    let extension = extension(filename);
    if !matches!(
        extension.as_str(),
        "docx" | "pptx" | "xlsx" | "odt" | "odp" | "ods"
    ) {
        return Err(DocumentExtractionError::UnsupportedFormat);
    }
    let archive = Archive::read(content)?;
    let mut images = EmbeddedImages::new(&archive, filename, reserved_filenames);
    let mut output = TextOutput::default();
    formats::extract(&extension, &archive, &mut images, &mut output)?;
    output.push("\n");
    let text = output.finish(&images.filenames);
    images
        .files
        .retain(|file| text.contains(&format!("[{}]", file.filename)));
    Ok(DocumentAttachments {
        text_file: NormalizedAttachmentFile {
            filename: filename.to_owned(),
            media_type: "text/plain",
            content: text.into_bytes(),
        },
        images: images.files,
    })
}

fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .map_or("", |(_, value)| value)
        .to_ascii_lowercase()
}

fn basename(name: &str) -> &str {
    name.rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("document")
}

fn image_prefix(name: &str) -> String {
    let name = basename(name);
    let name = name
        .rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map_or(name, |(stem, _)| stem);
    let mut result = String::new();
    let mut replacement = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
            result.push(character);
            replacement = false;
        } else if !replacement {
            result.push('-');
            replacement = true;
        }
    }
    if result.is_empty() {
        "document".to_owned()
    } else {
        result
    }
}

fn image_label(path: &str) -> String {
    let mut units = 0;
    basename(path)
        .chars()
        .take_while(|character| {
            units += character.len_utf16();
            units <= 200
        })
        .map(|character| {
            if character <= '\u{001f}' || matches!(character, '\u{007f}' | '[' | ']') {
                '_'
            } else {
                character
            }
        })
        .collect()
}

fn image_kind(extension: &str) -> Option<(ImageFormat, &'static str, &'static str)> {
    match extension {
        "png" => Some((ImageFormat::Png, "image/png", "png")),
        "jpg" | "jpeg" => Some((ImageFormat::Jpeg, "image/jpeg", "jpg")),
        "gif" => Some((ImageFormat::Gif, "image/gif", "gif")),
        "webp" => Some((ImageFormat::WebP, "image/webp", "webp")),
        _ => None,
    }
}

fn valid_image(bytes: &[u8], expected: ImageFormat) -> bool {
    if image::guess_format(bytes).ok() != Some(expected) {
        return false;
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), expected);
    let mut limits = Limits::default();
    // image 0.25.10 forwards this allowance to png::Decoder before read_info, including iCCP
    // inflation. It is not a hard allocation limit for every other image decoder.
    limits.max_alloc = Some(IMAGE_DECODER_ALLOCATION_BYTES as u64);
    reader.limits(limits);
    reader
        .into_dimensions()
        .is_ok_and(|(width, height)| width != 0 && height != 0)
}

struct EmbeddedImages<'archive> {
    archive: &'archive Archive,
    prefix: String,
    files: Vec<NormalizedAttachmentFile>,
    filenames: BTreeMap<String, String>,
    citations: BTreeMap<String, String>,
    reserved: BTreeSet<String>,
    count: usize,
    bytes: usize,
    reference_work_bytes: usize,
    validation_work_bytes: usize,
}

impl<'archive> EmbeddedImages<'archive> {
    fn new(archive: &'archive Archive, name: &str, reserved: &[String]) -> Self {
        Self {
            archive,
            prefix: image_prefix(name),
            files: Vec::new(),
            filenames: BTreeMap::new(),
            citations: BTreeMap::new(),
            reserved: reserved.iter().map(|name| name.to_lowercase()).collect(),
            count: 0,
            bytes: 0,
            reference_work_bytes: 0,
            validation_work_bytes: 0,
        }
    }

    /// Paths are already canonicalized by relationship/direct-reference resolution.
    fn citation(&mut self, path: Option<&str>) -> Result<String, DocumentExtractionError> {
        let Some(path) = path else {
            return Ok("[Embedded image reference could not be resolved]".to_owned());
        };
        // Charge before comparing potentially huge map keys. Relationship citations additionally
        // cache their result by relationship identity, so repeated targets bypass this scan.
        self.reference_work_bytes = self
            .reference_work_bytes
            .checked_add(path.len())
            .filter(|work| *work <= MAX_REFERENCE_WORK_BYTES)
            .ok_or(DocumentExtractionError::ReferenceWorkLimit)?;
        if let Some(citation) = self.citations.get(path) {
            return Ok(citation.clone());
        }
        let citation = self.new_citation(path);
        self.citations.insert(path.to_owned(), citation.clone());
        Ok(citation)
    }

    fn new_citation(&mut self, path: &str) -> String {
        if path.is_empty() {
            return "[Unsafe embedded image path omitted]".to_owned();
        }
        let label = image_label(path);
        let bytes = self.archive.bytes(path);
        let kind = image_kind(&extension(path));
        let (Some(bytes), Some((format, media_type, output_extension))) = (bytes, kind) else {
            return format!("[Unsupported embedded image omitted: {label}]");
        };
        // Both attachment admission and validation-work admission precede decoder construction.
        if self.files.len() >= MAX_EMBEDDED_IMAGES
            || bytes.len() > MAX_EMBEDDED_IMAGE_BYTES
            || bytes.len() > MAX_EMBEDDED_IMAGES_BYTES - self.bytes
        {
            return format!("[Embedded image omitted by attachment limits: {label}]");
        }
        // Reserve worst-case PNG metadata inflation, even if construction fails. Other decoders
        // can copy their encoded input before applying image::Limits, so charge those bytes too.
        let work = bytes.len() + IMAGE_DECODER_ALLOCATION_BYTES;
        if work > MAX_IMAGE_VALIDATION_BYTES - self.validation_work_bytes {
            return format!("[Embedded image omitted by validation limits: {label}]");
        }
        self.validation_work_bytes += work;
        if !valid_image(bytes, format) {
            return format!("[Invalid embedded image omitted: {label}]");
        }
        let filename = loop {
            self.count += 1;
            let candidate = format!("{}-image-{}.{output_extension}", self.prefix, self.count);
            if self.reserved.insert(candidate.to_lowercase()) {
                break candidate;
            }
        };
        self.bytes += bytes.len();
        self.files.push(NormalizedAttachmentFile {
            filename: filename.clone(),
            media_type,
            content: bytes.to_vec(),
        });
        let citation = format!("[{filename}]");
        self.filenames.insert(path.to_owned(), filename);
        citation
    }
}

/// Stores at most one Unicode scalar beyond the ceiling to detect actual truncation.
#[derive(Default)]
struct TextOutput {
    text: String,
    units: usize,
}

impl TextOutput {
    fn full(&self) -> bool {
        self.units > MAX_TEXT_UNITS
    }

    fn push(&mut self, value: &str) {
        for character in value.chars() {
            if self.units > MAX_TEXT_UNITS {
                break;
            }
            self.units += character.len_utf16();
            self.text.push(character);
        }
    }

    fn section(&mut self, value: &str) {
        if !self.text.is_empty() {
            self.push("\n\n");
        }
        self.push(value);
    }

    fn finish(mut self, filenames: &BTreeMap<String, String>) -> String {
        if self.units <= MAX_TEXT_UNITS {
            return self.text;
        }
        let budget = MAX_TEXT_UNITS - TRUNCATION_NOTICE.encode_utf16().count();
        let mut units = 0;
        let mut end = 0;
        for (offset, character) in self.text.char_indices() {
            if units + character.len_utf16() > budget {
                break;
            }
            units += character.len_utf16();
            end = offset + character.len_utf8();
        }
        let prefix = &self.text[..end];
        if let Some(open) = prefix.rfind('[')
            && prefix.rfind(']').is_none_or(|close| open > close)
        {
            let tail = &self.text[open + 1..];
            if filenames
                .values()
                .any(|name| tail.starts_with(&format!("{name}]")) || name.starts_with(tail))
            {
                end = open;
            }
        }
        self.text.truncate(end);
        self.text.push_str(TRUNCATION_NOTICE);
        self.text
    }
}
