//! ZIP metadata admission and capped decompression, without filesystem paths.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};

use zip::ZipReadOptions;
use zip::read::{ArchiveOffset, Config};
use zip::unstable::read::ZipIterable;

use super::compressed_reads::{CompressedReadBudget, CompressedReader};
use super::xml::Xml;
use super::{
    DocumentExtractionError, MAX_ARCHIVE_BYTES, MAX_ARCHIVE_ENTRIES, MAX_ENTRY_BYTES,
    MAX_UNCOMPRESSED_BYTES, MAX_XML_ENTRY_BYTES,
};

pub(super) struct Archive {
    entries: BTreeMap<String, Vec<u8>>,
    xml_work_bytes: Cell<u64>,
}

impl Archive {
    pub(super) fn read(content: &[u8]) -> Result<Self, DocumentExtractionError> {
        if content.len() > MAX_ARCHIVE_BYTES {
            return Err(DocumentExtractionError::InputTooLarge);
        }
        let admitted = super::zip_admission::admit(content)?;
        let reader_view = admitted.reader_view(content)?;
        // Admission makes the final uncommented EOCD the reader's first accepted candidate. This
        // bounds discovery and its pre-read ZIP64 allocation, before any entry quota can apply.
        // The iterable reader then visits each physically admitted central record once per pass;
        // its variable-length allocations are confined to those disjoint record bounds.
        let mut zip = ZipIterable::try_new(
            Cursor::new(reader_view.as_ref()),
            Config {
                archive_offset: ArchiveOffset::Known(0),
            },
        )
        .map_err(|_| DocumentExtractionError::InvalidArchive)?;
        if zip.len() != admitted.entries {
            return Err(DocumentExtractionError::InvalidArchive);
        }
        let mut selected = Vec::new();
        let mut names = BTreeSet::new();
        let mut total = 0_u64;
        let files = zip
            .files()
            .map_err(|_| DocumentExtractionError::InvalidArchive)?;
        if files.reader_mut().position() != admitted.directory_start {
            return Err(DocumentExtractionError::InvalidArchive);
        }
        // A complete metadata-only pass precedes any decompressor. At most 5000 records were
        // physically counted, including duplicate names and metadata-only binaries.
        for (index, entry) in files.enumerate() {
            if index >= MAX_ARCHIVE_ENTRIES {
                return Err(DocumentExtractionError::TooManyEntries);
            }
            let entry = entry.map_err(|_| DocumentExtractionError::InvalidArchive)?;
            let name = entry
                .name()
                .map_err(|_| DocumentExtractionError::UnsafeArchivePath)?;
            let canonical = name.strip_suffix('/').unwrap_or(&name);
            if normalize_path(&name).as_deref() != Some(canonical)
                || canonical.is_empty()
                || name.contains('\\')
                || name
                    .chars()
                    .any(|character| character <= '\u{001f}' || character == '\u{007f}')
                || (name.as_bytes().get(1) == Some(&b':')
                    && name.as_bytes()[0].is_ascii_alphabetic())
            {
                return Err(DocumentExtractionError::UnsafeArchivePath);
            }
            if !names.insert(name.to_string()) {
                return Err(DocumentExtractionError::DuplicateArchivePath);
            }
            if entry.encrypted() || entry.is_symlink() {
                return Err(DocumentExtractionError::UnsupportedArchiveEntry);
            }
            if entry.size() > MAX_ENTRY_BYTES {
                return Err(DocumentExtractionError::EntryTooLarge);
            }
            let extension = super::extension(&name);
            let is_xml = matches!(extension.as_str(), "xml" | "rels");
            if is_xml && entry.size() > MAX_XML_ENTRY_BYTES {
                return Err(DocumentExtractionError::XmlTooLarge);
            }
            total = total
                .checked_add(entry.size())
                .ok_or(DocumentExtractionError::ExpandedArchiveTooLarge)?;
            if total > MAX_UNCOMPRESSED_BYTES {
                return Err(DocumentExtractionError::ExpandedArchiveTooLarge);
            }
            if !entry.is_dir() && (is_xml || super::image_kind(&extension).is_some()) {
                selected.push((index, name.into_owned(), entry.size(), is_xml));
            }
        }
        let mut entries = BTreeMap::new();
        let mut actual_total = 0_u64;
        let mut selected = selected.into_iter().peekable();
        let files = zip
            .files()
            .map_err(|_| DocumentExtractionError::InvalidArchive)?;
        // Disjoint normal entry headers and encoded payloads occupy at most the physical input.
        // A shared non-resetting quota also bounds lying/overlapping claims and owned-entry Drop.
        let compressed_reads = CompressedReadBudget::new(content.len());
        let mut index = 0;
        while let Some(entry) = files.next() {
            let entry = entry.map_err(|_| DocumentExtractionError::InvalidArchive)?;
            let current = index;
            index += 1;
            if selected
                .peek()
                .is_none_or(|(index, _, _, _)| *index != current)
            {
                continue;
            }
            let (_, name, declared_size, is_xml) =
                selected.next().expect("selected entry matches this index");
            let limit = if is_xml {
                MAX_XML_ENTRY_BYTES
            } else {
                MAX_ENTRY_BYTES
            };
            let read = {
                let mut source = CompressedReader::new(files.reader_mut(), &compressed_reads);
                let mut entry = entry
                    .with_reader(&mut source, ZipReadOptions::default())
                    .map_err(|_| DocumentExtractionError::InvalidArchive)?;
                let mut bytes = Vec::new();
                // The extra byte detects a lying expanded size. The source also bounds raw bytes
                // drained by ZipFile::drop after a short Deflate stream or an early read error.
                let result = entry.by_ref().take(limit + 1).read_to_end(&mut bytes);
                drop(entry);
                result.map(|_| bytes)
            };
            // ZipFile::drop discards its copy error, so inspect the quota only after Drop ran.
            if compressed_reads.rejected() {
                return Err(DocumentExtractionError::InvalidArchive);
            }
            let bytes = read.map_err(|_| DocumentExtractionError::InvalidArchive)?;
            if bytes.len() as u64 > limit {
                return Err(if is_xml {
                    DocumentExtractionError::XmlTooLarge
                } else {
                    DocumentExtractionError::EntryTooLarge
                });
            }
            if bytes.len() as u64 != declared_size {
                return Err(DocumentExtractionError::InvalidArchive);
            }
            actual_total += bytes.len() as u64;
            if actual_total > MAX_UNCOMPRESSED_BYTES {
                return Err(DocumentExtractionError::ExpandedArchiveTooLarge);
            }
            entries.insert(name, bytes);
        }
        Ok(Self {
            entries,
            xml_work_bytes: Cell::new(0),
        })
    }

