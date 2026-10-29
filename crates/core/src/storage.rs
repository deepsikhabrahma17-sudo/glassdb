//! Raw byte storage under the pager: a real file, or memory.
//!
//! The pager only ever asks for "these bytes at this offset", so the same
//! engine runs on a file (CLI), in memory (tests) and in the browser (WASM).

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::error::Result;

pub trait Storage: Send {
    /// Fill `buf` with the bytes starting at `offset`.
    /// Fails if the range runs past the end.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;
    /// Write `data` at `offset`, growing the storage if needed.
    /// Any gap before `offset` reads back as zeros.
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()>;
    /// Current size in bytes.
    fn size(&mut self) -> Result<u64>;
    /// Make every write so far durable (fsync for files).
    fn sync(&mut self) -> Result<()>;
}

/// Storage backed by a file on disk.
pub struct FileStorage {
    file: File,
}

/// Storage held in memory. Clones share the same bytes, so a test can "close"
/// a pager and reopen the same in-memory disk.
#[derive(Clone, Default)]
pub struct MemStorage {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl FileStorage {
    /// Open `path` for reading and writing, creating it if missing.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(FileStorage { file })
    }
}

impl Storage for FileStorage {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        // read_exact fails with UnexpectedEof if the file ends early.
        self.file.read_exact(buf)?;
        Ok(())
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        // Writing past the end leaves a gap that the OS fills with zeros.
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)?;
        Ok(())
    }

    fn size(&mut self) -> Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    fn sync(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }
}

impl MemStorage {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        MemStorage {
            bytes: Arc::new(Mutex::new(bytes)),
        }
    }

    /// A copy of the current contents.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }
}

impl Storage for MemStorage {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let bytes = self.bytes.lock().unwrap();
        let start = offset as usize;
        let src = bytes
            .get(start..start + buf.len())
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        buf.copy_from_slice(src);
        Ok(())
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        let mut bytes = self.bytes.lock().unwrap();
        let start = offset as usize;
        let end = start + data.len();
        if bytes.len() < end {
            bytes.resize(end, 0);
        }
        bytes[start..end].copy_from_slice(data);
        Ok(())
    }

    fn size(&mut self) -> Result<u64> {
        Ok(self.bytes.lock().unwrap().len() as u64)
    }

    fn sync(&mut self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract every storage must meet.
    fn check_contract(storage: &mut dyn Storage) {
        assert_eq!(storage.size().unwrap(), 0);
        storage.write_at(10, b"hello").unwrap();
        assert_eq!(storage.size().unwrap(), 15);

        let mut buf = [0u8; 5];
        storage.read_at(10, &mut buf).unwrap();
        assert_eq!(&buf, b"hello");

        let mut gap = [9u8; 10];
        storage.read_at(0, &mut gap).unwrap();
        assert_eq!(gap, [0; 10], "gap before a write reads as zeros");

        let mut past_end = [0u8; 4];
        assert!(storage.read_at(13, &mut past_end).is_err());

        storage.write_at(0, b"ab").unwrap();
        assert_eq!(storage.size().unwrap(), 15, "overwrite does not grow");
        storage.sync().unwrap();
    }

    #[test]
    fn mem_storage_meets_contract() {
        check_contract(&mut MemStorage::new());
    }

    #[test]
    fn mem_storage_clones_share_bytes() {
        let disk = MemStorage::new();
        disk.clone().write_at(0, b"xyz").unwrap();
        assert_eq!(disk.to_bytes(), b"xyz");
        assert_eq!(MemStorage::from_bytes(vec![1, 2]).to_bytes(), vec![1, 2]);
    }

    #[test]
    fn file_storage_meets_contract_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("storage.db");
        check_contract(&mut FileStorage::open(&path).unwrap());

        let mut reopened = FileStorage::open(&path).unwrap();
        let mut buf = [0u8; 5];
        reopened.read_at(10, &mut buf).unwrap();
        assert_eq!(&buf, b"hello");
    }
}
