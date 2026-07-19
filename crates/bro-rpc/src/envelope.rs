//! The payload-generic post-handshake message wrapper.
//!
//! Concrete daemon<->fleetd message bodies (spawn specs, event/command
//! enums, ...) land with fleetd itself; this crate only owns the envelope
//! shape and the generation-fenced I/O that carries it.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{FramedIo, RpcError};

pub const MAX_MESSAGE_ID_BYTES: usize = 256;

/// The protocol version and connection generation a handshake settled on.
/// Every envelope read or written over the resulting [`NegotiatedIo`] is
/// checked against this binding; a mismatch fails closed rather than being
/// silently accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionBinding {
    pub protocol_version: u16,
    pub connection_generation: u64,
}

/// A single application-level message, generic over its body type `T`.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct Envelope<T> {
    pub protocol_version: u16,
    pub connection_generation: u64,
    pub message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    pub body: T,
}

/// A [`FramedIo`] paired with the [`ConnectionBinding`] a handshake produced.
/// `write_envelope`/`read_envelope` validate every envelope against that
/// binding on both sides of the wire, so a stale-generation or
/// wrong-protocol-version message never reaches application code.
#[derive(Debug)]
pub struct NegotiatedIo<T> {
    framed: FramedIo<T>,
    binding: ConnectionBinding,
}

impl<T> NegotiatedIo<T> {
    pub(crate) fn new(framed: FramedIo<T>, binding: ConnectionBinding) -> Self {
        Self { framed, binding }
    }

    pub fn binding(&self) -> ConnectionBinding {
        self.binding
    }

    pub fn max_frame_bytes(&self) -> usize {
        self.framed.max_frame_bytes()
    }

    pub fn into_framed(self) -> FramedIo<T> {
        self.framed
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> NegotiatedIo<T> {
    pub async fn write_envelope<B: Serialize>(
        &mut self,
        envelope: &Envelope<B>,
    ) -> Result<(), RpcError> {
        validate_envelope(envelope, self.binding)?;
        self.framed.write_json(envelope).await
    }

    pub async fn read_envelope<B: DeserializeOwned>(&mut self) -> Result<Envelope<B>, RpcError> {
        let envelope = self.framed.read_json::<Envelope<B>>().await?;
        validate_envelope(&envelope, self.binding)?;
        Ok(envelope)
    }
}

/// Checked on both read and write:
/// - `protocol_version` matches the negotiated binding
/// - `connection_generation` matches the negotiated binding (stale-generation
///   fencing: a message from a superseded connection generation is rejected,
///   not silently accepted)
/// - `message_id` is non-empty and at most [`MAX_MESSAGE_ID_BYTES`] bytes
/// - `reply_to`, when present, is non-empty, at most
///   [`MAX_MESSAGE_ID_BYTES`] bytes, and different from `message_id` (a
///   message cannot reply to itself)
pub fn validate_envelope<T>(
    envelope: &Envelope<T>,
    binding: ConnectionBinding,
) -> Result<(), RpcError> {
    if envelope.protocol_version != binding.protocol_version {
        return Err(RpcError::VersionMismatch {
            expected: binding.protocol_version,
            actual: envelope.protocol_version,
        });
    }
    if envelope.connection_generation != binding.connection_generation {
        return Err(RpcError::StaleGeneration {
            expected: binding.connection_generation,
            actual: envelope.connection_generation,
        });
    }
    validate_message_id(&envelope.message_id)?;
    if let Some(reply_to) = &envelope.reply_to {
        validate_message_id(reply_to)?;
        if reply_to == &envelope.message_id {
            return Err(RpcError::InvalidEnvelope(
                "message_id and reply_to must differ".to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_message_id(message_id: &str) -> Result<(), RpcError> {
    if message_id.is_empty() || message_id.len() > MAX_MESSAGE_ID_BYTES {
        return Err(RpcError::InvalidEnvelope(format!(
            "message id must be non-empty and at most {MAX_MESSAGE_ID_BYTES} bytes, got {} bytes",
            message_id.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> ConnectionBinding {
        ConnectionBinding {
            protocol_version: 1,
            connection_generation: 4,
        }
    }

    fn envelope(message_id: &str, reply_to: Option<&str>) -> Envelope<serde_json::Value> {
        Envelope {
            protocol_version: 1,
            connection_generation: 4,
            message_id: message_id.to_string(),
            reply_to: reply_to.map(str::to_string),
            body: serde_json::json!({"ok": true}),
        }
    }

    #[test]
    fn accepts_a_well_formed_envelope() {
        assert!(validate_envelope(&envelope("m-1", None), binding()).is_ok());
        assert!(validate_envelope(&envelope("m-2", Some("m-1")), binding()).is_ok());
    }

    #[test]
    fn rejects_protocol_version_drift() {
        let mut value = envelope("m-1", None);
        value.protocol_version = 2;
        assert!(matches!(
            validate_envelope(&value, binding()),
            Err(RpcError::VersionMismatch {
                expected: 1,
                actual: 2
            })
        ));
    }

    #[test]
    fn rejects_stale_generation() {
        let mut value = envelope("m-1", None);
        value.connection_generation = 3;
        assert!(matches!(
            validate_envelope(&value, binding()),
            Err(RpcError::StaleGeneration {
                expected: 4,
                actual: 3
            })
        ));
    }

    #[test]
    fn rejects_empty_and_oversize_message_id() {
        assert!(matches!(
            validate_envelope(&envelope("", None), binding()),
            Err(RpcError::InvalidEnvelope(_))
        ));
        let long = "x".repeat(MAX_MESSAGE_ID_BYTES + 1);
        assert!(matches!(
            validate_envelope(&envelope(&long, None), binding()),
            Err(RpcError::InvalidEnvelope(_))
        ));
    }

    #[test]
    fn rejects_reply_to_equal_to_message_id() {
        assert!(matches!(
            validate_envelope(&envelope("m-1", Some("m-1")), binding()),
            Err(RpcError::InvalidEnvelope(_))
        ));
    }
}
