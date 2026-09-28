//! UDP forwarding (FR-4). UDP has no connection to give a flow a lifetime,
//! so one is kept here: a session per client `(addr, port)`, holding the
//! socket that faces the target and the time of the last datagram in either
//! direction. Idle sessions are reclaimed after [`SESSION_TTL`]; the table
//! never holds more than [`SESSION_CAP`] sessions, because UDP sources are
//! forgeable and an unbounded table is a memory-exhaustion path (FR-4.3).
//! The table is process memory and vanishes with the process (FR-4.5).

use super::proxy::{RECV_ERROR_BACKOFF, Stats, admit, should_log};
use super::rule::Rule;
use std::collections::HashMap;
use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Idle time after which a session is reclaimed (FR-4.2).
pub const SESSION_TTL: Duration = Duration::from_secs(60);

/// Maximum number of concurrent sessions per rule. A safety property, not a
/// setting (FR-4.3).
pub const SESSION_CAP: usize = 512;

/// Largest datagram we relay: the UDP maximum payload.
const MAX_DATAGRAM: usize = 65_535;

/// A session is identified by the listener it arrived on and the client;
/// the reply has to leave through the same listener socket.
pub type SessionKey = (SocketAddr, SocketAddr);

/// The bounded, TTL-reclaimed session table (AC-20). Generic over the value
/// so the reclaim logic is testable without sockets; the forwarder stores a
/// [`Session`].
#[derive(Debug)]
pub struct SessionTable<K, V> {
    ttl: Duration,
    cap: usize,
    entries: HashMap<K, Entry<V>>,
}

#[derive(Debug)]
struct Entry<V> {
    value: V,
    last_active: Instant,
}

impl<K: Hash + Eq + Clone, V> SessionTable<K, V> {
    pub fn new(ttl: Duration, cap: usize) -> Self {
        Self {
            ttl,
            cap,
            entries: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.cap
    }

    /// Look up a session and mark it active.
    pub fn touch(&mut self, key: &K, now: Instant) -> Option<&V> {
        let e = self.entries.get_mut(key)?;
        e.last_active = now;
        Some(&e.value)
    }

    /// Add a session. Refused (the value is handed back) when the table is
    /// full: new sessions are dropped, active ones are never evicted.
    pub fn insert(&mut self, key: K, value: V, now: Instant) -> Result<(), V> {
        if self.is_full() {
            return Err(value);
        }
        self.entries.insert(
            key,
            Entry {
                value,
                last_active: now,
            },
        );
        Ok(())
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.entries.remove(key).map(|e| e.value)
    }

    /// Drop every session idle for longer than the TTL and hand them back.
    pub fn expire(&mut self, now: Instant) -> Vec<V> {
        let ttl = self.ttl;
        let dead: Vec<K> = self
            .entries
            .iter()
            .filter(|(_, e)| now.saturating_duration_since(e.last_active) >= ttl)
            .map(|(k, _)| k.clone())
            .collect();
        dead.iter()
            .filter_map(|k| self.entries.remove(k))
            .map(|e| e.value)
            .collect()
    }

    /// Drop everything (shutdown).
    pub fn clear(&mut self) -> Vec<V> {
        self.entries.drain().map(|(_, e)| e.value).collect()
    }
}

/// One live client: its target-facing socket and the task relaying the
/// target's replies. Dropping the session aborts the task, which closes the
/// socket.
pub struct Session {
    upstream: Arc<UdpSocket>,
    /// `None` only after [`Session::into_task`] took it.
    reverse: Option<JoinHandle<()>>,
}

impl Session {
    fn new(upstream: Arc<UdpSocket>, reverse: JoinHandle<()>) -> Self {
        Self {
            upstream,
            reverse: Some(reverse),
        }
    }

