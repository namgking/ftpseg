"""Fetch a compact MinGW-w64 toolchain into .rust/mingw.

Rust's windows-gnu host ships gcc, ld and dlltool but no assembler, and
dlltool needs `as` to turn its generated assembly into an import library for
every `raw-dylib` link (tokio, clap and native-tls all use those). This script
pulls in w64devkit, a small portable MinGW-w64 distribution that provides the
missing `as.exe`, and unpacks it inside the workspace.

The project publishes a plain .zip on some releases and a 7-Zip self-extracting
.7z.exe on others, so both shapes are handled.
"""

import json
import os
import shutil
import subprocess
import sys
import urllib.request
import zipfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ARCHIVE_DIR = os.path.join(ROOT, ".rust")
DEST = os.path.join(ROOT, ".rust", "mingw")
API = "https://api.github.com/repos/skeeto/w64devkit/releases/latest"
HEADERS = {"User-Agent": "ftpseg-setup"}
WANTED = ("as.exe", "dlltool.exe", "gcc.exe", "ld.exe")


def fetch_json(url):
    request = urllib.request.Request(url, headers=HEADERS)
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)


def pick_asset(assets):
    """Prefer a plain zip, otherwise the 64-bit self-extracting 7z."""
    for asset in assets:
        name = asset["name"]
        if name.startswith("w64devkit-") and name.endswith(".zip"):
            return asset, "zip"
    for asset in assets:
        name = asset["name"]
        if name.startswith("w64devkit-x64") and name.endswith(".7z.exe"):
            return asset, "sfx"
    return None, None


def download(asset):
    path = os.path.join(ARCHIVE_DIR, asset["name"])
    print(
        "downloading {0} ({1:.1f} MiB)".format(asset["name"], asset["size"] / 1048576),
        flush=True,
    )
    request = urllib.request.Request(asset["browser_download_url"], headers=HEADERS)
    with urllib.request.urlopen(request, timeout=1800) as response:
        with open(path, "wb") as handle:
            shutil.copyfileobj(response, handle, 256 * 1024)
    print("download complete:", os.path.getsize(path), "bytes", flush=True)
    return path


def collect_found():
    found = {}
    for dirpath, _dirnames, filenames in os.walk(DEST):
        for name in set(WANTED) & set(filenames):
            found.setdefault(name, os.path.join(dirpath, name))
    return found


def ensure_libgcc_eh(root):
    """Give the toolchain the `libgcc_eh.a` that Rust's std asks for.

    rustc always passes `-lgcc_eh` when linking for windows-gnu, but modern
    MinGW-w64 folded the unwinder into `libgcc.a` and no longer ships a separate
    `libgcc_eh.a`, so the link fails with "cannot find -lgcc_eh". An empty
    archive satisfies the reference: rustc also passes `-lgcc`, which is where
    `_Unwind_Resume` and friends actually come from.
    """
    lib_dir = os.path.join(root, "lib")
    if not os.path.isdir(lib_dir):
        return None

    target = os.path.join(lib_dir, "libgcc_eh.a")
    if os.path.exists(target):
        return target

    ar = os.path.join(root, "bin", "ar.exe")
    if os.path.exists(ar):
        subprocess.run([ar, "rcs", target], check=False)
    if not os.path.exists(target):
        # `!<arch>\n` on its own is a valid empty archive.
        with open(target, "wb") as handle:
            handle.write(b"!<arch>\n")
    print("created linker shim:", target, flush=True)
    return target


def main():
    # Re-running must be cheap: skip the download when the toolchain is already
    # unpacked and only re-check the linker shim.
    already = collect_found()
    if set(WANTED) <= set(already):
        print("MinGW-w64 already present in", DEST, flush=True)
        root = os.path.dirname(os.path.dirname(already["as.exe"]))
        ensure_libgcc_eh(root)
        for name in WANTED:
            print("FOUND  ", name, already[name])
        return 0

    release = fetch_json(API)
    assets = release.get("assets", [])
    print("release:", release["tag_name"], flush=True)

    asset, kind = pick_asset(assets)
    if asset is None:
        print("no usable asset; available:", [a["name"] for a in assets])
        return 1

    os.makedirs(ARCHIVE_DIR, exist_ok=True)
    archive = download(asset)

    if os.path.isdir(DEST):
        shutil.rmtree(DEST)
    os.makedirs(DEST, exist_ok=True)

    if kind == "zip":
        with zipfile.ZipFile(archive) as handle:
            handle.extractall(DEST)
        print("extracted zip to", DEST, flush=True)
    else:
        # 7-Zip self-extracting archive: arguments are forwarded to the
        # embedded 7z module, so `-o` chooses the destination.
        print("running self-extractor into", DEST, flush=True)
        result = subprocess.run([archive, "-y", "-o" + DEST])
        if result.returncode != 0:
            print("self-extractor exited with", result.returncode, flush=True)
            return 1

    found = collect_found()
    for name in WANTED:
        print("FOUND  " if name in found else "MISSING", name, found.get(name, ""))

    if "as.exe" in found:
        root = os.path.dirname(os.path.dirname(found["as.exe"]))
        ensure_libgcc_eh(root)

    return 0 if set(WANTED) <= set(found) else 1


if __name__ == "__main__":
    sys.exit(main())
