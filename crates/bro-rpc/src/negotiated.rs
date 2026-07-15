use bro_protocol::{Envelope, WorkerMessage};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{FramedIo, RpcError};

pub const MAX_MESSAGE_ID_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionBinding {
    pub protocol_version: u16,
    pub connection_generation: u64,
}

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

    pub(crate) fn into_parts(self) -> (T, ConnectionBinding, usize) {
        let (io, max_frame_bytes) = self.framed.into_parts();
        (io, self.binding, max_frame_bytes)
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> NegotiatedIo<T> {
    pub async fn write_envelope(&mut self, envelope: &Envelope) -> Result<(), RpcError> {
        validate_envelope(envelope, self.binding)?;
        self.framed.write_json(envelope).await
    }

    pub async fn read_envelope(&mut self) -> Result<Envelope, RpcError> {
        let envelope = self.framed.read_json::<Envelope>().await?;
        validate_envelope(&envelope, self.binding)?;
        Ok(envelope)
    }
}

pub fn validate_envelope(envelope: &Envelope, binding: ConnectionBinding) -> Result<(), RpcError> {
    if envelope.protocol_version != binding.protocol_version {
        return Err(RpcError::ProtocolVersionMismatch {
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
            return Err(RpcError::CorrelationMismatch(
                "message_id and reply_to must differ".to_string(),
            ));
        }
    }

    match &envelope.body {
        WorkerMessage::CapabilityRequest(request) => {
            if envelope.reply_to.is_some() {
                return Err(RpcError::CorrelationMismatch(
                    "capability request cannot carry reply_to".to_string(),
                ));
            }
            if request.call_id != envelope.message_id {
                return Err(RpcError::CorrelationMismatch(
                    "capability request call_id must equal envelope message_id".to_string(),
                ));
            }
        }
        WorkerMessage::CapabilityResponse(response) => {
            let Some(reply_to) = envelope.reply_to.as_deref() else {
                return Err(RpcError::CorrelationMismatch(
                    "capability response requires reply_to".to_string(),
                ));
            };
            if response.call_id != reply_to {
                return Err(RpcError::CorrelationMismatch(
                    "capability response call_id must equal envelope reply_to".to_string(),
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_message_id(message_id: &str) -> Result<(), RpcError> {
    if message_id.is_empty() || message_id.len() > MAX_MESSAGE_ID_BYTES {
        return Err(RpcError::InvalidMessageId {
            limit: MAX_MESSAGE_ID_BYTES,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use bro_protocol::{CapabilityRequest, CapabilityResponse};
    use serde_json::json;

    use super::*;

    fn binding() -> ConnectionBinding {
        ConnectionBinding {
            protocol_version: 1,
            connection_generation: 4,
        }
    }

    fn envelope(body: WorkerMessage) -> Envelope {
        Envelope {
            protocol_version: 1,
            connection_generation: 4,
            message_id: "message-1".to_string(),
            reply_to: None,
            body,
        }
    }

    #[test]
    fn stale_protocol_and_generation_fail_closed() {
        let mut value = envelope(WorkerMessage::DrainAck);
        value.protocol_version = 2;
        assert!(matches!(
            validate_envelope(&value, binding()),
            Err(RpcError::ProtocolVersionMismatch {
                expected: 1,
                actual: 2
            })
        ));

        value.protocol_version = 1;
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
    fn capability_correlation_has_one_authoritative_identity() {
        let request = envelope(WorkerMessage::CapabilityRequest(CapabilityRequest {
            call_id: "different".to_string(),
            invocation_id: None,
            capability: "corpus".to_string(),
            operation: "search".to_string(),
            bounded_payload: json!({}),
            deadline_unix_ms: None,
        }));
        assert!(matches!(
            validate_envelope(&request, binding()),
            Err(RpcError::CorrelationMismatch(_))
        ));

        let mut response = envelope(WorkerMessage::CapabilityResponse(
            CapabilityResponse::success("message-1", json!({"ok": true})),
        ));
        response.message_id = "response-1".to_string();
        response.reply_to = Some("message-1".to_string());
        assert!(validate_envelope(&response, binding()).is_ok());

        if let WorkerMessage::CapabilityResponse(response) = &mut response.body {
            response.call_id = "wrong".to_string();
        }
        assert!(matches!(
            validate_envelope(&response, binding()),
            Err(RpcError::CorrelationMismatch(_))
        ));
    }
}
