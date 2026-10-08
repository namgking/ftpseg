//! Error types shared by the whole crate.

use std::time::Duration;

/// Everything that can go wrong while talking FTP or writing the local file.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The server said something we could not make sense of.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// A `4xx`/`5xx` reply where we needed a positive one.
    #[error("server replied {code} {message}")]
    Server { code: u16, message: String },

    /// A syntactically valid reply of the wrong kind.
    #[error("expected {expected}, server replied {code} {message}")]
    UnexpectedReply {
        expected: String,
        code: u16,
        message: String,
    },

    #[error("TLS error: {0}")]
    Tls(String),

    #[error("timed out after {0:?}")]
    Timeout(Duration),

    /// The server does not implement something we need for this mode.
    #[error("server does not support {0}")]
    Unsupported(String),

    #[error("invalid configuration: {0}")]
    Config(String),

    /// A transfer ended before the expected number of bytes arrived.
    #[error("short transfer: expected {expected} bytes, got {actual}")]
    ShortTransfer { expected: u64, actual: u64 },

    /// The remote file does not exist / is not a plain file.
    #[error("remote file not found: {0}")]
    NotFound(String),

    /// A name or credential holds a character the server's character set cannot
    /// express. Refusing beats substituting, which would earn a confusing 550.
    #[error(
        "the character {character:?} cannot be written in {charset}, which the server needs \
         in order to read this name; choose a wider character set with --encoding \
         (for example --encoding utf-8 or --encoding gbk)"
    )]
    Unrepresentable { character: char, charset: String },

    #[error("URL/path parse error: {0}")]
    Url(String),

    /// Raised when the user interrupts, or a worker is told to stop.
    #[error("aborted")]
    Aborted,
}

impl Error {
    /// `true` when retrying the exact same operation could plausibly succeed.
    ///
    /// Protocol misuse and configuration problems are permanent; a dropped
    /// socket or a busy server is not.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Io(_) | Error::Timeout(_) => true,
            Error::ShortTransfer { .. } => true,
            Error::Server { code, .. } => matches!(code, 421 | 425 | 426 | 450 | 451 | 452),
            _ => false,
        }
    }

    pub fn server(code: u16, message: impl Into<String>) -> Self {
        Error::Server {
            code,
            message: message.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
