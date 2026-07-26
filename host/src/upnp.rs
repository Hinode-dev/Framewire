//! Best-effort UPnP IGD port forwarding, so viewers on a different network
//! than the host can connect P2P without needing a TURN relay server.
//!
//! Entirely optional and additive: if no UPnP-capable router is found (or
//! it has UPnP disabled, or there's an extra NAT layer upstream like
//! carrier-grade NAT), this just does nothing and streaming falls back to
//! whatever P2P connectivity STUN alone can achieve — today's behavior.
//!
//! A 1:1 NAT mapping (external port == internal port) is required for
//! `SettingEngine::set_nat_1to1_ips` to produce a valid candidate, so the
//! whole port range is requested with matching external/internal ports.

use std::net::{IpAddr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use igd_next::{PortMappingProtocol, SearchOptions};

/// Wide enough to give one local port per concurrent viewer connection,
/// with headroom past the 5-viewer target.
pub const PORT_RANGE_MIN: u16 = 40000;
pub const PORT_RANGE_MAX: u16 = 40029;

pub struct PortForward {
    pub external_ip: IpAddr,
    pub port_min: u16,
    pub port_max: u16,
}

/// Attempts to map `PORT_RANGE_MIN..=PORT_RANGE_MAX` (UDP) 1:1 through the
/// LAN gateway and learn the router's public IP. Returns `None` on any
/// failure — callers should just proceed without it.
pub async fn try_setup() -> Option<PortForward> {
    let options = SearchOptions {
        timeout: Some(Duration::from_secs(3)),
        ..Default::default()
    };
    let gateway = match igd_next::aio::tokio::search_gateway(options).await {
        Ok(g) => g,
        Err(e) => {
            println!("[upnp] no UPnP gateway found, skipping port forwarding: {e}");
            return None;
        }
    };

    let external_ip = match gateway.get_external_ip().await {
        Ok(ip) => ip,
        Err(e) => {
            println!("[upnp] failed to get external IP, skipping port forwarding: {e}");
            return None;
        }
    };

    let Some(local_ip) = local_ip_for_gateway(gateway.addr) else {
        println!("[upnp] failed to determine local IP, skipping port forwarding");
        return None;
    };

    let mut mapped_max = None;
    for port in PORT_RANGE_MIN..=PORT_RANGE_MAX {
        let local_addr = SocketAddr::V4(SocketAddrV4::new(local_ip, port));
        match gateway
            .add_port(PortMappingProtocol::UDP, port, local_addr, 0, "Framewire")
            .await
        {
            Ok(()) => mapped_max = Some(port),
            Err(e) => {
                println!(
                    "[upnp] failed to map port {port}, stopping (mapped {PORT_RANGE_MIN}..={:?} so far): {e}",
                    mapped_max
                );
                break;
            }
        }
    }

    let port_max = mapped_max?;
    println!(
        "[upnp] mapped UDP {PORT_RANGE_MIN}..={port_max} -> external IP {external_ip} \
         (viewers on other networks can now connect P2P without a TURN relay)"
    );
    Some(PortForward {
        external_ip,
        port_min: PORT_RANGE_MIN,
        port_max,
    })
}

/// The local IP the OS would use to reach `gateway_addr` — found by
/// "connecting" a UDP socket (no packets are actually sent for UDP connect;
/// this just resolves the outbound route) and reading back its local
/// address, the standard way to answer "what's my LAN IP" without
/// hardcoding an interface name.
fn local_ip_for_gateway(gateway_addr: SocketAddr) -> Option<std::net::Ipv4Addr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect(gateway_addr).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) => Some(ip),
        IpAddr::V6(_) => None,
    }
}
