//! Black-box checks for the loopback GUI and its JSON API.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recursive_directory_queue_preserves_structure_and_deduplicates() {
    let mut files = common::Vfs::new();
    files.add("/中文根/甲/同名.bin", vec![1; 8192]);
    files.add("/中文根/乙/深层/同名.bin", vec![2; 16384]);
    files.add("/中文根/根文件.txt", vec![3; 1024]);
    let ftp = common::TestServer::start(files, common::ServerConfig::new()).await;
    let temp = common::temp_dir("gui-recursive");
    let gui = Gui::start_with_config(Some(&temp.join("gui.json")));
    let site = serde_json::json!({"name":"recursive","host":ftp.host(),"port":ftp.port(),"user":"testuser","password":"testpass"});
    let (status, body) = gui.call("POST", "/api/sites", Some(&site.to_string()), None);
    assert_eq!(status, 200, "{body}");
    let site_id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_u64()
        .unwrap();
    let original = temp.join("downloads");
    let settings = serde_json::json!({"download_dir":original,"connections":1});
    assert_eq!(
        gui.call("POST", "/api/settings", Some(&settings.to_string()), None)
            .0,
        200
    );
    // directories-only is valid. A second mixed request overlaps selected roots/files.
    for request in [
        serde_json::json!({"site_id":site_id,"directories":["/中文根"]}),
        serde_json::json!({"site_id":site_id,"directories":["/中文根/乙", "/中文根/", "/中文根"],"paths":["/中文根/甲/同名.bin", "/中文根/甲/同名.bin"]}),
    ] {
        let (status, body) = gui.call("POST", "/api/queue", Some(&request.to_string()), None);
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["ids"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }
    let changed = serde_json::json!({"download_dir":temp.join("changed")});
    assert_eq!(
        gui.call("POST", "/api/settings", Some(&changed.to_string()), None)
            .0,
        200
    );
    let mut final_jobs = Vec::new();
    for _ in 0..200 {
        let state: serde_json::Value =
            serde_json::from_str(&gui.call("GET", "/api/state", None, None).1).unwrap();
        let jobs = state["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 6);
        assert!(jobs.iter().all(|j| j["status"] != "failed"), "{jobs:?}");
        if jobs.iter().all(|j| j["status"] == "done") {
            final_jobs = jobs.clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(final_jobs.len(), 6, "recursive queue timed out");
    let mut targets = std::collections::HashSet::new();
    for job in &final_jobs {
        let local = std::path::PathBuf::from(job["local"].as_str().unwrap());
        assert!(local.is_absolute() && local.starts_with(&original));
        assert!(targets.insert(local.clone()), "target collision");
        let suffix = job["remote"].as_str().unwrap().trim_start_matches('/');
        assert!(local.ends_with(std::path::Path::new(suffix)));
        let expected = if suffix.contains("/甲/") {
            vec![1; 8192]
        } else if suffix.contains("/乙/") {
            vec![2; 16384]
        } else {
            vec![3; 1024]
        };
        assert_eq!(std::fs::read(&local).unwrap(), expected);
    }
    // Retry uses the same absolute target even though settings were changed.
    let before = final_jobs[0]["local"].clone();
    let retry = serde_json::json!({"id":final_jobs[0]["id"]});
    assert_eq!(
        gui.call("POST", "/api/jobs/retry", Some(&retry.to_string()), None)
            .0,
        200
    );
    let state: serde_json::Value =
        serde_json::from_str(&gui.call("GET", "/api/state", None, None).1).unwrap();
    assert_eq!(state["jobs"][0]["local"], before);
    drop(gui);
    std::fs::remove_dir_all(temp).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsafe_listing_fails_atomically_and_empty_directory_is_explicit() {
    let mut files = common::Vfs::new();
    files.add("/root/good.bin", vec![1]);
    files.add("/root/z:unsafe.bin", vec![2]);
    let ftp = common::TestServer::start(files, common::ServerConfig::new()).await;
    let temp = common::temp_dir("gui-unsafe-tree");
    let gui = Gui::start_with_config(Some(&temp.join("gui.json")));
    let site = serde_json::json!({"name":"unsafe","host":ftp.host(),"port":ftp.port(),"user":"testuser","password":"testpass"});
    let (_, body) = gui.call("POST", "/api/sites", Some(&site.to_string()), None);
    let site_id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_u64()
        .unwrap();
    let request =
        serde_json::json!({"site_id":site_id,"directories":["/root"],"paths":["/root/good.bin"]});
    let (status, body) = gui.call("POST", "/api/queue", Some(&request.to_string()), None);
    assert_eq!(status, 502, "{body}");
    assert!(body.contains("unsafe"));
    let state: serde_json::Value =
        serde_json::from_str(&gui.call("GET", "/api/state", None, None).1).unwrap();
    assert!(state["jobs"].as_array().unwrap().is_empty());
    let request = serde_json::json!({"site_id":site_id,"directories":["/empty"]});
    let (status, body) = gui.call("POST", "/api/queue", Some(&request.to_string()), None);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("currently contain no files"));
    drop(gui);
    std::fs::remove_dir_all(temp).unwrap();
}
#[test]
fn credentials_are_obfuscated_and_legacy_config_is_migrated() {
    let temp = common::temp_dir("encrypted-config");
    let file = temp.join("gui.json");
    let legacy = serde_json::json!({"sites":[{"id":1,"name":"Legacy","host":"localhost","port":21,"user":"sensitive-user","password":"sensitive-password","tls":"none","encoding":"auto","insecure":false}],"settings":{"queue_concurrency":2,"connections":4,"download_dir":temp.join("downloads").to_string_lossy()}});
    std::fs::write(&file, legacy.to_string()).unwrap();
    let gui = Gui::start_with_config(Some(&file));
    let persisted = std::fs::read_to_string(&file).unwrap();
    assert!(!persisted.contains("sensitive-user"));
    assert!(!persisted.contains("sensitive-password"));
    let data: serde_json::Value = serde_json::from_str(&persisted).unwrap();
    assert!(data["sites"][0].get("user").is_none());
    assert!(data["sites"][0].get("password").is_none());
    assert!(data["sites"][0]["credentials"]
        .as_str()
        .unwrap()
        .starts_with("obfuscated:v1:"));
    let public: serde_json::Value =
        serde_json::from_str(&gui.call("GET", "/api/state", None, None).1).unwrap();
    assert_eq!(public["sites"][0]["user"], "sensitive-user");
    assert_eq!(public["sites"][0]["has_password"], true);
    drop(gui);
    let gui = Gui::start_with_config(Some(&file));
    let public: serde_json::Value =
        serde_json::from_str(&gui.call("GET", "/api/state", None, None).1).unwrap();
    assert_eq!(public["sites"][0]["user"], "sensitive-user");
    let update = serde_json::json!({"id":1,"name":"Updated","host":"localhost","port":21,"user":"sensitive-user","password":"","tls":"none","encoding":"auto","insecure":false});
    let (status, body) = gui.call("POST", "/api/sites", Some(&update.to_string()), None);
    assert_eq!(status, 200, "{body}");
    assert!(!std::fs::read_to_string(&file)
        .unwrap()
        .contains("sensitive-password"));
    drop(gui);
    let gui = Gui::start_with_config(Some(&file));
    let public: serde_json::Value =
        serde_json::from_str(&gui.call("GET", "/api/state", None, None).1).unwrap();
    assert_eq!(public["sites"][0]["has_password"], true);
    drop(gui);
    std::fs::remove_dir_all(temp).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_removes_partial_and_checkpoint_but_keeps_completed_files() {
    let temp = common::temp_dir("gui-cancel-cleanup");
    let mut files = common::Vfs::new();
    files.add("/slow.bin", vec![7; 2 * 1024 * 1024]);
    files.add("/small.bin", vec![9; 32]);
    let ftp = common::TestServer::start(
        files,
        common::ServerConfig::new()
            .chunk_size(4096)
            .chunk_delay(Duration::from_millis(10)),
    )
    .await;
    let gui = Gui::start_with_config(Some(&temp.join("gui.json")));
    let site = serde_json::json!({"name":"cancel","host":ftp.host(),"port":ftp.port(),"user":"testuser","password":"testpass"});
    let (_, body) = gui.call("POST", "/api/sites", Some(&site.to_string()), None);
    let site_id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_u64()
        .unwrap();
    let settings = serde_json::json!({"queue_concurrency":1,"connections":1,"download_dir":temp.join("downloads").to_string_lossy()});
    assert_eq!(
        gui.call("POST", "/api/settings", Some(&settings.to_string()), None)
            .0,
        200
    );
    let queue = serde_json::json!({"site_id":site_id,"paths":["/slow.bin","/small.bin"]});
    assert_eq!(
        gui.call("POST", "/api/queue", Some(&queue.to_string()), None)
            .0,
        200
    );
    let mut target = None;
    for _ in 0..200 {
        let state: serde_json::Value =
            serde_json::from_str(&gui.call("GET", "/api/state", None, None).1).unwrap();
        let job = &state["jobs"][0];
        if job["downloaded"].as_u64().unwrap_or(0) > 0 {
            target = Some((
                job["id"].as_u64().unwrap(),
                std::path::PathBuf::from(job["local"].as_str().unwrap()),
            ));
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (id, local) = target.expect("download did not start");
    assert!(local.exists());
    let store = ftpseg::state::StateStore::for_download(&local);
    let tmp = std::path::PathBuf::from(format!("{}.tmp", store.path().display()));
    std::fs::write(store.path(), b"checkpoint fixture").unwrap();
    std::fs::write(&tmp, b"temp fixture").unwrap();
    let cancel = serde_json::json!({"id":id});
    assert_eq!(
        gui.call("POST", "/api/jobs/cancel", Some(&cancel.to_string()), None)
            .0,
        200
    );
    let mut finished = false;
    for _ in 0..200 {
        let state: serde_json::Value =
            serde_json::from_str(&gui.call("GET", "/api/state", None, None).1).unwrap();
        if state["jobs"][0]["status"] == "cancelled" && state["jobs"][1]["status"] == "done" {
            assert!(state["jobs"][0]["error"].is_null(), "{}", state["jobs"][0]);
            let completed = std::path::PathBuf::from(state["jobs"][1]["local"].as_str().unwrap());
            let cancel_done = serde_json::json!({"id":state["jobs"][1]["id"]});
            assert_eq!(
                gui.call(
                    "POST",
                    "/api/jobs/cancel",
                    Some(&cancel_done.to_string()),
                    None
                )
                .0,
                200
            );
            assert_eq!(std::fs::read(completed).unwrap(), vec![9; 32]);
            finished = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(finished);
    assert!(!local.exists());
    assert!(!store.path().exists());
    assert!(!tmp.exists());
    drop(gui);
    std::fs::remove_dir_all(temp).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_preserves_checkpoint_resumes_and_cancel_deletes() {
    let temp = common::temp_dir("gui-pause");
    let expected = vec![17; 512 * 1024];
    let mut files = common::Vfs::new();
    files.add("/slow.bin", expected.clone());
    let ftp = common::TestServer::start(
        files,
        common::ServerConfig::new()
            .chunk_size(4096)
            .chunk_delay(Duration::from_millis(10)),
    )
    .await;
    let config = temp.join("gui.json");
    let gui = Gui::start_with_config(Some(&config));
    let instance = gui.state()["instance_id"].as_str().unwrap().to_owned();
    assert!(!instance.is_empty());
    assert_eq!(gui.state()["instance_id"], instance);
    let site = serde_json::json!({"name":"pause","host":ftp.host(),"port":ftp.port(),"user":"testuser","password":"testpass"});
    let (code, body) = gui.call("POST", "/api/sites", Some(&site.to_string()), None);
    assert_eq!(code, 200, "{body}");
    let site_id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"].clone();
    let settings = serde_json::json!({"queue_concurrency":1,"connections":1,"download_dir":temp.join("downloads")});
    assert_eq!(
        gui.call("POST", "/api/settings", Some(&settings.to_string()), None)
            .0,
        200
    );
    let queue = serde_json::json!({"site_id":site_id,"paths":["/slow.bin","/slow.bin"]});
    // Duplicate paths are deduplicated, so enqueue the second job separately.
    for _ in 0..2 {
        assert_eq!(
            gui.call("POST", "/api/queue", Some(&queue.to_string()), None)
                .0,
            200
        );
    }
    let jobs = gui.state()["jobs"].as_array().unwrap().clone();
    let first = jobs[0]["id"].as_u64().unwrap();
    let second = jobs[1]["id"].as_u64().unwrap();
    assert_eq!(gui.job_action("pause", second), 200);
    assert_eq!(gui.state()["jobs"][1]["status"], "paused");
    assert_eq!(gui.state()["jobs"][1]["downloaded"], 0);
    gui.wait_job(0, |j| j["downloaded"].as_u64().unwrap_or(0) >= 32768)
        .await;
    assert_eq!(gui.job_action("pause", first), 200);
    // Resume must not overlap the old writer: pausing retains the cancel marker.
    let state = gui.state();
    if state["jobs"][0]["status"] == "pausing" {
        assert_eq!(gui.job_action("resume", first), 400);
    }
    let paused = gui.wait_job(0, |j| j["status"] == "paused").await;
    let local = std::path::PathBuf::from(paused["local"].as_str().unwrap());
    let store = ftpseg::state::StateStore::for_download(&local);
    assert_eq!(
        std::fs::metadata(&local).unwrap().len(),
        expected.len() as u64
    );
    assert!(store.path().is_file());
    let checkpoint = std::fs::read(store.path()).unwrap();
    let downloaded = paused["downloaded"].as_u64().unwrap();
    assert!(downloaded > 0 && downloaded < expected.len() as u64);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(gui.state()["jobs"][0]["downloaded"], downloaded);
    assert_eq!(std::fs::read(store.path()).unwrap(), checkpoint);
    assert_eq!(gui.job_action("retry", first), 200); // paused retry == resume
    let done = gui.wait_job(0, |j| j["status"] == "done").await;
    assert_eq!(done["downloaded"], expected.len() as u64);
    assert_eq!(std::fs::read(&local).unwrap(), expected);
    assert_eq!(gui.job_action("pause", first), 200); // completion protection
    assert_eq!(gui.job_action("cancel", first), 200);
    assert_eq!(gui.state()["jobs"][0]["status"], "done");
    assert!(local.exists());

    assert_eq!(gui.job_action("resume", second), 200);
    gui.wait_job(1, |j| j["downloaded"].as_u64().unwrap_or(0) >= 32768)
        .await;
    assert_eq!(gui.job_action("pause", second), 200);
    let paused = gui.wait_job(1, |j| j["status"] == "paused").await;
    let partial = std::path::PathBuf::from(paused["local"].as_str().unwrap());
    let store = ftpseg::state::StateStore::for_download(&partial);
    assert!(partial.is_file() && store.path().is_file());
    assert_eq!(gui.job_action("cancel", second), 200);
    gui.wait_job(1, |j| j["status"] == "cancelled").await;
    assert!(!partial.exists() && !store.path().exists());

    // Cancel supersedes pausing, whether the writers stop before or after the API call.
    assert_eq!(
        gui.call("POST", "/api/queue", Some(&queue.to_string()), None)
            .0,
        200
    );
    gui.wait_job(2, |j| j["downloaded"].as_u64().unwrap_or(0) >= 32768)
        .await;
    let third = gui.state()["jobs"][2]["id"].as_u64().unwrap();
    assert_eq!(gui.job_action("pause", third), 200);
    assert_eq!(gui.job_action("cancel", third), 200);
    let cancelled = gui.wait_job(2, |j| j["status"] == "cancelled").await;
    assert!(!std::path::Path::new(cancelled["local"].as_str().unwrap()).exists());

    // Paused queues may exit, but restart creates a new identity and empty queue.
    assert_eq!(
        gui.call("POST", "/api/queue", Some(&queue.to_string()), None)
            .0,
        200
    );
    let fourth = gui.state()["jobs"][3]["id"].as_u64().unwrap();
    assert_eq!(gui.job_action("pause", fourth), 200);
    gui.wait_job(3, |j| j["status"] == "paused").await;
    assert_eq!(gui.call("POST", "/api/shutdown", Some("{}"), None).0, 200);
    drop(gui);
    let gui = Gui::start_with_config(Some(&config));
    assert_ne!(gui.state()["instance_id"], instance);
    assert!(gui.state()["jobs"].as_array().unwrap().is_empty());
    drop(gui);
    std::fs::remove_dir_all(temp).unwrap();
}

mod common;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Gui {
    child: Child,
    port: u16,
}
impl Drop for Gui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Gui {
    fn state(&self) -> serde_json::Value {
        let (code, body) = self.call("GET", "/api/state", None, None);
        assert_eq!(code, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    }
    fn job_action(&self, action: &str, id: u64) -> u16 {
        self.call(
            "POST",
            &format!("/api/jobs/{action}"),
            Some(&serde_json::json!({"id":id}).to_string()),
            None,
        )
        .0
    }
    async fn wait_job(
        &self,
        index: usize,
        predicate: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        for _ in 0..300 {
            let job = self.state()["jobs"][index].clone();
            assert_ne!(job["status"], "failed", "{job}");
            if predicate(&job) {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("job {index} timed out: {}", self.state()["jobs"][index]);
    }
    fn start() -> Self {
        Self::start_with_config(None)
    }
    fn start_with_config(config: Option<&std::path::Path>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let config = config.map(std::path::Path::to_path_buf).unwrap_or_else(|| {
            std::env::temp_dir().join(format!(
                "ftpseg-gui-test-{}-{port}.json",
                std::process::id()
            ))
        });
        let child = Command::new(env!("CARGO_BIN_EXE_ftpseg"))
            .args(["gui", "--port", &port.to_string()])
            .env("FTPSEG_GUI_CONFIG", config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let gui = Self { child, port };
        for _ in 0..80 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return gui;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("GUI did not listen on 127.0.0.1:{port}");
    }
    fn call(
        &self,
        method: &str,
        path: &str,
        data: Option<&str>,
        origin: Option<&str>,
    ) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let body = data.unwrap_or("");
        let origin = origin
            .map(|s| format!("Origin: {s}\r\n"))
            .unwrap_or_default();
        let request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{origin}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", self.port, body.len());
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let code = response.split_whitespace().nth(1).unwrap().parse().unwrap();
        (code, response.split_once("\r\n\r\n").unwrap().1.to_owned())
    }
}

#[test]
fn site_settings_queue_and_origin_protection() {
    // Hold an FTP probe open so shutdown protection does not race an immediate
    // connection-refused failure (notably fast on macOS loopback).
    let ftp_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let gui = Gui::start();
    let (status, html) = gui.call("GET", "/", None, None);
    assert_eq!(status, 200);
    assert!(html.contains("下载队列"));
    for expected in [
        "全部",
        "正在下载",
        "待下载",
        "已完成",
        "失败/取消",
        "总共数据",
        "已下载数据",
        "实时下载速度",
        "remoteToggle",
        "queueToggle",
        "aria-expanded",
        "exitBtn",
        "closedView",
        "应用已关闭",
        "reconnectBtn",
    ] {
        assert!(html.contains(expected), "missing GUI feature: {expected}");
    }
    let (status, body) = gui.call("GET", "/api/state", None, None);
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["sites"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let site = serde_json::json!({"name":"Local","host":"127.0.0.1","port":ftp_listener.local_addr().unwrap().port(),"user":"alice","password":"a secret","tls":"none","encoding":"auto","insecure":false}).to_string();
    let (status, body) = gui.call(
        "POST",
        "/api/sites",
        Some(&site),
        Some("http://evil.example"),
    );
    assert_eq!(status, 403, "{body}");
    let (status, body) = gui.call("POST", "/api/sites", Some(&site), None);
    assert_eq!(status, 200, "{body}");
    let site_id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_u64()
        .unwrap();
    assert!(!body.contains("a secret"));
    let (status, body) = gui.call("GET", "/api/state", None, None);
    assert_eq!(status, 200);
    assert!(!body.contains("a secret"));
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["sites"][0]["has_password"], true);
    let (status, body) = gui.call(
        "POST",
        "/api/settings",
        Some(r#"{"queue_concurrency":2,"connections":8,"download_dir":"relative"}"#),
        None,
    );
    assert_eq!(status, 400, "{body}");
    let (status, body) = gui.call(
        "POST",
        "/api/queue",
        Some(&format!(
            r#"{{"site_id":{site_id},"paths":["/../../escape"]}}"#
        )),
        None,
    );
    assert_eq!(status, 400, "{body}");
    let (status, body) = gui.call(
        "POST",
        "/api/queue",
        Some(&format!(r#"{{"site_id":{site_id},"paths":["/foo.bin"]}}"#)),
        None,
    );
    assert_eq!(status, 200, "{body}");
    let (status, body) = gui.call("POST", "/api/shutdown", Some("{}"), None);
    assert_eq!(status, 409, "{body}");
    let job_id =
        serde_json::from_str::<serde_json::Value>(&gui.call("GET", "/api/state", None, None).1)
            .unwrap()["jobs"][0]["id"]
            .as_u64()
            .unwrap();
    let (status, body) = gui.call(
        "POST",
        "/api/jobs/cancel",
        Some(&format!(r#"{{"id":{job_id}}}"#)),
        None,
    );
    assert_eq!(status, 200, "{body}");
    drop(ftp_listener); // Release the stalled probe before waiting for cancellation.
    let (status, body) = gui.call("DELETE", &format!("/api/sites?id={site_id}"), None, None);
    assert_eq!(status, 200, "{body}");
    // Cancellation may still be cleaning up the running FTP probe.
    for _ in 0..100 {
        let (status, _) = gui.call("POST", "/api/shutdown", Some("{}"), None);
        if status == 200 {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("GUI did not exit after cancellation");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lists_and_downloads_real_files_through_gui_queue() {
    let mut files = common::Vfs::new();
    let expected = vec![42u8; 256 * 1024];
    files.add("/pub/sample.bin", expected.clone());
    let ftp = common::TestServer::start(files, common::ServerConfig::new()).await;
    let temp = common::temp_dir("gui-download");
    let gui = Gui::start_with_config(Some(&temp.join("gui.json")));
    let site = serde_json::json!({"name":"Test FTP","host":ftp.host(),"port":ftp.port(),"user":"testuser","password":"testpass","tls":"none","encoding":"auto","insecure":false});
    let (status, body) = gui.call("POST", "/api/sites", Some(&site.to_string()), None);
    assert_eq!(status, 200, "{body}");
    let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_u64()
        .unwrap();
    let (status, body) = gui.call(
        "GET",
        &format!("/api/list?site_id={id}&path=%2Fpub"),
        None,
        None,
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("sample.bin"));
    let settings = serde_json::json!({"queue_concurrency":2,"connections":3,"download_dir":temp.join("downloads").to_string_lossy()});
    let (status, body) = gui.call("POST", "/api/settings", Some(&settings.to_string()), None);
    assert_eq!(status, 200, "{body}");
    let queue = serde_json::json!({"site_id":id,"paths":["/pub/sample.bin"]});
    let (status, body) = gui.call("POST", "/api/queue", Some(&queue.to_string()), None);
    assert_eq!(status, 200, "{body}");
    let job_id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["ids"][0]
        .as_u64()
        .unwrap();
    let mut completed = false;
    for _ in 0..150 {
        let (status, body) = gui.call("GET", "/api/state", None, None);
        assert_eq!(status, 200);
        let state: serde_json::Value = serde_json::from_str(&body).unwrap();
        let job = &state["jobs"][0];
        if job["status"] == "failed" {
            panic!("GUI download failed: {job}");
        }
        if job["status"] == "done" {
            assert_eq!(job["downloaded"], expected.len() as u64);
            assert_eq!(job["total"], expected.len() as u64);
            completed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(completed, "GUI queue did not complete");
    assert_eq!(
        std::fs::read(temp.join("downloads").join(format!("{job_id}-sample.bin"))).unwrap(),
        expected
    );
    drop(gui);
    std::fs::remove_dir_all(temp).unwrap();
}
