//! The one path every call to a provider, an MCP server or a webhook endpoint
//! takes (the security standard's "Calling providers"): only HTTPS, only to
//! public addresses (checked after DNS resolution, and connected to only at
//! the addresses checked), no redirects, bounded in size and time, failures
//! classified by what happened, never by what the other side wrote, and the
//! credential injected last and scrubbed from anything that comes back.
//!
//! [`OutboundHttp`] is the seam; services depend on it, and tests substitute
//! it. Which addresses count as public is decided here, in [`is_public`], so
//! every implementation refuses the same ones.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use async_trait::async_trait;
use zeroize::Zeroizing;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

/// A credential to send: one header, set after every other header so nothing
/// the caller adds can replace or shadow it.
pub struct Credential {
    pub header: String,
    pub value: Zeroizing<String>,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("header", &self.header)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct OutboundRequest {
    pub method: Method,
    /// An absolute `https://` URL to a public host.
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub credential: Option<Credential>,
}

/// How long a call may take and how much it may answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub timeout: Duration,
    pub max_response_bytes: usize,
}

impl Limits {
    /// The default answer's limit.
    pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 1 << 20;
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            max_response_bytes: Self::DEFAULT_MAX_RESPONSE_BYTES,
        }
    }
}

/// What came back. Any status is an answer, `5xx` included: the caller
/// classifies statuses. The credential is already scrubbed from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Why a destination is refused before anything is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Not an absolute URL, or one with a user name or password in it.
    InvalidUrl,
    /// Not `https`.
    NotHttps,
    /// The host is, or resolves to, a private, loopback, link-local or other
    /// non-public address.
    NotPublic,
    /// The host doesn't resolve, which may be temporary: a retrier treats it
    /// as it treats being unreachable.
    Unresolvable,
    /// A header name or value that isn't valid, a credential that is empty,
    /// longer than [`MAX_CREDENTIAL_BYTES`] or sent in a reserved header.
    InvalidHeader,
}

/// How a call failed. Never carries anything the other side wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OutboundError {
    #[error("refused before sending: {0:?}")]
    Refused(Refusal),
    /// The other side answered with a redirect (301, 302, 303, 307 or 308),
    /// which is never followed. Other `3xx`, like `304`, are answers.
    #[error("answered with a redirect, which isn't followed")]
    Redirected,
    #[error("took longer than allowed")]
    TimedOut,
    #[error("answered more than allowed")]
    TooLarge,
    /// No connection could be made, TLS included: a certificate that doesn't
    /// verify is unreachable too.
    #[error("couldn't connect")]
    Unreachable,
    /// The connection failed partway.
    #[error("the exchange failed")]
    Failed,
}

#[async_trait]
pub trait OutboundHttp: Send + Sync {
    async fn send(&self, request: OutboundRequest, limits: Limits) -> Result<OutboundResponse, OutboundError>;
}

/// Whether Zupflow may call this address: only globally routable unicast.
/// Everything special-purpose is refused. IPv4-mapped and IPv4-translated
/// IPv6 addresses are judged by the IPv4 address they carry; the tunnelling
/// and translation prefixes (NAT64, 6to4, Teredo) are refused outright.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => is_public_v6(ip),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let special = a == 0 // "this network"
        || a == 10
        || (a == 100 && (64..=127).contains(&b)) // shared address space (CGNAT)
        || a == 127
        || (a == 169 && b == 254) // link-local, where cloud metadata services live
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 192 && b == 0 && c == 2) // documentation
        || (a == 192 && b == 88 && c == 99) // 6to4 relay anycast
        || (a == 192 && b == 168)
        || (a == 198 && (18..=19).contains(&b)) // benchmarking
        || (a == 198 && b == 51 && c == 100) // documentation
        || (a == 203 && b == 0 && c == 113) // documentation
        || a >= 224; // multicast, reserved and broadcast
    !special
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    if let Some(embedded) = ip.to_ipv4_mapped() {
        return is_public_v4(embedded);
    }
    // IPv4-translated (`::ffff:0:a.b.c.d`): judged by the address it carries.
    if segments[..4] == [0, 0, 0, 0] && segments[4] == 0xffff && segments[5] == 0 {
        let [a, b] = segments[6].to_be_bytes();
        let [c, d] = segments[7].to_be_bytes();
        return is_public_v4(Ipv4Addr::new(a, b, c, d));
    }
    let special = ip.is_unspecified()
        || ip.is_loopback()
        || segments[..6] == [0, 0, 0, 0, 0, 0] // IPv4-compatible, deprecated
        || (segments[0] == 0x0064 && segments[1] == 0xff9b) // NAT64: refused outright, not judged by its IPv4 address
        || (segments[0] == 0x0100 && segments[1..4] == [0, 0, 0]) // discard
        // IETF protocol assignments, 2001::/23: Teredo, benchmarking, ORCHID and the rest.
        || (segments[0] == 0x2001 && segments[1] < 0x0200)
        || (segments[0] == 0x2001 && segments[1] == 0x0db8) // documentation
        || segments[0] == 0x2002 // 6to4: refused outright
        || (segments[0] == 0x3fff && segments[1] < 0x1000) // documentation, 3fff::/20
        || segments[0] == 0x5f00 // SRv6 segment identifiers
        || segments[0] & 0xfe00 == 0xfc00 // unique local
        || segments[0] & 0xffc0 == 0xfe80 // link-local
        || segments[0] & 0xffc0 == 0xfec0 // site-local, deprecated
        || segments[0] & 0xff00 == 0xff00; // multicast
    !special
}

