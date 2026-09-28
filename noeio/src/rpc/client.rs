use crate::rpc::outgoing;
use noeio_proto::proto::noeio::v1::daemon_service_client::DaemonServiceClient;
use noeio_proto::proto::noeio::v1::route_service_client::RouteServiceClient;
use noeio_proto::proto::noeio::v1::virtual_nic_service_client::VirtualNicServiceClient;
use noeio_proto::proto::noeio::v1::{
    AdvertiseRouteRequest, CreateVirtualNicRequest, ListRoutesRequest, ListRoutesResponse,
    ListVirtualNicsRequest, NetCheckRequest, PathKind, RouteSource, RouteState, VirtualNicEntry,
    WithdrawRouteRequest,
};
use tonic::transport::Channel;

pub struct CliRpcClient {
    daemon_client: DaemonServiceClient<Channel>,
    vnic_client: VirtualNicServiceClient<Channel>,
    route_client: RouteServiceClient<Channel>,
}

impl CliRpcClient {
    pub async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let channel = outgoing().await?;

        Ok(Self {
            daemon_client: DaemonServiceClient::new(channel.clone()),
            vnic_client: VirtualNicServiceClient::new(channel.clone()),
            route_client: RouteServiceClient::new(channel),
        })
    }

    pub async fn net_check(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let resp = self.daemon_client.net_check(NetCheckRequest {}).await?;
        let derpers = &resp.get_ref().derpers;
        if derpers.is_empty() {
            println!("No Derper servers configured.");
        } else {
            println!(
                "Derper server RTT latency. noeio selects the lowest-latency Derper server for relay forwarding."
            );
            println!();
            for d in derpers {
                match d.rtt_ms {
                    0 => println!("{}\t-", d.address),
                    ms => println!("{}\t{}ms", d.address, ms),
                }
            }
        }
        Ok(())
    }

    pub async fn create_vnic(
        &mut self,
        ip: String,
        ip_version: String,
        network_id: String,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let resp = self
            .vnic_client
            .create_virtual_nic(CreateVirtualNicRequest {
                ip,
                ip_version,
                network_id,
            })
            .await?;
        println!("Vnic created, tun: {}", resp.get_ref().tun_name);
        Ok(())
    }

    /// The daemon's virtual nics with the peers known in each network. The
    /// one read-only query `noeio forward` makes (FR-6.2).
    pub async fn list_virtual_nics(
        &mut self,
    ) -> Result<Vec<VirtualNicEntry>, Box<dyn std::error::Error>> {
        let resp = self
            .vnic_client
            .list_virtual_nics(ListVirtualNicsRequest {})
            .await
            .map_err(render_status)?;
        Ok(resp.into_inner().nics)
    }

    pub async fn advertise_routes(
        &mut self,
        cidrs: Vec<String>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let resp = self
            .route_client
            .advertise_route(AdvertiseRouteRequest { cidrs })
            .await
            .map_err(render_status)?;
        print_advertised(&resp.get_ref().advertised);
        Ok(())
    }

    pub async fn withdraw_routes(
        &mut self,
        cidrs: Vec<String>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let resp = self
            .route_client
            .withdraw_route(WithdrawRouteRequest { cidrs })
            .await
            .map_err(render_status)?;
        print_advertised(&resp.get_ref().advertised);
        Ok(())
    }

    pub async fn list_routes(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let resp = self.route_client.list_routes(ListRoutesRequest {}).await?;
        print!("{}", render_route_list(resp.get_ref()));
        Ok(())
    }
}

fn print_advertised(advertised: &[String]) {
    if advertised.is_empty() {
        println!("advertising: (none)");
    } else {
        println!("advertising: {}", advertised.join(", "));
    }
}

/// Turn a gRPC status into the message the operator should read, not the
/// raw `status: FailedPrecondition, message: ...` debug dump.
fn render_status(status: tonic::Status) -> Box<dyn std::error::Error> {
    status.message().to_string().into()
}

