use crc32fast::Hasher;
use std::fmt::{Display, Formatter};
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use std::vec;

use crate::disk::sync_file_to_disk;
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
            _ => Err(WalError::CorruptedLog(format!(
                "Invalid entry type: {}",
                value
            ))),
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

// Layout of a WAL record
// | lsn | entry_type | key_len | value_len | timestamp | key_bytes | value_bytes | checksum |

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
        let mut lsn_buf = [0u8; 8];
        if let Err(e) = self.reader.read_exact(&mut lsn_buf) {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                return None;
            }
            return Some(Err(e.into()));
        }
        let lsn = Lsn::from_le_bytes(lsn_buf);

        let mut type_buf = [0u8; 1];
        if let Err(e) = self.reader.read_exact(&mut type_buf) {
            return Some(Err(e.into()));
        }
        let entry_type = match WalEntryType::try_from(type_buf[0]) {
            Ok(t) => t,
            Err(e) => return Some(Err(e)),
        };

        let mut key_len_buf = [0u8; 8];
        if let Err(e) = self.reader.read_exact(&mut key_len_buf) {
            return Some(Err(e.into()));
        }
        let key_len = u64::from_le_bytes(key_len_buf);

        let mut value_len_buf = [0u8; 8];
        if let Err(e) = self.reader.read_exact(&mut value_len_buf) {
            return Some(Err(e.into()));
        }
        let value_len = u64::from_le_bytes(value_len_buf);

        let mut timestamp_buf = [0u8; 8];
        if let Err(e) = self.reader.read_exact(&mut timestamp_buf) {
            return Some(Err(e.into()));
        }
        let timestamp = u64::from_le_bytes(timestamp_buf);

        let mut key = vec![0u8; key_len as usize];
        if let Err(e) = self.reader.read_exact(&mut key) {
            return Some(Err(e.into()));
        }

        let value = if value_len > 0 {
            let mut val_buf = vec![0u8; value_len as usize];
            if let Err(e) = self.reader.read_exact(&mut val_buf) {
                return Some(Err(e.into()));
            }
            Some(val_buf)
        } else {
            None
        };

        let mut checksum_buf = [0u8; 4];
        if let Err(e) = self.reader.read_exact(&mut checksum_buf) {
            return Some(Err(e.into()));
        }
        let expected_checksum = u32::from_le_bytes(checksum_buf);

        let mut hasher = Hasher::new();
        hasher.update(&lsn_buf);
        hasher.update(&type_buf);
        hasher.update(&key_len_buf);
        hasher.update(&value_len_buf);
        hasher.update(&timestamp_buf);
        hasher.update(&key);
        if let Some(ref v) = value {
            hasher.update(v);
        }
        let actual_checksum = hasher.finalize();

        if actual_checksum != expected_checksum {
            return Some(Err(WalError::CorruptedLog(format!(
                "Checksum mismatch for LSN {}: expected {}, got {}",
                lsn, expected_checksum, actual_checksum
            ))));
        }

        Some(Ok(WalEntry {
            lsn,
            entry_type,
            key,
            value,
            timestamp,
        }))
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
    pub fn append(
        &mut self,
        lsn: Lsn,
        entry_type: WalEntryType,
        key: &[u8],
        value: Option<&[u8]>,
    ) -> Result<Lsn> {
        let next_lsn = lsn + 1;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| WalError::CorruptedLog(e.to_string()))?
            .as_micros() as u64;
        let key_len = key.len() as u64;
        let value_bytes = match entry_type {
            WalEntryType::Put => {
                value.ok_or_else(|| WalError::CorruptedLog("Put entry missing value".into()))?
            }
            WalEntryType::Delete => &[],
        };
        let value_len = value_bytes.len() as u64;

        let mut record = Vec::new();

        record.extend_from_slice(&lsn.to_le_bytes());
        record.push(entry_type as u8);
        record.extend_from_slice(&key_len.to_le_bytes());
        record.extend_from_slice(&value_len.to_le_bytes());
        record.extend_from_slice(&timestamp.to_le_bytes());
        record.extend_from_slice(key);
        record.extend_from_slice(value_bytes);

        let mut hasher = Hasher::new();
        hasher.update(&record);
        let checksum = hasher.finalize();

        self.file.write_all(&record)?;
        self.file.write_all(&checksum.to_le_bytes())?;
        Ok(next_lsn)
    }

    pub fn flush(&mut self) -> Result<()> {
        self.file.flush()?;
        sync_file_to_disk();
        Ok(())
    }
}
