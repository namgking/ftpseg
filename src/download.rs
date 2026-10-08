//! The segmented download engine.
//!
//! Shape of a download:
//!
//! 1. one probe connection learns the file size, modification time and whether
//!    the server honours `REST`;
//! 2. the file is cut into segments, reusing a previous run's state file when
//!    one matches;
//! 3. a pool of logged-in control connections feeds a set of workers, each of
//!    which claims segments from a shared queue and writes straight into the
//!    destination file at absolute offsets.
//!
//! Work stealing from a queue is what keeps the tail of a transfer short: a
//! connection that happens to be fast simply claims more segments.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::sync::{watch, Mutex};

use crate::client::{ConnectOptions, FeatureSet, FtpClient};
use crate::config::{DownloadOptions, DownloadSummary, SegmentPolicy};
use crate::encoding::Charset;
use crate::error::{Error, Result};
use crate::fsutil;
use crate::paths;
use crate::plan::{self, Segment};
use crate::pool::ConnectionPool;
use crate::progress::Reporter;
use crate::ratelimit::RateLimiter;
use crate::state::{StateFile, StateStore};
use crate::transport::shutdown_quietly;

/// Read size per data connection. Large enough that syscall overhead stays
/// invisible, small enough to keep the memory footprint flat as connections
/// grow.
const CHUNK: usize = 1024 * 1024;

/// How often the resume file is refreshed while transferring.
const STATE_FLUSH_INTERVAL: Duration = Duration::from_secs(2);

/// What the server can tell us about the remote file.
#[derive(Debug, Clone)]
pub struct RemoteInfo {
    pub size: Option<u64>,
    pub mdtm: Option<String>,
    pub features: FeatureSet,
    /// The server positively confirmed `REST` works, not just advertised it.
    pub rest_supported: bool,
    pub is_dir: bool,
    /// The character set that was negotiated for paths and credentials.
    pub charset: Charset,
}

impl RemoteInfo {
    pub fn is_segmentable(&self) -> bool {
        self.rest_supported && self.size.map(|s| s > 0).unwrap_or(false)
    }
}

/// Log in once and collect everything needed to plan a download.
pub async fn probe(connect: &ConnectOptions, remote: &str) -> Result<RemoteInfo> {
    let mut client = FtpClient::connect(connect).await?;

    let mut size = client.size(remote).await?;
    let mut is_dir = false;
    if size.is_none() {
        // Not every server implements SIZE, and some report a directory size.
        if let Some(entry) = find_entry(&mut client, remote).await {
            if entry.is_dir {
                is_dir = true;
            } else {
                size = entry.size;
            }
        }
    }

    let mdtm = client.mdtm(remote).await.unwrap_or(None);
    let rest_supported = client.supports_rest().await;
    let features = client.features().clone();
    let charset = client.charset();
    client.quit().await;

    Ok(RemoteInfo {
        size,
        mdtm,
        features,
        rest_supported,
        is_dir,
        charset,
    })
}

/// Look the remote path up in its parent directory listing.
async fn find_entry(client: &mut FtpClient, remote: &str) -> Option<crate::protocol::DirEntry> {
    let (parent, name) = paths::split_remote(remote);
    let entries = client.list(parent.as_deref()).await.ok()?;
    entries.into_iter().find(|entry| entry.name == name)
}

/// Shared, immutable-ish state handed to every worker.
struct Context {
    remote: String,
    remote_size: u64,
    mdtm: Option<String>,
    segments: Mutex<Vec<Segment>>,
    queue: Mutex<VecDeque<usize>>,
    pool: Arc<ConnectionPool>,
    reporter: Arc<Reporter>,
    limiter: Option<Arc<RateLimiter>>,
    file: Arc<std::fs::File>,
    io_timeout: Duration,
    retries: usize,
    state: StateStore,
    local_display: String,
    /// `true` once the transfer must stop.
    cancel: watch::Receiver<bool>,
}

impl Context {
    fn is_cancelled(&self) -> bool {
        *self.cancel.borrow()
    }
}

/// Download `options.remote` into `options.local`, in parallel segments.
///
/// `cancel` is watched so the caller can stop cleanly (Ctrl-C, or a test):
/// the resume file is written before returning [`Error::Aborted`].
pub async fn download_file(
    connect: Arc<ConnectOptions>,
    options: DownloadOptions,
    cancel: watch::Receiver<bool>,
) -> Result<DownloadSummary> {
    download_file_with_progress(connect, options, cancel, None).await
}

