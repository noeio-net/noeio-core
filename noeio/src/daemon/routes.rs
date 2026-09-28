//! Subnet-route policy: what this node may advertise, and which advertised
//! routes it accepts from others.
//!
//! Two roles, decided per CIDR rather than per node:
//!
//! - **advertiser** — this node routes for the CIDR. Only Linux can (the
//!   forwarding/NAT side exists only there); elsewhere every advertisement is
//!   rejected *and never enters `PeerInfo`*, so it cannot be broadcast.
//! - **consumer** — this node installs a route to the CIDR through the peer
//!   that advertises it.
//!
//! The same blacklist ([`validate_cidr`]) is applied on both sides: a
//! consumer never trusts that the advertiser validated.

use noeio_common::host_info::PeerId;
use smoltcp::wire::Ipv4Cidr;
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};

/// Shortest prefix a subnet route may have. Anything shorter is an exit-node
/// route, which is out of scope and a great way to lose the box.
pub const MIN_PREFIX_LEN: u8 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteError {
    /// Not parseable as `a.b.c.d/n`.
    Malformed(String),
    /// Prefix shorter than [`MIN_PREFIX_LEN`] (includes `0.0.0.0/0`).
    TooBroad(Ipv4Cidr),
    /// Overlaps the overlay itself; traffic would loop back into the tunnel.
    OverlapsOverlay { cidr: Ipv4Cidr, overlay: IpAddr },
    /// Contains a derper or STUN server; would cut the control plane.
    CoversControlPlane { cidr: Ipv4Cidr, addr: IpAddr },
    /// Loopback or link-local.
    Reserved(Ipv4Cidr),
    /// Advertising is not supported on this OS (consumer-only platform).
    PlatformConsumerOnly {
        cidr: Ipv4Cidr,
        platform: &'static str,
    },
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(s) => write!(f, "'{s}' is not a valid IPv4 CIDR"),
            Self::TooBroad(c) => write!(
                f,
                "{c}: prefix shorter than /{MIN_PREFIX_LEN} (exit-node routes are not supported)"
            ),
            Self::OverlapsOverlay { cidr, overlay } => write!(
                f,
                "{cidr} contains this node's overlay address {overlay}; overlay traffic would loop"
            ),
            Self::CoversControlPlane { cidr, addr } => write!(
                f,
                "{cidr} contains control-plane server {addr} (derper/STUN); the node would lose its relay"
            ),
            Self::Reserved(c) => write!(f, "{c} is loopback or link-local"),
            Self::PlatformConsumerOnly { cidr, platform } => write!(
                f,
                "{cidr}: advertising subnet routes (acting as a subnet router) is not supported on {platform}. \
                 This node can still *use* subnet routes advertised by other nodes; advertise {cidr} from a Linux node instead"
            ),
        }
    }
}

impl std::error::Error for RouteError {}

/// Addresses a CIDR must not swallow: our overlay IPs and the resolved
/// control-plane servers.
#[derive(Debug, Default, Clone)]
pub struct Protected {
    pub overlay_ips: Vec<IpAddr>,
    pub control_plane: Vec<IpAddr>,
}

/// Parse and normalize one CIDR string. `192.168.10.1/24` becomes
/// `192.168.10.0/24` (the caller may warn about the correction).
pub fn parse_cidr(s: &str) -> Result<Ipv4Cidr, RouteError> {
    let s = s.trim();
    let cidr: Ipv4Cidr = s
        .parse()
        .map_err(|_| RouteError::Malformed(s.to_string()))?;
    Ok(cidr.network())
}

