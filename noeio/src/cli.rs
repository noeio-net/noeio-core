use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "noeio", version)]
#[command(subcommand_required = true, arg_required_else_help = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Boot the noeio daemon
    Boot {
        /// Path to the configuration file
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// UDP listen port
        #[arg(short, long, default_value_t = 2026)]
        port: u16,
        /// Extra STUN servers; pass several as a comma separated list, e.g.
        /// --stun stun.a.example:3478,stun.b.example:3478
        #[arg(long = "stun", value_delimiter = ',', value_name = "ADDR")]
        stun: Vec<String>,
        /// Extra derper servers; pass several as a comma separated list, e.g.
        /// --derper-server derp.a.example:8080,derp.b.example:8080
        #[arg(long = "derper-server", value_delimiter = ',', value_name = "ADDR")]
        derper_servers: Vec<String>,
        /// Report tokens, comma separated, paired positionally with
        /// --derper-server so the first token belongs to the first derper.
        /// Servers without a token report unauthenticated
        #[arg(long = "derper-token", value_delimiter = ',', value_name = "TOKEN")]
        derper_tokens: Vec<String>,
        /// Subnets this node routes for (subnet router role, Linux only);
        /// comma separated CIDRs, e.g. --advertise-routes 192.168.10.0/24,172.20.0.0/16
        #[arg(long = "advertise-routes", value_delimiter = ',', value_name = "CIDR")]
        advertise_routes: Vec<String>,
        /// Install subnet routes advertised by other nodes
        #[arg(long = "accept-routes")]
        accept_routes: bool,
    },
    /// Check Derper relay server RTT latency
    Netcheck,
    /// Create a new resource
    Create {
        #[command(subcommand)]
        resource: CreateResource,
    },
    /// Manage subnet routes on the running daemon
    Route {
        #[command(subcommand)]
        command: RouteCommand,
    },
    /// Forward one port in the foreground; the forwarding lives exactly as
    /// long as this command runs (Ctrl+C / SIGTERM / SIGHUP stop it).
    ///
    /// Needs the daemon (`noeio boot`) to be running and the same privileges
    /// as `noeio route`, because the overlay addresses are asked from the
    /// daemon's RPC socket.
    #[command(after_help = "\
Examples:
  noeio forward --listen noeio:8080 --target 192.168.10.7:80
      expose a LAN host's port to the overlay
  noeio forward --listen lan:9090 --target 110.20.0.9:22
      let LAN machines without an agent reach an overlay node
  noeio forward --listen noeio:5432 --target 127.0.0.1:5432 --allow-from 110.20.0.0/24
      expose a loopback-only local service to one overlay subnet

Exit codes: 0 stopped by a signal, 1 environment error (daemon down, bind
failed), 2 invalid rule.")]
    Forward {
        /// Where to listen, as <ADDR>:<PORT>. ADDR is `noeio` (every overlay
        /// address of this node; only peers can connect), `lan` (every
        /// physical interface address; only the LAN can connect) or one
        /// specific local IPv4 address. 0.0.0.0 and 127.0.0.1 are refused.
        #[arg(long, value_name = "ADDR:PORT")]
        listen: String,
        /// Where to forward to, as <IPv4>:<PORT> (no hostnames), e.g.
        /// 192.168.10.7:80 or 127.0.0.1:5432
        #[arg(long, value_name = "ADDR:PORT")]
        target: String,
        /// Transport protocol. UDP keeps one session per client address,
        /// reclaimed after 60s idle, at most 512 per process
        #[arg(long, value_enum, default_value_t = crate::forward::rule::Proto::Tcp)]
        proto: crate::forward::rule::Proto,
        /// Only accept connections from these IPv4 CIDRs (comma separated).
        /// A coarse source-IP filter, not authentication
        #[arg(long = "allow-from", value_delimiter = ',', value_name = "CIDR")]
        allow_from: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum RouteCommand {
    /// Start routing for one or more subnets (Linux only), e.g.
    /// noeio route advertise 192.168.10.0/24 172.20.0.0/16
    Advertise {
        #[arg(required = true, value_name = "CIDR")]
        cidrs: Vec<String>,
    },
    /// Stop routing for one or more subnets
    Withdraw {
        #[arg(required = true, value_name = "CIDR")]
        cidrs: Vec<String>,
    },
    /// Show advertised and learned subnet routes with their state
    List,
}

#[derive(Subcommand, Debug)]
pub enum CreateResource {
    /// Create a new virtual NIC
    Vnic {
        /// IP address
        #[arg(short, long)]
        ip: String,
        /// IP version (e.g. "v4", "v6")
        #[arg(long, default_value = "v4")]
        ip_version: String,
        /// Network ID
        #[arg(short, long)]
        network: String,
    },
}
