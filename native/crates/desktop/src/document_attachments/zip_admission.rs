//! Allocation-free central-directory count admission before per-entry ZIP metadata exists.
//!
//! The fixed footer fields and record lengths follow zip 9.2.2's cached format definitions.
//! Its fixed-block parsing trait is private, so this pass reads only those framing integers.
//! The public exact-version ZipIterable API owns ZIP64 extra fields, entry parsing, compression
//! and CRC verification. No filename, extra-field payload or decompressed data is parsed here.

use std::borrow::Cow;

use super::{DocumentExtractionError, MAX_ARCHIVE_ENTRIES};

pub(super) struct Admission {
    pub directory_start: u64,
    pub entries: usize,
    footer_offset: usize,
}

impl Admission {
    /// Presents the verified footer as the latest EOCD candidate, without changing entry bytes.
    pub(super) fn reader_view<'input>(
        &self,
        content: &'input [u8],
    ) -> Result<Cow<'input, [u8]>, DocumentExtractionError> {
        let end = self
            .footer_offset
            .checked_add(22)
            .ok_or(DocumentExtractionError::InvalidArchive)?;
        let prefix = content
            .get(..end)
            .ok_or(DocumentExtractionError::InvalidArchive)?;
        if end == content.len() {
            return Ok(Cow::Borrowed(content));
        }
        // The sole copy is bounded by Archive's 20 MiB input admission. ZIP comments contribute
        // no extracted document content. Payloads, directory offsets and ZIP64 records stay exact.
        let mut view = prefix.to_vec();
        view[end - 2..end].fill(0);
        Ok(Cow::Owned(view))
    }
}

