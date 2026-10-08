//! Character set handling for the FTP command channel.
//!
//! FTP predates Unicode, so nothing on the wire is guaranteed to be UTF-8. A
//! server that announces `UTF8` in its `FEAT` reply is telling us it will
//! accept and send UTF-8; a server that says nothing is usually speaking a
//! legacy code page — GBK for a Chinese host, Big5 for a Taiwanese one,
//! Shift_JIS for a Japanese one, and so on.
//!
//! Both directions matter and both are lossy in different ways:
//!
//! * outgoing names and credentials have to be converted *into* the server's
//!   character set, and a character it cannot represent must be reported rather
//!   than silently turned into `?` (the server would just answer `550`);
//! * incoming directory listings have to be converted *out of* it, otherwise
//!   every non-ASCII file name arrives as replacement characters.
//!
//! The chosen charset applies to paths and credentials. Protocol verbs, reply
//! codes and numeric arguments stay ASCII, which every supported charset
//! encodes identically.

use encoding_rs::{Encoding, REPLACEMENT, UTF_8};

use crate::error::{Error, Result};

/// What the user asked for on the command line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CharsetRequest {
    /// Decide from the server's capabilities and, failing that, the local system.
    #[default]
    Auto,
    /// Use exactly this character set, whatever the server advertises.
    Named(String),
}

/// How a character set came to be chosen, for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The server advertised `UTF8` in its `FEAT` reply.
    ServerUtf8,
    /// The user named it with `--encoding`.
    Explicit,
    /// A legacy server, so the local ANSI code page was assumed.
    SystemCodePage(u16),
    /// Nothing better was available; UTF-8 is the default assumption.
    SystemDefaultUtf8,
}

/// A resolved character set, used for every path and credential on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Charset {
    encoding: &'static Encoding,
    origin: Origin,
}

impl Charset {
    /// UTF-8, used until the server's capabilities are known.
    pub const fn utf8() -> Self {
        Self {
            encoding: UTF_8,
            origin: Origin::SystemDefaultUtf8,
        }
    }

    pub fn encoding(self) -> &'static Encoding {
        self.encoding
    }

    /// The canonical name, e.g. `UTF-8`, `GBK`, `windows-1252`.
    pub fn name(self) -> &'static str {
        self.encoding.name()
    }

    pub fn origin(self) -> Origin {
        self.origin
    }

    pub fn is_utf8(self) -> bool {
        self.encoding == UTF_8
    }

    /// A one-line explanation of what is in use and why.
    pub fn describe(self) -> String {
        match self.origin {
            Origin::ServerUtf8 => format!("{} (server advertises UTF8)", self.name()),
            Origin::Explicit => format!("{} (--encoding)", self.name()),
            Origin::SystemCodePage(code_page) => {
                format!("{} (system code page {code_page})", self.name())
            }
            Origin::SystemDefaultUtf8 => format!("{} (default)", self.name()),
        }
    }

    /// Convert text into the bytes the server expects.
    ///
    /// Refuses rather than substitutes: a name the server cannot represent
    /// would come back as `550 File not found`, which is a far worse thing to
    /// debug than a clear message here.
    pub fn encode(self, text: &str) -> Result<Vec<u8>> {
        if self.is_utf8() {
            return Ok(text.as_bytes().to_vec());
        }
        let (encoded, _actual, had_errors) = self.encoding.encode(text);
        if had_errors {
            let character = first_unrepresentable(text, self.encoding).unwrap_or('?');
            return Err(Error::Unrepresentable {
                character,
                charset: self.name().to_string(),
            });
        }
        Ok(encoded.into_owned())
    }

    /// Convert bytes that came from the server back into text.
    pub fn decode(self, bytes: &[u8]) -> String {
        self.encoding
            .decode_without_bom_handling(bytes)
            .0
            .into_owned()
    }
}

/// The first character `encoding` cannot represent, for a useful error message.
fn first_unrepresentable(text: &str, encoding: &'static Encoding) -> Option<char> {
    text.chars().find(|character| {
        let mut buffer = [0u8; 4];
        let (_, _, had_errors) = encoding.encode(character.encode_utf8(&mut buffer));
        had_errors
    })
}

