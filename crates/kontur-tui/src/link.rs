//! Invite-link formatting, parsing, and IP discovery for zero-config hosting.
//!
//! Link format: `kontur://<ip>:<port>/<64-hex-token>`
//! The token is the operator seat's 32-byte seed, hex-encoded.

use std::net::IpAddr;

// ---------------------------------------------------------------------------
// format_invite
// ---------------------------------------------------------------------------

/// Format a paste-able invite link.
///
/// # Example
/// ```
/// let seed = [0xabu8; 32];
/// let link = kontur_tui::link::format_invite("203.0.113.5", 7777, &seed);
/// assert!(link.starts_with("kontur://203.0.113.5:7777/"));
/// assert_eq!(link.len(), "kontur://203.0.113.5:7777/".len() + 64);
/// ```
pub fn format_invite(ip: &str, port: u16, seed: &[u8; 32]) -> String {
    let hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();
    format!("kontur://{ip}:{port}/{hex}")
}

// ---------------------------------------------------------------------------
// parse_invite
// ---------------------------------------------------------------------------

/// Parse a kontur invite link back into (addr, seed).
///
/// `addr` is `host:port` suitable for TCP connection.
///
/// Strict rules:
/// - scheme must be `kontur://`
/// - exactly one `/` separating host:port from token
/// - token must be exactly 64 lowercase hex chars (32 bytes)
pub fn parse_invite(link: &str) -> Result<(String, [u8; 32]), String> {
    let rest = link
        .strip_prefix("kontur://")
        .ok_or_else(|| format!("invalid scheme: expected 'kontur://' in '{link}'"))?;

    // Split on the first `/` to separate host:port from token.
    let slash = rest
        .find('/')
        .ok_or_else(|| "missing '/' between address and token".to_string())?;

    let addr_part = &rest[..slash];
    let token_part = &rest[slash + 1..];

    if addr_part.is_empty() {
        return Err("missing address in invite link".to_string());
    }

    // Validate addr has a port component.
    if !addr_part.contains(':') {
        return Err(format!("address '{addr_part}' is missing a port"));
    }

    // Validate token: exactly 64 hex chars.
    if token_part.len() != 64 {
        return Err(format!(
            "token must be 64 hex characters (32 bytes); got {} characters",
            token_part.len()
        ));
    }

    let seed = hex_to_seed(token_part)
        .ok_or_else(|| format!("token contains non-hex characters: '{token_part}'"))?;

    Ok((addr_part.to_string(), seed))
}

fn hex_to_seed(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = hex_digit(chunk[0])?;
        let lo = hex_digit(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// discover_ip
// ---------------------------------------------------------------------------

/// Discover the best IP address to put in an invite link.
///
/// Priority:
/// 1. Public IP via `curl -s --max-time 2 https://api.ipify.org` (if it
///    returns a valid `IpAddr`).
/// 2. LAN IP via the UDP trick (connect to 8.8.8.8:80, read local_addr).
/// 3. Fallback: `127.0.0.1`.
///
/// Returns the IP as a string. Call sites should print the caveat line:
/// "link uses your public/LAN IP — if NATed, forward port 7777 or share your LAN address"
/// Public IP via an external service, validated as an IpAddr (a captive
/// portal returning HTML must never end up in a link). None if unreachable.
pub fn discover_public_ip() -> Option<String> {
    let output = std::process::Command::new("curl")
        .args(["-s", "--max-time", "2", "https://api.ipify.org"])
        .output()
        .ok()?;
    let raw = String::from_utf8_lossy(&output.stdout);
    let candidate = raw.trim();
    candidate.parse::<IpAddr>().ok().map(|ip| ip.to_string())
}

/// LAN IP via the UDP-connect trick (no packets sent). None if unavailable.
pub fn discover_lan_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|a| a.ip().to_string())
}

/// Best single address for an invite: LAN first (works for same-machine and
/// same-network operators with zero router config), then public, then loopback.
pub fn discover_ip() -> String {
    discover_lan_ip()
        .or_else(discover_public_ip)
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_from_byte(b: u8) -> [u8; 32] {
        [b; 32]
    }

    #[test]
    fn roundtrip_format_then_parse() {
        let seed = seed_from_byte(0xab);
        let link = format_invite("203.0.113.5", 7777, &seed);
        let (addr, parsed_seed) = parse_invite(&link).expect("parse should succeed");
        assert_eq!(addr, "203.0.113.5:7777");
        assert_eq!(parsed_seed, seed);
    }

    #[test]
    fn roundtrip_random_looking_seed() {
        let mut seed = [0u8; 32];
        for (i, b) in seed.iter_mut().enumerate() {
            *b = (i * 7 + 13) as u8;
        }
        let link = format_invite("10.0.0.1", 9999, &seed);
        let (addr, parsed_seed) = parse_invite(&link).expect("parse should succeed");
        assert_eq!(addr, "10.0.0.1:9999");
        assert_eq!(parsed_seed, seed);
    }

    #[test]
    fn rejects_bad_scheme() {
        let err = parse_invite("http://1.2.3.4:7777/abababababababababababababababababababababababababababababababababab")
            .unwrap_err();
        assert!(err.contains("invalid scheme"), "got: {err}");
    }

    #[test]
    fn rejects_missing_slash() {
        let err = parse_invite("kontur://1.2.3.4:7777").unwrap_err();
        assert!(err.contains("missing '/'"), "got: {err}");
    }

    #[test]
    fn rejects_short_token() {
        let err = parse_invite("kontur://1.2.3.4:7777/abcdef").unwrap_err();
        assert!(err.contains("64"), "got: {err}");
    }

    #[test]
    fn rejects_non_hex_token() {
        // 64 chars but contains 'z'
        let bad = "kontur://1.2.3.4:7777/".to_string()
            + "z"
            + &"a".repeat(63);
        let err = parse_invite(&bad).unwrap_err();
        assert!(err.contains("non-hex"), "got: {err}");
    }

    #[test]
    fn rejects_missing_port() {
        let token = "ab".repeat(32);
        let link = format!("kontur://1.2.3.4/{token}");
        let err = parse_invite(&link).unwrap_err();
        assert!(err.contains("port"), "got: {err}");
    }

    #[test]
    fn link_format_is_stable() {
        // Verifies the exact format a user would paste, using a known seed.
        let seed = [0x9fu8; 32];
        let link = format_invite("203.0.113.5", 7777, &seed);
        assert_eq!(
            link,
            format!(
                "kontur://203.0.113.5:7777/{}",
                "9f".repeat(32)
            )
        );
    }
}

/// The invite in both reachability flavours, for the console's [l] toggle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InviteLinks {
    /// Same-machine / same-network join command (no router config needed).
    pub lan: Option<String>,
    /// Off-network join command (requires forwarding the operator port).
    pub wan: Option<String>,
    pub port: u16,
}
