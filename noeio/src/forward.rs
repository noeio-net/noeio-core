//! `noeio forward`: a foreground, user-space port forwarder.
//!
//! One process, one rule, no state anywhere else. The daemon is asked once
//! at start-up which overlay addresses this node has (FR-6.2); after that the
//! forwarder is an ordinary program holding ordinary sockets, and the kernel
//! reclaims everything when the process ends by any means (FR-3.4). This
//! module lives outside `daemon/` on purpose: nothing in here runs inside
//! `noeio boot`.

pub mod proxy;
pub mod rule;
pub mod udp;

use crate::rpc::client::CliRpcClient;
use proxy::{Forwarder, Shutdown};
use rule::{HostAddrs, Proto, Rule, Side};
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

pub use rule::{ListenSpec, RuleError};

/// Upper bound on the graceful exit (FR-3.3). Everything in it is
/// cancellation, so it normally completes in milliseconds.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Exit codes of the command (§6.3).
pub const EXIT_OK: i32 = 0;
pub const EXIT_ENV: i32 = 1;
pub const EXIT_USAGE: i32 = 2;

/// Command-line input, as parsed by clap (strings kept raw so that the rule
/// gate can report every problem at once, FR-1.3).
#[derive(Debug, Clone)]
pub struct Args {
    pub listen: String,
    pub target: String,
    pub proto: Proto,
    pub allow_from: Vec<String>,
}

/// Debug-build test hook (AC-19): `NOEIO_FORWARD_HOST_ADDRS` supplies the
/// [`HostAddrs`] snapshot instead of the daemon RPC, so the process-level
/// lifecycle tests need neither root nor a daemon. Format:
/// `overlay=IP[,IP];peers=IP[,IP];lan=IP[,IP]`. Absent from release builds.
#[cfg(debug_assertions)]
pub const HOST_ADDRS_ENV: &str = "NOEIO_FORWARD_HOST_ADDRS";

#[cfg(debug_assertions)]
fn host_addrs_override() -> Option<Result<HostAddrs, String>> {
    let raw = std::env::var(HOST_ADDRS_ENV).ok()?;
    let mut host = HostAddrs {
        lan_enumerated: true,
        ..Default::default()
    };
    for part in raw.split(';').filter(|p| !p.trim().is_empty()) {
        let Some((key, list)) = part.split_once('=') else {
            return Some(Err(format!("{HOST_ADDRS_ENV}: bad segment '{part}'")));
        };
        let ips: Result<Vec<Ipv4Addr>, _> = list
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().parse::<Ipv4Addr>())
            .collect();
        let ips = match ips {
            Ok(ips) => ips,
            Err(err) => return Some(Err(format!("{HOST_ADDRS_ENV}: {part}: {err}"))),
        };
        match key.trim() {
            "overlay" => host.overlay = ips,
            "peers" => host.peers = ips,
            "lan" => host.lan = ips,
            other => return Some(Err(format!("{HOST_ADDRS_ENV}: unknown key '{other}'"))),
        }
    }
    Some(Ok(host))
}

/// Gather the machine's addresses: overlay side from the daemon, LAN side
/// from the interface table (FR-3.1 steps 1–2). Fails when the daemon is
/// not reachable (FR-6.3); there is no bypass.
pub async fn discover_host_addrs() -> Result<HostAddrs, String> {
    #[cfg(debug_assertions)]
    if let Some(forced) = host_addrs_override() {
        return forced;
    }

    let mut client = CliRpcClient::new()
        .await
        .map_err(|err| format!("failed to connect to daemon: {err}\nIs `noeio boot` running?"))?;
    let nics = client
        .list_virtual_nics()
        .await
        .map_err(|err| format!("failed to query the daemon's virtual nics: {err}"))?;

    let mut overlay = Vec::new();
    let mut peers = Vec::new();
    for nic in nics {
        match nic.ip.parse::<Ipv4Addr>() {
            Ok(ip) => overlay.push(ip),
            Err(_) => tracing::warn!(nic = nic.tun_name, ip = nic.ip, "skipping non-IPv4 nic"),
        }
        peers.extend(
            nic.peer_ips
                .iter()
                .filter_map(|s| s.parse::<Ipv4Addr>().ok()),
        );
    }
    overlay.sort();
    overlay.dedup();
    peers.sort();
    peers.dedup();

    let exclude: Vec<IpAddr> = overlay.iter().copied().map(IpAddr::V4).collect();
    Ok(HostAddrs {
        overlay,
        peers,
        lan: crate::daemon::routes::local_addrs(&exclude),
        lan_enumerated: crate::daemon::routes::can_enumerate_interfaces(),
    })
}