/// As `download_file`, with an optional callback invoked for each written chunk.
/// The callback receives a delta, not the cumulative total; it must return quickly.
pub async fn download_file_with_progress(
    connect: Arc<ConnectOptions>,
    options: DownloadOptions,
    cancel: watch::Receiver<bool>,
    progress: Option<Arc<dyn Fn(u64) + Send + Sync>>,
) -> Result<DownloadSummary> {
    let started = Instant::now();
    let store = StateStore::for_download(&options.local);
    let info = probe(&connect, &options.remote).await?;

    if info.is_dir {
        return Err(Error::Config(format!(
            "{} is a directory on the server",
            options.remote
        )));
    }

    let Some(remote_size) = info.size else {
        // Without a size there is nothing to split; fall back to one stream.
        return single_stream(connect, options, cancel, started, None, progress).await;
    };

    // Re-running a finished download should be a no-op rather than a surprise
    // full re-download.
    if options.resume && !options.force && !store.exists() {
        if let Ok(metadata) = std::fs::metadata(&options.local) {
            if metadata.is_file() && metadata.len() == remote_size && remote_size > 0 {
                return Ok(DownloadSummary {
                    bytes: remote_size,
                    segments: 0,
                    connections_used: 0,
                    elapsed: started.elapsed(),
                    resumed_from: remote_size,
                    already_complete: true,
                });
            }
        }
    }

    if !info.is_segmentable() {
        if remote_size > 0 {
            let why = if info.rest_supported {
                "server does not report a size"
            } else {
                "server does not support REST (byte-range restart)"
            };
            eprintln!(
                "note: downloading {} with a single connection: {why}",
                options.remote
            );
        }
        return single_stream(
            connect,
            options,
            cancel,
            started,
            Some(remote_size),
            progress,
        )
        .await;
    }

    let (mut segments, mut resumed_from) =
        build_plan(&store, &options, remote_size, info.mdtm.as_deref())?;

    // A resume trusts bytes that are supposedly already on disk. If the file
    // those bytes live in has gone missing or has been shrunk, that trust is
    // unfounded and the only safe move is to start over.
    if resumed_from > 0 && !local_has_resumable_data(&options.local, remote_size) {
        eprintln!(
            "warning: {} is missing or shorter than the recorded progress; starting over",
            options.local.display()
        );
        store.remove();
        segments = plan::plan_segments(remote_size, &options.policy);
        resumed_from = 0;
    }

    if options.dry_run {
        return Ok(DownloadSummary {
            bytes: remote_size,
            segments: segments.len(),
            connections_used: 0,
            elapsed: started.elapsed(),
            resumed_from,
            already_complete: false,
        });
    }

    let mut reporter = Reporter::new(remote_size, options.policy.connections, !options.quiet);
    if let Some(observer) = progress {
        reporter = reporter.with_observer(observer);
    }
    let reporter = Arc::new(reporter);
    reporter.set_status("starting");
    if resumed_from > 0 {
        reporter.println(&format!(
            "resuming {} from {} of {}",
            options.local.display(),
            fsutil::human_bytes(resumed_from),
            fsutil::human_bytes(remote_size)
        ));
    }

    // Fresh downloads replace whatever was there; resumed ones keep the bytes
    // already validated against the state file.
    let truncate = resumed_from == 0;
    let file = Arc::new(fsutil::open_destination(&options.local, truncate)?);
    if truncate {
        if let Err(e) = fsutil::preallocate(&file, remote_size) {
            // Preallocation is an optimisation, never a correctness need.
            reporter.println(&format!("note: could not preallocate file ({e})"));
        }
    }

    let pending: VecDeque<usize> = (0..segments.len())
        .filter(|index| !segments[*index].is_complete())
        .collect();
    if pending.is_empty() {
        store.remove();
        reporter.finish("already complete");
        return Ok(DownloadSummary {
            bytes: remote_size,
            segments: segments.len(),
            connections_used: 0,
            elapsed: started.elapsed(),
            resumed_from,
            already_complete: true,
        });
    }

    let pool = ConnectionPool::new(Arc::clone(&connect), options.policy.connections);

    // Record the plan immediately, so even a hard kill leaves a usable record.
    let mut initial = StateFile::new(
        options.remote.clone(),
        remote_size,
        info.mdtm.clone(),
        options.local.to_string_lossy(),
        &segments,
    );
    initial.touch();
    let _ = store.save(&initial);

    let context = Arc::new(Context {
        remote: options.remote.clone(),
        remote_size,
        mdtm: info.mdtm.clone(),
        segments: Mutex::new(segments.clone()),
        queue: Mutex::new(pending),
        pool: Arc::clone(&pool),
        reporter: Arc::clone(&reporter),
        limiter: options.limit.map(|rate| Arc::new(RateLimiter::new(rate))),
        file: Arc::clone(&file),
        io_timeout: connect.timeout,
        retries: options.retries,
        state: store.clone(),
        local_display: options.local.to_string_lossy().into_owned(),
        cancel: cancel.clone(),
    });

    let saver = spawn_state_saver(Arc::clone(&context));

    let mut handles = Vec::with_capacity(options.policy.connections);
    for worker_id in 0..options.policy.connections.max(1) {
        let context = Arc::clone(&context);
        handles.push(tokio::spawn(async move {
            worker_loop(context, worker_id).await
        }));
    }

    let mut errors = Vec::new();
    for handle in handles {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => errors.push(e),
            Err(join_error) => errors.push(Error::Protocol(format!(
                "download worker terminated unexpectedly: {join_error}"
            ))),
        }
    }
    saver.abort();
    // Join the aborted saver before returning or writing the final checkpoint.
    // GUI cancellation may remove the target/checkpoint once this call returns.
    let _ = saver.await;

    if let Some(error) = errors.into_iter().find(|e| !matches!(e, Error::Aborted)) {
        // The transfer failed, so persist exactly how far each segment got.
        save_state(&context).await;
        reporter.abandon(&format!("failed: {error}"));
        pool.shutdown().await;
        return Err(error);
    }

    if context.is_cancelled() {
        save_state(&context).await;
        let done = plan::completed_bytes(&context.segments.lock().await);
        reporter.abandon(&format!("interrupted at {}", fsutil::human_bytes(done)));
        pool.shutdown().await;
        return Err(Error::Aborted);
    }

    // Every segment claims to be done; verify before declaring success.
    let segments = context.segments.lock().await.clone();
    let written = plan::completed_bytes(&segments);
    if written != remote_size {
        save_state(&context).await;
        reporter.abandon("transfer incomplete");
        pool.shutdown().await;
        return Err(Error::ShortTransfer {
            expected: remote_size,
            actual: written,
        });
    }

    // Bookkeeping can be right while the writes went somewhere else, so check
    // the file on disk too before calling this a success.
    if !verify_local_size(&options.local, remote_size)? {
        let actual = std::fs::metadata(&options.local)
            .map(|meta| meta.len())
            .unwrap_or(0);
        save_state(&context).await;
        reporter.abandon("file size does not match");
        pool.shutdown().await;
        return Err(Error::ShortTransfer {
            expected: remote_size,
            actual,
        });
    }

    let _ = file.sync_all();
    if !options.keep_state {
        store.remove();
    }
    pool.shutdown().await;

    let elapsed = started.elapsed();
    reporter.finish(&format!("done in {:.1}s", elapsed.as_secs_f64()));
    Ok(DownloadSummary {
        bytes: remote_size,
        segments: segments.len(),
        connections_used: pool.created(),
        elapsed,
        resumed_from,
        already_complete: false,
    })
}

