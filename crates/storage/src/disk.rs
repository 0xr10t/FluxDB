//! The Disk Manager provides a layer of abstraction between the disk
//! and the rest of the database functionality, managing page-level I/O.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::FileExt;

/// Represents a unique identifier for a page in the database.
pub type PageId = u64;

/// Errors that can occur during disk operations.
#[derive(Debug)]
pub enum DiskError {
    /// An underlying I/O error.
    Io(io::Error),
    /// The provided buffer or data size does not match the configured page size.
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
/// This layer handles low-level page I/O, ensuring that data is correctly
/// read from and written to the underlying storage medium.
pub struct DiskManager {
    db_path: PathBuf,
    file: File,
    page_size: usize,
}

impl DiskManager {
    /// Opens or creates the database file at the specified path.
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

    /// Reads a page from the database file into the provided buffer.
    ///
    /// The buffer must be pre-allocated and its length must exactly match
    /// the `page_size` configured for this `DiskManager`.
    ///
    /// # Errors
    ///
    /// * Returns [`DiskError::InvalidPageSize`] if the buffer length does not
    ///   match the configured page size.
    /// * Returns [`DiskError::Io`] if an I/O error occurs during reading.
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

    /// Writes a page from the provided data buffer to the database file.
    ///
    /// The data buffer's length must exactly match the `page_size` configured
    /// for this `DiskManager`. The write is not guaranteed to be durable until
    /// [`DiskManager::sync_data`] is called.
    ///
    /// # Errors
    ///
    /// * Returns [`DiskError::InvalidPageSize`] if the data buffer length does not
    ///   match the configured page size.
    /// * Returns [`DiskError::Io`] if an I/O error occurs during writing.
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

    /// Flushes file data to disk (equivalent to `fdatasync`).
    pub fn sync_data(&self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
    }

    /// Performs an atomic write for small "whole file" updates, such as catalogs or manifests.
    ///
    /// The process involves:
    /// 1. Writing to a temporary file.
    /// 2. Syncing the temporary file to disk.
    /// 3. Renaming the temporary file to the destination path (atomic).
    /// 4. Syncing the parent directory to ensure the metadata update is durable.
    pub fn atomic_write_file(&self, path: &Path, data: &[u8]) -> Result<()> {
        let temp_path = path.with_extension("tmp");

        let mut f = File::create(&temp_path)?;
        f.write_all(data)?;

        f.sync_all()?;
        drop(f);

        fs::rename(&temp_path, path)?;

        if let Some(parent) = path.parent() {
            let dir = File::open(parent)?;
            dir.sync_all()?;
        };

        Ok(())
    }

    /// Flushes both the file and its parent directory to ensure metadata
    /// (such as rename or creation) is durable.
    pub fn sync_file_and_dir(file: &File, file_path: &Path) -> Result<()> {
        file.sync_all()?;
        if let Some(parent) = file_path.parent() {
            let dir = File::open(parent)?;
            dir.sync_all()?;
        }
        Ok(())
    }
}
