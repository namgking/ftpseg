use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=assets/ftpseg.ico");
    println!("cargo:rerun-if-env-changed=FTPSEG_RESOURCE_COMPILER");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let icon = root
        .join("assets/ftpseg.ico")
        .to_string_lossy()
        .replace('\\', "/");
    let rc = out.join("ftpseg.rc");
    fs::write(&rc, format!("1 ICON \"{icon}\"\n")).expect("write icon resource script");
    let gnu = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu");
    let resource = out.join(if gnu {
        "ftpseg-icon.o"
    } else {
        "ftpseg-icon.res"
    });
    let compiler = env::var_os("FTPSEG_RESOURCE_COMPILER").unwrap_or_else(|| {
        if gnu {
            let name = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
                Ok("x86") => "i686-w64-mingw32-windres.exe",
                _ => "x86_64-w64-mingw32-windres.exe",
            };
            let local = root.join(".rust/mingw/w64devkit/bin").join(name);
            if local.is_file() {
                local.into_os_string()
            } else {
                "windres".into()
            }
        } else {
            "rc.exe".into()
        }
    });
    let mut command = Command::new(compiler);
    if gnu {
        command
            .arg("--input")
            .arg(&rc)
            .arg("--output")
            .arg(&resource)
            .args(["--output-format", "coff"]);
    } else {
        command
            .arg("/nologo")
            .arg(format!("/fo{}", resource.display()))
            .arg(&rc);
    }
    let status = command
        .status()
        .expect("run Windows resource compiler (windres or Windows SDK rc.exe required)");
    assert!(status.success(), "Windows icon resource compilation failed");
    for bin in ["ftpseg", "ftpseg-gui"] {
        println!("cargo:rustc-link-arg-bin={bin}={}", resource.display());
    }
}