    pub(super) fn bytes(&self, path: &str) -> Option<&[u8]> {
        self.entries.get(path).map(Vec::as_slice)
    }

    pub(super) fn paths(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    pub(super) fn xml(&self, path: &str) -> Result<Xml, DocumentExtractionError> {
        // A relationship may target any archive name. Binary entries never enter XML decoding.
        if !matches!(super::extension(path).as_str(), "xml" | "rels") {
            return Err(DocumentExtractionError::InvalidDocumentStructure);
        }
        let bytes = self
            .bytes(path)
            .ok_or(DocumentExtractionError::MissingDocumentPart)?;
        // Repeated worksheet/drawing relationships must not multiply XML parsing without a cap.
        // This is a local work counter, not a cache or global parser state.
        let work = self.xml_work_bytes.get() + bytes.len() as u64;
        if work > MAX_UNCOMPRESSED_BYTES {
            return Err(DocumentExtractionError::XmlComplexityLimit);
        }
        self.xml_work_bytes.set(work);
        Xml::parse(bytes)
    }
}

pub(super) fn normalize_path(path: &str) -> Option<String> {
    let replaced = path.replace('\\', "/");
    let mut segments = Vec::new();
    for segment in replaced.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            _ => segments.push(segment),
        }
    }
    Some(segments.join("/"))
}

pub(super) fn resolve_path(source: &str, target: &str) -> Option<String> {
    // Relationships are references inside an admitted archive. There is no URL fetch fallback.
    if let Some(target) = target.strip_prefix('/') {
        return normalize_path(target);
    }
    let directory = source
        .rsplit_once('/')
        .map_or("", |(directory, _)| directory);
    normalize_path(&format!("{directory}/{target}"))
}

pub(super) fn relationships_path(source: &str) -> String {
    let (directory, filename) = source.rsplit_once('/').unwrap_or(("", source));
    if directory.is_empty() {
        format!("_rels/{filename}.rels")
    } else {
        format!("{directory}/_rels/{filename}.rels")
    }
}
