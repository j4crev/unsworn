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

    /// Whether the connection's peer is a trusted proxy, whose forwarding
    /// headers may be passed on.
    pub fn trusts(&self, peer: IpAddr) -> bool {
        self.trusted(peer.to_canonical())
    }

    /// The client's address, given the address of the connection's peer.
    ///
    /// Addresses come out in canonical form: an IPv4 client that reached an
    /// IPv6 socket (`::ffff:203.0.113.9`) is `203.0.113.9`, so IPv4 rules
    /// and trusted proxies match it.
    pub fn resolve(&self, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
        let mut ip = peer.to_canonical();
        if !self.trusted(ip) {
            return ip;
        }
        let forwarded: Vec<&str> = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .collect();
        // Walk from the closest hop back, stopping at the first untrusted
        // one. An entry that isn't an address ends the walk too: whatever
        // is beyond it can't be vouched for.
        for hop in forwarded.iter().rev() {
            let Some(hop) = parse_hop(hop) else {
                break;
            };
            ip = hop;
            if !self.trusted(hop) {
                break;
            }
        }
        ip
    }
}

/// An `X-Forwarded-For` entry: an address, which some proxies follow with
/// a port.
fn parse_hop(entry: &str) -> Option<IpAddr> {
    let entry = entry.trim();
    let ip: Option<IpAddr> = entry.parse().ok().or_else(|| {
        entry
            .parse::<std::net::SocketAddr>()
            .ok()
            .map(|addr| addr.ip())
    });
    ip.map(|ip| ip.to_canonical())
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

        // An entry that isn't an address stops the walk: the spoofable
        // entries left of it are never reached.
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "198.51.100.1, unknown, 10.0.0.7".parse().unwrap(),
        );
        assert_eq!(
            trusted.resolve(peer, &headers),
            "10.0.0.7".parse::<IpAddr>().unwrap()
        );
        // IPv4 through an IPv6 socket is still IPv4, for the peer and for
        // forwarded entries.
        let mapped: IpAddr = "::ffff:10.0.0.2".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "::ffff:203.0.113.9".parse().unwrap());
        assert_eq!(
            trusted.resolve(mapped, &headers),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
        assert_eq!(none.resolve(mapped, &headers), peer);

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "203.0.113.9:4711, [2001:db8::1]:80".parse().unwrap(),
        );
        let all = ClientIp::new(&["all".into()]).unwrap();
        assert_eq!(
            all.resolve(peer, &headers),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
    }
}
