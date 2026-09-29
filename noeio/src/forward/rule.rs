//! What one `noeio forward` invocation asks for, and the single gate every
//! rule passes before a socket is touched (FR-1).
//!
//! Everything here is pure: the inputs are the command-line strings and a
//! [`HostAddrs`] snapshot of the machine, the output is either a fully
//! resolved [`Rule`] (with the exact addresses to bind) or *every* reason it
//! was refused. That keeps AC-16 / AC-17 table-driven and socket-free.

use smoltcp::wire::Ipv4Cidr;
use std::fmt;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::str::FromStr;

/// Transport protocol of a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Proto {
    Tcp,
    Udp,
}

impl fmt::Display for Proto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        })
    }
}

/// The `--listen` argument as written: a keyword that expands to a set of
/// local addresses, or one specific address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenSpec {
    /// Every overlay address of this node (each noeio virtual nic).
    Noeio(u16),
    /// Every IPv4 address of the physical interfaces.
    Lan(u16),
    /// Exactly this local address.
    Addr(SocketAddrV4),
}

impl ListenSpec {
    pub fn port(&self) -> u16 {
        match self {
            Self::Noeio(p) | Self::Lan(p) => *p,
            Self::Addr(a) => a.port(),
        }
    }
}

impl FromStr for ListenSpec {
    type Err = RuleError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let Some((host, port)) = s.rsplit_once(':') else {
            return Err(RuleError::ListenMalformed(s.to_string()));
        };
        let port: u16 = match port.parse() {
            Ok(p) if p != 0 => p,
            _ => return Err(RuleError::ListenPort(port.to_string())),
        };
        match host {
            "noeio" => Ok(Self::Noeio(port)),
            "lan" => Ok(Self::Lan(port)),
            _ => match host.parse::<Ipv4Addr>() {
                Ok(ip) => Ok(Self::Addr(SocketAddrV4::new(ip, port))),
                Err(_) => Err(RuleError::ListenMalformed(s.to_string())),
            },
        }
    }
}

impl fmt::Display for ListenSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Noeio(p) => write!(f, "noeio:{p}"),
            Self::Lan(p) => write!(f, "lan:{p}"),
            Self::Addr(a) => write!(f, "{a}"),
        }
    }
}

/// Which side of the node a listener faces. The two never mix: that is the
/// core invariant of the feature (FR-2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Reachable by noeio peers only.
    Overlay,
    /// Reachable from the physical LAN only.
    Lan,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Overlay => "overlay",
            Self::Lan => "lan",
        })
    }
}

/// The addresses this machine holds, gathered once at start-up (FR-2.5):
/// the overlay side from the daemon (FR-6.2), the LAN side from the
/// interface table (FR-2.2).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HostAddrs {
    /// Overlay address of each local virtual nic.
    pub overlay: Vec<Ipv4Addr>,
    /// Overlay addresses of every peer the daemon currently knows.
    pub peers: Vec<Ipv4Addr>,
    /// IPv4 addresses of the physical interfaces (loopback and TUNs left out).
    pub lan: Vec<Ipv4Addr>,
    /// Whether `lan` is trustworthy. False where the interface table cannot
    /// be read (Windows, FR-2.7): the `lan` keyword is refused and a specific
    /// address that is not an overlay address is taken to be LAN-side.
    pub lan_enumerated: bool,
}

