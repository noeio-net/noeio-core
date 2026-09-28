//! The user-space proxy: bind every address of a rule (all or nothing), then
//! for TCP accept, filter by `--allow-from`, open a second connection to the
//! target and copy bytes both ways (FR-2.5, FR-3.5, FR-5.1). The UDP path
//! lives in [`super::udp`]; this module owns the tasks of both.
//!
//! Every listener, every in-flight connection and every UDP session is a
//! tokio task owned by the [`Forwarder`]; dropping or aborting them closes
//! the sockets. Nothing here outlives the process, by construction.

use super::rule::{Proto, Rule};
use super::udp;
use std::net::{IpAddr, SocketAddr, SocketAddrV4};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::JoinSet;

/// How long a connection to the target may take before the client is
/// dropped and the attempt counted as a target failure.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Pause after an accept()/recv() error so a permanently broken listener
/// (its address went away, FR-3.6) does not spin.
pub(super) const RECV_ERROR_BACKOFF: Duration = Duration::from_millis(250);

/// Counters of one rule (FR-5.1). `bytes_in` flows client → target,
/// `bytes_out` target → client. For UDP a "connection" is a session.
#[derive(Debug, Default)]
pub struct Stats {
    pub active: AtomicU64,
    pub total: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    pub target_failures: AtomicU64,
    pub rejected: AtomicU64,
    /// accept() errors (TCP) or recv_from() errors (UDP) on a listener.
    pub accept_errors: AtomicU64,
    /// UDP only: datagrams from new clients dropped because the session
    /// table was full (FR-4.3).
    pub session_limit_drops: AtomicU64,
}

impl Stats {
    pub(super) fn bump(counter: &AtomicU64) -> u64 {
        counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn active(&self) -> u64 {
        self.active.load(Ordering::Relaxed)
    }

    /// The second line of the exit summary (FR-5.4).
    pub fn summary(&self, proto: Proto) -> String {
        let mut out = format!(
            "total: {} {}, {} in, {} out, {} target failures, {} rejected by allow-from",
            self.total.load(Ordering::Relaxed),
            match proto {
                Proto::Tcp => "connections",
                Proto::Udp => "sessions",
            },
            human_bytes(self.bytes_in.load(Ordering::Relaxed)),
            human_bytes(self.bytes_out.load(Ordering::Relaxed)),
            self.target_failures.load(Ordering::Relaxed),
            self.rejected.load(Ordering::Relaxed),
        );
        match proto {
            Proto::Tcp => out.push_str(&format!(
                ", {} accept errors",
                self.accept_errors.load(Ordering::Relaxed)
            )),
            Proto::Udp => out.push_str(&format!(
                ", {} dropped at session limit ({}), {} receive errors",
                self.session_limit_drops.load(Ordering::Relaxed),
                udp::SESSION_CAP,
                self.accept_errors.load(Ordering::Relaxed)
            )),
        }
        out
    }
}

/// `4.2 MiB` style rendering for the summary.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Log on the first occurrence and every 1000th after that, the same
/// rate-limiting the daemon uses for AllowedIPs drops.
pub(super) fn should_log(count: u64) -> bool {
    count == 1 || count.is_multiple_of(1000)
}

/// One bound address, in the rule's protocol.
#[derive(Debug)]
pub enum Listener {
    Tcp(TcpListener),
    Udp(UdpSocket),
}

/// Bind every address of the rule. Any failure releases what was already
/// bound and reports which address failed (FR-2.5).
pub async fn bind_all(rule: &Rule) -> Result<Vec<Listener>, BindError> {
    let mut listeners = Vec::with_capacity(rule.binds.len());
    for addr in &rule.binds {
        let bound = match rule.proto {
            Proto::Tcp => TcpListener::bind(*addr).await.map(Listener::Tcp),
            Proto::Udp => UdpSocket::bind(*addr).await.map(Listener::Udp),
        };
        match bound {
            Ok(l) => {
                if addr.port() < 1024 {
                    tracing::warn!(%addr, "bound a privileged port (<1024)");
                }
                listeners.push(l);
            }
            // `listeners` is dropped on return: all-or-nothing.
            Err(source) => {
                return Err(BindError {
                    addr: *addr,
                    source,
                });
            }
        }
    }
    Ok(listeners)
}

#[derive(Debug)]
pub struct BindError {
    pub addr: SocketAddrV4,
    pub source: std::io::Error,
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to bind {}: {}", self.addr, self.source)
    }
}

