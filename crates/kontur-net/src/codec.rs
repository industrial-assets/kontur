use std::io;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use tokio::io::BufReader;

/// Write a serializable value as JSON-lines to an async writer.
pub async fn write_json<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, v: &T) -> io::Result<()> {
    let mut line = serde_json::to_string(v)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.flush().await
}

/// Read a JSON-lines value from an async reader.
/// Returns `Ok(None)` on EOF, `Ok(Some(v))` on successful parse.
/// Returns `Err(InvalidData)` if the line exceeds 1 MiB (post-hoc cap —
/// bounded by one oversized allocation; a streaming byte-limit would require
/// a custom reader and is overkill for the expected message sizes here).
pub async fn read_json<R: AsyncBufRead + Unpin, T: for<'de> Deserialize<'de>>(
    r: &mut R,
) -> io::Result<Option<T>> {
    let mut line = String::new();
    if r.read_line(&mut line).await? == 0 {
        return Ok(None);
    }
    if line.len() > 1_000_000 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    serde_json::from_str(line.trim_end())
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ClientMsg, ServerMsg, WireRole, WireState, WirePhase, WireSeat};
    use kontur_core::{OperatorId, GateId, Verdict, ReviewDepth, Timestamp, Sig, CastVerdict};

    async fn roundtrip<T>(v: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let (client, server) = tokio::io::duplex(4096);
        let (_, mut w) = tokio::io::split(client);
        let (r, _) = tokio::io::split(server);
        let mut reader = tokio::io::BufReader::new(r);
        write_json(&mut w, v).await.unwrap();
        read_json(&mut reader).await.unwrap().expect("one message")
    }

    #[tokio::test]
    async fn roundtrip_client_hello() {
        let original = ClientMsg::Hello {
            seat: "A".to_string(),
            operator: OperatorId([1; 32]),
        };

        let recovered: ClientMsg = roundtrip(&original).await;
        assert_eq!(original, recovered);
    }

    #[tokio::test]
    async fn roundtrip_client_ready() {
        let original = ClientMsg::Ready;

        let recovered: ClientMsg = roundtrip(&original).await;
        assert_eq!(original, recovered);
    }

    #[tokio::test]
    async fn roundtrip_client_cast() {
        let cast_verdict = CastVerdict {
            operator: OperatorId([1; 32]),
            verdict: Verdict::Go,
            depth: ReviewDepth::Summary,
            comment: None,
            cast_at: Timestamp(1000),
            signature: Sig([0; 64]),
        };
        let original = ClientMsg::Cast {
            gate_id: GateId("gate-1".to_string()),
            verdict: cast_verdict,
        };

        let recovered: ClientMsg = roundtrip(&original).await;
        assert_eq!(original, recovered);
    }

    #[tokio::test]
    async fn roundtrip_client_hand_edit() {
        let original = ClientMsg::HandEdit {
            path: "/src/main.rs".to_string(),
            contents: "fn main() {}".to_string(),
        };

        let recovered: ClientMsg = roundtrip(&original).await;
        assert_eq!(original, recovered);
    }

    #[tokio::test]
    async fn roundtrip_client_rotate() {
        let original = ClientMsg::Rotate;

        let recovered: ClientMsg = roundtrip(&original).await;
        assert_eq!(original, recovered);
    }

    #[tokio::test]
    async fn roundtrip_client_bye() {
        let original = ClientMsg::Bye;

        let recovered: ClientMsg = roundtrip(&original).await;
        assert_eq!(original, recovered);
    }

    #[tokio::test]
    async fn roundtrip_server_welcome() {
        let original = ServerMsg::Welcome {
            seat: "A".to_string(),
        };

        let recovered: ServerMsg = roundtrip(&original).await;
        assert_eq!(original, recovered);
    }

    #[tokio::test]
    async fn roundtrip_server_state() {
        let original = ServerMsg::State(Box::new(WireState {
            phase: WirePhase::AwaitOperators,
            seats: vec![WireSeat {
                label: "Seat A".to_string(),
                operator: OperatorId([1; 32]),
                role: WireRole::Driver,
                linked: true,
                ready: false,
            }],
            fleet: vec![],
            log: vec!["started".to_string()],
            gate: None,
        }));

        let recovered: ServerMsg = roundtrip(&original).await;
        assert_eq!(original, recovered);
    }

    #[tokio::test]
    async fn roundtrip_server_rejected() {
        let original = ServerMsg::Rejected {
            reason: "invalid operator".to_string(),
        };

        let recovered: ServerMsg = roundtrip(&original).await;
        assert_eq!(original, recovered);
    }

    #[tokio::test]
    async fn roundtrip_multi_message() {
        let (client, server) = tokio::io::duplex(4096);
        let (_, mut w) = tokio::io::split(client);
        let (r, _) = tokio::io::split(server);
        let mut reader = tokio::io::BufReader::new(r);

        let msg1 = ClientMsg::Ready;
        let msg2 = ClientMsg::Bye;

        write_json(&mut w, &msg1).await.unwrap();
        write_json(&mut w, &msg2).await.unwrap();

        let back1: ClientMsg = read_json(&mut reader).await.unwrap().expect("first message");
        let back2: ClientMsg = read_json(&mut reader).await.unwrap().expect("second message");

        assert_eq!(msg1, back1);
        assert_eq!(msg2, back2);
    }

    #[tokio::test]
    async fn read_json_eof() {
        let bytes = b"";
        let mut reader = bytes.as_ref();
        let mut buf_reader = BufReader::new(&mut reader);
        let result: io::Result<Option<ClientMsg>> = read_json(&mut buf_reader).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);
    }

    #[tokio::test]
    async fn read_json_invalid() {
        let bytes = b"not valid json\n";
        let mut reader = bytes.as_ref();
        let mut buf_reader = BufReader::new(&mut reader);
        let result: io::Result<Option<ClientMsg>> = read_json(&mut buf_reader).await;
        assert!(result.is_err());
    }
}
