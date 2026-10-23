//! Packet framing: 3-byte length + 1-byte sequence id, with payloads of 16 MiB - 1 or more split
//! across several packets (TiDB `server/internal/packetio`).

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream};

use crate::constants::MAX_PAYLOAD_LEN;

/// Errors from reading a packet.
#[derive(Debug)]
pub enum PacketError {
    Io(io::Error),
    /// The client used an unexpected sequence id.
    BadSequence {
        expected: u8,
        got: u8,
    },
    /// The logical payload exceeds `max_allowed_packet`.
    TooLarge,
}

impl From<io::Error> for PacketError {
    fn from(e: io::Error) -> Self {
        PacketError::Io(e)
    }
}

impl std::fmt::Display for PacketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PacketError::Io(e) => write!(f, "{e}"),
            PacketError::BadSequence { expected, got } => {
                write!(
                    f,
                    "invalid sequence, received {got} while expecting {expected}"
                )
            }
            PacketError::TooLarge => {
                f.write_str("Got a packet bigger than 'max_allowed_packet' bytes")
            }
        }
    }
}

impl std::error::Error for PacketError {}

/// Buffered packet reader/writer over a byte stream. Writes are buffered until [`flush`].
///
/// [`flush`]: PacketIo::flush
#[derive(Debug)]
pub struct PacketIo<S> {
    stream: BufStream<S>,
    seq: u8,
    max_allowed_packet: usize,
}

impl<S: AsyncRead + AsyncWrite + Unpin> PacketIo<S> {
    pub fn new(stream: S, max_allowed_packet: usize) -> Self {
        PacketIo {
            stream: BufStream::with_capacity(16 * 1024, 16 * 1024, stream),
            seq: 0,
            max_allowed_packet,
        }
    }

    /// Next sequence id expected/used.
    pub fn sequence(&self) -> u8 {
        self.seq
    }

    /// Resets the sequence id; called before reading each command.
    pub fn reset_sequence(&mut self) {
        self.seq = 0;
    }

    /// Reads one logical packet, joining split packets.
    pub async fn read_packet(&mut self) -> Result<Vec<u8>, PacketError> {
        let mut payload = Vec::new();
        loop {
            let mut header = [0u8; 4];
            self.stream.read_exact(&mut header).await?;
            let len =
                usize::from(header[0]) | usize::from(header[1]) << 8 | usize::from(header[2]) << 16;
            if header[3] != self.seq {
                return Err(PacketError::BadSequence {
                    expected: self.seq,
                    got: header[3],
                });
            }
            self.seq = self.seq.wrapping_add(1);
            if payload.len() + len > self.max_allowed_packet {
                return Err(PacketError::TooLarge);
            }
            let start = payload.len();
            payload.resize(start + len, 0);
            self.stream.read_exact(&mut payload[start..]).await?;
            if len < MAX_PAYLOAD_LEN {
                return Ok(payload);
            }
        }
    }

    /// Buffers one logical packet, splitting it if needed. A payload whose length is a multiple
    /// of `MAX_PAYLOAD_LEN` is terminated by an empty packet.
    pub async fn write_packet(&mut self, payload: &[u8]) -> io::Result<()> {
        let mut rest = payload;
        loop {
            let n = rest.len().min(MAX_PAYLOAD_LEN);
            let len = (n as u32).to_le_bytes();
            self.stream
                .write_all(&[len[0], len[1], len[2], self.seq])
                .await?;
            self.stream.write_all(&rest[..n]).await?;
            self.seq = self.seq.wrapping_add(1);
            rest = &rest[n..];
            if n < MAX_PAYLOAD_LEN {
                return Ok(());
            }
        }
    }

    pub async fn flush(&mut self) -> io::Result<()> {
        self.stream.flush().await
    }

    /// Shuts down the write side (best effort, used on connection close).
    pub async fn shutdown(&mut self) -> io::Result<()> {
        self.stream.shutdown().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::DEFAULT_MAX_ALLOWED_PACKET;

    /// A `PacketIo` reading `raw` (the other end of a duplex pipe, already filled and closed).
    async fn reader(raw: &[u8], max_allowed_packet: usize) -> PacketIo<tokio::io::DuplexStream> {
        let (mut a, b) = tokio::io::duplex(raw.len() + 1);
        a.write_all(raw).await.unwrap();
        drop(a);
        PacketIo::new(b, max_allowed_packet)
    }

    async fn roundtrip(payload: Vec<u8>) -> (Vec<u8>, Vec<u8>) {
        let (a, mut b) = tokio::io::duplex(payload.len() + 1024);
        let mut w = PacketIo::new(a, DEFAULT_MAX_ALLOWED_PACKET);
        w.write_packet(&payload).await.unwrap();
        w.flush().await.unwrap();
        drop(w);
        let mut raw = Vec::new();
        b.read_to_end(&mut raw).await.unwrap();
        let back = reader(&raw, DEFAULT_MAX_ALLOWED_PACKET)
            .await
            .read_packet()
            .await
            .unwrap();
        (raw, back)
    }

    #[tokio::test]
    async fn small_packet_framing() {
        let (raw, back) = roundtrip(b"\x03select 1".to_vec()).await;
        assert_eq!(&raw[..4], &[9, 0, 0, 0]);
        assert_eq!(&raw[4..], b"\x03select 1");
        assert_eq!(back, b"\x03select 1");
    }

    #[tokio::test]
    async fn empty_packet_framing() {
        let (raw, back) = roundtrip(Vec::new()).await;
        assert_eq!(raw, [0, 0, 0, 0]);
        assert!(back.is_empty());
    }

    #[tokio::test]
    async fn exact_max_payload_gets_empty_trailer() {
        // Same rule as TiDB's PacketIO.WritePacket: `for length >= maxPayloadLen`.
        let payload = vec![7u8; MAX_PAYLOAD_LEN];
        let (raw, back) = roundtrip(payload.clone()).await;
        assert_eq!(raw.len(), 4 + MAX_PAYLOAD_LEN + 4);
        assert_eq!(&raw[..4], &[0xff, 0xff, 0xff, 0]);
        assert_eq!(&raw[4 + MAX_PAYLOAD_LEN..], &[0, 0, 0, 1]);
        assert_eq!(back, payload);
    }

    #[tokio::test]
    async fn large_payload_is_split() {
        let payload: Vec<u8> = (0..(2 * MAX_PAYLOAD_LEN + 10)).map(|i| i as u8).collect();
        let (raw, back) = roundtrip(payload.clone()).await;
        assert_eq!(raw.len(), payload.len() + 3 * 4);
        assert_eq!(&raw[..4], &[0xff, 0xff, 0xff, 0]);
        let second = 4 + MAX_PAYLOAD_LEN;
        assert_eq!(&raw[second..second + 4], &[0xff, 0xff, 0xff, 1]);
        let third = 2 * (4 + MAX_PAYLOAD_LEN);
        assert_eq!(&raw[third..third + 4], &[10, 0, 0, 2]);
        assert_eq!(back, payload);
    }

    #[tokio::test]
    async fn bad_sequence_is_rejected() {
        let mut r = reader(&[1, 0, 0, 5, 0x0e], DEFAULT_MAX_ALLOWED_PACKET).await;
        match r.read_packet().await {
            Err(PacketError::BadSequence {
                expected: 0,
                got: 5,
            }) => {}
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn too_large_is_rejected() {
        let mut r = reader(&[10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8).await;
        assert!(matches!(r.read_packet().await, Err(PacketError::TooLarge)));
    }
}
