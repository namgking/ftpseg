//! End-to-end tests against the in-process FTP server.
//!
//! These cover the behaviour that actually matters for a segmented downloader:
//! that the bytes are right, that several connections really are used, that an
//! interrupted download resumes instead of restarting, and that the client
//! copes with servers that lack `REST`, `SIZE`, `EPSV` or `MLSD`.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use ftpseg::client::{ConnectOptions, FtpClient, TlsMode};
use ftpseg::config::{DownloadOptions, DownloadSummary};
use ftpseg::download;
use ftpseg::encoding::CharsetRequest;
use ftpseg::error::Error;
use ftpseg::state::StateStore;

use common::{pseudo_random, temp_dir, AbortReply, ServerConfig, TestServer, Vfs};

fn connect_options(server: &TestServer) -> ConnectOptions {
    ConnectOptions {
        host: server.host(),
        port: server.port(),
        user: "testuser".to_string(),
        password: "testpass".to_string(),
        tls: TlsMode::None,
        accept_invalid_certs: false,
        connect_timeout: Duration::from_secs(5),
        timeout: Duration::from_secs(15),
        verbose: false,
        charset: CharsetRequest::Auto,
    }
}

async fn download(
    server: &TestServer,
    remote: &str,
    local: &std::path::Path,
    connections: usize,
) -> Result<DownloadSummary, Error> {
    let mut options = DownloadOptions::new(remote, local.to_path_buf());
    options.policy.connections = connections;
    options.quiet = true;
    let (_tx, rx) = watch::channel(false);
    download::download_file(Arc::new(connect_options(server)), options, rx).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_a_small_file_with_one_connection() {
    let mut vfs = Vfs::new();
    let content = pseudo_random(200 * 1024, 7);
    vfs.add("/pub/small.bin", content.clone());
    let server = TestServer::start(vfs, ServerConfig::new()).await;

    let dir = temp_dir("small");
    let local = dir.join("small.bin");

    let summary = download(&server, "/pub/small.bin", &local, 1)
        .await
        .unwrap();

    assert_eq!(summary.bytes, content.len() as u64);
    assert!(!summary.already_complete);
    assert_eq!(std::fs::read(&local).unwrap(), content);
    assert_eq!(server.stats().retr_count(), 1);
    // A finished download leaves no resume file behind.
    assert!(!StateStore::for_download(&local).exists());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn splits_a_large_file_across_several_connections() {
    let mut vfs = Vfs::new();
    let content = pseudo_random(8 * 1024 * 1024, 11);
    vfs.add("/pub/big.bin", content.clone());
    // Slowing the server down makes overlapping transfers observable.
    let server = TestServer::start(
        vfs,
        ServerConfig::new()
            .chunk_delay(Duration::from_millis(2))
            .chunk_size(16 * 1024),
    )
    .await;

    let dir = temp_dir("parallel");
    let local = dir.join("big.bin");

    let summary = download(&server, "/pub/big.bin", &local, 4).await.unwrap();

    assert_eq!(
        std::fs::read(&local).unwrap(),
        content,
        "file must be byte identical"
    );
    assert_eq!(summary.bytes, content.len() as u64);

    let stats = server.stats();
    assert!(
        summary.segments > 1,
        "expected several segments, got {}",
        summary.segments
    );
    assert!(
        stats.max_concurrent_transfers() >= 2,
        "expected overlapping transfers, peak was {}",
        stats.max_concurrent_transfers()
    );
    assert!(
        stats.rest_offsets().iter().any(|offset| *offset > 0),
        "expected REST to be used for at least one segment, saw {:?}",
        stats.rest_offsets()
    );
    assert!(stats.retr_count() >= 2);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resumes_an_interrupted_download() {
    let mut vfs = Vfs::new();
    let content = pseudo_random(8 * 1024 * 1024, 13);
    vfs.add("/pub/resume.bin", content.clone());
    let server = TestServer::start(
        vfs,
        ServerConfig::new()
            .chunk_delay(Duration::from_millis(5))
            .chunk_size(16 * 1024),
    )
    .await;

    let dir = temp_dir("resume");
    let local = dir.join("resume.bin");

    let mut options = DownloadOptions::new("/pub/resume.bin", local.clone());
    options.policy.connections = 4;
    options.quiet = true;

    // Interrupt the first attempt part way through. The throttled server makes
    // the whole transfer take far longer than this, so the abort is reliable
    // rather than a race against completion.
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let first = {
        let options = options.clone();
        let connect = Arc::new(connect_options(&server));
        tokio::spawn(async move { download::download_file(connect, options, cancel_rx).await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    cancel_tx.send(true).unwrap();

    let interrupted = first.await.unwrap();
    assert!(
        matches!(interrupted, Err(Error::Aborted)),
        "expected an abort, got {interrupted:?}"
    );

    let store = StateStore::for_download(&local);
    assert!(
        store.exists(),
        "an interrupted download must leave a resume file"
    );

    // The destination is preallocated to its final size, so its length says
    // nothing about progress; the resume file is the record that matters.
    let state = store.load().expect("the resume file should be readable");
    assert_eq!(state.remote_size, content.len() as u64);
    let partial = ftpseg::plan::completed_bytes(&state.segments());
    assert!(
        partial > 0 && partial < content.len() as u64,
        "expected partial progress, recorded {partial} of {}",
        content.len()
    );
    assert_eq!(
        std::fs::metadata(&local).unwrap().len(),
        content.len() as u64,
        "the destination should already be preallocated"
    );

    // Resume: the second attempt must continue rather than start over.
    let rest_before = server.stats().rest_count();
    let summary = download(&server, "/pub/resume.bin", &local, 4)
        .await
        .unwrap();

    assert!(
        summary.resumed_from > 0,
        "resume should have reused existing bytes"
    );
    assert!(!store.exists(), "resume file should be removed on success");

    // Compare in a way that reports where the damage is, rather than dumping
    // eight million bytes into the test log.
    let actual = std::fs::read(&local).unwrap();
    assert_eq!(
        actual.len(),
        content.len(),
        "resumed file has the wrong length"
    );
    let wrong: Vec<usize> = actual
        .iter()
        .zip(content.iter())
        .enumerate()
        .filter(|(_, (got, want))| got != want)
        .map(|(offset, _)| offset)
        .collect();
    if !wrong.is_empty() {
        let first = wrong[0];
        let last = *wrong.last().unwrap();
        let planned = state.segments();
        let owner = planned
            .iter()
            .position(|s| first as u64 >= s.start && (first as u64) < s.start + s.len);
        panic!(
            "{} of {} bytes are wrong, first at {first}, last at {last};\n\
             first wrong byte lies in segment {owner:?} of {};\n\
             plan recorded after the interruption: {planned:?}",
            wrong.len(),
            content.len(),
            planned.len()
        );
    }

    assert!(
        server.stats().rest_count() > rest_before,
        "the resumed attempt should still use REST"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn falls_back_to_one_connection_without_rest() {
    let mut vfs = Vfs::new();
    let content = pseudo_random(2 * 1024 * 1024, 17);
    vfs.add("/pub/norest.bin", content.clone());
    let server = TestServer::start(vfs, ServerConfig::new().support_rest(false)).await;

    let dir = temp_dir("norest");
    let local = dir.join("norest.bin");

    let summary = download(&server, "/pub/norest.bin", &local, 4)
        .await
        .unwrap();

    assert_eq!(std::fs::read(&local).unwrap(), content);
    assert_eq!(summary.segments, 1);
    assert_eq!(
        server.stats().retr_count(),
        1,
        "a server without REST must only ever see one transfer"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discovers_the_size_from_a_listing_when_size_is_missing() {
    let mut vfs = Vfs::new();
    let content = pseudo_random(3 * 1024 * 1024, 19);
    vfs.add("/pub/nosize.bin", content.clone());
    let server = TestServer::start(vfs, ServerConfig::new().support_size(false)).await;

    let dir = temp_dir("nosize");
    let local = dir.join("nosize.bin");

    let summary = download(&server, "/pub/nosize.bin", &local, 4)
        .await
        .unwrap();

    assert_eq!(std::fs::read(&local).unwrap(), content);
    assert!(
        summary.segments > 1,
        "the size from MLSD should still allow segmentation"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uses_pasv_when_epsv_is_unavailable() {
    let mut vfs = Vfs::new();
    let content = pseudo_random(1024 * 1024, 23);
    vfs.add("/pub/pasv.bin", content.clone());
    let server = TestServer::start(vfs, ServerConfig::new().support_epsv(false)).await;

    let dir = temp_dir("pasv");
    let local = dir.join("pasv.bin");

    download(&server, "/pub/pasv.bin", &local, 2).await.unwrap();

    assert_eq!(std::fs::read(&local).unwrap(), content);
    assert!(
        server.stats().command_count("PASV") >= 1,
        "the client should have fallen back to PASV"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reports_a_missing_file() {
    let vfs = Vfs::new();
    let server = TestServer::start(vfs, ServerConfig::new()).await;

    let dir = temp_dir("missing");
    let local = dir.join("missing.bin");

    let error = download(&server, "/nope.bin", &local, 2).await.unwrap_err();
    assert!(
        matches!(error, Error::NotFound(_)),
        "expected NotFound, got {error:?}"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_completed_download_is_not_repeated() {
    let mut vfs = Vfs::new();
    let content = pseudo_random(512 * 1024, 29);
    vfs.add("/pub/once.bin", content.clone());
    let server = TestServer::start(vfs, ServerConfig::new()).await;

    let dir = temp_dir("once");
    let local = dir.join("once.bin");

    download(&server, "/pub/once.bin", &local, 2).await.unwrap();
    let transfers = server.stats().retr_count();

    let second = download(&server, "/pub/once.bin", &local, 2).await.unwrap();
    assert!(second.already_complete);
    assert_eq!(second.bytes, content.len() as u64);
    assert_eq!(
        server.stats().retr_count(),
        transfers,
        "nothing should have been transferred the second time"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

/// The reply an abandoned transfer produces varies between servers; all three
/// shapes must leave the connection reusable and the file intact.
async fn survives_abort_reply(abort_reply: AbortReply, tag: &str) {
    let mut vfs = Vfs::new();
    let content = pseudo_random(4 * 1024 * 1024, 31);
    vfs.add("/pub/abort.bin", content.clone());
    let server = TestServer::start(vfs, ServerConfig::new().abort_reply(abort_reply)).await;

    let dir = temp_dir(tag);
    let local = dir.join("abort.bin");

    let summary = download(&server, "/pub/abort.bin", &local, 4)
        .await
        .unwrap();

    assert_eq!(
        std::fs::read(&local).unwrap(),
        content,
        "file must survive the {abort_reply:?} abort sequence"
    );
    // Every segment except the last stops early, so the pool must have
    // recovered each connection rather than opening a new one every time.
    assert!(
        summary.connections_used <= 4,
        "connections should be reused, used {}",
        summary.connections_used
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handles_servers_that_answer_426_then_226() {
    survives_abort_reply(AbortReply::TwoReplies, "abort-two").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handles_servers_that_only_answer_426() {
    survives_abort_reply(AbortReply::Single426, "abort-single").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handles_servers_that_say_nothing_after_an_abort() {
    survives_abort_reply(AbortReply::Silent, "abort-silent").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn applies_the_bandwidth_limit() {
    let mut vfs = Vfs::new();
    let content = pseudo_random(2 * 1024 * 1024, 37);
    vfs.add("/pub/limited.bin", content.clone());
    let server = TestServer::start(vfs, ServerConfig::new()).await;

    let dir = temp_dir("limited");
    let local = dir.join("limited.bin");

    let options = {
        let mut options = DownloadOptions::new("/pub/limited.bin", local.clone());
        options.policy.connections = 2;
        options.limit = Some(4 * 1024 * 1024); // 4 MiB/s against a 2 MiB file
        options.quiet = true;
        options
    };

    let started = Instant::now();
    let (_tx, rx) = watch::channel(false);
    download::download_file(Arc::new(connect_options(&server)), options, rx)
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(std::fs::read(&local).unwrap(), content);
    assert!(
        elapsed >= Duration::from_millis(200),
        "a 4 MiB/s cap on 2 MiB should take a noticeable moment, took {elapsed:?}"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lists_a_directory_over_mlsd() {
    let mut vfs = Vfs::new();
    vfs.add("/pub/a.bin", vec![0u8; 10]);
    vfs.add("/pub/b.bin", vec![0u8; 2048]);
    vfs.add("/pub/nested/c.bin", vec![0u8; 5]);
    let server = TestServer::start(vfs, ServerConfig::new()).await;

    let mut client = FtpClient::connect(&connect_options(&server)).await.unwrap();
    let mut entries = client.list(Some("/pub")).await.unwrap();
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].name, "a.bin");
    assert_eq!(entries[0].size, Some(10));
    assert!(!entries[0].is_dir);
    assert_eq!(entries[1].name, "b.bin");
    assert_eq!(entries[1].size, Some(2048));
    assert!(entries[2].is_dir);
    assert_eq!(entries[2].name, "nested");

    client.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lists_a_directory_over_list_when_mlsd_is_missing() {
    let mut vfs = Vfs::new();
    vfs.add("/pub/a.bin", vec![0u8; 10]);
    vfs.add("/pub/nested/c.bin", vec![0u8; 5]);
    let server = TestServer::start(vfs, ServerConfig::new().support_mlsd(false)).await;

    let mut client = FtpClient::connect(&connect_options(&server)).await.unwrap();
    let mut entries = client.list(Some("/pub")).await.unwrap();
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "a.bin");
    assert_eq!(entries[0].size, Some(10));
    assert!(entries[1].is_dir);

    client.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reports_server_capabilities() {
    let mut vfs = Vfs::new();
    vfs.add("/pub/a.bin", vec![0u8; 4096]);
    let server = TestServer::start(vfs, ServerConfig::new()).await;

    let info = download::probe(&connect_options(&server), "/pub/a.bin")
        .await
        .unwrap();

    assert_eq!(info.size, Some(4096));
    assert_eq!(info.mdtm.as_deref(), Some("20240102030405"));
    assert!(info.rest_supported);
    assert!(!info.is_dir);
    assert!(info.features.rest_stream);
    assert!(info.features.mlsd);
    assert!(info.features.epsv);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_password_is_reported() {
    let mut vfs = Vfs::new();
    vfs.add("/pub/a.bin", vec![0u8; 10]);
    let server = TestServer::start(vfs, ServerConfig::new()).await;

    let mut options = connect_options(&server);
    options.password = "wrong".to_string();

    let error = FtpClient::connect(&options).await.unwrap_err();
    assert!(
        matches!(error, Error::Server { .. }),
        "expected a server rejection, got {error:?}"
    );
}

/// Run the real binary against the in-process server, so the argument parsing
/// and the download wiring are covered too.
async fn run_binary(args: Vec<std::ffi::OsString>) -> std::process::Output {
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_ftpseg"))
            .args(args)
            .output()
            .expect("the ftpseg binary should be runnable")
    })
    .await
    .expect("the helper task should not panic")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_binary_downloads_a_file_end_to_end() {
    let mut vfs = Vfs::new();
    let content = pseudo_random(3 * 1024 * 1024, 41);
    vfs.add("/pub/cli.bin", content.clone());
    let server = TestServer::start(vfs, ServerConfig::new()).await;

    let dir = temp_dir("cli");
    let local = dir.join("cli.bin");

    let output = run_binary(vec![
        "get".into(),
        server.url("/pub/cli.bin").into(),
        "-o".into(),
        local.clone().into(),
        "-n".into(),
        "4".into(),
        "--quiet".into(),
        // Fail fast rather than prompting, which would hang the test.
        "--no-prompt".into(),
    ])
    .await;

    assert!(
        output.status.success(),
        "ftpseg exited with {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(&local).unwrap(),
        content,
        "the binary must produce a byte identical file"
    );
    assert!(
        server.stats().retr_count() >= 2,
        "the binary should have used several segments, saw {}",
        server.stats().retr_count()
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_binary_dry_run_plans_without_transferring() {
    let mut vfs = Vfs::new();
    let content = pseudo_random(2 * 1024 * 1024, 43);
    vfs.add("/pub/plan.bin", content.clone());
    let server = TestServer::start(vfs, ServerConfig::new()).await;

    let dir = temp_dir("cli-dry");
    let local = dir.join("plan.bin");

    let output = run_binary(vec![
        "get".into(),
        server.url("/pub/plan.bin").into(),
        "-o".into(),
        local.clone().into(),
        "-n".into(),
        "4".into(),
        "--dry-run".into(),
        "--no-prompt".into(),
    ])
    .await;

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "dry run failed: {stdout}");
    assert!(
        stdout.contains("REST support: yes"),
        "the plan should report REST support, got: {stdout}"
    );
    assert!(
        stdout.contains("segments"),
        "the plan should describe the segmentation, got: {stdout}"
    );
    assert!(!local.exists(), "a dry run must not create the file");
    assert_eq!(
        server.stats().retr_count(),
        0,
        "a dry run must not transfer anything"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_binary_lists_and_describes_the_server() {
    let mut vfs = Vfs::new();
    vfs.add("/pub/a.bin", vec![7u8; 4096]);
    let server = TestServer::start(vfs, ServerConfig::new()).await;

    let listing = run_binary(vec![
        "ls".into(),
        server.url("/pub").into(),
        "--no-prompt".into(),
    ])
    .await;
    assert!(
        listing.status.success(),
        "ls failed: {}",
        String::from_utf8_lossy(&listing.stderr)
    );
    assert!(
        String::from_utf8_lossy(&listing.stdout).contains("a.bin"),
        "the listing should contain the file"
    );

    let stat = run_binary(vec![
        "stat".into(),
        server.url("/pub/a.bin").into(),
        "--no-prompt".into(),
    ])
    .await;
    let stdout = String::from_utf8_lossy(&stat.stdout);
    assert!(stat.status.success(), "stat failed: {stdout}");
    assert!(
        stdout.contains("4096"),
        "stat should report the size: {stdout}"
    );
    assert!(
        stdout.contains("REST STREAM : yes"),
        "stat should report REST support: {stdout}"
    );
    assert!(
        stdout.contains("charset") && stdout.contains("UTF-8"),
        "stat should report the character set in use: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// Character sets
//
// These exercise a server that speaks something other than UTF-8, which is what
// a lot of Chinese, Taiwanese and Japanese FTP hosts still do.
// ---------------------------------------------------------------------------

/// Download with an explicit character set request.
async fn download_with_charset(
    server: &TestServer,
    options: DownloadOptions,
    charset: CharsetRequest,
) -> Result<DownloadSummary, Error> {
    let mut connect = connect_options(server);
    connect.charset = charset;
    let (_tx, rx) = watch::channel(false);
    download::download_file(Arc::new(connect), options, rx).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_a_chinese_name_from_a_legacy_gbk_server() {
    const NAME: &str = "中文数据文件.bin";
    let mut vfs = Vfs::new();
    let content = pseudo_random(3 * 1024 * 1024, 51);
    vfs.add(&format!("/pub/{NAME}"), content.clone());
    let server = TestServer::start(vfs, ServerConfig::new().legacy_charset("gbk")).await;

    let dir = temp_dir("charset-gbk-download");
    let local = dir.join("out.bin");

    let mut options = DownloadOptions::new(format!("/pub/{NAME}"), local.clone());
    options.policy.connections = 4;
    options.quiet = true;

    let summary = download_with_charset(&server, options, CharsetRequest::Named("gbk".to_string()))
        .await
        .unwrap();

    assert_eq!(std::fs::read(&local).unwrap(), content, "bytes must match");
    assert!(
        summary.segments > 1,
        "a GBK name should still segment, got {} segment(s)",
        summary.segments
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_uses_utf8_for_a_server_that_announces_it() {
    const NAME: &str = "中文.bin";
    let mut vfs = Vfs::new();
    let content = pseudo_random(512 * 1024, 53);
    vfs.add(&format!("/pub/{NAME}"), content.clone());
    // Default config advertises UTF8 in FEAT.
    let server = TestServer::start(vfs, ServerConfig::new()).await;

    let dir = temp_dir("charset-auto-utf8");
    let local = dir.join("out.bin");
    let mut options = DownloadOptions::new(format!("/pub/{NAME}"), local.clone());
    options.quiet = true;

    let summary = download_with_charset(&server, options, CharsetRequest::Auto)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&local).unwrap(), content);
    assert_eq!(summary.bytes, content.len() as u64);

    // A server that advertises UTF8 is also told we will use it.
    assert!(
        server.stats().command_count("OPTS") >= 1,
        "the client should send OPTS UTF8 ON, saw {:?}",
        server.stats().commands()
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_falls_back_to_the_local_code_page_for_a_legacy_server() {
    // Whatever this machine's code page resolves to, the fallback path must
    // work end to end. On a Chinese Windows that is GBK, which is also the
    // case worth testing with a non-ASCII name.
    let fallback = CharsetRequest::Auto.resolve(false).unwrap();
    let name = if fallback.encode("中文").is_ok() {
        "中文文件.bin"
    } else {
        "plain-name.bin"
    };

    let mut vfs = Vfs::new();
    let content = pseudo_random(256 * 1024, 59);
    vfs.add(&format!("/pub/{name}"), content.clone());
    let server = TestServer::start(vfs, ServerConfig::new().legacy_charset(fallback.name())).await;

    let dir = temp_dir("charset-auto-legacy");
    let local = dir.join("out.bin");
    let mut options = DownloadOptions::new(format!("/pub/{name}"), local.clone());
    options.quiet = true;

    download_with_charset(&server, options, CharsetRequest::Auto)
        .await
        .unwrap();

    assert_eq!(std::fs::read(&local).unwrap(), content);
    assert!(
        server.stats().command_count("OPTS") == 0,
        "a server without UTF8 must not be sent OPTS UTF8 ON"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listing_decodes_chinese_names_from_a_gbk_server() {
    const NAME: &str = "中文文件.bin";
    let mut vfs = Vfs::new();
    vfs.add(&format!("/pub/{NAME}"), vec![7u8; 1234]);
    vfs.add("/pub/ascii.bin", vec![1u8; 10]);
    let server = TestServer::start(vfs, ServerConfig::new().legacy_charset("gbk")).await;

    let mut connect = connect_options(&server);
    connect.charset = CharsetRequest::Named("gbk".to_string());
    let mut client = FtpClient::connect(&connect).await.unwrap();

    let mut entries = client.list(Some("/pub")).await.unwrap();
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "ascii.bin");
    assert_eq!(
        entries[1].name, NAME,
        "a GBK listing must decode into readable text, not replacement characters"
    );
    assert_eq!(entries[1].size, Some(1234));

    client.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chinese_credentials_are_sent_in_the_server_charset() {
    let mut vfs = Vfs::new();
    let content = pseudo_random(64 * 1024, 61);
    vfs.add("/pub/secret.bin", content.clone());

    // A GBK server with non-ASCII credentials.
    let config = ServerConfig::new()
        .legacy_charset("gbk")
        .with_credentials("用户", "密码");
    let server = TestServer::start(vfs, config).await;

    let dir = temp_dir("charset-credentials");
    let local = dir.join("secret.bin");

    // Correct charset: the login is understood.
    let mut connect = connect_options(&server);
    connect.user = "用户".to_string();
    connect.password = "密码".to_string();
    connect.charset = CharsetRequest::Named("gbk".to_string());
    let mut client = FtpClient::connect(&connect)
        .await
        .expect("GBK-encoded credentials should be accepted");
    assert_eq!(client.charset().name(), "GBK");
    client.quit().await;

    // Wrong charset: the same credentials come out as mojibake, and the server
    // rejects them. Proof that the character set really is applied to login.
    let mut wrong = connect_options(&server);
    wrong.user = "用户".to_string();
    wrong.password = "密码".to_string();
    wrong.charset = CharsetRequest::Named("utf-8".to_string());
    let error = FtpClient::connect(&wrong)
        .await
        .expect_err("UTF-8 credentials must not satisfy a GBK server");
    assert!(
        matches!(error, Error::Server { code: 530, .. }),
        "expected a 530 rejection, got {error:?}"
    );

    // And the download path works with those credentials.
    let mut options = DownloadOptions::new("/pub/secret.bin", local.clone());
    options.quiet = true;
    let mut ok = connect_options(&server);
    ok.user = "用户".to_string();
    ok.password = "密码".to_string();
    ok.charset = CharsetRequest::Named("gbk".to_string());
    let (_tx, rx) = watch::channel(false);
    download::download_file(Arc::new(ok), options, rx)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&local).unwrap(), content);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_name_the_server_charset_cannot_carry_is_refused() {
    let mut vfs = Vfs::new();
    vfs.add("/pub/ascii.bin", vec![0u8; 16]);
    // latin-1 / windows-1252 has no Chinese characters at all.
    let server = TestServer::start(vfs, ServerConfig::new().legacy_charset("latin-1")).await;

    let dir = temp_dir("charset-unrepresentable");
    let local = dir.join("out.bin");
    let mut options = DownloadOptions::new("/pub/中文.bin", local.clone());
    options.quiet = true;

    let error = download_with_charset(
        &server,
        options,
        CharsetRequest::Named("latin-1".to_string()),
    )
    .await
    .expect_err("a name the server cannot express must be refused");

    match error {
        Error::Unrepresentable { character, charset } => {
            assert_eq!(character, '中');
            assert!(charset.contains("1252"), "got {charset}");
        }
        other => panic!("expected an Unrepresentable error, got {other:?}"),
    }
    assert!(!local.exists(), "nothing should have been downloaded");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_binary_honours_an_explicit_encoding() {
    const NAME: &str = "中文文件.bin";
    let mut vfs = Vfs::new();
    let content = pseudo_random(1024 * 1024, 67);
    vfs.add(&format!("/pub/{NAME}"), content.clone());
    let server = TestServer::start(vfs, ServerConfig::new().legacy_charset("gbk")).await;

    let dir = temp_dir("charset-cli");
    let local = dir.join("out.bin");
    // The URL carries the name as UTF-8; --encoding converts it for the wire.
    let url = server.url(&format!("/pub/{NAME}"));

    let output = run_binary(vec![
        "get".into(),
        url.into(),
        "-o".into(),
        local.clone().into(),
        "--encoding".into(),
        "gbk".into(),
        "--quiet".into(),
        "--no-prompt".into(),
    ])
    .await;

    assert!(
        output.status.success(),
        "ftpseg failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(&local).unwrap(), content);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_encoding_fails_before_connecting() {
    let output = run_binary(vec![
        "get".into(),
        "ftp://127.0.0.1:1/x.bin".into(),
        "--encoding".into(),
        "klingon".into(),
        "--no-prompt".into(),
    ])
    .await;

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown character set"),
        "expected a clear complaint about the character set, got: {stderr}"
    );
    assert!(
        !stderr.contains("I/O error"),
        "the character set should be rejected before any connection attempt: {stderr}"
    );
}
