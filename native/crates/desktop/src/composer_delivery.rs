//! Immutable rich-composer preparation shared by Send, Prefix and enqueue.
//!
//! This module performs captured-owner read-only preparation. It owns text
//! projection, source-local metadata and mention offsets. Callers own admission,
//! durable byte publication, file staging, revision consumption and dispatch.

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::{AsyncWindowContext, WeakEntity};
use openchamber_native_core::draft::DraftKey;
use openchamber_native_core::draft_content::DraftFileContent;
use openchamber_native_core::ids::MessageId;
use openchamber_native_core::message_queue::QueuedSendConfig;
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::prompt_parts::Metadata;
use openchamber_native_transport::queue_manifest::{
    QueueAgent, QueueLocalCapture, QueueLocalFile, QueueLocalItem, QueueMention, QueuePayloadItem,
    QueuePromptContext, QueuePromptParts, QueueSkill,
};

use crate::attachment_spool::{BlobDescriptor, BlobId};
use crate::composer_document::{ComposerCompanionInput, ComposerInputs, ComposerTextPiece};
use crate::composer_file_preparation::{
    self, CapturedFileMention, CapturedFileMentionSource, FileMentionPreparationError,
    PreparedFileMention,
};
use crate::composer_preparation::{
    CapturedComposerPreparation, ComposerEditPlan, PreparedComposerCapture,
};
use crate::mutation_journal::JournalError;
use crate::network::NetworkTask;
use crate::queue_capture::{QueueNativeSource, QueueOperationId, sha256};
use crate::queue_payload_io::{self, QueueCaptureFailure};
use crate::queue_storage::{
    QueueComposerPreparation, QueueFileBinding, QueueNativeFileDescriptor, QueuePreparationSource,
    QueuePreparationSourceId, QueueTransferItem,
};
use crate::queue_take::QueueServer;
use crate::workspace::Workspace;

pub(crate) struct ComposerDeliveryInput {
    pub(crate) inputs: ComposerInputs,
    pub(crate) configuration: QueuedSendConfig,
    pub(crate) native_context_ids: Vec<MessageId>,
    pub(crate) native_files: Vec<QueueNativeFileDescriptor>,
    pub(crate) skill_context_ids: Vec<MessageId>,
}

pub(crate) struct ComposerDeliverySource {
    pub(crate) capture: QueueLocalCapture,
    pub(crate) files: Vec<BlobDescriptor>,
    /// Retained provenance only. A local capture grants no Take acknowledgement.
    pub(crate) original: Option<QueueTransferItem>,
}

pub(crate) struct ComposerDelivery {
    pub(crate) sources: Vec<ComposerDeliverySource>,
    pub(crate) content: String,
    pub(crate) text: String,
    pub(crate) context_ids: Vec<MessageId>,
    pub(crate) configuration: QueuedSendConfig,
    /// Explicit derived instructions. The immutable original native draft and
    /// every sealed dependency ID remain unchanged. The caller must obtain ledger
    /// authority for these IDs before it can admit or freeze this delivery.
    pub(crate) derived_contexts: Vec<QueuePromptContext>,
}

impl ComposerDelivery {
    /// Enqueue and Prefix publish one composer item. Its anchors already address
    /// the final delivered text; merging parts must not shift them a second time.
    pub(crate) fn into_capture(
        self,
    ) -> Result<(QueueLocalCapture, Vec<BlobDescriptor>), QueueCaptureFailure> {
        validate_metadata(self.sources.iter().map(|source| &source.capture))?;
        let directory = self
            .sources
            .first()
            .ok_or(QueueCaptureFailure::Invalid)?
            .capture
            .directory()
            .to_owned();
        let mut item = QueueLocalItem {
            content: self.content,
            text: self.text,
            files: Vec::new(),
            send_config: self.configuration,
            prompt_parts: QueuePromptParts::default(),
        };
        let mut files = Vec::new();
        for source in self.sources {
            validate_files(&source.capture, &source.files)?;
            files.extend(source.files.iter().copied());
            let source = source.capture.item();
            item.files.extend(source.files.iter().cloned());
            item.prompt_parts
                .file_references
                .extend(source.prompt_parts.file_references.iter().cloned());
            item.prompt_parts
                .agents
                .extend(source.prompt_parts.agents.iter().cloned());
            item.prompt_parts
                .skills
                .extend(source.prompt_parts.skills.iter().cloned());
            item.prompt_parts
                .context
                .extend(source.prompt_parts.context.iter().cloned());
            if let Some(metadata) = &source.prompt_parts.metadata {
                let merged = item
                    .prompt_parts
                    .metadata
                    .get_or_insert_with(Metadata::default);
                for (key, value) in &metadata.0 {
                    merged.0.entry(key.clone()).or_insert_with(|| value.clone());
                }
            }
        }
        // Descriptors retain exact occurrence order, including repeated bindings.
        // Their local labels never come from model-facing file names or URIs.
        // The caller receives no server index or acknowledgement capability.
        let capture = QueueLocalCapture::new(&directory, item, self.context_ids, sha256)?;
        Ok((capture, files))
    }
}

