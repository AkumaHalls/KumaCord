//! The HTTP listener: hyper's HTTP/1 server with the limits `axum::serve` does not set.
//!
//! `axum::serve` gives hyper no timer, so hyper's header-read timeout silently never fires, and
//! it takes every connection it is offered. On a home PC open to the internet that lets anybody,
//! without logging in, hold any number of sockets (each with up to ~400 KiB of half-sent request
//! head) forever. Here:
//!
//! - **Header-read / idle timeout** ([`Limits::header_read_timeout`]): a request head must arrive
//!   in full within this time, and an idle keep-alive connection is closed after it (hyper starts
//!   the same timer while it waits for the next request).
//! - **Smaller read buffer** ([`Limits::max_buf`]) for request heads.
//! - **Connection caps**, global and per address ([`Limits::max_conns`],
//!   [`Limits::max_conns_per_ip`]): one more is closed right after `accept`. A WebSocket keeps its
//!   connection's slot for as long as it lives (the [`ConnSlot`] travels with the upgrade, see
//!   `ws.rs`), so long-lived sockets count too.
//! - **Request timeout** ([`Limits::request_timeout`], [`Limits::login_timeout`]): the time to
//!   answer one request (body upload included), applied by [`timeout_layer`].
//!
//! v1.0.2: the hub may listen on several sockets (IPv4 and IPv6, `listen.rs`): each runs its own
//! [`serve`] with the same [`Slots`], so the caps count all of them together. An IPv4 peer on a
//! dual-stack socket is turned back into plain IPv4 here ([`crate::listen::canonical_peer`]), and
//! an IPv6 peer counts against the per-address cap by its /64 network
//! ([`crate::listen::peer_group`]).

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{ConnectInfo, Request};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Router;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tower::ServiceExt;

/// The listener's limits.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Connections open at once, all addresses together.
    pub max_conns: usize,
    /// Connections open at once from one IP address.
    pub max_conns_per_ip: usize,
    /// A request head must arrive within this; also how long an idle keep-alive connection stays.
    pub header_read_timeout: Duration,
    /// Time to answer one request (reading its body included).
    pub request_timeout: Duration,
    /// Same, for the unauthenticated `POST /api/login` and `GET /api/health`.
    pub login_timeout: Duration,
    /// hyper's read buffer (request heads must fit in it).
    pub max_buf: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_conns: 1024,
            max_conns_per_ip: 128,
            header_read_timeout: Duration::from_secs(20),
            request_timeout: Duration::from_secs(120),
            login_timeout: Duration::from_secs(20),
            max_buf: 64 * 1024,
        }
    }
}

#[derive(Default)]
struct Counts {
    total: usize,
    per_ip: HashMap<IpAddr, usize>,
}

/// Who holds which connection slots.
#[derive(Clone)]
pub struct Slots {
    limits: Arc<Limits>,
    counts: Arc<Mutex<Counts>>,
}

/// One open connection's place in the caps; freed when the last clone is dropped (the
/// connection closed, or the WebSocket it became ended).
#[derive(Clone)]
pub struct ConnSlot(#[allow(dead_code)] Arc<SlotInner>);

struct SlotInner {
    ip: IpAddr,
    counts: Arc<Mutex<Counts>>,
}

impl Drop for SlotInner {
    fn drop(&mut self) {
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        c.total = c.total.saturating_sub(1);
        if let Some(n) = c.per_ip.get_mut(&self.ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                c.per_ip.remove(&self.ip);
            }
        }
    }
}

impl Slots {
    pub fn new(limits: Limits) -> Self {
        Slots { limits: Arc::new(limits), counts: Arc::default() }
    }

    /// A slot for a new connection from `ip`, or `None` when a cap is reached.
    pub fn take(&self, ip: IpAddr) -> Option<ConnSlot> {
        let ip = crate::listen::peer_group(ip);
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        let mine = c.per_ip.get(&ip).copied().unwrap_or(0);
        if c.total >= self.limits.max_conns || mine >= self.limits.max_conns_per_ip {
            return None;
        }
        c.total += 1;
        *c.per_ip.entry(ip).or_default() += 1;
        Some(ConnSlot(Arc::new(SlotInner { ip, counts: self.counts.clone() })))
    }

    /// Connections open right now (tests).
    pub fn open(&self) -> usize {
        self.counts.lock().unwrap_or_else(|e| e.into_inner()).total
    }
}

/// Request timeout middleware (see [`Limits`]).
pub async fn timeout_layer(limits: Arc<Limits>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let limit = if path == "/api/login" || path == "/api/health" { limits.login_timeout } else { limits.request_timeout };
    match tokio::time::timeout(limit, next.run(req)).await {
        Ok(r) => r,
        Err(_) => (StatusCode::REQUEST_TIMEOUT, "request timed out").into_response(),
    }
}

/// Accept and serve connections until `shutdown`; then let the open ones finish (bounded).
pub async fn serve(listener: TcpListener, app: Router, slots: Slots, mut shutdown: watch::Receiver<bool>) {
    let limits = slots.limits.clone();
    // Every connection task holds a sender; all of them gone = all connections closed.
    let (alive_tx, mut alive_rx) = mpsc::channel::<Infallible>(1);
    let mut refused: u64 = 0;
    loop {
        let (stream, peer) = tokio::select! {
            r = listener.accept() => match r {
                Ok(x) => x,
                Err(e) => {
                    // Out of sockets / handles: back off a little instead of spinning.
                    tracing::debug!("accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
            _ = crate::state::wait_shutdown(&mut shutdown) => break,
        };
        let peer = crate::listen::canonical_peer(peer);
        let Some(slot) = slots.take(peer.ip()) else {
            refused += 1;
            if refused.is_power_of_two() {
                tracing::warn!(%peer, refused, "connection refused: too many open connections");
            }
            drop(stream);
            continue;
        };
        let _ = stream.set_nodelay(true);
        let app = app.clone();
        let (limits, alive, mut stop) = (limits.clone(), alive_tx.clone(), shutdown.clone());
        tokio::spawn(async move {
            let _alive = alive;
            let svc_slot = slot.clone();
            let svc = hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
                req.extensions_mut().insert(ConnectInfo(peer));
                req.extensions_mut().insert(svc_slot.clone());
                app.clone().oneshot(req.map(axum::body::Body::new))
            });
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(limits.header_read_timeout)
                .max_buf_size(limits.max_buf.max(8192))
                .keep_alive(true);
            let conn = builder.serve_connection(TokioIo::new(stream), svc).with_upgrades();
            tokio::pin!(conn);
            tokio::select! {
                r = conn.as_mut() => {
                    if let Err(e) = r {
                        tracing::trace!(%peer, "connection ended: {e}");
                    }
                }
                _ = crate::state::wait_shutdown(&mut stop) => {
                    conn.as_mut().graceful_shutdown();
                    let _ = tokio::time::timeout(Duration::from_secs(5), conn.as_mut()).await;
                }
            }
            drop(slot);
        });
    }
    drop(alive_tx);
    let _ = tokio::time::timeout(Duration::from_secs(5), alive_rx.recv()).await;
}
