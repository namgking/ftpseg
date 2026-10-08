//! Local-only browser API. Saved credentials are reversibly obfuscated, not
//! securely encrypted; anyone with config access can recover them.
use crate::{
    client::{ConnectOptions, FtpClient, TlsMode},
    config::DownloadOptions,
    download,
    encoding::CharsetRequest,
    error::{Error, Result},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{watch, Notify},
};

const MAX_HEADERS: usize = 16 * 1024;
const MAX_BODY: usize = 1024 * 1024;
const HTML: &str = include_str!("gui.html");

#[derive(Clone, Serialize, Deserialize)]
struct Site {
    id: u64,
    name: String,
    host: String,
    port: u16,
    #[serde(default, skip_serializing)]
    user: String,
    #[serde(default, skip_serializing)]
    password: String,
    #[serde(default)]
    credentials: String,
    tls: String,
    encoding: String,
    insecure: bool,
}
impl Site {
    fn public(&self) -> Value {
        json!({"id":self.id,"name":self.name,"host":self.host,"port":self.port,"user":self.user,"tls":self.tls,"encoding":self.encoding,"insecure":self.insecure,"has_password":!self.password.is_empty()})
    }
    fn connection(&self) -> ConnectOptions {
        ConnectOptions {
            host: self.host.clone(),
            port: self.port,
            user: self.user.clone(),
            password: if self.password.is_empty() && self.user.eq_ignore_ascii_case("anonymous") {
                "anonymous@".into()
            } else {
                self.password.clone()
            },
            tls: match self.tls.as_str() {
                "explicit" => TlsMode::Explicit,
                "implicit" => TlsMode::Implicit,
                _ => TlsMode::None,
            },
            accept_invalid_certs: self.insecure,
            connect_timeout: Duration::from_secs(15),
            timeout: Duration::from_secs(30),
            verbose: false,
            charset: if self.encoding.is_empty() || self.encoding.eq_ignore_ascii_case("auto") {
                CharsetRequest::Auto
            } else {
                CharsetRequest::Named(self.encoding.clone())
            },
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Settings {
    queue_concurrency: usize,
    connections: usize,
    download_dir: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            queue_concurrency: 5,
            connections: 5,
            download_dir: std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("downloads")
                .to_string_lossy()
                .into_owned(),
        }
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Stored {
    sites: Vec<Site>,
    settings: Settings,
}
#[derive(Clone, Serialize)]
struct Job {
    id: u64,
    site_id: u64,
    remote: String,
    /// Absolute, frozen at enqueue time (including across retries/settings changes).
    local: PathBuf,
    status: String,
    downloaded: u64,
    total: Option<u64>,
    error: Option<String>,
}
struct RuntimeJob {
    job: Job,
    cancel: Option<watch::Sender<bool>>,
}
struct State {
    stored: Stored,
    jobs: Vec<RuntimeJob>,
    next_id: u64,
    running: usize,
    scanning: usize,
}
struct ScanGuard(Arc<App>);
impl Drop for ScanGuard {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.scanning = state.scanning.saturating_sub(1);
    }
}
struct App {
    state: Mutex<State>,
    file: PathBuf,
    wake: Notify,
    shutdown: Notify,
    /// Runtime-only identity: browser caches must not send old job IDs after restart.
    instance_id: String,
}

fn config_file() -> Result<PathBuf> {
    // An explicit location is authoritative: never silently save credentials elsewhere.
    if let Some(path) = std::env::var_os("FTPSEG_GUI_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    let default = base.map(|base| base.join("ftpseg").join("gui.json"));
    // Prefer a portable config next to the running executable, not the shell's
    // working directory. Keep old configs discoverable to avoid losing sites on
    // upgrade, and never overwrite one with a fresh empty config.
    let executable = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(".ftpseg").join("gui.json")));
    let working = std::env::current_dir()
        .ok()
        .map(|dir| dir.join(".ftpseg").join("gui.json"));
    choose_config_file(executable, default, working)
}

fn choose_config_file(
    executable: Option<PathBuf>,
    legacy: Option<PathBuf>,
    working: Option<PathBuf>,
) -> Result<PathBuf> {
    choose_config_file_with(executable, legacy, working, check_config_writable)
}

fn choose_config_file_with(
    executable: Option<PathBuf>,
    legacy: Option<PathBuf>,
    working: Option<PathBuf>,
    writable: impl Fn(&Path) -> std::io::Result<()>,
) -> Result<PathBuf> {
    // An existing app-local config wins. If a prior version wrote to APPDATA,
    // keep reading it rather than silently dropping credentials/site settings.
    if let Some(file) = executable.as_ref().filter(|file| file.is_file()) {
        return Ok(file.clone());
    }
    if let Some(file) = legacy.as_ref().filter(|file| file.is_file()) {
        return Ok(file.clone());
    }
    if let Some(file) = working.as_ref().filter(|file| file.is_file()) {
        return Ok(file.clone());
    }
    let mut last_error = None;
    for file in [executable, working, legacy].into_iter().flatten() {
        if let Err(error) = writable(&file) {
            eprintln!(
                "GUI config path {} is not writable: {error}",
                file.display()
            );
            last_error = Some(format!("{}: {error}", file.display()));
            continue;
        }
        return Ok(file);
    }
    Err(Error::Config(format!(
        "no writable GUI config directory; set FTPSEG_GUI_CONFIG to a writable path ({})",
        last_error.unwrap_or_else(|| "no candidate directories".into())
    )))
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[tokio::test]
    async fn pausing_keeps_activity_marker_and_cancel_takes_precedence() {
        let (tx, _rx) = watch::channel(false);
        let app = Arc::new(App {
            state: Mutex::new(State {
                stored: Stored::default(),
                jobs: vec![RuntimeJob {
                    job: Job {
                        id: 1,
                        site_id: 1,
                        remote: "/file.bin".into(),
                        local: PathBuf::from("unused-file.bin"),
                        status: "running".into(),
                        downloaded: 123,
                        total: Some(456),
                        error: Some("previous error".into()),
                    },
                    cancel: Some(tx),
                }],
                next_id: 2,
                running: 1,
                scanning: 0,
            }),
            file: PathBuf::from("unused-config.json"),
            wake: Notify::new(),
            shutdown: Notify::new(),
            instance_id: "test-instance".into(),
        });
        let body = json!({"id":1});
        assert_eq!(
            api(app.clone(), "POST", "/api/jobs/pause", body.clone())
                .await
                .0,
            200
        );
        {
            let state = app.state.lock().unwrap();
            let job = &state.jobs[0];
            assert_eq!(job.job.status, "pausing");
            assert_eq!(job.job.downloaded, 123);
            assert_eq!(job.job.error.as_deref(), Some("previous error"));
            assert!(*job.cancel.as_ref().unwrap().borrow());
            assert_eq!(state.running, 1);
        }
        assert_eq!(
            api(app.clone(), "POST", "/api/shutdown", json!({})).await.0,
            409
        );
        assert_eq!(
            api(app.clone(), "POST", "/api/jobs/resume", body.clone())
                .await
                .0,
            400
        );
        assert_eq!(
            api(app.clone(), "POST", "/api/jobs/retry", body.clone())
                .await
                .0,
            400
        );
        assert_eq!(
            api(app.clone(), "POST", "/api/jobs/cancel", body.clone())
                .await
                .0,
            200
        );
        assert_eq!(app.state.lock().unwrap().jobs[0].job.status, "cancelling");
        assert_eq!(
            api(app.clone(), "POST", "/api/jobs/pause", body).await.0,
            400
        );
        let state = app.state.lock().unwrap();
        assert_eq!(state.jobs[0].job.status, "cancelling");
        assert!(state.jobs[0].cancel.is_some());
        assert_eq!(state.running, 1);
    }

    #[test]
    fn rejects_unsafe_server_entry_names() {
        for name in [
            "",
            ".",
            "..",
            "../escape",
            "sub/file",
            r"sub\file",
            "C:evil",
            "a<b",
            "a>b",
            "a\"b",
            "a|b",
            "a?b",
            "a*b",
            "trailing.",
            "trailing ",
            "line\nfeed",
            "nul\0byte",
            "CON",
            "nul.txt",
            "CON .txt",
            "NUL .bin",
            "COM1.bin",
            "LPT9",
            "COM¹.txt",
        ] {
            assert!(!safe_entry_name(name), "accepted unsafe name: {name:?}");
        }
        for name in [
            "中文目录",
            "文件.bin",
            "ordinary file.txt",
            "COM10.txt",
            "a..b",
        ] {
            assert!(safe_entry_name(name), "rejected safe name: {name:?}");
        }
    }

    #[test]
    fn remote_containment_respects_component_boundaries() {
        assert!(contains_remote("/pub", "/pub/sub/file"));
        assert!(!contains_remote("/pub", "/public/file"));
        assert_eq!(normalize_remote("/pub//sub/"), "/pub/sub");
    }

    #[test]
    fn portable_credentials_need_no_os_account() {
        let mut stored: Stored = serde_json::from_value(json!({
            "sites":[{"id":1,"name":"portable","host":"localhost","port":21,
                "user":"测试用户","password":"test-secret","tls":"none","encoding":"auto","insecure":false}],
            "settings":{"queue_concurrency":2,"connections":4,"download_dir":"downloads"}
        })).unwrap();
        assert!(unlock_sites(&mut stored).unwrap());
        assert!(stored.sites[0].credentials.starts_with("obfuscated:v1:"));
        let saved = serde_json::to_string(&stored).unwrap();
        assert!(!saved.contains("test-secret"));
        assert!(!saved.contains("测试用户"));
        let mut restored: Stored = serde_json::from_str(&saved).unwrap();
        assert!(!unlock_sites(&mut restored).unwrap());
        assert_eq!(restored.sites[0].user, "测试用户");
        assert_eq!(restored.sites[0].password, "test-secret");
    }

    #[test]
    fn new_config_prefers_application_directory() {
        let app = PathBuf::from("app-local-config.json");
        let legacy = PathBuf::from("profile-config.json");
        let chosen =
            choose_config_file_with(Some(app.clone()), Some(legacy), None, |_| Ok(())).unwrap();
        assert_eq!(chosen, app);
    }

    #[test]
    fn existing_legacy_config_is_not_lost() {
        let dir = std::env::temp_dir().join(format!("ftpseg-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("legacy.json");
        std::fs::write(&legacy, b"{}").unwrap();
        let app = dir.join("new.json");
        let chosen =
            choose_config_file_with(Some(app), Some(legacy.clone()), None, |_| Ok(())).unwrap();
        assert_eq!(chosen, legacy);
        std::fs::remove_file(legacy).unwrap();
    }

    #[test]
    fn unwritable_application_directory_uses_working_directory() {
        let app = PathBuf::from("unwritable-app-config.json");
        let working = PathBuf::from("writable-working-config.json");
        let chosen =
            choose_config_file_with(Some(app.clone()), None, Some(working.clone()), |file| {
                if file == app {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "blocked",
                    ))
                } else {
                    Ok(())
                }
            })
            .unwrap();
        assert_eq!(chosen, working);
    }
}

fn check_config_writable(file: &Path) -> std::io::Result<()> {
    let parent = file.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "config path has no parent",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    // Test actual file creation, not just directory metadata or an ACL guess.
    // create_new avoids clobbering an existing file left by another process.
    let probe = parent.join(format!(
        ".ftpseg-write-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    std::fs::remove_file(probe)
}
#[derive(Serialize, Deserialize)]
struct SiteCredentials {
    user: String,
    password: String,
}

fn seal_site(site: &mut Site) -> Result<()> {
    let plaintext = serde_json::to_string(&SiteCredentials {
        user: site.user.clone(),
        password: site.password.clone(),
    })
    .map_err(|_| Error::Config("cannot encode site credentials".into()))?;
    site.credentials = crate::credentials::protect(&plaintext)?;
    Ok(())
}

fn unlock_sites(stored: &mut Stored) -> Result<bool> {
    let mut migrated = false;
    for site in &mut stored.sites {
        if site.credentials.is_empty() {
            seal_site(site)?;
            migrated = true;
        } else {
            let plaintext = crate::credentials::unprotect(&site.credentials).map_err(|_| {
                Error::Config(format!("cannot read saved credentials for site {}: configuration may be damaged, or legacy DPAPI/keyring credentials need the original account/environment to migrate; restore a valid config or reconfigure the site", site.id))
            })?;
            let credentials: SiteCredentials = serde_json::from_str(&plaintext)
                .map_err(|_| Error::Config("invalid decrypted site credentials".into()))?;
            site.user = credentials.user;
            site.password = credentials.password;
            if crate::credentials::needs_migration(&site.credentials) {
                seal_site(site)?;
                migrated = true;
            }
        }
    }
    Ok(migrated)
}

fn save(app: &App, stored: &Stored) -> Result<()> {
    let parent = app
        .file
        .parent()
        .ok_or_else(|| Error::Config("invalid configuration path".into()))?;
    std::fs::create_dir_all(parent).map_err(|e| {
        Error::Config(format!(
            "cannot create GUI config directory {}: {e}",
            parent.display()
        ))
    })?;
    if stored.sites.iter().any(|site| site.credentials.is_empty()) {
        return Err(Error::Config(
            "refusing to save unprotected site credentials".into(),
        ));
    }
    let bytes = serde_json::to_vec_pretty(stored).map_err(|e| Error::Config(e.to_string()))?;
    let temp = app.file.with_extension("json.tmp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        use std::io::Write;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    std::fs::write(&temp, &bytes)
        .map_err(|e| Error::Config(format!("cannot write GUI config {}: {e}", temp.display())))?;
    replace_config(&temp, &app.file).map_err(|e| {
        Error::Config(format!(
            "cannot save GUI config {}: {e}",
            app.file.display()
        ))
    })?;
    Ok(())
}
#[cfg(windows)]
fn replace_config(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // Both paths are in the same directory; replace without first deleting the
    // old config, so a failed migration cannot discard saved credentials.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_config(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(source, target)
}

fn bad(message: impl Into<String>) -> (u16, Value) {
    (400, json!({"error":message.into()}))
}
fn query(path: &str, key: &str) -> Option<String> {
    let raw = path.split_once('?')?.1;
    raw.split('&')
        .filter_map(|item| item.split_once('='))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| {
            percent_encoding::percent_decode_str(v)
                .decode_utf8()
                .ok()
                .map(|s| s.into_owned())
        })
}
fn id(path: &str) -> std::result::Result<u64, (u16, Value)> {
    query(path, "id")
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("missing id"))
}
fn required<'a>(body: &'a Value, k: &str) -> std::result::Result<&'a str, (u16, Value)> {
    body.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| bad(format!("missing {k}")))
}
fn valid_remote(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\r', '\n', '\0'])
        && path.split('/').all(|p| p != ".." && p != ".")
        && !path.contains('\\')
}
fn local_name(remote: &str) -> Option<&str> {
    let name = remote.trim_end_matches('/').rsplit('/').next()?;
    safe_entry_name(name).then_some(name)
}