/// Reuse the stored plan when it still describes the same remote file.
fn build_plan(
    store: &StateStore,
    options: &DownloadOptions,
    remote_size: u64,
    mdtm: Option<&str>,
) -> Result<(Vec<Segment>, u64)> {
    if options.resume && !options.force {
        if let Some(state) = store.load() {
            if state.matches(&options.remote, remote_size, mdtm) {
                let stored = state.segments();
                match plan::validate_resumed(&stored, remote_size) {
                    Some(segments) => {
                        let done = plan::completed_bytes(&segments);
                        return Ok((segments, done));
                    }
                    None => eprintln!(
                        "warning: resume file {} does not match the remote file; starting over",
                        store.path().display()
                    ),
                }
            } else {
                eprintln!(
                    "warning: remote file changed since {}; starting over",
                    store.path().display()
                );
            }
        }
    }

    let segments = plan::plan_segments(remote_size, &options.policy);
    store.remove();
    Ok((segments, 0))
}

async fn save_state(context: &Context) {
    let segments = context.segments.lock().await.clone();
    let mut state = StateFile::new(
        context.remote.clone(),
        context.remote_size,
        context.mdtm.clone(),
        context.local_display.clone(),
        &segments,
    );
    state.touch();
    if let Err(e) = context.state.save(&state) {
        eprintln!("warning: could not write resume file: {e}");
    }
}

