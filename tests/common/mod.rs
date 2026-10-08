//! A small in-process FTP server used by the integration tests.
//!
//! It is deliberately strict about the parts the client relies on: `SIZE`,
//! `FEAT`, `REST`, `EPSV`/`PASV` and the reply sequence after an aborted
//! transfer. Everything is served from memory so tests can assert on exact
//! bytes.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use ftpseg::encoding::{Charset, CharsetRequest};

/// How the server reacts when the client closes a data connection early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortReply {
    /// `426` followed by `226`, like vsftpd. Exercises the client's resync.
    TwoReplies,
    /// Just `426`.
    Single426,
    /// Say nothing at all about the aborted transfer.
    Silent,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub user: String,
    pub password: String,
    pub support_feat: bool,
    pub support_size: bool,
    pub support_rest: bool,
    pub support_mlsd: bool,
    pub support_epsv: bool,
    pub support_mdtm: bool,
    /// Artificial pause between chunks, used to make concurrency observable.
    pub chunk_delay: Duration,
    pub chunk_size: usize,
    pub abort_reply: AbortReply,
    /// Reject the connection outright, for error-path tests.
    pub refuse_login: bool,
    /// Character set the server speaks for command arguments and listings.
    pub charset: Charset,
    /// Whether `FEAT` advertises `UTF8`.
    pub advertise_utf8: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            user: "testuser".to_string(),
            password: "testpass".to_string(),
            support_feat: true,
            support_size: true,
            support_rest: true,
            support_mlsd: true,
            support_epsv: true,
            support_mdtm: true,
            chunk_delay: Duration::ZERO,
            chunk_size: 32 * 1024,
            abort_reply: AbortReply::TwoReplies,
            refuse_login: false,
            charset: Charset::utf8(),
            advertise_utf8: true,
        }
    }
}

impl ServerConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn chunk_delay(mut self, delay: Duration) -> Self {
        self.chunk_delay = delay;
        self
    }

    /// Bytes written per socket write, which controls how finely a transfer
    /// can be interleaved with another one.
    pub fn chunk_size(mut self, size: usize) -> Self {
        self.chunk_size = size.max(1);
        self
    }

    pub fn support_rest(mut self, value: bool) -> Self {
        self.support_rest = value;
        self
    }

    /// Set the accepted login, including non-ASCII names, which only work if
    /// the client converts them into the server's character set.
    pub fn with_credentials(mut self, user: &str, password: &str) -> Self {
        self.user = user.to_string();
        self.password = password.to_string();
        self
    }

    pub fn support_size(mut self, value: bool) -> Self {
        self.support_size = value;
        self
    }

    pub fn support_epsv(mut self, value: bool) -> Self {
        self.support_epsv = value;
        self
    }

    pub fn support_mlsd(mut self, value: bool) -> Self {
        self.support_mlsd = value;
        self
    }

    pub fn abort_reply(mut self, value: AbortReply) -> Self {
        self.abort_reply = value;
        self
    }

    /// Make this server speak a legacy character set, like a real pre-Unicode
    /// host: argument bytes and listing output use it, and `FEAT` stays silent
    /// about UTF-8.
    pub fn legacy_charset(mut self, label: &str) -> Self {
        self.charset = CharsetRequest::Named(label.to_string())
            .resolve(false)
            .expect("tests only name known character sets");
        self.advertise_utf8 = false;
        self
    }

    /// Advertise UTF-8 support in `FEAT`.
    pub fn advertise_utf8(mut self, value: bool) -> Self {
        self.advertise_utf8 = value;
        self
    }
}

/// Counters the tests assert against.
#[derive(Debug, Default)]
pub struct Stats {
    logins: AtomicUsize,
    retr_count: AtomicUsize,
    rest_count: AtomicUsize,
    live_transfers: AtomicUsize,
    max_concurrent_transfers: AtomicUsize,
    rest_offsets: Mutex<Vec<u64>>,
    commands: Mutex<Vec<String>>,
}

impl Stats {
    pub fn logins(&self) -> usize {
        self.logins.load(Ordering::SeqCst)
    }

    pub fn retr_count(&self) -> usize {
        self.retr_count.load(Ordering::SeqCst)
    }

    pub fn rest_count(&self) -> usize {
        self.rest_count.load(Ordering::SeqCst)
    }

    pub fn max_concurrent_transfers(&self) -> usize {
        self.max_concurrent_transfers.load(Ordering::SeqCst)
    }