fn safe_entry_name(name: &str) -> bool {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
    {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    !matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) && !["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}

const MAX_TREE_ENTRIES: usize = 10_000;
const MAX_TREE_DEPTH: usize = 64;

struct CollectedFile {
    remote: String,
    relative: PathBuf,
    total: Option<u64>,
}

fn normalize_remote(path: &str) -> String {
    let parts: Vec<_> = path.split('/').filter(|part| !part.is_empty()).collect();
    format!(
        "{}{}",
        if path.starts_with('/') { "/" } else { "" },
        parts.join("/")
    )
}

fn contains_remote(root: &str, path: &str) -> bool {
    root == path
        || path
            .strip_prefix(root)
            .is_some_and(|tail| tail.starts_with('/'))
}

async fn collect_directories(
    site: &Site,
    roots: &[String],
    explicit_count: usize,
) -> Result<Vec<CollectedFile>> {
    if site.password.is_empty() && !site.user.eq_ignore_ascii_case("anonymous") {
        return Err(Error::Config(
            "password required for non-anonymous login".into(),
        ));
    }
    let mut ftp = FtpClient::connect(&site.connection()).await?;
    let result = async {
        let mut files = Vec::new();
        let mut seen_remote = HashSet::new();
        let mut seen_local = HashSet::new();
        let mut root_names = HashSet::new();
        let mut stack = Vec::new();
        for (index, root) in roots.iter().enumerate() {
            let name =
                local_name(root).ok_or_else(|| Error::Config("unsafe directory name".into()))?;
            let mut local = name.to_owned();
            // Different remote roots may share a basename, including Windows case aliases.
            while !root_names.insert(local.to_lowercase()) {
                local = format!("{index}-{local}");
            }
            stack.push((root.clone(), PathBuf::from(local), 0usize));
        }
        let mut count = roots.len() + explicit_count;
        while let Some((remote, relative, depth)) = stack.pop() {
            if !seen_remote.insert(remote.clone()) {
                continue;
            }
            for entry in ftp.list(Some(&remote)).await? {
                if !safe_entry_name(&entry.name) {
                    return Err(Error::Config(format!(
                        "unsafe server entry name: {:?}",
                        entry.name
                    )));
                }
                count += 1;
                if count > MAX_TREE_ENTRIES {
                    return Err(Error::Config(
                        "directory selection exceeds 10000 files/directories".into(),
                    ));
                }
                let child_remote = format!("{remote}/{}", entry.name);
                let child_relative = relative.join(&entry.name);
                if !seen_local.insert(child_relative.to_string_lossy().to_lowercase()) {
                    return Err(Error::Config(
                        "duplicate or case-conflicting server entry name".into(),
                    ));
                }
                if entry.is_dir {
                    if depth >= MAX_TREE_DEPTH {
                        return Err(Error::Config("directory selection exceeds depth 64".into()));
                    }
                    stack.push((child_remote, child_relative, depth + 1));
                } else {
                    files.push(CollectedFile {
                        remote: child_remote,
                        relative: child_relative,
                        total: entry.size,
                    });
                }
            }
        }
        Ok(files)
    }
    .await;
    ftp.quit().await;
    result
}

/// Atomic directory creation reserves a batch namespace across requests/processes.
async fn reserve_batch(base: &Path) -> Result<PathBuf> {
    tokio::fs::create_dir_all(base).await?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..1000 {
        let dir = base.join(format!("batch-{stamp}-{}-{attempt}", std::process::id()));
        match tokio::fs::create_dir(&dir).await {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::Config("cannot reserve unique download batch".into()))
}

fn valid_dir(dir: &str) -> bool {
    !dir.trim().is_empty() && !dir.contains('\0') && Path::new(dir).is_absolute()
}

/// Serve a browser UI and JSON API bound strictly to a loopback IP (IPv4 or IPv6).
/// Credentials are only obfuscated at rest; never expose this listener via a proxy.
pub async fn serve(bind: SocketAddr) -> Result<()> {
    serve_with_ready(bind, |_| {}).await
}

/// Start the local GUI, notifying the launcher only after the listener is ready.
/// Using an ephemeral port avoids collisions with a CLI GUI already running.
pub async fn serve_with_ready(
    bind: SocketAddr,
    ready: impl FnOnce(SocketAddr) + Send,
) -> Result<()> {
    if !bind.ip().is_loopback() {
        return Err(Error::Config("GUI must bind to a loopback address".into()));
    }
    let file = config_file()?;
    println!("GUI config: {}", file.display());
    let mut stored = if file.exists() {
        serde_json::from_slice::<Stored>(&std::fs::read(&file)?)
            .map_err(|e| Error::Config(format!("GUI config: {e}")))?
    } else {
        Stored {
            sites: Vec::new(),
            settings: Settings::default(),
        }
    };
    let migrated = unlock_sites(&mut stored)?;
    let next_id = stored
        .sites
        .iter()
        .map(|s| s.id)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let app = Arc::new(App {
        state: Mutex::new(State {
            stored,
            jobs: Vec::new(),
            next_id,
            running: 0,
            scanning: 0,
        }),
        file,
        wake: Notify::new(),
        shutdown: Notify::new(),
        instance_id: format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ),
    });
    if migrated {
        save(&app, &app.state.lock().unwrap().stored)?;
        println!("Legacy credentials migrated to portable obfuscated storage");
    }
    let listener = TcpListener::bind(bind).await?;
    let actual = listener.local_addr()?;
    println!("GUI: http://{actual}/");
    ready(actual);
    tokio::spawn(dispatch(Arc::clone(&app)));
    loop {
        tokio::select! {
            incoming = listener.accept() => {
                let (stream, _) = incoming?;
                let app = Arc::clone(&app);
                tokio::spawn(async move { let _ = handle(stream, app).await; });
            }
            _ = app.shutdown.notified() => return Ok(()),
        }
    }
}
async fn dispatch(app: Arc<App>) {
    loop {
        let notified = app.wake.notified();
        let candidate = {
            let mut state = app.state.lock().unwrap();
            if state.running >= state.stored.settings.queue_concurrency {
                None
            } else {
                let idx = state.jobs.iter().position(|j| j.job.status == "queued");
                idx.map(|idx| {
                    let site = state
                        .stored
                        .sites
                        .iter()
                        .find(|s| s.id == state.jobs[idx].job.site_id)
                        .cloned();
                    let settings = state.stored.settings.clone();
                    let (tx, rx) = watch::channel(false);
                    state.jobs[idx].job.status = "running".into();
                    state.jobs[idx].job.error = None;
                    state.jobs[idx].cancel = Some(tx);
                    state.running += 1;
                    (
                        state.jobs[idx].job.id,
                        state.jobs[idx].job.remote.clone(),
                        state.jobs[idx].job.local.clone(),
                        site,
                        settings,
                        rx,
                    )
                })
            }
        };
        if let Some((id, remote, local, site, settings, rx)) = candidate {
            let app = Arc::clone(&app);
            tokio::spawn(async move {
                let result = if let Some(site) = site {
                    run_download(
                        Arc::clone(&app),
                        id,
                        remote,
                        local.clone(),
                        site,
                        settings,
                        rx,
                    )
                    .await
                } else {
                    Err(Error::Config("site removed".into()))
                };
                // run_download has joined all writers before cleanup is allowed.
                // A successful transfer wins a simultaneous cancellation request.
                let cleanup = {
                    let mut state = app.state.lock().unwrap();
                    let mut cleanup = false;
                    if let Some(job) = state.jobs.iter_mut().find(|j| j.job.id == id) {
                        match result {
                            Ok((bytes, total)) => {
                                job.job.status = "done".into();
                                job.job.downloaded = bytes;
                                job.job.total = Some(total);
                                job.job.error = None;
                                job.cancel = None;
                            }
                            Err(_) if job.job.status == "cancelling" => cleanup = true,
                            Err(_) if job.job.status == "pausing" => {
                                // The downloader has joined writers and saved its checkpoint.
                                // Do not delete partial files or overwrite an existing error.
                                job.job.status = "paused".into();
                                job.cancel = None;
                            }
                            Err(e) => {
                                job.job.status = "failed".into();
                                job.job.error = Some(e.to_string());
                                job.cancel = None;
                            }
                        }
                    }
                    if !cleanup {
                        state.running = state.running.saturating_sub(1);
                    }
                    cleanup
                };
                if cleanup {
                    finish_cleanup(&app, id, &local).await;
                }
                app.wake.notify_one();
            });
        } else {
            notified.await;
        }
    }
}
/// Delete only the frozen target and the two paths actually used by StateStore.
/// Missing files are fine; directories are never recursively removed.
async fn cleanup_download(local: &Path) -> std::result::Result<(), String> {
    let store = crate::state::StateStore::for_download(local);
    let mut temp = store.path().as_os_str().to_os_string();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let mut errors = Vec::new();
    for path in [local, store.path(), temp.as_path()] {
        match tokio::fs::remove_file(path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("cancel cleanup failed: {}", errors.join("; ")))
    }
}

async fn finish_cleanup(app: &App, id: u64, local: &Path) {
    // No state lock across filesystem awaits. cancel stays Some and the queue
    // slot stays occupied until all three deletion attempts have finished.
    let result = cleanup_download(local).await;
    let mut state = app.state.lock().unwrap();
    if let Some(job) = state.jobs.iter_mut().find(|j| j.job.id == id) {
        job.job.status = "cancelled".into();
        job.job.error = result.err();
        if job.job.error.is_none() {
            job.job.downloaded = 0;
        }
        job.cancel = None;
    }
    state.running = state.running.saturating_sub(1);
}

async fn run_download(
    app: Arc<App>,
    id: u64,
    remote: String,
    local: PathBuf,
    site: Site,
    settings: Settings,
    cancel: watch::Receiver<bool>,
) -> Result<(u64, u64)> {
    if *cancel.borrow() {
        return Err(Error::Aborted);
    }
    local_name(&remote).ok_or_else(|| Error::Config("unsafe filename".into()))?;
    if site.password.is_empty() && !site.user.eq_ignore_ascii_case("anonymous") {
        return Err(Error::Config(
            "password required for non-anonymous login".into(),
        ));
    }
    let connection = Arc::new(site.connection());
    let info = download::probe(&connection, &remote).await?;
    if info.is_dir {
        return Err(Error::Config("remote path is a directory".into()));
    }
    {
        let mut state = app.state.lock().unwrap();
        if let Some(job) = state.jobs.iter_mut().find(|j| j.job.id == id) {
            job.job.total = info.size;
        }
    }
    if *cancel.borrow() {
        return Err(Error::Aborted);
    }
    let dir = local
        .parent()
        .ok_or_else(|| Error::Config("invalid local target".into()))?;
    tokio::fs::create_dir_all(dir).await?;
    let mut options = DownloadOptions::new(&remote, local);
    // From this point the downloader either resumes the validated checkpoint or
    // starts over. Never add new progress to an unvalidated in-memory counter.
    {
        let mut state = app.state.lock().unwrap();
        if let Some(job) = state.jobs.iter_mut().find(|j| j.job.id == id) {
            job.job.downloaded = 0;
        }
    }
    if let Some(total) = info.size {
        if let Some(saved) = crate::state::StateStore::for_download(&options.local).load() {
            if saved.matches(&remote, total, info.mdtm.as_deref())
                && std::fs::metadata(&options.local).is_ok_and(|m| m.is_file() && m.len() >= total)
            {
                if let Some(segments) = crate::plan::validate_resumed(&saved.segments(), total) {
                    let mut state = app.state.lock().unwrap();
                    if let Some(job) = state.jobs.iter_mut().find(|j| j.job.id == id) {
                        job.job.downloaded = crate::plan::completed_bytes(&segments);
                    }
                }
            }
        }
    }
    options.policy.connections = settings.connections;
    options.quiet = true;
    let app2 = Arc::clone(&app);
    let observer: Arc<dyn Fn(u64) + Send + Sync> = Arc::new(move |delta| {
        let mut state = app2.state.lock().unwrap();
        if let Some(job) = state.jobs.iter_mut().find(|j| j.job.id == id) {
            job.job.downloaded = job.job.downloaded.saturating_add(delta);
        }
    });
    let summary =
        download::download_file_with_progress(connection, options, cancel, Some(observer)).await?;
    Ok((summary.bytes, summary.bytes))
}
async fn api(app: Arc<App>, method: &str, path: &str, body: Value) -> (u16, Value) {
    match (method, path.split('?').next().unwrap_or(path)) {
        ("POST", "/api/shutdown") => {
            let state = app.state.lock().unwrap();
            if state.running > 0
                || state.scanning > 0
                || state.jobs.iter().any(|job| job.job.status == "queued")
            {
                return (409, json!({"error":"downloads are still active or queued"}));
            }
            // Paused jobs may exit: the queue is memory-only, while files/checkpoints
            // remain on disk. Browser display caches do not restore backend jobs.
            // Let the HTTP response finish before closing the listener.
            app.shutdown.notify_one();
            (200, json!({"ok":true}))
        }
        ("GET", "/api/state") => {
            let s = app.state.lock().unwrap();
            (
                200,
                json!({"instance_id":app.instance_id,"sites":s.stored.sites.iter().map(Site::public).collect::<Vec<_>>(),"settings":s.stored.settings,"jobs":s.jobs.iter().map(|j|j.job.clone()).collect::<Vec<_>>()}),
            )
        }
        ("POST", "/api/sites") => {
            let name = match required(&body, "name") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let host = match required(&body, "host") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let port = body.get("port").and_then(Value::as_u64).unwrap_or(21);
            let tls = body.get("tls").and_then(Value::as_str).unwrap_or("none");
            if name.trim().is_empty()
                || host.trim().is_empty()
                || host.contains(['\n', '\r', '\0'])
                || port == 0
                || port > 65535
                || !["none", "explicit", "implicit"].contains(&tls)
            {
                return bad("invalid site");
            }
            let encoding = body
                .get("encoding")
                .and_then(Value::as_str)
                .unwrap_or("auto");
            let request = if encoding.eq_ignore_ascii_case("auto") {
                CharsetRequest::Auto
            } else {
                CharsetRequest::Named(encoding.into())
            };
            if request.resolve(false).is_err() {
                return bad("invalid encoding");
            }
            let mut s = app.state.lock().unwrap();
            let old = body
                .get("id")
                .and_then(Value::as_u64)
                .and_then(|id| s.stored.sites.iter().position(|x| x.id == id));
            if body.get("id").is_some() && old.is_none() {
                return (404, json!({"error":"site not found"}));
            }
            let id = old.map(|i| s.stored.sites[i].id).unwrap_or(s.next_id);
            let password = body
                .get("password")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .or_else(|| old.map(|i| s.stored.sites[i].password.clone()))
                .unwrap_or_default();
            let mut site = Site {
                id,
                name: name.into(),
                host: host.into(),
                port: port as u16,
                user: body
                    .get("user")
                    .and_then(Value::as_str)
                    .unwrap_or("anonymous")
                    .into(),
                password,
                credentials: String::new(),
                tls: tls.into(),
                encoding: encoding.into(),
                insecure: body
                    .get("insecure")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            };
            // Encrypt before changing live state; failed protection/save must not
            // leave an apparently saved but unprotected site in memory.
            if let Some(previous) = old
                .map(|i| &s.stored.sites[i])
                .filter(|previous| previous.user == site.user && previous.password == site.password)
            {
                site.credentials = previous.credentials.clone();
            } else if let Err(e) = seal_site(&mut site) {
                return (500, json!({"error":e.to_string()}));
            }
            let mut candidate = s.stored.clone();
            if let Some(i) = old {
                candidate.sites[i] = site.clone();
            } else {
                candidate.sites.push(site.clone());
            }
            if let Err(e) = save(&app, &candidate) {
                return (500, json!({"error":e.to_string()}));
            }
            s.stored = candidate;
            if old.is_none() {
                s.next_id = s.next_id.saturating_add(1);
            }
            (200, site.public())
        }
        ("DELETE", "/api/sites") => {
            let id = match id(path) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let mut s = app.state.lock().unwrap();
            if !s.stored.sites.iter().any(|x| x.id == id) {
                return (404, json!({"error":"site not found"}));
            };
            s.stored.sites.retain(|x| x.id != id);
            if let Err(e) = save(&app, &s.stored) {
                return (500, json!({"error":e.to_string()}));
            };
            (200, json!({"ok":true}))
        }
        ("POST", "/api/settings") => {
            let mut s = app.state.lock().unwrap();
            let mut settings = s.stored.settings.clone();
            if let Some(v) = body.get("queue_concurrency") {
                settings.queue_concurrency = match v.as_u64() {
                    Some(n @ 1..=32) => n as usize,
                    _ => return bad("queue_concurrency must be 1..32"),
                };
            }
            if let Some(v) = body.get("connections") {
                settings.connections = match v.as_u64() {
                    Some(n @ 1..=32) => n as usize,
                    _ => return bad("connections must be 1..32"),
                };
            }
            if let Some(v) = body.get("download_dir") {
                settings.download_dir = match v.as_str() {
                    Some(dir) if valid_dir(dir) => dir.into(),
                    _ => return bad("download_dir must be absolute"),
                };
            }
            s.stored.settings = settings.clone();
            if let Err(e) = save(&app, &s.stored) {
                return (500, json!({"error":e.to_string()}));
            };
            app.wake.notify_one();
            (200, json!(settings))
        }
        ("GET", "/api/list") => {
            let site_id = match query(path, "site_id").and_then(|s| s.parse::<u64>().ok()) {
                Some(v) => v,
                None => return bad("missing site_id"),
            };
            let remote = query(path, "path").unwrap_or_default();
            if !remote.is_empty() && !valid_remote(&remote) {
                return bad("invalid path");
            };
            let site = {
                app.state
                    .lock()
                    .unwrap()
                    .stored
                    .sites
                    .iter()
                    .find(|s| s.id == site_id)
                    .cloned()
            };
            let Some(site) = site else {
                return (404, json!({"error":"site not found"}));
            };
            if site.password.is_empty() && !site.user.eq_ignore_ascii_case("anonymous") {
                return bad("password required for non-anonymous login");
            }
            match FtpClient::connect(&site.connection()).await {
                Ok(mut ftp) => {
                    let result = ftp
                        .list(if remote.is_empty() {
                            None
                        } else {
                            Some(&remote)
                        })
                        .await;
                    ftp.quit().await;
                    match result {
                        Ok(entries) => (
                            200,
                            json!({"entries":entries.iter().map(|e|json!({"name":e.name,"is_dir":e.is_dir,"size":e.size,"modify":e.modify})).collect::<Vec<_>>()}),
                        ),
                        Err(e) => (502, json!({"error":e.to_string()})),
                    }
                }
                Err(e) => (502, json!({"error":e.to_string()})),
            }
        }
        ("POST", "/api/queue") => {
            let site_id = match body.get("site_id").and_then(Value::as_u64) {
                Some(v) => v,
                None => return bad("missing site_id"),
            };
            let parse = |key: &str| -> std::result::Result<Vec<String>, (u16, Value)> {
                let Some(value) = body.get(key) else {
                    return Ok(Vec::new());
                };
                let Some(values) = value.as_array() else {
                    return Err(bad(format!("{key} must be a string array")));
                };
                values
                    .iter()
                    .map(|value| {
                        let Some(remote) = value.as_str() else {
                            return Err(bad(format!("invalid {key}")));
                        };
                        if !valid_remote(remote)
                            || remote
                                .split('/')
                                .filter(|p| !p.is_empty())
                                .any(|p| !safe_entry_name(p))
                            || local_name(remote).is_none()
                        {
                            return Err(bad(format!("invalid {key}")));
                        }
                        Ok(normalize_remote(remote))
                    })
                    .collect()
            };
            let paths = match parse("paths") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let mut directories = match parse("directories") {
                Ok(v) => v,
                Err(e) => return e,
            };
            if paths.is_empty() && directories.is_empty() {
                return bad("at least one file or directory is required");
            }
            if paths.len() > 500 || paths.len() + directories.len() > MAX_TREE_ENTRIES {
                return bad(
                    "selection exceeds limits (500 explicit files, 10000 files/directories)",
                );
            }
            directories.sort();
            directories.dedup();
            let roots: Vec<_> = directories
                .iter()
                .filter(|root| {
                    !directories
                        .iter()
                        .any(|other| other != *root && contains_remote(other, root))
                })
                .cloned()
                .collect();
            let (site, download_dir, _scan) = {
                let mut s = app.state.lock().unwrap();
                let Some(site) = s.stored.sites.iter().find(|x| x.id == site_id).cloned() else {
                    return (404, json!({"error":"site not found"}));
                };
                let dir = PathBuf::from(&s.stored.settings.download_dir);
                if !dir.is_absolute() {
                    return bad("download_dir must be absolute");
                }
                s.scanning += 1;
                (site, dir, ScanGuard(Arc::clone(&app)))
            };
            // No state lock is held during FTP traversal or filesystem awaits.
            let files = if roots.is_empty() {
                Vec::new()
            } else {
                match collect_directories(
                    &site,
                    &roots,
                    paths
                        .iter()
                        .filter(|path| !roots.iter().any(|root| contains_remote(root, path)))
                        .collect::<HashSet<_>>()
                        .len(),
                )
                .await
                {
                    Ok(files) => files,
                    Err(e) => return (502, json!({"error":e.to_string()})),
                }
            };
            let mut seen: HashSet<_> = files.iter().map(|f| f.remote.clone()).collect();
            let explicit: Vec<_> = paths
                .into_iter()
                .filter(|remote| seen.insert(remote.clone()))
                .collect();
            if files.len() + explicit.len() > MAX_TREE_ENTRIES {
                return bad("selection exceeds 10000 files/directories");
            }
            let batch = if files.is_empty() {
                None
            } else {
                match reserve_batch(&download_dir).await {
                    Ok(dir) => Some(dir),
                    Err(e) => return (500, json!({"error":e.to_string()})),
                }
            };
            let mut s = app.state.lock().unwrap();
            if !s.stored.sites.iter().any(|x| x.id == site_id) {
                return (404, json!({"error":"site removed during scan"}));
            }
            let mut ids = Vec::new();
            let mut add = |remote: String, target: Option<PathBuf>, total: Option<u64>| {
                let id = s.next_id;
                s.next_id = s.next_id.saturating_add(1);
                let local = target.unwrap_or_else(|| {
                    download_dir.join(format!("{id}-{}", local_name(&remote).unwrap()))
                });
                ids.push(id);
                s.jobs.push(RuntimeJob {
                    job: Job {
                        id,
                        site_id,
                        remote,
                        local,
                        status: "queued".into(),
                        downloaded: 0,
                        total,
                        error: None,
                    },
                    cancel: None,
                });
            };
            for file in files {
                add(
                    file.remote,
                    Some(batch.as_ref().unwrap().join(file.relative)),
                    file.total,
                );
            }
            for remote in explicit {
                add(remote, None, None);
            }
            app.wake.notify_one();
            (
                200,
                json!({"ids":ids,"message":if ids.is_empty() { "Selected directories currently contain no files; no empty directories were created." } else { "Files queued; empty directories are not created." }}),
            )
        }
        ("POST", "/api/jobs/pause") => {
            let Some(id) = body.get("id").and_then(Value::as_u64) else {
                return bad("missing id");
            };
            let mut s = app.state.lock().unwrap();
            let Some(job) = s.jobs.iter_mut().find(|j| j.job.id == id) else {
                return (404, json!({"error":"job not found"}));
            };
            match job.job.status.as_str() {
                "done" | "paused" | "pausing" => {}
                "queued" if job.cancel.is_none() => job.job.status = "paused".into(),
                "running" => {
                    job.job.status = "pausing".into();
                    if let Some(tx) = &job.cancel {
                        let _ = tx.send(true);
                    }
                }
                _ => return bad("job cannot be paused in its current state"),
            }
            (200, json!({"ok":true}))
        }
        ("POST", "/api/jobs/retry")
        | ("POST", "/api/jobs/resume")
        | ("POST", "/api/jobs/cancel") => {
            let Some(id) = body.get("id").and_then(Value::as_u64) else {
                return bad("missing id");
            };
            let resume = path.starts_with("/api/jobs/resume");
            let retry = resume || path.starts_with("/api/jobs/retry");
            let mut s = app.state.lock().unwrap();
            let Some(job) = s.jobs.iter_mut().find(|j| j.job.id == id) else {
                return (404, json!({"error":"job not found"}));
            };
            if retry {
                if job.cancel.is_some() || job.job.status == "running" || job.job.status == "queued"
                {
                    return bad("job still active; wait for cancellation to finish");
                };
                if resume && job.job.status != "paused" {
                    return bad("only paused jobs can be resumed");
                }
                // Preserve progress until run_download initializes it from the checkpoint.
                // A paused retry is exactly a resume; other retries retain prior behavior.
                if job.job.status != "paused" {
                    job.job.downloaded = 0;
                    job.job.total = None;
                    job.job.error = None;
                }
                job.job.status = "queued".into();
                app.wake.notify_one();
            } else {
                if job.job.status == "done"
                    || job.cancel.is_some() && job.job.status == "cancelling"
                {
                    return (200, json!({"ok":true}));
                }
                job.job.error = None;
                job.job.status = "cancelling".into();
                if let Some(tx) = &job.cancel {
                    let _ = tx.send(true);
                } else {
                    // Queued retries and failed jobs can still own partial files.
                    // Reserve an activity slot and marker before releasing the lock.
                    let local = job.job.local.clone();
                    let (tx, _) = watch::channel(true);
                    job.cancel = Some(tx);
                    s.running += 1;
                    let app = Arc::clone(&app);
                    tokio::spawn(async move {
                        finish_cleanup(&app, id, &local).await;
                        app.wake.notify_one();
                    });
                }
            }
            (200, json!({"ok":true}))
        }
        _ => (404, json!({"error":"not found"})),
    }
}
async fn handle(mut stream: TcpStream, app: Arc<App>) -> Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut chunk))
            .await
            .map_err(|_| Error::Timeout(Duration::from_secs(10)))??;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEADERS {
            return send(
                &mut stream,
                431,
                json!({"error":"headers too large"}),
                false,
            )
            .await;
        }
    };
    if header_end > MAX_HEADERS {
        return send(
            &mut stream,
            431,
            json!({"error":"headers too large"}),
            false,
        )
        .await;
    }
    let head = match std::str::from_utf8(&buf[..header_end]) {
        Ok(s) => s,
        Err(_) => return send(&mut stream, 400, json!({"error":"invalid headers"}), false).await,
    };
    let mut lines = head.split("\r\n");
    let request = lines.next().unwrap_or("");
    let parts: Vec<_> = request.split_whitespace().collect();
    if parts.len() != 3
        || parts[2] != "HTTP/1.1"
        || !["GET", "POST", "DELETE"].contains(&parts[0])
        || !parts[1].starts_with('/')
    {
        return send(&mut stream, 400, json!({"error":"invalid request"}), false).await;
    }
    let (method, path) = (parts[0], parts[1]);
    let mut host = None;
    let mut origin = None;
    let mut length = None;
    let mut content_type = None;
    let mut transfer = false;
    for line in lines.filter(|s| !s.is_empty()) {
        let Some((k, v)) = line.split_once(':') else {
            return send(&mut stream, 400, json!({"error":"invalid header"}), false).await;
        };
        let v = v.trim();
        if k.eq_ignore_ascii_case("host") {
            if host.replace(v).is_some() {
                return send(&mut stream, 400, json!({"error":"duplicate host"}), false).await;
            }
        }
        if k.eq_ignore_ascii_case("origin") {
            if origin.replace(v).is_some() {
                return send(&mut stream, 400, json!({"error":"duplicate origin"}), false).await;
            }
        }
        if k.eq_ignore_ascii_case("content-length") {
            if length.replace(v).is_some() {
                return send(&mut stream, 400, json!({"error":"duplicate length"}), false).await;
            }
        }
        if k.eq_ignore_ascii_case("content-type") {
            content_type = Some(v)
        }
        if k.eq_ignore_ascii_case("transfer-encoding") {
            transfer = true
        }
    }
    let local = stream.local_addr()?;
    let allowed = match local.ip() {
        IpAddr::V4(_) => format!("127.0.0.1:{}", local.port()),
        IpAddr::V6(_) => format!("[::1]:{}", local.port()),
    };
    let valid_host = host.is_some_and(|h| {
        h.eq_ignore_ascii_case(&allowed)
            || (local.port() == 80
                && ["localhost", "127.0.0.1", "[::1]"]
                    .iter()
                    .any(|x| h.eq_ignore_ascii_case(x)))
    });
    let valid_origin = origin.map_or(true, |o| {
        o.eq_ignore_ascii_case(&format!("http://{allowed}"))
    });
    if !valid_host || !valid_origin || transfer {
        return send(
            &mut stream,
            403,
            json!({"error":"invalid host/origin or transfer encoding"}),
            false,
        )
        .await;
    }
    if method == "GET" && (path == "/" || path == "/gui.html") {
        return send(&mut stream, 200, Value::String(HTML.into()), true).await;
    }
    if !path.starts_with("/api/") {
        return send(&mut stream, 404, json!({"error":"not found"}), false).await;
    }
    let len = match length {
        Some(s) => match s.parse::<usize>() {
            Ok(n) if n <= MAX_BODY => n,
            _ => return send(&mut stream, 413, json!({"error":"body too large"}), false).await,
        },
        None => 0,
    };
    if method == "POST"
        && (!content_type.is_some_and(|s| {
            s.split(';')
                .next()
                .is_some_and(|x| x.trim().eq_ignore_ascii_case("application/json"))
        }) || len == 0)
    {
        return send(
            &mut stream,
            415,
            json!({"error":"JSON body required"}),
            false,
        )
        .await;
    }
    let mut body = buf[header_end..].to_vec();
    if body.len() > len {
        return send(&mut stream, 400, json!({"error":"unexpected bytes"}), false).await;
    }
    while body.len() < len {
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut chunk))
            .await
            .map_err(|_| Error::Timeout(Duration::from_secs(10)))??;
        if n == 0 {
            return send(&mut stream, 400, json!({"error":"incomplete body"}), false).await;
        }
        if body.len() + n > len {
            return send(&mut stream, 400, json!({"error":"unexpected bytes"}), false).await;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    let json = if method == "POST" {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => return send(&mut stream, 400, json!({"error":"invalid JSON"}), false).await,
        }
    } else {
        Value::Null
    };
    let (status, response) = api(app, method, path, json).await;
    send(&mut stream, status, response, false).await
}
async fn send(stream: &mut TcpStream, status: u16, body: Value, html: bool) -> Result<()> {
    let data = if html {
        body.as_str().unwrap_or("").as_bytes().to_vec()
    } else {
        serde_json::to_vec(&body).map_err(|e| Error::Config(e.to_string()))?
    };
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "Error",
    };
    let head=format!("HTTP/1.1 {status} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nX-Ftpseg-Gui: 1\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'\r\nConnection: close\r\n\r\n",if html {"text/html; charset=utf-8"}else{"application/json; charset=utf-8"},data.len());
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&data).await?;
    Ok(())
}
