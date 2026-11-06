//! Private durable storage for immutable, already-prepared attachment bytes.
//!
//! The caller holds the native profile's existing exclusive journal lock and
//! serializes every operation off the UI thread. `open` receives an existing,
//! trusted native profile directory, including trusted ancestors. This module
//! neither acquires another lock nor accepts source paths or rereads sources.
//!
//! The journal assigns random blob labels independently of content. Descriptors
//! bind each label to an exact length and SHA-256 digest of the captured bytes.
//! Reads verify both before returning bytes. Persist a descriptor only after a
//! durable receipt. Keep references and dispatch blocked on directory-sync
//! uncertainty until an explicit identical `put` is durable.
//! Failed or interrupted new writes can leave an uncommitted file. An incomplete
//! file remains for explicit release. An unpublished held queue chunk can instead
//! resume an exact captured prefix through the committed journal authority.
//!
//! Release requires the caller's durable proof that no draft, send, creation,
//! queue handoff, uncertain enqueue or staging operation still references the
//! blob. There is no scan, pruning, startup deletion or failed-load cleanup.
//!
//! Unix private modes, single-link regular files and directory fsync are required.
//! Other platforms fail closed before creating anything. Metadata/handle checks
//! assume cooperating holders of the profile lock; they do not protect against
//! a hostile same-user process replacing trusted ancestors during an operation.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const DIRECTORY: &str = "attachment-spool";
/// Fixed comparison workspace charged to every put by the serialized worker.
pub(crate) const PUT_COMPARE_BYTES: usize = 16 * 1024;

/// A journal-assigned random label of exactly 32 lowercase hexadecimal ASCII bytes.
/// This label grants access only to its fixed spool filename and is not a hash.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct BlobId([u8; 32]);

impl BlobId {
    pub(crate) fn parse(value: &str) -> Result<Self, SpoolError> {
        let bytes: [u8; 32] = value
            .as_bytes()
            .try_into()
            .map_err(|_| SpoolError::InvalidId)?;
        if !bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        {
            return Err(SpoolError::InvalidId);
        }
        Ok(Self(bytes))
    }

    /// Returns the label for journal serialization, without a filesystem path.
    pub(crate) fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("validated ASCII blob identity")
    }
}

impl fmt::Debug for BlobId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BlobId([REDACTED])")
    }
}

/// Immutable journal metadata. Reconstruct only from the caller's validated journal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlobDescriptor {
    id: BlobId,
    byte_len: u64,
    sha256: [u8; 32],
}

impl BlobDescriptor {
    /// Restores all required journal fields, including an exactly 32-byte digest.
    /// The ID is already validated; read validates the length ceiling and content.
    pub(crate) fn new(id: BlobId, byte_len: u64, sha256: [u8; 32]) -> Self {
        Self {
            id,
            byte_len,
            sha256,
        }
    }

    pub(crate) fn id(&self) -> &BlobId {
        &self.id
    }

    pub(crate) fn byte_len(&self) -> u64 {
        self.byte_len
    }

    /// Returns the digest for journal serialization without mutable access.
    pub(crate) fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }
}

impl fmt::Debug for BlobDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BlobDescriptor")
            .field("id", &self.id)
            .field("byte_len", &self.byte_len)
            .finish_non_exhaustive()
    }
}

/// Complete bytes with content-free diagnostics and no mutable access.
pub(crate) struct BlobBytes(Vec<u8>);

impl BlobBytes {
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for BlobBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BlobBytes")
            .field("byte_len", &self.0.len())
            .finish_non_exhaustive()
    }
}

/// File sync or unlink has completed in both cases. An uncertain directory sync
/// must not be reported as a precommit failure or a confirmed durable change.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpoolCommit {
    Durable,
    DirectorySyncUncertain(io::ErrorKind),
}

/// A matching existing blob receives the same descriptor without overwriting it.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlobReceipt {
    pub(crate) descriptor: BlobDescriptor,
    pub(crate) commit: SpoolCommit,
}

/// Contains no paths, filenames, digests, source bytes or underlying diagnostic payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpoolError {
    InvalidId,
    UnsupportedPlatform,
    UnsafeStorage,
    TooLarge,
    LengthMismatch,
    ContentMismatch,
    Collision,
    Io(io::ErrorKind),
}

impl fmt::Display for SpoolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId => formatter.write_str("invalid attachment blob identity"),
            Self::UnsupportedPlatform => {
                formatter.write_str("private attachment storage is unsupported on this platform")
            }
            Self::UnsafeStorage => formatter.write_str("attachment spool storage is unsafe"),
            Self::TooLarge => {
                formatter.write_str("attachment blob exceeds the supplied byte limit")
            }
            Self::LengthMismatch => {
                formatter.write_str("attachment blob length differs from its descriptor")
            }
            Self::ContentMismatch => {
                formatter.write_str("attachment blob content differs from its descriptor")
            }
            Self::Collision => {
                formatter.write_str("attachment blob identity already has different bytes")
            }
            Self::Io(kind) => write!(formatter, "attachment spool I/O failure: {kind}"),
        }
    }
}