/// Headers the outbound path sets itself: a caller's are dropped, and a
/// credential or an app's token placement may never name one.
pub const RESERVED_HEADERS: [&str; 11] = [
    "host",
    "accept-encoding",
    "connection",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

/// The longest credential the path sends: scrubbing stays cheap, and a
/// tenant can't make every call slow by choosing an enormous one.
pub const MAX_CREDENTIAL_BYTES: usize = 8 << 10;

/// Replaces every occurrence of `secret` in `bytes` with `[redacted]`, in
/// time linear in their lengths.
pub fn scrub(bytes: &mut Vec<u8>, secret: &[u8]) {
    if secret.is_empty() || bytes.len() < secret.len() {
        return;
    }
    let finder = memchr::memmem::Finder::new(secret);
    let mut out = Vec::with_capacity(bytes.len());
    let mut copied = 0;
    for start in finder.find_iter(bytes) {
        if start < copied {
            continue;
        }
        out.extend_from_slice(&bytes[copied..start]);
        out.extend_from_slice(b"[redacted]");
        copied = start + secret.len();
    }
    if copied == 0 {
        return;
    }
    out.extend_from_slice(&bytes[copied..]);
    *bytes = out;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("an address")
    }

    #[test]
    fn refuses_every_non_public_address() {
        for refused in [
            "0.0.0.0",
            "0.1.2.3",
            "10.0.0.1",
            "100.64.0.1",
            "100.127.255.254",
            "127.0.0.1",
            "127.255.255.255",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.8",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "64:ff9b::a00:1",
            "100::1",
            "2001:db8::1",
            "2001::1",
            "2002:a00:1::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001:2::1",
            "2001:10::1",
            "2001:20::1",
            "3fff::1",
            "3fff:fff::1",
            "5f00::1",
            "::ffff:0:7f00:1",
            "::ffff:0:a00:1",
        ] {
            assert!(!is_public(ip(refused)), "{refused}");
        }
    }

    #[test]
    fn allows_public_addresses() {
        for allowed in [
            "1.1.1.1",
            "8.8.8.8",
            "100.63.255.255",
            "100.128.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "192.167.1.1",
            "203.0.114.1",
            "2606:4700:4700::1111",
            "2a00:1450:4001:80b::200e",
            "::ffff:8.8.8.8",
            "::ffff:0:808:808",
            "2001:4860:4860::8888",
            "3fff:1000::1",
        ] {
            assert!(is_public(ip(allowed)), "{allowed}");
        }
    }

    #[test]
    fn scrubs_every_occurrence() {
        let mut body = b"token=sk_live_abc; again sk_live_abc!".to_vec();
        scrub(&mut body, b"sk_live_abc");
        assert_eq!(body, b"token=[redacted]; again [redacted]!");
        let mut untouched = b"nothing here".to_vec();
        scrub(&mut untouched, b"secret");
        assert_eq!(untouched, b"nothing here");
        scrub(&mut untouched, b"");
        assert_eq!(untouched, b"nothing here");
        let mut overlapping = b"aaaa".to_vec();
        scrub(&mut overlapping, b"aa");
        assert_eq!(overlapping, b"[redacted][redacted]");
    }

    /// Linear: a long secret against a large body stays fast.
    #[test]
    fn scrubs_in_linear_time() {
        let secret = vec![b'a'; MAX_CREDENTIAL_BYTES - 1];
        let mut body = vec![b'a'; 1 << 20];
        let started = std::time::Instant::now();
        scrub(&mut body, &[secret.as_slice(), b"b"].concat());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    }
}
