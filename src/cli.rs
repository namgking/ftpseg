//! Command line interface: argument definitions and target resolution.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgAction, Args, Parser, Subcommand};
use url::Url;

use crate::client::{ConnectOptions, TlsMode};
use crate::encoding::CharsetRequest;
use crate::error::{Error, Result};
use crate::fsutil;
use crate::paths;

#[derive(Debug, Parser)]
#[command(
    name = "ftpseg",
    version,
    about = "FTP/FTPS client with multi-threaded segmented downloads",
    long_about = "Downloads files over FTP or FTPS using several connections at once.\n\
                  The remote file is split into byte ranges, each fetched with its own \
                  REST + RETR pair, and written straight into the destination file at the \
                  right offset. Interrupted downloads resume from a state file."
)]
pub struct Cli {
    #[command(flatten)]
    pub connection: ConnectionArgs,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Args, Clone)]
pub struct ConnectionArgs {
    /// User name (defaults to anonymous)
    #[arg(short = 'u', long, global = true, env = "FTPSEG_USER")]
    pub user: Option<String>,

    /// Password (prefer FTPSEG_PASSWORD or the URL to keep it out of history)
    #[arg(
        short = 'p',
        long,
        global = true,
        env = "FTPSEG_PASSWORD",
        hide_env_values = true
    )]
    pub password: Option<String>,

    /// Control port (21 for ftp, 990 for implicit FTPS)
    #[arg(short = 'P', long, global = true)]
    pub port: Option<u16>,

    /// Use explicit FTPS: connect in the clear, then upgrade with AUTH TLS
    #[arg(long, global = true)]
    pub tls: bool,

    /// Use implicit FTPS: TLS from the first byte (ftps:// also means this)
    #[arg(long = "tls-implicit", global = true)]
    pub tls_implicit: bool,

    /// Do not verify the server certificate
    #[arg(long, global = true)]
    pub insecure: bool,

    /// Reply timeout in seconds
    #[arg(long, global = true, default_value_t = 30, value_name = "SECONDS")]
    pub timeout: u64,

    /// Connection timeout in seconds
    #[arg(long, global = true, default_value_t = 15, value_name = "SECONDS")]
    pub connect_timeout: u64,

    /// Host to use when the target is a bare remote path
    #[arg(long, global = true, value_name = "HOST")]
    pub host: Option<String>,

    /// Never prompt for a password; fail instead
    #[arg(long, global = true)]
    pub no_prompt: bool,

    /// Character set for remote paths and credentials
    ///
    /// `auto` uses UTF-8 when the server advertises it and otherwise the local
    /// system code page (936/GBK on a Chinese Windows). Name a character set
    /// explicitly for a legacy server, e.g. `--encoding gbk`.
    #[arg(long, global = true, value_name = "CHARSET", default_value = "auto")]
    pub encoding: String,

    /// Print the FTP dialogue
    #[arg(short = 'v', long, global = true, action = ArgAction::Count)]
    pub verbose: u8,

    /// Runtime worker threads (default: one per CPU core)
    #[arg(long, global = true, value_name = "N")]
    pub threads: Option<usize>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Download a file using parallel byte-range requests
    Get(GetArgs),
    /// List a remote directory
    Ls(LsArgs),
    /// Show remote file details and server capabilities
    Stat(StatArgs),
    /// Open the local graphical client in a browser
    Gui(GuiArgs),
}

#[derive(Debug, Args)]
pub struct GuiArgs {
    /// Loopback port for the GUI
    #[arg(long, default_value_t = 17721)]
    pub port: u16,
}

#[derive(Debug, Args)]
pub struct GetArgs {
    /// ftp:// or ftps:// URL, or a remote path when --host is given
    pub target: String,

    /// Where to save the file (default: the remote file name)
    #[arg(short = 'o', long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Number of parallel connections
    #[arg(short = 'n', long, value_name = "N", default_value_t = 4)]
    pub connections: usize,

    /// Fixed segment size, e.g. 8M (default: derived from the file size)
    #[arg(short = 's', long, value_name = "SIZE", value_parser = parse_size_arg)]
    pub segment_size: Option<u64>,

    /// Never split into segments smaller than this
    #[arg(long, value_name = "SIZE", value_parser = parse_size_arg)]
    pub min_segment: Option<u64>,

    /// Bandwidth cap for the whole download, e.g. 2M
    #[arg(long, value_name = "RATE", value_parser = parse_size_arg)]
    pub limit: Option<u64>,

    /// Attempts per segment before giving up
    #[arg(long, value_name = "N", default_value_t = 3)]
    pub retries: usize,

    /// Ignore any previous partial download and start over
    #[arg(long)]
    pub no_resume: bool,

    /// Delete the destination and re-download from scratch
    #[arg(long)]
    pub force: bool,