/// The blacklist shared by advertiser and consumer (FR-1.5 / S-4). Assumes
/// `cidr` is already in network form.
pub fn validate_cidr(cidr: Ipv4Cidr, protected: &Protected) -> Result<(), RouteError> {
    if cidr.prefix_len() < MIN_PREFIX_LEN {
        return Err(RouteError::TooBroad(cidr));
    }
    let addr = cidr.address();
    if addr.is_loopback()
        || addr.is_link_local()
        || cidr.contains_addr(&Ipv4Addr::LOCALHOST)
        || cidr.contains_addr(&Ipv4Addr::new(169, 254, 0, 0))
    {
        return Err(RouteError::Reserved(cidr));
    }
    for ip in &protected.overlay_ips {
        if let IpAddr::V4(v4) = ip
            && cidr.contains_addr(v4)
        {
            return Err(RouteError::OverlapsOverlay { cidr, overlay: *ip });
        }
    }
    for ip in &protected.control_plane {
        if let IpAddr::V4(v4) = ip
            && cidr.contains_addr(v4)
        {
            return Err(RouteError::CoversControlPlane { cidr, addr: *ip });
        }
    }
    Ok(())
}

/// Whether this build can act as a subnet router at all (FR-5.5 / FR-9).
pub const fn platform_can_advertise() -> bool {
    cfg!(target_os = "linux")
}

/// Human name of the platform for error messages.
pub const fn platform_name() -> &'static str {
    std::env::consts::OS
}

/// Everything an advertisement must pass before it is allowed into
/// `PeerInfo.advertised_routes`. This is the single gate on the broadcast
/// path (FR-9.1): the platform rule and the blacklist share it, and every
/// entry point (config, CLI flag, RPC) goes through it.
pub fn validate_advertisement(s: &str, protected: &Protected) -> Result<Ipv4Cidr, RouteError> {
    let cidr = parse_cidr(s)?;
    if !platform_can_advertise() {
        return Err(RouteError::PlatformConsumerOnly {
            cidr,
            platform: platform_name(),
        });
    }
    validate_cidr(cidr, protected)?;
    Ok(cidr)
}

/// Why an advertised route is not installed on this consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// Fails the shared blacklist (an advertiser that skipped validation).
    Invalid,
    /// Overlaps a subnet one of our physical interfaces sits on; the local
    /// LAN always wins.
    LocalLan,
    /// This node advertises the CIDR itself; it is the exit, so a TUN route
    /// would loop.
    SelfAdvertised,
    /// Another peer with a lower `peer_id` advertises the same CIDR and is
    /// the active exit; this one is standby.
    Standby,
    /// `accept_routes = false`.
    PolicyOff,
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "fails route validation",
            Self::LocalLan => "conflicts with a directly connected local network",
            Self::SelfAdvertised => "advertised by this node itself",
            Self::Standby => "standby: another peer is the active exit for this prefix",
            Self::PolicyOff => "accept_routes is disabled",
        })
    }
}

/// One advertised route as seen by the consumer, with the decision made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub cidr: Ipv4Cidr,
    pub peer_id: PeerId,
    pub rejected: Option<Rejection>,
}

/// Inputs to [`decide`] that describe this node.
#[derive(Debug, Default, Clone)]
pub struct ConsumerPolicy {
    pub accept_routes: bool,
    /// CIDRs of the subnets our physical interfaces are on.
    pub local_lans: Vec<Ipv4Cidr>,
    /// CIDRs this node advertises itself.
    pub self_advertised: Vec<Ipv4Cidr>,
    pub protected: Protected,
}

