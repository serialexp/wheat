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
//! command needed. And it counts the WebSocket data frames each way, which are the
//! protocol's messages. They are counts, so a contract on them holds on any machine.

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
    /// WebSocket data frames: the protocol sends each message as one.
    pub(crate) client_messages: u64,
    pub(crate) server_messages: u64,
}

#[derive(Default)]
struct Counters {
    turns: AtomicU64,
    client_bytes: AtomicU64,
    server_bytes: AtomicU64,
    client_messages: AtomicU64,
    server_messages: AtomicU64,
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
            client_messages: self.counters.client_messages.swap(0, Ordering::Relaxed),
            server_messages: self.counters.server_messages.swap(0, Ordering::Relaxed),
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
    let mut frames = FrameCounter::default();
    loop {
        let read = match from.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let arrived = Instant::now();
        count(side, read, &last, &counters);
        let messages = frames.feed(&buffer[..read]);
        match side {
            Side::Client => &counters.client_messages,
            Side::Server => &counters.server_messages,
        }
        .fetch_add(messages, Ordering::Relaxed);
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

/// Counts the WebSocket data frames in one direction of a connection, past its HTTP
/// upgrade, from bytes split however they arrive.
#[derive(Default)]
struct FrameCounter {
    state: FrameState,
}

enum FrameState {
    /// In the HTTP upgrade, with this many bytes of its closing `\r\n\r\n` seen.
    Upgrade { matched: usize },
    /// In a frame header, with `have` of its bytes read.
    Header { header: [u8; 14], have: usize },
    /// In a frame's payload, with `left` bytes to go.
    Payload { left: u64 },
}

impl Default for FrameState {
    fn default() -> Self {
        Self::Upgrade { matched: 0 }
    }
}

impl FrameCounter {
    /// Reads `bytes` and returns how many data frames begin in them.
    fn feed(&mut self, mut bytes: &[u8]) -> u64 {
        const END_OF_HEADERS: &[u8] = b"\r\n\r\n";
        let mut frames = 0;
        while let Some(&byte) = bytes.first() {
            match &mut self.state {
                FrameState::Upgrade { matched } => {
                    bytes = &bytes[1..];
                    *matched = if byte == END_OF_HEADERS[*matched] {
                        *matched + 1
                    } else {
                        usize::from(byte == b'\r')
                    };
                    if *matched == END_OF_HEADERS.len() {
                        self.state = FrameState::Header {
                            header: [0; 14],
                            have: 0,
                        };
                    }
                }
                FrameState::Header { header, have } => {
                    bytes = &bytes[1..];
                    header[*have] = byte;
                    *have += 1;
                    if let Some((payload, data)) = frame_header(&header[..*have]) {
                        frames += u64::from(data);
                        self.state = if payload == 0 {
                            FrameState::Header {
                                header: [0; 14],
                                have: 0,
                            }
                        } else {
                            FrameState::Payload { left: payload }
                        };
                    }
                }
                FrameState::Payload { left } => {
                    let skipped = (*left).min(bytes.len() as u64);
                    bytes = &bytes[skipped as usize..];
                    *left -= skipped;
                    if *left == 0 {
                        self.state = FrameState::Header {
                            header: [0; 14],
                            have: 0,
                        };
                    }
                }
            }
        }
        frames
    }
}

/// The payload length of a complete frame header, and whether the frame carries
/// data rather than control; `None` while `header` is still short.
fn frame_header(header: &[u8]) -> Option<(u64, bool)> {
    let [first, second, ..] = *header else {
        return None;
    };
    let extended = match second & 0x7f {
        126 => 2,
        127 => 8,
        _ => 0,
    };
    let mask = if second & 0x80 == 0 { 0 } else { 4 };
    if header.len() < 2 + extended + mask {
        return None;
    }
    let payload = match extended {
        0 => u64::from(second & 0x7f),
        _ => header[2..2 + extended]
            .iter()
            .fold(0, |length, &byte| length << 8 | u64::from(byte)),
    };
    // Opcodes 0x8 and up are close, ping and pong.
    Some((payload, first & 0x08 == 0))
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

    /// One WebSocket frame with `opcode` and `payload`, masked as a client's are.
    fn frame(opcode: u8, payload: usize, masked: bool) -> Vec<u8> {
        let mask = if masked { 0x80 } else { 0 };
        let mut bytes = vec![0x80 | opcode];
        match payload {
            0..=125 => bytes.push(mask | payload as u8),
            126..=0xffff => {
                bytes.push(mask | 126);
                bytes.extend_from_slice(&(payload as u16).to_be_bytes());
            }
            _ => {
                bytes.push(mask | 127);
                bytes.extend_from_slice(&(payload as u64).to_be_bytes());
            }
        }
        if masked {
            bytes.extend_from_slice(&[1, 2, 3, 4]);
        }
        bytes.resize(bytes.len() + payload, 0x81);
        bytes
    }

    #[test]
    fn the_frame_counter_counts_data_frames_however_the_bytes_are_split() {
        let mut stream =
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\r\n\r\n".to_vec();
        for (opcode, payload, masked) in [
            (0x2, 5, false),
            (0x2, 300, true),
            (0x9, 0, true),
            (0x2, 70_000, false),
            (0x2, 0, true),
            (0x8, 2, false),
        ] {
            stream.extend(frame(opcode, payload, masked));
        }

        assert_eq!(FrameCounter::default().feed(&stream), 4);
        let mut counter = FrameCounter::default();
        let one_at_a_time: u64 = stream.iter().map(|byte| counter.feed(&[*byte])).sum();
        assert_eq!(one_at_a_time, 4);
        let mut counter = FrameCounter::default();
        let in_pieces: u64 = stream.chunks(7).map(|piece| counter.feed(piece)).sum();
        assert_eq!(in_pieces, 4);
    }
}
