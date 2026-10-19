//! A TCP proxy that adds a round-trip time, for the protocol cases.
//!
//! The cases run the server in-process on loopback, where a round trip costs next to
//! nothing, so a protocol that talks back and forth more than it needs to looks as
//! fast as one that doesn't. Each byte through this proxy arrives half the RTT after
//! it was sent, in both directions. Bytes keep flowing while earlier ones are still
//! delayed, so the proxy adds latency but no bandwidth limit.
//!
//! It also counts turns: bursts of client bytes that open a connection or follow
//! bytes from the server. Every turn costs one RTT, so turns are the round trips a
//! command needed. They are counts, so a contract on them holds on any machine.

use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        TcpListener, TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    runtime::Runtime,
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::Instant,
};

/// Proxies to one upstream address until [`LatencyProxy::stop`].
pub(crate) struct LatencyProxy {
    runtime: Runtime,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<()>,
    addr: SocketAddr,
    counters: Arc<Counters>,
}

/// What passed through the proxy since the last [`LatencyProxy::take`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Traffic {
    /// Client bursts that opened a connection or answered the server: round trips.
    pub(crate) turns: u64,
    pub(crate) client_bytes: u64,
    pub(crate) server_bytes: u64,
}

#[derive(Default)]
struct Counters {
    turns: AtomicU64,
    client_bytes: AtomicU64,
    server_bytes: AtomicU64,
}

/// Which side sent last on one connection; a client burst after the server's is a
/// new turn.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Client,
    Server,
}

impl LatencyProxy {
    /// Listens on a loopback port and forwards every connection to `upstream`, with
    /// `rtt` added to each round trip.
    pub(crate) fn start(upstream: SocketAddr, rtt: Duration) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0"))?;
        let addr = listener.local_addr()?;
        let counters = Arc::new(Counters::default());
        let (shutdown, mut shutdown_rx) = oneshot::channel();
        let accepting = Arc::clone(&counters);
        let task = runtime.spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => return,
                    accepted = listener.accept() => {
                        let Ok((client, _)) = accepted else { continue };
                        tokio::spawn(proxy(client, upstream, rtt / 2, Arc::clone(&accepting)));
                    }
                }
            }
        });
        Ok(Self {
            runtime,
            shutdown,
            task,
            addr,
            counters,
        })
    }

    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The traffic since the last call, which starts the next count.
    pub(crate) fn take(&self) -> Traffic {
        Traffic {
            turns: self.counters.turns.swap(0, Ordering::Relaxed),
            client_bytes: self.counters.client_bytes.swap(0, Ordering::Relaxed),
            server_bytes: self.counters.server_bytes.swap(0, Ordering::Relaxed),
        }
    }

    /// Stops accepting connections; open ones end with the runtime.
    pub(crate) fn stop(self) -> Result<()> {
        let _ = self.shutdown.send(());
        self.runtime
            .block_on(self.task)
            .context("latency proxy task")?;
        self.runtime.shutdown_background();
        Ok(())
    }
}

/// Forwards one connection both ways, each byte `delay` after it arrived.
async fn proxy(client: TcpStream, upstream: SocketAddr, delay: Duration, counters: Arc<Counters>) {
    let Ok(server) = TcpStream::connect(upstream).await else {
        return;
    };
    // The proxy must not hold back small writes on top of the delay.
    let _ = client.set_nodelay(true);
    let _ = server.set_nodelay(true);
    let (client_read, client_write) = client.into_split();
    let (server_read, server_write) = server.into_split();
    let last = Arc::new(Mutex::new(None));
    let upstream_half = direction(
        client_read,
        server_write,
        delay,
        Side::Client,
        Arc::clone(&last),
        Arc::clone(&counters),
    );
    let downstream_half = direction(
        server_read,
        client_write,
        delay,
        Side::Server,
        last,
        counters,
    );
    let _ = tokio::join!(upstream_half, downstream_half);
}

/// Copies `from` to `to`, holding each read back until `delay` after it arrived.
async fn direction(
    mut from: OwnedReadHalf,
    mut to: OwnedWriteHalf,
    delay: Duration,
    side: Side,
    last: Arc<Mutex<Option<Side>>>,
    counters: Arc<Counters>,
) {
    let (queue, mut delayed) = mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
    let writer = tokio::spawn(async move {
        while let Some((due, bytes)) = delayed.recv().await {
            tokio::time::sleep_until(due).await;
            if to.write_all(&bytes).await.is_err() {
                return;
            }
        }
        let _ = to.shutdown().await;
    });
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = match from.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let arrived = Instant::now();
        count(side, read, &last, &counters);
        if queue
            .send((arrived + delay, buffer[..read].to_vec()))
            .is_err()
        {
            break;
        }
    }
    drop(queue);
    let _ = writer.await;
}

fn count(side: Side, bytes: usize, last: &Mutex<Option<Side>>, counters: &Counters) {
    let mut last = last.lock().expect("the turn tracker is never poisoned");
    match side {
        Side::Client => {
            if *last != Some(Side::Client) {
                counters.turns.fetch_add(1, Ordering::Relaxed);
            }
            counters
                .client_bytes
                .fetch_add(bytes as u64, Ordering::Relaxed);
        }
        Side::Server => {
            counters
                .server_bytes
                .fetch_add(bytes as u64, Ordering::Relaxed);
        }
    }
    *last = Some(side);
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    /// An upstream that answers every line with the same line.
    fn echo_server(runtime: &Runtime) -> SocketAddr {
        let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let addr = listener.local_addr().unwrap();
        runtime.spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buffer = [0_u8; 1024];
                    while let Ok(read) = socket.read(&mut buffer).await {
                        if read == 0 || socket.write_all(&buffer[..read]).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        addr
    }

    #[test]
    fn each_exchange_is_one_turn_and_waits_at_least_the_rtt() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let rtt = Duration::from_millis(40);
        let proxy = LatencyProxy::start(echo_server(&runtime), rtt).unwrap();
        let mut socket = std::net::TcpStream::connect(proxy.addr()).unwrap();
        socket.set_nodelay(true).unwrap();
        let mut reply = [0_u8; 6];
        for _ in 0..3 {
            let sent = std::time::Instant::now();
            // Two writes in one turn: the second follows the first, not the server.
            socket.write_all(b"pin").unwrap();
            socket.write_all(b"g\n\n").unwrap();
            socket.read_exact(&mut reply).unwrap();
            // The delay is what this proxy is for, so it is the subject here: a reply
            // can never come back sooner than one RTT.
            assert!(sent.elapsed() >= rtt, "{:?} < {rtt:?}", sent.elapsed());
        }
        let traffic = proxy.take();
        assert_eq!(traffic.turns, 3);
        assert_eq!(traffic.client_bytes, 18);
        assert_eq!(traffic.server_bytes, 18);
        assert_eq!(proxy.take(), Traffic::default());
        drop(socket);
        proxy.stop().unwrap();
    }
}