    /// Take the relay task out so the caller can abort *and await* it.
    pub fn into_task(mut self) -> JoinHandle<()> {
        self.reverse.take().expect("task taken once")
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(task) = &self.reverse {
            task.abort();
        }
    }
}

pub type SharedTable = Arc<Mutex<SessionTable<SessionKey, Session>>>;

pub fn new_shared_table() -> SharedTable {
    Arc::new(Mutex::new(SessionTable::new(SESSION_TTL, SESSION_CAP)))
}

fn sync_active(stats: &Stats, table: &SessionTable<SessionKey, Session>) {
    stats.active.store(table.len() as u64, Ordering::Relaxed);
}

/// The receive loop of one listener socket: admit, find or open the
/// session, forward the datagram to the target.
pub async fn recv_loop(
    listener: Arc<UdpSocket>,
    rule: Arc<Rule>,
    stats: Arc<Stats>,
    sessions: SharedTable,
) {
    let local = match listener.local_addr() {
        Ok(a) => a,
        Err(err) => {
            tracing::error!("udp listener has no local address: {err}");
            return;
        }
    };
    tracing::info!(addr = %local, "udp receive loop started");
    let target = rule.target;
    let mut buf = vec![0u8; MAX_DATAGRAM];
    loop {
        let (n, peer) = match listener.recv_from(&mut buf).await {
            Ok(x) => x,
            Err(err) => {
                let n = Stats::bump(&stats.accept_errors);
                if should_log(n) {
                    tracing::warn!(
                        addr = %local,
                        errors = n,
                        "recv failed (has the bound address gone away?): {err}"
                    );
                }
                tokio::time::sleep(RECV_ERROR_BACKOFF).await;
                continue;
            }
        };
        if !admit(&rule, &stats, peer) {
            continue;
        }
        let key: SessionKey = (local, peer);
        let now = Instant::now();

        let upstream = {
            let mut table = sessions.lock().unwrap();
            table.touch(&key, now).map(|s| s.upstream.clone())
        };
        let upstream = match upstream {
            Some(u) => u,
            None => {
                // New client. The cap is checked before a socket is opened
                // so a flood of sources cannot even burn descriptors.
                if sessions.lock().unwrap().is_full() {
                    let d = Stats::bump(&stats.session_limit_drops);
                    if should_log(d) {
                        tracing::warn!(
                            %peer,
                            dropped = d,
                            "udp session table full ({SESSION_CAP}); dropping datagram from new client"
                        );
                    }
                    continue;
                }
                let upstream = match open_upstream(target).await {
                    Ok(u) => Arc::new(u),
                    Err(err) => {
                        let f = Stats::bump(&stats.target_failures);
                        if should_log(f) {
                            tracing::warn!(%peer, %target, failures = f, "cannot open udp socket toward target: {err}");
                        }
                        continue;
                    }
                };
                let reverse = tokio::spawn(reverse(
                    upstream.clone(),
                    listener.clone(),
                    key,
                    stats.clone(),
                    sessions.clone(),
                ));
                let session = Session::new(upstream.clone(), reverse);
                let mut table = sessions.lock().unwrap();
                if table.insert(key, session, now).is_err() {
                    // Raced with another listener of the same rule.
                    Stats::bump(&stats.session_limit_drops);
                    continue;
                }
                Stats::bump(&stats.total);
                sync_active(&stats, &table);
                tracing::debug!(%peer, %target, sessions = table.len(), "udp session opened");
                upstream
            }
        };

        match upstream.send(&buf[..n]).await {
            // `bytes_in` counts what reached the target, so datagrams
            // dropped above (table full, no upstream) never enter it.
            Ok(sent) => {
                stats.bytes_in.fetch_add(sent as u64, Ordering::Relaxed);
            }
            Err(err) => {
                // Typically ECONNREFUSED surfaced from an earlier ICMP port
                // unreachable: the target is down. Drop the session so the
                // next datagram retries from scratch (FR-3.5).
                let f = Stats::bump(&stats.target_failures);
                if should_log(f) {
                    tracing::warn!(%peer, %target, failures = f, "udp send to target failed: {err}");
                }
                let mut table = sessions.lock().unwrap();
                table.remove(&key);
                sync_active(&stats, &table);
            }
        }
    }
}

/// A socket that talks to the target only; the kernel picks the source
/// address by route (the LAN address for a LAN target, the overlay address
/// for a peer).
async fn open_upstream(target: std::net::SocketAddrV4) -> std::io::Result<UdpSocket> {
    let socket = UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).await?;
    socket.connect(target).await?;
    Ok(socket)
}

