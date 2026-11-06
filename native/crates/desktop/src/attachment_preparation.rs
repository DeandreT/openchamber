//! Captures complete composer content into one immutable outgoing prompt.

use openchamber_native_core::draft::{Draft, DraftKey};
use openchamber_native_core::draft_content::DraftFileContent;
use openchamber_native_core::ids::AttachmentId;
use openchamber_native_transport::prompt_parts::{
    Metadata, PromptFile, PromptParts, SyntheticContext,
};

use crate::attachment_catalog::StagingPhase;
use crate::workspace::Workspace;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureError {
    InvalidContent,
    PreparationIncomplete,
}

impl Workspace {
    /// Assigns admission identities once, after every file has a retained target.
    /// Retries use the captured `Prompt` rather than running this conversion again.
    pub(crate) fn capture_draft_parts(
        &mut self,
        owner: &DraftKey,
        draft: &Draft,
    ) -> Result<PromptParts, CaptureError> {
        if self.attachment_storage.has_pending(owner) {
            return Err(CaptureError::PreparationIncomplete);
        }
        crate::draft_storage::validate(&mut 0, draft).map_err(|_| CaptureError::InvalidContent)?;
        let mut parts = PromptParts {
            agents: draft.content.agents.clone(),
            skills: draft.content.skills.clone(),
            metadata: parse_metadata(draft.content.metadata_json.as_deref())?,
            ..PromptParts::default()
        };
        for file in &draft.content.files {
            let uri = match &file.content {
                DraftFileContent::Reference { uri } => uri.clone(),
                DraftFileContent::Prepared => {
                    if !self.prepared_attachment_ready(owner, &file.id) {
                        return Err(CaptureError::PreparationIncomplete);
                    }
                    let prepared = self
                        .attachments
                        .get(&file.id)
                        .ok_or(CaptureError::PreparationIncomplete)?;
                    let staging = prepared
                        .staging
                        .as_ref()
                        .filter(|staging| {
                            staging.runtime == *owner.runtime()
                                && crate::attachment_catalog::remote_path(&staging.directory)
                                    .as_deref()
                                    == Some(owner.directory())
                                && staging.phase == StagingPhase::Ready
                        })
                        .ok_or(CaptureError::PreparationIncomplete)?;
                    if file.byte_len != Some(prepared.blob.byte_len())
                        || file.name.as_deref() != Some(staging.filename.as_str())
                    {
                        return Err(CaptureError::InvalidContent);
                    }
                    staging.uri.clone()
                }
            };
            parts.files.push(PromptFile {
                uri,
                name: file.name.clone(),
            });
        }
        for context in &draft.content.context {
            let metadata = parse_metadata(context.metadata_json.as_deref())?;
            let instructions = context
                .instructions
                .as_ref()
                .filter(|text| !text.trim().is_empty());
            if context.text.trim().is_empty() && instructions.is_none() {
                return Err(CaptureError::InvalidContent);
            }
            // Queued context uses the same instruction-before-context order.
            if let Some(instructions) = instructions {
                parts.context.push(SyntheticContext {
                    id: self.message_ids.next().0,
                    text: instructions.clone(),
                    description: None,
                    metadata: None,
                });
            }
            if !context.text.trim().is_empty() {
                parts.context.push(SyntheticContext {
                    id: self.message_ids.next().0,
                    text: context.text.clone(),
                    description: context.description.clone(),
                    metadata,
                });
            } else if metadata.is_some() || context.description.is_some() {
                return Err(CaptureError::InvalidContent);
            }
        }
        Ok(parts)
    }
}

pub(crate) fn prepared_resources(draft: &Draft) -> Vec<AttachmentId> {
    draft
        .content
        .files
        .iter()
        .filter(|file| matches!(file.content, DraftFileContent::Prepared))
        .map(|file| file.id.clone())
        .collect()
}

fn parse_metadata(value: Option<&str>) -> Result<Option<Metadata>, CaptureError> {
    value
        .map(|value| {
            serde_json::from_str(value)
                .map(Metadata)
                .map_err(|_| CaptureError::InvalidContent)
        })
        .transpose()
}
