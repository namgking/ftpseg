//! Double-clickable launcher for the local GUI; no terminal or CLI arguments.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(windows)]
mod browser_windows;
#[cfg(windows)]
use browser_windows::default_browser_command;

const PORT: u16 = 17721;

fn main() {
    if let Err(error) = run() {
        report_error(&format!("无法启动 ftpseg GUI：{error}"));
    }
}

fn run() -> Result<(), String> {
    let url = format!("http://127.0.0.1:{PORT}/");
    // Reuse the running app instead of starting another process that might
    // concurrently modify the same site configuration or download directory.
    if TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, PORT)),
        Duration::from_millis(350),
    )
    .is_ok()
    {
        if !is_our_server(PORT) {
            return Err(format!(
                "端口 {PORT} 已由其他程序占用，请关闭占用程序后重试"
            ));
        }
        return open_browser(&url);
    }
    run_server()
}

fn run_server() -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime
        .block_on(ftpseg::gui::serve_with_ready(
            SocketAddr::from((Ipv4Addr::LOCALHOST, PORT)),
            |address| {
                if let Err(error) = open_browser(&format!("http://{address}/")) {
                    report_error(&format!(
                        "服务已启动，但无法自动打开浏览器：{error}\n请手动访问 http://{address}/"
                    ));
                }
            },
        ))
        .map_err(|e| e.to_string())
}

fn is_our_server(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        Duration::from_millis(350),
    ) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    if stream
        .write_all(
            format!(
                "GET /api/state HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .is_err()
    {
        return false;
    }
    let mut response = Vec::new();
    let mut chunk = [0u8; 512];
    while response.len() < 8192 {
        let Ok(n) = stream.read(&mut chunk) else {
            return false;
        };
        if n == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..n]);
        if response.windows(4).any(|part| part == b"\r\n\r\n") {
            break;
        }
    }
    let headers = String::from_utf8_lossy(&response);
    headers.starts_with("HTTP/1.1 200 ") && headers.contains("X-Ftpseg-Gui: 1\r\n")
}

#[cfg(windows)]
fn open_browser(url: &str) -> Result<(), String> {
    match default_browser_command(url) {
        Ok((program, args)) => Command::new(&program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("无法启动默认浏览器 {program}：{e}")),
        Err(error) => open_browser_shell(url)
            .map_err(|shell| format!("无法解析默认浏览器：{error}；备用调用失败：{shell}")),
    }
}

#[cfg(windows)]
fn open_browser_shell(url: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let verb: Vec<u16> = "open".encode_utf16().chain(Some(0)).collect();
    let target: Vec<u16> = std::ffi::OsStr::new(url)
        .encode_wide()
        .chain(Some(0))
        .collect();
    // Some Windows 11 browser associations use COM activation. Initialize this
    // calling thread and wait for shell handoff, especially for a reused launcher
    // that exits immediately after opening the URL.
    let hr = unsafe {
        CoInitializeEx(
            std::ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    };
    if hr < 0 {
        return Err(format!(
            "浏览器 COM 初始化失败（HRESULT 0x{:08X}）",
            hr as u32
        ));
    }
    struct ComGuard;
    impl Drop for ComGuard {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }
    let _com = ComGuard;
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
    info.lpVerb = verb.as_ptr();
    info.lpFile = target.as_ptr();
    info.nShow = SW_SHOWNORMAL;
    if unsafe { ShellExecuteExW(&mut info) } != 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        Err(format!(
            "{}；Win32：{error}",
            shell_error(info.hInstApp as isize)
        ))
    }
}

#[cfg(windows)]
fn shell_error(code: isize) -> String {
    let reason = match code {
        0 | 8 => "系统内存或资源不足",
        2 => "找不到默认浏览器程序",
        3 => "默认浏览器程序路径不存在",
        5 => "系统或安全策略拒绝打开浏览器",
        26 => "浏览器文件正在被占用",
        27 | 31 => "HTTP 协议未关联有效浏览器，请在 Windows 设置 → 应用 → 默认应用中设置默认浏览器",
        28 | 29 | 30 => "无法与默认浏览器通信",
        32 => "默认浏览器依赖文件缺失",
        _ => "Windows 无法打开默认浏览器",
    };
    format!("{reason}（ShellExecute 错误代码 {code}）")
}

#[cfg(not(windows))]
fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(url);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn shell_errors_report_browser_association_and_policy() {
        assert!(shell_error(31).contains("默认应用"));
        assert!(shell_error(5).contains("安全策略"));
        assert!(shell_error(2).contains("找不到"));
    }
}

#[cfg(windows)]
fn report_error(message: &str) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let text: Vec<u16> = std::ffi::OsStr::new(message)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let title: Vec<u16> = std::ffi::OsStr::new("ftpseg GUI")
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn report_error(message: &str) {
    eprintln!("{message}");
}
