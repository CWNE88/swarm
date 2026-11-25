//! Minimal length-prefixed framing over a QUIC stream: a `u16`
//! big-endian byte length followed by that many bytes. QUIC streams are
//! already ordered/reliable byte streams with no built-in message
//! boundaries, so something has to mark where one Noise handshake
//! message or sealed heartbeat frame ends and the next begins.

use anyhow::{bail, Result};
use quinn::{RecvStream, SendStream};

/// Comfortably larger than a Noise handshake message or a sealed
/// heartbeat payload; just a sanity bound against a misbehaving peer
/// claiming an absurd length.
pub const MAX_FRAME: usize = 8192;

pub async fn write_frame(stream: &mut SendStream, payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_FRAME {
        bail!("frame of {} bytes exceeds MAX_FRAME", payload.len());
    }
    let len = payload.len() as u16;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    Ok(())
}

/// Reads one frame. *Not* cancel-safe (quinn's `read_exact` isn't):
/// dropping this future partway through a frame loses whatever it had
/// already read, and every later read on the stream is then misaligned.
/// Fine anywhere it's simply awaited to completion; anything that races
/// a read against other work in a `select!` must use `FrameReader`.
pub async fn read_frame(stream: &mut RecvStream) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).await?;
    let len = u16::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        bail!("peer claimed a frame of {len} bytes, exceeding MAX_FRAME");
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

/// A cancel-safe `read_frame`: bytes read so far live in here, not in
/// the read future, so a read dropped partway through a frame (another
/// `select!` branch winning — a cover tick, a heartbeat, a send) loses
/// nothing, and the next call picks up exactly where it left off. Keep
/// one per stream, for that stream's whole life.
#[derive(Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub async fn read(&mut self, stream: &mut RecvStream) -> Result<Vec<u8>> {
        loop {
            if self.buf.len() >= 2 {
                let len = u16::from_be_bytes([self.buf[0], self.buf[1]]) as usize;
                if len > MAX_FRAME {
                    bail!("peer claimed a frame of {len} bytes, exceeding MAX_FRAME");
                }
                if self.buf.len() >= 2 + len {
                    let frame = self.buf[2..2 + len].to_vec();
                    self.buf.drain(..2 + len);
                    return Ok(frame);
                }
            }
            match stream.read_chunk(MAX_FRAME, true).await? {
                Some(chunk) => self.buf.extend_from_slice(&chunk.bytes),
                None => bail!("stream finished"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A raw (no Noise) bidirectional QUIC stream over loopback.
    async fn stream_pair() -> (SendStream, RecvStream, quinn::Endpoint, quinn::Endpoint) {
        let server = crate::transport::bind("[::1]:0".parse().unwrap()).unwrap();
        let client = crate::transport::bind("[::1]:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        let accept = {
            let server = server.clone();
            tokio::spawn(async move {
                let conn = server.accept().await.unwrap().await.unwrap();
                conn.accept_bi().await.unwrap()
            })
        };
        let conn = client.connect(addr, "swarm").unwrap().await.unwrap();
        let (mut send, _client_recv) = conn.open_bi().await.unwrap();
        send.write_all(&[0, 0]).await.unwrap();
        let (_server_send, mut recv) = accept.await.unwrap();
        assert_eq!(read_frame(&mut recv).await.unwrap(), Vec::<u8>::new());
        std::mem::forget(_client_recv);
        std::mem::forget(_server_send);
        (send, recv, server, client)
    }

    #[tokio::test]
    async fn a_frame_reader_survives_being_cancelled_mid_frame() {
        let (mut send, mut recv, _server, _client) = stream_pair().await;
        let body: Vec<u8> = (0..600u32).map(|i| i as u8).collect();

        let writer = tokio::spawn({
            let body = body.clone();
            async move {
                send.write_all(&(body.len() as u16).to_be_bytes()).await.unwrap();
                tokio::time::sleep(Duration::from_millis(60)).await;
                send.write_all(&body[..250]).await.unwrap();
                tokio::time::sleep(Duration::from_millis(60)).await;
                send.write_all(&body[250..]).await.unwrap();
                send.write_all(&3u16.to_be_bytes()).await.unwrap();
                send.write_all(b"end").await.unwrap();
                send
            }
        });

        let mut reader = FrameReader::default();
        let mut interruptions = 0;
        let first = loop {
            tokio::select! {
                frame = reader.read(&mut recv) => break frame.unwrap(),
                _ = tokio::time::sleep(Duration::from_millis(10)) => interruptions += 1,
            }
        };
        assert!(interruptions > 5, "the read should have been cut off repeatedly mid-frame");
        assert_eq!(first, body);
        assert_eq!(reader.read(&mut recv).await.unwrap(), b"end");
        let _send = writer.await.unwrap();
    }
}
