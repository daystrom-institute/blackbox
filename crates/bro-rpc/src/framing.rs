//! Bounded frame I/O: a big-endian u32 length prefix followed by UTF-8 JSON.
//!
//! Never newline framing, and never an unbounded intermediate buffer. Writes
//! are bounded by serializing into a [`LimitedWriter`] that aborts mid-walk
//! the instant the caller's `max_frame_bytes` budget is exceeded, so a
//! streaming `Serialize` impl cannot force an unbounded allocation before we
//! notice the frame is too large. Reads reject an oversize frame from the
//! 4-byte length header alone, before allocating the payload buffer.

use std::io::{self, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::RpcError;

pub const DEFAULT_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// A framed transport over any async byte stream: one bounded JSON value per
/// frame, length-prefixed, never newline-delimited.
#[derive(Debug)]
pub struct FramedIo<T> {
    io: T,
    max_frame_bytes: usize,
}

impl<T> FramedIo<T> {
    pub fn new(io: T) -> Self {
        Self::with_max_frame_bytes(io, DEFAULT_MAX_FRAME_BYTES)
    }

    pub fn with_max_frame_bytes(io: T, max_frame_bytes: usize) -> Self {
        Self {
            io,
            max_frame_bytes,
        }
    }

    pub fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }

    pub fn into_inner(self) -> T {
        self.io
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> FramedIo<T> {
    pub async fn write_json<V: Serialize>(&mut self, value: &V) -> Result<(), RpcError> {
        let bytes = serialize_bounded(value, self.max_frame_bytes)?;
        write_frame(&mut self.io, &bytes).await
    }

    pub async fn read_json<V: DeserializeOwned>(&mut self) -> Result<V, RpcError> {
        read_json_frame(&mut self.io, self.max_frame_bytes).await
    }
}

/// A [`Write`] sink that aborts the moment the accumulated byte count would
/// exceed `limit`, instead of buffering an unbounded amount of output before
/// the caller gets a chance to check the length. `serde_json::to_writer`
/// walks the value incrementally, so a hostile or buggy `Serialize` impl that
/// streams an unbounded sequence is stopped mid-walk, not after materializing
/// gigabytes of JSON.
#[derive(Debug)]
struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
    attempted: Option<usize>,
}

impl LimitedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(16 * 1024)),
            limit,
            attempted: None,
        }
    }
}

impl Write for LimitedWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let attempted = self.bytes.len().saturating_add(buffer.len());
        if attempted > self.limit {
            self.attempted = Some(attempted);
            return Err(io::Error::other("bounded JSON frame limit exceeded"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn serialize_bounded<V: Serialize>(
    value: &V,
    max_frame_bytes: usize,
) -> Result<Vec<u8>, RpcError> {
    let effective_limit = max_frame_bytes.min(u32::MAX as usize);
    let mut writer = LimitedWriter::new(effective_limit);
    let result = serde_json::to_writer(&mut writer, value);
    if let Some(actual) = writer.attempted {
        return Err(RpcError::FrameTooLarge {
            actual,
            limit: effective_limit,
        });
    }
    result?;
    Ok(writer.bytes)
}

pub(crate) async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
) -> Result<(), RpcError> {
    if bytes.len() > u32::MAX as usize {
        return Err(RpcError::FrameTooLarge {
            actual: bytes.len(),
            limit: u32::MAX as usize,
        });
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub(crate) async fn read_json_frame<R: AsyncRead + Unpin, V: DeserializeOwned>(
    reader: &mut R,
    max_frame_bytes: usize,
) -> Result<V, RpcError> {
    // Reject an oversize frame from the 4-byte length header alone, before
    // allocating a buffer for the payload.
    let len = reader.read_u32().await? as usize;
    if len > max_frame_bytes {
        return Err(RpcError::FrameTooLarge {
            actual: len,
            limit: max_frame_bytes,
        });
    }
    let mut bytes = vec![0_u8; len];
    reader.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use serde::ser::{SerializeSeq, Serializer};
    use tokio::io::{AsyncWriteExt, duplex};

    use super::*;

    struct StreamingSequence {
        visited: Arc<AtomicUsize>,
    }

    impl Serialize for StreamingSequence {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let mut sequence = serializer.serialize_seq(Some(1_000_000))?;
            for value in 0_u64..1_000_000 {
                self.visited.fetch_add(1, Ordering::Relaxed);
                sequence.serialize_element(&value)?;
            }
            sequence.end()
        }
    }

    #[tokio::test]
    async fn round_trip_preserves_value() {
        let (left, right) = duplex(4096);
        let mut writer = FramedIo::new(left);
        let mut reader = FramedIo::new(right);
        writer
            .write_json(&serde_json::json!({"hello": "fleetd", "n": 7}))
            .await
            .unwrap();
        let value: serde_json::Value = reader.read_json().await.unwrap();
        assert_eq!(value, serde_json::json!({"hello": "fleetd", "n": 7}));
    }

    #[tokio::test]
    async fn outbound_serialization_stops_at_bound_before_writing() {
        let (left, mut right) = duplex(64);
        let visited = Arc::new(AtomicUsize::new(0));
        let mut framed = FramedIo::with_max_frame_bytes(left, 64);
        let error = framed
            .write_json(&StreamingSequence {
                visited: visited.clone(),
            })
            .await
            .unwrap_err();
        assert!(matches!(error, RpcError::FrameTooLarge { limit: 64, .. }));
        assert!(
            visited.load(Ordering::Relaxed) < 100,
            "serializer traversed the unbounded input after reaching the frame limit"
        );

        let read = tokio::time::timeout(Duration::from_millis(20), right.read_u32()).await;
        assert!(
            read.is_err(),
            "oversized frame wrote a partial length prefix"
        );
    }

    #[tokio::test]
    async fn oversized_inbound_frame_is_rejected_before_payload_allocation() {
        let (mut writer, reader) = duplex(64);
        writer.write_u32(9).await.unwrap();
        writer.write_all(b"123456789").await.unwrap();
        let mut framed = FramedIo::with_max_frame_bytes(reader, 8);
        let error = framed.read_json::<serde_json::Value>().await.unwrap_err();
        assert!(matches!(
            error,
            RpcError::FrameTooLarge {
                actual: 9,
                limit: 8
            }
        ));
    }

    #[tokio::test]
    async fn malformed_json_is_rejected_precisely() {
        let (mut writer, reader) = duplex(64);
        writer.write_u32(4).await.unwrap();
        writer.write_all(b"nope").await.unwrap();
        let mut framed = FramedIo::new(reader);
        assert!(matches!(
            framed.read_json::<serde_json::Value>().await,
            Err(RpcError::InvalidJson(_))
        ));
    }
}
