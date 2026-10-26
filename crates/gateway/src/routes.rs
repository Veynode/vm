//! Browser sessions in flight. A route is one browser session bridged to
//! one host: a tunnel bidirectional stream for reliable messages plus a
//! bounded queue for what the host sends outside it (video streams,
//! datagrams). The queue holds at most a couple of frames: when the browser
//! side is behind, new video is refused and the host sees it as lost, so
//! latency is shed at the host pacer instead of piling up here.
use crate::tunnel::Hosts;
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;
use vm_remote_proto::tunnel::{self, RouteOpen, GATEWAY_NOTICE};

/// Queued downlink items per route.
pub const DOWNLINK_QUEUE: usize = 2;

pub enum Downlink {
    Video(quinn::RecvStream),
    Datagram(Bytes),
}

struct Entry {
    host: usize,
    tx: mpsc::Sender<Downlink>,
}

#[derive(Default)]
pub struct Routes {
    next: AtomicU64,
    live: Mutex<HashMap<u64, Entry>>,
}

/// The tunnel half of a browser session. Its fields can be moved apart;
/// the route stays registered until `guard` drops.
pub struct Route {
    pub id: u64,
    pub host: quinn::Connection,
    pub send: quinn::SendStream,
    pub recv: quinn::RecvStream,
    pub downlink: mpsc::Receiver<Downlink>,
    pub guard: Guard,
}

/// Unregisters a route when dropped.
pub struct Guard {
    id: u64,
    routes: Arc<Routes>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.routes.live.lock().unwrap().remove(&self.id);
    }
}

impl Routes {
    pub fn count(&self) -> usize {
        self.live.lock().unwrap().len()
    }

    /// Hands a host item to its route; gives it back when the route is gone
    /// or full.
    pub fn downlink(&self, route_id: u64, item: Downlink) -> Result<(), Downlink> {
        let tx = match self.live.lock().unwrap().get(&route_id) {
            Some(e) => e.tx.clone(),
            None => return Err(item),
        };
        tx.try_send(item).map_err(|e| match e {
            mpsc::error::TrySendError::Full(i) | mpsc::error::TrySendError::Closed(i) => i,
        })
    }

    /// Drops every route of a host connection that went away.
    pub fn close_host(&self, host: usize) {
        self.live.lock().unwrap().retain(|_, e| e.host != host);
    }

    /// Opens a route to `host` (or the only connected host).
    pub async fn open(
        self: &Arc<Self>,
        hosts: &Hosts,
        host: Option<&str>,
        transport: &str,
        origin: &str,
        remote: &str,
        sessions_max: usize,
    ) -> Result<Route> {
        if self.count() >= sessions_max {
            bail!("session capacity")
        }
        let conn = match host {
            Some(name) => hosts.get(name).context("host offline")?,
            None => hosts.only().context("no single host online")?.1,
        };
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = mpsc::channel(DOWNLINK_QUEUE);
        self.live.lock().unwrap().insert(
            id,
            Entry {
                host: conn.stable_id(),
                tx,
            },
        );
        let guard = Guard {
            id,
            routes: self.clone(),
        };
        let (send, recv) = conn.open_bi().await?;
        let mut route = Route {
            id,
            host: conn,
            send,
            recv,
            downlink: rx,
            guard,
        };
        route
            .send
            .write_all(&route_open(id, transport, origin, remote)?)
            .await?;
        Ok(route)
    }
}

/// One `u32 len | message` frame, or `None` on a clean end of stream.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    let n = r.read(&mut header).await?;
    if n == 0 {
        return Ok(None);
    }
    r.read_exact(&mut header[n..]).await?;
    let mut message = vec![0u8; tunnel::frame_len(header)?];
    r.read_exact(&mut message).await?;
    Ok(Some(message))
}

/// Rejects messages a browser may not send: empty, oversized, or posing as
/// a gateway notice.
pub fn browser_message_ok(message: &[u8]) -> bool {
    !message.is_empty() && message.len() <= tunnel::MAX_MESSAGE && message[0] != GATEWAY_NOTICE
}

pub fn route_open(id: u64, transport: &str, origin: &str, remote: &str) -> Result<Vec<u8>> {
    tunnel::frame(&serde_json::to_vec(&RouteOpen {
        route_id: id,
        transport: transport.into(),
        origin: origin.into(),
        remote: remote.into(),
    })?)
}
