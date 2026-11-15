//! Working out the client's IP address.

use std::net::IpAddr;

use http::HeaderMap;
use ipnet::IpNet;

/// Works out the client's IP, trusting `X-Forwarded-For` only from
/// configured proxies (`[server].trusted_proxies`).
#[derive(Debug, Clone, Default)]
pub struct ClientIp {
    trust_all: bool,
    nets: Vec<IpNet>,
}

impl ClientIp {
    pub fn new(trusted: &[String]) -> Result<ClientIp, String> {
        let mut nets = Vec::new();
        let mut trust_all = false;
        for t in trusted {
            if t == "all" {
                trust_all = true;
            } else if let Ok(net) = t.parse::<IpNet>() {
                nets.push(net);
            } else if let Ok(ip) = t.parse::<IpAddr>() {
                nets.push(IpNet::from(ip));
            } else {
                return Err(format!(
                    "[server].trusted_proxies: \"{t}\" isn't an IP or CIDR"
                ));
            }
        }
        Ok(ClientIp { trust_all, nets })
    }

    fn trusted(&self, ip: IpAddr) -> bool {
        self.trust_all || self.nets.iter().any(|n| n.contains(&ip))
    }

    /// The client's address, given the address of the connection's peer.
    pub fn resolve(&self, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
        let mut ip = peer;
        if !self.trusted(ip) {
            return ip;
        }
        let forwarded: Vec<IpAddr> = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        // Walk from the closest hop back, stopping at the first untrusted one.
        for hop in forwarded.iter().rev() {
            ip = *hop;
            if !self.trusted(*hop) {
                break;
            }
        }
        ip
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust() {
        let peer: IpAddr = "10.0.0.2".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.9, 10.0.0.7".parse().unwrap());
        let none = ClientIp::new(&[]).unwrap();
        assert_eq!(none.resolve(peer, &headers), peer);
        let trusted = ClientIp::new(&["10.0.0.0/8".into()]).unwrap();
        assert_eq!(
            trusted.resolve(peer, &headers),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
        assert!(ClientIp::new(&["nope".into()]).is_err());
    }
}
