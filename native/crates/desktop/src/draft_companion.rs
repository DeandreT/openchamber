//! Lossless queue content appended to a native composer.
//!
//! The native Draft remains the owner of ordinary typing and native source-document
//! groups. Ordered companions own the fields Draft cannot represent: distinct
//! display/delivered text, per-file and skill mentions, independent metadata and
//! frozen admission identities. Only compact references enter QueueStorage.

use gpui_kit::{AsyncWindowContext, WeakEntity};
use openchamber_native_core::draft::{Draft, DraftKey};
#[cfg(test)]
use openchamber_native_transport::queue_manifest::QueuePayloadItem;
use serde::{Deserialize, Serialize};

use crate::queue_capture::QueueNativeSource;
use crate::queue_payload_io::{self, QueueCaptureFailure};
use crate::queue_storage::{DraftCompanionRef, QueueDestination, QueueTransferItem};
use crate::queue_take::{self, MaterializedQueueItem};
use crate::workspace::Workspace;

/// Complete local handoff record. Source text and rich content are stored once in
/// this spool object; the original queue payloads retain their own immutable seals.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompanionDocument {
    version: u32,
    pub(crate) native: QueueNativeSource,
    pub(crate) items: Vec<QueueTransferItem>,
}

impl CompanionDocument {
    pub(crate) fn new(native: QueueNativeSource, items: Vec<QueueTransferItem>) -> Self {
        Self {
            version: 1,
            native,
            items,
        }
    }

    fn validate(&self, owner: &DraftKey) -> Result<(), QueueCaptureFailure> {
        if self.version != 1
            || &self.native.owner != owner
            || self.items.is_empty()
            || self.items.len() > 20
        {
            return Err(QueueCaptureFailure::Invalid);
        }
        crate::draft_storage::validate(&mut 0, &self.native.draft)?;
        Ok(())
    }
}

/// Captured once by the ordinary send/enqueue consumer, before any asynchronous
/// preparation. Consumption uses these exact references, preserving newer appends.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompanionCapture {
    pub(crate) native: QueueNativeSource,
    pub(crate) references: Vec<DraftCompanionRef>,
}

impl Workspace {
    #[cfg(test)]
    pub(crate) fn composer_has_companion(&self, owner: &DraftKey) -> bool {
        !self.queue_storage.draft_companions(owner).is_empty()
    }

    pub(crate) fn capture_draft_companions(
        &self,
        owner: &DraftKey,
        draft: Draft,
    ) -> CompanionCapture {
        CompanionCapture {
            native: QueueNativeSource {
                owner: owner.clone(),
                draft,
            },
            references: self.queue_storage.draft_companions(owner),
        }
    }
}

/// Used by ordinary capture/send and explicit restart materialization. This reads
/// committed local bytes only. It never looks up, takes, acknowledges or sends.
/// The caller keeps its captured runtime alive until all reads finish.
pub(crate) async fn materialize(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    capture: &CompanionCapture,
) -> Result<Vec<MaterializedQueueItem>, QueueCaptureFailure> {
    let mut result = Vec::new();
    for reference in &capture.references {
        let operation = this.update_in(cx, |this, _, _| {
            let operation = this.queue_storage.operation(&reference.operation)?;
            let local = this.queue_storage.local_take(&reference.operation)?;
            if operation.owner().draft_key().ok().as_ref() != Some(&capture.native.owner)
                || !matches!(&local.destination, QueueDestination::Draft { companion } if companion == &reference.companion)
            { return None; }
            Some(operation.clone())
        }).ok().flatten().ok_or(QueueCaptureFailure::Unavailable)?;
        let bytes = queue_payload_io::read(this, cx, &reference.companion).await?;
        let expected = operation.owner().draft_key()?;
        let companion = cx
            .background_executor()
            .spawn(async move {
                let companion: CompanionDocument =
                    serde_json::from_slice(&bytes).map_err(|_| QueueCaptureFailure::Invalid)?;
                companion.validate(&expected)?;
                Ok::<_, QueueCaptureFailure>(companion)
            })
            .await?;
        let local = this
            .update_in(cx, |this, _, _| {
                this.queue_storage.local_take(&reference.operation).cloned()
            })
            .ok()
            .flatten()
            .ok_or(QueueCaptureFailure::Unavailable)?;
        // A well-formed standalone companion cannot substitute different payloads.
        if companion.items != local.transfer.items {
            return Err(QueueCaptureFailure::Invalid);
        }
        result.extend(
            queue_take::materialize_items(this, cx, operation.owner(), &companion.items).await?,
        );
    }
    Ok(result)
}

/// Display projection only. Sending consumes the typed items above and never this
/// joined string. Native typing precedes each queued Edit in append order.
#[cfg(test)]
pub(crate) fn display_text(native: &Draft, items: &[MaterializedQueueItem]) -> String {
    let mut text = native.text.clone();
    for item in items {
        let content = match item.document.item() {
            QueuePayloadItem::Manifest(item) => &item.content,
            QueuePayloadItem::Legacy(item) => &item.content,
        };
        if !text.is_empty() && !content.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(content);
    }
    text
}