impl std::error::Error for SpoolError {}

impl From<io::Error> for SpoolError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

/// Filesystem owner under one trusted native profile, using its existing lock.
pub(crate) struct AttachmentSpool {
    path: PathBuf,
    profile: File,
    directory: File,
}

impl fmt::Debug for AttachmentSpool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttachmentSpool")
            .finish_non_exhaustive()
    }
}

impl AttachmentSpool {
    /// Creates only the fixed spool child. Existing unsafe storage stays untouched.
    /// Opening never lists, reads or deletes any blob. Failed opens grant no owner.
    pub(crate) fn open(profile_path: &Path) -> Result<Self, SpoolError> {
        if !cfg!(unix) {
            return Err(SpoolError::UnsupportedPlatform);
        }
        if !profile_path.is_absolute() {
            return Err(SpoolError::UnsafeStorage);
        }
        private_directory_metadata(profile_path)?;
        let profile = File::open(profile_path)?;
        let path = profile_path.join(DIRECTORY);
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        private_directory_metadata(&path)?;
        let directory = File::open(&path)?;
        let spool = Self {
            path,
            profile,
            directory,
        };
        spool.check_location()?;
        // Also settle an earlier open whose directory creation was not confirmed.
        spool.profile.sync_all()?;
        spool.directory.sync_all()?;
        Ok(spool)
    }