/// Relay the target's replies back to the client through the listener
/// socket the request arrived on, keeping the session alive.
async fn reverse(
    upstream: Arc<UdpSocket>,
    listener: Arc<UdpSocket>,
    key: SessionKey,
    stats: Arc<Stats>,
    sessions: SharedTable,
) {
    let client = key.1;
    let mut buf = vec![0u8; MAX_DATAGRAM];
    loop {
        match upstream.recv(&mut buf).await {
            Ok(n) => {
                if let Err(err) = listener.send_to(&buf[..n], client).await {
                    tracing::debug!(%client, "udp reply to client failed: {err}");
                    continue;
                }
                stats.bytes_out.fetch_add(n as u64, Ordering::Relaxed);
                sessions.lock().unwrap().touch(&key, Instant::now());
            }
            Err(err) => {
                // ICMP unreachable from the target. End the session; a new
                // datagram from the client opens a fresh one.
                let f = Stats::bump(&stats.target_failures);
                if should_log(f) {
                    tracing::warn!(%client, failures = f, "udp target unreachable: {err}");
                }
                let mut table = sessions.lock().unwrap();
                // Removing our own Session aborts this task; nothing after
                // this line runs, which is fine.
                table.remove(&key);
                sync_active(&stats, &table);
                return;
            }
        }
    }
}