/// Decide, for every `(peer, cidr)` advertisement, whether this node installs
/// it (FR-3.4). Pure: the same inputs always yield the same decisions, and
/// nothing about message timing enters (FR-8.6).
///
/// Ordering of the rules matters and is the priority list from FR-3.4:
/// local LAN beats everything, then self-advertised, then the blacklist, then
/// primary election by lowest `peer_id`. Containment between *different*
/// prefixes is not a conflict — both install and longest-prefix match sorts
/// it out.
pub fn decide(policy: &ConsumerPolicy, advertised: &[(PeerId, Ipv4Cidr)]) -> Vec<Decision> {
    if !policy.accept_routes {
        return advertised
            .iter()
            .map(|&(peer_id, cidr)| Decision {
                cidr,
                peer_id,
                rejected: Some(Rejection::PolicyOff),
            })
            .collect();
    }

    // Lowest peer_id per exact prefix is the active exit.
    let mut active: BTreeMap<Ipv4Cidr, PeerId> = BTreeMap::new();
    for &(peer_id, cidr) in advertised {
        active
            .entry(cidr.network())
            .and_modify(|p| *p = (*p).min(peer_id))
            .or_insert(peer_id);
    }

    advertised
        .iter()
        .map(|&(peer_id, cidr)| {
            let cidr = cidr.network();
            let rejected = if policy.local_lans.iter().any(|lan| overlaps(lan, &cidr)) {
                Some(Rejection::LocalLan)
            } else if policy
                .self_advertised
                .iter()
                .any(|own| overlaps(own, &cidr))
            {
                Some(Rejection::SelfAdvertised)
            } else if validate_cidr(cidr, &policy.protected).is_err() {
                Some(Rejection::Invalid)
            } else if active.get(&cidr) != Some(&peer_id) {
                Some(Rejection::Standby)
            } else {
                None
            };
            Decision {
                cidr,
                peer_id,
                rejected,
            }
        })
        .collect()
}

/// Two prefixes overlap when either contains the other.
fn overlaps(a: &Ipv4Cidr, b: &Ipv4Cidr) -> bool {
    a.contains_subnet(b) || b.contains_subnet(a)
}

/// Resolve every configured derper and STUN server to the addresses an
/// advertised CIDR must not cover. Unresolvable entries are skipped with a
/// warning: they can't be protected, but they also can't be what the node is
/// currently relying on.
pub async fn resolve_control_plane(cfg: &crate::config::Config) -> Vec<IpAddr> {
    let hosts = cfg
        .derper
        .servers
        .iter()
        .map(|d| d.address.as_str())
        .chain(cfg.stun.servers.iter().map(String::as_str));
    let mut out = Vec::new();
    for host in hosts {
        match tokio::net::lookup_host(host).await {
            Ok(addrs) => out.extend(addrs.map(|a| a.ip())),
            Err(err) => tracing::warn!(host, "could not resolve control-plane server: {err}"),
        }
    }
    out.sort();
    out.dedup();
    out
}

/// IPv4 networks configured on our non-loopback interfaces, minus our own
/// TUNs (`exclude`). Shared by the local-LAN route rule and by
/// `noeio forward --listen lan:<port>`. Filtering is by interface flag only,
/// exactly as [`local_lans`] always did; callers that must not see a
/// loopback *address* on an odd interface filter that themselves.
#[cfg(unix)]
fn physical_v4(exclude: &[IpAddr]) -> impl Iterator<Item = pnet::ipnetwork::Ipv4Network> + '_ {
    pnet::datalink::interfaces()
        .into_iter()
        .filter(|iface| !iface.is_loopback())
        .flat_map(|iface| iface.ips)
        .filter_map(move |net| match net {
            pnet::ipnetwork::IpNetwork::V4(v4) if !exclude.contains(&IpAddr::V4(v4.ip())) => {
                Some(v4)
            }
            _ => None,
        })
}

/// IPv4 subnets our physical interfaces sit on, for the local-LAN rule.
/// Loopback and our own TUNs (`exclude`) are left out.
#[cfg(unix)]
pub fn local_lans(exclude: &[IpAddr]) -> Vec<Ipv4Cidr> {
    physical_v4(exclude)
        .map(|v4| Ipv4Cidr::new(v4.ip(), v4.prefix()).network())
        // A /32 on a physical interface is a point-to-point address, not a
        // LAN; it should not veto a subnet route.
        .filter(|cidr| cidr.prefix_len() < 32)
        .collect()
}

