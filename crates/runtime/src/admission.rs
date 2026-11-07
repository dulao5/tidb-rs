//! Q1, statement admission (N3 3.6, decision D34).
//!
//! Go limits concurrently executing commands with `token-limit` (default 1000, config
//! validation maps 0 to 1000 and caps it at 1 << 20): `clientConn.dispatch` takes a token
//! from `server.concurrentLimiter` (`util.TokenLimiter`) before running a command and puts it
//! back after; when none is left it **waits**, it never rejects. [`TokenLimiter`] is that file
//! (`pkg/util/tokenlimiter.go`, ported whole per constitution 4.4); [`Admission`] adds the
//! runtime's queue metrics and the switches of the functional exceptions, which are off.
//!
//! The Go metrics of `server.getToken` / `releaseToken` (`tidb_server_tokens`,
//! `tidb_server_get_token_duration_seconds`) are recorded by the caller, as in Go (block
//! D03.1; `tidb_metrics::token_acquired` / `token_released`).
//!
//! Waiting order: Go's tokens are a buffered channel, and goroutines blocked receiving from
//! a channel are woken in FIFO order; tokio's `Semaphore` is fair (FIFO) too.
//!
//! go-file: pkg/util/tokenlimiter.go

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use prometheus::IntGauge;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

use crate::NotSupported;
use crate::metrics::Queue;
use crate::metrics::QueueMetrics;

/// Go `util.Token`: the permission to keep on running. Dropping it puts it back.
#[derive(Debug)]
pub struct Token {
    _permit: OwnedSemaphorePermit,
    running: Option<IntGauge>,
}

impl Drop for Token {
    // glue: runtime queue metric
    fn drop(&mut self) {
        if let Some(g) = &self.running {
            g.dec();
        }
    }
}

/// Go `util.TokenLimiter`: limits the number of concurrent tasks.
#[derive(Debug, Clone)]
pub struct TokenLimiter {
    count: usize,
    sem: Arc<Semaphore>,
}

impl TokenLimiter {
    // go: TokenLimiter.Put L2 (a token is a semaphore permit: putting it back is dropping it)
    pub fn put(&self, tk: Token) {
        drop(tk);
    }

    // go: TokenLimiter.Get L2 (receive from the token channel → acquire a FIFO semaphore permit)
    pub async fn get(&self) -> Token {
        let permit = self
            .sem
            .clone()
            .acquire_owned()
            .await
            .expect("the token semaphore is never closed");
        Token {
            _permit: permit,
            running: None,
        }
    }

    // go: NewTokenLimiter L2 (a channel pre-filled with count tokens → a semaphore with count permits)
    pub fn new(count: usize) -> TokenLimiter {
        TokenLimiter {
            count,
            sem: Arc::new(Semaphore::new(count)),
        }
    }

    /// The `count` the limiter was created with.
    // glue: accessor (Go reads the unexported field in tests)
    pub fn count(&self) -> usize {
        self.count
    }

    /// Tokens currently available.
    // glue: accessor (Go: len(tl.ch))
    pub fn available(&self) -> usize {
        self.sem.available_permits()
    }
}

/// Q1 configuration. The defaults are Go's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionConfig {
    /// Go config `token-limit`, after `Config.Valid` (0 → 1000, at most 1 << 20).
    pub token_limit: usize,
    /// Functional exception: per-resource-group shares of the tokens. Off (Go has none).
    pub per_group_shares: bool,
    /// Functional exception: reject a statement that waited longer than this with a new
    /// error code (not 1317). Off (Go waits forever).
    pub reject_after: Option<Duration>,
}

impl Default for AdmissionConfig {
    // glue: Go defaults
    fn default() -> AdmissionConfig {
        AdmissionConfig {
            token_limit: 1000,
            per_group_shares: false,
            reject_after: None,
        }
    }
}

/// Q1: the server's statement admission. One per server, shared by every connection.
#[derive(Clone)]
pub struct Admission {
    limiter: TokenLimiter,
    metrics: Arc<QueueMetrics>,
}

impl Admission {
    /// Fails when a functional exception is switched on: they are not implemented yet.
    // glue: N3 3.6 Q1
    pub fn new(cfg: &AdmissionConfig) -> Result<Admission, NotSupported> {
        if cfg.per_group_shares {
            return Err(NotSupported("statement admission per resource group"));
        }
        if cfg.reject_after.is_some() {
            return Err(NotSupported("rejecting statements on admission timeout"));
        }
        Ok(Admission {
            limiter: TokenLimiter::new(cfg.token_limit),
            metrics: Arc::new(QueueMetrics::new(Queue::Admission)),
        })
    }

    /// Wait for a token (Go `server.getToken` without its Go metrics). Cancel-safe: dropping
    /// the future gives up the place in the queue.
    // glue: N3 3.6 Q1 (TokenLimiter.Get + runtime queue metrics)
    pub async fn get(&self) -> Token {
        let start = Instant::now();
        let waiting = Waiting::new(&self.metrics.waiting);
        let mut tk = self.limiter.get().await;
        drop(waiting);
        self.metrics.wait.observe(start.elapsed().as_secs_f64());
        self.metrics.running.inc();
        tk.running = Some(self.metrics.running.clone());
        tk
    }

    /// Give the token back (Go `server.releaseToken` without its Go metrics).
    // glue: N3 3.6 Q1 (TokenLimiter.Put)
    pub fn put(&self, tk: Token) {
        self.limiter.put(tk);
    }

    // glue: accessor
    pub fn limiter(&self) -> &TokenLimiter {
        &self.limiter
    }
}

/// Counts a waiter in a queue's `waiting` gauge for as long as it lives (also when the
/// waiting future is dropped).
pub(crate) struct Waiting<'a>(&'a IntGauge);

impl<'a> Waiting<'a> {
    // glue: queue metric guard
    pub(crate) fn new(g: &'a IntGauge) -> Waiting<'a> {
        g.inc();
        Waiting(g)
    }
}

impl Drop for Waiting<'_> {
    // glue: queue metric guard
    fn drop(&mut self) {
        self.0.dec();
    }
}
