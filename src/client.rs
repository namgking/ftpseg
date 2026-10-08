//! The FTP/FTPS control connection and everything that hangs off it: login,
//! capability discovery, data connections and transfers.

use std::net::IpAddr;
use std::time::Duration;

use native_tls::TlsConnector;
use tokio::net::TcpStream;
use tokio_native_tls::TlsConnector as TokioTlsConnector;

use crate::encoding::{Charset, CharsetRequest};
use crate::error::{Error, Result};
use crate::protocol::{
    parse_epsv, parse_mdtm, parse_pasv, parse_size, resolve_pasv_target, DirEntry, Reply,
};
use crate::transport::{read_to_end_capped, ControlStream, Stream};

/// How TLS is negotiated, if at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    /// Plain FTP.
    None,
    /// Plain control port, upgraded in-band with `AUTH TLS` (RFC 4217).
    Explicit,
    /// TLS from the first byte, conventionally port 990.
    Implicit,
}

impl TlsMode {
    pub fn is_tls(self) -> bool {
        !matches!(self, TlsMode::None)
    }
}

/// Everything needed to build a fresh control connection.
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub tls: TlsMode,
    pub accept_invalid_certs: bool,
    pub connect_timeout: Duration,
    pub timeout: Duration,
    pub verbose: bool,
    /// Which character set the wire should speak for paths and credentials.
    pub charset: CharsetRequest,
}

