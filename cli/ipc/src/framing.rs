//! The wire framing shared by both binaries: a 4-byte big-endian `u32` length prefix
//! followed by that many bytes of JSON payload.
//!
//! `dira`'s client (`cli/dira/src/client.rs`) and `dirad`'s control server
//! (`cli/dirad/src/control.rs`) each used to implement this inline against a concrete
//! `UnixStream`; both now call in here instead, so the wire format has exactly one
//! definition and it is generic over the transport rather than tied to a socket.

use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Read one framed payload: a 4-byte BE length prefix, then that many bytes.
pub async fn read_frame<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Write one framed payload: a 4-byte BE length prefix, then the bytes, then flush.
pub async fn write_frame<S: AsyncWrite + Unpin>(stream: &mut S, bytes: &[u8]) -> io::Result<()> {
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await?;
    stream.write_all(bytes).await?;
    stream.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    // `tokio::io::duplex` is a portable in-memory AsyncRead+AsyncWrite pipe, so this
    // round-trip exercises the framing itself on every platform without touching a
    // real socket or named pipe (those get their own transport-specific tests).
    #[tokio::test]
    async fn frame_round_trips_over_a_duplex_pipe() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_frame(&mut a, b"hello, dirad").await.unwrap();
        let got = read_frame(&mut b).await.unwrap();
        assert_eq!(got, b"hello, dirad");
    }

    #[tokio::test]
    async fn empty_frame_round_trips() {
        let (mut a, mut b) = tokio::io::duplex(64);
        write_frame(&mut a, b"").await.unwrap();
        let got = read_frame(&mut b).await.unwrap();
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn frame_larger_than_the_duplex_buffer_still_round_trips() {
        // Exercises multiple poll_read/poll_write cycles inside read_exact/write_all
        // (the duplex buffer is smaller than the payload).
        let (mut a, mut b) = tokio::io::duplex(16);
        let payload = vec![7u8; 10_000];
        let payload_clone = payload.clone();
        let writer = tokio::spawn(async move {
            write_frame(&mut a, &payload_clone).await.unwrap();
        });
        let got = read_frame(&mut b).await.unwrap();
        writer.await.unwrap();
        assert_eq!(got, payload);
    }
}
