use crate::daemon::NoeioDaemon;
use crate::interface::virtual_nic::VirtualNic;
use noeio_proto::proto::noeio::v1::virtual_nic_service_server::VirtualNicService;
use noeio_proto::proto::noeio::v1::{
    CreateVirtualNicRequest, CreateVirtualNicResponse, ListVirtualNicsRequest,
    ListVirtualNicsResponse, VirtualNicEntry,
};
use std::net::Ipv4Addr;
use std::sync::Arc;
use tonic::{Request, Response, Status};

pub struct VirtualNicServiceImpl {
    state: Arc<NoeioDaemon>,
}

impl VirtualNicServiceImpl {
    pub fn new(state: Arc<NoeioDaemon>) -> Self {
        Self { state }
    }

    /// Snapshot of every registered nic with the overlay addresses of the
    /// peers in its network. Read-only: this is what `noeio forward` uses to
    /// expand `--listen noeio:<port>` and to refuse overlay→overlay relays
    /// (FR-6.2); nothing about the caller is remembered.
    async fn snapshot(&self) -> Vec<VirtualNicEntry> {
        let host = self.state.host_info.lock().await.clone();
        let peers = self.state.router.peers();
        let mut nics = Vec::new();
        for (nic_id, tun_name) in self.state.nics.interfaces() {
            let Some(ip) = self.state.nics.get(&nic_id).map(|nic| nic.ip) else {
                continue;
            };
            let local = host
                .as_ref()
                .and_then(|h| h.peers.iter().find(|p| p.peer_id == nic_id).cloned());
            let (network_id, peer_ips) = match local {
                Some(local) => {
                    let mut ips: Vec<String> = peers
                        .iter()
                        .map(|p| p.info())
                        .filter(|info| info.network_id == local.network_id && info.noeio_ip != ip)
                        .map(|info| info.noeio_ip.to_string())
                        .collect();
                    ips.sort();
                    ips.dedup();
                    (
                        uuid::Uuid::from_bytes(local.network_id)
                            .hyphenated()
                            .to_string(),
                        ips,
                    )
                }
                // The nic is registered but host_info is not initialized yet
                // (no STUN answer so far): report the nic, with no peers.
                None => (String::new(), Vec::new()),
            };
            nics.push(VirtualNicEntry {
                tun_name,
                ip: ip.to_string(),
                network_id,
                peer_id: nic_id,
                peer_ips,
            });
        }
        nics.sort_by(|a, b| a.ip.cmp(&b.ip));
        nics
    }
}

#[tonic::async_trait]
impl VirtualNicService for VirtualNicServiceImpl {
    async fn create_virtual_nic(
        &self,
        request: Request<CreateVirtualNicRequest>,
    ) -> Result<Response<CreateVirtualNicResponse>, Status> {
        let req = request.get_ref();
        let ip_addr = req
            .ip
            .parse::<Ipv4Addr>()
            .map_err(|_| Status::invalid_argument(format!("invalid ip address: '{}'", req.ip)))?;

        let (nic, reader) = VirtualNic::create_ipv4_nic(ip_addr).await.map_err(|err| {
            Status::failed_precondition(format!(
                "failed to create virtual nic for {}: {}",
                req.ip, err
            ))
        })?;
        let tun_name = nic.tun_name.clone();

        self.state
            .register_nic(self.state.clone(), nic, reader, req.network_id.clone())
            .await
            .map_err(Status::failed_precondition)?;

        Ok(Response::from(CreateVirtualNicResponse { tun_name }))
    }

    async fn list_virtual_nics(
        &self,
        _request: Request<ListVirtualNicsRequest>,
    ) -> Result<Response<ListVirtualNicsResponse>, Status> {
        Ok(Response::new(ListVirtualNicsResponse {
            nics: self.snapshot().await,
        }))
    }
}
