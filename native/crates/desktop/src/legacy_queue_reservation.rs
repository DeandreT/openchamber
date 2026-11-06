//! Exact reservation recovery for retained text-only journal attempts.
//!
//! New payload transfers use their own capture and reservation owner. This module
//! keeps an older attempt's original request and identities usable after restart.

use openchamber_native_core::ids::SessionId;
use openchamber_native_core::message_queue::{QueuedItemId, QueuedSendConfig};
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::message_queue::QueueItemInput;
use openchamber_native_transport::prompt::{Delivery, Prompt};

use crate::send_attempt::QueueReservation;

/// Acquires only the original prefix of an already retained legacy attempt.
pub(crate) async fn reserve_captured_prefix(
    client: &ServerClient,
    session: &SessionId,
    directory: &str,
    prompt: &Prompt,
    intent: &QueueReservation,
) -> Result<(), TransportError> {
    let model = prompt.model.as_ref().ok_or_else(|| {
        TransportError::Malformed("a queue reservation has no captured model".into())
    })?;
    let input = QueueItemInput {
        content: intent.composer.clone(),
        text: intent.composer.clone(),
        send_config: QueuedSendConfig {
            provider_id: model.provider_id.clone(),
            model_id: model.model_id.clone(),
            variant: model.variant.clone(),
            agent: prompt.agent.clone(),
        },
    };
    let ids: Vec<_> = intent
        .items
        .iter()
        .cloned()
        .map(QueuedItemId::new)
        .collect();
    let receipt = client
        .reserve_queued_prefix(
            session,
            directory,
            &prompt.message_id,
            &ids,
            &input,
            prompt.delivery == Some(Delivery::Steer),
        )
        .await?;
    let observed = receipt.prompt();
    if receipt.composer != intent.composer
        || observed.message_id != prompt.message_id
        || observed.text != prompt.text
        || observed.parts != prompt.parts
        || observed.model != prompt.model
        || observed.agent != prompt.agent
        || observed.delivery != prompt.delivery
    {
        return Err(TransportError::Malformed(
            "queue reservation changed the captured prompt".into(),
        ));
    }
    Ok(())
}
