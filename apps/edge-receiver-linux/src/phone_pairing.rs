//! The phone pairing window.
//!
//! `edge-receiver-linux pair-phone` opens a window with a one-time 128-bit
//! token, shown in the QR code. A phone that presents it is pinned. Wrong
//! tokens are refused without closing the window, so garbage attempts can't
//! invalidate the real token; after a few failures from one address, that
//! address is limited to one attempt per second. (A 128-bit token can't be
//! guessed; the limit only protects the receiver.)

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

pub(crate) const PAIRING_WINDOW: Duration = Duration::from_secs(5 * 60);
const FREE_FAILURES: u32 = 3;
const FAILURE_SPACING: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairingRefusal {
    /// No window is open, or it expired.
    Closed,
    WrongToken,
    RateLimited,
    /// The token was right but the receiver couldn't pin the phone.
    Failed,
}

impl PairingRefusal {
    pub fn code(self) -> &'static str {
        match self {
            Self::Closed => "pairing_closed",
            Self::WrongToken => "pairing_wrong_token",
            Self::RateLimited => "pairing_rate_limited",
            Self::Failed => "pairing_failed",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::Closed => "pairing isn't open on the computer; run pair-phone again",
            Self::WrongToken => "the pairing code doesn't match; scan the current QR code",
            Self::RateLimited => "too many wrong pairing codes; wait a moment",
            Self::Failed => "the computer couldn't save the pairing; see its log",
        }
    }
}

struct Failures {
    count: u32,
    last: Instant,
}

pub(crate) struct PairingWindow {
    token: String,
    expires: Instant,
    failures: HashMap<IpAddr, Failures>,
}

impl PairingWindow {
    pub fn open(now: Instant) -> Self {
        let mut bytes = [0_u8; 16];
        getrandom::getrandom(&mut bytes).expect("the OS random source is available");
        Self {
            token: bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
            expires: now + PAIRING_WINDOW,
            failures: HashMap::new(),
        }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn is_open(&self, now: Instant) -> bool {
        now < self.expires
    }

    /// Checks a token from `ip`. The caller pins the phone and only then
    /// closes the window, so a failed pin save leaves the token usable.
    pub fn check(&mut self, token: &str, ip: IpAddr, now: Instant) -> Result<(), PairingRefusal> {
        if !self.is_open(now) {
            return Err(PairingRefusal::Closed);
        }
        if let Some(failures) = self.failures.get(&ip)
            && failures.count >= FREE_FAILURES
            && now.duration_since(failures.last) < FAILURE_SPACING
        {
            return Err(PairingRefusal::RateLimited);
        }
        if constant_time_eq(token.as_bytes(), self.token.as_bytes()) {
            return Ok(());
        }
        let failures = self.failures.entry(ip).or_insert(Failures {
            count: 0,
            last: now,
        });
        failures.count += 1;
        failures.last = now;
        Err(PairingRefusal::WrongToken)
    }
}

/// The link in the QR code. The phone learns the computer's name from its
/// `Hello`, so everything here is ASCII without spaces.
pub(crate) fn pairing_uri(
    addresses: &[IpAddr],
    port: u16,
    fingerprint: &str,
    token: &str,
) -> String {
    let mut uri = format!("hyprmobile://pair?v=1&fp={fingerprint}&port={port}&token={token}");
    for address in addresses {
        uri.push_str(&format!("&addr={address}"));
    }
    uri
}

/// Addresses a phone on the LAN can reach the listener on: no VPN tunnels,
/// overlays or bridges (a tunnel address is useless to the phone), and only
/// address families the listener accepts. IPv4 first.
pub(crate) fn lan_addresses(listen: SocketAddr) -> Vec<IpAddr> {
    let interfaces = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .map(|interface| (interface.name.clone(), interface.ip()));
    select_lan_addresses(interfaces, listen)
}

fn select_lan_addresses(
    interfaces: impl IntoIterator<Item = (String, IpAddr)>,
    listen: SocketAddr,
) -> Vec<IpAddr> {
    if !listen.ip().is_unspecified() {
        return vec![listen.ip()];
    }
    let mut addresses: Vec<IpAddr> = interfaces
        .into_iter()
        .filter(|(name, ip)| {
            !is_tunnel_or_virtual(name)
                && is_usable_lan_ip(*ip)
                // A `0.0.0.0` listener doesn't accept IPv6.
                && (listen.is_ipv6() || ip.is_ipv4())
        })
        .map(|(_, ip)| ip)
        .collect();
    addresses.sort_by_key(|ip| (ip.is_ipv6(), *ip));
    addresses.dedup();
    addresses
}

/// Interfaces that are VPN tunnels, overlays or local bridges, never the LAN.
fn is_tunnel_or_virtual(name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "lo",
        "tun",
        "tap",
        "wg",
        "ppp",
        "docker",
        "br-",
        "veth",
        "virbr",
        "vnet",
        "zt",
        "tailscale",
    ];
    PREFIXES.iter().any(|prefix| name.starts_with(prefix)) || name.contains("pia")
}

