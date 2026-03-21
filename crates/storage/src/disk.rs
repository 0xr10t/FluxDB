//! The Disk Manager is the layer that sits in between the disk
//! and the rest of the db functionality

use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::FileExt;

pub type PageId = u64;

/// Errors that can occur during disk operations.
#[derive(Debug)]
pub enum DiskError {
    Io(io::Error),
    InvalidPageSize,
}

impl From<io::Error> for DiskError {
    fn from(err: io::Error) -> Self {
        DiskError::Io(err)
    }
}

pub type Result<T> = std::result::Result<T, DiskError>;

/// Disk manager for a single database file with fixed-size pages.
///
/// Notes:
/// - This is page I/O, not B+Tree logic.
/// - Atomic rename writes are better suited for small metadata files; see `atomic_write_file`.
pub struct DiskManager {
    db_path: PathBuf,
    file: File,
    page_size: usize,
}

impl DiskManager {
    // Functions to:
    // - Atomic write for small “whole file” updates (catalog/manifest):
    // - write temp file
    // - fsync temp
    // - rename over destination (atomic on same filesystem)
    // - fsync parent directory

    // - Open or create the database file.
    pub fn new(path: impl AsRef<Path>, page_size: usize) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // make .write implicit behavior explicit
            .truncate(false)
            .open(&path)?;

        Ok(Self {
            db_path: path.as_ref().to_path_buf(),
            file,
            page_size,
        })
    }

    // - Read a page into a new Vec<> or [u8; page_size].
    //
    // Errors
    // 1. your vec.len() != page_size, if you are passing vec
    // 2. your vector must be preallocated to page_size
    pub fn read_page(&self, page_id: PageId, buffer: &mut [u8]) -> Result<()> {
        if buffer.len() != self.page_size {
            return Err(DiskError::InvalidPageSize);
        }

        let offset = page_id * self.page_size as u64;

        #[cfg(unix)]
        self.file.read_exact_at(buffer, offset)?;
        #[cfg(not(unix))]
        {
            // this is not as threadsafe as pread
            use std::io::{Read, Seek, SeekFrom};
            let mut file = &self.file;
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(buffer)?;
        }
        Ok(())
    }

    // - Write a page from bytes (must be page_size). Durable if you call `sync_data()` afterwards.
    pub fn write_page(&self, page_id: PageId, data: &[u8]) -> Result<()> {
        if data.len() != self.page_size {
            return Err(DiskError::InvalidPageSize);
        }
        let offset = page_id * self.page_size as u64;

        #[cfg(unix)]
        self.file.write_all_at(data, offset)?;
        #[cfg(not(unix))]
        {
            // this is not even as threadsafe as pwrite
            use std::io::{Seek, SeekFrom};
            let mut file = &self.file;
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(data)?;
        }
        Ok(())
    }

    /// Flushes file data to disk (fdatasync).
    // - sync_data() - Flush file data (Linux: similar to fdatasync via Rust's sync_data).
    pub fn sync_data(&self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
    }

    pub fn sync_file_and_dir(file: &File, file_path: &Path) {}
}
