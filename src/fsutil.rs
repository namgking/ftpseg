//! Local file helpers: positioned writes, preallocation and human-readable
//! size parsing.
//!
//! Segments are written concurrently, so every worker must be able to write at
//! an absolute offset without disturbing the others. That rules out a shared
//! cursor; Windows and Unix each expose the right primitive under a different
//! name, which is what this module hides.

use std::fs::File;
use std::io;
use std::path::Path;

use crate::error::{Error, Result};

/// Write the whole buffer at `offset`, independently of any file cursor.
#[cfg(unix)]
pub fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

/// Write the whole buffer at `offset`, independently of any file cursor.
#[cfg(windows)]
pub fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut written = 0usize;
    while written < buf.len() {
        // `seek_write` is allowed to write less than asked, so loop.
        let n = file.seek_write(&buf[written..], offset + written as u64)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write segment data",
            ));
        }
        written += n;
    }
    Ok(())
}

/// Fallback for platforms without a positioned-write extension.
#[cfg(not(any(unix, windows)))]
pub fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut cloned = file.try_clone()?;
    cloned.seek(SeekFrom::Start(offset))?;
    cloned.write_all(buf)
}

/// Grow the file to its final size up front.
///
/// This is best-effort: a failure only means the filesystem does not support
/// preallocation, which costs fragmentation but never correctness.
pub fn preallocate(file: &File, size: u64) -> io::Result<()> {
    if size == 0 {
        return Ok(());
    }
    if file.metadata()?.len() >= size {
        return Ok(());
    }
    file.set_len(size)
}

/// Open (or create) the destination file for concurrent positioned writes.
pub fn open_destination(path: &Path, truncate: bool) -> Result<File> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(truncate)
        .open(path)?;
    Ok(file)
}

/// Parse `1024`, `64K`, `8M`, `1G`, `2TB` (case insensitive, optional `B`/`iB`).
///
/// Deliberately decimal-free: FTP tools speak powers of two, so `K` is 1024.
pub fn parse_size(input: &str) -> Result<u64> {
    let text = input.trim();
    if text.is_empty() {
        return Err(Error::Config("empty size value".into()));
    }

    let digits_end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    if digits_end == 0 {
        return Err(Error::Config(format!("invalid size: {input}")));
    }
    let number: u64 = text[..digits_end]
        .parse()
        .map_err(|_| Error::Config(format!("invalid size: {input}")))?;

    let suffix = text[digits_end..].trim().to_ascii_uppercase();
    let suffix = suffix.strip_suffix('B').unwrap_or(&suffix);
    let suffix = suffix.strip_suffix('I').unwrap_or(suffix);

    let multiplier: u64 = match suffix {
        "" => 1,
        "K" => 1024,
        "M" => 1024 * 1024,
        "G" => 1024 * 1024 * 1024,
        "T" => 1024 * 1024 * 1024 * 1024,
        other => {
            return Err(Error::Config(format!(
                "unknown size suffix {other:?} (use K, M, G or T)"
            )))
        }
    };

    number
        .checked_mul(multiplier)
        .ok_or_else(|| Error::Config(format!("size out of range: {input}")))
}

/// Render a byte count the way a download tool should.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

/// Bytes per second, for progress output.
pub fn human_rate(bytes_per_sec: f64) -> String {
    format!("{}/s", human_bytes(bytes_per_sec.max(0.0) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_suffixed_sizes() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("64K").unwrap(), 64 * 1024);
        assert_eq!(parse_size("64KB").unwrap(), 64 * 1024);
        assert_eq!(parse_size("8m").unwrap(), 8 * 1024 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size(" 2 GiB ").unwrap(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn rejects_nonsense_sizes() {
        assert!(parse_size("").is_err());
        assert!(parse_size("K").is_err());
        assert!(parse_size("12X").is_err());
        assert!(parse_size("99999999999999999999").is_err());
    }

    #[test]
    fn formats_human_readable_sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.00 KiB");
        assert_eq!(human_bytes(1536), "1.50 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.00 MiB");
    }

    #[test]
    fn writes_at_absolute_offsets() {
        let dir = std::env::temp_dir().join(format!("ftpseg-fsutil-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("positioned.bin");
        let file = open_destination(&path, true).unwrap();
        preallocate(&file, 10).unwrap();
        write_at(&file, b"hello", 0).unwrap();
        write_at(&file, b"world", 5).unwrap();
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), b"helloworld");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
