//! Directory listing parsing.
//!
//! `MLSD` (RFC 3659) is machine readable and is always preferred. `LIST` output
//! has no standard format at all, so the fallback parser understands the two
//! shapes that cover virtually every real server: the Unix `ls -l` style and
//! the MS-DOS style.

/// One entry from a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    /// `YYYYMMDDHHMMSS`, when the server reports it.
    pub modify: Option<String>,
}

impl DirEntry {
    fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            is_dir: false,
            size: None,
            modify: None,
        }
    }
}

/// Parse one `MLSD` line: `fact=value;fact=value; name`.
pub fn parse_mlsd_line(line: &str) -> Option<DirEntry> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.trim().is_empty() {
        return None;
    }
    let idx = line.find(' ')?;
    let (facts, name) = (&line[..idx], line[idx + 1..].trim());
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }

    let mut entry = DirEntry::new(name);
    for fact in facts.split(';') {
        if fact.is_empty() {
            continue;
        }
        let (key, value) = match fact.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        match key.to_ascii_lowercase().as_str() {
            "type" => match value.to_ascii_lowercase().as_str() {
                "dir" => entry.is_dir = true,
                // `cdir`/`pdir` are the "." and ".." aliases; treat them as dirs
                "cdir" | "pdir" => entry.is_dir = true,
                _ => {}
            },
            "size" => entry.size = value.parse::<u64>().ok(),
            "modify" => entry.modify = Some(value.to_string()),
            _ => {}
        }
    }
    Some(entry)
}

/// Parse one `LIST` line, Unix or MS-DOS flavour.
pub fn parse_list_line(line: &str) -> Option<DirEntry> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.trim().is_empty() {
        return None;
    }

    // Total line from `ls -l` style servers.
    if line.starts_with("total ") {
        return None;
    }

    match parse_dos_line(line) {
        Some(entry) => Some(entry),
        None => parse_unix_line(line),
    }
}

/// `-rw-r--r--   1 owner group     1234 Jan  2 03:04 name`
fn parse_unix_line(line: &str) -> Option<DirEntry> {
    let mut parts = line.split_whitespace();
    let perms = parts.next()?;
    if perms.len() < 10 {
        return None;
    }
    let kind = perms.as_bytes()[0];
    if !matches!(kind, b'-' | b'd' | b'l' | b'b' | b'c' | b'p' | b's' | b'D') {
        return None;
    }
    // Seven leading fields before the name: perms links owner group size
    // month day time-or-year.
    let rest: Vec<&str> = parts.collect();
    if rest.len() < 6 {
        return None;
    }
    let size = rest[3].parse::<u64>().ok();

    // The name is everything after the date field, so re-split positionally to
    // keep spaces inside file names. Six fields follow the permissions (links,
    // owner, group, size, month, day) and then the time-or-year makes seven.
    let after_perms = line.trim_start().split_once(char::is_whitespace)?.1;
    let mut fields = after_perms.split_whitespace();
    for _ in 0..7 {
        fields.next()?;
    }
    let name = join_remaining(&mut fields)?;
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }

    let mut entry = DirEntry::new(name);
    entry.is_dir = kind == b'd';
    entry.size = size;
    Some(entry)
}

/// `01-02-24  03:04PM       <DIR>          name` or `01-02-24  03:04PM            1234 name`
fn parse_dos_line(line: &str) -> Option<DirEntry> {
    let trimmed = line.trim_start();
    let first = trimmed.split_whitespace().next()?;
    // Expect MM-DD-YY or MM-DD-YYYY.
    let date_parts: Vec<&str> = first.split('-').collect();
    if date_parts.len() != 3
        || !date_parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    let mut fields = trimmed.split_whitespace();
    fields.next()?; // date
    fields.next()?; // time
    let third = fields.next()?;

    let (is_dir, size) = if third.eq_ignore_ascii_case("<DIR>") {
        (true, None)
    } else {
        (false, third.parse::<u64>().ok())
    };

    let name = join_remaining(&mut fields)?;
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    let mut entry = DirEntry::new(name);
    entry.is_dir = is_dir;
    entry.size = size;
    Some(entry)
}

/// Join the remaining whitespace-separated fields back into a file name.
///
/// `LIST` does not quote names, so the only way to keep spaces is to stop
/// splitting after the fixed leading columns.
fn join_remaining(fields: &mut std::str::SplitWhitespace<'_>) -> Option<String> {
    let rest: Vec<&str> = fields.collect();
    if rest.is_empty() {
        None
    } else {
        Some(rest.join(" ").trim().to_string())
    }
}

/// Parse a whole listing body, choosing the parser by `is_mlsd`.
pub fn parse_listing(body: &str, is_mlsd: bool) -> Vec<DirEntry> {
    body.lines()
        .filter_map(|l| {
            if is_mlsd {
                parse_mlsd_line(l)
            } else {
                parse_list_line(l)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mlsd_file() {
        let e = parse_mlsd_line("type=file;size=1234;modify=20240102030405; report.pdf").unwrap();
        assert_eq!(e.name, "report.pdf");
        assert!(!e.is_dir);
        assert_eq!(e.size, Some(1234));
        assert_eq!(e.modify.as_deref(), Some("20240102030405"));
    }

    #[test]
    fn parses_mlsd_dir_and_skips_dot_entries() {
        let e = parse_mlsd_line("type=dir;modify=20240102030405; sub dir").unwrap();
        assert!(e.is_dir);
        assert_eq!(e.name, "sub dir");
        assert!(parse_mlsd_line("type=cdir; .").is_none());
        assert!(parse_mlsd_line("type=pdir; ..").is_none());
    }

    #[test]
    fn parses_unix_list_line() {
        let e = parse_list_line("-rw-r--r--   1 alice staff     1234 Jan  2 03:04 my file.txt")
            .unwrap();
        assert_eq!(e.name, "my file.txt");
        assert!(!e.is_dir);
        assert_eq!(e.size, Some(1234));
    }

    #[test]
    fn parses_unix_list_dir_line() {
        let e =
            parse_list_line("drwxr-xr-x   4 root  wheel      512 Dec 31  2023 archive").unwrap();
        assert!(e.is_dir);
        assert_eq!(e.name, "archive");
    }

    #[test]
    fn parses_dos_list_line() {
        let e = parse_list_line("01-02-24  03:04PM       <DIR>          stuff").unwrap();
        assert!(e.is_dir);
        assert_eq!(e.name, "stuff");

        let e = parse_list_line("01-02-24  03:04PM                 4096 notes.txt").unwrap();
        assert!(!e.is_dir);
        assert_eq!(e.size, Some(4096));
        assert_eq!(e.name, "notes.txt");
    }

    #[test]
    fn ignores_total_and_blank() {
        assert!(parse_list_line("total 42").is_none());
        assert!(parse_list_line("   ").is_none());
        assert!(parse_list_line("nonsense").is_none());
    }

    #[test]
    fn parses_full_listing() {
        let body = "\
type=file;size=10; a.txt\r
type=dir; b\r
";
        let entries = parse_listing(body, true);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "a.txt");
    }
}