    /// Every `REST` offset the server saw, in order.
    pub fn rest_offsets(&self) -> Vec<u64> {
        self.rest_offsets
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    pub fn commands(&self) -> Vec<String> {
        self.commands.lock().map(|v| v.clone()).unwrap_or_default()
    }

    pub fn command_count(&self, name: &str) -> usize {
        self.commands()
            .iter()
            .filter(|c| c.eq_ignore_ascii_case(name))
            .count()
    }

    fn record(&self, command: &str) {
        if let Ok(mut commands) = self.commands.lock() {
            commands.push(command.to_string());
        }
    }

    fn enter_transfer(&self) -> usize {
        let live = self.live_transfers.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_concurrent_transfers
            .fetch_max(live, Ordering::SeqCst);
        live
    }

    fn leave_transfer(&self) {
        self.live_transfers.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A flat virtual filesystem: absolute path -> contents.
#[derive(Debug, Default, Clone)]
pub struct Vfs {
    files: BTreeMap<String, Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
}

impl Vfs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, path: &str, contents: Vec<u8>) -> &mut Self {
        self.files.insert(normalize(path), contents);
        self
    }

    pub fn get(&self, path: &str) -> Option<&Vec<u8>> {
        self.files.get(&normalize(path))
    }

    fn is_dir(&self, path: &str) -> bool {
        let dir = normalize(path);
        if dir == "/" {
            return true;
        }
        let prefix = format!("{dir}/");
        self.files.keys().any(|key| key.starts_with(&prefix))
    }

    fn children(&self, dir: &str) -> Vec<Entry> {
        let dir = normalize(dir);
        let prefix = if dir == "/" {
            "/".to_string()
        } else {
            format!("{dir}/")
        };

        let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
        for (path, contents) in &self.files {
            let Some(rest) = path.strip_prefix(&prefix) else {
                continue;
            };
            if rest.is_empty() {
                continue;
            }
            match rest.split_once('/') {
                Some((name, _)) => {
                    entries.entry(name.to_string()).or_insert_with(|| Entry {
                        name: name.to_string(),
                        is_dir: true,
                        size: None,
                    });
                }
                None => {
                    entries.insert(
                        rest.to_string(),
                        Entry {
                            name: rest.to_string(),
                            is_dir: false,
                            size: Some(contents.len() as u64),
                        },
                    );
                }
            }
        }
        entries.into_values().collect()
    }

    fn mlsd(&self, dir: &str) -> String {
        let mut out = String::new();
        for entry in self.children(dir) {
            if entry.is_dir {
                out.push_str("type=dir;modify=20240102030405; ");
            } else {
                out.push_str(&format!(
                    "type=file;size={};modify=20240102030405; ",
                    entry.size.unwrap_or(0)
                ));
            }
            out.push_str(&entry.name);
            out.push_str("\r\n");
        }
        out
    }

    fn list(&self, dir: &str) -> String {
        let mut out = String::new();
        for entry in self.children(dir) {
            if entry.is_dir {
                out.push_str(&format!(
                    "drwxr-xr-x 2 owner group 4096 Jan 02 03:04 {}\r\n",
                    entry.name
                ));
            } else {
                out.push_str(&format!(
                    "-rw-r--r-- 1 owner group {} Jan 02 03:04 {}\r\n",
                    entry.size.unwrap_or(0),
                    entry.name
                ));
            }
        }
        out
    }
}

fn normalize(path: &str) -> String {
    let mut path = path.trim().to_string();
    if path.is_empty() {
        return "/".to_string();
    }
    if !path.starts_with('/') {
        path.insert(0, '/');
    }
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    path
}

struct ServerState {
    vfs: Vfs,
    config: ServerConfig,
    stats: Arc<Stats>,
}

/// A running test server. Dropping it stops the accept loop.
pub struct TestServer {
    addr: SocketAddr,
    stats: Arc<Stats>,
    handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    /// Start a server on an ephemeral loopback port.
    pub async fn start(vfs: Vfs, config: ServerConfig) -> TestServer {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        let stats = Arc::new(Stats::default());
        let state = Arc::new(ServerState {
            vfs,
            config,
            stats: Arc::clone(&stats),
        });

        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    let _ = handle_connection(stream, state).await;
                });
            }
        });

        TestServer {
            addr,
            stats,
            handle,
        }
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn host(&self) -> String {
        self.addr.ip().to_string()
    }

    pub fn url(&self, path: &str) -> String {
        format!(
            "ftp://{}:{}@{}:{}{}",
            "testuser",
            "testpass",
            self.host(),
            self.port(),
            path
        )
    }

    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

struct Session {
    user: Option<String>,
    authed: bool,
    rest: u64,
    cwd: String,
    pending_data: Option<TcpListener>,
}

type Writer = tokio::net::tcp::OwnedWriteHalf;

async fn reply(writer: &mut Writer, line: &str) -> std::io::Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\r\n").await?;
    writer.flush().await
}