/// Refresh the resume file periodically, independent of segment boundaries.
fn spawn_state_saver(context: Arc<Context>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(STATE_FLUSH_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // the first tick fires immediately
        loop {
            ticker.tick().await;
            if context.is_cancelled() {
                continue;
            }
            let segments = context.segments.lock().await.clone();
            let mut state = StateFile::new(
                context.remote.clone(),
                context.remote_size,
                context.mdtm.clone(),
                context.local_display.clone(),
                &segments,
            );
            state.touch();
            if let Err(e) = context.state.save(&state) {
                eprintln!("warning: could not write resume file: {e}");
                return;
            }
        }
    })
}

/// Claim segments from the queue until it runs dry.
async fn worker_loop(context: Arc<Context>, worker_id: usize) -> Result<()> {
    loop {
        if context.is_cancelled() {
            return Err(Error::Aborted);
        }
        let next = {
            let mut queue = context.queue.lock().await;
            queue.pop_front()
        };
        let Some(index) = next else {
            return Ok(());
        };

        match run_segment(Arc::clone(&context), worker_id, index).await {
            Ok(()) => {}
            Err(error) => {
                // Stop the other workers; this one reports the real reason.
                context.reporter.worker_finish(worker_id);
                return Err(error);
            }
        }
    }
}

/// Run one segment to completion, retrying transient failures.
async fn run_segment(context: Arc<Context>, worker_id: usize, index: usize) -> Result<()> {
    let (start, len) = {
        let segments = context.segments.lock().await;
        let segment = &segments[index];
        (segment.start, segment.len)
    };

    let mut attempt = 0usize;
    loop {
        match transfer_segment(Arc::clone(&context), worker_id, index, start, len).await {
            Ok(()) => {
                context.reporter.worker_finish(worker_id);
                return Ok(());
            }
            Err(Error::Aborted) => return Err(Error::Aborted),
            Err(error) => {
                if attempt >= context.retries || !error.is_retryable() {
                    return Err(error);
                }
                attempt += 1;
                let backoff = Duration::from_millis(250 * (1u64 << (attempt - 1).min(5)));
                context.reporter.println(&format!(
                    "segment {index} at {}: {error} — retry {attempt}/{} in {:?}",
                    fsutil::human_bytes(start),
                    context.retries,
                    backoff
                ));
                tokio::time::sleep(backoff).await;
                if context.is_cancelled() {
                    return Err(Error::Aborted);
                }
            }
        }
    }
}

