use crc32fast::Hasher;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use std::fmt::{Display, Formatter};

use crate::page::Lsn;

#[derive(Debug)]
pub enum WalError {
    Io(io::Error),
    CorruptedLog(String),
    InvalidLsn,
}

impl From<io::Error> for WalError {
    fn from(err: io::Error) -> Self {
        WalError::Io(err)
    }
}

impl Display for WalError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            WalError::Io(err) => write!(f, "IO error: {}", err),
            WalError::CorruptedLog(msg) => write!(f, "Corrupted log: {}", msg),
            WalError::InvalidLsn => write!(f, "Invalid LSN"),
        }
    }
}

impl std::error::Error for WalError {}

pub type Result<T> = std::result::Result<T, WalError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalEntryType {
    Put = 0,
    Delete = 1,
}

impl TryFrom<u8> for WalEntryType {
    type Error = WalError;
    fn try_from(value: u8) -> Result<Self> {
        match value {
            0 => Ok(WalEntryType::Put),
            1 => Ok(WalEntryType::Delete),
            _ => Err(WalError::CorruptedLog(format!("Invalid entry type: {}", value))),
        }
    }
}

pub struct WalEntry {
    pub lsn: Lsn,
    pub entry_type: WalEntryType,
    pub key: Vec<u8>,
    pub value: Option<Vec<u8>>,
    pub timestamp: u64,
}

pub struct WalIterator {
    reader: BufReader<File>,
}

pub struct Wal {
    path: PathBuf,
    file: BufWriter<File>,
}

impl Iterator for WalIterator {
    type Item = Result<WalEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        todo!()
    }
}

impl WalIterator {
    pub fn new(path: &Path) -> io::Result<WalIterator> {
        let file = OpenOptions::new().read(true).open(path)?;
        Ok(WalIterator {
            reader: BufReader::new(file),
        })
    }
}

impl Wal {
    pub fn append(&mut self) -> Result<Lsn> {
        todo!()
    }

    pub fn flush(&mut self) -> Result<()> {
        todo!()
    }
}