async fn handle_connection(stream: TcpStream, state: Arc<ServerState>) -> std::io::Result<()> {
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    if state.config.refuse_login {
        reply(&mut writer, "421 Service not available").await?;
        return Ok(());
    }

    reply(&mut writer, "220 ftpseg test server ready").await?;

    let mut session = Session {
        user: None,
        authed: false,
        rest: 0,
        cwd: "/".to_string(),
        pending_data: None,
    };

    // Commands are read as raw bytes, not as text: a legacy server receives
    // names in its own character set, and those bytes are frequently not valid
    // UTF-8 at all.
    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line).await?;
        if read == 0 {
            return Ok(());
        }
        while matches!(line.last(), Some(b'\n') | Some(b'\r')) {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }

        // The verb is always ASCII; everything after the first space is an
        // argument in the server's character set.
        let (command, argument) = match line.iter().position(|byte| *byte == b' ') {
            Some(index) => (
                String::from_utf8_lossy(&line[..index]).to_ascii_uppercase(),
                state.config.charset.decode(&line[index + 1..]),
            ),
            None => (
                String::from_utf8_lossy(&line).to_ascii_uppercase(),
                String::new(),
            ),
        };
        state.stats.record(&command);

        match command.as_str() {
            "USER" => {
                if argument == state.config.user {
                    session.user = Some(argument);
                    reply(&mut writer, "331 Please specify the password").await?;
                } else {
                    reply(&mut writer, "530 Unknown user").await?;
                }
            }
            "PASS" => {
                if session.user.is_some() && argument == state.config.password {
                    session.authed = true;
                    state.stats.logins.fetch_add(1, Ordering::SeqCst);
                    reply(&mut writer, "230 Login successful").await?;
                } else {
                    reply(&mut writer, "530 Login incorrect").await?;
                }
            }
            "SYST" => reply(&mut writer, "215 UNIX Type: L8").await?,
            "FEAT" => {
                if !state.config.support_feat {
                    reply(&mut writer, "500 Unknown command").await?;
                } else {
                    writer.write_all(b"211-Features:\r\n").await?;
                    if state.config.support_size {
                        writer.write_all(b" SIZE\r\n").await?;
                    }
                    if state.config.support_mdtm {
                        writer.write_all(b" MDTM\r\n").await?;
                    }
                    if state.config.support_mlsd {
                        writer.write_all(b" MLSD\r\n").await?;
                    }
                    if state.config.advertise_utf8 {
                        writer.write_all(b" UTF8\r\n").await?;
                    }
                    if state.config.support_rest {
                        writer.write_all(b" REST STREAM\r\n").await?;
                    }
                    if state.config.support_epsv {
                        writer.write_all(b" EPSV\r\n").await?;
                    }
                    writer.write_all(b"211 End\r\n").await?;
                    writer.flush().await?;
                }
            }
            "OPTS" => reply(&mut writer, "200 OPTS ok").await?,
            "PBSZ" => reply(&mut writer, "200 PBSZ=0").await?,
            "PROT" => reply(&mut writer, "200 Protection level set").await?,
            "TYPE" => reply(&mut writer, "200 Type set").await?,
            "NOOP" => reply(&mut writer, "200 NOOP ok").await?,
            "QUIT" => {
                reply(&mut writer, "221 Goodbye").await?;
                return Ok(());
            }
            "PWD" => reply(&mut writer, "257 \"/\" is the current directory").await?,
            "CWD" => {
                session.cwd = normalize(&argument);
                reply(&mut writer, "250 Directory changed").await?;
            }
            "SIZE" => {
                if !state.config.support_size {
                    reply(&mut writer, "502 SIZE not implemented").await?;
                } else if let Some(contents) = state.vfs.get(&argument) {
                    reply(&mut writer, &format!("213 {}", contents.len())).await?;
                } else {
                    reply(&mut writer, "550 Could not get file size").await?;
                }
            }
            "MDTM" => {
                if !state.config.support_mdtm {
                    reply(&mut writer, "502 MDTM not implemented").await?;
                } else if state.vfs.get(&argument).is_some() {
                    reply(&mut writer, "213 20240102030405").await?;
                } else {
                    reply(&mut writer, "550 No such file").await?;
                }
            }
            "REST" => {
                if !state.config.support_rest {
                    reply(&mut writer, "500 REST not understood").await?;
                } else {
                    match argument.parse::<u64>() {
                        Ok(offset) => {
                            session.rest = offset;
                            state.stats.rest_count.fetch_add(1, Ordering::SeqCst);
                            if let Ok(mut offsets) = state.stats.rest_offsets.lock() {
                                offsets.push(offset);
                            }
                            reply(&mut writer, &format!("350 Restarting at {offset}")).await?;
                        }
                        Err(_) => reply(&mut writer, "501 Bad offset").await?,
                    }
                }
            }
            "EPSV" => {
                if !state.config.support_epsv {
                    reply(&mut writer, "500 EPSV not understood").await?;
                } else {
                    match TcpListener::bind(("127.0.0.1", 0)).await {
                        Ok(listener) => {
                            let port = listener.local_addr()?.port();
                            session.pending_data = Some(listener);
                            reply(
                                &mut writer,
                                &format!("229 Entering Extended Passive Mode (|||{port}|)"),
                            )
                            .await?;
                        }
                        Err(_) => reply(&mut writer, "425 Cannot open data connection").await?,
                    }
                }
            }
            "PASV" => match TcpListener::bind(("127.0.0.1", 0)).await {
                Ok(listener) => {
                    let port = listener.local_addr()?.port();
                    session.pending_data = Some(listener);
                    let p1 = port / 256;
                    let p2 = port % 256;
                    reply(
                        &mut writer,
                        &format!("227 Entering Passive Mode (127,0,0,1,{p1},{p2})"),
                    )
                    .await?;
                }
                Err(_) => reply(&mut writer, "425 Cannot open data connection").await?,
            },
            "RETR" => {
                let Some(contents) = state.vfs.get(&argument).cloned() else {
                    if state.vfs.is_dir(&argument) {
                        reply(&mut writer, "550 Is a directory").await?;
                    } else {
                        reply(&mut writer, "550 Failed to open file").await?;
                    }
                    continue;
                };
                let Some(listener) = session.pending_data.take() else {
                    reply(&mut writer, "425 Use PASV or EPSV first").await?;
                    continue;
                };
                state.stats.retr_count.fetch_add(1, Ordering::SeqCst);
                let offset = session.rest;
                session.rest = 0;
                serve_transfer(&state, &mut writer, listener, contents, offset, true).await?;
            }
            "MLSD" | "LIST" => {
                let is_mlsd = command == "MLSD";
                if is_mlsd && !state.config.support_mlsd {
                    reply(&mut writer, "500 MLSD not understood").await?;
                    continue;
                }
                let Some(listener) = session.pending_data.take() else {
                    reply(&mut writer, "425 Use PASV or EPSV first").await?;
                    continue;
                };
                let target = if argument.is_empty() {
                    session.cwd.clone()
                } else {
                    normalize(&argument)
                };
                let body = if is_mlsd {
                    state.vfs.mlsd(&target)
                } else {
                    state.vfs.list(&target)
                };
                // Listings go out in the server's character set, exactly as a
                // pre-Unicode host would send them.
                let body = state
                    .config
                    .charset
                    .encode(&body)
                    .expect("test file names fit the configured character set");
                serve_transfer(&state, &mut writer, listener, body, 0, false).await?;
            }
            _ => reply(&mut writer, "500 Unknown command").await?,
        }
    }
}

