//! Persistence for the last seen filesystem event id.
//!
//! Why a trait: macOS FSEvents can replay everything since a stored event id, which would let a restart
//! catch up on changes made while the app was closed. `notify` 6 does not expose that API, so only the
//! storage side exists here.
//!
//! TODO: wire FSEvents replay (`kFSEventStreamEventIdSinceNow` / stored id) once the engine needs it.
//! Until then the periodic re-sync covers changes made while the app was closed.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

pub trait EventIdStore: Send + Sync {
    fn load(&self) -> Result<Option<u64>>;
    fn save(&self, id: u64) -> Result<()>;
}

/// Stores the id as decimal text in one file. Writes go to a temp file first so a crash cannot leave a
/// half-written id behind.
pub struct FileEventIdStore {
    path: PathBuf,
}

impl FileEventIdStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl EventIdStore for FileEventIdStore {
    fn load(&self) -> Result<Option<u64>> {
        match fs::read_to_string(&self.path) {
            Ok(s) => {
                let id = s
                    .trim()
                    .parse::<u64>()
                    .with_context(|| format!("bad event id in {}", self.path.display()))?;
                Ok(Some(id))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    fn save(&self, id: u64) -> Result<()> {
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, id.to_string()).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileEventIdStore::new(dir.path().join("event-id"));
        assert_eq!(store.load().unwrap(), None);
        store.save(12_345_678_901).unwrap();
        assert_eq!(store.load().unwrap(), Some(12_345_678_901));
        store.save(7).unwrap();
        assert_eq!(store.load().unwrap(), Some(7));
    }
}