/// Why a rule was refused. Every variant is a user error (exit code 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleError {
    ListenMalformed(String),
    ListenPort(String),
    ListenUnspecified,
    ListenLoopback,
    ListenNotLocal(Ipv4Addr),
    LanNotEnumerable,
    NoOverlayAddr,
    NoLanAddr,
    TargetMalformed(String),
    TargetHostname(String),
    TargetPort(String),
    SelfLoop(SocketAddrV4),
    RelayToSelf(Ipv4Addr),
    RelayToPeer(Ipv4Addr),
    AllowFromMalformed(String),
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ListenMalformed(s) => write!(
                f,
                "--listen '{s}': expected <ADDR>:<PORT> where ADDR is `noeio`, `lan` or an IPv4 address"
            ),
            Self::ListenPort(p) => write!(f, "--listen: port '{p}' is not in 1..=65535"),
            Self::ListenUnspecified => write!(
                f,
                "--listen 0.0.0.0 is refused: it would expose the rule on both the overlay and the LAN; use `noeio`, `lan` or one specific address"
            ),
            Self::ListenLoopback => write!(
                f,
                "--listen 127.0.0.1 is refused: a loopback listener is reachable from this machine only, nothing needs forwarding there"
            ),
            Self::ListenNotLocal(ip) => write!(
                f,
                "--listen {ip}: not an address of this machine (neither an overlay address known to the daemon nor a physical interface address)"
            ),
            Self::LanNotEnumerable => write!(
                f,
                "--listen lan:<port> is not available on {}: interfaces cannot be enumerated here; write the physical address instead, e.g. --listen 192.168.10.3:<port>",
                std::env::consts::OS
            ),
            Self::NoOverlayAddr => write!(
                f,
                "--listen noeio:<port>: the daemon has no virtual nic; create one first (`noeio create vnic`)"
            ),
            Self::NoLanAddr => write!(
                f,
                "--listen lan:<port>: no physical interface with an IPv4 address was found"
            ),
            Self::TargetMalformed(s) => {
                write!(
                    f,
                    "--target '{s}': expected <IPv4>:<PORT>, e.g. 192.168.10.7:80"
                )
            }
            Self::TargetHostname(h) => write!(
                f,
                "--target '{h}': hostnames are not accepted (their resolution can change after start-up); use an IPv4 literal"
            ),
            Self::TargetPort(p) => write!(f, "--target: port '{p}' is not in 1..=65535"),
            Self::SelfLoop(addr) => write!(
                f,
                "--target {addr} is one of the addresses this command would listen on; the forwarder would connect to itself"
            ),
            Self::RelayToSelf(ip) => write!(
                f,
                "--target {ip} is this node's own overlay address while listening on the overlay side; overlay→overlay relaying is not supported"
            ),
            Self::RelayToPeer(ip) => write!(
                f,
                "--target {ip} is another peer's overlay address while listening on the overlay side; this node would become an open relay between peers, which is not supported"
            ),
            Self::AllowFromMalformed(s) => {
                write!(
                    f,
                    "--allow-from '{s}' is not a valid IPv4 CIDR (e.g. 110.20.0.0/24)"
                )
            }
        }
    }
}

impl std::error::Error for RuleError {}

/// A validated rule: what to bind, which side it faces, where to forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// `--listen` as written, for the banner.
    pub listen: ListenSpec,
    pub side: Side,
    /// Every address that gets its own listener. Never empty.
    pub binds: Vec<SocketAddrV4>,
    pub target: SocketAddrV4,
    pub proto: Proto,
    /// Empty means "no source filter".
    pub allow_from: Vec<Ipv4Cidr>,
}

impl Rule {
    /// The `--allow-from` filter (FR-8.3). A coarse source-IP check, not
    /// authentication.
    pub fn allows(&self, src: Ipv4Addr) -> bool {
        self.allow_from.is_empty() || self.allow_from.iter().any(|c| c.contains_addr(&src))
    }
}

/// Expand a listen spec into the concrete addresses to bind and the side
/// they face (FR-2.1 – FR-2.4, FR-2.7). Pure; see AC-17.
pub fn derive_binds(
    spec: &ListenSpec,
    host: &HostAddrs,
) -> Result<(Side, Vec<SocketAddrV4>), RuleError> {
    match spec {
        ListenSpec::Noeio(port) => {
            if host.overlay.is_empty() {
                return Err(RuleError::NoOverlayAddr);
            }
            let mut ips = host.overlay.clone();
            ips.sort();
            ips.dedup();
            Ok((
                Side::Overlay,
                ips.into_iter()
                    .map(|ip| SocketAddrV4::new(ip, *port))
                    .collect(),
            ))
        }
        ListenSpec::Lan(port) => {
            if !host.lan_enumerated {
                return Err(RuleError::LanNotEnumerable);
            }
            // The TUN addresses are excluded even if the interface table
            // reported them: `lan` must never face the overlay.
            let mut ips: Vec<Ipv4Addr> = host
                .lan
                .iter()
                .copied()
                .filter(|ip| !ip.is_loopback() && !host.overlay.contains(ip))
                .collect();
            ips.sort();
            ips.dedup();
            if ips.is_empty() {
                return Err(RuleError::NoLanAddr);
            }
            Ok((
                Side::Lan,
                ips.into_iter()
                    .map(|ip| SocketAddrV4::new(ip, *port))
                    .collect(),
            ))
        }
        ListenSpec::Addr(addr) => {
            let ip = *addr.ip();
            if ip.is_unspecified() {
                return Err(RuleError::ListenUnspecified);
            }
            if ip.is_loopback() {
                return Err(RuleError::ListenLoopback);
            }
            let side = if host.overlay.contains(&ip) {
                Side::Overlay
            } else if !host.lan_enumerated {
                // FR-2.7: no interface table (Windows). Every non-overlay
                // address is taken to be LAN-side; a wrong guess fails at
                // bind().
                Side::Lan
            } else if host.lan.contains(&ip) {
                Side::Lan
            } else {
                return Err(RuleError::ListenNotLocal(ip));
            };
            Ok((side, vec![*addr]))
        }
    }
}

