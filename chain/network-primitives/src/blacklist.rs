/// A blacklist for socket addresses.  Supports adding individual IP:port tuples
/// to the blacklist or entire IPs.
#[derive(Debug, Default, Clone)]
pub struct Blacklist(
    /// Only IPv6 addresses are stored.  IPv4 addresses are mapped to IPv6 before being added.
    ///
    /// Without the mapping, we could blacklist an IPv4 and still interact with that address if
    /// it is presented as IPv6.
    std::collections::HashMap<std::net::Ipv6Addr, PortsSet>,
);

// TODO(CP-34): merge Blacklist with whitelist functionality and replace them with sth
// like AuthorizationConfig.
impl Blacklist {
    /// Construct a blacklist from list of addresses.
    ///
    /// Arguments:
    /// - `blacklist` - list of strings in one of the following format:
    ///    - "IP" - for example 127.0.0.1 - if only IP is provided we will block all ports
    ///    - "IP:PORT - for example 127.0.0.1:2134
    pub fn from_iter<I: AsRef<str> + std::fmt::Display>(
        blacklist: impl IntoIterator<Item = I>,
    ) -> Self {
        let mut result = Self::default();
        for addr in blacklist {
            if result.add(addr.as_ref()).is_err() {
                tracing::warn!(target: "network", "{}: invalid blacklist pattern, ignoring", addr);
            }
        }
        result
    }

    fn add(&mut self, addr: &str) -> Result<(), std::net::AddrParseError> {
        match addr.parse::<PatternAddr>()? {
            PatternAddr::Ip(ip) => {
                self.0.entry(ip).and_modify(|ports| ports.add_all()).or_insert(PortsSet::All);
            }
            PatternAddr::IpPort(addr) => {
                self.0
                    .entry(*addr.ip())
                    .and_modify(|ports| ports.add_port(addr.port()))
                    .or_insert_with(|| PortsSet::new(addr.port()));
            }
        }
        Ok(())
    }

    /// Returns whether given address is on the blacklist.
    pub fn contains(&self, addr: &std::net::SocketAddr) -> bool {
        let ip = match addr.ip() {
            std::net::IpAddr::V4(ip) => ip.to_ipv6_mapped(),
            std::net::IpAddr::V6(ip) => ip,
        };
        match self.0.get(&ip) {
            None => false,
            Some(ports) => ports.contains(addr.port()),
        }
    }
}

/// Used to match a socket addr by IP:Port or only by IP
#[cfg_attr(test, derive(Debug, PartialEq))]
enum PatternAddr {
    Ip(std::net::Ipv6Addr),
    IpPort(std::net::SocketAddrV6),
}

impl std::str::FromStr for PatternAddr {
    type Err = std::net::AddrParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(ip_addr) = s.parse::<std::net::IpAddr>() {
            let ip_addr_v6 = match ip_addr {
                std::net::IpAddr::V4(ip) => ip.to_ipv6_mapped(),
                std::net::IpAddr::V6(ip) => ip,
            };
            return Ok(PatternAddr::Ip(ip_addr_v6));
        }
        let socket_addr_v6 = match s.parse::<std::net::SocketAddr>()? {
            std::net::SocketAddr::V4(socket_addr) => std::net::SocketAddrV6::new(
                socket_addr.ip().to_ipv6_mapped(),
                socket_addr.port(),
                0,
                0,
            ),
            std::net::SocketAddr::V6(socket_addr) => socket_addr,
        };
        Ok(PatternAddr::IpPort(socket_addr_v6))
    }
}

/// Set of TCP ports with special case for ‘all ports’.
#[derive(Debug, Clone)]
enum PortsSet {
    All,
    Some(std::collections::HashSet<u16>),
}

impl PortsSet {
    fn new(port: u16) -> Self {
        Self::Some(std::collections::HashSet::from_iter(Some(port).into_iter()))
    }

    fn add_all(&mut self) {
        *self = Self::All
    }

    fn add_port(&mut self, port: u16) {
        if let Self::Some(ports) = self {
            ports.insert(port);
        }
    }

    fn contains(&self, port: u16) -> bool {
        match self {
            Self::All => true,
            Self::Some(ports) => ports.contains(&port),
        }
    }
}

#[cfg(test)]
mod test {
    #[test]
    fn test_pattern_addr() {
        fn parse(value: &str) -> String {
            match value.parse() {
                Ok(super::PatternAddr::Ip(ip)) => ip.to_string(),
                Ok(super::PatternAddr::IpPort(addr)) => addr.to_string(),
                Err(_) => "err".to_string(),
            }
        }

        assert_eq!("err", parse("foo"));
        assert_eq!("err", parse("192.0.2.*"));
        assert_eq!("err", parse("192.0.2.0/24"));
        assert_eq!("err", parse("192.0.2.4.5"));
        assert_eq!("err", parse("192.0.2.4:424242"));

        assert_eq!("::ffff:192.0.2.4", parse("192.0.2.4"));
        assert_eq!("[::ffff:192.0.2.4]:0", parse("192.0.2.4:0"));
        assert_eq!("[::ffff:192.0.2.4]:42", parse("192.0.2.4:42"));

        assert_eq!("::1", parse("::1"));
        assert_eq!("[::1]:42", parse("[::1]:42"));

        assert_eq!("::ffff:127.0.0.1", parse("::ffff:127.0.0.1"));
        assert_eq!("[::ffff:127.0.0.1]:42", parse("[::ffff:127.0.0.1]:42"));
    }

    #[test]
    fn test_ports_set() {
        let mut ports = super::PortsSet::new(42);
        assert!(ports.contains(42));
        assert!(!ports.contains(24));
        ports.add_port(24);
        assert!(ports.contains(42));
        assert!(ports.contains(24));
        assert!(!ports.contains(12));
        ports.add_all();
        assert!(ports.contains(42));
        assert!(ports.contains(24));
        assert!(ports.contains(12));
    }

    #[test]
    fn test_blacklist() {
        use std::net::*;

        let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 4));
        let lo4 = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let lo6 = IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1));
        let mapped_ip = IpAddr::V6("::ffff:192.0.2.4".parse().unwrap());
        let mapped_lo4 = IpAddr::V6("::ffff:127.0.0.1".parse().unwrap());

        let blacklist = super::Blacklist::from_iter(vec![
            "127.0.0.1".to_string(),
            "192.0.2.4:42".to_string(),
            "[::1]:42".to_string(),
        ]);

        assert!(blacklist.contains(&SocketAddr::new(lo4, 42)));
        assert!(blacklist.contains(&SocketAddr::new(lo4, 8080)));
        assert!(blacklist.contains(&SocketAddr::new(ip, 42)));
        assert!(!blacklist.contains(&SocketAddr::new(ip, 8080)));
        assert!(blacklist.contains(&SocketAddr::new(lo6, 42)));
        assert!(!blacklist.contains(&SocketAddr::new(lo6, 8080)));
        assert!(blacklist.contains(&SocketAddr::new(mapped_lo4, 42)));
        assert!(blacklist.contains(&SocketAddr::new(mapped_lo4, 8080)));
        assert!(blacklist.contains(&SocketAddr::new(mapped_ip, 42)));
        assert!(!blacklist.contains(&SocketAddr::new(mapped_ip, 8080)));
    }
}
