//! A shared physical-read ceiling for ZIP entry construction, decoding and destructor cleanup.
//!
//! zip 9.2.2 drains an owned entry's remaining compressed bytes when it is dropped. The reader
//! supplied to `ZipFileEntry::with_reader` must therefore enforce this ceiling itself. Seeking
//! never replenishes it, and an error swallowed by a destructor remains observable to the caller.

use std::cell::Cell;
use std::io::{self, Read, Seek, SeekFrom};

pub(super) struct CompressedReadBudget {
    limit: usize,
    read_bytes: Cell<usize>,
    rejected: Cell<bool>,
}

impl CompressedReadBudget {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            limit,
            read_bytes: Cell::new(0),
            rejected: Cell::new(false),
        }
    }

    pub(super) fn rejected(&self) -> bool {
        self.rejected.get()
    }

    #[cfg(test)]
    pub(super) fn bytes_read(&self) -> usize {
        self.read_bytes.get()
    }
}

pub(super) struct CompressedReader<'reader, R> {
    inner: &'reader mut R,
    budget: &'reader CompressedReadBudget,
}

impl<'reader, R> CompressedReader<'reader, R> {
    pub(super) fn new(inner: &'reader mut R, budget: &'reader CompressedReadBudget) -> Self {
        Self { inner, budget }
    }
}

impl<R: Read> Read for CompressedReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let remaining = self.budget.limit - self.budget.read_bytes.get();
        if self.budget.rejected() || remaining == 0 {
            self.budget.rejected.set(true);
            return Err(io::Error::other("Document compressed read limit exceeded"));
        }
        let count = remaining.min(buffer.len());
        let result = self.inner.read(&mut buffer[..count]);
        match result {
            Ok(0) => {
                // The ZIP layer's Take owns each advertised compressed range. EOF from the
                // physical source before that Take finishes is an inconsistent range claim.
                self.budget.rejected.set(true);
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Incomplete compressed document range",
                ))
            }
            Ok(read) => {
                self.budget
                    .read_bytes
                    .set(self.budget.read_bytes.get() + read);
                Ok(read)
            }
            Err(error) => {
                self.budget.rejected.set(true);
                Err(error)
            }
        }
    }
}

impl<R: Seek> Seek for CompressedReader<'_, R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        // The quota follows bytes actually read across all seeks and all entries.
        self.inner.seek(position)
    }
}