fn is_usable_lan_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified(),
        IpAddr::V6(ip) => !ip.is_loopback() && !ip.is_unspecified() && !ip.is_unicast_link_local(),
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHONE: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 0, 21));
    const OTHER: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 0, 99));

    #[test]
    fn tokens_are_random_128_bit_hex() {
        let now = Instant::now();
        let first = PairingWindow::open(now);
        let second = PairingWindow::open(now);
        assert_eq!(first.token().len(), 32);
        assert!(first.token().bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first.token(), second.token());
    }

    #[test]
    fn the_right_token_is_accepted_until_expiry() {
        let now = Instant::now();
        let mut window = PairingWindow::open(now);
        let token = window.token().to_string();
        assert_eq!(window.check(&token, PHONE, now), Ok(()));
        assert_eq!(
            window.check(&token, PHONE, now + PAIRING_WINDOW),
            Err(PairingRefusal::Closed)
        );
    }

    #[test]
    fn wrong_tokens_do_not_invalidate_the_real_one() {
        let now = Instant::now();
        let mut window = PairingWindow::open(now);
        let token = window.token().to_string();
        for _ in 0..10 {
            window.check("garbage", OTHER, now).ok();
        }
        assert_eq!(window.check(&token, PHONE, now), Ok(()));
    }

    #[test]
    fn repeated_failures_from_one_address_are_spaced_out() {
        let now = Instant::now();
        let mut window = PairingWindow::open(now);
        let token = window.token().to_string();
        for _ in 0..FREE_FAILURES {
            assert_eq!(
                window.check("garbage", OTHER, now),
                Err(PairingRefusal::WrongToken)
            );
        }
        assert_eq!(
            window.check(&token, OTHER, now),
            Err(PairingRefusal::RateLimited)
        );
        // Other addresses are unaffected, and the limit lifts after a second.
        assert_eq!(window.check(&token, PHONE, now), Ok(()));
        assert_eq!(window.check(&token, OTHER, now + FAILURE_SPACING), Ok(()));
    }

    #[test]
    fn pairing_uri_lists_every_address() {
        let uri = pairing_uri(
            &["192.168.0.11".parse().unwrap(), "fd57::1".parse().unwrap()],
            42420,
            "bb73d9ee:6e6f1951",
            "00ff",
        );
        assert_eq!(
            uri,
            "hyprmobile://pair?v=1&fp=bb73d9ee:6e6f1951&port=42420&token=00ff&addr=192.168.0.11&addr=fd57::1"
        );
    }

    #[test]
    fn lan_addresses_skip_tunnels_and_unreachable_families() {
        let interfaces = || {
            [
                ("lo", "127.0.0.1"),
                ("wlan0", "192.168.0.11"),
                ("wlan0", "fd57::1"),
                ("wlan0", "fe80::1"),
                ("wgpia0", "10.8.0.2"),
                ("tun0", "10.9.0.2"),
                ("docker0", "172.17.0.1"),
                ("enp3s0", "192.168.1.5"),
            ]
            .map(|(name, ip)| (name.to_string(), ip.parse().unwrap()))
        };
        let ipv4_only = select_lan_addresses(interfaces(), "0.0.0.0:42420".parse().unwrap());
        assert_eq!(
            ipv4_only,
            ["192.168.0.11", "192.168.1.5"].map(|ip| ip.parse::<IpAddr>().unwrap())
        );
        let dual_stack = select_lan_addresses(interfaces(), "[::]:42420".parse().unwrap());
        assert_eq!(
            dual_stack,
            ["192.168.0.11", "192.168.1.5", "fd57::1"].map(|ip| ip.parse::<IpAddr>().unwrap())
        );
        let fixed = select_lan_addresses(interfaces(), "192.168.0.11:42420".parse().unwrap());
        assert_eq!(fixed, ["192.168.0.11".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn token_comparison_needs_an_exact_match() {
        assert!(constant_time_eq(b"abcd", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abce"));
        assert!(!constant_time_eq(b"abcd", b"abc"));
    }
}