#[derive(Debug)]
pub(crate) enum ComposerDeliveryError {
    Capture(QueueCaptureFailure),
    Preparation,
    File,
}
impl From<QueueCaptureFailure> for ComposerDeliveryError {
    fn from(error: QueueCaptureFailure) -> Self {
        Self::Capture(error)
    }
}
impl From<openchamber_native_transport::queue_manifest::QueueManifestError>
    for ComposerDeliveryError
{
    fn from(error: openchamber_native_transport::queue_manifest::QueueManifestError) -> Self {
        Self::Capture(error.into())
    }
}

impl From<FileMentionPreparationError> for ComposerDeliveryError {
    fn from(_: FileMentionPreparationError) -> Self {
        Self::File
    }
}

struct SourcePlan {
    source: ComposerDeliverySource,
    edit: Option<ComposerEditPlan>,
    skill_id: Option<MessageId>,
}

enum DeliveryPiece {
    Source(usize),
    Literal(String),
    EditedLiteral {
        ordinal: usize,
        edit: Box<ComposerEditPlan>,
        skill_id: MessageId,
    },
}

struct DeliveryPlan {
    owner: DraftKey,
    /// Exact captured server spelling, retained independently of the draft key.
    directory: String,
    preparation: CapturedComposerPreparation,
    configuration: QueuedSendConfig,
    sources: Vec<SourcePlan>,
    pieces: Vec<DeliveryPiece>,
}

struct SourceRequests {
    identity: QueuePreparationSourceId,
    capture: Arc<QueueLocalCapture>,
    requests: Vec<(QueueMention, String)>,
}

impl DeliveryPlan {
    fn requests(&self) -> Vec<SourceRequests> {
        let request = |identity, edit: &ComposerEditPlan| SourceRequests {
            identity,
            capture: Arc::clone(edit.source()),
            requests: edit
                .file_requests()
                .iter()
                .map(|request| {
                    (
                        request.mention().clone(),
                        request.resolved_path().to_owned(),
                    )
                })
                .collect(),
        };
        let mut requests = self
            .sources
            .iter()
            .enumerate()
            .filter_map(|(index, source)| {
                source
                    .edit
                    .as_ref()
                    .map(|edit| request(QueuePreparationSourceId::Source(index), edit))
            })
            .collect::<Vec<_>>();
        requests.extend(self.pieces.iter().filter_map(|piece| match piece {
            DeliveryPiece::EditedLiteral { ordinal, edit, .. } => {
                Some(request(QueuePreparationSourceId::Literal(*ordinal), edit))
            }
            _ => None,
        }));
        requests
    }
}

