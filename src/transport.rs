//! Byte transport: a TCP or TLS stream plus line-oriented helpers for the
//! FTP control channel.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio_native_tls::TlsStream;

use crate::error::{Error, Result};

/// A connection that may or may not be wrapped in TLS.
pub enum Stream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl Stream {
    pub fn is_tls(&self) -> bool {
        matches!(self, Stream::Tls(_))
    }

    pub fn as_tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::Tls(s) => s.get_ref().get_ref().get_ref(),
        }
    }
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stream::Plain(_) => f.write_str("Stream::Plain"),
            Stream::Tls(_) => f.write_str("Stream::Tls"),
        }
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// The FTP control connection is line oriented, so wrap the stream with a
/// small buffer that hands back one CRLF-terminated line at a time.
pub struct ControlStream {
    inner: Stream,
    buf: Vec<u8>,
    start: usize,
    end: usize,
}

impl ControlStream {
    pub fn new(inner: Stream) -> Self {
        Self {
            inner,
            buf: vec![0u8; 4096],
            start: 0,
            end: 0,
        }
    }

    pub fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }

    pub fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.inner.as_tcp().peer_addr()
    }

    pub fn into_inner(self) -> Stream {
        self.inner
    }

    fn find_newline(&self) -> Option<usize> {
        self.buf[self.start..self.end]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| self.start + i)
    }

    /// Read one line, without its trailing CRLF.
    pub async fn read_line(&mut self, timeout: Duration) -> Result<String> {
        loop {
            if let Some(nl) = self.find_newline() {
                let mut line = self.buf[self.start..nl].to_vec();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.start = nl + 1;
                if self.start >= self.end {
                    self.start = 0;
                    self.end = 0;
                }
                return Ok(String::from_utf8_lossy(&line).into_owned());
            }

            // Compact, then grow if a single line fills the buffer.
            if self.start > 0 {
                self.buf.copy_within(self.start..self.end, 0);
                self.end -= self.start;
                self.start = 0;
            }
            if self.end == self.buf.len() {
                if self.buf.len() >= 64 * 1024 {
                    return Err(Error::Protocol("control line longer than 64 KiB".into()));
                }
                let new_len = self.buf.len() * 2;
                self.buf.resize(new_len, 0);
            }

            let n = tokio::time::timeout(timeout, self.inner.read(&mut self.buf[self.end..]))
                .await
                .map_err(|_| Error::Timeout(timeout))??;
            if n == 0 {
                return Err(Error::Protocol(
                    "control connection closed by server".to_string(),
                ));
            }
            self.end += n;
        }
    }

    /// Send one command line.
    ///
    /// Takes bytes rather than `&str`: a path or credential may have been
    /// converted into a legacy character set, and such a command is not valid
    /// UTF-8 any more.
    pub async fn write_line(&mut self, line: &[u8], timeout: Duration) -> Result<()> {
        let mut data = Vec::with_capacity(line.len() + 2);
        data.extend_from_slice(line);
        data.extend_from_slice(b"\r\n");
        tokio::time::timeout(timeout, self.inner.write_all(&data))
            .await
            .map_err(|_| Error::Timeout(timeout))??;
        tokio::time::timeout(timeout, self.inner.flush())
            .await
            .map_err(|_| Error::Timeout(timeout))??;
        Ok(())
    }
}

/// Read up to `limit` bytes from a data connection, used by listing commands.
pub async fn read_to_end_capped(
    stream: &mut Stream,
    limit: usize,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 8192];
    loop {
        let n = tokio::time::timeout(timeout, stream.read(&mut chunk))
            .await
            .map_err(|_| Error::Timeout(timeout))??;
        if n == 0 {
            return Ok(out);
        }
        if out.len() + n > limit {
            return Err(Error::Protocol(format!("listing exceeded {limit} bytes")));
        }
        out.extend_from_slice(&chunk[..n]);
    }
}

/// Close the write direction politely; data connections are closed early when a
/// segment is complete, which is what tells the server to stop sending.
pub async fn shutdown_quietly(stream: &mut Stream) {
    let _ = stream.shutdown().await;
}