impl std::error::Error for BindError {}

/// What a graceful shutdown released (first line of the exit summary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shutdown {
    pub listeners_closed: usize,
    /// TCP: in-flight connections aborted. UDP: sessions dropped.
    pub connections_aborted: usize,
}

/// One running rule: its listener loops and its in-flight state.
pub struct Forwarder {
    rule: Arc<Rule>,
    pub stats: Arc<Stats>,
    /// Accept loops (TCP) or receive loops plus the sweeper (UDP).
    tasks: JoinSet<()>,
    /// TCP: in-flight connections. Finished tasks are reaped on each accept
    /// so the set does not grow without bound. Never held across an await.
    conns: Arc<Mutex<JoinSet<()>>>,
    /// UDP: the session table shared by every listener of the rule, so the
    /// cap is per rule (FR-4.3).
    sessions: udp::SharedTable,
    listeners: usize,
}

impl Forwarder {
    /// Start one loop per listener (FR-3.1 step 6).
    pub fn start(rule: Rule, listeners: Vec<Listener>) -> Self {
        let rule = Arc::new(rule);
        let stats = Arc::new(Stats::default());
        let conns: Arc<Mutex<JoinSet<()>>> = Arc::new(Mutex::new(JoinSet::new()));
        let sessions = udp::new_shared_table();
        let mut tasks = JoinSet::new();
        let count = listeners.len();
        for listener in listeners {
            match listener {
                Listener::Tcp(listener) => {
                    tasks.spawn(accept_loop(
                        listener,
                        rule.clone(),
                        stats.clone(),
                        conns.clone(),
                    ));
                }
                Listener::Udp(socket) => {
                    tasks.spawn(udp::recv_loop(
                        Arc::new(socket),
                        rule.clone(),
                        stats.clone(),
                        sessions.clone(),
                    ));
                }
            }
        }
        if rule.proto == Proto::Udp {
            tasks.spawn(udp::sweeper(sessions.clone(), stats.clone()));
        }
        Self {
            rule,
            stats,
            tasks,
            conns,
            sessions,
            listeners: count,
        }
    }

    pub fn rule(&self) -> &Rule {
        &self.rule
    }

    /// Stop accepting, abort every in-flight connection / drop every UDP
    /// session, close the listeners (FR-3.2). Everything here is
    /// cancellation; the awaits only collect the aborted tasks.
    pub async fn shutdown(mut self) -> Shutdown {
        // Aborting the listener tasks drops their sockets.
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}

        let mut aborted = 0;

        let mut conns = std::mem::take(&mut *self.conns.lock().unwrap());
        conns.abort_all();
        while let Some(res) = conns.join_next().await {
            if res.is_err_and(|e| e.is_cancelled()) {
                aborted += 1;
            }
        }

        let sessions = self.sessions.lock().unwrap().clear();
        for session in sessions {
            let task = session.into_task();
            task.abort();
            let _ = task.await;
            aborted += 1;
        }

        for addr in &self.rule.binds {
            tracing::info!(%addr, "listener closed");
        }
        Shutdown {
            listeners_closed: self.listeners,
            connections_aborted: aborted,
        }
    }
}