/// One attempt at one segment: log in the range, stream it, write it.
async fn transfer_segment(
    context: Arc<Context>,
    worker_id: usize,
    index: usize,
    start: u64,
    len: u64,
) -> Result<()> {
    let mut done = {
        let segments = context.segments.lock().await;
        segments[index].done
    };
    if done >= len {
        return Ok(());
    }

    let mut conn = context.pool.acquire().await?;
    // `done` counts the bytes of this segment that are already on disk, so the
    // resume offset the server needs and the position we write at are both
    // derived from it. Mixing the two up would silently corrupt every resumed
    // segment beyond its first partial chunk.
    let offset = start + done;

    let mut data = match conn
        .client()
        .begin_retr(&context.remote, Some(offset))
        .await
    {
        Ok(stream) => stream,
        Err(error) => {
            // A refused or unsupported RETR means the control connection is
            // fine but the request was not; a transport failure means the
            // connection is suspect.
            if matches!(error, Error::Io(_) | Error::Timeout(_) | Error::Protocol(_)) {
                conn.mark_broken();
            }
            return Err(error);
        }
    };

    context.reporter.worker_start(
        worker_id,
        &format!("#{index} @{}", fsutil::human_bytes(start)),
        len,
        done,
    );

    // The server closes the data connection itself once the file ends, so a
    // segment that reaches the last byte needs no abort and no resync.
    let mut early_close = start + len < context.remote_size;
    let mut buffer = vec![0u8; CHUNK];

    let stream_result: Result<()> = async {
        while done < len {
            if context.is_cancelled() {
                return Err(Error::Aborted);
            }

            let want = (len - done).min(CHUNK as u64) as usize;
            let mut cancel = context.cancel.clone();
            let n = read_with_cancel(
                &mut data,
                &mut buffer[..want],
                context.io_timeout,
                &mut cancel,
            )
            .await?;

            if n == 0 {
                // End of file before the segment was filled: either the remote
                // file shrank or the server stopped early.
                early_close = false;
                break;
            }

            if let Some(limiter) = &context.limiter {
                limiter.acquire(n).await;
            }

            write_chunk(&context.file, &buffer[..n], start + done)?;

            done += n as u64;
            {
                let mut segments = context.segments.lock().await;
                segments[index].done = done;
            }
            context.reporter.advance(n as u64);
            context.reporter.worker_advance(worker_id, n as u64);
        }
        Ok(())
    }
    .await;

    // Always close the data connection before reading the transfer's verdict.
    shutdown_quietly(&mut data).await;
    drop(data);

    let verdict = conn.client().finish_transfer(early_close).await;
    match verdict {
        Ok(()) => {}
        // The server explicitly rejected the transfer: the bytes we wrote
        // cannot be trusted.
        Err(error @ Error::Server { .. }) => {
            conn.mark_broken();
            return Err(error);
        }
        // Losing sync with an aborted transfer is expected and recoverable:
        // the data already arrived, so keep it and drop the connection.
        Err(_) => conn.mark_broken(),
    }

    stream_result?;

    if done < len {
        return Err(Error::ShortTransfer {
            expected: len,
            actual: done,
        });
    }
    Ok(())
}

/// Write to disk without stalling the runtime.
///
/// `block_in_place` keeps the worker thread usable by other tasks, but it is
/// only legal on a multi-threaded runtime, so fall back to a direct call.
/// Positioned writes leave the file cursor alone, so no cloning is needed and
/// workers never contend over a shared offset.
fn write_chunk(file: &std::fs::File, buffer: &[u8], offset: u64) -> Result<()> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| fsutil::write_at(file, buffer, offset))?;
        }
        _ => fsutil::write_at(file, buffer, offset)?,
    }
    Ok(())
}

/// Read from a data connection, stopping promptly when the transfer is
/// cancelled.
///
/// The cancellation signal is a `watch` channel. If its sender disappears then
/// nobody can cancel the download any more, so the read is awaited on its own
/// instead of spinning on a dead channel.
async fn read_with_cancel(
    data: &mut crate::transport::Stream,
    buffer: &mut [u8],
    timeout: Duration,
    cancel: &mut watch::Receiver<bool>,
) -> Result<usize> {
    let read = tokio::time::timeout(timeout, data.read(buffer));
    tokio::pin!(read);
    let mut cancellable = true;

    loop {
        if !cancellable {
            return Ok(read.await.map_err(|_| Error::Timeout(timeout))??);
        }
        tokio::select! {
            biased;
            result = &mut read => {
                return Ok(result.map_err(|_| Error::Timeout(timeout))??);
            }
            changed = cancel.changed() => match changed {
                Ok(()) if *cancel.borrow() => return Err(Error::Aborted),
                // The flag went back to false; keep waiting for data.
                Ok(()) => {}
                // No sender left, so cancellation is impossible.
                Err(_) => cancellable = false,
            },
        }
    }
}

