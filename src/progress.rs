//! Progress display.
//!
//! One bar for the whole download and one per connection slot. Bars are
//! optional: with `--quiet`, or when stderr is not a terminal, every method
//! becomes a no-op and the caller does not have to care.

use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::fsutil::human_bytes;

struct Inner {
    multi: MultiProgress,
    overall: ProgressBar,
    workers: Vec<ProgressBar>,
    total: u64,
}

/// Renders transfer progress on stderr.
pub struct Reporter {
    inner: Option<Inner>,
    observer: Option<Arc<dyn Fn(u64) + Send + Sync>>,
}

impl Reporter {
    /// Build a reporter for `total` bytes spread over `workers` slots.
    pub fn new(total: u64, workers: usize, enabled: bool) -> Self {
        if !enabled || !std::io::stderr().is_terminal() {
            return Self {
                inner: None,
                observer: None,
            };
        }

        let multi = MultiProgress::new();
        let overall = multi.add(ProgressBar::new(total));
        overall.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} {bytes:>12}/{total_bytes:<12} {bytes_per_sec:>14} eta {eta:>8}  {msg}",
            )
            .expect("valid template")
            .progress_chars("=> "),
        );
        overall.enable_steady_tick(Duration::from_millis(150));

        let workers = (0..workers.max(1))
            .map(|i| {
                let bar = multi.add(ProgressBar::new(0));
                bar.set_style(
                    ProgressStyle::with_template(
                        "  c{prefix:.bold} [{bar:28.cyan/blue}] {bytes:>11}/{total_bytes:<11} {percent:>3}%",
                    )
                    .expect("valid template")
                    .progress_chars("=> "),
                );
                bar.set_prefix(format!("{i:<2}"));
                bar
            })
            .collect();

        Self {
            inner: Some(Inner {
                multi,
                overall,
                workers,
                total,
            }),
            observer: None,
        }
    }

    /// Observe real transferred bytes even when terminal rendering is disabled.
    pub fn with_observer(mut self, observer: Arc<dyn Fn(u64) + Send + Sync>) -> Self {
        self.observer = Some(observer);
        self
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Print a line above the bars, so it is not overwritten.
    pub fn println(&self, message: &str) {
        match &self.inner {
            Some(inner) => {
                let _ = inner.multi.println(message);
            }
            None => eprintln!("{message}"),
        }
    }

    /// A worker has picked up a segment.
    pub fn worker_start(&self, worker: usize, label: &str, segment_len: u64, already_done: u64) {
        if let Some(inner) = &self.inner {
            if let Some(bar) = inner.workers.get(worker) {
                bar.set_length(segment_len);
                bar.set_position(already_done);
                bar.set_message(label.to_string());
            }
        }
    }

    pub fn worker_advance(&self, worker: usize, bytes: u64) {
        if let Some(inner) = &self.inner {
            if let Some(bar) = inner.workers.get(worker) {
                bar.inc(bytes);
            }
        }
    }

    pub fn worker_finish(&self, worker: usize) {
        if let Some(inner) = &self.inner {
            if let Some(bar) = inner.workers.get(worker) {
                bar.set_position(bar.length().unwrap_or(0));
                bar.set_message("done");
            }
        }
    }

    pub fn set_status(&self, status: &str) {
        if let Some(inner) = &self.inner {
            inner.overall.set_message(status.to_string());
        }
    }

    pub fn advance(&self, bytes: u64) {
        if let Some(observer) = &self.observer {
            observer(bytes);
        }
        if let Some(inner) = &self.inner {
            inner.overall.inc(bytes);
        }
    }

    /// Finish, leaving one summary line behind.
    pub fn finish(&self, message: &str) {
        if let Some(inner) = &self.inner {
            inner.overall.set_position(inner.total);
            inner.overall.finish_with_message(message.to_string());
            for bar in &inner.workers {
                bar.finish_and_clear();
            }
        }
    }

    /// Abandon the display when the transfer failed or was interrupted.
    pub fn abandon(&self, message: &str) {
        if let Some(inner) = &self.inner {
            inner.overall.abandon_with_message(message.to_string());
            for bar in &inner.workers {
                bar.finish_and_clear();
            }
        }
    }
}

/// One-line plan summary, used for `--dry-run` and verbose runs.
pub fn describe_rate(bytes_per_sec: Option<u64>) -> String {
    match bytes_per_sec {
        Some(rate) => format!("limited to {}", crate::fsutil::human_rate(rate as f64)),
        None => "unlimited".to_string(),
    }
}

/// Render `bytes` for messages.
pub fn format_bytes(bytes: u64) -> String {
    human_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_reporter_is_silent() {
        let reporter = Reporter::new(100, 2, false);
        assert!(!reporter.is_enabled());
        // None of these should panic.
        reporter.advance(10);
        reporter.worker_start(0, "x", 10, 0);
        reporter.worker_advance(0, 5);
        reporter.worker_finish(0);
        reporter.set_status("busy");
        reporter.finish("ok");
        reporter.abandon("bad");
    }

    #[test]
    fn formats_bytes_for_messages() {
        assert_eq!(format_bytes(2048), "2.00 KiB");
        assert_eq!(describe_rate(None), "unlimited");
        assert!(describe_rate(Some(1024)).contains("1.00 KiB/s"));
    }
}
