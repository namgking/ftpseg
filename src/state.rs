//! Resume bookkeeping.
//!
//! The state file records exactly where each byte range ended up, so an
//! interrupted download can pick up mid-segment. It is written atomically
//! (temp file plus rename) so a crash can never leave a truncated state file
//! that would misreport progress.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::plan::Segment;

/// Current state file schema version.
pub const STATE_VERSION: u32 = 1;

/// Suffix appended to the destination path.
pub const STATE_SUFFIX: &str = ".ftpseg-state.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SegmentRecord {
    pub start: u64,
    pub len: u64,
    #[serde(default)]
    pub done: u64,
}

impl From<&Segment> for SegmentRecord {
    fn from(segment: &Segment) -> Self {
        Self {
            start: segment.start,
            len: segment.len,
            done: segment.done,
        }
    }
}

impl From<&SegmentRecord> for Segment {
    fn from(record: &SegmentRecord) -> Self {
        Segment {
            start: record.start,
            len: record.len,
            done: record.done,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StateFile {
    pub version: u32,
    pub remote: String,
    pub remote_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mdtm: Option<String>,
    pub local: String,
    pub updated_unix: u64,
    pub segments: Vec<SegmentRecord>,
}

impl StateFile {
    pub fn new(
        remote: impl Into<String>,
        remote_size: u64,
        mdtm: Option<String>,
        local: impl Into<String>,
        segments: &[Segment],
    ) -> Self {
        Self {
            version: STATE_VERSION,
            remote: remote.into(),
            remote_size,
            mdtm,
            local: local.into(),
            updated_unix: now_unix(),
            segments: segments.iter().map(SegmentRecord::from).collect(),
        }
    }

    pub fn segments(&self) -> Vec<Segment> {
        self.segments.iter().map(Segment::from).collect()
    }

    /// `true` when the record describes the same remote file we are looking at.
    pub fn matches(&self, remote: &str, size: u64, mdtm: Option<&str>) -> bool {
        if self.version != STATE_VERSION || self.remote != remote || self.remote_size != size {
            return false;
        }
        match (self.mdtm.as_deref(), mdtm) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }

    pub fn touch(&mut self) {
        self.updated_unix = now_unix();
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reads and writes the resume file next to the download.
#[derive(Debug, Clone)]
pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    /// The conventional location: the destination path plus a suffix.
    pub fn for_download(local: &Path) -> Self {
        let mut name = local.as_os_str().to_os_string();
        name.push(STATE_SUFFIX);
        Self {
            path: PathBuf::from(name),
        }
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.is_file()
    }

    /// Load the state file, treating anything unreadable as "no state".
    ///
    /// A corrupt state file must never block a download, so parse failures are
    /// reported and then ignored.
    pub fn load(&self) -> Option<StateFile> {
        let data = std::fs::read_to_string(&self.path).ok()?;
        match serde_json::from_str::<StateFile>(&data) {
            Ok(state) => Some(state),
            Err(e) => {
                eprintln!(
                    "warning: ignoring unreadable resume file {} ({e})",
                    self.path.display()
                );
                None
            }
        }
    }

    /// Write the state file atomically.
    pub fn save(&self, state: &StateFile) -> Result<()> {
        let json = serde_json::to_string_pretty(state)
            .map_err(|e| crate::error::Error::Config(format!("cannot serialise state: {e}")))?;
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn remove(&self) {
        let _ = std::fs::remove_file(&self.path);
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let _ = std::fs::remove_file(PathBuf::from(tmp));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{plan_segments, Segment};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ftpseg-state-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = temp_dir("roundtrip");
        let local = dir.join("file.bin");
        let store = StateStore::for_download(&local);
        assert!(store.path().to_string_lossy().ends_with(STATE_SUFFIX));
        assert!(!store.exists());

        let mut segments: Vec<Segment> = plan_segments(4096, &Default::default());
        segments[0].advance(512);
        let state = StateFile::new(
            "/remote/file.bin",
            4096,
            None,
            local.to_string_lossy(),
            &segments,
        );
        store.save(&state).unwrap();
        assert!(store.exists());

        let loaded = store.load().unwrap();
        assert_eq!(loaded, state);
        assert_eq!(loaded.segments()[0].done, 512);

        store.remove();
        assert!(!store.exists());
        assert!(store.load().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_state_is_ignored() {
        let dir = temp_dir("corrupt");
        let local = dir.join("file.bin");
        let store = StateStore::for_download(&local);
        std::fs::write(store.path(), b"{ this is not json").unwrap();
        assert!(store.load().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn matching_requires_the_same_remote_file() {
        let segments = plan_segments(4096, &Default::default());
        let state = StateFile::new(
            "/remote/a.bin",
            4096,
            Some("20240102030405".into()),
            "a.bin",
            &segments,
        );

        assert!(state.matches("/remote/a.bin", 4096, Some("20240102030405")));
        assert!(!state.matches("/remote/b.bin", 4096, Some("20240102030405")));
        assert!(!state.matches("/remote/a.bin", 8192, Some("20240102030405")));
        assert!(!state.matches("/remote/a.bin", 4096, Some("20250101000000")));
        // A server without MDTM cannot contradict us.
        assert!(state.matches("/remote/a.bin", 4096, None));
    }
}