async fn serve_transfer(
    state: &Arc<ServerState>,
    writer: &mut Writer,
    listener: TcpListener,
    body: Vec<u8>,
    offset: u64,
    is_retr: bool,
) -> std::io::Result<()> {
    let accepted = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await;
    let Ok(Ok((mut data, _))) = accepted else {
        reply(writer, "425 No data connection").await?;
        return Ok(());
    };

    reply(writer, "150 Opening data connection").await?;

    let start = (offset as usize).min(body.len());
    let payload = &body[start..];
    let chunk_size = state.config.chunk_size.max(1);
    let mut aborted = false;

    state.stats.enter_transfer();
    for chunk in payload.chunks(chunk_size) {
        if data.write_all(chunk).await.is_err() {
            // The client read exactly as many bytes as it wanted and hung up.
            aborted = true;
            break;
        }
        if !state.config.chunk_delay.is_zero() {
            tokio::time::sleep(state.config.chunk_delay).await;
        }
    }
    state.stats.leave_transfer();

    if !aborted {
        let _ = data.shutdown().await;
    }
    drop(data);

    if aborted && is_retr {
        match state.config.abort_reply {
            AbortReply::TwoReplies => {
                reply(writer, "426 Failure writing network stream").await?;
                reply(writer, "226 Abort successful").await?;
            }
            AbortReply::Single426 => {
                reply(writer, "426 Transfer aborted").await?;
            }
            AbortReply::Silent => {}
        }
    } else {
        reply(writer, "226 Transfer complete").await?;
    }
    Ok(())
}

/// Deterministic pseudo-random bytes, so a corrupted segment cannot pass a
/// comparison by accident.
pub fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut x = seed | 1;
    for _ in 0..len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push((x & 0xff) as u8);
    }
    out
}

/// A unique temporary directory for one test.
pub fn temp_dir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::AtomicU32;
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("ftpseg-it-{tag}-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}