    /// Stores the complete captured slice with create_new and no overwrite.
    /// The caller supplies the established limit for this prepared representation.
    /// Basic 10 MiB input and 20 MiB normalized embedded images have separate callers.
    /// Hashes the captured slice once for its descriptor. No source is read here,
    /// including on an identical explicit repeat. Labels never deduplicate by hash.
    /// Existing-file comparisons use the fixed PUT_COMPARE_BYTES workspace.
    ///
    /// Every Err precedes a confirmed file sync and leaves existing blobs intact.
    /// A failed new write may leave an uncommitted file, possibly incomplete.
    /// A directory-sync failure after file sync returns an uncertain receipt.
    pub(crate) fn put(
        &mut self,
        id: BlobId,
        bytes: &[u8],
        max_bytes: usize,
    ) -> Result<BlobReceipt, SpoolError> {
        if bytes.len() > max_bytes {
            return Err(SpoolError::TooLarge);
        }
        self.check_location()?;
        let sha256: [u8; 32] = Sha256::digest(bytes).into();
        let descriptor = BlobDescriptor::new(id, bytes.len() as u64, sha256);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = match options.open(self.blob_path(&id)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let mut file = self.open_blob(&id)?;
                if file.metadata()?.len() != descriptor.byte_len {
                    return Err(SpoolError::Collision);
                }
                compare_captured(&mut file, bytes)?;
                if file.metadata()?.len() != descriptor.byte_len {
                    return Err(SpoolError::Collision);
                }
                file.sync_all()?;
                return Ok(BlobReceipt {
                    descriptor,
                    commit: self.sync_directory(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        self.check_blob_file(&id, &file)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        Ok(BlobReceipt {
            descriptor,
            commit: self.sync_directory(),
        })
    }

    /// Resumes only an unpublished, durably held chunk supplied by the journal owner.
    /// A short file must be an exact prefix of the retained capture. Append preserves
    /// its identity; complete, different, oversized and unsafe artifacts stay untouched.
    pub(crate) fn resume_unpublished(
        &mut self,
        expected: &BlobDescriptor,
        bytes: &[u8],
        max_bytes: usize,
    ) -> Result<BlobReceipt, SpoolError> {
        read_limit(expected, max_bytes)?;
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        if bytes.len() as u64 != expected.byte_len || digest != expected.sha256 {
            return Err(SpoolError::ContentMismatch);
        }
        match self.put(expected.id, bytes, max_bytes) {
            Err(SpoolError::Collision) => {}
            result => return result,
        }
        let mut prefix = self.open_blob(&expected.id)?;
        let before = prefix.metadata()?;
        let length = before.len();
        if length >= expected.byte_len {
            return Err(SpoolError::Collision);
        }
        compare_captured(&mut prefix, &bytes[..length as usize])?;
        let mut file = OpenOptions::new()
            .append(true)
            .open(self.blob_path(&expected.id))?;
        let opened = self.check_blob_file(&expected.id, &file)?;
        if !same_inode(&before, &opened) || opened.len() != length {
            return Err(SpoolError::UnsafeStorage);
        }
        file.write_all(&bytes[length as usize..])?;
        file.flush()?;
        file.sync_all()?;
        if self.check_blob_file(&expected.id, &file)?.len() != expected.byte_len {
            return Err(SpoolError::LengthMismatch);
        }
        Ok(BlobReceipt {
            descriptor: *expected,
            commit: self.sync_directory(),
        })
    }

    /// Reads exactly the journaled byte length within the supplied ceiling, then
    /// verifies its SHA-256 digest before returning any bytes. Missing, short,
    /// growing, over-limit and same-length corrupt blobs fail without empty success.
    /// The recorded length plus one oversize sentinel is allocated before reading.
    pub(crate) fn read(
        &self,
        descriptor: &BlobDescriptor,
        max_bytes: usize,
    ) -> Result<BlobBytes, SpoolError> {
        read_limit(descriptor, max_bytes)?;
        let mut file = self.open_blob(&descriptor.id)?;
        let bytes = read_complete(&mut file, descriptor, max_bytes)?;
        let sha256: [u8; 32] = Sha256::digest(bytes.as_slice()).into();
        if sha256 != descriptor.sha256 {
            return Err(SpoolError::ContentMismatch);
        }
        Ok(bytes)
    }

    /// Unlinks only this blob after the caller proves every reference is gone and
    /// durably saves that change under the same profile lock. This includes drafts,
    /// sends, creations, queue handoffs, uncertain enqueues and pending staging.
    /// Failed/partial journal loads provide no such proof. No server file is touched.
    /// Repeating an authorized release confirms a missing name with directory fsync.
    pub(crate) fn release_after_durable_unreference(
        &mut self,
        id: &BlobId,
    ) -> Result<SpoolCommit, SpoolError> {
        self.check_location()?;
        let path = self.blob_path(id);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                let _file = self.open_blob(id)?;
                fs::remove_file(path)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // A missing target grants no authority to ignore a failed location check.
                self.check_location()?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(self.sync_directory())
    }

    fn blob_path(&self, id: &BlobId) -> PathBuf {
        self.path.join(format!("blob-{}.bin", id.as_str()))
    }

    fn check_location(&self) -> Result<(), SpoolError> {
        let profile_path = self.path.parent().ok_or(SpoolError::UnsafeStorage)?;
        let profile = private_directory_metadata(profile_path)?;
        let directory = private_directory_metadata(&self.path)?;
        if !same_inode(&profile, &self.profile.metadata()?)
            || !same_inode(&directory, &self.directory.metadata()?)
        {
            return Err(SpoolError::UnsafeStorage);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if directory.uid() != profile.uid() {
                return Err(SpoolError::UnsafeStorage);
            }
        }
        Ok(())
    }

    fn open_blob(&self, id: &BlobId) -> Result<File, SpoolError> {
        self.check_location()?;
        let path = self.blob_path(id);
        let before = fs::symlink_metadata(&path)?;
        check_private_file(&before, &self.directory.metadata()?)?;
        let file = File::open(path)?;
        let after = self.check_blob_file(id, &file)?;
        if !same_inode(&before, &after) {
            return Err(SpoolError::UnsafeStorage);
        }
        Ok(file)
    }

    fn check_blob_file(&self, id: &BlobId, file: &File) -> Result<fs::Metadata, SpoolError> {
        self.check_location()?;
        let named = fs::symlink_metadata(self.blob_path(id))?;
        let opened = file.metadata()?;
        let directory = self.directory.metadata()?;
        check_private_file(&named, &directory)?;
        check_private_file(&opened, &directory)?;
        if !same_inode(&named, &opened) {
            return Err(SpoolError::UnsafeStorage);
        }
        Ok(opened)
    }

    fn sync_directory(&self) -> SpoolCommit {
        directory_commit(self.directory.sync_all())
    }
}

fn directory_commit(result: io::Result<()>) -> SpoolCommit {
    match result {
        Ok(()) => SpoolCommit::Durable,
        Err(error) => SpoolCommit::DirectorySyncUncertain(error.kind()),
    }
}

fn read_limit(descriptor: &BlobDescriptor, max_bytes: usize) -> Result<u64, SpoolError> {
    if descriptor.byte_len > max_bytes as u64 {
        return Err(SpoolError::TooLarge);
    }
    descriptor
        .byte_len
        .checked_add(1)
        .ok_or(SpoolError::TooLarge)
}

fn read_complete(
    file: &mut File,
    descriptor: &BlobDescriptor,
    max_bytes: usize,
) -> Result<BlobBytes, SpoolError> {
    read_limit(descriptor, max_bytes)?;
    if file.metadata()?.len() != descriptor.byte_len {
        return Err(SpoolError::LengthMismatch);
    }
    let bytes = read_captured(file, descriptor.byte_len)?;
    if file.metadata()?.len() != descriptor.byte_len {
        return Err(SpoolError::LengthMismatch);
    }
    Ok(bytes)
}

fn compare_captured(reader: &mut impl Read, captured: &[u8]) -> Result<(), SpoolError> {
    let mut scratch = [0; PUT_COMPARE_BYTES];
    for expected in captured.chunks(PUT_COMPARE_BYTES) {
        let actual = &mut scratch[..expected.len()];
        match reader.read_exact(actual) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(SpoolError::Collision);
            }
            Err(error) => return Err(error.into()),
        }
        if scratch[..expected.len()] != expected[..] {
            return Err(SpoolError::Collision);
        }
    }
    // Reuse the comparison workspace for EOF, including an empty captured blob.
    loop {
        match reader.read(&mut scratch[..1]) {
            Ok(0) => return Ok(()),
            Ok(_) => return Err(SpoolError::Collision),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
}

fn read_captured(reader: &mut impl Read, byte_len: u64) -> Result<BlobBytes, SpoolError> {
    // Reserve the oversize sentinel before reading. Filling this fixed slice
    // cannot grow the allocation when a file changes after its metadata check.
    let capacity: usize = byte_len
        .checked_add(1)
        .ok_or(SpoolError::TooLarge)?
        .try_into()
        .map_err(|_| SpoolError::TooLarge)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| SpoolError::Io(io::ErrorKind::OutOfMemory))?;
    bytes.resize(capacity, 0);
    let mut read = 0;
    while read < capacity {
        match reader.read(&mut bytes[read..]) {
            Ok(0) => break,
            Ok(count) => read += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    if read as u64 != byte_len {
        return Err(SpoolError::LengthMismatch);
    }
    bytes.truncate(read);
    Ok(BlobBytes(bytes))
}

// These checks repeat the private-storage pattern in mutation_journal.rs because
// its helpers are private and tightening that older owner's contract is separate
// work. Existing permissions are rejected here without repairing another store.
fn private_directory_metadata(path: &Path) -> Result<fs::Metadata, SpoolError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(SpoolError::UnsafeStorage);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o7777 != 0o700 {
            return Err(SpoolError::UnsafeStorage);
        }
    }
    Ok(metadata)
}

fn check_private_file(metadata: &fs::Metadata, directory: &fs::Metadata) -> Result<(), SpoolError> {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(SpoolError::UnsafeStorage);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if !matches!(metadata.mode() & 0o7777, 0o400 | 0o600)
            || metadata.nlink() != 1
            || metadata.uid() != directory.uid()
        {
            return Err(SpoolError::UnsafeStorage);
        }
    }
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

fn same_inode(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        left.dev() == right.dev() && left.ino() == right.ino()
    }
    #[cfg(not(unix))]
    {
        let _ = (left, right);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: u128) -> BlobId {
        BlobId::parse(&format!("{value:032x}")).expect("fixed ASCII test identity")
    }

    #[test]
    fn rejects_malformed_ids_and_path_authority() {
        for invalid in [
            "",
            "..",
            "../00000000000000000000000000000",
            "/0000000000000000000000000000000",
            "0000000000000000000000000000000/",
            "0000000000000000000000000000000\\",
            "0000000000000000000000000000000\0",
            "0000000000000000000000000000000A",
            "0000000000000000000000000000000g",
            "0000000000000000000000000000000é",
            "0000000000000000000000000000000",
            "000000000000000000000000000000000",
            "................................",
            "C:000000000000000000000000000000",
        ] {
            assert_eq!(
                BlobId::parse(invalid),
                Err(SpoolError::InvalidId),
                "reject path-like or malformed identity"
            );
        }
    }

    #[test]
    fn directory_sync_uncertainty_is_a_committed_outcome_without_diagnostics() {
        assert_eq!(directory_commit(Ok(())), SpoolCommit::Durable);
        let commit = directory_commit(Err(io::Error::other("private-path-and-content")));
        assert_eq!(
            commit,
            SpoolCommit::DirectorySyncUncertain(io::ErrorKind::Other)
        );
        assert!(!format!("{commit:?}").contains("private-path-and-content"));
    }

    #[test]
    fn comparison_classifies_short_growing_and_different_files_as_collisions() {
        for existing in [
            b"save".as_slice(),
            b"saved!".as_slice(),
            b"other".as_slice(),
        ] {
            let mut reader = existing;
            assert_eq!(
                compare_captured(&mut reader, b"saved"),
                Err(SpoolError::Collision)
            );
        }
        let mut empty = b"".as_slice();
        assert_eq!(compare_captured(&mut empty, b""), Ok(()));
        let mut growing = b"!".as_slice();
        assert_eq!(
            compare_captured(&mut growing, b""),
            Err(SpoolError::Collision)
        );
    }

    #[test]
    fn complete_reads_allocate_the_oversize_sentinel_before_reading() {
        for length in [0, 1, PUT_COMPARE_BYTES] {
            let source = vec![0x5a; length];
            let mut reader = source.as_slice();
            let bytes = read_captured(&mut reader, length as u64).expect("exact captured length");
            assert_eq!(bytes.as_slice(), source);
            assert_eq!(
                bytes.0.capacity(),
                length + 1,
                "the sentinel remains allocated and must stay charged after truncation"
            );
        }
    }

    #[test]
    fn growth_after_the_metadata_check_consumes_only_one_reserved_oversize_byte() {
        use openchamber_native_transport::attachment::MAX_PREPARED_ATTACHMENT_BYTES;

        struct GrowingReader {
            read_bytes: usize,
            calls: usize,
        }
        impl Read for GrowingReader {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                self.calls += 1;
                buffer.fill(0x5a);
                self.read_bytes += buffer.len();
                Ok(buffer.len())
            }
        }
        // This is the content boundary immediately after an exact metadata stat.
        // The reader keeps growing and never returns EOF.
        let mut reader = GrowingReader {
            read_bytes: 0,
            calls: 0,
        };
        assert!(matches!(
            read_captured(&mut reader, MAX_PREPARED_ATTACHMENT_BYTES as u64),
            Err(SpoolError::LengthMismatch)
        ));
        assert_eq!(reader.read_bytes, MAX_PREPARED_ATTACHMENT_BYTES + 1);
        assert_eq!(
            reader.calls, 1,
            "growth cannot trigger another allocation or read"
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn unsupported_platform_fails_before_filesystem_access() {
        assert!(matches!(
            AttachmentSpool::open(Path::new("unused")),
            Err(SpoolError::UnsupportedPlatform)
        ));
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQUENCE: AtomicU64 = AtomicU64::new(0);

        struct Scratch(PathBuf);

        impl Scratch {
            fn new() -> Self {
                for _ in 0..32 {
                    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
                    let path = PathBuf::from(format!(
                        "/tmp/opencode/attachment-spool-{}-{sequence}",
                        std::process::id()
                    ));
                    match fs::DirBuilder::new().mode(0o700).create(&path) {
                        Ok(()) => return Self(path),
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                        Err(_) => panic!("create private test profile"),
                    }
                }
                panic!("test profile names collided")
            }

            fn open(&self) -> AttachmentSpool {
                AttachmentSpool::open(&self.0).expect("open private test spool")
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn exact_bytes_reopen_and_identical_repeat_keep_the_same_file() {
            let scratch = Scratch::new();
            let mut spool = scratch.open();
            let mut bytes: Vec<u8> = (0..=255).collect();
            bytes.extend_from_slice("\0é👩🏽‍💻\r\n".as_bytes());
            let captured = bytes.clone();
            let receipt = spool
                .put(id(1), &bytes, bytes.len())
                .expect("persist complete bytes");
            assert_eq!(
                receipt.commit,
                SpoolCommit::Durable,
                "receipt requires file and directory sync"
            );
            let inode = fs::metadata(spool.blob_path(&id(1)))
                .expect("blob metadata")
                .ino();
            bytes.fill(0);
            drop(spool);
            let mut spool = scratch.open();
            let descriptor = BlobDescriptor::new(
                BlobId::parse(receipt.descriptor.id().as_str()).expect("journal identity"),
                receipt.descriptor.byte_len(),
                *receipt.descriptor.sha256(),
            );
            assert_eq!(
                spool
                    .read(&descriptor, captured.len())
                    .expect("reopen bytes")
                    .as_slice(),
                captured
            );
            assert_eq!(
                spool
                    .put(id(1), &captured, captured.len())
                    .expect("repeat exact bytes"),
                receipt
            );
            assert_eq!(
                fs::metadata(spool.blob_path(&id(1)))
                    .expect("same blob metadata")
                    .ino(),
                inode,
                "identical repeat never replaces the file"
            );
        }

        #[test]
        fn unpublished_resume_preserves_nonmatching_complete_and_unsafe_artifacts() {
            use std::os::unix::fs::symlink;

            let scratch = Scratch::new();
            let mut spool = scratch.open();
            let captured = b"retained full capture";
            let digest: [u8; 32] = Sha256::digest(captured).into();
            for (label, existing) in [
                (id(301), b"other prefix".to_vec()),
                (id(302), vec![b'x'; captured.len()]),
            ] {
                let _ = spool
                    .put(label, &existing, captured.len())
                    .expect("independent pre-existing artifact");
                let inode = fs::metadata(spool.blob_path(&label))
                    .expect("original inode")
                    .ino();
                let expected = BlobDescriptor::new(label, captured.len() as u64, digest);
                assert_eq!(
                    spool.resume_unpublished(&expected, captured, captured.len()),
                    Err(SpoolError::Collision)
                );
                assert_eq!(
                    fs::read(spool.blob_path(&label)).expect("artifact remains"),
                    existing
                );
                assert_eq!(
                    fs::metadata(spool.blob_path(&label))
                        .expect("unchanged inode")
                        .ino(),
                    inode
                );
            }
            let protected = spool
                .put(id(303), b"protected target", captured.len())
                .expect("protected independent target")
                .descriptor;
            symlink(spool.blob_path(&id(303)), spool.blob_path(&id(304)))
                .expect("unsafe held-name fixture");
            let expected = BlobDescriptor::new(id(304), captured.len() as u64, digest);
            assert_eq!(
                spool.resume_unpublished(&expected, captured, captured.len()),
                Err(SpoolError::UnsafeStorage)
            );
            assert_eq!(
                spool
                    .read(&protected, captured.len())
                    .expect("protected bytes")
                    .as_slice(),
                b"protected target"
            );
            let complete = spool
                .put(id(305), captured, captured.len())
                .expect("complete unpublished artifact");
            let inode = fs::metadata(spool.blob_path(&id(305)))
                .expect("complete inode")
                .ino();
            assert_eq!(
                spool
                    .resume_unpublished(&complete.descriptor, captured, captured.len())
                    .expect("identical completion"),
                complete
            );
            assert_eq!(
                fs::metadata(spool.blob_path(&id(305)))
                    .expect("same complete inode")
                    .ino(),
                inode
            );
        }

        #[test]
        fn maximum_prepared_repeat_compares_the_real_file_with_fixed_workspace() {
            use openchamber_native_transport::attachment::MAX_PREPARED_ATTACHMENT_BYTES;
            use std::io::{Seek, SeekFrom};

            struct ReadSizes<'a> {
                file: &'a mut File,
                largest_request: usize,
                read_bytes: usize,
                eof_checks: usize,
            }
            impl Read for ReadSizes<'_> {
                fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                    self.largest_request = self.largest_request.max(buffer.len());
                    let read = self.file.read(buffer)?;
                    self.read_bytes += read;
                    if read == 0 {
                        self.eof_checks += 1;
                    }
                    Ok(read)
                }
            }
            let scratch = Scratch::new();
            let mut spool = scratch.open();
            // One full-size captured allocation is retained across both puts.
            let mut bytes = vec![0x5a; MAX_PREPARED_ATTACHMENT_BYTES];
            let first = spool
                .put(id(15), &bytes, bytes.len())
                .expect("maximum durable put");
            assert_eq!(first.commit, SpoolCommit::Durable);
            let mut file = spool.open_blob(&id(15)).expect("open exact committed blob");
            let inode = file.metadata().expect("original inode").ino();
            {
                let mut reader = ReadSizes {
                    file: &mut file,
                    largest_request: 0,
                    read_bytes: 0,
                    eof_checks: 0,
                };
                assert_eq!(compare_captured(&mut reader, &bytes), Ok(()));
                assert_eq!(reader.largest_request, PUT_COMPARE_BYTES);
                assert_eq!(reader.read_bytes, MAX_PREPARED_ATTACHMENT_BYTES);
                assert_eq!(reader.eof_checks, 1);
            }
            assert_eq!(
                spool
                    .put(id(15), &bytes, bytes.len())
                    .expect("durable identical repeat"),
                first
            );
            *bytes.last_mut().expect("nonempty maximum payload") ^= 1;
            assert_eq!(
                spool.put(id(15), &bytes, bytes.len()),
                Err(SpoolError::Collision)
            );
            file.seek(SeekFrom::End(-1))
                .expect("inspect final original byte");
            let mut last = [0];
            file.read_exact(&mut last)
                .expect("read unchanged last byte");
            assert_eq!(
                last,
                [0x5a],
                "a final-chunk mismatch never overwrites the file"
            );
            assert_eq!(
                fs::metadata(spool.blob_path(&id(15)))
                    .expect("same blob")
                    .ino(),
                inode
            );
        }

        #[test]
        fn collisions_never_overwrite_existing_bytes() {
            let scratch = Scratch::new();
            let mut spool = scratch.open();
            let receipt = spool.put(id(2), b"saved", 8).expect("persist original");
            for different in [b"other".as_slice(), b"longer".as_slice(), b"".as_slice()] {
                assert_eq!(spool.put(id(2), different, 8), Err(SpoolError::Collision));
                assert_eq!(
                    spool
                        .read(&receipt.descriptor, 8)
                        .expect("preserved original")
                        .as_slice(),
                    b"saved"
                );
            }
        }

        #[test]
        fn rejects_over_limit_missing_short_and_long_blobs_without_truncating() {
            let scratch = Scratch::new();
            let mut spool = scratch.open();
            assert_eq!(spool.put(id(3), b"12345", 4), Err(SpoolError::TooLarge));
            assert!(
                !spool.blob_path(&id(3)).exists(),
                "over-limit input creates nothing"
            );
            let descriptor = BlobDescriptor::new(id(3), 4, Sha256::digest(b"1234").into());
            assert!(
                matches!(
                    spool.read(&descriptor, 4),
                    Err(SpoolError::Io(io::ErrorKind::NotFound))
                ),
                "missing blob is an error"
            );
            let receipt = spool.put(id(3), b"1234", 4).expect("exact limit");
            assert!(matches!(
                spool.read(&receipt.descriptor, 3),
                Err(SpoolError::TooLarge)
            ));
            for damaged in [b"12".as_slice(), b"12345".as_slice()] {
                fs::write(spool.blob_path(&id(3)), damaged)
                    .expect("simulate interrupted or changed storage");
                assert!(matches!(
                    spool.read(&descriptor, 4),
                    Err(SpoolError::LengthMismatch)
                ));
                assert_eq!(spool.put(id(3), b"1234", 4), Err(SpoolError::Collision));
                assert_eq!(
                    fs::read(spool.blob_path(&id(3))).expect("damaged bytes retained"),
                    damaged
                );
            }
            let empty = spool
                .put(id(4), b"", 0)
                .expect("complete empty representation");
            assert!(
                spool
                    .read(&empty.descriptor, 0)
                    .expect("complete empty bytes")
                    .as_slice()
                    .is_empty()
            );
            assert!(matches!(
                spool.read(
                    &BlobDescriptor::new(id(4), u64::MAX, *empty.descriptor.sha256()),
                    usize::MAX
                ),
                Err(SpoolError::TooLarge)
            ));
        }

        #[test]
        fn release_removes_one_authorized_blob_and_preserves_other_files() {
            let scratch = Scratch::new();
            let mut spool = scratch.open();
            let first = spool.put(id(5), b"first", 8).expect("first blob");
            let other = spool.put(id(6), b"other", 8).expect("other blob");
            let unrelated = spool.path.join("unrecognized-entry");
            fs::write(&unrelated, b"preserve").expect("unrelated file");
            assert_eq!(
                spool
                    .release_after_durable_unreference(first.descriptor.id())
                    .expect("caller has durably removed all references"),
                SpoolCommit::Durable
            );
            assert_eq!(
                spool
                    .release_after_durable_unreference(first.descriptor.id())
                    .expect("repeat authorized release"),
                SpoolCommit::Durable
            );
            drop(spool);
            let spool = scratch.open();
            assert!(matches!(
                spool.read(&first.descriptor, 8),
                Err(SpoolError::Io(io::ErrorKind::NotFound))
            ));
            assert_eq!(
                spool
                    .read(&other.descriptor, 8)
                    .expect("other blob survives")
                    .as_slice(),
                b"other"
            );
            assert_eq!(
                fs::read(unrelated).expect("opening never scans or prunes"),
                b"preserve"
            );
        }

        #[test]
        fn rejects_symlink_files_and_directories_without_touching_targets() {
            use std::os::unix::fs::symlink;
            let scratch = Scratch::new();
            let mut spool = scratch.open();
            let target = spool.put(id(7), b"target", 8).expect("target bytes");
            symlink(spool.blob_path(&id(7)), spool.blob_path(&id(8))).expect("file symlink");
            let descriptor = BlobDescriptor::new(id(8), 6, *target.descriptor.sha256());
            assert!(matches!(
                spool.read(&descriptor, 8),
                Err(SpoolError::UnsafeStorage)
            ));
            assert_eq!(
                spool.put(id(8), b"target", 8),
                Err(SpoolError::UnsafeStorage)
            );
            assert_eq!(
                spool.release_after_durable_unreference(&id(8)),
                Err(SpoolError::UnsafeStorage)
            );
            assert_eq!(
                spool
                    .read(&target.descriptor, 8)
                    .expect("target untouched")
                    .as_slice(),
                b"target"
            );
            let alias = scratch.0.join("profile-alias");
            symlink(&scratch.0, &alias).expect("profile symlink");
            assert!(matches!(
                AttachmentSpool::open(&alias),
                Err(SpoolError::UnsafeStorage)
            ));
            let linked_profile = Scratch::new();
            symlink(&spool.path, linked_profile.0.join(DIRECTORY))
                .expect("spool directory symlink");
            assert!(matches!(
                AttachmentSpool::open(&linked_profile.0),
                Err(SpoolError::UnsafeStorage)
            ));
        }

        #[test]
        fn rejects_hardlinks_public_permissions_and_nonregular_files() {
            let scratch = Scratch::new();
            let mut spool = scratch.open();
            let receipt = spool.put(id(9), b"private", 8).expect("private blob");
            let path = spool.blob_path(&id(9));
            let link = scratch.0.join("second-link");
            fs::hard_link(&path, &link).expect("second hardlink");
            assert!(matches!(
                spool.read(&receipt.descriptor, 8),
                Err(SpoolError::UnsafeStorage)
            ));
            assert_eq!(
                spool.put(id(9), b"private", 8),
                Err(SpoolError::UnsafeStorage)
            );
            assert_eq!(
                spool.release_after_durable_unreference(&id(9)),
                Err(SpoolError::UnsafeStorage)
            );
            fs::remove_file(&link).expect("remove test hardlink");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
                .expect("unsafe file mode");
            assert!(matches!(
                spool.read(&receipt.descriptor, 8),
                Err(SpoolError::UnsafeStorage)
            ));
            assert_eq!(
                spool.put(id(9), b"private", 8),
                Err(SpoolError::UnsafeStorage)
            );
            assert_eq!(
                spool.release_after_durable_unreference(&id(9)),
                Err(SpoolError::UnsafeStorage)
            );
            assert_eq!(
                fs::metadata(&path).expect("unchanged permissions").mode() & 0o7777,
                0o640
            );
            fs::create_dir(spool.blob_path(&id(10))).expect("directory at blob name");
            assert!(matches!(
                spool.read(
                    &BlobDescriptor::new(id(10), 0, Sha256::digest(b"").into()),
                    8
                ),
                Err(SpoolError::UnsafeStorage)
            ));
            fs::set_permissions(&spool.path, fs::Permissions::from_mode(0o750))
                .expect("unsafe spool mode");
            assert!(matches!(
                AttachmentSpool::open(&scratch.0),
                Err(SpoolError::UnsafeStorage)
            ));
            assert_eq!(spool.put(id(11), b"new", 8), Err(SpoolError::UnsafeStorage));
            assert_eq!(
                fs::metadata(&spool.path)
                    .expect("spool mode retained")
                    .mode()
                    & 0o7777,
                0o750
            );
            fs::set_permissions(&spool.path, fs::Permissions::from_mode(0o700))
                .expect("restore test mode");
            fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o750))
                .expect("unsafe profile mode");
            assert!(matches!(
                AttachmentSpool::open(&scratch.0),
                Err(SpoolError::UnsafeStorage)
            ));
            fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o700))
                .expect("restore test profile");
        }

        #[test]
        fn same_length_file_corruption_is_rejected_without_overwrite() {
            let scratch = Scratch::new();
            let mut spool = scratch.open();
            let receipt = spool
                .put(id(13), b"saved", 8)
                .expect("persist original bytes");
            let path = spool.blob_path(&id(13));
            fs::write(&path, b"other").expect("same-length storage corruption");
            assert_eq!(
                fs::metadata(&path).expect("corrupt blob length").len(),
                receipt.descriptor.byte_len(),
                "the length check alone cannot detect this corruption"
            );
            assert!(matches!(
                spool.read(&receipt.descriptor, 8),
                Err(SpoolError::ContentMismatch)
            ));
            assert_eq!(
                spool.put(id(13), b"saved", 8),
                Err(SpoolError::Collision),
                "put retains exact-byte collision classification"
            );
            assert_eq!(
                fs::read(&path).expect("collision never overwrites"),
                b"other"
            );
        }

        #[test]
        fn restored_descriptor_with_wrong_digest_rejects_unchanged_bytes() {
            let scratch = Scratch::new();
            let mut spool = scratch.open();
            let receipt = spool
                .put(id(14), b"saved", 8)
                .expect("persist original bytes");
            let mut wrong_digest = *receipt.descriptor.sha256();
            wrong_digest[0] ^= 1;
            let wrong = BlobDescriptor::new(
                *receipt.descriptor.id(),
                receipt.descriptor.byte_len(),
                wrong_digest,
            );
            drop(spool);
            let spool = scratch.open();
            assert!(matches!(
                spool.read(&wrong, 8),
                Err(SpoolError::ContentMismatch)
            ));
            assert_eq!(
                spool
                    .read(&receipt.descriptor, 8)
                    .expect("unchanged bytes still match the original descriptor")
                    .as_slice(),
                b"saved"
            );
        }

        #[test]
        fn diagnostics_redact_identity_paths_digests_and_content() {
            let scratch = Scratch::new();
            let mut spool = scratch.open();
            let receipt = spool
                .put(id(12), b"private-attachment-content", 64)
                .expect("persist private bytes");
            let bytes = spool
                .read(&receipt.descriptor, 64)
                .expect("read private bytes");
            let error = SpoolError::from(io::Error::other("private-io-diagnostic"));
            let mismatch = SpoolError::ContentMismatch;
            let diagnostics = format!(
                "{spool:?} {receipt:?} {bytes:?} {error:?} {error} {mismatch:?} {mismatch}"
            );
            let identity = id(12);
            let digest_debug = format!("{:?}", receipt.descriptor.sha256());
            let digest_hex: String = receipt
                .descriptor
                .sha256()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            for private in [
                identity.as_str(),
                scratch.0.to_str().expect("test path"),
                "private-attachment-content",
                "private-io-diagnostic",
                &digest_debug,
                &digest_hex,
            ] {
                assert!(
                    !diagnostics.contains(private),
                    "diagnostics must omit private values"
                );
            }
        }
    }
}