#[cfg(not(unix))]
pub fn local_lans(_exclude: &[IpAddr]) -> Vec<Ipv4Cidr> {
    // pnet is not linked on Windows (needs Npcap). Without interface
    // enumeration the local-LAN rule cannot fire; the route table's own
    // longest-prefix match still prefers a directly connected /24 over an
    // equal-or-shorter learned prefix.
    Vec::new()
}

/// IPv4 addresses of our non-loopback interfaces (`exclude`, i.e. our TUNs,
/// left out), sorted and deduplicated. Empty on platforms where interfaces
/// cannot be enumerated; see [`can_enumerate_interfaces`].
#[cfg(unix)]
pub fn local_addrs(exclude: &[IpAddr]) -> Vec<Ipv4Addr> {
    let mut addrs: Vec<Ipv4Addr> = physical_v4(exclude).map(|v4| v4.ip()).collect();
    addrs.sort();
    addrs.dedup();
    addrs
}

#[cfg(not(unix))]
pub fn local_addrs(_exclude: &[IpAddr]) -> Vec<Ipv4Addr> {
    Vec::new()
}

/// Whether [`local_addrs`] / [`local_lans`] can see the interface table at
/// all. False on Windows, where pnet is not linked.
pub const fn can_enumerate_interfaces() -> bool {
    cfg!(unix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> Ipv4Cidr {
        s.parse().unwrap()
    }

    fn protected() -> Protected {
        Protected {
            overlay_ips: vec!["110.20.0.5".parse().unwrap()],
            control_plane: vec![
                "203.0.113.9".parse().unwrap(),
                "198.51.100.3".parse().unwrap(),
            ],
        }
    }

    #[test]
    fn parse_normalizes_host_bits() {
        assert_eq!(parse_cidr("192.168.10.1/24").unwrap(), c("192.168.10.0/24"));
        assert_eq!(parse_cidr(" 10.0.0.0/8 ").unwrap(), c("10.0.0.0/8"));
        assert!(matches!(parse_cidr("nope"), Err(RouteError::Malformed(_))));
        assert!(matches!(
            parse_cidr("10.0.0.0/33"),
            Err(RouteError::Malformed(_))
        ));
        assert!(matches!(
            parse_cidr("10.0.0.0"),
            Err(RouteError::Malformed(_))
        ));
    }

    /// AC-10: the FR-1.5 blacklist.
    #[test]
    fn blacklist_rejects_each_class() {
        let p = protected();
        assert_eq!(
            validate_cidr(c("0.0.0.0/0"), &p),
            Err(RouteError::TooBroad(c("0.0.0.0/0")))
        );
        assert!(matches!(
            validate_cidr(c("10.0.0.0/7"), &p),
            Err(RouteError::TooBroad(_))
        ));
        assert!(validate_cidr(c("10.0.0.0/8"), &p).is_ok());

        assert!(matches!(
            validate_cidr(c("110.20.0.0/16"), &p),
            Err(RouteError::OverlapsOverlay { .. })
        ));
        assert!(matches!(
            validate_cidr(c("203.0.113.0/24"), &p),
            Err(RouteError::CoversControlPlane { .. })
        ));
        assert!(matches!(
            validate_cidr(c("198.51.100.0/30"), &p),
            Err(RouteError::CoversControlPlane { .. })
        ));
        assert!(matches!(
            validate_cidr(c("127.0.0.0/8"), &p),
            Err(RouteError::Reserved(_))
        ));
        assert!(matches!(
            validate_cidr(c("169.254.0.0/16"), &p),
            Err(RouteError::Reserved(_))
        ));
        assert!(matches!(
            validate_cidr(c("169.254.10.0/24"), &p),
            Err(RouteError::Reserved(_))
        ));

        assert!(validate_cidr(c("192.168.10.0/24"), &p).is_ok());
        assert!(validate_cidr(c("172.20.0.0/16"), &p).is_ok());
    }

    /// AC-16 (3): on a consumer-only platform no CIDR ever passes the gate,
    /// so nothing can reach `PeerInfo.advertised_routes`.
    #[test]
    fn advertisement_gate_follows_platform() {
        let result = validate_advertisement("192.168.10.0/24", &protected());
        if platform_can_advertise() {
            assert_eq!(result.unwrap(), c("192.168.10.0/24"));
        } else {
            let err = result.unwrap_err();
            assert!(matches!(err, RouteError::PlatformConsumerOnly { .. }));
            let msg = err.to_string();
            assert!(msg.contains(platform_name()), "{msg}");
            assert!(msg.contains("192.168.10.0/24"), "{msg}");
            // FR-9.9: must not read as "subnet routes unsupported".
            assert!(msg.contains("can still"), "{msg}");
        }
        // The blacklist still applies before the platform gate would matter:
        // a broken CIDR is malformed everywhere.
        assert!(matches!(
            validate_advertisement("bogus", &protected()),
            Err(RouteError::Malformed(_))
        ));
    }

    fn policy() -> ConsumerPolicy {
        ConsumerPolicy {
            accept_routes: true,
            local_lans: vec![c("192.168.1.0/24")],
            self_advertised: vec![c("172.16.0.0/16")],
            protected: protected(),
        }
    }

    #[test]
    fn decide_accepts_a_clean_route() {
        let d = decide(&policy(), &[(10, c("10.0.0.0/8"))]);
        assert_eq!(d[0].rejected, None);
    }

    #[test]
    fn decide_policy_off_rejects_everything() {
        let mut p = policy();
        p.accept_routes = false;
        let d = decide(&p, &[(10, c("10.0.0.0/8"))]);
        assert_eq!(d[0].rejected, Some(Rejection::PolicyOff));
    }

    #[test]
    fn decide_local_lan_wins() {
        // Exact, wider and narrower overlaps all lose to the LAN.
        for cidr in ["192.168.1.0/24", "192.168.0.0/16", "192.168.1.128/25"] {
            let d = decide(&policy(), &[(10, c(cidr))]);
            assert_eq!(d[0].rejected, Some(Rejection::LocalLan), "{cidr}");
        }
    }

    #[test]
    fn decide_self_advertised_is_not_installed() {
        let d = decide(&policy(), &[(10, c("172.16.5.0/24"))]);
        assert_eq!(d[0].rejected, Some(Rejection::SelfAdvertised));
    }

    #[test]
    fn decide_consumer_side_blacklist_applies() {
        // S-4: even if the advertiser "validated", we check again.
        let d = decide(&policy(), &[(10, c("203.0.113.0/24"))]);
        assert_eq!(d[0].rejected, Some(Rejection::Invalid));
    }

    #[test]
    fn decide_elects_lowest_peer_id_for_same_prefix() {
        let d = decide(&policy(), &[(20, c("10.0.0.0/8")), (10, c("10.0.0.0/8"))]);
        assert_eq!(d[0].rejected, Some(Rejection::Standby));
        assert_eq!(d[1].rejected, None);
        // When the active one goes away the standby takes over.
        let d = decide(&policy(), &[(20, c("10.0.0.0/8"))]);
        assert_eq!(d[0].rejected, None);
    }

    #[test]
    fn decide_installs_nested_prefixes_from_different_peers() {
        let d = decide(&policy(), &[(10, c("10.0.0.0/8")), (20, c("10.1.0.0/16"))]);
        assert!(d.iter().all(|d| d.rejected.is_none()));
    }

    #[test]
    fn decide_normalizes_host_bits_before_electing() {
        let d = decide(&policy(), &[(20, c("10.0.0.7/8")), (10, c("10.0.0.0/8"))]);
        assert_eq!(d[0].cidr, c("10.0.0.0/8"));
        assert_eq!(d[0].rejected, Some(Rejection::Standby));
    }
}