impl ConnectOptions {
    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// What the server told us in its `FEAT` reply, plus what we probed ourselves.
#[derive(Debug, Clone, Default)]
pub struct FeatureSet {
    pub raw: Vec<String>,
    pub feat_supported: bool,
    pub size: bool,
    pub mdtm: bool,
    pub mlsd: bool,
    pub utf8: bool,
    /// `REST STREAM`: byte-range restart, the prerequisite for segmentation.
    pub rest_stream: bool,
    pub epsv: bool,
    pub auth_tls: bool,
    pub cpsv: bool,
}

impl FeatureSet {
    pub fn from_lines(lines: &[String]) -> Self {
        let mut set = FeatureSet {
            raw: lines.to_vec(),
            feat_supported: true,
            ..Default::default()
        };
        for line in lines {
            let upper = line.to_ascii_uppercase();
            let name = upper.split_whitespace().next().unwrap_or("");
            match name {
                "SIZE" => set.size = true,
                "MDTM" => set.mdtm = true,
                "MLSD" => set.mlsd = true,
                "UTF8" => set.utf8 = true,
                "REST" => {
                    if upper.contains("STREAM") {
                        set.rest_stream = true;
                    }
                }
                "EPSV" => set.epsv = true,
                "CPSV" => set.cpsv = true,
                "AUTH" if upper.contains("TLS") || upper.contains("SSL") => set.auth_tls = true,
                _ => {}
            }
        }
        set
    }
}

/// An open, logged-in control connection.
pub struct FtpClient {
    /// `None` only while the socket is being moved into a TLS wrapper.
    ctl: Option<ControlStream>,
    host: String,
    timeout: Duration,
    connect_timeout: Duration,
    features: FeatureSet,
    /// Set once `EPSV` turns out to be unusable, so we stop retrying it.
    use_epsv: bool,
    peer_ip: Option<IpAddr>,
    tls_connector: Option<TokioTlsConnector>,
    /// Whether data connections need their own TLS session (`PROT P`).
    transfer_tls: bool,
    verbose: bool,
    /// Character set used for paths and credentials on this connection.
    charset: Charset,
}

async fn connect_tcp(host: &str, port: u16, timeout: Duration) -> Result<TcpStream> {
    let stream = tokio::time::timeout(timeout, TcpStream::connect((host, port)))
        .await
        .map_err(|_| Error::Timeout(timeout))??;
    // Command/reply ping-pong is latency bound, so do not wait on Nagle.
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

fn build_connector(opts: &ConnectOptions) -> Result<TokioTlsConnector> {
    let mut builder = TlsConnector::builder();
    if opts.accept_invalid_certs {
        builder.danger_accept_invalid_certs(true);
        builder.danger_accept_invalid_hostnames(true);
    }
    builder.min_protocol_version(Some(native_tls::Protocol::Tlsv12));
    let connector = builder.build().map_err(|e| Error::Tls(e.to_string()))?;
    Ok(TokioTlsConnector::from(connector))
}

/// A raw line of a reply, code prefix included.
fn raw_first(reply: &Reply) -> String {
    reply.lines.first().cloned().unwrap_or_default()
}

impl FtpClient {
    /// Connect, handshake, log in and learn the server's capabilities.
    pub async fn connect(opts: &ConnectOptions) -> Result<FtpClient> {
        let connector = if opts.tls.is_tls() {
            Some(build_connector(opts)?)
        } else {
            None
        };

        let tcp = connect_tcp(&opts.host, opts.port, opts.connect_timeout).await?;
        let peer_ip = tcp.peer_addr().ok().map(|addr| addr.ip());

        let stream = if opts.tls == TlsMode::Implicit {
            let tls = connector
                .as_ref()
                .expect("connector built for TLS mode")
                .connect(&opts.host, tcp)
                .await
                .map_err(|e| Error::Tls(e.to_string()))?;
            Stream::Tls(Box::new(tls))
        } else {
            Stream::Plain(tcp)
        };

        let mut client = FtpClient {
            ctl: Some(ControlStream::new(stream)),
            host: opts.host.clone(),
            timeout: opts.timeout,
            connect_timeout: opts.connect_timeout,
            features: FeatureSet::default(),
            use_epsv: true,
            peer_ip,
            tls_connector: connector,
            transfer_tls: opts.tls.is_tls(),
            verbose: opts.verbose,
            // UTF-8 until `FEAT` says otherwise.
            charset: Charset::utf8(),
        };

        // Greeting: 220, or 120 followed by 220 when the server needs a moment.
        let greeting = client.read_reply().await?;
        let greeting = if greeting.code == 120 {
            client.read_reply().await?
        } else {
            greeting
        };
        if !greeting.is_positive() {
            return Err(greeting.into_error());
        }

        if opts.tls == TlsMode::Explicit {
            client.upgrade_to_tls().await?;
        }

        // Capabilities are probed *before* logging in, because both the
        // credentials and every path that follows have to be written in the
        // server's character set, and only FEAT can tell us which that is.
        // Servers that refuse FEAT before login just get the fallback.
        client.read_features().await;
        client.charset = opts.charset.resolve(client.features.utf8)?;
        client.announce_utf8().await;

        client.login(&opts.user, &opts.password).await?;
        client.set_binary().await?;

        // If the pre-login probe was refused, try again now that we are in and
        // upgrade the character set if it turns out the server does speak UTF-8.
        if !client.features.feat_supported {
            client.read_features().await;
            let resolved = opts.charset.resolve(client.features.utf8)?;
            if resolved.encoding() != client.charset.encoding() {
                if client.verbose {
                    eprintln!(
                        "note: switching character set to {} after login",
                        resolved.describe()
                    );
                }
                client.charset = resolved;
                client.announce_utf8().await;
            }
        }

        Ok(client)
    }

    /// Tell a UTF-8 capable server that we will actually use UTF-8.
    ///
    /// Some servers need `OPTS UTF8 ON` before they accept multi-byte names;
    /// others do not implement the command at all, which is not an error.
    async fn announce_utf8(&mut self) {
        if !self.charset.is_utf8() || !self.features.utf8 {
            return;
        }
        let _ = self.cmd("OPTS UTF8 ON").await;
    }

    /// The character set in use for paths and credentials.
    pub fn charset(&self) -> Charset {
        self.charset
    }

    fn ctl(&mut self) -> Result<&mut ControlStream> {
        self.ctl
            .as_mut()
            .ok_or_else(|| Error::Protocol("control connection is closed".into()))
    }

    pub fn features(&self) -> &FeatureSet {
        &self.features
    }

    pub fn is_tls(&self) -> bool {
        self.ctl.as_ref().map(|c| c.is_tls()).unwrap_or(false)
    }

    pub fn peer_ip(&self) -> Option<IpAddr> {
        self.peer_ip
    }

    fn log_out(&self, line: &[u8]) {
        if !self.verbose {
            return;
        }
        if line.len() >= 5 && line[..5].eq_ignore_ascii_case(b"PASS ") {
            eprintln!("-> PASS ********");
        } else {
            // Decode with the negotiated charset so the trace shows real names
            // rather than mojibake.
            eprintln!("-> {}", self.charset.decode(line));
        }
    }

    /// Send a command, given as raw bytes, and read its reply.
    async fn send(&mut self, line: &[u8]) -> Result<Reply> {
        self.log_out(line);
        let timeout = self.timeout;
        self.ctl()?.write_line(line, timeout).await?;
        self.read_reply().await
    }

    /// Send a command whose arguments are all ASCII.
    pub async fn cmd(&mut self, line: &str) -> Result<Reply> {
        self.send(line.as_bytes()).await
    }

    /// Send `verb argument`, converting the argument into the server's charset.
    ///
    /// This is the only safe way to name a remote file: protocol verbs, reply
    /// codes and numbers are ASCII in every charset we support, but paths and
    /// credentials are not.
    pub async fn cmd_arg(&mut self, verb: &str, argument: &str) -> Result<Reply> {
        let mut line = Vec::with_capacity(verb.len() + 1 + argument.len());
        line.extend_from_slice(verb.as_bytes());
        line.push(b' ');
        line.extend_from_slice(&self.charset.encode(argument)?);
        self.send(&line).await
    }

    /// Send a command that must produce one of `expected` codes.
    pub async fn cmd_expect(&mut self, line: &str, expected: &[u16]) -> Result<Reply> {
        let reply = self.cmd(line).await?;
        if expected.contains(&reply.code) {
            Ok(reply)
        } else {
            Err(Error::UnexpectedReply {
                expected: expected
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join("/"),
                code: reply.code,
                message: reply.text(),
            })
        }
    }

    /// Read one complete reply, single or multi line.
    pub async fn read_reply(&mut self) -> Result<Reply> {
        self.read_reply_with(self.timeout).await
    }

    async fn read_reply_with(&mut self, timeout: Duration) -> Result<Reply> {
        let first = self.ctl()?.read_line(timeout).await?;
        let (code, more) = Reply::parse_code(&first)
            .ok_or_else(|| Error::Protocol(format!("malformed reply: {first:?}")))?;
        let terminator = format!("{code:03} ");
        let mut lines = vec![first];

        if more {
            loop {
                let line = self.ctl()?.read_line(timeout).await?;
                let done = line.starts_with(&terminator);
                lines.push(line);
                if done {
                    break;
                }
                if lines.len() > 8192 {
                    return Err(Error::Protocol("reply has too many lines".into()));
                }
            }
        }

        if self.verbose {
            for line in &lines {
                eprintln!("<- {line}");
            }
        }
        Ok(Reply::new(code, lines))
    }

    /// Upgrade a plain control connection to TLS, then protect data too.
    async fn upgrade_to_tls(&mut self) -> Result<()> {
        let reply = self.cmd("AUTH TLS").await?;
        if reply.is_not_implemented() {
            return Err(Error::Unsupported("AUTH TLS".into()));
        }
        if reply.code != 234 {
            return Err(Error::UnexpectedReply {
                expected: "234".into(),
                code: reply.code,
                message: reply.text(),
            });
        }

        let old = self
            .ctl
            .take()
            .ok_or_else(|| Error::Protocol("control connection is closed".into()))?;
        let tcp = match old.into_inner() {
            Stream::Plain(tcp) => tcp,
            Stream::Tls(_) => return Err(Error::Config("connection is already encrypted".into())),
        };
        let connector = self
            .tls_connector
            .clone()
            .ok_or_else(|| Error::Config("no TLS connector available".into()))?;
        let tls = connector
            .connect(&self.host, tcp)
            .await
            .map_err(|e| Error::Tls(e.to_string()))?;
        self.ctl = Some(ControlStream::new(Stream::Tls(Box::new(tls))));
        self.transfer_tls = true;

        let reply = self.cmd("PBSZ 0").await?;
        if reply.is_negative() {
            return Err(reply.into_error());
        }
        let reply = self.cmd("PROT P").await?;
        if reply.is_negative() {
            return Err(reply.into_error());
        }
        Ok(())
    }

    async fn login(&mut self, user: &str, password: &str) -> Result<()> {
        let user = if user.is_empty() { "anonymous" } else { user };
        let reply = self.cmd_arg("USER", user).await?;

        match reply.code {
            230 => return Ok(()),
            331 | 332 => {}
            _ => return Err(reply.into_error()),
        }

        let reply = self.cmd_arg("PASS", password).await?;
        if reply.is_positive() {
            Ok(())
        } else {
            Err(reply.into_error())
        }
    }

    async fn set_binary(&mut self) -> Result<()> {
        let reply = self.cmd("TYPE I").await?;
        if reply.is_positive() {
            Ok(())
        } else {
            Err(reply.into_error())
        }
    }

    async fn read_features(&mut self) {
        if let Ok(reply) = self.cmd("FEAT").await {
            if reply.code == 211 {
                self.features = FeatureSet::from_lines(&reply.feature_lines());
                return;
            }
        }
        self.features.feat_supported = false;
    }

    /// Confirm the server really honours `REST`, not just advertises it.
    ///
    /// This guard matters: a server that silently ignores `REST` would make
    /// every segment after the first return data from offset zero, which would
    /// corrupt the output file.
    pub async fn supports_rest(&mut self) -> bool {
        if self.features.rest_stream {
            return true;
        }
        matches!(self.cmd("REST 0").await, Ok(reply) if reply.code == 350)
    }

    /// `SIZE path`. `Ok(None)` when the server has no `SIZE` or refuses it.
    pub async fn size(&mut self, path: &str) -> Result<Option<u64>> {
        let reply = self.cmd_arg("SIZE", path).await?;
        if !reply.is_positive() {
            return Ok(None);
        }
        parse_size(&raw_first(&reply)).map(Some)
    }

    /// `MDTM path` as `YYYYMMDDHHMMSS`.
    pub async fn mdtm(&mut self, path: &str) -> Result<Option<String>> {
        let reply = self.cmd_arg("MDTM", path).await?;
        if !reply.is_positive() {
            return Ok(None);
        }
        Ok(parse_mdtm(&raw_first(&reply)).ok())
    }

    pub async fn pwd(&mut self) -> Result<String> {
        let reply = self.cmd("PWD").await?;
        if !reply.is_positive() {
            return Err(reply.into_error());
        }
        let text = reply.text();
        // 257 "/pub" is current directory
        if let (Some(start), Some(end)) = (text.find('"'), text.rfind('"')) {
            if end > start {
                return Ok(text[start + 1..end].to_string());
            }
        }
        Ok(text)
    }

    pub async fn cwd(&mut self, path: &str) -> Result<()> {
        let reply = self.cmd_arg("CWD", path).await?;
        if reply.is_positive() {
            Ok(())
        } else {
            Err(reply.into_error())
        }
    }

    /// Directory listing, preferring `MLSD` and falling back to `LIST`.
    pub async fn list(&mut self, path: Option<&str>) -> Result<Vec<DirEntry>> {
        if self.features.mlsd || !self.features.feat_supported {
            match self.list_with("MLSD", path).await {
                Ok(entries) => return Ok(entries),
                Err(Error::Unsupported(_)) => {}
                Err(e) => return Err(e),
            }
        }
        self.list_with("LIST", path).await
    }

    async fn list_with(&mut self, command: &str, path: Option<&str>) -> Result<Vec<DirEntry>> {
        let mut data = self.open_data_connection().await?;
        let reply = match path {
            Some(p) if !p.is_empty() => self.cmd_arg(command, p).await?,
            _ => self.cmd(command).await?,
        };
        if !(reply.code == 125 || reply.code == 150) {
            drop(data);
            if reply.is_not_implemented() {
                return Err(Error::Unsupported(command.to_string()));
            }
            return Err(reply.into_error());
        }

        let timeout = self.timeout;
        let body = read_to_end_capped(&mut data, 16 * 1024 * 1024, timeout).await?;
        drop(data);
        self.finish_transfer(false).await?;

        // Listings come back in the server's character set, so a legacy server
        // would otherwise turn every non-ASCII name into replacement characters.
        Ok(crate::protocol::listing::parse_listing(
            &self.charset.decode(&body),
            command == "MLSD",
        ))
    }

    /// Ask for a passive data connection (`EPSV` preferred, `PASV` fallback).
    pub async fn open_data_connection(&mut self) -> Result<Stream> {
        if self.use_epsv {
            match self.cmd("EPSV").await {
                Ok(reply) if reply.code == 229 => {
                    let advertised = parse_epsv(&raw_first(&reply))?;
                    return self.connect_data(advertised.host, advertised.port).await;
                }
                Ok(reply) if reply.is_not_implemented() => {
                    self.use_epsv = false;
                }
                Ok(reply) => return Err(reply.into_error()),
                Err(Error::Timeout(_)) => return Err(Error::Timeout(self.timeout)),
                Err(e) => return Err(e),
            }
        }

        let reply = self.cmd("PASV").await?;
        if !reply.is_positive() {
            return Err(reply.into_error());
        }
        let advertised = parse_pasv(&raw_first(&reply))?;
        let target = resolve_pasv_target(&advertised, self.peer_ip);
        self.connect_data(target.host, target.port).await
    }

    async fn connect_data(&mut self, host: Option<IpAddr>, port: u16) -> Result<Stream> {
        let host = host
            .or(self.peer_ip)
            .ok_or_else(|| Error::Protocol("passive reply carried no usable address".into()))?;
        let tcp = connect_tcp(&host.to_string(), port, self.connect_timeout).await?;
        if !self.transfer_tls {
            return Ok(Stream::Plain(tcp));
        }
        let connector = self
            .tls_connector
            .clone()
            .ok_or_else(|| Error::Config("no TLS connector for data connection".into()))?;
        let tls = connector
            .connect(&self.host, tcp)
            .await
            .map_err(|e| Error::Tls(e.to_string()))?;
        Ok(Stream::Tls(Box::new(tls)))
    }

    /// Start a `RETR`, optionally from `offset` bytes into the file.
    ///
    /// The returned stream is the data connection; the caller reads exactly the
    /// bytes it wants and then calls [`FtpClient::finish_transfer`].
    pub async fn begin_retr(&mut self, path: &str, offset: Option<u64>) -> Result<Stream> {
        if let Some(offset) = offset.filter(|o| *o > 0) {
            let reply = self.cmd(&format!("REST {offset}")).await?;
            if reply.code != 350 {
                return Err(Error::Unsupported(format!(
                    "REST (server replied {})",
                    reply.code
                )));
            }
        }

        let data = self.open_data_connection().await?;
        let reply = self.cmd_arg("RETR", path).await?;
        if reply.code == 125 || reply.code == 150 {
            return Ok(data);
        }
        drop(data);
        if reply.is_not_implemented() {
            return Err(Error::Unsupported("RETR".into()));
        }
        Err(reply.into_error())
    }

    /// Read the reply that ends a transfer, once the data connection is closed.
    ///
    /// `early_close` must be `true` when we deliberately stopped reading before
    /// the server ran out of file. In that case the server is still busy
    /// writing into a socket nobody reads, and its opinion no longer matters:
    /// the bytes we asked for already arrived. Rather than wait for a verdict
    /// that may never come, we establish a synchronisation point with `NOOP`
    /// and absorb whatever the abandoned transfer still owes us.
    pub async fn finish_transfer(&mut self, early_close: bool) -> Result<()> {
        if early_close {
            return self.resync().await;
        }

        let mut final_reply = None;
        for _ in 0..4 {
            let reply = self.read_reply_with(self.timeout).await?;
            if reply.is_intermediate() {
                continue;
            }
            final_reply = Some(reply);
            break;
        }

        match final_reply {
            Some(reply) if reply.is_positive() => Ok(()),
            Some(reply) => Err(reply.into_error()),
            None => Err(Error::Protocol(
                "transfer ended without a final reply".into(),
            )),
        }
    }

    /// Put the command channel back in step after an aborted transfer.
    ///
    /// Servers disagree about how many replies they still owe: some send `426`
    /// then `226`, some send `426` only, some send nothing at all. Replies are
    /// ordered per connection, so a single `NOOP` marks the end of the backlog
    /// and everything before its `200` can be discarded.
    async fn resync(&mut self) -> Result<()> {
        let timeout = self.timeout;
        self.ctl()?.write_line(b"NOOP", timeout).await?;
        if self.verbose {
            eprintln!("-> NOOP (resync)");
        }

        let wait = timeout.min(Duration::from_secs(10));
        for _ in 0..6 {
            let reply = self.read_reply_with(wait).await?;
            if reply.code == 200 {
                return Ok(());
            }
        }
        Err(Error::Protocol(
            "control connection out of sync after aborted transfer".into(),
        ))
    }

    /// Friendly goodbye. Failures are ignored: we are closing anyway.
    pub async fn quit(&mut self) {
        if self.ctl.is_none() {
            return;
        }
        let _ = self.cmd("QUIT").await;
        self.ctl = None;
    }
}

impl std::fmt::Debug for FtpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FtpClient")
            .field("host", &self.host)
            .field("encrypted", &self.is_tls())
            .field("transfer_tls", &self.transfer_tls)
            .field("features", &self.features)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_features() {
        let lines: Vec<String> = [
            "SIZE",
            "MDTM",
            "MLSD",
            "UTF8",
            "REST STREAM",
            "EPSV",
            "AUTH TLS",
            "CPSV",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let set = FeatureSet::from_lines(&lines);
        assert!(set.size && set.mdtm && set.mlsd && set.utf8);
        assert!(set.rest_stream && set.epsv && set.auth_tls && set.cpsv);
        assert!(set.feat_supported);
    }

    #[test]
    fn rest_without_stream_is_not_segmentable() {
        let lines = vec!["REST STREAM".to_string()];
        assert!(FeatureSet::from_lines(&lines).rest_stream);

        let lines = vec!["REST".to_string()];
        assert!(!FeatureSet::from_lines(&lines).rest_stream);
    }

    #[test]
    fn tls_mode_helpers() {
        assert!(!TlsMode::None.is_tls());
        assert!(TlsMode::Explicit.is_tls());
        assert!(TlsMode::Implicit.is_tls());
    }
}