/// The start-up banner (FR-5.2, FR-8.1, FR-8.4). Ends with the one thing the
/// user has to know: how to stop.
pub fn banner(rule: &Rule) -> String {
    let mut out = format!(
        "forwarding {} {} -> {}\n",
        rule.proto, rule.listen, rule.target
    );
    for addr in &rule.binds {
        out.push_str(&format!("  bound {addr}\n"));
    }
    if !rule.allow_from.is_empty() {
        let list: Vec<String> = rule.allow_from.iter().map(ToString::to_string).collect();
        out.push_str(&format!(
            "  allow-from {} (source-IP filter, not authentication)\n",
            list.join(", ")
        ));
    }
    match rule.side {
        Side::Lan => out.push_str(&format!(
            "WARNING: this exposes {} to the whole LAN without authentication\n",
            rule.target
        )),
        Side::Overlay => out.push_str(&format!(
            "note: {} is reachable by every peer of the overlay network(s) above\n",
            rule.target
        )),
    }
    if rule.target.ip().is_loopback() {
        out.push_str(&format!(
            "WARNING: {} listens on loopback only; this rule makes it reachable from the {} side\n",
            rule.target, rule.side
        ));
    }
    if rule.proto == Proto::Udp {
        out.push_str(&format!(
            "  udp sessions: one per client address, reclaimed after {}s idle, at most {}\n",
            udp::SESSION_TTL.as_secs(),
            udp::SESSION_CAP
        ));
    }
    out.push_str("press Ctrl+C to stop\n");
    out
}

/// The exit summary (FR-5.4).
pub fn summary(done: &Shutdown, stats: &proxy::Stats, proto: Proto) -> String {
    let (unit, verb) = match proto {
        Proto::Tcp => ("connection", "aborted"),
        Proto::Udp => ("session", "dropped"),
    };
    format!(
        "shutting down: {} listener{} closed, {} {}{} {}\n{}\n",
        done.listeners_closed,
        if done.listeners_closed == 1 { "" } else { "s" },
        done.connections_aborted,
        unit,
        if done.connections_aborted == 1 {
            ""
        } else {
            "s"
        },
        verb,
        stats.summary(proto)
    )
}

