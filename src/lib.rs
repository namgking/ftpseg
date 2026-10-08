//! `ftpseg` — an FTP/FTPS client with multi-threaded segmented downloads.
//!
//! The crate is split so the pieces can be used independently:
//!
//! * [`client`] speaks FTP on one control connection;
//! * [`encoding`] decides which character set the wire speaks;
//! * [`plan`] cuts a file into byte ranges;
//! * [`pool`] hands logged-in connections to workers;
//! * [`download`] ties it together into a parallel download with resume;
//! * [`cli`] is the command line front end in `src/main.rs`.

pub mod cli;
pub mod client;
pub mod config;
mod credentials;
pub mod download;
pub mod encoding;
pub mod error;
pub mod fsutil;
pub mod gui;
pub mod paths;
pub mod plan;
pub mod pool;
pub mod progress;
pub mod protocol;
pub mod ratelimit;
pub mod state;
pub mod transport;

pub use client::{ConnectOptions, FtpClient, TlsMode};
pub use config::{DownloadOptions, DownloadSummary, SegmentPolicy};
pub use encoding::{Charset, CharsetRequest};
pub use error::{Error, Result};
