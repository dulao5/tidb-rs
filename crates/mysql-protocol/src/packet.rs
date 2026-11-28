//! Packet framing: 3-byte length + 1-byte sequence id, with payloads of 16 MiB - 1 or more split
//! across several packets (TiDB `server/internal/packetio`).

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::task::{Context, Poll};

use tokio::io::{
    AsyncBufRead, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter,
    ReadHalf, WriteHalf,
};
use tokio::sync::Mutex;

use crate::constants::MAX_PAYLOAD_LEN;

/// Errors from reading a packet.
#[derive(Debug)]
pub enum PacketError {
    /// Clean EOF at a packet boundary (0 bytes read; Go `io.EOF`).
    Eof,
    Io(io::Error),
    /// The client used an unexpected sequence id.
    BadSequence {
        expected: u8,
        got: u8,
    },
    /// The logical payload exceeds `max_allowed_packet`.
    TooLarge,
}

impl PacketError {
    /// Format this error for structured logs matching Go `packetio.go:163-177` and `io.EOF` /
    /// `io.ErrUnexpectedEOF`.
    pub fn log_string(&self) -> String {
        match self {
            PacketError::Eof => "EOF".to_owned(),
            PacketError::Io(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                "unexpected EOF".to_owned()
            }
            PacketError::Io(e) => e.to_string(),
            PacketError::BadSequence { expected, got } => {
                format!("[server:8052]invalid sequence, received {got} while expecting {expected}")
            }
            PacketError::TooLarge => {
                "[server:1153]Got a packet bigger than 'max_allowed_packet' bytes".to_owned()
            }
        }
    }
}

impl From<io::Error> for PacketError {
    fn from(e: io::Error) -> Self {
        PacketError::Io(e)
    }
}

impl std::fmt::Display for PacketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PacketError::Eof => f.write_str("EOF"),
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

/// Bytes read and written since the last [`SharedIoCounts::flush`] into
/// `tidb_server_packet_io_bytes`. Shared across split reader and writer halves and flushed once
/// per command and on drop.
#[derive(Debug, Default)]
pub struct SharedIoCounts {
    read: AtomicU64,
    written: AtomicU64,
}

impl SharedIoCounts {
    /// Publish any un-flushed byte counts to `tidb_server_packet_io_bytes`.
    pub fn flush(&self) {
        let r = self.read.swap(0, Ordering::Relaxed);
        if r > 0 {
            tidb_metrics::packet_in(r as usize);
        }
        let w = self.written.swap(0, Ordering::Relaxed);
        if w > 0 {
            tidb_metrics::packet_out(w as usize);
        }
    }
}

impl Drop for SharedIoCounts {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Independent buffered packet reader sharing `seq` and `counts` with its paired writer.
#[derive(Debug)]
pub struct PacketReader<R> {
    stream: BufReader<R>,
    seq: Arc<AtomicU8>,
    max_allowed_packet: usize,
    counts: Arc<SharedIoCounts>,
}

impl<R: AsyncRead + Unpin> PacketReader<R> {
    /// Wraps a read half with shared sequence counter and I/O byte tracker.
    pub fn new(
        stream: R,
        max_allowed_packet: usize,
        seq: Arc<AtomicU8>,
        counts: Arc<SharedIoCounts>,
    ) -> Self {
        Self {
            stream: BufReader::with_capacity(16 * 1024, stream),
            seq,
            max_allowed_packet,
            counts,
        }
    }

    /// Consumes the reader, returning its inner stream and any buffered unconsumed bytes.
    pub fn into_parts(self) -> (R, Vec<u8>) {
        let buffered = self.stream.buffer().to_vec();
        (self.stream.into_inner(), buffered)
    }