/// The whole command (FR-3.1 → FR-3.3). Returns the process exit code;
/// diagnostics go to stderr, banner and summary to stdout (§6.4).
pub async fn run(args: Args) -> i32 {
    let host = match discover_host_addrs().await {
        Ok(h) => h,
        Err(msg) => {
            eprintln!("{msg}");
            return EXIT_ENV;
        }
    };

    let rule = match rule::validate(
        &args.listen,
        &args.target,
        args.proto,
        &args.allow_from,
        &host,
    ) {
        Ok(rule) => rule,
        Err(errors) => {
            eprintln!("invalid forward rule:");
            for err in errors {
                eprintln!("  - {err}");
            }
            return EXIT_USAGE;
        }
    };

    let listeners = match proxy::bind_all(&rule).await {
        Ok(l) => l,
        Err(err) => {
            eprintln!("{err}");
            return EXIT_ENV;
        }
    };

    print!("{}", banner(&rule));
    let proto = rule.proto;
    let forwarder = Forwarder::start(rule, listeners);

    let signal = crate::signal::wait_for_shutdown().await;
    tracing::info!(signal, "shutdown signal received, stopping forwarder");

    let stats = forwarder.stats.clone();
    match tokio::time::timeout(SHUTDOWN_TIMEOUT, forwarder.shutdown()).await {
        Ok(done) => {
            print!("{}", summary(&done, &stats, proto));
            EXIT_OK
        }
        Err(_) => {
            // Nothing needs to be cleaned up: the kernel reclaims the
            // sockets with the process.
            eprintln!("shutdown did not finish within {SHUTDOWN_TIMEOUT:?}; exiting");
            std::process::exit(EXIT_OK);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddrV4;

    fn rule(side: Side, target: &str, allow: &[&str]) -> Rule {
        let target: SocketAddrV4 = target.parse().unwrap();
        let (listen, binds) = match side {
            Side::Overlay => (
                ListenSpec::Noeio(8080),
                vec!["110.20.0.1:8080".parse().unwrap()],
            ),
            Side::Lan => (
                ListenSpec::Lan(9090),
                vec!["192.168.10.3:9090".parse().unwrap()],
            ),
        };
        Rule {
            listen,
            side,
            binds,
            target,
            proto: Proto::Tcp,
            allow_from: allow.iter().map(|s| s.parse().unwrap()).collect(),
        }
    }

    /// FR-5.2 / FR-8.1 / FR-8.4: rule line, bound addresses, warnings, and
    /// the fixed last line.
    #[test]
    fn banner_contents() {
        let text = banner(&rule(Side::Overlay, "192.168.10.7:80", &[]));
        assert!(text.starts_with("forwarding tcp noeio:8080 -> 192.168.10.7:80\n"));
        assert!(text.contains("  bound 110.20.0.1:8080\n"));
        assert!(!text.contains("WARNING"));
        assert!(text.ends_with("press Ctrl+C to stop\n"));

        let text = banner(&rule(Side::Lan, "110.20.0.9:22", &[]));
        assert!(text.contains("  bound 192.168.10.3:9090\n"));
        assert!(text.contains(
            "WARNING: this exposes 110.20.0.9:22 to the whole LAN without authentication\n"
        ));

        let text = banner(&rule(Side::Overlay, "127.0.0.1:5432", &["110.20.0.0/24"]));
        assert!(text.contains("127.0.0.1:5432 listens on loopback only"));
        assert!(text.contains("allow-from 110.20.0.0/24"));

        let mut udp_rule = rule(Side::Overlay, "192.168.10.1:53", &[]);
        udp_rule.proto = Proto::Udp;
        let text = banner(&udp_rule);
        assert!(text.starts_with("forwarding udp noeio:8080 -> 192.168.10.1:53\n"));
        assert!(text.contains(
            "udp sessions: one per client address, reclaimed after 60s idle, at most 512"
        ));
    }

    #[test]
    fn summary_lines() {
        let stats = proxy::Stats::default();
        let done = Shutdown {
            listeners_closed: 1,
            connections_aborted: 3,
        };
        let text = summary(&done, &stats, Proto::Tcp);
        assert!(text.starts_with("shutting down: 1 listener closed, 3 connections aborted\n"));
        assert!(text.contains("total: 0 connections, 0 B in, 0 B out"));
        let text = summary(&done, &stats, Proto::Udp);
        assert!(text.starts_with("shutting down: 1 listener closed, 3 sessions dropped\n"));
        assert!(text.contains("total: 0 sessions"));
    }

    #[cfg(debug_assertions)]
    #[test]
    fn host_addrs_override_parses() {
        // Serialize env access with the other env test via a unique key
        // would be nicer, but this is the only test touching the variable.
        unsafe {
            std::env::set_var(
                HOST_ADDRS_ENV,
                "overlay=110.20.0.1; peers=110.20.0.9,110.20.0.12 ;lan=192.168.10.3",
            );
        }
        let host = host_addrs_override().unwrap().unwrap();
        unsafe { std::env::remove_var(HOST_ADDRS_ENV) };
        assert_eq!(
            host.overlay,
            vec!["110.20.0.1".parse::<Ipv4Addr>().unwrap()]
        );
        assert_eq!(host.peers.len(), 2);
        assert_eq!(host.lan, vec!["192.168.10.3".parse::<Ipv4Addr>().unwrap()]);
        assert!(host.lan_enumerated);
        assert!(host_addrs_override().is_none());
    }
}