/// Download with a single connection, for files that cannot be segmented.
async fn single_stream(
    connect: Arc<ConnectOptions>,
    options: DownloadOptions,
    cancel: watch::Receiver<bool>,
    started: Instant,
    known_size: Option<u64>,
    progress: Option<Arc<dyn Fn(u64) + Send + Sync>>,
) -> Result<DownloadSummary> {
    if options.dry_run {
        return Ok(DownloadSummary {
            bytes: known_size.unwrap_or(0),
            segments: if known_size.unwrap_or(0) > 0 { 1 } else { 0 },
            connections_used: 0,
            elapsed: started.elapsed(),
            resumed_from: 0,
            already_complete: false,
        });
    }

    let store = StateStore::for_download(&options.local);
    // A single stream always starts at byte zero, so any previous partial
    // state is meaningless here.
    store.remove();

    let mut client = FtpClient::connect(&connect).await?;
    let mut reporter = Reporter::new(known_size.unwrap_or(0), 1, !options.quiet);
    if let Some(observer) = progress {
        reporter = reporter.with_observer(observer);
    }
    let reporter = Arc::new(reporter);
    reporter.set_status("single connection");

    let file = fsutil::open_destination(&options.local, true)?;
    if let Some(size) = known_size {
        let _ = fsutil::preallocate(&file, size);
    }

    let mut data = client.begin_retr(&options.remote, None).await?;
    let limiter = options.limit.map(|rate| Arc::new(RateLimiter::new(rate)));
    let mut buffer = vec![0u8; CHUNK];
    let mut written = 0u64;

    let result: Result<()> = async {
        loop {
            if *cancel.borrow() {
                return Err(Error::Aborted);
            }
            let mut cancel = cancel.clone();
            let n = read_with_cancel(&mut data, &mut buffer, connect.timeout, &mut cancel).await?;
            if n == 0 {
                return Ok(());
            }
            if let Some(limiter) = &limiter {
                limiter.acquire(n).await;
            }
            write_chunk(&file, &buffer[..n], written)?;
            written += n as u64;
            reporter.advance(n as u64);
            reporter.worker_advance(0, n as u64);
        }
    }
    .await;

    shutdown_quietly(&mut data).await;
    drop(data);
    let verdict = client.finish_transfer(false).await;

    match (result, verdict) {
        (Err(error), _) => {
            reporter.abandon(&format!("failed: {error}"));
            return Err(error);
        }
        (Ok(()), Err(error @ Error::Server { .. })) => {
            reporter.abandon(&format!("failed: {error}"));
            return Err(error);
        }
        (Ok(()), _) => {}
    }

    if let Some(size) = known_size {
        if written != size {
            reporter.abandon("transfer incomplete");
            return Err(Error::ShortTransfer {
                expected: size,
                actual: written,
            });
        }
    }

    let _ = file.sync_all();
    let elapsed = started.elapsed();
    reporter.finish(&format!("done in {:.1}s", elapsed.as_secs_f64()));
    Ok(DownloadSummary {
        bytes: written,
        segments: 1,
        connections_used: 1,
        elapsed,
        resumed_from: 0,
        already_complete: false,
    })
}

/// Best-effort check that a completed file is still the expected size.
pub fn verify_local_size(path: &Path, expected: u64) -> Result<bool> {
    let metadata = std::fs::metadata(path)?;
    Ok(metadata.is_file() && metadata.len() == expected)
}

/// Whether the destination still holds the data a resume plan assumes.
///
/// A preallocated download is exactly `remote_size` bytes long from the start,
/// so anything shorter means the file was replaced or truncated since the
/// resume file was written.
fn local_has_resumable_data(path: &Path, remote_size: u64) -> bool {
    match std::fs::metadata(path) {
        Ok(metadata) => metadata.is_file() && metadata.len() >= remote_size,
        Err(_) => false,
    }
}

/// Plan a download without transferring, used by `stat` and `--dry-run`.
pub fn describe_plan(size: u64, policy: &SegmentPolicy) -> String {
    let segments = plan::plan_segments(size, policy);
    format!(
        "{} segments of about {} across {} connections",
        segments.len(),
        fsutil::human_bytes(segments.first().map(|s| s.len).unwrap_or(0)),
        policy.connections
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segmentability_requirements() {
        let base = RemoteInfo {
            size: Some(1024),
            mdtm: None,
            features: FeatureSet::default(),
            rest_supported: true,
            is_dir: false,
            charset: Charset::utf8(),
        };
        assert!(base.is_segmentable());

        let no_rest = RemoteInfo {
            rest_supported: false,
            ..base.clone()
        };
        assert!(!no_rest.is_segmentable());

        let no_size = RemoteInfo {
            size: None,
            ..base.clone()
        };
        assert!(!no_size.is_segmentable());

        let empty = RemoteInfo {
            size: Some(0),
            ..base
        };
        assert!(!empty.is_segmentable());
    }

    #[test]
    fn describes_a_plan() {
        let text = describe_plan(8 * 1024 * 1024, &SegmentPolicy::default());
        assert!(text.contains("segments"));
        assert!(text.contains("connections"));
    }

    #[test]
    fn verifies_local_size() {
        let dir = std::env::temp_dir().join(format!("ftpseg-verify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.bin");
        std::fs::write(&path, b"12345").unwrap();
        assert!(verify_local_size(&path, 5).unwrap());
        assert!(!verify_local_size(&path, 6).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
