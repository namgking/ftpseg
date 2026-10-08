//! Resolve the HTTP association without starting Explorer or a command shell.
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::UI::Shell::CommandLineToArgvW;
use windows_sys::Win32::UI::Shell::{AssocQueryStringW, ASSOCSTR_COMMAND};

pub(super) fn default_browser_command(url: &str) -> Result<(String, Vec<String>), String> {
    let association: Vec<u16> = "http".encode_utf16().chain(Some(0)).collect();
    let verb: Vec<u16> = "open".encode_utf16().chain(Some(0)).collect();
    let mut length = 0;
    unsafe {
        AssocQueryStringW(
            0,
            ASSOCSTR_COMMAND,
            association.as_ptr(),
            verb.as_ptr(),
            std::ptr::null_mut(),
            &mut length,
        )
    };
    if length == 0 || length > 32768 {
        return Err("HTTP 关联没有可直接启动的命令".into());
    }
    let mut command = vec![0u16; length as usize];
    let hr = unsafe {
        AssocQueryStringW(
            0,
            ASSOCSTR_COMMAND,
            association.as_ptr(),
            verb.as_ptr(),
            command.as_mut_ptr(),
            &mut length,
        )
    };
    if hr < 0 {
        return Err(format!("读取 HTTP 关联失败 0x{:08X}", hr as u32));
    }
    let mut count = 0;
    let argv = unsafe { CommandLineToArgvW(command.as_ptr(), &mut count) };
    if argv.is_null() {
        return Err("默认浏览器命令无法解析".into());
    }
    struct Guard(*mut *mut u16);
    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe { LocalFree(self.0.cast()) };
        }
    }
    let _guard = Guard(argv);
    let mut parts = Vec::new();
    for index in 0..count as usize {
        let ptr = unsafe { *argv.add(index) };
        let mut len = 0;
        while unsafe { *ptr.add(len) } != 0 {
            len += 1;
        }
        parts.push(String::from_utf16_lossy(unsafe {
            std::slice::from_raw_parts(ptr, len)
        }));
    }
    prepare(parts, url)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_arguments_and_replaces_url_without_shell() {
        let exe = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let (_, args) = prepare(
            vec![exe.clone(), "--single-argument".into(), "%1".into()],
            "http://127.0.0.1:17721/",
        )
        .unwrap();
        assert_eq!(args, ["--single-argument", "http://127.0.0.1:17721/"]);
        let (_, args) = prepare(vec![exe], "http://127.0.0.1:17721/").unwrap();
        assert_eq!(args, ["http://127.0.0.1:17721/"]);
        assert!(prepare(vec![], "url").is_err());
        assert!(prepare(vec!["browser.exe".into()], "url").is_err());
    }
}

fn prepare(mut parts: Vec<String>, url: &str) -> Result<(String, Vec<String>), String> {
    if parts.is_empty() {
        return Err("默认浏览器命令为空".into());
    }
    let program = parts.remove(0);
    if !std::path::Path::new(&program).is_absolute() || !std::path::Path::new(&program).is_file() {
        return Err("默认浏览器程序路径无效".into());
    }
    let name = std::path::Path::new(&program)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    if ["explorer.exe", "cmd.exe", "powershell.exe", "rundll32.exe"].contains(&name.as_str()) {
        return Err("默认关联不是可直接启动的浏览器".into());
    }
    let mut replaced = false;
    for arg in &mut parts {
        for placeholder in ["%1", "%l", "%L", "%u", "%U"] {
            if arg.contains(placeholder) {
                *arg = arg.replace(placeholder, url);
                replaced = true;
            }
        }
    }
    if !replaced {
        parts.push(url.into());
    }
    Ok((program, parts))
}