pub(super) fn admit(content: &[u8]) -> Result<Admission, DocumentExtractionError> {
    let invalid = DocumentExtractionError::InvalidArchive;
    let last = content.len().checked_sub(22).ok_or(invalid)?;
    let first = content.len().saturating_sub(22 + usize::from(u16::MAX));
    let mut footer = None;
    for offset in (first..=last).rev() {
        if !content[offset..].starts_with(b"PK\x05\x06") {
            continue;
        }
        let end = &content[offset..offset + 22];
        if offset + 22 + usize::from(read16(end, 20)?) == content.len() {
            footer = Some((offset, end));
            break;
        }
    }
    let (footer_offset, end) = footer.ok_or(invalid)?;
    // The pinned reader's backwards finder must encounter this signature first in reader_view.
    if end[4..].windows(4).any(|bytes| bytes == b"PK\x05\x06") {
        return Err(invalid);
    }
    let disk = read16(end, 4)?;
    let directory_disk = read16(end, 6)?;
    let local_count = read16(end, 8)?;
    let count = read16(end, 10)?;
    let size32 = read32(end, 12)?;
    let start32 = read32(end, 16)?;
    if !matches!(disk, 0 | u16::MAX) || !matches!(directory_disk, 0 | u16::MAX) {
        return Err(DocumentExtractionError::UnsupportedArchiveEntry);
    }
    let locator_offset = footer_offset.checked_sub(20);
    let has_zip64 =
        locator_offset.is_some_and(|offset| content[offset..].starts_with(b"PK\x06\x07"));
    let reader_uses_zip64 = count == u16::MAX || size32 == u32::MAX || start32 == u32::MAX;
    let (start, size, declared, boundary) = if has_zip64 {
        // zip 9.2.2 only enters its ZIP64 branch for these three sentinel fields. If it instead
        // uses ZIP32, that branch's single-disk/count checks must also accept this very footer.
        if !reader_uses_zip64 && (disk != 0 || directory_disk != 0 || local_count != count) {
            return Err(invalid);
        }
        let locator_offset = locator_offset.expect("ZIP64 locator has an admitted offset");
        let locator = &content[locator_offset..footer_offset];
        if read32(locator, 4)? != 0 || read32(locator, 16)? != 1 {
            return Err(DocumentExtractionError::UnsupportedArchiveEntry);
        }
        let position = usize::try_from(read64(locator, 8)?).map_err(|_| invalid)?;
        let record = content.get(position..locator_offset).ok_or(invalid)?;
        if !record.starts_with(b"PK\x06\x06") {
            return Err(invalid);
        }
        let record_size = read64(record, 4)?;
        let wide_local_count = read64(record, 24)?;
        let wide_count = read64(record, 32)?;
        let wide_size = read64(record, 40)?;
        let wide_start = read64(record, 48)?;
        if record_size < 44
            || record_size.checked_add(12) != Some(record.len() as u64)
            || read32(record, 16)? != 0
            || read32(record, 20)? != 0
            || wide_local_count != wide_count
        {
            return Err(invalid);
        }
        // Non-sentinel ZIP32 values must agree with the ZIP64 footer used by the reader.
        if (count != u16::MAX && u64::from(count) != wide_count)
            || (local_count != u16::MAX && u64::from(local_count) != wide_count)
            || (size32 != u32::MAX && u64::from(size32) != wide_size)
            || (start32 != u32::MAX && u64::from(start32) != wide_start)
        {
            return Err(invalid);
        }
        (wide_start, wide_size, wide_count, position as u64)
    } else {
        if disk != 0
            || directory_disk != 0
            || count != local_count
            || count == u16::MAX
            || size32 == u32::MAX
            || start32 == u32::MAX
        {
            return Err(invalid);
        }
        (
            u64::from(start32),
            u64::from(size32),
            u64::from(count),
            footer_offset as u64,
        )
    };
    if declared > MAX_ARCHIVE_ENTRIES as u64 {
        return Err(DocumentExtractionError::TooManyEntries);
    }
    // Canonical Office/ODF archives have a contiguous directory directly before their footer.
    if start.checked_add(size) != Some(boundary) {
        return Err(invalid);
    }
    let start_offset = usize::try_from(start).map_err(|_| invalid)?;
    let boundary = usize::try_from(boundary).map_err(|_| invalid)?;
    let directory = content.get(start_offset..boundary).ok_or(invalid)?;
    let mut position = 0;
    let mut actual = 0;
    while position < directory.len() {
        let record = directory.get(position..).ok_or(invalid)?;
        if !record.starts_with(b"PK\x01\x02") {
            return Err(invalid);
        }
        if actual >= MAX_ARCHIVE_ENTRIES {
            return Err(DocumentExtractionError::TooManyEntries);
        }
        let fixed = record.get(..46).ok_or(invalid)?;
        let variable = usize::from(read16(fixed, 28)?)
            + usize::from(read16(fixed, 30)?)
            + usize::from(read16(fixed, 32)?);
        position = position
            .checked_add(46 + variable)
            .filter(|next| *next <= directory.len())
            .ok_or(invalid)?;
        actual += 1;
    }
    if actual as u64 != declared {
        return Err(invalid);
    }
    // Audited against zip 9.2.2 find_central_directory.rs: Known(0) probes the admitted offset
    // directly. The contiguous directory/count checks satisfy its minimum-size check; the ZIP64
    // record length, locator and disk checks satisfy its first candidate. It therefore returns
    // without revisiting earlier signatures. Its one ZIP64 sector allocation is input-bounded.
    Ok(Admission {
        directory_start: start,
        entries: actual,
        footer_offset,
    })
}

fn field<const N: usize>(
    content: &[u8],
    offset: usize,
) -> Result<[u8; N], DocumentExtractionError> {
    let end = offset
        .checked_add(N)
        .ok_or(DocumentExtractionError::InvalidArchive)?;
    content
        .get(offset..end)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(DocumentExtractionError::InvalidArchive)
}

fn read16(content: &[u8], offset: usize) -> Result<u16, DocumentExtractionError> {
    Ok(u16::from_le_bytes(field(content, offset)?))
}

fn read32(content: &[u8], offset: usize) -> Result<u32, DocumentExtractionError> {
    Ok(u32::from_le_bytes(field(content, offset)?))
}

fn read64(content: &[u8], offset: usize) -> Result<u64, DocumentExtractionError> {
    Ok(u64::from_le_bytes(field(content, offset)?))
}