/// The `route list` table. Written to answer the three questions that come
/// up when a subnet route "doesn't work": where did it come from, which
/// path does it take right now, and why isn't it installed.
pub fn render_route_list(resp: &ListRoutesResponse) -> String {
    let mut out = String::new();
    let role = if resp.can_advertise {
        "advertiser + consumer"
    } else {
        "consumer-only (this platform cannot advertise subnet routes; it can use routes advertised by Linux nodes)"
    };
    out.push_str(&format!("platform:      {} — {}\n", resp.platform, role));
    out.push_str(&format!(
        "accept_routes: {}\n",
        if resp.accept_routes { "on" } else { "off" }
    ));
    if resp.can_advertise {
        out.push_str(&format!(
            "nat/forward:   {}\n",
            if resp.nat_applied {
                "applied"
            } else {
                "not applied"
            }
        ));
    }
    out.push('\n');
    if resp.routes.is_empty() {
        out.push_str("no subnet routes\n");
        return out;
    }
    out.push_str(&format!(
        "{:<20} {:<7} {:<11} {:<16} {:<9} {:<30} {}\n",
        "CIDR", "SOURCE", "PEER", "VIA", "STATE", "PATH", "REASON"
    ));
    for r in &resp.routes {
        let source = match RouteSource::try_from(r.source) {
            Ok(RouteSource::Local) => "local",
            Ok(RouteSource::Remote) => "remote",
            _ => "?",
        };
        let state = match RouteState::try_from(r.state) {
            Ok(RouteState::Active) => "active",
            Ok(RouteState::Standby) => "standby",
            Ok(RouteState::Rejected) => "rejected",
            _ => "?",
        };
        let path = match RouteSource::try_from(r.source) {
            Ok(RouteSource::Local) => "-".to_string(),
            _ => match PathKind::try_from(r.path) {
                Ok(PathKind::Direct) => format!("direct {}", r.path_addr),
                Ok(PathKind::Relay) => "relay (derper)".to_string(),
                Ok(PathKind::None) => "none".to_string(),
                _ => "?".to_string(),
            },
        };
        let peer = if r.peer_id == 0 {
            "-".to_string()
        } else {
            r.peer_id.to_string()
        };
        let via = if r.via.is_empty() {
            "-"
        } else {
            r.via.as_str()
        };
        out.push_str(&format!(
            "{:<20} {:<7} {:<11} {:<16} {:<9} {:<30} {}\n",
            r.cidr, source, peer, via, state, path, r.reject_reason
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeio_proto::proto::noeio::v1::RouteEntry;

    /// FR-7.4 / FR-9.6: the listing names the peer, the path, the reason,
    /// and flags a consumer-only platform.
    #[test]
    fn route_list_answers_the_three_questions() {
        let resp = ListRoutesResponse {
            routes: vec![
                RouteEntry {
                    cidr: "192.168.10.0/24".into(),
                    source: RouteSource::Remote as i32,
                    peer_id: 42,
                    via: "110.20.0.1".into(),
                    state: RouteState::Active as i32,
                    path: PathKind::Direct as i32,
                    path_addr: "203.0.113.5:2026".into(),
                    ..Default::default()
                },
                RouteEntry {
                    cidr: "10.0.0.0/8".into(),
                    source: RouteSource::Remote as i32,
                    peer_id: 43,
                    via: "110.20.0.9".into(),
                    state: RouteState::Rejected as i32,
                    reject_reason: "conflicts with a directly connected local network".into(),
                    path: PathKind::Relay as i32,
                    ..Default::default()
                },
            ],
            can_advertise: false,
            platform: "macos".into(),
            accept_routes: true,
            nat_applied: false,
        };
        let text = render_route_list(&resp);
        assert!(text.contains("consumer-only"), "{text}");
        assert!(text.contains("can use routes advertised"), "{text}");
        assert!(text.contains("42"));
        assert!(text.contains("direct 203.0.113.5:2026"));
        assert!(text.contains("relay (derper)"));
        assert!(text.contains("conflicts with a directly connected local network"));
        assert!(
            !text.contains("nat/forward"),
            "consumer-only has no NAT line"
        );
    }
}
