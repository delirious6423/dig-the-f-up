//! Cross-process "I am the indexer" lock for an index folder.
//!
//! The app's service and `dtfu index` both write the index. Two writers would embed the same files twice and
//! queue behind each other's SQLite write locks, so the process that indexes holds this lock for as long as it
//! does. A process that does not get it can still search (readers never take it).
//!
//! The lock is an OS advisory lock on `indexer.lock` inside the index folder. The OS drops it when the process
//! exits or crashes, so a stale lock cannot survive its owner. Keep the index folder on a local disk: advisory
//! locks are unreliable on some network file systems.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use anyhow::{Context, Result};

const FILE: &str = "indexer.lock";

/// Held while this process is the one indexing the folder; released on drop.
#[derive(Debug)]
pub struct IndexerLock {
    _file: File,
}

impl IndexerLock {
    /// `Ok(None)` when another process holds it. The folder must exist.
    pub fn try_acquire(dir: &Path) -> Result<Option<IndexerLock>> {
        let path = dir.join(FILE);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(IndexerLock { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e).with_context(|| format!("locking {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_holder_is_refused_until_the_first_drops() {
        let tmp = tempfile::tempdir().unwrap();
        let first = IndexerLock::try_acquire(tmp.path()).unwrap().expect("free folder locks");
        assert!(IndexerLock::try_acquire(tmp.path()).unwrap().is_none());
        drop(first);
        assert!(IndexerLock::try_acquire(tmp.path()).unwrap().is_some());
    }

    #[test]
    fn different_folders_do_not_interfere() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let _la = IndexerLock::try_acquire(a.path()).unwrap().unwrap();
        assert!(IndexerLock::try_acquire(b.path()).unwrap().is_some());
    }
}