/// Look up a character set by any of its usual names.
///
/// Accepts the WHATWG labels `encoding_rs` knows (`gbk`, `big5`, `shift_jis`,
/// `latin-1`, …) plus the `cp936` / `windows-936` spellings Windows users
/// reach for.
pub fn lookup(label: &str) -> Result<&'static Encoding> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return Err(Error::Config("empty character set name".into()));
    }

    let lower = trimmed.to_ascii_lowercase();

    // `latin-1` is the spelling people actually type, and the one this crate's
    // own messages suggest, but the WHATWG label set only knows `latin1`.
    // WHATWG folds ISO-8859-1 onto windows-1252, which is what browsers and
    // every other FTP client do too.
    if lower == "latin-1" {
        return Ok(Encoding::for_label(b"windows-1252")
            .expect("windows-1252 is part of the WHATWG label set"));
    }

    for prefix in ["cp", "windows-", "windows", "ibm"] {
        if let Some(digits) = lower.strip_prefix(prefix) {
            if let Ok(code_page) = digits.parse::<u16>() {
                if let Some(encoding) = encoding_for_code_page(code_page) {
                    return Ok(encoding);
                }
            }
        }
    }

    match Encoding::for_label(trimmed.as_bytes()) {
        Some(encoding) if encoding != REPLACEMENT => Ok(encoding),
        _ => Err(Error::Config(format!(
            "unknown character set {label:?}; try auto, utf-8, gbk, gb18030, big5, \
             shift_jis, euc-kr, latin-1, or a code page such as cp936"
        ))),
    }
}

/// Map a Windows code page number onto an encoding.
///
/// The East Asian pages are the ones that actually matter for FTP servers;
/// everything else is reached through the WHATWG labels `encoding_rs` accepts.
pub fn encoding_for_code_page(code_page: u16) -> Option<&'static Encoding> {
    let label: &[u8] = match code_page {
        65001 => b"utf-8",
        866 => b"ibm866",
        874 => b"windows-874",
        932 => b"shift_jis",
        936 => b"gbk",
        949 => b"euc-kr",
        950 => b"big5",
        1250 => b"windows-1250",
        1251 => b"windows-1251",
        1252 => b"windows-1252",
        1253 => b"windows-1253",
        1254 => b"windows-1254",
        1255 => b"windows-1255",
        1256 => b"windows-1256",
        1257 => b"windows-1257",
        1258 => b"windows-1258",
        51932 => b"euc-jp",
        54936 => b"gb18030",
        _ => return None,
    };
    Encoding::for_label(label)
}

/// The local ANSI code page, used when a server does not announce UTF-8.
///
/// This mirrors what graphical FTP clients do with their "auto" setting: on a
/// Chinese Windows the code page is 936, so a server that says nothing is
/// assumed to be speaking GBK. macOS and Linux terminals are UTF-8, so there is
/// no legacy page to guess and `None` is returned.
#[cfg(windows)]
fn system_code_page() -> Option<u16> {
    // GetACP takes no arguments and cannot fail; it just reports the code page
    // the process was started with.
    let code_page = unsafe { windows_sys::Win32::Globalization::GetACP() };
    if code_page == 0 {
        None
    } else {
        u16::try_from(code_page).ok()
    }
}

#[cfg(not(windows))]
fn system_code_page() -> Option<u16> {
    None
}

impl CharsetRequest {
    /// Parse and validate a `--encoding` value.
    ///
    /// Validation happens here so that a typo fails immediately, before any
    /// connection is opened.
    pub fn parse(value: &str) -> Result<Self> {
        let trimmed = value.trim();
        if trimmed.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        lookup(trimmed)?;
        Ok(Self::Named(trimmed.to_string()))
    }

    pub fn is_auto(&self) -> bool {
        matches!(self, Self::Auto)
    }