async fn accept_loop(
    listener: TcpListener,
    rule: Arc<Rule>,
    stats: Arc<Stats>,
    conns: Arc<Mutex<JoinSet<()>>>,
) {
    let local = listener.local_addr().ok();
    tracing::info!(addr = ?local, "accept loop started");
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let mut set = conns.lock().unwrap();
                while set.try_join_next().is_some() {}
                set.spawn(handle_conn(stream, peer, rule.clone(), stats.clone()));
            }
            Err(err) => {
                let n = Stats::bump(&stats.accept_errors);
                if should_log(n) {
                    tracing::warn!(
                        addr = ?local,
                        errors = n,
                        "accept failed (has the bound address gone away?): {err}"
                    );
                }
                tokio::time::sleep(RECV_ERROR_BACKOFF).await;
            }
        }
    }
}

/// The source of a datagram or connection as an IPv4 address, if the rule
/// accepts it. Counts and (rate-limited) logs a refusal (FR-8.3).
pub(super) fn admit(rule: &Rule, stats: &Stats, peer: SocketAddr) -> bool {
    let allowed = match peer.ip() {
        IpAddr::V4(v4) => rule.allows(v4),
        // The listener is bound to an IPv4 address; anything else cannot be
        // matched against the v4 filter, so it is refused.
        IpAddr::V6(_) => false,
    };
    if !allowed {
        let n = Stats::bump(&stats.rejected);
        if should_log(n) {
            tracing::warn!(%peer, rejected = n, "refused by --allow-from");
        }
    }
    allowed
}

/// Decrements `active` however the task ends, including abort.
struct ActiveGuard(Arc<Stats>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn handle_conn(mut client: TcpStream, peer: SocketAddr, rule: Arc<Rule>, stats: Arc<Stats>) {
    if !admit(&rule, &stats, peer) {
        return;
    }

    Stats::bump(&stats.total);
    stats.active.fetch_add(1, Ordering::Relaxed);
    let _active = ActiveGuard(stats.clone());

    let target = rule.target;
    let upstream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(target)).await {
        Ok(Ok(s)) => s,
        Ok(Err(err)) => {
            let n = Stats::bump(&stats.target_failures);
            if should_log(n) {
                tracing::warn!(%peer, %target, failures = n, "target unreachable: {err}");
            }
            return;
        }
        Err(_) => {
            let n = Stats::bump(&stats.target_failures);
            if should_log(n) {
                tracing::warn!(%peer, %target, failures = n, "target connect timed out");
            }
            return;
        }
    };

    tracing::debug!(%peer, %target, "connection opened");
    // Bytes are counted on the target-facing socket as they pass, so a
    // connection that ends in an error (RST mid-transfer) still reports what
    // it moved; `copy_bidirectional` itself returns no counts on Err.
    let mut upstream = Counted::new(upstream, stats.clone());
    let result = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    let (to_target, to_client) = (upstream.written, upstream.read);
    match result {
        Ok(_) => {
            tracing::debug!(%peer, %target, in_ = to_target, out = to_client, "connection closed");
        }
        Err(err) => {
            tracing::debug!(%peer, %target, in_ = to_target, out = to_client, "connection ended with error: {err}");
        }
    }
}

/// A stream that adds what it moves to the rule's counters as it goes:
/// writes are client → target (`bytes_in`), reads are target → client
/// (`bytes_out`).
struct Counted<S> {
    inner: S,
    stats: Arc<Stats>,
    written: u64,
    read: u64,
}

