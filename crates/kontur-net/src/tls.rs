//! Per-session TLS: self-signed cert generation, SHA-256 cert pinning.
//!
//! The host generates a fresh self-signed certificate each session (`generate()`).
//! The fingerprint (SHA-256 of the DER-encoded certificate) is embedded in the
//! invite link and used by the joining client as the sole trust root — no CA,
//! no hostname check; the pin delivered via the private invite link is enough.
//!
//! Agent endpoint stays plaintext (localhost-only; `nc` bridge to CC).

use std::io;
use std::sync::Arc;

use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsConnector, client::TlsStream as ClientTlsStream};

// ---------------------------------------------------------------------------
// SessionTls
// ---------------------------------------------------------------------------

/// TLS context for the operator listener.  Generated once per session;
/// the fingerprint is embedded in every invite link.
pub struct SessionTls {
    pub acceptor: TlsAcceptor,
    /// SHA-256 of the DER-encoded certificate (cert-DER hash).
    pub fingerprint: [u8; 32],
}

impl SessionTls {
    /// First 16 bytes of the SHA-256 cert-DER fingerprint.
    /// Second-preimage resistance at 128 bits is adequate for cert pinning;
    /// full collision resistance (birthday bound) is irrelevant here.
    pub fn fingerprint16(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out.copy_from_slice(&self.fingerprint[..16]);
        out
    }
}

/// Generate a per-session self-signed TLS certificate (CN "kontur-session").
/// Returns a `SessionTls` containing the `TlsAcceptor` and the cert fingerprint.
pub fn generate() -> SessionTls {
    // Generate a self-signed certificate with rcgen.
    let subject_alt_names = vec!["kontur-session".to_string()];
    let CertifiedKey { cert, key_pair } =
        generate_simple_self_signed(subject_alt_names)
            .expect("rcgen: failed to generate self-signed cert");

    let cert_der: CertificateDer<'static> = cert.into();
    let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
        PrivatePkcs8KeyDer::from(key_pair.serialize_der()),
    );

    // Fingerprint = SHA-256(DER bytes).
    let fingerprint: [u8; 32] = Sha256::digest(cert_der.as_ref()).into();

    // Build rustls ServerConfig (no client auth).
    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("rustls: invalid cert/key");

    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    SessionTls {
        acceptor,
        fingerprint,
    }
}

// ---------------------------------------------------------------------------
// Pinned client connector
// ---------------------------------------------------------------------------

/// Connect to `addr` over TLS, verifying that the server certificate's
/// SHA-256 DER fingerprint (first 16 bytes) matches `fp16`.
///
/// No CA chain, no hostname check: the pin (delivered via the private invite
/// link) is the sole trust root.
pub async fn connect_pinned(
    addr: &str,
    fp16: [u8; 16],
) -> io::Result<ClientTlsStream<TcpStream>> {
    let stream = TcpStream::connect(addr).await?;

    let client_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier { fp16 }))
        .with_no_client_auth();

    let connector = TlsConnector::from(Arc::new(client_config));
    // The server name is irrelevant — we pin by cert fingerprint, not hostname.
    let server_name = ServerName::try_from("kontur-session")
        .expect("static server name is valid");

    connector
        .connect(server_name, stream)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::ConnectionRefused, e))
}

// ---------------------------------------------------------------------------
// PinnedVerifier
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct PinnedVerifier {
    fp16: [u8; 16],
}

impl rustls::client::danger::ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let got: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        if got[..16] == self.fp16 {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "certificate fingerprint mismatch: expected {}, got {}",
                fp16_hex(&self.fp16),
                fp16_hex(&got[..16].try_into().unwrap()),
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// ---------------------------------------------------------------------------
// Fingerprint helpers
// ---------------------------------------------------------------------------

/// Encode a 32-byte fingerprint as 64 lowercase hex characters.
pub fn fp_hex(fp: &[u8; 32]) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

/// Encode a 16-byte fingerprint prefix as 32 lowercase hex characters.
fn fp16_hex(fp: &[u8; 16]) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parse a 64-char hex fingerprint string back to 32 bytes.
pub fn parse_fp_hex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Helper: accept-loop integration
// ---------------------------------------------------------------------------

/// Accept a TCP connection, perform the TLS handshake, and attach the
/// resulting stream to `server`.  Handshake errors are logged and dropped —
/// the function never panics.
pub async fn attach_tls(
    server: &crate::server::SessionServer,
    acceptor: &TlsAcceptor,
    stream: TcpStream,
) {
    match acceptor.accept(stream).await {
        Ok(tls_stream) => {
            server.attach(tls_stream).await;
        }
        Err(e) => {
            // Log to stderr (no panic, no crash).
            eprintln!("kontur-net: TLS handshake error: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // fp_hex / parse_fp_hex roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn fp_hex_roundtrip() {
        let fp: [u8; 32] = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
            0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10,
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        ];
        let hex = fp_hex(&fp);
        assert_eq!(hex.len(), 64);
        let parsed = parse_fp_hex(&hex).expect("parse_fp_hex failed");
        assert_eq!(parsed, fp);
    }

    #[test]
    fn parse_fp_hex_rejects_short() {
        assert!(parse_fp_hex("abcd").is_none());
    }

    #[test]
    fn parse_fp_hex_rejects_non_hex() {
        let bad = "z".repeat(64);
        assert!(parse_fp_hex(&bad).is_none());
    }

    // -----------------------------------------------------------------------
    // Wrong pin → handshake fails with a clear error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn tls_wrong_pin_rejected() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let session_tls = generate();
        let acceptor = session_tls.acceptor.clone();

        // Dummy server: just accept TLS, don't attach to any SessionServer.
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let _ = acceptor.accept(stream).await;
                });
            }
        });

        // Use a wrong (all-zeros) fingerprint.
        let wrong_fp = [0u8; 16];
        let result = connect_pinned(&addr, wrong_fp).await;

        assert!(
            result.is_err(),
            "wrong pin must cause connect_pinned to fail"
        );
        // The error message is forwarded from the rustls General error we emit
        // in PinnedVerifier::verify_server_cert; confirm it mentions the mismatch.
        // (If rustls ever wraps the message in a way that obscures the text, the
        // is_err() assertion above is still the load-bearing check.)
        let err_str = result.unwrap_err().to_string();
        assert!(
            err_str.contains("mismatch") || err_str.contains("fingerprint"),
            "expected error mentioning mismatch or fingerprint; got: {err_str}"
        );
    }

    // -----------------------------------------------------------------------
    // Correct pin → TLS handshake succeeds
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn tls_correct_pin_connects() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let session_tls = generate();
        let fingerprint = session_tls.fingerprint16();
        let acceptor = session_tls.acceptor.clone();

        // Dummy server: accept TLS.
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let _ = acceptor.accept(stream).await;
                });
            }
        });

        // Correct fingerprint → should succeed.
        let result = connect_pinned(&addr, fingerprint).await;
        assert!(result.is_ok(), "correct pin must succeed: {:?}", result.err());
    }
}