    /// Reads one logical packet, joining split packets.
    ///
    /// Sequence ID is incremented only after verifying `header[3] == expected`.
    pub async fn read_packet(&mut self) -> Result<Vec<u8>, PacketError> {
        let mut payload = Vec::new();
        loop {
            let mut header = [0u8; 4];
            let first = self.stream.read(&mut header[..1]).await?;
            if first == 0 {
                if payload.is_empty() {
                    return Err(PacketError::Eof);
                }
                return Err(PacketError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unexpected EOF",
                )));
            }
            self.stream
                .read_exact(&mut header[1..])
                .await
                .map_err(|e| {
                    if e.kind() == io::ErrorKind::UnexpectedEof {
                        PacketError::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "unexpected EOF",
                        ))
                    } else {
                        PacketError::Io(e)
                    }
                })?;
            let len =
                usize::from(header[0]) | usize::from(header[1]) << 8 | usize::from(header[2]) << 16;
            let expected = self.seq.load(Ordering::SeqCst);
            if header[3] != expected {
                return Err(PacketError::BadSequence {
                    expected,
                    got: header[3],
                });
            }
            self.seq.store(expected.wrapping_add(1), Ordering::SeqCst);
            if payload.len() + len > self.max_allowed_packet {
                return Err(PacketError::TooLarge);
            }
            let start = payload.len();
            payload.resize(start + len, 0);
            self.stream
                .read_exact(&mut payload[start..])
                .await
                .map_err(|e| {
                    if e.kind() == io::ErrorKind::UnexpectedEof {
                        PacketError::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "unexpected EOF",
                        ))
                    } else {
                        PacketError::Io(e)
                    }
                })?;
            if len < MAX_PAYLOAD_LEN {
                // Go counts the payload of the logical packet, without headers.
                self.counts
                    .read
                    .fetch_add(payload.len() as u64, Ordering::Relaxed);
                return Ok(payload);
            }
        }
    }

    /// Non-blocking liveness probe via `BufReader::poll_fill_buf` (Decision 8).
    ///
    /// Returns `true` when buffered bytes exist, new unread bytes arrive, or the socket is
    /// still open (`Poll::Pending`); returns `false` on EOF (`Ok(&[])`) or I/O error.
    pub fn probe_alive(&mut self) -> bool {
        if !self.stream.buffer().is_empty() {
            return true;
        }
        let waker = futures::task::noop_waker_ref();
        let mut cx = Context::from_waker(waker);
        match Pin::new(&mut self.stream).poll_fill_buf(&mut cx) {
            Poll::Pending => true,
            Poll::Ready(Ok(buf)) => !buf.is_empty(),
            Poll::Ready(Err(_)) => false,
        }
    }
}

/// Independent buffered packet writer sharing `seq` and `counts` with its paired reader.
#[derive(Debug)]
pub struct PacketWriter<W> {
    stream: BufWriter<W>,
    seq: Arc<AtomicU8>,
    counts: Arc<SharedIoCounts>,
}

impl<W: AsyncWrite + Unpin> PacketWriter<W> {
    /// Wraps a write half with shared sequence counter and I/O byte tracker.
    pub fn new(stream: W, seq: Arc<AtomicU8>, counts: Arc<SharedIoCounts>) -> Self {
        Self {
            stream: BufWriter::with_capacity(16 * 1024, stream),
            seq,
            counts,
        }
    }

    /// Returns the underlying write half (used by `into_parts` after flushing).
    pub fn into_inner(self) -> W {
        self.stream.into_inner()
    }