impl<S> Counted<S> {
    fn new(inner: S, stats: Arc<Stats>) -> Self {
        Self {
            inner,
            stats,
            written: 0,
            read: 0,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Counted<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let res = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &res {
            let n = (buf.filled().len() - before) as u64;
            this.read += n;
            this.stats.bytes_out.fetch_add(n, Ordering::Relaxed);
        }
        res
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Counted<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let res = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &res {
            this.written += *n as u64;
            this.stats.bytes_in.fetch_add(*n as u64, Ordering::Relaxed);
        }
        res
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::forward::rule::{ListenSpec, Proto, Side};
    use std::net::Ipv4Addr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn echo_server() -> SocketAddrV4 {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = match l.local_addr().unwrap() {
            SocketAddr::V4(a) => a,
            _ => unreachable!(),
        };
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        addr
    }

    pub(in crate::forward) fn free_port() -> u16 {
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// The tests bind loopback, which `validate` refuses on purpose; build
    /// the rule directly.
    pub(in crate::forward) fn loopback_rule(
        proto: Proto,
        port: u16,
        target: SocketAddrV4,
        allow: Vec<&str>,
    ) -> Rule {
        Rule {
            listen: ListenSpec::Addr(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)),
            side: Side::Lan,
            binds: vec![SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)],
            target,
            proto,
            allow_from: allow.into_iter().map(|s| s.parse().unwrap()).collect(),
        }
    }

    fn tcp_rule(port: u16, target: SocketAddrV4, allow: Vec<&str>) -> Rule {
        loopback_rule(Proto::Tcp, port, target, allow)
    }

    /// AC-18: bytes survive both directions and a FIN propagates.
    #[tokio::test]
    async fn echo_round_trip_and_close_propagation() {
        let target = echo_server().await;
        let rule = tcp_rule(free_port(), target, vec![]);
        let bind = rule.binds[0];
        let listeners = bind_all(&rule).await.unwrap();
        let fwd = Forwarder::start(rule, listeners);

        let mut c = TcpStream::connect(bind).await.unwrap();
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let (mut r, mut w) = c.split();
        let writer = async {
            w.write_all(&payload).await.unwrap();
            w.shutdown().await.unwrap();
        };
        let reader = async {
            let mut got = Vec::new();
            r.read_to_end(&mut got).await.unwrap();
            got
        };
        let (_, got) = tokio::join!(writer, reader);
        assert_eq!(got, payload, "echo must match byte for byte");

        // The FIN we sent reached the echo server through the proxy and its
        // FIN came back: read_to_end returned. Give the task a moment to
        // record its counters.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(fwd.stats.total.load(Ordering::Relaxed), 1);
        assert_eq!(
            fwd.stats.bytes_in.load(Ordering::Relaxed),
            payload.len() as u64
        );
        assert_eq!(
            fwd.stats.bytes_out.load(Ordering::Relaxed),
            payload.len() as u64
        );
        assert_eq!(fwd.stats.active(), 0);

        let done = fwd.shutdown().await;
        assert_eq!(done.listeners_closed, 1);
        assert_eq!(done.connections_aborted, 0);
        // The port is free again right away.
        std::net::TcpListener::bind(bind).unwrap();
    }

    /// A connection that ends in an error (client sends RST instead of FIN)
    /// still has its bytes in the counters.
    #[tokio::test]
    async fn bytes_are_counted_when_the_connection_errors() {
        let target = echo_server().await;
        let rule = tcp_rule(free_port(), target, vec![]);
        let bind = rule.binds[0];
        let listeners = bind_all(&rule).await.unwrap();
        let fwd = Forwarder::start(rule, listeners);

        let mut c = TcpStream::connect(bind).await.unwrap();
        let payload = vec![0xA5u8; 100_000];
        c.write_all(&payload).await.unwrap();
        let mut got = vec![0u8; payload.len()];
        c.read_exact(&mut got).await.unwrap();
        assert_eq!(got, payload);

        // SO_LINGER 0 turns the close into a reset. tokio deprecates the
        // setter because a non-zero linger blocks on drop; zero does not.
        #[allow(deprecated)]
        c.set_linger(Some(Duration::ZERO)).unwrap();
        drop(c);
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(
            fwd.stats.bytes_in.load(Ordering::Relaxed),
            payload.len() as u64
        );
        assert_eq!(
            fwd.stats.bytes_out.load(Ordering::Relaxed),
            payload.len() as u64
        );
        assert_eq!(fwd.stats.active(), 0);
        fwd.shutdown().await;
    }

    /// AC-12: a dead target is counted and does not stop the forwarder.
    #[tokio::test]
    async fn dead_target_is_counted_and_forwarder_survives() {
        let dead = SocketAddrV4::new(Ipv4Addr::LOCALHOST, free_port());
        let rule = tcp_rule(free_port(), dead, vec![]);
        let bind = rule.binds[0];
        let listeners = bind_all(&rule).await.unwrap();
        let fwd = Forwarder::start(rule, listeners);

        let mut c = TcpStream::connect(bind).await.unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(c.read(&mut buf).await.unwrap(), 0, "client sees EOF");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(fwd.stats.target_failures.load(Ordering::Relaxed), 1);

        // Still accepting.
        TcpStream::connect(bind).await.unwrap();
        fwd.shutdown().await;
    }

    /// AC-15: a source outside --allow-from is closed and counted.
    #[tokio::test]
    async fn allow_from_rejects_and_counts() {
        let target = echo_server().await;
        let rule = tcp_rule(free_port(), target, vec!["10.99.0.0/16"]);
        let bind = rule.binds[0];
        let listeners = bind_all(&rule).await.unwrap();
        let fwd = Forwarder::start(rule, listeners);

        let mut c = TcpStream::connect(bind).await.unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(c.read(&mut buf).await.unwrap(), 0);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(fwd.stats.rejected.load(Ordering::Relaxed), 1);
        assert_eq!(fwd.stats.total.load(Ordering::Relaxed), 0);
        fwd.shutdown().await;
    }

    /// AC-6 at the library level: an in-flight connection is aborted by
    /// shutdown, the client sees the socket close, the port is reusable.
    #[tokio::test]
    async fn shutdown_aborts_in_flight_connections() {
        let target = echo_server().await;
        let rule = tcp_rule(free_port(), target, vec![]);
        let bind = rule.binds[0];
        let listeners = bind_all(&rule).await.unwrap();
        let fwd = Forwarder::start(rule, listeners);

        let mut c = TcpStream::connect(bind).await.unwrap();
        c.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        assert_eq!(fwd.stats.active(), 1);

        let done = tokio::time::timeout(Duration::from_secs(2), fwd.shutdown())
            .await
            .expect("shutdown is bounded");
        assert_eq!(done.connections_aborted, 1);

        // The proxy dropped its end: we read EOF or a reset.
        let mut tail = [0u8; 16];
        match c.read(&mut tail).await {
            Ok(0) => {}
            Ok(n) => panic!("unexpected {n} bytes after shutdown"),
            Err(err) => assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset),
        }
        std::net::TcpListener::bind(bind).unwrap();
    }

    /// AC-11: two binds, the second one taken → nothing stays bound.
    #[tokio::test]
    async fn bind_is_all_or_nothing() {
        let target = echo_server().await;
        let free = free_port();
        let taken = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let taken_port = taken.local_addr().unwrap().port();
        let rule = Rule {
            binds: vec![
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, free),
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, taken_port),
            ],
            ..tcp_rule(free, target, vec![])
        };
        let err = bind_all(&rule).await.unwrap_err();
        assert_eq!(err.addr.port(), taken_port);
        assert!(err.to_string().contains(&taken_port.to_string()));
        // The first address was released again.
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, free)).unwrap();
    }

    #[test]
    fn human_bytes_renders_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(4_404_019), "4.2 MiB");
        assert_eq!(human_bytes(124_361_113), "118.6 MiB");
    }

    #[test]
    fn summary_names_the_unit_per_protocol() {
        let stats = Stats::default();
        assert!(stats.summary(Proto::Tcp).contains("0 connections"));
        assert!(stats.summary(Proto::Tcp).contains("accept errors"));
        let udp = stats.summary(Proto::Udp);
        assert!(udp.contains("0 sessions"));
        assert!(udp.contains("dropped at session limit (512)"));
    }
}