async fn plan_delivery(
    owner: &DraftKey,
    client: &ServerClient,
    directory: &str,
    input: ComposerDeliveryInput,
    companion_context_ids: Vec<Vec<MessageId>>,
) -> Result<DeliveryPlan, ComposerDeliveryError> {
    let ComposerDeliveryInput {
        inputs,
        configuration,
        native_context_ids,
        native_files,
        skill_context_ids,
    } = input;
    let ComposerInputs {
        native,
        companions,
        text,
        exclusions,
    } = inputs;
    if &native.owner != owner
        || companion_context_ids.len() != companions.len()
        || exclusions
            .iter()
            .any(|excluded| excluded.source > companions.len())
        || exclusions.iter().enumerate().any(|(index, excluded)| {
            exclusions[..index]
                .iter()
                .any(|old| old.source == excluded.source)
        })
    {
        return Err(QueueCaptureFailure::Invalid.into());
    }
    let preparation = CapturedComposerPreparation::new(owner.clone(), client, directory)
        .map_err(|_| ComposerDeliveryError::Preparation)?;
    let native = native_source(
        &native,
        &native_files,
        native_context_ids,
        configuration.clone(),
        directory,
    )?;
    let mut sources = vec![native];
    let mut process = vec![true];
    for (companion, context_ids) in companions.into_iter().zip(companion_context_ids) {
        match companion {
            ComposerCompanionInput::Original(item) => {
                crate::queue_take::validate_file_bindings(&item)?;
                let files = match item.document.item() {
                    QueuePayloadItem::Manifest(manifest) => manifest.attachment_manifest.iter().map(|file| {
                        item.retained.files.iter().find(|binding| matches!(binding, QueueFileBinding::Manifest { blob, .. } if blob == &file.attachment.blob_id))
                            .ok_or(QueueCaptureFailure::Invalid)?.descriptor().map_err(QueueCaptureFailure::from)
                    }).collect::<Result<Vec<_>, _>>()?,
                    QueuePayloadItem::Legacy(legacy) => (0..legacy.attachments.len()).map(|ordinal| {
                        item.retained.files.iter().find(|binding| matches!(binding, QueueFileBinding::Legacy { ordinal: saved, .. } if *saved == ordinal))
                            .ok_or(QueueCaptureFailure::Invalid)?.descriptor().map_err(QueueCaptureFailure::from)
                    }).collect::<Result<Vec<_>, _>>()?,
                };
                let capture = QueueLocalCapture::from_document(
                    item.document,
                    item.indexed.index().context_ids.clone(),
                    sha256,
                )?
                .rebind_context_ids(context_ids, sha256)?;
                sources.push(ComposerDeliverySource {
                    capture,
                    files,
                    original: Some(item.retained),
                });
                process.push(false);
            }
            ComposerCompanionInput::Edited {
                capture,
                files,
                reprocess_text,
            } => {
                let capture = capture.rebind_context_ids(context_ids, sha256)?;
                sources.push(ComposerDeliverySource {
                    capture,
                    files,
                    original: None,
                });
                process.push(reprocess_text);
            }
        }
    }
    if sources.len() > 21
        || sources
            .iter()
            .any(|source| source.capture.directory() != directory)
    {
        return Err(QueueCaptureFailure::Invalid.into());
    }
    validate_metadata(sources.iter().map(|source| &source.capture))?;
    let mut skill_ids = skill_context_ids.into_iter();
    let mut prepared = Vec::with_capacity(sources.len());
    for (source_index, (source, process)) in sources.into_iter().zip(process).enumerate() {
        validate_files(&source.capture, &source.files)?;
        let (edit, skill_id) = if process {
            let skill_id = skill_ids.next().ok_or(QueueCaptureFailure::Invalid)?;
            let excluded = exclusions
                .iter()
                .find(|excluded| excluded.source == source_index);
            let shared = Arc::new(QueueLocalCapture::from_bytes(
                source.capture.bytes().to_vec(),
                directory,
                sha256,
            )?);
            let edit = preparation
                .plan_edit_with_exclusions(shared, Some(skill_id.clone()), source_index, excluded)
                .await
                .map_err(|_| ComposerDeliveryError::Preparation)?;
            (Some(edit), Some(skill_id))
        } else {
            (None, None)
        };
        prepared.push(SourcePlan {
            source,
            edit,
            skill_id,
        });
    }
    let mut projected = Vec::with_capacity(text.len());
    for (ordinal, piece) in text.into_iter().enumerate() {
        match piece {
            ComposerTextPiece::Source(index) => projected.push(DeliveryPiece::Source(index)),
            ComposerTextPiece::Literal {
                text,
                edited: false,
            } => projected.push(DeliveryPiece::Literal(text)),
            ComposerTextPiece::Literal { text, edited: true } => {
                let original = QueueLocalCapture::new(
                    directory,
                    QueueLocalItem {
                        content: text.clone(),
                        text,
                        files: Vec::new(),
                        send_config: configuration.clone(),
                        prompt_parts: QueuePromptParts::default(),
                    },
                    Vec::new(),
                    sha256,
                )?;
                let skill_id = skill_ids.next().ok_or(QueueCaptureFailure::Invalid)?;
                let edit = preparation
                    .plan_edit(Arc::new(original), Some(skill_id.clone()))
                    .await
                    .map_err(|_| ComposerDeliveryError::Preparation)?;
                projected.push(DeliveryPiece::EditedLiteral {
                    ordinal,
                    edit: Box::new(edit),
                    skill_id,
                });
            }
        }
    }
    Ok(DeliveryPlan {
        owner: owner.clone(),
        directory: directory.to_owned(),
        preparation,
        configuration,
        sources: prepared,
        pieces: projected,
    })
}

fn verify_preparation(
    plan: &QueueComposerPreparation,
    sources: &[SourceRequests],
    expected: &QueueComposerPreparation,
) -> Result<(), JournalError> {
    if plan.owner()? != expected.owner()?
        || plan.source_sha256 != expected.source_sha256
        || plan.generation != expected.generation
        || plan.revision != expected.revision
        || plan.sources != expected.sources
        || plan.derived_context_ids != expected.derived_context_ids
    {
        return Err(JournalError::Invalid(
            "composer preparation differs from its captured input",
        ));
    }
    for file in &plan.files {
        if !sources.iter().any(|source| {
            source.identity == file.source
                && source.requests.iter().any(|(mention, path)| {
                    mention.start == file.mention.start
                        && mention.end == file.mention.end
                        && mention.text == file.mention.text
                        && path == &file.server_path
                })
        }) {
            return Err(JournalError::Invalid(
                "derived file has no captured typed request",
            ));
        }
    }
    Ok(())
}

fn admit_preparation(
    workspace: &mut Workspace,
    id: &QueueOperationId,
    plan: QueueComposerPreparation,
    sources: &[SourceRequests],
) -> Result<(), QueueCaptureFailure> {
    if workspace.close_in_progress() || !workspace.persistence_ready() {
        return Err(QueueCaptureFailure::Unavailable);
    }
    let mut queue = std::mem::take(&mut workspace.queue_storage);
    let mut holds = std::mem::take(&mut workspace.attachment_preparations);
    let attachments = workspace.attachments.clone();
    let expected = plan.clone();
    let result = queue.plan_composer_preparation(
        id,
        plan,
        &attachments,
        &mut holds,
        |operation, plan| {
            if operation.source_sha256() != Some(&expected.source_sha256) {
                return Err(JournalError::Invalid(
                    "composer preparation lost its original source seal",
                ));
            }
            verify_preparation(plan, sources, &expected)
        },
        |candidate, preparations| {
            workspace.attachment_preparations = preparations.to_vec();
            workspace.validate_queue_candidate(candidate)
        },
    );
    workspace.queue_storage = queue;
    workspace.attachment_preparations = holds;
    result.map_err(Into::into)
}

