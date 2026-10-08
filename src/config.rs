//! Options shared by the CLI and the download engine.

use std::path::PathBuf;

/// How the remote file should be carved up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentPolicy {
    /// Maximum number of simultaneous data transfers.
    pub connections: usize,
    /// Fixed segment size, or `None` to derive one from the file size.
    pub segment_size: Option<u64>,
    /// Never create segments smaller than this.
    pub min_segment: u64,
    /// Upper bound on the segment count, to keep the resume file small.
    pub max_segments: usize,
    /// Target segments per connection when no explicit size is given.
    pub segments_per_connection: usize,
}

impl Default for SegmentPolicy {
    fn default() -> Self {
        Self {
            connections: 4,
            segment_size: None,
            min_segment: 1024 * 1024,
            max_segments: 512,
            segments_per_connection: 4,
        }
    }
}

/// Everything the downloader needs for one file.
#[derive(Debug, Clone)]
pub struct DownloadOptions {
    /// Path on the server, relative to the login directory or absolute.
    pub remote: String,
    /// Destination on disk.
    pub local: PathBuf,
    pub policy: SegmentPolicy,
    /// Reuse an existing partial download when one is found.
    pub resume: bool,
    /// Throw away any partial download and start over.
    pub force: bool,
    /// Global bandwidth cap in bytes per second.
    pub limit: Option<u64>,
    /// Per-segment retry attempts.
    pub retries: usize,
    /// Keep the resume file after a successful download.
    pub keep_state: bool,
    /// Suppress the progress display.
    pub quiet: bool,
    /// Report the plan without transferring anything.
    pub dry_run: bool,
}

impl DownloadOptions {
    pub fn new(remote: impl Into<String>, local: impl Into<PathBuf>) -> Self {
        Self {
            remote: remote.into(),
            local: local.into(),
            policy: SegmentPolicy::default(),
            resume: true,
            force: false,
            limit: None,
            retries: 3,
            keep_state: false,
            quiet: false,
            dry_run: false,
        }
    }
}

/// How a download ended, for the summary line.
#[derive(Debug, Clone)]
pub struct DownloadSummary {
    pub bytes: u64,
    pub segments: usize,
    pub connections_used: usize,
    pub elapsed: std::time::Duration,
    /// Bytes that were already on disk when this run started.
    pub resumed_from: u64,
    /// The destination already held the whole file, so nothing was transferred.
    pub already_complete: bool,
}

impl DownloadSummary {
    pub fn bytes_per_second(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            0.0
        } else {
            self.bytes as f64 / secs
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_sane() {
        let p = SegmentPolicy::default();
        assert!(p.connections >= 1);
        assert!(p.min_segment > 0);
        assert!(p.max_segments >= 1);
    }

    #[test]
    fn summary_computes_rate() {
        let summary = DownloadSummary {
            bytes: 2048,
            segments: 2,
            connections_used: 2,
            elapsed: std::time::Duration::from_secs(2),
            resumed_from: 0,
            already_complete: false,
        };
        assert!((summary.bytes_per_second() - 1024.0).abs() < f64::EPSILON);
    }
}