    /// Keep the resume file after a successful download
    #[arg(long)]
    pub keep_state: bool,

    /// No progress display
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// Show the segment plan without transferring anything
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct LsArgs {
    /// ftp:// or ftps:// URL, or a remote path when --host is given
    pub target: String,

    /// Show size and date columns
    #[arg(short = 'l', long)]
    pub long: bool,
}

#[derive(Debug, Args)]
pub struct StatArgs {
    /// ftp:// or ftps:// URL, or a remote path when --host is given
    pub target: String,
}

fn parse_size_arg(value: &str) -> std::result::Result<u64, String> {
    fsutil::parse_size(value).map_err(|e| e.to_string())
}

/// A fully resolved connection target: where to connect, as whom, and to what.
#[derive(Debug, Clone)]
pub struct TargetSpec {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
    pub remote: String,
    pub tls: TlsMode,
}

impl TargetSpec {
    /// Work out the target from the positional argument plus the global flags.
    ///
    /// A URL carries host, port, credentials and path; the flags override it.
    /// A bare string is only a path, so it needs `--host`.
    pub fn resolve(target: &str, args: &ConnectionArgs) -> Result<Self> {
        if target.contains("://") {
            Self::from_url(target, args)
        } else {
            Self::from_path(target, args)
        }
    }

    fn from_url(target: &str, args: &ConnectionArgs) -> Result<Self> {
        let url = Url::parse(target).map_err(|e| Error::Url(format!("{target}: {e}")))?;

        let scheme_tls = match url.scheme() {
            "ftp" => TlsMode::None,
            // By convention ftps:// means implicit TLS on port 990.
            "ftps" => TlsMode::Implicit,
            other => {
                return Err(Error::Url(format!(
                    "unsupported scheme {other:?}; use ftp:// or ftps://"
                )))
            }
        };

        let tls = if args.tls_implicit {
            TlsMode::Implicit
        } else if args.tls {
            TlsMode::Explicit
        } else {
            scheme_tls
        };

        let host = url
            .host_str()
            .ok_or_else(|| Error::Url(format!("{target} has no host")))?
            .to_string();

        let default_port = if tls == TlsMode::Implicit { 990 } else { 21 };
        let port = args.port.or_else(|| url.port()).unwrap_or(default_port);

        let user = args
            .user
            .clone()
            .or_else(|| non_empty(url.username()))
            .map(|u| percent_decode(&u));
        let password = args
            .password
            .clone()
            .or_else(|| url.password().map(percent_decode));

        let path = percent_decode(url.path().trim_start_matches('/'));
        let remote = if path.is_empty() {
            "/".to_string()
        } else if url.path().starts_with('/') {
            format!("/{path}")
        } else {
            path
        };

        Ok(TargetSpec {
            host,
            port,
            user,
            password,
            remote,
            tls,
        })
    }

    fn from_path(target: &str, args: &ConnectionArgs) -> Result<Self> {
        let host = args.host.clone().ok_or_else(|| {
            // Build the example without doubling the separator when the path
            // already starts with a slash.
            let example = target.trim_start_matches('/');
            Error::Config(format!(
                "{target:?} is a remote path, so a host is required: \
                 pass a full URL (ftp://host/{example}) or --host HOST"
            ))
        })?;

        let tls = if args.tls_implicit {
            TlsMode::Implicit
        } else if args.tls {
            TlsMode::Explicit
        } else {
            TlsMode::None
        };
        let default_port = if tls == TlsMode::Implicit { 990 } else { 21 };

        Ok(TargetSpec {
            host,
            port: args.port.unwrap_or(default_port),
            user: args.user.clone(),
            password: args.password.clone(),
            remote: target.to_string(),
            tls,
        })
    }

    /// Build the low-level connection options, prompting for a password only
    /// when there is a human to answer.
    pub fn connect_options(&self, args: &ConnectionArgs) -> Result<ConnectOptions> {
        let user = self.user.clone().unwrap_or_else(|| "anonymous".to_string());
        let password = match &self.password {
            Some(password) => password.clone(),
            None => {
                if user == "anonymous" || user == "ftp" {
                    "anonymous@".to_string()
                } else if args.no_prompt || !std::io::stdin().is_terminal() {
                    return Err(Error::Config(format!(
                        "no password for user {user}: use --password, set FTPSEG_PASSWORD, \
                         or put it in the URL"
                    )));
                } else {
                    rpassword::prompt_password(format!("Password for {user}@{}: ", self.host))
                        .map_err(|e| Error::Config(format!("could not read password: {e}")))?
                }
            }
        };

        Ok(ConnectOptions {
            host: self.host.clone(),
            port: self.port,
            user,
            password,
            tls: self.tls,
            accept_invalid_certs: args.insecure,
            connect_timeout: Duration::from_secs(args.connect_timeout.max(1)),
            timeout: Duration::from_secs(args.timeout.max(1)),
            verbose: args.verbose > 0,
            // Parsed up front so a typo fails before the connection is opened.
            charset: CharsetRequest::parse(&args.encoding)?,
        })
    }