    /// Turn the request into a concrete character set.
    ///
    /// `server_utf8` is what the server's `FEAT` reply said.
    pub fn resolve(&self, server_utf8: bool) -> Result<Charset> {
        match self {
            // An explicit choice always wins, even over a server that claims
            // UTF-8: the user may know something FEAT does not.
            Self::Named(label) => Ok(Charset {
                encoding: lookup(label)?,
                origin: Origin::Explicit,
            }),
            Self::Auto => {
                if server_utf8 {
                    return Ok(Charset {
                        encoding: UTF_8,
                        origin: Origin::ServerUtf8,
                    });
                }
                match system_code_page().and_then(|cp| encoding_for_code_page(cp).map(|e| (cp, e)))
                {
                    Some((code_page, encoding)) => Ok(Charset {
                        encoding,
                        origin: Origin::SystemCodePage(code_page),
                    }),
                    None => Ok(Charset::utf8()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_auto_and_named_requests() {
        assert_eq!(CharsetRequest::parse("auto").unwrap(), CharsetRequest::Auto);
        assert_eq!(
            CharsetRequest::parse("  AUTO ").unwrap(),
            CharsetRequest::Auto
        );
        assert_eq!(
            CharsetRequest::parse("GBK").unwrap(),
            CharsetRequest::Named("GBK".to_string())
        );
    }

    #[test]
    fn rejects_unknown_names() {
        let error = CharsetRequest::parse("klingon").unwrap_err();
        assert!(matches!(error, Error::Config(_)), "got {error:?}");
        assert!(CharsetRequest::parse("").is_err());
    }

    #[test]
    fn accepts_code_page_spellings() {
        assert_eq!(lookup("cp936").unwrap(), lookup("gbk").unwrap());
        assert_eq!(lookup("windows-936").unwrap(), lookup("gbk").unwrap());
        assert_eq!(lookup("cp1252").unwrap(), lookup("latin-1").unwrap());
        assert_eq!(lookup("cp65001").unwrap(), UTF_8);
        assert!(lookup("cp99999").is_err());
    }

    #[test]
    fn explicit_choice_beats_the_server() {
        let charset = CharsetRequest::Named("gbk".into()).resolve(true).unwrap();
        assert_eq!(charset.name(), "GBK");
        assert_eq!(charset.origin(), Origin::Explicit);
        assert!(!charset.is_utf8());
    }

    #[test]
    fn auto_prefers_utf8_when_the_server_announces_it() {
        let charset = CharsetRequest::Auto.resolve(true).unwrap();
        assert!(charset.is_utf8());
        assert_eq!(charset.origin(), Origin::ServerUtf8);
    }

    #[test]
    fn auto_falls_back_to_the_system_code_page() {
        let charset = CharsetRequest::Auto.resolve(false).unwrap();
        match system_code_page() {
            Some(code_page) => {
                assert_eq!(charset.origin(), Origin::SystemCodePage(code_page));
                // On this machine the code page is 936, so GBK is expected.
                assert_eq!(
                    charset.name(),
                    encoding_for_code_page(code_page).unwrap().name()
                );
            }
            None => assert!(charset.is_utf8(), "no code page means UTF-8"),
        }
    }

    #[test]
    fn encodes_ascii_identically_in_every_charset() {
        for label in ["utf-8", "gbk", "big5", "shift_jis", "latin-1"] {
            let charset = CharsetRequest::Named(label.into()).resolve(false).unwrap();
            assert_eq!(charset.encode("pub/file-1.bin").unwrap(), b"pub/file-1.bin");
        }
    }

    #[test]
    fn encodes_chinese_for_a_gbk_server() {
        let charset = CharsetRequest::Named("gbk".into()).resolve(false).unwrap();
        let bytes = charset.encode("中文文件.bin").unwrap();
        assert_eq!(
            bytes,
            vec![0xD6, 0xD0, 0xCE, 0xC4, 0xCE, 0xC4, 0xBC, 0xFE, b'.', b'b', b'i', b'n']
        );
        // And it round-trips back into readable text.
        assert_eq!(charset.decode(&bytes), "中文文件.bin");
    }

    #[test]
    fn decodes_a_gbk_listing_instead_of_mangling_it() {
        let charset = CharsetRequest::Named("gbk".into()).resolve(false).unwrap();
        let gbk = [0xD6, 0xD0, 0xCE, 0xC4]; // 中文
        assert_eq!(charset.decode(&gbk), "中文");
        // The same bytes are not valid UTF-8, which is exactly why the naive
        // lossy decode used to print replacement characters.
        assert_ne!(String::from_utf8_lossy(&gbk), "中文");
    }

    #[test]
    fn refuses_characters_the_target_cannot_represent() {
        let charset = CharsetRequest::Named("latin-1".into())
            .resolve(false)
            .unwrap();
        let error = charset.encode("café/中文.bin").unwrap_err();
        match error {
            Error::Unrepresentable { character, charset } => {
                assert_eq!(character, '中');
                assert!(charset.contains("1252"), "got {charset}");
            }
            other => panic!("expected an unrepresentable-character error, got {other:?}"),
        }
        // Latin-1 can still express its own accented letters.
        assert!(charset.encode("café.bin").is_ok());
    }

    #[test]
    fn utf8_accepts_everything() {
        let charset = Charset::utf8();
        assert!(charset.encode("中文/emoji-🦀.bin").is_ok());
        assert_eq!(charset.name(), "UTF-8");
        assert!(charset.describe().contains("UTF-8"));
    }
}