/// The original operation and source must already be durable. Fresh raw reads
/// occur only after the exact input sources acquire preparation ownership.
/// Every mention's complete output bundle is admitted before its first put.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare_operation(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    generation: u64,
    revision: u64,
    explicit_retry: bool,
    input: ComposerDeliveryInput,
) -> Result<ComposerDelivery, ComposerDeliveryError> {
    let owner = input.inputs.native.owner.clone();
    let companion_context_ids = this
        .update_in(cx, |workspace, _, _| {
            let operation = workspace
                .queue_storage
                .operation(id)
                .ok_or(QueueCaptureFailure::Unavailable)?;
            if workspace.close_in_progress()
                || !workspace.persistence_ready()
                || operation.owner().draft_key()? != owner
                || operation.owner().runtime() != server.runtime.as_str()
                || operation.owner().directory() != server.scope.directory()
                || operation.owner().session() != server.scope.session().as_str()
                || operation
                    .composer_preparation()
                    .is_some_and(|preparation| preparation.inputs_object().is_some())
            {
                return Err(QueueCaptureFailure::Unavailable);
            }
            (1..=input.inputs.companions.len())
                .map(|source_index| {
                    workspace
                        .queue_storage
                        .companion_context_ids(id, source_index)
                        .map(|ids| ids.into_iter().map(MessageId::from).collect::<Vec<_>>())
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(QueueCaptureFailure::from)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    let captured_owner = owner.clone();
    let client = server.client.clone();
    let directory = server.scope.directory().to_owned();
    let plan = NetworkTask::spawn_on(&server.executor, async move {
        plan_delivery(
            &captured_owner,
            &client,
            &directory,
            input,
            companion_context_ids,
        )
        .await
    })
    .await
    .ok_or(QueueCaptureFailure::Unavailable)??;
    let sources = plan.requests();
    let sealed_sources = sources
        .iter()
        .map(|source| QueuePreparationSource {
            identity: source.identity,
            byte_len: source.capture.bytes().len() as u64,
            sha256: source.capture.sha256(),
        })
        .collect::<Vec<_>>();
    let mut preparation = this
        .update_in(cx, |workspace, _, _| {
            let operation = workspace
                .queue_storage
                .operation(id)
                .ok_or(QueueCaptureFailure::Unavailable)?;
            if operation.owner().draft_key()? != owner
                || operation.owner().runtime() != server.runtime.as_str()
                || operation.owner().directory() != server.scope.directory()
                || operation.owner().session() != server.scope.session().as_str()
            {
                return Err(QueueCaptureFailure::Invalid);
            }
            let seal = *operation
                .source_sha256()
                .ok_or(QueueCaptureFailure::Invalid)?;
            let preparation = if let Some(previous) = operation.composer_preparation() {
                if previous.owner()? != owner
                    || previous.source_sha256 != seal
                    || previous.generation != generation
                    || previous.revision != revision
                    || previous.inputs_object().is_some()
                    || !previous.derived_context_ids.is_empty()
                    || (!explicit_retry
                        && (!previous.sources.is_empty() || !previous.files.is_empty()))
                    || (!previous.sources.is_empty() && previous.sources != sealed_sources)
                {
                    return Err(QueueCaptureFailure::Unavailable);
                }
                let mut preparation = previous.clone();
                if preparation.sources.is_empty() {
                    preparation.sources = sealed_sources;
                }
                preparation
            } else {
                QueueComposerPreparation::new(
                    &owner,
                    seal,
                    generation,
                    revision,
                    sealed_sources,
                    Vec::new(),
                    Vec::new(),
                )
            };
            admit_preparation(workspace, id, preparation.clone(), &sources)?;
            Ok::<_, QueueCaptureFailure>(preparation)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await?;
    let original_names = plan
        .sources
        .iter()
        .flat_map(|source| source.source.capture.item().files.iter())
        .map(|file| {
            file.name
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(&file.name)
                .to_owned()
        })
        .collect::<Vec<_>>();
    let mut outputs = HashMap::new();
    let mut first_failure = None;
    for source in &sources {
        if source.requests.is_empty() {
            continue;
        }
        let captured_plan = preparation.clone();
        let identity = source.identity;
        let captured_source = Arc::clone(&source.capture);
        let shared = cx
            .background_executor()
            .spawn(async move {
                CapturedFileMentionSource::new(&captured_plan, identity, captured_source)
                    .map(Arc::new)
            })
            .await?;
        let mut bundles = Vec::with_capacity(source.requests.len());
        for (mention, path) in &source.requests {
            let request = CapturedFileMention::new(Arc::clone(&shared), mention.clone(), path)?;
            let same_bundle = |file: &crate::queue_storage::QueueDerivedFile| {
                file.source == source.identity
                    && file.mention.start == mention.start
                    && file.mention.end == mention.end
                    && file.mention.text == mention.text
            };
            let existing = preparation.files.iter().any(&same_bundle);
            let mut reserved = original_names.clone();
            reserved.extend(
                preparation
                    .files
                    .iter()
                    .filter(|file| !same_bundle(file))
                    .map(|file| file.name.clone()),
            );
            if existing {
                if !explicit_retry {
                    return Err(QueueCaptureFailure::Invalid.into());
                }
                match composer_file_preparation::retry(this, cx, id, server, request, reserved)
                    .await
                {
                    Ok(bundle) => bundles.push(bundle),
                    Err(error) => {
                        first_failure.get_or_insert(error);
                    }
                }
                preparation = this
                    .update_in(cx, |workspace, _, _| {
                        workspace
                            .queue_storage
                            .operation(id)
                            .and_then(|operation| operation.composer_preparation())
                            .cloned()
                    })
                    .ok()
                    .flatten()
                    .ok_or(QueueCaptureFailure::Unavailable)?;
                continue;
            }
            let normalized = match composer_file_preparation::read_and_normalize(
                this, cx, id, server, request, reserved,
            )
            .await
            {
                Ok(normalized) => normalized,
                Err(error) => {
                    first_failure.get_or_insert(error);
                    continue;
                }
            };
            let count = normalized.output_count();
            let document = normalized.document_group_required();
            let (labels, group) = this
                .update_in(cx, |workspace, _, _| {
                    let group = document
                        .then(|| workspace.new_storage_id())
                        .transpose()
                        .map_err(|_| QueueCaptureFailure::Invalid)?;
                    let mut labels = Vec::with_capacity(count);
                    for _ in 0..count {
                        let label = workspace
                            .new_storage_id()
                            .map_err(|_| QueueCaptureFailure::Invalid)?;
                        let blob = BlobId::parse(label.as_str())
                            .map_err(|_| QueueCaptureFailure::Invalid)?;
                        if labels.contains(&blob) || group.as_ref() == Some(&label) {
                            return Err(QueueCaptureFailure::Invalid);
                        }
                        labels.push(blob);
                    }
                    Ok::<_, QueueCaptureFailure>((labels, group.map(|group| group.to_string())))
                })
                .map_err(|_| QueueCaptureFailure::Unavailable)??;
            preparation.files.extend(normalized.plan(&labels, group)?);
            this.update_in(cx, |workspace, _, _| {
                admit_preparation(workspace, id, preparation.clone(), &sources)
            })
            .map_err(|_| QueueCaptureFailure::Unavailable)??;
            let bundle = composer_file_preparation::prepare(this, cx, id, server, normalized).await;
            // Publication advances status. Every append preserves those exact
            // statuses along with the IDs and descriptors already made durable.
            preparation = this
                .update_in(cx, |workspace, _, _| {
                    workspace
                        .queue_storage
                        .operation(id)
                        .and_then(|operation| operation.composer_preparation())
                        .cloned()
                })
                .ok()
                .flatten()
                .ok_or(QueueCaptureFailure::Unavailable)?;
            match bundle {
                Ok(bundle) => bundles.push(bundle),
                Err(error) => {
                    first_failure.get_or_insert(error);
                }
            }
        }
        outputs.insert(source.identity, bundles);
    }
    if let Some(error) = first_failure {
        return Err(error.into());
    }
    let delivery = NetworkTask::spawn_on(&server.executor, async move {
        finish_delivery(plan, outputs).await
    })
    .await
    .ok_or(QueueCaptureFailure::Unavailable)??;
    preparation.derived_context_ids = delivery
        .derived_contexts
        .iter()
        .map(|context| {
            context
                .id
                .as_ref()
                .map(ToString::to_string)
                .ok_or(QueueCaptureFailure::Invalid)
        })
        .collect::<Result<_, _>>()?;
    this.update_in(cx, |workspace, _, _| {
        admit_preparation(workspace, id, preparation, &sources)
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await?;
    Ok(delivery)
}

fn take_outputs(
    outputs: &mut HashMap<QueuePreparationSourceId, Vec<PreparedFileMention>>,
    identity: QueuePreparationSourceId,
    owner: &DraftKey,
) -> Result<(Vec<Vec<QueueLocalFile>>, Vec<BlobDescriptor>), QueueCaptureFailure> {
    let mut files = Vec::new();
    let mut descriptors = Vec::new();
    for bundle in outputs.remove(&identity).unwrap_or_default() {
        if &bundle.owner != owner {
            return Err(QueueCaptureFailure::Invalid);
        }
        let mut items = Vec::with_capacity(bundle.outputs.len());
        for output in bundle.outputs {
            if output.provenance.descriptor()? != output.blob
                || output.provenance.name != output.file.name
                || output.provenance.mime_type != output.file.mime_type
            {
                return Err(QueueCaptureFailure::Invalid);
            }
            items.push(output.file);
            descriptors.push(output.blob);
        }
        files.push(items);
    }
    Ok((files, descriptors))
}

async fn finish_delivery(
    plan: DeliveryPlan,
    mut outputs: HashMap<QueuePreparationSourceId, Vec<PreparedFileMention>>,
) -> Result<ComposerDelivery, ComposerDeliveryError> {
    let DeliveryPlan {
        owner,
        directory,
        preparation,
        configuration,
        sources,
        pieces,
    } = plan;
    let mut content = String::new();
    for piece in &pieces {
        let text = match piece {
            DeliveryPiece::Source(index) => {
                &sources
                    .get(*index)
                    .ok_or(QueueCaptureFailure::Invalid)?
                    .source
                    .capture
                    .item()
                    .content
            }
            DeliveryPiece::Literal(text) => text,
            DeliveryPiece::EditedLiteral { edit, .. } => &edit.source().item().content,
        };
        content.push_str(text);
    }
    let mut prepared = Vec::with_capacity(sources.len());
    let mut derived = Vec::new();
    for (
        index,
        SourcePlan {
            mut source,
            edit,
            skill_id,
        },
    ) in sources.into_iter().enumerate()
    {
        if let Some(edit) = edit {
            let (files, descriptors) = take_outputs(
                &mut outputs,
                QueuePreparationSourceId::Source(index),
                &owner,
            )?;
            let result = preparation
                .finish_edit(edit, files)
                .await
                .map_err(|_| ComposerDeliveryError::Preparation)?;
            let (capture, extra) = accept_prepared(
                &owner,
                &source.capture,
                result,
                &skill_id.ok_or(QueueCaptureFailure::Invalid)?,
            )?;
            source.capture = capture;
            source.files.extend(descriptors);
            derived.extend(extra);
        }
        validate_files(&source.capture, &source.files)?;
        prepared.push(source);
    }
    let mut projected = Vec::with_capacity(pieces.len());
    let mut literals = Vec::new();
    let mut offset = 0u64;
    for piece in pieces {
        let piece = match piece {
            DeliveryPiece::Source(index) => ProjectionPiece::Source(index),
            DeliveryPiece::Literal(text) => ProjectionPiece::Literal(text),
            DeliveryPiece::EditedLiteral {
                ordinal,
                edit,
                skill_id,
            } => {
                let original = Arc::clone(edit.source());
                let (files, descriptors) = take_outputs(
                    &mut outputs,
                    QueuePreparationSourceId::Literal(ordinal),
                    &owner,
                )?;
                let result = preparation
                    .finish_edit(*edit, files)
                    .await
                    .map_err(|_| ComposerDeliveryError::Preparation)?;
                let (capture, extra) = accept_prepared(&owner, &original, result, &skill_id)?;
                if capture.item().prompt_parts.metadata.is_some() {
                    return Err(QueueCaptureFailure::Invalid.into());
                }
                derived.extend(extra);
                let text = capture.item().text.clone();
                literals.push((capture, descriptors, offset));
                ProjectionPiece::Literal(text)
            }
        };
        let text = match &piece {
            ProjectionPiece::Source(index) => {
                &prepared
                    .get(*index)
                    .ok_or(QueueCaptureFailure::Invalid)?
                    .capture
                    .item()
                    .text
            }
            ProjectionPiece::Literal(text) => text,
        };
        offset = offset
            .checked_add(text.encode_utf16().count() as u64)
            .ok_or(QueueCaptureFailure::Invalid)?;
        projected.push(piece);
    }
    if !outputs.is_empty() {
        return Err(QueueCaptureFailure::Invalid.into());
    }
    let mut native = prepared[0].capture.item().clone();
    let mut ids = prepared[0].capture.context_ids().to_vec();
    for context in &derived {
        ids.push(context.id.clone().ok_or(QueueCaptureFailure::Invalid)?);
        native.prompt_parts.context.push(context.clone());
    }
    prepared[0].capture = QueueLocalCapture::new(&directory, native, ids, sha256)?;
    let captures = project(prepared.iter().map(|source| &source.capture), &projected)?;
    for (source, capture) in prepared.iter_mut().zip(captures) {
        source.capture = capture;
    }
    // Literal anchors are already relative to their own expanded text. Add their
    // exact projected offset after source anchors have been relocated.
    if !literals.is_empty() {
        let mut native = prepared[0].capture.item().clone();
        for (literal, descriptors, offset) in literals {
            let mut item = literal.item().clone();
            for file in &mut item.files {
                relocate(&mut file.mention, &literal.item().text, Some(offset));
            }
            for file in &mut item.prompt_parts.file_references {
                relocate(&mut file.mention, &literal.item().text, Some(offset));
            }
            for agent in &mut item.prompt_parts.agents {
                relocate(&mut agent.mention, &literal.item().text, Some(offset));
            }
            for skill in &mut item.prompt_parts.skills {
                relocate(&mut skill.mention, &literal.item().text, Some(offset));
            }
            native.files.extend(item.files);
            native.prompt_parts.agents.extend(item.prompt_parts.agents);
            native.prompt_parts.skills.extend(item.prompt_parts.skills);
            native
                .prompt_parts
                .file_references
                .extend(item.prompt_parts.file_references);
            prepared[0].files.extend(descriptors);
        }
        prepared[0].capture = QueueLocalCapture::new(
            &directory,
            native,
            prepared[0].capture.context_ids().to_vec(),
            sha256,
        )?;
    }
    validate_metadata(prepared.iter().map(|source| &source.capture))?;
    let text = prepared[0].capture.item().text.clone();
    let context_ids = prepared
        .iter()
        .flat_map(|source| source.capture.context_ids().iter().cloned())
        .collect();
    Ok(ComposerDelivery {
        sources: prepared,
        content,
        text,
        context_ids,
        configuration,
        derived_contexts: derived,
    })
}

fn accept_prepared(
    owner: &DraftKey,
    original: &QueueLocalCapture,
    result: PreparedComposerCapture,
    skill_id: &MessageId,
) -> Result<(QueueLocalCapture, Vec<QueuePromptContext>), QueueCaptureFailure> {
    if result.owner() != owner {
        return Err(QueueCaptureFailure::Invalid);
    }
    accept_capture(original, result.into_capture(), skill_id)
}

fn without_anchors(item: &mut QueueLocalItem) {
    for file in &mut item.files {
        file.mention = None;
    }
    for file in &mut item.prompt_parts.file_references {
        file.mention = None;
    }
    for agent in &mut item.prompt_parts.agents {
        agent.mention = None;
    }
    for skill in &mut item.prompt_parts.skills {
        skill.mention = None;
    }
}

fn accept_capture(
    original: &QueueLocalCapture,
    result: QueueLocalCapture,
    skill_id: &MessageId,
) -> Result<(QueueLocalCapture, Vec<QueuePromptContext>), QueueCaptureFailure> {
    let mut old = original.item().clone();
    let mut semantic = result.item().clone();
    without_anchors(&mut old);
    without_anchors(&mut semantic);
    if result.directory() != original.directory()
        || semantic.content != old.content
        || !semantic.files.starts_with(&old.files)
        || semantic.send_config != old.send_config
        || !result.context_ids().starts_with(original.context_ids())
        || result.context_ids().len() > original.context_ids().len() + 1
        || !semantic
            .prompt_parts
            .context
            .starts_with(&old.prompt_parts.context)
        || !semantic
            .prompt_parts
            .file_references
            .starts_with(&old.prompt_parts.file_references)
        || !old
            .prompt_parts
            .agents
            .iter()
            .all(|agent| semantic.prompt_parts.agents.contains(agent))
        || !old
            .prompt_parts
            .skills
            .iter()
            .all(|skill| semantic.prompt_parts.skills.contains(skill))
        || !json_equal(
            &serde_json::to_value(&semantic.prompt_parts.metadata)
                .map_err(|_| QueueCaptureFailure::Invalid)?,
            &serde_json::to_value(&old.prompt_parts.metadata)
                .map_err(|_| QueueCaptureFailure::Invalid)?,
        )
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    let mut item = result.item().clone();
    let extra = item
        .prompt_parts
        .context
        .split_off(original.context_ids().len());
    if extra.len() > 1
        || extra
            .first()
            .is_some_and(|context| context.id.as_ref() != Some(skill_id))
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    Ok((
        QueueLocalCapture::new(
            result.directory(),
            item,
            original.context_ids().to_vec(),
            sha256,
        )?,
        extra,
    ))
}

fn validate_files(
    capture: &QueueLocalCapture,
    files: &[BlobDescriptor],
) -> Result<(), QueueCaptureFailure> {
    if capture.item().files.len() != files.len()
        || capture
            .item()
            .files
            .iter()
            .zip(files)
            .any(|(file, held)| file.byte_len != held.byte_len() || &file.sha256 != held.sha256())
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    Ok(())
}

fn metadata(value: Option<&str>) -> Result<Option<Metadata>, QueueCaptureFailure> {
    value
        .map(|value| {
            serde_json::from_str(value)
                .map(Metadata)
                .map_err(|_| QueueCaptureFailure::Invalid)
        })
        .transpose()
}

fn native_source(
    source: &QueueNativeSource,
    descriptors: &[QueueNativeFileDescriptor],
    ids: Vec<MessageId>,
    configuration: QueuedSendConfig,
    directory: &str,
) -> Result<ComposerDeliverySource, QueueCaptureFailure> {
    let mut files = Vec::new();
    let mut held = Vec::new();
    for file in &source.draft.content.files {
        let saved = descriptors
            .iter()
            .find(|saved| saved.id == file.id.as_str())
            .ok_or(QueueCaptureFailure::Invalid)?;
        if !matches!(file.content, DraftFileContent::Prepared)
            || file.byte_len != Some(saved.byte_len)
        {
            return Err(QueueCaptureFailure::Invalid);
        }
        held.push(BlobDescriptor::new(
            BlobId::parse(&saved.id).map_err(|_| QueueCaptureFailure::Invalid)?,
            saved.byte_len,
            saved.sha256,
        ));
        files.push(QueueLocalFile {
            name: file.name.clone().ok_or(QueueCaptureFailure::Invalid)?,
            mime_type: file
                .media_type
                .clone()
                .ok_or(QueueCaptureFailure::Invalid)?,
            byte_len: saved.byte_len,
            sha256: saved.sha256,
            description: None,
            mention: None,
        });
    }
    let mut context = Vec::new();
    for entry in &source.draft.content.context {
        if let Some(instructions) = &entry.instructions {
            context.push(QueuePromptContext {
                id: None,
                text: instructions.clone(),
                description: None,
                metadata: None,
            });
        }
        if !entry.text.trim().is_empty() {
            context.push(QueuePromptContext {
                id: None,
                text: entry.text.clone(),
                description: entry.description.clone(),
                metadata: metadata(entry.metadata_json.as_deref())?,
            });
        } else if entry.description.is_some() || entry.metadata_json.is_some() {
            return Err(QueueCaptureFailure::Invalid);
        }
    }
    let mention = |value: &openchamber_native_core::draft_content::Mention| QueueMention {
        start: u64::from(value.start),
        end: u64::from(value.end),
        text: value.text.clone(),
    };
    let capture = QueueLocalCapture::new(
        directory,
        QueueLocalItem {
            content: source.draft.text.clone(),
            text: source.draft.text.clone(),
            files,
            send_config: configuration,
            prompt_parts: QueuePromptParts {
                file_references: Vec::new(),
                agents: source
                    .draft
                    .content
                    .agents
                    .iter()
                    .map(|agent| QueueAgent {
                        name: agent.name.clone(),
                        mention: agent.mention.as_ref().map(mention),
                    })
                    .collect(),
                skills: source
                    .draft
                    .content
                    .skills
                    .iter()
                    .map(|skill| QueueSkill {
                        id: skill.id.clone(),
                        mention: None,
                    })
                    .collect(),
                metadata: metadata(source.draft.content.metadata_json.as_deref())?,
                context,
            },
        },
        ids,
        sha256,
    )?;
    Ok(ComposerDeliverySource {
        capture,
        files: held,
        original: None,
    })
}

pub(crate) fn json_equal(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| json_equal(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, value)| {
                    right.get(key).is_some_and(|right| json_equal(value, right))
                })
        }
        _ => left == right,
    }
}

pub(crate) fn validate_metadata<'a>(
    captures: impl Iterator<Item = &'a QueueLocalCapture>,
) -> Result<(), QueueCaptureFailure> {
    let mut keys = std::collections::HashMap::<&str, &serde_json::Value>::new();
    for capture in captures {
        if let Some(metadata) = &capture.item().prompt_parts.metadata {
            for (key, value) in &metadata.0 {
                if keys
                    .insert(key, value)
                    .is_some_and(|old| !json_equal(old, value))
                {
                    return Err(QueueCaptureFailure::Invalid);
                }
            }
        }
    }
    Ok(())
}

enum ProjectionPiece {
    Source(usize),
    Literal(String),
}

fn relocate(mention: &mut Option<QueueMention>, text: &str, offset: Option<u64>) {
    *mention = mention.as_ref().and_then(|mention| {
        let offset = offset?;
        let start = usize::try_from(mention.start).ok()?;
        let length = usize::try_from(mention.end.checked_sub(mention.start)?).ok()?;
        let value: Vec<_> = text.encode_utf16().skip(start).take(length).collect();
        if value.len() != length
            || String::from_utf16(&value).ok().as_deref() != Some(mention.text.as_str())
        {
            return None;
        }
        Some(QueueMention {
            start: mention.start.checked_add(offset)?,
            end: mention.end.checked_add(offset)?,
            text: mention.text.clone(),
        })
    });
}

fn project<'a>(
    captures: impl Iterator<Item = &'a QueueLocalCapture>,
    pieces: &[ProjectionPiece],
) -> Result<Vec<QueueLocalCapture>, QueueCaptureFailure> {
    let captures: Vec<_> = captures.collect();
    let mut exact = String::new();
    let mut offsets = vec![None; captures.len()];
    let mut counts = vec![0usize; captures.len()];
    let mut utf16 = 0u64;
    for piece in pieces {
        let text = match piece {
            ProjectionPiece::Source(index) => {
                let capture = captures.get(*index).ok_or(QueueCaptureFailure::Invalid)?;
                counts[*index] += 1;
                offsets[*index] = (counts[*index] == 1).then_some(utf16);
                &capture.item().text
            }
            ProjectionPiece::Literal(text) => text,
        };
        utf16 = utf16
            .checked_add(text.encode_utf16().count() as u64)
            .ok_or(QueueCaptureFailure::Invalid)?;
        exact.push_str(text);
    }
    if captures.len() > 1 && !exact.is_empty() && exact.trim().is_empty() {
        return Err(QueueCaptureFailure::Invalid);
    }
    captures
        .iter()
        .enumerate()
        .map(|(index, capture)| {
            let mut item = capture.item().clone();
            item.text = if index == 0 {
                exact.clone()
            } else {
                String::new()
            };
            let offset = (counts[index] == 1).then_some(offsets[index]).flatten();
            for file in &mut item.files {
                relocate(&mut file.mention, &capture.item().text, offset);
            }
            for file in &mut item.prompt_parts.file_references {
                relocate(&mut file.mention, &capture.item().text, offset);
            }
            for agent in &mut item.prompt_parts.agents {
                relocate(&mut agent.mention, &capture.item().text, offset);
            }
            for skill in &mut item.prompt_parts.skills {
                relocate(&mut skill.mention, &capture.item().text, offset);
            }
            QueueLocalCapture::new(
                capture.directory(),
                item,
                capture.context_ids().to_vec(),
                sha256,
            )
            .map_err(Into::into)
        })
        .collect()
}

#[cfg(test)]
#[path = "composer_delivery_tests.rs"]
mod tests;
