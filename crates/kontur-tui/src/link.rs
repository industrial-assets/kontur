//! Invite-link formatting, parsing, and IP discovery for zero-config hosting.
//!
//! Link format (v2): `kontur://<ip>:<port>/<code>`
//! where `code` = base32(secret16 ‖ fp16), RFC 4648 lowercase, no padding, exactly 52 chars.
//!
//! `secret16`: 16 random bytes embedded in the link (the operator's magic-link credential).
//! The Ed25519 seed is DERIVED: `seed = sha256("kontur-invite-v1" ‖ secret16)`.
//! `fp16`: first 16 bytes of the SHA-256 cert-DER fingerprint (cert pinning).
//!
//! v1 links (64-hex path, or containing `#`) are rejected with a clear upgrade message.

use std::net::IpAddr;

use kontur_core::sha256;

// ---------------------------------------------------------------------------
// RFC 4648 base32 (lowercase, no padding)
// ---------------------------------------------------------------------------

const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";

fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut buf: u64 = 0;
    let mut bits = 0u32;
    for &b in bytes {
        buf = (buf << 8) | b as u64;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buf >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buf << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

fn base32_char_to_val(c: u8) -> Option<u8> {
    match c {
        b'a'..=b'z' => Some(c - b'a'),
        b'A'..=b'Z' => Some(c - b'A'),
        b'2'..=b'7' => Some(c - b'2' + 26),
        _ => None,
    }
}

fn base32_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut buf: u64 = 0;
    let mut bits = 0u32;
    for &b in s.as_bytes() {
        let v = base32_char_to_val(b)
            .ok_or_else(|| format!("invalid base32 character: '{}'", b as char))?;
        buf = (buf << 5) | v as u64;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// derive_seed
// ---------------------------------------------------------------------------

/// Derive the Ed25519 seed from a 16-byte secret using a domain-separated hash.
///
/// `seed = sha256("kontur-invite-v1" ‖ secret16)`
///
/// Domain separator provides second-preimage resistance; 128-bit security is
/// fine for key derivation in this context.
pub fn derive_seed(secret: &[u8; 16]) -> [u8; 32] {
    let mut input = Vec::with_capacity(16 + 16);
    input.extend_from_slice(b"kontur-invite-v1");
    input.extend_from_slice(secret);
    sha256(&input).0
}

// ---------------------------------------------------------------------------
// format_invite
// ---------------------------------------------------------------------------

/// Format a v2 paste-able invite link.
///
/// The code is base32(secret16 ‖ fp16) — 32 bytes → 52 chars, no padding.
///
/// # Example
/// ```
/// let secret = [0xabu8; 16];
/// let fp16 = [0x00u8; 16];
/// let link = kontur_tui::link::format_invite("203.0.113.5", 7777, &secret, &fp16);
/// assert!(link.starts_with("kontur://203.0.113.5:7777/"));
/// assert!(!link.contains('#'));
/// ```
pub fn format_invite(ip: &str, port: u16, secret: &[u8; 16], fp16: &[u8; 16]) -> String {
    let mut payload = Vec::with_capacity(32);
    payload.extend_from_slice(secret);
    payload.extend_from_slice(fp16);
    let code = base32_encode(&payload);
    format!("kontur://{ip}:{port}/{code}")
}

// ---------------------------------------------------------------------------
// parse_invite
// ---------------------------------------------------------------------------

/// Parse a v2 kontur invite link back into (addr, secret16, fp16).
///
/// `addr` is `host:port` suitable for TCP connection.
///
/// Strict rules:
/// - scheme must be `kontur://`
/// - address must contain a port
/// - code must be exactly 52 lowercase base32 chars (no `#` fragment)
/// - v1 links (64-hex path or containing `#`) are rejected with a clear message
pub fn parse_invite(link: &str) -> Result<(String, [u8; 16], [u8; 16]), String> {
    let rest = link
        .strip_prefix("kontur://")
        .ok_or_else(|| format!("invalid scheme: expected 'kontur://' in '{link}'"))?;

    // v1 detection: fragment `#` present → old format
    if rest.contains('#') {
        return Err("old invite format — ask the host for a fresh link".to_string());
    }

    // Split on the first `/` to separate host:port from code.
    let slash = rest
        .find('/')
        .ok_or_else(|| "missing '/' between address and code".to_string())?;

    let addr_part = &rest[..slash];
    let code = &rest[slash + 1..];

    if addr_part.is_empty() {
        return Err("missing address in invite link".to_string());
    }

    // Validate addr has a port component.
    if !addr_part.contains(':') {
        return Err(format!("address '{addr_part}' is missing a port"));
    }

    // v1 detection: 64-char hex path (old format) → old format error.
    // A 64-char all-hex code would decode as v1 format.
    if code.len() == 64 && code.bytes().all(|b: u8| b.is_ascii_hexdigit()) {
        return Err("old invite format — ask the host for a fresh link".to_string());
    }

    // Validate code: exactly 52 base32 chars.
    if code.len() != 52 {
        return Err(format!(
            "invite code must be 52 base32 characters; got {} characters",
            code.len()
        ));
    }

    let payload = base32_decode(code)
        .map_err(|e| format!("invalid invite code: {e}"))?;

    // 52 base32 chars → floor(52*5/8) = 32 bytes
    if payload.len() < 32 {
        return Err(format!(
            "invite code decoded to {} bytes; expected 32",
            payload.len()
        ));
    }

    let mut secret16 = [0u8; 16];
    let mut fp16 = [0u8; 16];
    secret16.copy_from_slice(&payload[..16]);
    fp16.copy_from_slice(&payload[16..32]);

    Ok((addr_part.to_string(), secret16, fp16))
}

// ---------------------------------------------------------------------------
// discover_ip
// ---------------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // base32 roundtrip tests
    // -----------------------------------------------------------------------

    #[test]
    fn base32_known_answer_rfc4648() {
        // RFC 4648 §10 vectors (lowercased, unpadded) — catches a
        // systematically-wrong-but-invertible codec that roundtrips would miss.
        assert_eq!(base32_encode(b""), "");
        assert_eq!(base32_encode(b"f"), "my");
        assert_eq!(base32_encode(b"fo"), "mzxq");
        assert_eq!(base32_encode(b"foo"), "mzxw6");
        assert_eq!(base32_encode(b"foob"), "mzxw6yq");
        assert_eq!(base32_encode(b"fooba"), "mzxw6ytb");
        assert_eq!(base32_encode(b"foobar"), "mzxw6ytboi");
        assert_eq!(base32_decode("mzxw6ytboi").unwrap(), b"foobar");
        assert_eq!(base32_decode("MZXW6YTBOI").unwrap(), b"foobar"); // uppercase accepted
    }

    #[test]
    fn base32_roundtrip_all_zeros() {
        let input = [0u8; 32];
        let encoded = base32_encode(&input);
        assert_eq!(encoded.len(), 52, "32 bytes must encode to 52 base32 chars");
        let decoded = base32_decode(&encoded).expect("decode failed");
        assert_eq!(&decoded[..32], &input[..]);
    }

    #[test]
    fn base32_roundtrip_all_ff() {
        let input = [0xffu8; 32];
        let encoded = base32_encode(&input);
        assert_eq!(encoded.len(), 52);
        let decoded = base32_decode(&encoded).expect("decode failed");
        assert_eq!(&decoded[..32], &input[..]);
    }

    #[test]
    fn base32_roundtrip_fixed_vector() {
        // Known 32-byte payload: alternating 0xaa 0x55
        let mut input = [0u8; 32];
        for (i, b) in input.iter_mut().enumerate() {
            *b = if i % 2 == 0 { 0xaa } else { 0x55 };
        }
        let encoded = base32_encode(&input);
        assert_eq!(encoded.len(), 52);
        let decoded = base32_decode(&encoded).expect("decode failed");
        assert_eq!(&decoded[..32], &input[..]);
    }

    #[test]
    fn base32_rejects_bad_char() {
        let bad = "!".to_string() + &"a".repeat(51);
        let err = base32_decode(&bad).unwrap_err();
        assert!(err.contains("invalid base32 character"), "got: {err}");
    }

    #[test]
    fn base32_rejects_bad_length() {
        // Not a base32_decode test directly, but verifies parse_invite rejects wrong lengths.
        let short_code = "aaaa"; // 4 chars → wrong length for invite
        let link = format!("kontur://1.2.3.4:7777/{short_code}");
        let err = parse_invite(&link).unwrap_err();
        assert!(err.contains("52"), "expected error mentioning 52; got: {err}");
    }

    // -----------------------------------------------------------------------
    // format / parse roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn format_parse_roundtrip() {
        let secret: [u8; 16] = [0xab; 16];
        let fp16: [u8; 16] = [0x12; 16];
        let link = format_invite("203.0.113.5", 7777, &secret, &fp16);
        let (addr, parsed_secret, parsed_fp16) = parse_invite(&link).expect("parse should succeed");
        assert_eq!(addr, "203.0.113.5:7777");
        assert_eq!(parsed_secret, secret);
        assert_eq!(parsed_fp16, fp16);
    }

    #[test]
    fn link_is_52_chars() {
        let secret = [0u8; 16];
        let fp16 = [0u8; 16];
        let link = format_invite("127.0.0.1", 7777, &secret, &fp16);
        // Strip scheme and address to get just the code.
        let code = link.split('/').next_back().unwrap();
        assert_eq!(code.len(), 52, "v2 code must be exactly 52 chars; got {}", code.len());
    }

    // -----------------------------------------------------------------------
    // v1 rejection
    // -----------------------------------------------------------------------

    #[test]
    fn v1_rejected_with_hex_path() {
        // 64-char hex path (old v1 format)
        let token = "ab".repeat(32); // 64 hex chars
        let link = format!("kontur://1.2.3.4:7777/{token}");
        let err = parse_invite(&link).unwrap_err();
        assert!(
            err.contains("old invite format"),
            "expected old-format error; got: {err}"
        );
    }

    #[test]
    fn v1_rejected_with_hash_fragment() {
        // v1 links contained `#fp` fragment
        let token = "ab".repeat(32);
        let fp = "cd".repeat(32);
        let link = format!("kontur://1.2.3.4:7777/{token}#{fp}");
        let err = parse_invite(&link).unwrap_err();
        assert!(
            err.contains("old invite format"),
            "expected old-format error; got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // derive_seed tests
    // -----------------------------------------------------------------------

    #[test]
    fn derive_seed_deterministic() {
        let secret = [0x42u8; 16];
        let s1 = derive_seed(&secret);
        let s2 = derive_seed(&secret);
        assert_eq!(s1, s2, "derive_seed must be deterministic");
    }

    #[test]
    fn derive_seed_differs() {
        let s1 = derive_seed(&[0x01u8; 16]);
        let s2 = derive_seed(&[0x02u8; 16]);
        assert_ne!(s1, s2, "different secrets must produce different seeds");
    }

    // -----------------------------------------------------------------------
    // Other parse error cases
    // -----------------------------------------------------------------------

    #[test]
    fn rejects_bad_scheme() {
        let err = parse_invite("http://1.2.3.4:7777/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .unwrap_err();
        assert!(err.contains("invalid scheme"), "got: {err}");
    }

    #[test]
    fn rejects_missing_slash() {
        let err = parse_invite("kontur://1.2.3.4:7777").unwrap_err();
        assert!(err.contains("missing '/'"), "got: {err}");
    }

    #[test]
    fn rejects_missing_port() {
        let code = "a".repeat(52);
        let link = format!("kontur://1.2.3.4/{code}");
        let err = parse_invite(&link).unwrap_err();
        assert!(err.contains("port"), "got: {err}");
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