    /// A sensible local file name for this target.
    pub fn default_local_name(&self) -> String {
        let name = paths::basename(&self.remote);
        if name.is_empty() {
            "download.bin".to_string()
        } else {
            name
        }
    }
}

fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn percent_decode(value: &str) -> String {
    percent_encoding::percent_decode_str(value)
        .decode_utf8_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn args() -> ConnectionArgs {
        ConnectionArgs {
            user: None,
            password: None,
            port: None,
            tls: false,
            tls_implicit: false,
            insecure: false,
            timeout: 30,
            connect_timeout: 15,
            host: None,
            no_prompt: true,
            encoding: "auto".to_string(),
            verbose: 0,
            threads: None,
        }
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_a_plain_ftp_url() {
        let spec = TargetSpec::resolve("ftp://example.com/pub/file.bin", &args()).unwrap();
        assert_eq!(spec.host, "example.com");
        assert_eq!(spec.port, 21);
        assert_eq!(spec.remote, "/pub/file.bin");
        assert_eq!(spec.tls, TlsMode::None);
        assert_eq!(spec.default_local_name(), "file.bin");
    }

    #[test]
    fn ftps_url_means_implicit_tls_on_990() {
        let spec = TargetSpec::resolve("ftps://example.com/file.bin", &args()).unwrap();
        assert_eq!(spec.tls, TlsMode::Implicit);
        assert_eq!(spec.port, 990);
    }

    #[test]
    fn tls_flag_upgrades_plain_ftp() {
        let mut a = args();
        a.tls = true;
        let spec = TargetSpec::resolve("ftp://example.com/file.bin", &a).unwrap();
        assert_eq!(spec.tls, TlsMode::Explicit);
        assert_eq!(spec.port, 21);
    }

    #[test]
    fn credentials_come_from_the_url_and_can_be_overridden() {
        let spec =
            TargetSpec::resolve("ftp://alice:s3cret@example.com:2121/file.bin", &args()).unwrap();
        assert_eq!(spec.user.as_deref(), Some("alice"));
        assert_eq!(spec.password.as_deref(), Some("s3cret"));
        assert_eq!(spec.port, 2121);

        let mut a = args();
        a.user = Some("bob".into());
        a.password = Some("hunter2".into());
        a.port = Some(21);
        let spec = TargetSpec::resolve("ftp://alice:s3cret@example.com:2121/f.bin", &a).unwrap();
        assert_eq!(spec.user.as_deref(), Some("bob"));
        assert_eq!(spec.password.as_deref(), Some("hunter2"));
        assert_eq!(spec.port, 21);
    }

    #[test]
    fn percent_escapes_are_decoded() {
        let spec = TargetSpec::resolve("ftp://example.com/my%20file%20name.bin", &args()).unwrap();
        assert_eq!(spec.remote, "/my file name.bin");
        assert_eq!(spec.default_local_name(), "my file name.bin");
    }

    #[test]
    fn a_password_may_contain_at_signs_and_colons() {
        // The authority ends at the *last* `@`, so these need no escaping.
        let spec = TargetSpec::resolve("ftp://alice:p@ss:w0rd@example.com/f.bin", &args()).unwrap();
        assert_eq!(spec.host, "example.com");
        assert_eq!(spec.user.as_deref(), Some("alice"));
        assert_eq!(spec.password.as_deref(), Some("p@ss:w0rd"));
    }

    #[test]
    fn percent_escapes_carry_the_characters_a_url_cannot() {
        // `/`, `#` and `?` would end the authority or start a fragment, so
        // they have to be escaped rather than written literally.
        let spec =
            TargetSpec::resolve("ftp://alice:p%2Fss%23x%3Fy@example.com/f.bin", &args()).unwrap();
        assert_eq!(spec.host, "example.com");
        assert_eq!(spec.password.as_deref(), Some("p/ss#x?y"));
    }

    #[test]
    fn an_unescaped_slash_in_a_password_is_rejected_outright() {
        // `/` ends the authority, so what follows is read as a port number and
        // the whole URL is refused. Louder than silently connecting somewhere
        // unexpected, and the fix is simply to escape it.
        let error = TargetSpec::resolve("ftp://alice:p/ss@example.com/f.bin", &args()).unwrap_err();
        assert!(matches!(error, Error::Url(_)), "got {error:?}");

        let spec = TargetSpec::resolve("ftp://alice:p%2Fss@example.com/f.bin", &args()).unwrap();
        assert_eq!(spec.host, "example.com");
        assert_eq!(spec.password.as_deref(), Some("p/ss"));
    }

    #[test]
    fn non_ascii_credentials_and_paths_survive_the_url() {
        let spec = TargetSpec::resolve(
            "ftp://%E7%94%A8%E6%88%B7:%E5%AF%86%E7%A0%81@example.com/%E4%B8%AD%E6%96%87%20file.bin",
            &args(),
        )
        .unwrap();
        assert_eq!(spec.user.as_deref(), Some("用户"));
        assert_eq!(spec.password.as_deref(), Some("密码"));
        assert_eq!(spec.remote, "/中文 file.bin");
        assert_eq!(spec.default_local_name(), "中文 file.bin");
    }

    #[test]
    fn encoding_reaches_the_connection_options() {
        let spec = TargetSpec::resolve("ftp://example.com/f.bin", &args()).unwrap();

        let mut a = args();
        a.encoding = "gbk".to_string();
        assert_eq!(
            spec.connect_options(&a).unwrap().charset,
            CharsetRequest::Named("gbk".into())
        );

        a.encoding = "windows-936".to_string();
        assert_eq!(
            spec.connect_options(&a).unwrap().charset,
            CharsetRequest::Named("windows-936".into())
        );

        a.encoding = "auto".to_string();
        assert_eq!(
            spec.connect_options(&a).unwrap().charset,
            CharsetRequest::Auto
        );

        // A typo must fail before anything is dialled.
        a.encoding = "klingon".to_string();
        assert!(spec.connect_options(&a).is_err());
    }

    #[test]
    fn a_bare_url_host_yields_the_root_path() {
        let spec = TargetSpec::resolve("ftp://example.com", &args()).unwrap();
        assert_eq!(spec.remote, "/");
        assert_eq!(spec.default_local_name(), "download.bin");
    }

    #[test]
    fn bare_path_needs_host() {
        let error = TargetSpec::resolve("/pub/file.bin", &args()).unwrap_err();
        match error {
            Error::Config(message) => {
                assert!(
                    message.contains("ftp://host/pub/file.bin"),
                    "got: {message}"
                )
            }
            other => panic!("expected a configuration error, got {other:?}"),
        }

        let mut a = args();
        a.host = Some("example.com".into());
        let spec = TargetSpec::resolve("/pub/file.bin", &a).unwrap();
        assert_eq!(spec.host, "example.com");
        assert_eq!(spec.remote, "/pub/file.bin");
    }

    #[test]
    fn rejects_other_schemes() {
        assert!(TargetSpec::resolve("http://example.com/x", &args()).is_err());
    }

    #[test]
    fn anonymous_login_needs_no_prompt() {
        let spec = TargetSpec::resolve("ftp://example.com/f.bin", &args()).unwrap();
        let options = spec.connect_options(&args()).unwrap();
        assert_eq!(options.user, "anonymous");
        assert_eq!(options.password, "anonymous@");
    }

    #[test]
    fn missing_password_fails_without_a_terminal() {
        let spec = TargetSpec::resolve("ftp://alice@example.com/f.bin", &args()).unwrap();
        let error = spec.connect_options(&args()).unwrap_err();
        assert!(matches!(error, Error::Config(_)));
    }

    #[test]
    fn parses_size_arguments() {
        assert_eq!(parse_size_arg("8M").unwrap(), 8 * 1024 * 1024);
        assert!(parse_size_arg("nope").is_err());
    }

    #[test]
    fn parses_subcommands() {
        let cli = Cli::try_parse_from([
            "ftpseg",
            "get",
            "ftp://example.com/f.bin",
            "-n",
            "8",
            "-o",
            "out.bin",
            "--limit",
            "2M",
        ])
        .unwrap();
        match cli.command {
            Command::Get(get) => {
                assert_eq!(get.connections, 8);
                assert_eq!(get.output.unwrap().to_string_lossy(), "out.bin");
                assert_eq!(get.limit, Some(2 * 1024 * 1024));
            }
            _ => panic!("expected the get subcommand"),
        }
    }

    #[test]
    fn parses_gui_port() {
        let cli = Cli::try_parse_from(["ftpseg", "gui", "--port", "17722"]).unwrap();
        match cli.command {
            Command::Gui(args) => assert_eq!(args.port, 17722),
            _ => panic!("expected the gui subcommand"),
        }
    }

    #[test]
    fn global_flags_work_after_the_subcommand() {
        let cli = Cli::try_parse_from([
            "ftpseg",
            "stat",
            "ftp://example.com/f.bin",
            "--tls",
            "--timeout",
            "5",
        ])
        .unwrap();
        assert!(cli.connection.tls);
        assert_eq!(cli.connection.timeout, 5);
    }
}
