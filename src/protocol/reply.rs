//! Parsing and formatting of FTP replies (RFC 959 section 4.2).
//!
//! A reply is either one line
//!
//! ```text
//! 213 1234567
//! ```
//!
//! or several lines, where every line but the last uses `-` after the code:
//!
//! ```text
//! 211-Features:
//!  SIZE
//!  REST STREAM
//! 211 End
//! ```

/// One complete server reply: a three digit code plus its text lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub code: u16,
    pub lines: Vec<String>,
}

impl Reply {
    pub fn new(code: u16, lines: Vec<String>) -> Self {
        Self { code, lines }
    }

    /// Split a raw line into its code and whether more lines follow.
    ///
    /// Returns `None` when the line is not a well-formed reply.
    pub fn parse_code(line: &str) -> Option<(u16, bool)> {
        let bytes = line.as_bytes();
        if bytes.len() < 3 || !bytes[..3].iter().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let code = line[..3].parse::<u16>().ok()?;
        let more = matches!(bytes.get(3), Some(b'-'));
        Some((code, more))
    }

    /// The reply code class: 1, 2, 3, 4 or 5.
    pub fn class(&self) -> u16 {
        self.code / 100
    }

    /// 1xx: the transfer is starting.
    pub fn is_intermediate(&self) -> bool {
        self.class() == 1
    }

    /// 2xx: success.
    pub fn is_positive(&self) -> bool {
        self.class() == 2
    }

    /// 3xx: needs more input from us.
    pub fn is_more_info(&self) -> bool {
        self.class() == 3
    }

    /// 4xx/5xx.
    pub fn is_negative(&self) -> bool {
        self.class() >= 4
    }

    /// `true` when the server rejected the command as unknown/unimplemented.
    ///
    /// This is how we probe for optional features without treating them as
    /// hard failures.
    pub fn is_not_implemented(&self) -> bool {
        matches!(self.code, 500 | 501 | 502 | 504)
    }

    /// The first line with the code prefix stripped, e.g. `Features:`.
    pub fn first_text(&self) -> String {
        let raw = self.lines.first().map(String::as_str).unwrap_or("");
        strip_code_prefix(raw)
    }

    /// All lines with code prefixes stripped, joined by `; `.
    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(|l| strip_code_prefix(l))
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Feature names from a `211` FEAT reply, one per line.
    pub fn feature_lines(&self) -> Vec<String> {
        // The first line is the header ("211-Features:") and the last is the
        // terminator ("211 End"), so take the middle.
        let mut lines = self.lines.clone();
        if lines.len() >= 2 {
            lines.remove(0);
            lines.pop();
        }
        lines
            .into_iter()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && !l.starts_with("211"))
            .collect()
    }

    /// Turn a negative reply into an [`crate::error::Error`].
    pub fn into_error(self) -> crate::error::Error {
        if self.code == 550 {
            crate::error::Error::NotFound(self.text())
        } else {
            crate::error::Error::server(self.code, self.text())
        }
    }
}

/// Drop a leading `NNN ` or `NNN-` reply code, leaving the text behind.
///
/// Lines that do not start with a code are returned trimmed and unchanged, so
/// this is safe to call on a value that was already stripped.
pub fn strip_code_prefix(line: &str) -> String {
    let bytes = line.as_bytes();
    if bytes.len() >= 4
        && bytes[..3].iter().all(|b| b.is_ascii_digit())
        && (bytes[3] == b' ' || bytes[3] == b'-')
    {
        line[4..].trim().to_string()
    } else {
        line.trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_line_reply() {
        let (code, more) = Reply::parse_code("213 1234567").unwrap();
        assert_eq!(code, 213);
        assert!(!more);
    }

    #[test]
    fn parses_multiline_reply() {
        let (code, more) = Reply::parse_code("211-Features:").unwrap();
        assert_eq!(code, 211);
        assert!(more);
    }

    #[test]
    fn rejects_garbage() {
        assert!(Reply::parse_code("hello there").is_none());
        assert!(Reply::parse_code("21 3").is_none());
        assert!(Reply::parse_code("").is_none());
    }

    #[test]
    fn classifies_codes() {
        let r = Reply::new(150, vec!["150 Opening data connection".into()]);
        assert!(r.is_intermediate());
        let r = Reply::new(226, vec!["226 Transfer complete".into()]);
        assert!(r.is_positive());
        let r = Reply::new(550, vec!["550 No such file".into()]);
        assert!(r.is_negative());
        assert!(!r.is_not_implemented());
        assert!(Reply::new(502, vec!["502 nope".into()]).is_not_implemented());
    }

    #[test]
    fn extracts_feature_names() {
        let r = Reply::new(
            211,
            vec![
                "211-Features:".into(),
                " SIZE".into(),
                " REST STREAM".into(),
                " UTF8".into(),
                "211 End".into(),
            ],
        );
        assert_eq!(r.feature_lines(), vec!["SIZE", "REST STREAM", "UTF8"]);
    }

    #[test]
    fn strips_code_prefix_from_text() {
        let r = Reply::new(550, vec!["550 /x/y: No such file or directory".into()]);
        assert_eq!(r.text(), "/x/y: No such file or directory");
    }
}