    /// Buffers one logical packet, splitting it if needed. A payload whose length is a multiple
    /// of `MAX_PAYLOAD_LEN` is terminated by an empty packet.
    pub async fn write_packet(&mut self, payload: &[u8]) -> io::Result<()> {
        // Go counts the logical packet with one 4-byte header.
        self.counts
            .written
            .fetch_add(payload.len() as u64 + 4, Ordering::Relaxed);
        let mut rest = payload;
        loop {
            let n = rest.len().min(MAX_PAYLOAD_LEN);
            let len = (n as u32).to_le_bytes();
            let seq = self.seq.fetch_add(1, Ordering::SeqCst);
            self.stream
                .write_all(&[len[0], len[1], len[2], seq])
                .await?;
            self.stream.write_all(&rest[..n]).await?;
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

/// Split buffered packet reader/writer over a byte stream.
///
/// The read half is behind an async mutex shared only with the non-blocking
/// [`Self::is_connection_alive_probe`] (`try_lock()`); during query execution `read_packet`
/// is idle so `SqlKiller` can probe socket EOF without holding or blocking writes.
#[derive(Debug)]
pub struct PacketIo<S> {
    reader: Arc<Mutex<PacketReader<ReadHalf<S>>>>,
    writer: PacketWriter<WriteHalf<S>>,
    seq: Arc<AtomicU8>,
    counts: Arc<SharedIoCounts>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> PacketIo<S> {
    pub fn new(stream: S, max_allowed_packet: usize) -> Self {
        let (r, w) = tokio::io::split(stream);
        let seq = Arc::new(AtomicU8::new(0));
        let counts = Arc::new(SharedIoCounts::default());
        PacketIo {
            reader: Arc::new(Mutex::new(PacketReader::new(
                r,
                max_allowed_packet,
                Arc::clone(&seq),
                Arc::clone(&counts),
            ))),
            writer: PacketWriter::new(w, Arc::clone(&seq), Arc::clone(&counts)),
            seq,
            counts,
        }
    }

    /// Returns a synchronous non-blocking liveness probe for `SqlKiller::is_connection_alive`.
    pub fn is_connection_alive_probe(&self) -> tidb_memory::sqlkiller::AliveFn
    where
        S: Send + 'static,
    {
        let reader = Arc::clone(&self.reader);
        Box::new(move || {
            let Ok(mut guard) = reader.try_lock() else {
                // Locked means `read_packet` is currently active or being torn down.
                return true;
            };
            guard.probe_alive()
        })
    }

    /// Publish the bytes counted so far to `tidb_server_packet_io_bytes`.
    pub fn flush_counts(&mut self) {
        self.counts.flush();
    }

    /// Next sequence id expected/used.
    pub fn sequence(&self) -> u8 {
        self.seq.load(Ordering::SeqCst)
    }

    /// Continues with a given sequence id (after replacing the underlying stream).
    pub fn set_sequence(&mut self, seq: u8) {
        self.seq.store(seq, Ordering::SeqCst);
    }

    /// Returns the stream and the bytes already read from it but not consumed. Pending writes
    /// must have been flushed.
    pub fn into_parts(self) -> (S, Vec<u8>) {
        let write_half = self.writer.into_inner();
        let reader = Arc::try_unwrap(self.reader)
            .unwrap_or_else(|_| panic!("PacketIo::into_parts with live reader clone"))
            .into_inner();
        let (read_half, buffered) = reader.into_parts();
        (read_half.unsplit(write_half), buffered)
    }

    /// Resets the sequence id; called before reading each command.
    pub fn reset_sequence(&mut self) {
        self.seq.store(0, Ordering::SeqCst);
    }

    /// Reads one logical packet, joining split packets.
    pub async fn read_packet(&mut self) -> Result<Vec<u8>, PacketError> {
        self.reader.lock().await.read_packet().await
    }

    /// Buffers one logical packet, splitting it if needed.
    pub async fn write_packet(&mut self, payload: &[u8]) -> io::Result<()> {
        self.writer.write_packet(payload).await
    }

    pub async fn flush(&mut self) -> io::Result<()> {
        self.writer.flush().await
    }

    /// Shuts down the write side (best effort, used on connection close).
    pub async fn shutdown(&mut self) -> io::Result<()> {
        self.writer.shutdown().await
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
    async fn bad_sequence_is_rejected_without_advancing_seq() {
        let mut r = reader(&[1, 0, 0, 5, 0x0e], DEFAULT_MAX_ALLOWED_PACKET).await;
        match r.read_packet().await {
            Err(PacketError::BadSequence {
                expected: 0,
                got: 5,
            }) => {}
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            r.sequence(),
            0,
            "seq must NOT increment when header[3] != expected"
        );
    }

    #[tokio::test]
    async fn too_large_is_rejected() {
        let mut r = reader(&[10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8).await;
        assert!(matches!(r.read_packet().await, Err(PacketError::TooLarge)));
    }

    #[tokio::test]
    async fn split_reader_writer_share_seq_and_probe_alive_detects_disconnect() {
        let (mut peer, server_stream) = tokio::io::duplex(256);
        let mut io = PacketIo::new(server_stream, DEFAULT_MAX_ALLOWED_PACKET);
        let probe = io.is_connection_alive_probe();

        // Connected and idle -> alive.
        assert!(probe());

        // Peer sends command with seq=0 -> reader advances seq to 1; writer shares seq=1.
        peer.write_all(&[2, 0, 0, 0, 0x03, b'1']).await.unwrap();
        let req = io.read_packet().await.unwrap();
        assert_eq!(req, b"\x031");
        assert_eq!(io.sequence(), 1);

        io.write_packet(b"ok").await.unwrap();
        io.flush().await.unwrap();
        assert_eq!(io.sequence(), 2);

        let mut reply = [0u8; 6];
        peer.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, &[2, 0, 0, 1, b'o', b'k']);

        // Dropping peer signals EOF -> non-blocking poll_fill_buf detects disconnect immediately.
        drop(peer);
        assert!(!probe(), "probe_alive must detect closed peer via EOF");
    }

    #[tokio::test]
    async fn into_parts_preserves_buffered_trailing_bytes() {
        let (mut peer, server_stream) = tokio::io::duplex(256);
        peer.write_all(&[1, 0, 0, 0, 0x0e, b'X', b'Y'])
            .await
            .unwrap();
        drop(peer);
        let mut io = PacketIo::new(server_stream, DEFAULT_MAX_ALLOWED_PACKET);
        let pkt = io.read_packet().await.unwrap();
        assert_eq!(pkt, [0x0e]);
        let (mut stream, buffered) = io.into_parts();
        assert_eq!(buffered, b"XY");
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
    }
}