/// Periodically reclaim idle sessions (FR-4.2).
pub async fn sweeper(sessions: SharedTable, stats: Arc<Stats>) {
    let period = (SESSION_TTL / 4).max(Duration::from_millis(250));
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let expired = {
            let mut table = sessions.lock().unwrap();
            let expired = table.expire(Instant::now());
            sync_active(&stats, &table);
            expired
        };
        if !expired.is_empty() {
            tracing::debug!(count = expired.len(), "udp sessions expired");
        }
        // Dropping the sessions aborts their relay tasks.
        drop(expired);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forward::proxy::tests::{free_port, loopback_rule};
    use crate::forward::proxy::{Forwarder, bind_all};
    use crate::forward::rule::Proto;
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn key(port: u16) -> SessionKey {
        let l = SocketAddr::from((Ipv4Addr::LOCALHOST, 1));
        (l, SocketAddr::from((Ipv4Addr::new(10, 0, 0, 1), port)))
    }

    /// AC-20: an idle session is reclaimed at the TTL; one that saw traffic
    /// is kept. Paused time makes the boundary exact.
    #[tokio::test(start_paused = true)]
    async fn ttl_reclaims_idle_sessions_only() {
        let ttl = Duration::from_secs(60);
        let mut t: SessionTable<SessionKey, &str> = SessionTable::new(ttl, 512);
        let t0 = Instant::now();
        t.insert(key(1), "idle", t0).unwrap();
        t.insert(key(2), "busy", t0).unwrap();

        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(
            t.expire(Instant::now()).is_empty(),
            "nothing before the TTL"
        );
        assert_eq!(t.touch(&key(2), Instant::now()), Some(&"busy"));

        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(t.expire(Instant::now()), vec!["idle"]);
        assert_eq!(t.len(), 1);
        assert!(t.touch(&key(1), Instant::now()).is_none());
        assert!(t.touch(&key(2), Instant::now()).is_some());

        tokio::time::advance(ttl).await;
        assert_eq!(t.expire(Instant::now()), vec!["busy"]);
        assert!(t.is_empty());
    }

    /// AC-20: the cap refuses the 513th session and evicts nothing.
    #[test]
    fn cap_refuses_new_sessions_without_evicting() {
        let mut t: SessionTable<SessionKey, u16> = SessionTable::new(SESSION_TTL, SESSION_CAP);
        let now = Instant::now();
        for p in 0..SESSION_CAP as u16 {
            t.insert(key(p), p, now).unwrap();
        }
        assert!(t.is_full());
        assert_eq!(t.insert(key(9999), 9999, now), Err(9999));
        assert_eq!(t.len(), SESSION_CAP);
        for p in 0..SESSION_CAP as u16 {
            assert_eq!(t.touch(&key(p), now), Some(&p), "session {p} survived");
        }
        // Once one leaves, one may enter.
        assert_eq!(t.remove(&key(0)), Some(0));
        t.insert(key(9999), 9999, now).unwrap();
    }

    /// AC-20: sessions are keyed by source; the same port on two hosts, or
    /// two ports on one host, are different sessions.
    #[test]
    fn sessions_are_per_source() {
        let mut t: SessionTable<SessionKey, &str> = SessionTable::new(SESSION_TTL, SESSION_CAP);
        let now = Instant::now();
        let l = SocketAddr::from((Ipv4Addr::LOCALHOST, 1));
        let a1 = (l, SocketAddr::from((Ipv4Addr::new(10, 0, 0, 1), 5000)));
        let a2 = (l, SocketAddr::from((Ipv4Addr::new(10, 0, 0, 1), 5001)));
        let b1 = (l, SocketAddr::from((Ipv4Addr::new(10, 0, 0, 2), 5000)));
        t.insert(a1, "a1", now).unwrap();
        t.insert(a2, "a2", now).unwrap();
        t.insert(b1, "b1", now).unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t.remove(&a1), Some("a1"));
        assert_eq!(t.touch(&a2, now), Some(&"a2"));
        assert_eq!(t.touch(&b1, now), Some(&"b1"));
    }

    async fn udp_echo() -> SocketAddrV4 {
        let s = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = match s.local_addr().unwrap() {
            SocketAddr::V4(a) => a,
            _ => unreachable!(),
        };
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                let (n, from) = s.recv_from(&mut buf).await.unwrap();
                s.send_to(&buf[..n], from).await.unwrap();
            }
        });
        addr
    }

    async fn client() -> UdpSocket {
        UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap()
    }

    async fn exchange(c: &UdpSocket, to: SocketAddrV4, payload: &[u8]) -> Vec<u8> {
        c.send_to(payload, to).await.unwrap();
        let mut buf = [0u8; 2048];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), c.recv_from(&mut buf))
            .await
            .expect("reply within 2s")
            .unwrap();
        buf[..n].to_vec()
    }

    /// End to end through the forwarder: datagrams echo, one client is one
    /// session no matter how many datagrams, a second client is a second
    /// session, and shutdown reports the sessions it dropped.
    #[tokio::test]
    async fn udp_relay_round_trip_and_sessions() {
        let target = udp_echo().await;
        let rule = loopback_rule(Proto::Udp, free_port(), target, vec![]);
        let bind = rule.binds[0];
        let fwd = Forwarder::start(rule.clone(), bind_all(&rule).await.unwrap());

        let c1 = client().await;
        for i in 0..5u8 {
            let msg = vec![i; 100 + i as usize];
            assert_eq!(exchange(&c1, bind, &msg).await, msg);
        }
        let c2 = client().await;
        assert_eq!(exchange(&c2, bind, b"second").await, b"second");

        assert_eq!(fwd.stats.total.load(Ordering::Relaxed), 2);
        assert_eq!(fwd.stats.active(), 2);
        let sent: u64 = (0..5u64).map(|i| 100 + i).sum::<u64>() + 6;
        assert_eq!(fwd.stats.bytes_in.load(Ordering::Relaxed), sent);
        assert_eq!(fwd.stats.bytes_out.load(Ordering::Relaxed), sent);

        let done = tokio::time::timeout(Duration::from_secs(2), fwd.shutdown())
            .await
            .expect("shutdown is bounded");
        assert_eq!(done.listeners_closed, 1);
        assert_eq!(done.connections_aborted, 2);
        // The port is free again right away.
        std::net::UdpSocket::bind(bind).unwrap();
    }

    /// FR-8.3 for UDP: a refused source never opens a session.
    #[tokio::test]
    async fn udp_allow_from_rejects_before_a_session_exists() {
        let target = udp_echo().await;
        let rule = loopback_rule(Proto::Udp, free_port(), target, vec!["10.99.0.0/16"]);
        let bind = rule.binds[0];
        let fwd = Forwarder::start(rule.clone(), bind_all(&rule).await.unwrap());

        let c = client().await;
        c.send_to(b"nope", bind).await.unwrap();
        let mut buf = [0u8; 16];
        assert!(
            tokio::time::timeout(Duration::from_millis(300), c.recv_from(&mut buf))
                .await
                .is_err(),
            "no reply for a refused source"
        );
        assert_eq!(fwd.stats.rejected.load(Ordering::Relaxed), 1);
        assert_eq!(fwd.stats.total.load(Ordering::Relaxed), 0);
        fwd.shutdown().await;
    }
}
