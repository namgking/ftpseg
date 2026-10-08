//! A small pool of logged-in control connections.
//!
//! FTP allows a single data transfer per control connection, so parallel
//! segments need parallel connections. Logging in is comparatively expensive
//! and servers often cap the number of sessions per user, so connections are
//! reused instead of being opened per segment.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::client::{ConnectOptions, FtpClient};
use crate::error::{Error, Result};

pub struct ConnectionPool {
    options: Arc<ConnectOptions>,
    limit: usize,
    idle: Mutex<Vec<FtpClient>>,
    permits: Arc<Semaphore>,
    created: AtomicUsize,
}

impl ConnectionPool {
    pub fn new(options: Arc<ConnectOptions>, limit: usize) -> Arc<Self> {
        let limit = limit.max(1);
        Arc::new(Self {
            options,
            limit,
            idle: Mutex::new(Vec::new()),
            permits: Arc::new(Semaphore::new(limit)),
            created: AtomicUsize::new(0),
        })
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Total number of connections opened so far.
    pub fn created(&self) -> usize {
        self.created.load(Ordering::Relaxed)
    }

    /// How many connections are waiting in the pool.
    pub fn idle_count(&self) -> usize {
        self.idle.lock().map(|v| v.len()).unwrap_or(0)
    }

    /// Take a connection, reusing an idle one or logging in a new one.
    pub async fn acquire(self: &Arc<Self>) -> Result<PooledConn> {
        // Held for the lifetime of the checkout, so the number of live
        // connections can never exceed the limit even if every segment starts
        // at once.
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Protocol("connection pool is closed".into()))?;

        let reused = self.idle.lock().ok().and_then(|mut idle| idle.pop());
        let client = match reused {
            Some(client) => client,
            None => {
                let client = FtpClient::connect(&self.options).await?;
                self.created.fetch_add(1, Ordering::Relaxed);
                client
            }
        };

        Ok(PooledConn {
            pool: Arc::clone(self),
            client: Some(client),
            permit: Some(permit),
            healthy: true,
        })
    }

    fn give_back(&self, client: FtpClient) {
        if let Ok(mut idle) = self.idle.lock() {
            if idle.len() < self.limit {
                idle.push(client);
            }
        }
    }

    /// Close every idle connection, e.g. before logging out.
    pub async fn shutdown(&self) {
        let clients: Vec<FtpClient> = match self.idle.lock() {
            Ok(mut idle) => idle.drain(..).collect(),
            Err(_) => Vec::new(),
        };
        for mut client in clients {
            client.quit().await;
        }
    }
}

/// A checked-out connection. Dropping it returns the connection to the pool,
/// unless it was marked broken.
pub struct PooledConn {
    pool: Arc<ConnectionPool>,
    client: Option<FtpClient>,
    permit: Option<OwnedSemaphorePermit>,
    healthy: bool,
}

impl PooledConn {
    pub fn client(&mut self) -> &mut FtpClient {
        self.client
            .as_mut()
            .expect("pooled connection is always present until drop")
    }

    /// Flag the connection as unusable so it is not handed out again.
    pub fn mark_broken(&mut self) {
        self.healthy = false;
    }

    /// `true` when the connection is still believed to be in step.
    pub fn is_healthy(&self) -> bool {
        self.healthy
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            if self.healthy {
                self.pool.give_back(client);
            }
        }
        // Dropping the permit releases the slot.
        self.permit.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn unreachable_options() -> Arc<ConnectOptions> {
        Arc::new(ConnectOptions {
            host: "127.0.0.1".to_string(),
            // Nothing listens here, so acquire() must fail rather than hang.
            port: 1,
            user: "u".into(),
            password: "p".into(),
            tls: crate::client::TlsMode::None,
            accept_invalid_certs: false,
            connect_timeout: Duration::from_millis(300),
            timeout: Duration::from_millis(300),
            verbose: false,
            charset: crate::encoding::CharsetRequest::Auto,
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_connections_release_their_slot() {
        let pool = ConnectionPool::new(unreachable_options(), 2);
        for _ in 0..4 {
            let result = pool.acquire().await;
            assert!(result.is_err(), "connect to a closed port must fail");
        }
        assert_eq!(pool.created(), 0);
        // Every permit must have been returned; otherwise this would block.
        let permit =
            tokio::time::timeout(Duration::from_secs(2), pool.permits.clone().acquire_owned())
                .await
                .expect("permits must be released after failures")
                .expect("semaphore is open");
        drop(permit);
        assert!(pool.idle_count() == 0);
    }
}