fn parse_target(s: &str) -> Result<SocketAddrV4, RuleError> {
    let s = s.trim();
    let Some((host, port)) = s.rsplit_once(':') else {
        return Err(RuleError::TargetMalformed(s.to_string()));
    };
    let ip: Ipv4Addr = match host.parse() {
        Ok(ip) => ip,
        Err(_) => {
            // Anything that is not an address but could be a name gets the
            // more helpful message.
            let name_like = !host.is_empty()
                && host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
                && host.chars().any(|c| c.is_ascii_alphabetic());
            return Err(if name_like {
                RuleError::TargetHostname(host.to_string())
            } else {
                RuleError::TargetMalformed(s.to_string())
            });
        }
    };
    match port.parse::<u16>() {
        Ok(p) if p != 0 => Ok(SocketAddrV4::new(ip, p)),
        _ => Err(RuleError::TargetPort(port.to_string())),
    }
}

/// The single gate (FR-1.3). Collects every error instead of stopping at the
/// first, so one run of the command shows everything that has to change.
pub fn validate(
    listen: &str,
    target: &str,
    proto: Proto,
    allow_from: &[String],
    host: &HostAddrs,
) -> Result<Rule, Vec<RuleError>> {
    let mut errors = Vec::new();

    let spec = ListenSpec::from_str(listen)
        .map_err(|e| errors.push(e))
        .ok();
    let binds = spec
        .as_ref()
        .and_then(|spec| derive_binds(spec, host).map_err(|e| errors.push(e)).ok());

    let target = parse_target(target).map_err(|e| errors.push(e)).ok();

    if let (Some((side, binds)), Some(target)) = (&binds, target) {
        if binds.contains(&target) {
            errors.push(RuleError::SelfLoop(target));
        } else if *side == Side::Overlay {
            let ip = *target.ip();
            if host.overlay.contains(&ip) {
                errors.push(RuleError::RelayToSelf(ip));
            } else if host.peers.contains(&ip) {
                errors.push(RuleError::RelayToPeer(ip));
            }
        }
    }

    let mut cidrs = Vec::new();
    for raw in allow_from {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        match raw.parse::<Ipv4Cidr>() {
            Ok(c) => cidrs.push(c),
            Err(_) => errors.push(RuleError::AllowFromMalformed(raw.to_string())),
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }
    let (side, binds) = binds.expect("no errors implies binds resolved");
    Ok(Rule {
        listen: spec.expect("no errors implies listen parsed"),
        side,
        binds,
        target: target.expect("no errors implies target parsed"),
        proto,
        allow_from: cidrs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    fn sa(s: &str) -> SocketAddrV4 {
        s.parse().unwrap()
    }

    fn host() -> HostAddrs {
        HostAddrs {
            overlay: vec![ip("110.20.0.1")],
            peers: vec![ip("110.20.0.9"), ip("110.20.0.12")],
            lan: vec![ip("192.168.10.3"), ip("10.0.0.3")],
            lan_enumerated: true,
        }
    }

    fn check(
        listen: &str,
        target: &str,
        proto: Proto,
        allow: &[&str],
    ) -> Result<Rule, Vec<RuleError>> {
        let allow: Vec<String> = allow.iter().map(|s| s.to_string()).collect();
        validate(listen, target, proto, &allow, &host())
    }

    fn err(listen: &str, target: &str, proto: Proto, allow: &[&str]) -> Vec<RuleError> {
        check(listen, target, proto, allow).expect_err("rule must be refused")
    }

    /// AC-16: every malformed or forbidden rule is refused with the right
    /// error type.
    #[test]
    fn refused_rules_table() {
        use RuleError::*;
        let ok_target = "192.168.10.7:80";
        let cases: Vec<(&str, &str, Proto, &[&str], RuleError)> = vec![
            (
                "noeio",
                ok_target,
                Proto::Tcp,
                &[],
                ListenMalformed("noeio".into()),
            ),
            (
                "nope:8080",
                ok_target,
                Proto::Tcp,
                &[],
                ListenMalformed("nope:8080".into()),
            ),
            (
                "1.2.3:8080",
                ok_target,
                Proto::Tcp,
                &[],
                ListenMalformed("1.2.3:8080".into()),
            ),
            (
                "noeio:0",
                ok_target,
                Proto::Tcp,
                &[],
                ListenPort("0".into()),
            ),
            (
                "noeio:65536",
                ok_target,
                Proto::Tcp,
                &[],
                ListenPort("65536".into()),
            ),
            (
                "noeio:abc",
                ok_target,
                Proto::Tcp,
                &[],
                ListenPort("abc".into()),
            ),
            (
                "0.0.0.0:8080",
                ok_target,
                Proto::Tcp,
                &[],
                ListenUnspecified,
            ),
            ("127.0.0.1:8080", ok_target, Proto::Tcp, &[], ListenLoopback),
            (
                "172.16.0.9:8080",
                ok_target,
                Proto::Tcp,
                &[],
                ListenNotLocal(ip("172.16.0.9")),
            ),
            (
                "noeio:8080",
                "192.168.10.7",
                Proto::Tcp,
                &[],
                TargetMalformed("192.168.10.7".into()),
            ),
            (
                "noeio:8080",
                "192.168.10.7:0",
                Proto::Tcp,
                &[],
                TargetPort("0".into()),
            ),
            (
                "noeio:8080",
                "192.168.10.7:65536",
                Proto::Tcp,
                &[],
                TargetPort("65536".into()),
            ),
            (
                "noeio:8080",
                "nas.local:80",
                Proto::Tcp,
                &[],
                TargetHostname("nas.local".into()),
            ),
            (
                "noeio:8080",
                "[::1]:80",
                Proto::Tcp,
                &[],
                TargetMalformed("[::1]:80".into()),
            ),
            // Self loop: the target is exactly what we would bind.
            (
                "noeio:8080",
                "110.20.0.1:8080",
                Proto::Tcp,
                &[],
                SelfLoop(sa("110.20.0.1:8080")),
            ),
            (
                "192.168.10.3:9090",
                "192.168.10.3:9090",
                Proto::Tcp,
                &[],
                SelfLoop(sa("192.168.10.3:9090")),
            ),
            // Relay refusal: overlay-side listener, overlay target.
            (
                "noeio:8080",
                "110.20.0.1:22",
                Proto::Tcp,
                &[],
                RelayToSelf(ip("110.20.0.1")),
            ),
            (
                "noeio:8080",
                "110.20.0.9:22",
                Proto::Tcp,
                &[],
                RelayToPeer(ip("110.20.0.9")),
            ),
            (
                "110.20.0.1:8080",
                "110.20.0.1:22",
                Proto::Tcp,
                &[],
                RelayToSelf(ip("110.20.0.1")),
            ),
            (
                "110.20.0.1:8080",
                "110.20.0.12:22",
                Proto::Tcp,
                &[],
                RelayToPeer(ip("110.20.0.12")),
            ),
            (
                "noeio:8080",
                ok_target,
                Proto::Tcp,
                &["110.20.0.0/24", "junk"],
                AllowFromMalformed("junk".into()),
            ),
            (
                "noeio:8080",
                ok_target,
                Proto::Tcp,
                &["110.20.0.5"],
                AllowFromMalformed("110.20.0.5".into()),
            ),
        ];
        for (listen, target, proto, allow, expected) in cases {
            let errors = err(listen, target, proto, allow);
            assert_eq!(
                errors,
                vec![expected.clone()],
                "listen={listen} target={target} proto={proto} allow={allow:?}"
            );
        }
    }

    /// FR-1.3: all errors are reported at once, not just the first.
    #[test]
    fn all_errors_are_collected() {
        let errors = err("0.0.0.0:0", "nas:0", Proto::Udp, &["x"]);
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(matches!(errors[0], RuleError::ListenPort(_)));
        assert!(matches!(errors[1], RuleError::TargetHostname(_)));
        assert!(matches!(errors[2], RuleError::AllowFromMalformed(_)));
    }

    /// LAN → overlay is the direction subnet routing cannot do; a lan-side
    /// listener may target a peer.
    #[test]
    fn lan_side_may_target_overlay_peer() {
        let rule = check("lan:9090", "110.20.0.9:22", Proto::Tcp, &[]).unwrap();
        assert_eq!(rule.side, Side::Lan);
        assert_eq!(
            rule.binds,
            vec![sa("10.0.0.3:9090"), sa("192.168.10.3:9090")]
        );
        assert_eq!(rule.target, sa("110.20.0.9:22"));
    }

    #[test]
    fn udp_rules_pass_the_same_gate() {
        let rule = check("noeio:53", "192.168.10.1:53", Proto::Udp, &[]).unwrap();
        assert_eq!(rule.proto, Proto::Udp);
        assert_eq!(rule.binds, vec![sa("110.20.0.1:53")]);
        let errors = err("noeio:53", "110.20.0.9:53", Proto::Udp, &[]);
        assert_eq!(errors, vec![RuleError::RelayToPeer(ip("110.20.0.9"))]);
    }

    #[test]
    fn overlay_side_to_lan_and_loopback_targets() {
        let rule = check(
            "noeio:8080",
            "192.168.10.7:80",
            Proto::Tcp,
            &["110.20.0.0/24"],
        )
        .unwrap();
        assert_eq!(rule.side, Side::Overlay);
        assert_eq!(rule.binds, vec![sa("110.20.0.1:8080")]);
        assert_eq!(
            rule.allow_from,
            vec!["110.20.0.0/24".parse::<Ipv4Cidr>().unwrap()]
        );
        assert!(rule.allows(ip("110.20.0.9")));
        assert!(!rule.allows(ip("110.21.0.9")));

        let rule = check("noeio:5432", "127.0.0.1:5432", Proto::Tcp, &[]).unwrap();
        assert!(rule.target.ip().is_loopback());
        assert!(rule.allows(ip("1.2.3.4")), "no allow-from means everyone");
    }

    /// AC-17: bind derivation for the three listen shapes.
    #[test]
    fn derive_binds_expands_keywords_and_keeps_addresses() {
        let mut h = host();
        h.overlay.push(ip("110.30.0.1"));
        // `lan` from the interface table may include the TUN, or a loopback
        // address on a non-lo interface; both are dropped here.
        h.lan.push(ip("110.20.0.1"));
        h.lan.push(ip("127.0.0.1"));

        let (side, binds) = derive_binds(&ListenSpec::Noeio(8080), &h).unwrap();
        assert_eq!(side, Side::Overlay);
        assert_eq!(binds, vec![sa("110.20.0.1:8080"), sa("110.30.0.1:8080")]);

        let (side, binds) = derive_binds(&ListenSpec::Lan(9090), &h).unwrap();
        assert_eq!(side, Side::Lan);
        assert_eq!(binds, vec![sa("10.0.0.3:9090"), sa("192.168.10.3:9090")]);

        let (side, binds) = derive_binds(&ListenSpec::Addr(sa("192.168.10.3:9090")), &h).unwrap();
        assert_eq!((side, binds), (Side::Lan, vec![sa("192.168.10.3:9090")]));

        let (side, binds) = derive_binds(&ListenSpec::Addr(sa("110.30.0.1:8080")), &h).unwrap();
        assert_eq!((side, binds), (Side::Overlay, vec![sa("110.30.0.1:8080")]));
    }

    #[test]
    fn derive_binds_without_addresses() {
        let empty = HostAddrs {
            lan_enumerated: true,
            ..Default::default()
        };
        assert_eq!(
            derive_binds(&ListenSpec::Noeio(1), &empty),
            Err(RuleError::NoOverlayAddr)
        );
        assert_eq!(
            derive_binds(&ListenSpec::Lan(1), &empty),
            Err(RuleError::NoLanAddr)
        );
    }

    /// FR-2.7: without an interface table `lan` is refused and a specific
    /// non-overlay address is accepted as LAN-side.
    #[test]
    fn windows_like_host_without_interface_table() {
        let h = HostAddrs {
            overlay: vec![ip("110.20.0.1")],
            peers: vec![],
            lan: vec![],
            lan_enumerated: false,
        };
        assert_eq!(
            derive_binds(&ListenSpec::Lan(9090), &h),
            Err(RuleError::LanNotEnumerable)
        );
        let (side, binds) = derive_binds(&ListenSpec::Addr(sa("192.168.10.3:9090")), &h).unwrap();
        assert_eq!((side, binds), (Side::Lan, vec![sa("192.168.10.3:9090")]));
        assert_eq!(
            derive_binds(&ListenSpec::Addr(sa("0.0.0.0:9090")), &h),
            Err(RuleError::ListenUnspecified)
        );
    }

    #[test]
    fn listen_spec_round_trips_through_display() {
        for s in ["noeio:8080", "lan:9090", "192.168.10.3:9090"] {
            assert_eq!(s.parse::<ListenSpec>().unwrap().to_string(), s);
        }
    }
}
