// Server-owned message queue: messages the user queued while a session was
// busy, delivered by the web server the moment the session goes idle. The
// queue lives here, not in the browser, so closing the tab, locking the phone,
// or losing the connection no longer strands what was queued. Structural
// template: permission-auto-accept (server-authoritative state, UI as a
// projection, VS Code keeps its own foreground implementation).
//
// Event-driven like session-goal: the shared upstream hub delivers
// `session.status`, and an idle transition arms a short per-session timer. The
// tick re-verifies idleness against OpenCode (status map + message tail) before
// it sends, because a queued prompt sent into a running turn would be steered
// into it instead of starting the next one.

import fs from 'fs';
import path from 'path';
import { createHash, randomUUID } from 'node:crypto';
import { z } from 'zod';
import { assertPromptResponse } from '../opencode/prompt-response.js';
import { unwrapOpenCodeResponse } from '../opencode/response-envelope.js';
import { createSessionActivityProbe } from '../opencode/session-activity.js';
import { attachmentHint } from '../routing/runtime.js';
import { buildGoalIntroText, createSessionGoal } from '../session-goal/create.js';
import {
  RECEIPT_BYTE_LIMIT, captureQueueRequest, createEnqueueReceipt, createTakeReceipt,
  parseRequestId, parseRequestOwner, parseStoredQueueRequest, publicQueueRequest,
  queueRequestError, requireMatchingRequest,
} from './receipts.js';
import { createQueueBlobStore, blobIdentity } from './blobs.js';
import { createManifestPromptBody, assertManifestPromptBudget, MANIFEST_PROMPT_BYTE_LIMIT, jsonByteLength } from './prompt-body.js';
import {
  BLOB_BYTE_LIMIT, attachmentIdSchema, parseManifestItem, parseUploadMetadata, queueUploadFilename,
  decodeInlineAttachment, storedBlobSchema, promptPartsSchema, referencePromptPartsSchema, manifestSendConfigSchema, capturedEntryCount, compactLegacyPayloadItem, parseStructuredItem,
} from './payloads.js';
import {
  assertStructuredBudget, parsePayloadPointer, readStructuredCapture, makePayloadIndex, sealPayloadPointer, referencesPayloadBlob, encodeLegacyCapture,
} from './structured-payloads.js';

const QUEUE_FILE_NAME = 'message-queue.json';
const QUEUE_FILE_VERSION = 1;

const MAX_SESSIONS = 50;
const MAX_ITEMS_PER_SESSION = 20;
const CONTENT_CHAR_LIMIT = 200_000;
// Match native's admission identity checks. Prepared attachments and metadata
// are deliberately not compared: OpenCode plugins can change that content.
const admissionIdentity = {
  id: z.string().startsWith('msg_').min(5),
  type: z.literal('user'),
  time: z.object({ created: z.number().int().nonnegative().safe() }),
};
const inboxUserSchema = z.object({
  ...admissionIdentity,
  sessionID: z.string().startsWith('ses_').min(5),
  delivery: z.enum(['queue', 'steer']),
});
const historyUserSchema = z.object({ ...admissionIdentity, text: z.string() });
const syntheticIdentity = { ...admissionIdentity, type: z.literal('synthetic') };
const inboxSyntheticSchema = z.object({ ...syntheticIdentity, sessionID: z.string().min(1), delivery: z.enum(['queue', 'steer']) });
const historySyntheticSchema = z.object({ ...syntheticIdentity, text: z.string() });

// Idle events arrive in bursts around a turn boundary; a short quiet window
// coalesces them before the tick verifies idleness against OpenCode.
const DISPATCH_QUIET_MS = 500;
// After a user abort the UI held the queue for two seconds so the stop is not
// immediately followed by the next prompt; the server keeps that window.
const ABORT_HOLD_MS = 2_000;
// While a background subagent keeps the turn open, how often the head is
// rechecked in case the parent's rerun idle event is missed.
const SUBAGENT_RECHECK_MS = 5_000;
const RETRY_BASE_DELAY_MS = 2_000;
const RETRY_MAX_DELAY_MS = 60_000;
const FETCH_TIMEOUT_MS = 15_000;
const MESSAGE_TAIL_LIMIT = 2;

const ATTACHMENT_SOURCES = new Set(['local', 'server', 'vscode']);
// Context captured with a queued message (see QueuedContextPart in the UI
// store): attached context items carry metadata the timeline renders back;
// the other kinds are plain synthetic text.
const CONTEXT_PART_KINDS = new Set(['context', 'instruction', 'synthetic']);
const SESSION_ID_PATTERN = /^[A-Za-z0-9_-]{4,128}$/;
const prefixComposerSchema = z.string().max(CONTENT_CHAR_LIMIT);
const contextClaimSchema = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('enqueue'), requestId: z.string() }).strict(),
  z.object({ kind: z.literal('take'), requestId: z.string(), itemId: z.string().min(1).max(256) }).strict(),
  z.object({ kind: z.literal('prefix'), id: z.string().regex(/^msg_[A-Za-z0-9_-]{1,120}$/) }).strict(),
]);
const contextOwnerSchema = z.object({
  sessionId: z.string().regex(SESSION_ID_PATTERN),
  directory: z.string().min(1).max(16384).refine((value) => value === value.trim()),
  claims: z.array(contextClaimSchema).min(1),
}).strict();
const prefixPromptOwnerSchema = z.object({
  sessionId: z.string().regex(SESSION_ID_PATTERN),
  directory: z.string().min(1).max(16384).refine((value) => value === value.trim()),
  state: z.enum(['reserved', 'consumed']),
}).strict();

const getQueuedSendRetryDelayMs = (failures) =>
  Math.min(RETRY_BASE_DELAY_MS * 2 ** Math.max(failures - 1, 0), RETRY_MAX_DELAY_MS);

// Boundary readers: the only place raw JSON (client bodies, the queue file,
// OpenCode responses, hub events) is inspected. Everything below them
// branches on the domain values they return.
const asNonEmptyString = (value) => (typeof value === 'string' && value.trim() ? value.trim() : '');
const asText = (value) => (typeof value === 'string' ? value : '');
const asRecord = (value) => (value && typeof value === 'object' && !Array.isArray(value) ? value : null);
const asList = (value) => (Array.isArray(value) ? value : null);
const asCount = (value) => (Number.isFinite(value) && value >= 0 ? Math.floor(value) : null);

const isValidSessionId = (value) => SESSION_ID_PATTERN.test(asNonEmptyString(value));

const httpError = (message, status) => Object.assign(new Error(message), { status });

const parseReservation = (value, sessionId, stored = false) => {
  const raw = asRecord(value);
  if (raw?.format === 'payload-v1') {
    const parsed = z.object({
      format: z.literal('payload-v1'), directory: z.string(), id: z.string().regex(/^msg_[A-Za-z0-9_-]{1,120}$/),
      itemIds: z.array(z.string().min(1).max(256)).min(1).max(MAX_ITEMS_PER_SESSION),
      composer: z.record(z.string(), z.unknown()), delivery: z.literal('steer').nullable().optional(),
    }).strict().safeParse(raw);
    if (!parsed.success || new Set(parsed.data.itemIds).size !== parsed.data.itemIds.length) throw new TypeError('invalid payload prefix reservation');
    return { ...parsed.data, directory: parseRequestOwner(sessionId, parsed.data.directory).directory,
      composer: parsePayloadPointer(parsed.data.composer, { stored }), delivery: parsed.data.delivery ?? null };
  }
  const itemIds = asList(raw?.itemIds);
  let sendConfig = parseSendConfig(raw?.sendConfig);
  if (!raw || !/^msg_[a-zA-Z0-9_-]{1,120}$/.test(asText(raw.id))
      || !itemIds?.length || itemIds.length > MAX_ITEMS_PER_SESSION
      || itemIds.some((id) => !asNonEmptyString(id)) || new Set(itemIds).size !== itemIds.length
      || !prefixComposerSchema.safeParse(raw.composer).success
      || !sendConfig || ![null, 'steer'].includes(raw.delivery ?? null)) {
    throw new TypeError('invalid prefix reservation');
  }
  if (raw.format !== undefined && raw.format !== 'manifest-v1') throw new TypeError('invalid prefix format');
  if (raw.format === 'manifest-v1' && Object.keys(raw).some((key) => !['id', 'itemIds', 'composer', 'sendConfig', 'delivery', 'format', 'promptParts', 'directory'].includes(key))) {
    throw new TypeError('unknown manifest prefix field');
  }
  if (raw.format === 'manifest-v1') {
    const parsed = manifestSendConfigSchema.safeParse(raw.sendConfig);
    if (!parsed.success) throw new TypeError('invalid captured prefix sendConfig');
    sendConfig = parsed.data;
  }
  if (raw.promptParts !== undefined && raw.format !== 'manifest-v1') throw new TypeError('prefix promptParts requires manifest-v1');
  const parts = raw.promptParts === undefined ? null : promptPartsSchema.safeParse(raw.promptParts);
  if (parts && !parts.success) throw new TypeError('invalid captured prefix promptParts');
  // Persisted and fingerprinted as JSON, so optional fields stay absent.
  const reservation = { id: raw.id, itemIds: [...itemIds], composer: raw.composer, sendConfig, delivery: raw.delivery ?? null };
  if (raw.format) reservation.format = raw.format;
  if (raw.format === 'manifest-v1') reservation.directory = parseRequestOwner(sessionId, raw.directory).directory;
  if (parts) reservation.promptParts = parts.data;
  return reservation;
};

const reservationFingerprint = (reservation) => JSON.stringify(reservation.format === 'payload-v1'
  ? { ...reservation, composer: { format: 'payload-v1', payloadManifest: reservation.composer.payloadManifest } }
  : reservation);

const nativeTakeable = (item) => !item.goal && !item.scheduledTask
  && !item.format
  && !item.promptParts
  && (item.context ?? []).every((part) => Buffer.byteLength(part.text) <= 1048576
    && Buffer.byteLength(part.instructions ?? '') <= 1048576
    && Buffer.byteLength(JSON.stringify(part.metadata ?? {})) <= 65536)
  && item.attachments.every((file) => {
  if (file.source === 'server' && file.serverPath) return 8 + 3 * Buffer.byteLength(file.serverPath) <= 65536;
  return Math.ceil(file.size / 3) * 4 + 256 <= 65536 && Buffer.byteLength(file.dataUrl ?? '') <= 65536;
});

// The native journal validates the complete reconstructed prompt, including
// UTF-8 byte counts and all files, agent mentions and synthetic entries.
// Values come from parsed legacy items, where these fields are strings.
const nativeIdentifier = (value) => value.length > 0
  && Buffer.byteLength(value) <= 4096 && !/[\u0000-\u001f\u007f-\u009f]/.test(value);
const nativeTransferFits = (items) => items.every((item) => !item.format)
  && Buffer.byteLength(items.map((item) => item.text).filter((text) => text.trim()).join('\n\n')) <= 1048576
  && items.reduce((count, item) => count + capturedEntryCount(item), 0) <= 4096
  && items.every((item) => nativeTakeable(item)
    && Buffer.byteLength(item.text) <= 1048576 && Buffer.byteLength(item.content) <= 1048576
    && [item.sendConfig.providerID, item.sendConfig.modelID, item.sendConfig.agent, item.sendConfig.variant, item.agentMention]
      .every((value) => value === undefined || nativeIdentifier(value))
    && item.attachments.every((file) => nativeIdentifier(file.filename)
      && ((file.source === 'server' && file.serverPath) || !/[\u0000-\u001f\u007f-\u009f]/.test(file.dataUrl)))
    && item.context.every((part) => part.text.trim() && (!part.instructions || part.instructions.trim())));

const parseSendConfig = (value) => {
  const raw = asRecord(value);
  if (!raw) return null;
  const providerID = asNonEmptyString(raw.providerID);
  const modelID = asNonEmptyString(raw.modelID);
  if (!providerID || !modelID) return null;
  const sendConfig = { providerID, modelID };
  const agent = asNonEmptyString(raw.agent);
  if (agent) sendConfig.agent = agent;
  const variant = asNonEmptyString(raw.variant);
  if (variant) sendConfig.variant = variant;
  return sendConfig;
};

// "Run as goal" captured with the message: `objective` only when the arming
// flow supplied its own (the plan flow), otherwise the delivered text is it.
const parseGoal = (value) => {
  if (value === undefined) return null;
  const raw = asRecord(value);
  if (!raw) throw new TypeError('invalid goal');
  const tokenBudget = raw.tokenBudget ?? null;
  if (tokenBudget !== null && !(Number.isInteger(tokenBudget) && tokenBudget > 0)) throw new TypeError('invalid goal tokenBudget');
  const goal = { tokenBudget };
  const objective = asNonEmptyString(raw.objective);
  if (objective) goal.objective = objective;
  return goal;
};

const parseAttachment = (value) => {
  const raw = asRecord(value);
  if (!raw) return null;
  const filename = asNonEmptyString(raw.filename);
  const mimeType = asNonEmptyString(raw.mimeType);
  const dataUrl = asText(raw.dataUrl);
  if (!filename || !mimeType || !dataUrl) return null;
  const attachment = {
    id: asNonEmptyString(raw.id) || `attachment-${Date.now()}-${Math.random().toString(36).slice(2, 9)}`,
    filename,
    mimeType,
    size: asCount(raw.size) ?? 0,
    source: ATTACHMENT_SOURCES.has(raw.source) ? raw.source : 'local',
  };
  const serverPath = asNonEmptyString(raw.serverPath);
  if (serverPath) attachment.serverPath = serverPath;
  const agentPath = asNonEmptyString(raw.agentPath);
  if (agentPath) attachment.agentPath = agentPath;
  attachment.dataUrl = dataUrl;
  return attachment;
};

const parseContextPart = (value) => {
  const raw = asRecord(value);
  if (!raw || !CONTEXT_PART_KINDS.has(raw.kind)) return null;
  const text = asText(raw.text);
  if (raw.kind !== 'context') return { kind: raw.kind, text };
  // The metadata is the UI's structured payload; the server only carries it
  // to the prompt, so its shape is the UI's to validate on the way back.
  const metadata = asRecord(raw.metadata);
  if (!metadata) return null;
  const part = { kind: 'context', text, metadata };
  const instructions = asNonEmptyString(raw.instructions);
  if (instructions) part.instructions = instructions;
  return part;
};

/**
 * Validates a queued item posted by a client. Throws a TypeError (→ 400) for
 * anything that could not be delivered later: a queue must never hold an item
 * the server cannot send.
 */
export const parseQueuedItemInput = (value) => {
  const raw = asRecord(value);
  if (!raw) throw new TypeError('item is required');
  const content = asText(raw.content).replace(/^\n+|\n+$/g, '');
  if (content.length > CONTENT_CHAR_LIMIT) throw new TypeError('item content is too long');
  const text = raw.text === undefined ? content : asText(raw.text);
  const attachments = (asList(raw.attachments) ?? []).map(parseAttachment);
  if (attachments.some((attachment) => attachment === null)) throw new TypeError('invalid attachment');
  const context = (asList(raw.context) ?? []).map(parseContextPart);
  if (context.some((part) => part === null)) throw new TypeError('invalid context part');
  const parsedParts = raw.promptParts === undefined ? undefined : referencePromptPartsSchema.safeParse(raw.promptParts);
  if (parsedParts && !parsedParts.success) throw new TypeError('invalid reference promptParts');
  const promptParts = parsedParts?.data;
  if (!text.trim() && attachments.length === 0 && context.length === 0 && !promptParts?.fileReferences?.length && !promptParts?.context.length) {
    throw new TypeError('item needs text, attachments, or context');
  }
  const sendConfig = parseSendConfig(raw.sendConfig);
  const provenance = asRecord(raw.scheduledTask);
  const projectId = asNonEmptyString(provenance?.projectId);
  const taskId = asNonEmptyString(provenance?.taskId);
  if (provenance && (!projectId || !taskId)) throw new TypeError('invalid scheduledTask provenance');
  if (!sendConfig && !provenance) throw new TypeError('item sendConfig with providerID and modelID is required');
  const goal = parseGoal(raw.goal);
  if (goal && !text.trim()) throw new TypeError('a goal needs message text');
  const item = { content, text };
  const agentMention = asNonEmptyString(raw.agentMention);
  if (agentMention) item.agentMention = agentMention;
  item.attachments = attachments;
  item.context = context;
  if (promptParts) item.promptParts = promptParts;
  if (promptParts && capturedEntryCount(item) > 4096) throw new TypeError('captured prompt exceeds entry limit');
  const contextPreview = asNonEmptyString(raw.contextPreview).slice(0, 103);
  if (contextPreview) item.contextPreview = contextPreview;
  item.sendConfig = sendConfig ?? {};
  if (goal) item.goal = goal;
  if (provenance) {
    item.scheduledTask = { projectId, taskId };
    const agent = asNonEmptyString(raw.sendConfig?.agent);
    if (agent) item.sendConfig.agent = agent;
  }
  return item;
};

const parseStoredItem = (value) => {
  const raw = asRecord(value);
  const id = raw ? asNonEmptyString(raw.id) : '';
  if (!id) return null;
  try {
    let item;
    if (raw.format === 'payload-v1') {
      const { id: _id, createdAt: _createdAt, requestId: _requestId, ...pointer } = raw;
      item = { id, createdAt: asCount(raw.createdAt) ?? Date.now(), ...parsePayloadPointer(pointer, { stored: true }) };
    } else {
      if (raw.format !== undefined && raw.format !== 'manifest-v1') throw new TypeError('unsupported stored item format');
      const { id: _id, createdAt: _createdAt, requestId: _requestId, attachments: _attachments, ...manifestInput } = raw;
      item = { id, createdAt: asCount(raw.createdAt) ?? Date.now(),
        ...(raw.format === 'manifest-v1' ? parseManifestItem(manifestInput) : parseQueuedItemInput(raw)) };
    }
    // Persisted as JSON: an item without a request receipt has no requestId key.
    if (raw.requestId !== undefined) item.requestId = parseRequestId(raw.requestId);
    return item;
  } catch {
    return null;
  }
};

// Queue item identity, persisted as JSON: an item without a request receipt
// has no requestId key.
const itemIdentity = (item) => {
  const identity = { id: item.id, createdAt: item.createdAt };
  if (item.requestId) identity.requestId = item.requestId;
  return identity;
};

const toPublicAttachment = ({ dataUrl: _dataUrl, ...attachment }) => attachment;

// What clients see: everything except the payloads — attachment data URLs
// (megabytes of base64) and captured context (a PR diff, say) — which would
// otherwise ride every broadcast. A take hands the full item back.
const toPublicItem = (item) => {
  if (item.format === 'payload-v1') {
    const publicItem = {
      id: item.id, createdAt: item.createdAt, format: item.format, content: item.payloadIndex.preview, text: item.payloadIndex.preview,
      attachments: [], sendConfig: { ...item.payloadIndex.sendConfig },
    };
    if (item.payloadIndex.contextPreview !== undefined) publicItem.contextPreview = item.payloadIndex.contextPreview;
    return publicItem;
  }
  const publicItem = { id: item.id, createdAt: item.createdAt, content: item.content, text: item.text };
  if (item.agentMention) publicItem.agentMention = item.agentMention;
  publicItem.attachments = item.format === 'manifest-v1'
    ? item.attachmentManifest.map((file) => ({ id: file.attachmentId, filename: file.filename, mimeType: file.mimeType, size: file.size, source: 'local' }))
    : item.attachments.map(toPublicAttachment);
  if (item.format) publicItem.format = item.format;
  // Older persisted items have no UI summary. Prefer their attached comment
  // before falling back to the model-facing context text.
  const contextPreview = item.contextPreview || item.context
    .filter((part) => part.kind !== 'instruction')
    .map((part) => asNonEmptyString(asRecord(part.metadata?.openchamberContext)?.text) || part.text.trim())
     .find(Boolean) || item.promptParts?.fileReferences?.[0]?.name;
  if (contextPreview) {
    const firstLine = contextPreview.split('\n', 1)[0];
    publicItem.contextPreview = firstLine.slice(0, 100)
      + (contextPreview.length > firstLine.length || firstLine.length > 100 ? '...' : '');
  }
  publicItem.sendConfig = { ...item.sendConfig };
  if (item.goal) publicItem.goal = { ...item.goal };
  if (item.scheduledTask) publicItem.scheduledTask = { ...item.scheduledTask };
  return publicItem;
};

const extractSessionStatus = (payload) => {
  if (payload.type !== 'session.status') return null;
  const properties = asRecord(payload.properties) ?? {};
  const status = asRecord(properties.status) ?? {};
  const info = asRecord(properties.info) ?? {};
  const sessionId = asNonEmptyString(properties.sessionID);
  const type = asNonEmptyString(status.type) || asNonEmptyString(info.type);
  if (!sessionId || !type) return null;
  return { sessionId, type };
};

const extractAssistantMessageUpdate = (payload) => {
  if (payload.type !== 'message.updated') return null;
  const info = asRecord(asRecord(payload.properties)?.info);
  if (!info || info.role !== 'assistant') return null;
  const sessionId = asNonEmptyString(info.sessionID);
  if (!sessionId) return null;
  return {
    sessionId,
    completed: asCount(asRecord(info.time)?.completed) !== null,
  };
};

/**
 * A user abort no longer arrives as an assistant message carrying
 * `MessageAbortedError`: v2 reports `session.execution.interrupted`, which the
 * translator turns into `session.idle` with `aborted: true`.
 */
const extractAbortedSessionId = (payload) => {
  if (payload.type !== 'session.idle') return null;
  const properties = asRecord(payload.properties) ?? {};
  if (properties.aborted !== true) return null;
  return asNonEmptyString(properties.sessionID);
};

const extractDeletedSessionId = (payload) => {
  if (payload.type !== 'session.deleted') return null;
  const properties = asRecord(payload.properties) ?? {};
  return asNonEmptyString(asRecord(properties.info)?.id) || asNonEmptyString(properties.sessionID) || null;
};

export function createMessageQueueRuntime({
  globalEventHub,
  buildOpenCodeUrl,
  getOpenCodeAuthHeaders,
  sessionKnowledgeRuntime = null,
  broadcastGlobalUiEvent,
  onPromptSent,
  beforeScheduledTaskSend,
  validateScheduledTaskTarget,
  onScheduledTaskResult,
  // Resolves the `openchamber/auto` sentinel into a real model and agent right
  // before the send; absent means the queue never sees the sentinel.
  resolveAutoSelection = null,
  // Saves the goal record of a message queued as a goal (the session metadata store).
  persistSessionGoal = null,
  // True while something else drives the session (an auto-review run), which
  // calls `release` when it is done. `deliveryHoldReady` resolves once that
  // owner knows which sessions it holds; the first dispatch waits for it.
  isDeliveryHeld = () => false,
  deliveryHoldReady = Promise.resolve(),
  dataDir,
  fetchImpl = fetch,
  now = Date.now,
  dispatchQuietMs = DISPATCH_QUIET_MS,
  abortHoldMs = ABORT_HOLD_MS,
  retryDelayMs = getQueuedSendRetryDelayMs,
}) {
  const filePath = path.join(dataDir, QUEUE_FILE_NAME);

  /** sessionId → { directory, items } */
  const queues = new Map();
  // A native send borrows a prefix while the queue remains its durable owner.
  // The stable prompt ID is also the receipt ID. No timeout releases ownership:
  // a late client may still admit that ID after losing its HTTP response.
  const reservations = new Map();
  // Receipts outlive queue items and session deletion. Only a pending take
  // owns bytes here; terminal receipts retain identities and a request digest.
  const requests = new Map();
  // Historical receipts keep their identities. This chain records each exact
  // custody transfer without making those old receipts competing owners.
  const contextOwners = new Map();
  // Every typed prefix owns its prompt identity, including text-only prefixes
  // with no synthetic context from which a fence could otherwise be recovered.
  const prefixPromptOwners = new Map();
  const blobs = new Map();
  const activeUploads = new Map();
  const blobStore = createQueueBlobStore(dataDir);
  // No expanded captures survive this exclusion. Never acquire blobOwnership
  // from inside it: enqueue/capture already hold that ownership lock.
  let payloadOperation = Promise.resolve();
  const withPayloadOperation = (work) => {
    const result = payloadOperation.then(work);
    payloadOperation = result.catch(() => {});
    return result;
  };
  // Root writes keep their existing serial order. This exclusion additionally
  // spans captured byte validation and ownership acquisition/release.
  let blobOwnership = Promise.resolve();
  const withBlobOwnership = (work) => {
    const result = blobOwnership.then(work);
    blobOwnership = result.catch(() => {});
    return result;
  };
  const preparingTransfers = new Set();
  const pendingRequests = new Set();
  const requestWrites = new Map();
  let revision = 0;
  let loadPromise = null;
  let writePromise = Promise.resolve();
  let stopped = false;
  let receiptRecoveryRequired = false;

  // An unfinished assistant message older than this marker is a run that died
  // with the previous server, not a streaming turn: no completion event will
  // ever arrive for it, so treating it as live strands restored queue items
  // forever. A run that outlived the restart (external OpenCode) is still
  // caught by the live status check, which runs first.
  const runtimeStartedAt = now();

  /** In-memory only — a restart has no in-flight sends. */
  const sending = new Map(); // sessionId → itemId
  const timers = new Map(); // sessionId → timeout
  const failures = new Map(); // sessionId → { itemId, failures, nextAttemptAt }
  const abortedAt = new Map(); // sessionId → timestamp
  // sessionId → directory, kept after the queue empties: the UI keys its
  // projection by directory, so the broadcast that removes the last item must
  // still name it or the client cannot tell which queue just finished.
  const directories = new Map();

  // --- persistence ---------------------------------------------------------

  // The queue file leaves empty collections and a clear recovery flag absent.
  const serialize = () => {
    const sessions = Object.fromEntries(Array.from(queues, ([sessionId, queue]) => {
      const entry = { directory: queue.directory, items: queue.items };
      if (reservations.has(sessionId)) entry.reservation = reservations.get(sessionId);
      return [sessionId, entry];
    }));
    const snapshot = { version: QUEUE_FILE_VERSION, revision, sessions };
    if (requests.size) snapshot.requests = Object.fromEntries(requests);
    if (contextOwners.size) snapshot.contextOwners = Object.fromEntries(contextOwners);
    if (prefixPromptOwners.size) snapshot.prefixPromptOwners = Object.fromEntries(prefixPromptOwners);
    if (blobs.size) snapshot.blobs = Object.fromEntries(blobs);
    if (receiptRecoveryRequired) snapshot.receiptRecoveryRequired = true;
    return snapshot;
  };

  const writeSnapshot = async (payload, durable) => {
    await fs.promises.mkdir(dataDir, { recursive: true });
    const tmpPath = `${filePath}.${process.pid}.tmp`;
    // Queued prompts are conversation content: readable by the owner only.
    const file = await fs.promises.open(tmpPath, 'w', 0o600);
    try {
      await file.writeFile(payload, 'utf8');
      if (durable) await file.sync();
    } finally {
      await file.close();
    }
    await fs.promises.rename(tmpPath, filePath);
    if (durable && process.platform !== 'win32') {
      const directory = await fs.promises.open(dataDir, 'r');
      try { await directory.sync(); } finally { await directory.close(); }
    }
  };

  const readFile = async () => {
    let raw;
    try {
      raw = await fs.promises.readFile(filePath, 'utf8');
    } catch (error) {
      if (asRecord(error)?.code === 'ENOENT') return { sessions: {}, revision: 0 };
      throw error;
    }
    let parsed;
    try {
      parsed = JSON.parse(raw);
    } catch (error) {
      // Malformed is a failure, not an empty queue: keep the bytes for the
      // user and start over rather than overwriting them on the next write.
      const backup = `${filePath}.corrupt-${now()}`;
      // Preserve and sync the backup before replacing the corrupt original.
      // There is never a missing-file interval that grants fresh receipt
      // authority if this process stops during recovery.
      await fs.promises.copyFile(filePath, backup, fs.constants.COPYFILE_EXCL);
      const retained = await fs.promises.open(backup, 'r');
      try { await retained.sync(); } finally { await retained.close(); }
      await writeSnapshot(JSON.stringify({ version: QUEUE_FILE_VERSION, revision: 0, sessions: {}, receiptRecoveryRequired: true }), true);
      console.warn(`[message-queue] queue file was unreadable and moved to ${backup}: ${error?.message ?? error}`);
      return { sessions: {}, revision: 0, receiptRecoveryRequired: true };
    }
    const stored = asRecord(parsed);
    if (!stored || stored.version !== QUEUE_FILE_VERSION || !Number.isSafeInteger(stored.revision)
        || stored.revision < 0 || !asRecord(stored.sessions)) throw new TypeError('invalid stored queue root');
    const sessions = {};
    for (const [sessionId, value] of Object.entries(asRecord(stored.sessions) ?? {})) {
      const entry = asRecord(value);
      if (!entry || !isValidSessionId(sessionId)) throw new TypeError('invalid stored queue session');
      const directory = asNonEmptyString(entry.directory);
      if (!directory) throw new TypeError('invalid stored queue directory');
      const storedItems = asList(entry.items);
      if (!storedItems) throw new TypeError('invalid stored queue items');
      const items = storedItems.map((value) => {
        const item = parseStoredItem(value);
        if (!item && (asRecord(value)?.requestId !== undefined || asRecord(value)?.format !== undefined)) throw new TypeError('invalid stored durable queue item');
        return item;
      }).filter(Boolean);
      if (items.length === 0) {
        if (entry.reservation != null) throw new TypeError('stored queue reservation lost its items');
        continue;
      }
      const reservation = entry.reservation == null ? null : parseReservation(entry.reservation, sessionId, true);
      if (reservation && ((reservation.format && reservation.directory !== directory)
          || !reservation.itemIds.every((id, index) => items[index]?.id === id
            && (reservation.format ? !items[index].goal && !items[index].scheduledTask : nativeTakeable(items[index]))))) {
        throw new TypeError('stored queue reservation does not match its prefix');
      }
      sessions[sessionId] = { directory, items, reservation };
    }
    const storedRequests = asRecord(stored.requests);
    if (stored.requests !== undefined && !storedRequests) throw new TypeError('invalid queue request ledger');
    if (storedRequests && stored.version !== QUEUE_FILE_VERSION) throw new TypeError('unsupported queue request file version');
    if (stored.receiptRecoveryRequired !== undefined && stored.receiptRecoveryRequired !== true) throw new TypeError('invalid queue receipt recovery marker');
    const receipts = new Map();
    for (const [requestId, value] of Object.entries(storedRequests ?? {})) {
      const record = parseStoredQueueRequest(value, requestId);
      if (record.kind === 'take' && record.state === 'transferring') {
        const items = record.items.map(parseStoredItem);
        if (items.length !== record.receipt.itemIds.length || items.some((item, index) => !item || item.id !== record.receipt.itemIds[index])) {
          throw new TypeError('stored take receipt does not own its full payload');
        }
        record.items = items;
        if (Buffer.byteLength(JSON.stringify({ ...publicQueueRequest(record), revision: Number.MAX_SAFE_INTEGER })) > RECEIPT_BYTE_LIMIT) throw new TypeError('stored take exceeds receipt limits');
      }
      receipts.set(requestId, record);
    }
    const owners = stored.contextOwners === undefined
      ? deriveContextOwners(receipts, sessions)
      : new Map(Object.entries(z.record(z.string().regex(/^msg_[A-Za-z0-9_-]{1,120}$/), contextOwnerSchema).parse(stored.contextOwners)));
    const promptOwners = stored.prefixPromptOwners === undefined
      ? derivePrefixPromptOwners(owners, sessions)
      : new Map(Object.entries(z.record(z.string().regex(/^msg_[A-Za-z0-9_-]{1,120}$/), prefixPromptOwnerSchema).parse(stored.prefixPromptOwners)));
    validatePrefixPromptOwners(promptOwners, owners, receipts, sessions);
    validateContextOwners(owners, receipts, sessions, promptOwners);
    for (const [sessionId, queue] of Object.entries(sessions)) {
      for (const item of queue.items) {
        if (item.requestId === undefined) continue;
        const record = receipts.get(item.requestId);
        if (!record || record.kind !== 'enqueue' || record.receipt.sessionId !== sessionId
            || record.receipt.directory !== queue.directory || record.receipt.itemId !== item.id
            || !['queued', 'uncertain'].includes(record.state)
            || record.receipt.contextIds.length !== capturedContext(item).length
            || capturedContext(item).some((part, index) => part.id !== undefined && part.id !== record.receipt.contextIds[index])) {
          throw new TypeError('stored queued item does not match its request receipt');
        }
      }
    }
    for (const record of receipts.values()) {
      if (record.kind === 'enqueue' && ['queued', 'uncertain'].includes(record.state)
          && !sessions[record.receipt.sessionId]?.items.some((item) => item.id === record.receipt.itemId && item.requestId === record.receipt.requestId)) {
        throw new TypeError('stored queue request lost its item');
      }
    }
    const storedBlobs = asRecord(stored.blobs);
    if (stored.blobs !== undefined && !storedBlobs) throw new TypeError('invalid queue blob ledger');
    const retainedBlobs = new Map();
    for (const [id, raw] of Object.entries(storedBlobs ?? {})) {
      const entry = storedBlobSchema.parse(raw);
      if (entry.metadata.blobId !== id || blobIdentity(entry.metadata) !== id) throw new TypeError('invalid stored blob identity');
      retainedBlobs.set(id, entry);
    }
    for (const [sessionId, queue] of Object.entries(sessions)) {
      for (const item of queue.items) validateManifestReferences(item, sessionId, queue.directory, retainedBlobs);
      if (queue.reservation?.format === 'payload-v1') validateManifestReferences(queue.reservation.composer, sessionId, queue.directory, retainedBlobs);
    }
    for (const record of receipts.values()) {
      for (const item of record.items ?? []) validateManifestReferences(item, record.receipt.sessionId, record.receipt.directory, retainedBlobs);
    }
    return { sessions, requests: receipts, contextOwners: owners, prefixPromptOwners: promptOwners, blobs: retainedBlobs,
      revision: asCount(stored.revision) ?? 0, receiptRecoveryRequired: stored.receiptRecoveryRequired === true };
  };

  const load = () => {
    if (!loadPromise) {
      loadPromise = readFile()
        .then((stored) => {
          for (const [sessionId, entry] of Object.entries(stored.sessions)) {
            queues.set(sessionId, { directory: entry.directory, items: entry.items });
            if (entry.reservation) reservations.set(sessionId, entry.reservation);
          }
          for (const [id, record] of stored.requests ?? []) requests.set(id, record);
          for (const [id, owner] of stored.contextOwners ?? []) contextOwners.set(id, owner);
          for (const [id, owner] of stored.prefixPromptOwners ?? []) prefixPromptOwners.set(id, owner);
          for (const [id, record] of stored.blobs ?? []) blobs.set(id, record);
          receiptRecoveryRequired = stored.receiptRecoveryRequired === true;
          revision = Math.max(revision, stored.revision);
        })
        .catch((error) => {
          // A read failure keeps the in-memory (empty) queue but must not be
          // mistaken for "nothing queued": the next write would clobber the
          // file, so writes stay disabled until a later load succeeds.
          loadPromise = null;
          throw error;
        });
    }
    return loadPromise;
  };

  const persist = () => {
    const payload = JSON.stringify(serialize());
    const durable = requests.size > 0 || contextOwners.size > 0 || prefixPromptOwners.size > 0 || blobs.size > 0 || reservations.size > 0 || receiptRecoveryRequired;
    const write = writePromise.then(() => writeSnapshot(payload, durable));
    writePromise = write.catch((error) => {
        console.warn('[message-queue] failed to persist queue:', error?.message ?? error);
      });
    return write;
  };

  const manifestOf = (metadata) => {
    const { blobId, requestId, attachmentId, filename, mimeType, size, sha256 } = metadata;
    return { blobId, requestId, attachmentId, filename, mimeType, size, sha256 };
  };
  const requireUploadOwner = (sessionId, requestId, directory) => {
    const receipt = requests.get(requestId)?.receipt;
    if ((receipt && (receipt.sessionId !== sessionId || receipt.directory !== directory))
        || [...blobs.values(), ...activeUploads.values()].some(({ metadata }) => metadata.requestId === requestId
          && (metadata.sessionId !== sessionId || metadata.directory !== directory))) {
      throw queueRequestError('requestId belongs to another captured owner', 409, 'queue_request_conflict');
    }
  };
  const validateManifestReferences = (item, sessionId, directory, index = blobs) => {
    if (item.format === 'payload-v1') {
      if (item.payloadIndex && item.payloadIndex.directory !== directory) throw new TypeError('payload root index belongs to another directory');
      for (const id of item.payloadIndex?.attachmentBlobIds ?? []) {
        const entry = index.get(id);
        if (!entry || entry.state !== 'available' || entry.metadata.sessionId !== sessionId || entry.metadata.directory !== directory) {
          throw queueRequestError('payload attachment index does not match an available owned upload', 409, 'queue_request_conflict');
        }
      }
      item = { format: 'manifest-v1', attachmentManifest: item.payloadManifest.chunks };
    }
    if (item.format !== 'manifest-v1') return;
    for (const file of item.attachmentManifest) {
      const entry = index.get(file.blobId);
      if (!entry || entry.state !== 'available' || entry.metadata.sessionId !== sessionId || entry.metadata.directory !== directory
          || Object.entries(manifestOf(entry.metadata)).some(([key, value]) => file[key] !== value)) {
        throw queueRequestError('attachment manifest does not match an available owned upload', 409, 'queue_request_conflict');
      }
    }
  };
  const verifyManifestFiles = async (item, sessionId, directory) => {
    validateManifestReferences(item, sessionId, directory);
    const verified = new Set();
    for (const descriptor of item.attachmentManifest) {
      if (verified.has(descriptor.blobId)) continue;
      await blobStore.read(blobs.get(descriptor.blobId).metadata);
      verified.add(descriptor.blobId);
    }
  };
  const prepareAttachment = (sessionId, requestId, attachmentId, input) => {
    parseRequestId(requestId);
    const { directory, ...fields } = input;
    parseRequestOwner(sessionId, directory);
    requireUploadOwner(sessionId, requestId, directory);
    const parsed = parseUploadMetadata(sessionId, requestId, attachmentId, fields);
    const metadata = { ...parsed, directory, blobId: blobIdentity({ sessionId, directory, requestId, attachmentId }) };
    if (activeUploads.has(metadata.blobId)) throw queueRequestError('attachment upload is in progress', 409, 'queue_request_pending');
    const existing = blobs.get(metadata.blobId);
    if (existing && Object.entries(metadata).some(([key, value]) => existing.metadata[key] !== value)) {
      throw queueRequestError('attachment identity belongs to different immutable metadata', 409, 'queue_request_conflict');
    }
    if (!existing && requests.has(requestId)) throw queueRequestError('captured queue request cannot acquire another attachment', 409, 'queue_request_conflict');
    if (existing?.state === 'released') throw queueRequestError('attachment identity has been explicitly released', 409, 'queue_request_conflict');
    return { metadata, state: 'available' };
  };
  const storeAttachment = async (sessionId, requestId, attachmentId, input, body) => {
    const record = prepareAttachment(sessionId, requestId, attachmentId, input);
    const { metadata } = record;
    await blobStore.write(metadata, body);
    blobs.set(metadata.blobId, record);
    await persist();
    return { attachment: manifestOf(metadata), state: record.state };
  };
  const uploadAttachment = async (sessionId, requestId, attachmentId, input, body) => {
    const captured = structuredClone(input);
    const record = await withBlobOwnership(async () => {
      await load();
      requireReceiptHistory();
      const entry = prepareAttachment(sessionId, requestId, attachmentId, captured);
      activeUploads.set(entry.metadata.blobId, entry);
      return entry;
    });
    const { metadata } = record;
    try {
      // A stalled network stream holds only this upload identity. Other
      // requests and sessions can still capture their durable queue state.
      await blobStore.write(metadata, body);
      return await withBlobOwnership(async () => {
        blobs.set(metadata.blobId, record);
        await persist();
        return { attachment: manifestOf(metadata), state: 'available' };
      });
    } finally { activeUploads.delete(metadata.blobId); }
  };
  const convertManifestItem = async (item, itemIndex, sessionId, requestId, directory) => {
    if (item.goal || item.scheduledTask || item.format === 'payload-v1') throw httpError('queued message requires a payload-backed transfer', 409);
    if (item.format === 'manifest-v1') {
      await verifyManifestFiles(item, sessionId, directory);
      return item;
    }
    if (capturedEntryCount(item) > 4096) throw new TypeError('captured prompt exceeds entry limit');
    if (Buffer.byteLength(JSON.stringify({ ...item, attachments: [] })) > RECEIPT_BYTE_LIMIT) {
      throw queueRequestError('captured metadata exceeds receipt limits', 413, 'queue_request_too_large');
    }
    const attachmentManifest = [];
    for (const [fileIndex, file] of item.attachments.entries()) {
      const bytes = decodeInlineAttachment(file);
      const attachmentId = `item-${itemIndex}-file-${fileIndex}`;
      const filename = queueUploadFilename(file.filename, `${attachmentId}.bin`);
      const modelName = file.agentPath ?? file.filename;
      const upload = await storeAttachment(sessionId, requestId, attachmentId, {
        directory, filename, mimeType: file.mimeType, size: bytes.length,
        sha256: createHash('sha256').update(bytes).digest('hex'),
      }, bytes);
      const manifestFile = { ...upload.attachment };
      if (file.agentPath !== undefined || modelName !== filename) manifestFile.agentPath = modelName;
      attachmentManifest.push(manifestFile);
    }
    const manifestInput = { format: 'manifest-v1', content: item.content, text: item.text, context: item.context };
    if (item.contextPreview) manifestInput.contextPreview = item.contextPreview;
    Object.assign(manifestInput, {
      sendConfig: item.sendConfig, attachmentManifest,
      promptParts: { skills: [], context: [], ...item.promptParts,
        agents: item.promptParts?.agents.length ? item.promptParts.agents : item.agentMention ? [{ name: item.agentMention }] : [] },
    });
    return { ...itemIdentity(item), ...parseManifestItem(manifestInput) };
  };
  const captureManifestItems = async (items, sessionId, requestId, directory) => {
    const converted = [];
    for (const [itemIndex, item] of items.entries()) {
      converted.push(await convertManifestItem(item, itemIndex, sessionId, requestId, directory));
    }
    return converted;
  };
  const readPayload = async (pointer, sessionId, directory, options) => {
    validateManifestReferences(pointer, sessionId, directory);
    const logical = await readStructuredCapture(pointer, directory, (chunk) => blobStore.read(blobs.get(chunk.blobId).metadata), options);
    if (logical.format === 'manifest-v1') await verifyManifestFiles(logical, sessionId, directory);
    else for (const file of logical.attachments) decodeInlineAttachment(file);
    return logical;
  };
  const indexPayload = (logical, directory, contextIds, receipt) => {
    const prepared = preparePrompt(logical, receipt);
    const promptBytes = prepared.streamBody?.contentLength ?? jsonByteLength(prepared.promptBody);
    let admissionBytes = promptBytes;
    for (const [index, synthetic] of capturedContext(logical).entries()) {
      admissionBytes = Math.max(admissionBytes, jsonByteLength({ ...synthetic, id: contextIds[index], delivery: 'queue' }));
    }
    return makePayloadIndex(logical, directory, contextIds, promptBytes, admissionBytes);
  };
  const requireCompactPayload = (item) => {
    item = { ...item, ...sealPayloadPointer({ format: item.format, payloadManifest: item.payloadManifest, payloadIndex: item.payloadIndex }) };
    if (Buffer.byteLength(JSON.stringify(item)) > RECEIPT_BYTE_LIMIT) {
      throw queueRequestError('payload pointer and verified index exceed receipt limits', 413, 'queue_request_too_large');
    }
    return item;
  };
  const materializePayload = async (pointer, sessionId, directory, receipt, options) => {
    const logical = await readPayload(pointer, sessionId, directory, options);
    const ids = pointer.payloadIndex.contextIds;
    const contexts = capturedContext(logical);
    if (contexts.length !== ids.length
        || contexts.some((context, index) => context.id !== undefined && context.id !== ids[index])
        || JSON.stringify(indexPayload(logical, directory, ids, receipt)) !== JSON.stringify(pointer.payloadIndex)) {
      throw new TypeError('structured payload differs from its verified root index');
    }
    return logical;
  };
  const freezePayloadContextIds = (logical, seed, receipt) => {
    const prefix = `msg_${createHash('sha256').update(seed).digest('hex').slice(0, 24)}_`;
    return capturedContext(logical).map((context, index) => receipt?.contextIds[index] ?? context.id ?? `${prefix}${String(index).padStart(6, '0')}`);
  };
  const capturePayloadItems = async (items, sessionId, requestId, directory) => {
    const converted = [];
    for (const [itemIndex, item] of items.entries()) {
      converted.push(await withPayloadOperation(async () => {
        const receipt = requests.get(item.requestId)?.receipt;
        if (item.format === 'payload-v1') {
          await materializePayload(item, sessionId, directory, receipt);
          return item;
        }
        if (item.goal || item.scheduledTask) throw httpError('queued message cannot be transferred', 409);
        let logical = item;
        if (logical.format === 'manifest-v1') await verifyManifestFiles(logical, sessionId, directory);
        else {
          logical = parseStructuredItem(compactLegacyPayloadItem(item), {
            attachmentIds: item.attachments.map((file) => file.id.length <= 256 ? file.id : null),
          });
          for (const file of logical.attachments) decodeInlineAttachment(file);
        }
        const { id: _id, createdAt: _createdAt, requestId: _requestId, attachments: _attachments, ...manifestInput } = logical;
        const input = logical.format === 'manifest-v1' ? manifestInput : compactLegacyPayloadItem(logical);
        const capture = { directory, item: input };
        const json = logical.format === 'manifest-v1' ? JSON.stringify(capture) : encodeLegacyCapture(capture);
        assertStructuredBudget(Buffer.byteLength(json));
        const bytes = Buffer.from(json);
        const chunks = [];
        for (let offset = 0; offset < bytes.length; offset += BLOB_BYTE_LIMIT) {
          const part = bytes.subarray(offset, offset + BLOB_BYTE_LIMIT);
          const ordinal = chunks.length;
          const upload = await storeAttachment(sessionId, requestId, `item-${itemIndex}-payload-${ordinal}`, {
            directory, filename: `payload-${ordinal}.json`, mimeType: 'application/json', size: part.length,
            sha256: createHash('sha256').update(part).digest('hex'),
          }, part);
          chunks.push({ ordinal, ...upload.attachment });
        }
        return requireCompactPayload({ ...itemIdentity(item),
          format: 'payload-v1', payloadManifest: { version: 1, encoding: 'utf-8-json', totalBytes: bytes.length,
            sha256: createHash('sha256').update(bytes).digest('hex'), chunks },
          payloadIndex: indexPayload(logical, directory, freezePayloadContextIds(logical, JSON.stringify([sessionId, directory, item.id]), receipt), receipt),
        });
      }));
    }
    return converted;
  };
  const ownedAttachment = (sessionId, requestId, attachmentId, directory) => {
    parseRequestId(requestId);
    parseRequestOwner(sessionId, directory);
    if (!attachmentIdSchema.safeParse(attachmentId).success) throw new TypeError('invalid attachmentId');
    const entry = blobs.get(blobIdentity({ sessionId, directory, requestId, attachmentId }));
    if (!entry) throw httpError('owned attachment not found', 404);
    return entry;
  };
  const getAttachment = (sessionId, requestId, attachmentId, directory) => withBlobOwnership(async () => {
    await load();
    requireReceiptHistory();
    const entry = ownedAttachment(sessionId, requestId, attachmentId, directory);
    if (entry.state !== 'available') throw httpError('owned attachment has been released', 410);
    return { attachment: manifestOf(entry.metadata), bytes: await blobStore.read(entry.metadata) };
  });
  const releaseAttachment = (sessionId, requestId, attachmentId, directory) => withBlobOwnership(async () => {
    await load();
    requireReceiptHistory();
    const entry = ownedAttachment(sessionId, requestId, attachmentId, directory);
    const id = entry.metadata.blobId;
    if (activeUploads.has(id)) throw queueRequestError('attachment upload is in progress', 409, 'queue_request_pending');
    const references = (item) => referencesPayloadBlob(item, id);
    if ([...queues.values()].some((queue) => queue.items.some(references))
        || [...reservations.values()].some((reservation) => reservation.format === 'payload-v1' && references(reservation.composer))
        || [...requests.values()].some((request) => (request.items ?? []).some(references))) {
      throw httpError('attachment is still owned by a queued item or pending transfer', 409);
    }
    // Commit every captured ownership transition and the tombstone before
    // unlink. A failed unlink is safe to repeat; the identity cannot reopen.
    entry.state = 'released';
    await persist();
    await blobStore.remove(entry.metadata);
    return { attachment: manifestOf(entry.metadata), state: 'released' };
  });

  // --- snapshots -----------------------------------------------------------

  const sessionSnapshot = (sessionId) => {
    const queue = queues.get(sessionId);
    return {
      sessionId,
      directory: queue?.directory ?? directories.get(sessionId) ?? '',
      items: (queue?.items ?? []).map((item) => {
        const publicItem = toPublicItem(item);
        const record = requests.get(item.requestId);
        if (!record) return publicItem;
        const queueDelivery = { requestId: record.receipt.requestId, state: record.state, attempt: record.attempt };
        if (record.blocked) queueDelivery.blocked = record.blocked;
        return { ...publicItem, queueDelivery };
      }),
      sendingId: sending.get(sessionId) ?? null,
    };
  };

  const snapshot = () => ({
    revision,
    sessions: Array.from(queues.keys()).map(sessionSnapshot),
  });

  const broadcast = (sessionId) => {
    broadcastGlobalUiEvent?.({
      type: 'openchamber:message-queue.updated',
      properties: { revision, session: sessionSnapshot(sessionId) },
    });
  };

  /** Every mutation goes through here: bump, persist, broadcast. */
  const commit = (sessionId) => {
    revision += 1;
    void persist().catch(() => {});
    broadcast(sessionId);
    return { revision, session: sessionSnapshot(sessionId) };
  };

  const reportScheduledResult = (item, status, error) => {
    if (!item.scheduledTask) return;
    Promise.resolve().then(() => onScheduledTaskResult?.(item.scheduledTask, status, error))
      .catch((failure) => console.warn('[message-queue] scheduled task state write failed:', failure?.message ?? failure));
  };

  const setQueueItems = (sessionId, directory, items, status = 'cancelled', error) => {
    for (const item of queues.get(sessionId)?.items ?? []) {
      if (item.scheduledTask && !items.some((entry) => entry.id === item.id)) reportScheduledResult(item, status, error);
      if (item.requestId && !items.some((entry) => entry.id === item.id)) {
        const record = requests.get(item.requestId);
        record.state = status === 'sent' ? 'dispatched' : status === 'taken' ? 'taken' : status === 'deleted' ? 'deleted' : 'removed';
        delete record.selection;
      }
    }
    directories.set(sessionId, directory);
    if (items.length === 0) {
      queues.delete(sessionId);
      return;
    }
    queues.set(sessionId, { directory, items });
  };

  // --- OpenCode access -----------------------------------------------------

  const openCodeFetch = async (fetchPath, { directory, method = 'GET', body, streamBody, query } = {}) => {
    const base = buildOpenCodeUrl(fetchPath, '');
    const params = new URLSearchParams(query || {});
    if (directory) params.set('directory', directory);
    const search = params.toString();
    const headers = { Accept: 'application/json', ...getOpenCodeAuthHeaders() };
    const controller = streamBody ? new AbortController() : null;
    const deadline = AbortSignal.timeout(FETCH_TIMEOUT_MS);
    const init = { method, headers, signal: controller ? AbortSignal.any([deadline, controller.signal]) : deadline };
    if (body) {
      headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(body);
    }
    if (streamBody) {
      headers['Content-Type'] = 'application/json';
      headers['Content-Length'] = String(streamBody.contentLength);
      init.body = streamBody.createStream(init.signal);
      init.duplex = 'half';
    }
    try {
      const response = await fetchImpl(search ? `${base}?${search}` : base, init);
      if (!response.ok) {
        const detail = await response.text().catch(() => '');
        throw httpError(`OpenCode ${method} ${fetchPath} failed with ${response.status}${detail ? `: ${detail.slice(0, 200)}` : ''}`, response.status);
      }
      if (method === 'POST') {
        await assertPromptResponse(response, 'OpenCode dispatch');
      }
      return unwrapOpenCodeResponse(await response.json().catch(() => null));
    } finally {
      controller?.abort();
      if (streamBody) await init.body.cancel().catch(() => {});
    }
  };

  /**
   * Live idleness, or null when it could not be established. Unknown is never
   * idle: a fetch failure re-arms instead of sending into a running turn.
   */
  const activityProbe = createSessionActivityProbe({ buildOpenCodeUrl, getOpenCodeAuthHeaders, timeoutMs: FETCH_TIMEOUT_MS, fetchImpl });

  /** True while a subagent of the session runs; null when it could not be checked. */
  const hasWorkingSubagents = async (sessionId) => {
    const statuses = await activityProbe.fetchActiveSessionStatuses();
    if (!statuses) return null;
    return activityProbe.hasWorkingDescendants(sessionId, statuses);
  };

  const isSessionIdle = async (sessionId, directory) => {
    // `/api/session/active` is global and lists only the sessions that are
    // running right now, so an absent entry means idle.
    // The route answers `{ data: { [id]: { type: 'running' } } }`; the shared
    // unwrap hands over the map, and an envelope is still accepted.
    const body = asRecord(await openCodeFetch('/api/session/active').catch(() => null));
    const statuses = asRecord(body && 'data' in body ? body.data : body);
    if (!statuses) return null;
    if (asRecord(statuses[sessionId])) return false;
    // A missed event leaves no entry while a turn still streams. The trailing
    // unfinished assistant message is the live evidence of that turn (mirrors
    // the UI gate). v2 lists messages newest first.
    const page = await openCodeFetch(`/api/session/${encodeURIComponent(sessionId)}/message`, {
      directory,
      query: { limit: String(MESSAGE_TAIL_LIMIT) },
    }).catch(() => null);
    const messages = asList(asRecord(page)?.data);
    if (!messages) return null;
    const last = asRecord(messages[0]);
    const lastTime = asRecord(last?.time);
    if (last?.type === 'assistant' && asCount(lastTime?.completed) === null) {
      const created = asCount(lastTime?.created);
      if (created === null || created >= runtimeStartedAt) return false;
      // Unfinished tail from before this runtime started: its run died with
      // the previous server, so it must not block delivery. (A missing
      // created timestamp stays conservative and blocks, as before.)
      console.log(`[message-queue] ignoring pre-boot unfinished tail for ${sessionId}`);
    }
    return true;
  };

  const resolveSlashCommand = async (text, directory, scoped = false) => {
    if (!text.startsWith('/')) return null;
    const [head, ...tail] = text.split(' ');
    const name = head.slice(1);
    if (!name) return null;
    const response = await openCodeFetch('/api/command', scoped ? { query: { 'location[directory]': directory } } : { directory });
    const commands = scoped ? z.array(z.object({ name: z.string().min(1) })).parse(response) : asList(response) ?? [];
    const match = commands.map(asRecord).find((command) => command?.name === name);
    if (!match) return null;
    return {
      name,
      arguments: tail.join(' '),
    };
  };

  // v2 takes prompt attachments as URIs; a data URL is one.
  const toPromptFile = (attachment) => {
    const file = { uri: attachment.dataUrl };
    const name = attachment.agentPath || attachment.filename;
    if (name) file.name = name;
    return file;
  };

  /**
   * Captured context travels as synthetic messages in v2 — the composer's
   * inline `synthetic: true` text parts are gone. One message per entry, an
   * attached item's metadata riding along, and its reading instructions (a
   * linked PR) going first.
   */
  const toContextMessages = (part) => {
    const body = { text: part.text, resume: false };
    if (part.kind !== 'context') return [body];
    const withMetadata = part.metadata ? { ...body, metadata: part.metadata } : body;
    return part.instructions
      ? [{ text: part.instructions, resume: false }, withMetadata]
      : [withMetadata];
  };
  const capturedContext = (item) => item.format === 'payload-v1' ? item.payloadIndex.contextIds.map((id) => ({ id })) : [
    ...item.context.flatMap(toContextMessages),
    ...(item.promptParts?.context ?? []).map((entry) => ({ ...entry, resume: false })),
  ];
  const prefixContextIds = (reservation, items, ledger = requests) => {
    const ids = [];
    const prefix = `msg_${createHash('sha256').update(reservation.id).digest('hex').slice(0, 24)}_`;
    const append = (id) => ids.push(id ?? `${prefix}${String(ids.length).padStart(6, '0')}`);
    for (const item of items) {
      for (const [index, context] of capturedContext(item).entries()) append(context.id ?? ledger.get(item.requestId)?.receipt.contextIds[index]);
    }
    for (const context of reservation.promptParts?.context ?? []) append(context.id);
    for (const id of reservation.format === 'payload-v1' ? reservation.composer.payloadIndex.contextIds : []) append(id);
    return ids;
  };
  const reservedMessageIds = (exceptSessionId) => {
    const ids = new Set();
    for (const [sessionId, reservation] of reservations) {
      if (sessionId === exceptSessionId) continue;
      ids.add(reservation.id);
      if (!reservation.format) continue;
      for (const id of prefixContextIds(reservation, queues.get(sessionId).items.slice(0, reservation.itemIds.length))) ids.add(id);
    }
    return ids;
  };

  const sameContextOwner = (left, right) => left.sessionId === right.sessionId && left.directory === right.directory;
  const itemOwnerKey = (owner, itemId) => JSON.stringify([owner.sessionId, owner.directory, itemId]);
  const legacyContextNamespace = (owner, itemId) => `msg_${createHash('sha256').update(itemOwnerKey(owner, itemId)).digest('hex').slice(0, 24)}_`;
  const transferredContextIds = (item, ledger = requests) => capturedContext(item)
    .map((part, index) => part.id ?? ledger.get(item.requestId)?.receipt.contextIds[index]).filter(Boolean);
  const indexItemTakes = (ledger) => {
    const takes = new Map();
    for (const record of ledger.values()) {
      if (record.kind !== 'take') continue;
      for (const id of record.receipt.itemIds) {
        const key = itemOwnerKey(record.receipt, id);
        takes.set(key, [...(takes.get(key) ?? []), record]);
      }
    }
    return takes;
  };
  const acknowledgedItemTake = (receipt, takes) => {
    const matches = takes.get(itemOwnerKey(receipt, receipt.itemId)) ?? [];
    if (matches.length > 1) throw new TypeError('stored context has ambiguous take custody');
    return matches[0]?.state === 'acknowledged' ? matches[0] : null;
  };
  const deriveContextOwners = (ledger, sessions) => {
    const owners = new Map();
    const takes = indexItemTakes(ledger);
    for (const record of ledger.values()) {
      if (record.kind !== 'enqueue') continue;
      const { receipt } = record;
      const take = record.state === 'taken' ? acknowledgedItemTake(receipt, takes) : null;
      for (const id of receipt.contextIds) {
        if (owners.has(id)) throw new TypeError('stored queue request reused a message identity without custody evidence');
        const claims = [{ kind: 'enqueue', requestId: receipt.requestId }];
        if (take) claims.push({ kind: 'take', requestId: take.receipt.requestId, itemId: receipt.itemId });
        owners.set(id, { sessionId: receipt.sessionId, directory: receipt.directory, claims });
      }
    }
    for (const record of ledger.values()) {
      if (record.kind !== 'take' || record.state !== 'transferring') continue;
      for (const item of record.items) {
        if (item.requestId) continue;
        for (const id of transferredContextIds(item, ledger)) {
          if (owners.has(id)) throw new TypeError('stored take reused another context identity');
          owners.set(id, { sessionId: record.receipt.sessionId, directory: record.receipt.directory,
            claims: [{ kind: 'take', requestId: record.receipt.requestId, itemId: item.id }] });
        }
      }
    }
    for (const [sessionId, queue] of Object.entries(sessions)) {
      const reservation = queue.reservation;
      if (!reservation?.format) continue;
      const selected = new Set(queue.items.slice(0, reservation.itemIds.length).map((item) => item.requestId));
      for (const id of prefixContextIds(reservation, queue.items.slice(0, reservation.itemIds.length), ledger)) {
        const owner = owners.get(id) ?? { sessionId, directory: queue.directory, claims: [] };
        const current = owner.claims.at(-1);
        if (!sameContextOwner(owner, { sessionId, directory: queue.directory })
            || (current && !(current.kind === 'enqueue' && selected.has(current.requestId)))) {
          throw new TypeError('stored prefix has conflicting context custody');
        }
        owner.claims.push({ kind: 'prefix', id: reservation.id });
        owners.set(id, owner);
      }
    }
    return owners;
  };
  const derivePrefixPromptOwners = (owners, sessions) => {
    const prompts = new Map();
    const retain = (id, owner, state) => {
      const previous = prompts.get(id);
      if (previous && (!sameContextOwner(previous, owner) || previous.state !== state)) {
        throw new TypeError('stored prefix prompt has conflicting ownership');
      }
      prompts.set(id, { sessionId: owner.sessionId, directory: owner.directory, state });
    };
    for (const owner of owners.values()) {
      const claim = owner.claims.at(-1);
      if (claim.kind !== 'prefix') continue;
      const queue = sessions[owner.sessionId];
      const active = queue?.reservation?.format && queue.reservation.id === claim.id && queue.directory === owner.directory;
      retain(claim.id, owner, active ? 'reserved' : 'consumed');
    }
    for (const [sessionId, queue] of Object.entries(sessions)) {
      if (queue.reservation?.format) retain(queue.reservation.id, { sessionId, directory: queue.directory }, 'reserved');
    }
    return prompts;
  };
  const validatePrefixPromptOwners = (prompts, owners, ledger, sessions) => {
    const invalid = () => { throw new TypeError('stored prefix prompt ownership does not match its reservation'); };
    const enqueuePrompts = new Set();
    for (const record of ledger.values()) {
      if (record.kind === 'enqueue') enqueuePrompts.add(record.receipt.messageId);
    }
    const active = new Map();
    for (const [sessionId, queue] of Object.entries(sessions)) {
      if (!queue.reservation) continue;
      const id = queue.reservation.id;
      if (active.has(id)) invalid();
      active.set(id, { sessionId, directory: queue.directory, reservation: queue.reservation });
      if (!queue.reservation.format) continue;
      const owner = prompts.get(id);
      if (!owner || owner.state !== 'reserved' || !sameContextOwner(owner, { sessionId, directory: queue.directory })) invalid();
    }
    for (const [id, owner] of prompts) {
      if (owners.has(id) || enqueuePrompts.has(id)) invalid();
      const current = active.get(id);
      if (owner.state === 'reserved'
        ? !current?.reservation.format || !sameContextOwner(current, owner)
        : current !== undefined) invalid();
    }
  };
  const validateContextOwners = (owners, ledger, sessions, prefixPrompts) => {
    const invalid = () => { throw new TypeError('stored context ownership does not match its custody evidence'); };
    const promptIds = new Set();
    const takes = indexItemTakes(ledger);
    // These indexes belong to this one load. Receipt/claim membership must not
    // rescan a 4096-entry capture for every historical custody edge.
    const enqueueContexts = new Map();
    const enqueueClaims = new Map();
    const enqueueItems = new Set();
    const takeItems = new Map();
    for (const [id, owner] of owners) {
      for (const claim of owner.claims) {
        if (claim.kind !== 'enqueue') continue;
        let ids = enqueueClaims.get(claim.requestId);
        if (!ids) { ids = new Set(); enqueueClaims.set(claim.requestId, ids); }
        ids.add(id);
      }
    }
    for (const record of ledger.values()) {
      if (record.kind === 'enqueue') {
        enqueueContexts.set(record.receipt.requestId, new Set(record.receipt.contextIds));
        enqueueItems.add(itemOwnerKey(record.receipt, record.receipt.itemId));
      } else {
        const retained = new Map();
        for (const item of record.items ?? []) {
          const contexts = capturedContext(item);
          const ids = contexts.map((part, index) => part.id ?? ledger.get(item.requestId)?.receipt.contextIds[index]).filter(Boolean);
          retained.set(item.id, { item, contexts, ids, contextIds: new Set(ids) });
        }
        takeItems.set(record.receipt.requestId, { itemIds: new Set(record.receipt.itemIds), retained });
      }
    }
    for (const record of ledger.values()) {
      if (record.kind !== 'enqueue') continue;
      if (promptIds.has(record.receipt.messageId) || owners.has(record.receipt.messageId)) invalid();
      promptIds.add(record.receipt.messageId);
      for (const id of record.receipt.contextIds) {
        const owner = owners.get(id);
        if (!owner || !sameContextOwner(owner, record.receipt)
            || !enqueueClaims.get(record.receipt.requestId)?.has(id)) invalid();
      }
    }
    for (const [id, owner] of owners) {
      // There can be one enqueue and one take edge per retained request, plus
      // one final prefix. No identity history expires while receipts remain.
      if (owner.claims.length > ledger.size + 1) invalid();
      const seen = new Set();
      for (const [index, claim] of owner.claims.entries()) {
        const previous = owner.claims[index - 1];
        const next = owner.claims[index + 1];
        const key = `${claim.kind}:${claim.requestId ?? claim.id}`;
        if (seen.has(key)) invalid();
        seen.add(key);
        if (claim.kind === 'prefix') {
          if (next || owners.has(claim.id) || promptIds.has(claim.id)) invalid();
          const promptOwner = prefixPrompts.get(claim.id);
          if (!promptOwner || !sameContextOwner(promptOwner, owner)) invalid();
          const queue = sessions[owner.sessionId];
          const active = queue?.reservation?.id === claim.id;
          if (previous?.kind === 'enqueue') {
            const source = ledger.get(previous.requestId);
            if (source.attempt !== 0 || (active
              ? source.state !== 'queued' || !queue.reservation.itemIds.includes(source.receipt.itemId)
              : !['taken', 'deleted'].includes(source.state))) invalid();
          } else if (previous && previous.kind !== 'take') invalid();
          continue;
        }
        const record = ledger.get(claim.requestId);
        if (!record || record.kind !== claim.kind || !sameContextOwner(owner, record.receipt)) invalid();
        if (claim.kind === 'enqueue') {
          if (!enqueueContexts.get(claim.requestId).has(id) || (previous && previous.kind !== 'take')) invalid();
          if (next?.kind === 'take' && (record.state !== 'taken' || record.attempt !== 0)) invalid();
          if (!next && ['queued', 'uncertain'].includes(record.state)
              && !sessions[owner.sessionId]?.items.some((item) => item.requestId === claim.requestId)) invalid();
          if (!next && record.state === 'taken' && acknowledgedItemTake(record.receipt, takes)) invalid();
        } else {
          parseRequestId(claim.requestId);
          const indexed = takeItems.get(claim.requestId);
          if (!indexed.itemIds.has(claim.itemId) || (record.state !== 'acknowledged' && (previous || next))) invalid();
          if (previous && (previous.kind !== 'enqueue' || ledger.get(previous.requestId).receipt.itemId !== claim.itemId)) invalid();
          if (!previous) {
            if (enqueueItems.has(itemOwnerKey(owner, claim.itemId))) invalid();
            if (record.state === 'transferring') {
              const retained = indexed.retained.get(claim.itemId);
              if (!retained || retained.item.requestId || !retained.contextIds.has(id)) invalid();
            } else {
              // Once an old legacy take dropped its payload, only its exact
              // deterministic owner/item namespace remains provable.
              const prefix = legacyContextNamespace(owner, claim.itemId);
              if (!id.startsWith(prefix) || !/^\d{6}$/.test(id.slice(prefix.length))) invalid();
            }
          }
          if (next && !['enqueue', 'prefix'].includes(next.kind)) invalid();
        }
      }
    }
    for (const record of ledger.values()) {
      if (record.kind !== 'take' || record.state !== 'transferring') continue;
      for (const { item, contexts, ids } of takeItems.get(record.receipt.requestId).retained.values()) {
        const source = ledger.get(item.requestId);
        if (item.requestId && (!source || source.kind !== 'enqueue' || source.state !== 'taken' || source.attempt !== 0
            || !sameContextOwner(source.receipt, record.receipt) || source.receipt.itemId !== item.id
            || source.receipt.contextIds.length !== contexts.length
            || contexts.some((part, index) => part.id !== undefined && part.id !== source.receipt.contextIds[index]))) invalid();
        for (const id of ids) {
          const owner = owners.get(id);
          const current = owner?.claims.at(-1);
          if (!owner || !sameContextOwner(owner, record.receipt) || (source
            ? current.kind !== 'enqueue' || current.requestId !== item.requestId
            : current.kind !== 'take' || current.requestId !== record.receipt.requestId || current.itemId !== item.id)) invalid();
        }
      }
    }
    for (const [sessionId, queue] of Object.entries(sessions)) {
      const reservation = queue.reservation;
      for (const item of queue.items) {
        const source = ledger.get(item.requestId);
        if (!source) continue;
        for (const id of source.receipt.contextIds) {
          const current = owners.get(id)?.claims.at(-1);
          if (!(current?.kind === 'enqueue' && current.requestId === item.requestId)
              && !(reservation?.format && reservation.itemIds.includes(item.id) && current?.kind === 'prefix' && current.id === reservation.id)) invalid();
        }
      }
      if (!reservation?.format) continue;
      const ids = prefixContextIds(reservation, queue.items.slice(0, reservation.itemIds.length), ledger);
      if (new Set([reservation.id, ...ids]).size !== ids.length + 1) invalid();
      for (const id of ids) {
        const owner = owners.get(id);
        const current = owner?.claims.at(-1);
        if (!owner || !sameContextOwner(owner, { sessionId, directory: queue.directory })
            || current.kind !== 'prefix' || current.id !== reservation.id) invalid();
      }
    }
  };
  const captureContextClaims = (owner, ids, claim, selectedRequests = new Set(), recovering = false) => {
    const conflict = () => { throw queueRequestError('captured context identity is already owned', 409, 'queue_request_conflict'); };
    const promptIds = new Set();
    const enqueueItems = new Set();
    for (const record of requests.values()) {
      if (record.kind !== 'enqueue') continue;
      promptIds.add(record.receipt.messageId);
      enqueueItems.add(itemOwnerKey(record.receipt, record.receipt.itemId));
    }
    const unprovenLegacyNamespaces = new Set();
    for (const record of requests.values()) {
      if (record.kind !== 'take' || record.state !== 'acknowledged') continue;
      for (const itemId of record.receipt.itemIds) {
        const seed = itemOwnerKey(record.receipt, itemId);
        if (!enqueueItems.has(seed)) unprovenLegacyNamespaces.add(legacyContextNamespace(record.receipt, itemId));
      }
    }
    for (const id of prefixPromptOwners.keys()) promptIds.add(id);
    const updates = new Map();
    for (const id of ids) {
      if (promptIds.has(id)) conflict();
      const retained = contextOwners.get(id);
      if (!retained) {
        // Old acknowledged legacy takes discarded their payload before this
        // ledger existed. Their deterministic namespace remains excluded, but
        // no missing context count or custody proof can be reconstructed.
        if (/^msg_[a-f0-9]{24}_\d{6}$/.test(id) && unprovenLegacyNamespaces.has(id.slice(0, -6))) conflict();
        updates.set(id, { ...owner, claims: [claim] });
        continue;
      }
      if (!sameContextOwner(retained, owner) || retained.claims.some((entry) => entry.requestId && pendingRequests.has(entry.requestId))) conflict();
      const current = retained.claims.at(-1);
      if (recovering && claim.kind === 'prefix' && current.kind === 'prefix' && current.id === claim.id) continue;
      if (current.kind === 'take') {
        if (requests.get(current.requestId)?.state !== 'acknowledged') conflict();
      } else if (!(claim.kind === 'prefix' && current.kind === 'enqueue' && selectedRequests.has(current.requestId)
          && requests.get(current.requestId)?.state === 'queued' && requests.get(current.requestId)?.attempt === 0)) conflict();
      updates.set(id, { ...retained, claims: [...retained.claims, claim] });
    }
    return updates;
  };
  const applyContextClaims = (updates) => {
    for (const [id, owner] of updates) contextOwners.set(id, owner);
  };
  const consumePrefixPrompt = (reservation, sessionId, directory) => {
    if (!reservation.format) return;
    const owner = prefixPromptOwners.get(reservation.id);
    if (!owner || owner.state !== 'reserved' || !sameContextOwner(owner, { sessionId, directory })) {
      throw new TypeError('prefix prompt lost its reserved ownership');
    }
    prefixPromptOwners.set(reservation.id, { ...owner, state: 'consumed' });
  };

  const preparePrompt = (item, receipt) => {
    const fileReferences = item.promptParts?.fileReferences ?? [];
    const promptFiles = [...item.attachments.map(toPromptFile), ...fileReferences];
    // A JSON request body whose key order is also measured into admission
    // bytes: absent parts stay absent.
    const promptBody = {};
    if (receipt) Object.assign(promptBody, { id: receipt.messageId, delivery: 'queue' });
    promptBody.text = item.text;
    if (promptFiles.length && item.format !== 'manifest-v1') promptBody.files = promptFiles;
    if (item.agentMention) promptBody.agents = [{ name: item.agentMention }];
    if (item.promptParts?.agents.length) promptBody.agents = item.promptParts.agents;
    if (item.promptParts?.skills.length) promptBody.skills = item.promptParts.skills;
    if (item.promptParts?.metadata !== undefined) promptBody.metadata = item.promptParts.metadata;
    const streamBody = item.format === 'manifest-v1' ? createManifestPromptBody(promptBody, item.attachmentManifest,
      (file, signal) => blobStore.read(blobs.get(file.blobId).metadata, signal), fileReferences) : null;
    return { promptFiles, promptBody, streamBody };
  };

  const sendPreparedItem = async (sessionId, directory, item) => {
    const request = requests.get(item.requestId);
    const receipt = request?.receipt;
    const { providerID, modelID, variant } = item.sendConfig;
    let agent = item.sendConfig.agent;
    let model = { id: modelID, providerID };
    if (variant) model.variant = variant;
    const manifest = item.format === 'manifest-v1';
    if (manifest) validateManifestReferences(item, sessionId, directory);
    const { promptFiles, promptBody, streamBody } = preparePrompt(item, receipt);
    if (streamBody && streamBody.contentLength > MANIFEST_PROMPT_BYTE_LIMIT) {
      if (request) request.blocked = 'prompt_body_too_large';
      assertManifestPromptBudget(streamBody.contentLength);
    }
    const contextMessages = capturedContext(item);

    // Jev routing, when the queued send named the Auto sentinel. A failure
    // inside resolves to the fallback model; only a missing fallback throws.
    const routed = request?.selection ?? (providerID && modelID ? await resolveAutoSelection?.({
      sessionId,
      directory,
      model,
      agent,
      // Routing needs media types and display names, never attachment bytes.
      requestText: `${item.text}${attachmentHint({ files: manifest
        ? item.attachmentManifest.map((file) => ({ uri: `data:${file.mimeType};base64,`, name: file.agentPath ?? file.filename })) : promptFiles })}`.trim(),
    }) : null);
    if (routed) {
      model = routed.model;
      agent = routed.agent ?? agent;
    }
    if (request && !request.selection) {
      request.selection = { model };
      if (agent) request.selection.agent = agent;
      await persistRequest(item.requestId);
    }

    // v2 selects model and agent on the session, not per prompt: the choice is
    // switched once and then persists.
    if (!item.scheduledTask || (providerID && modelID)) await openCodeFetch(`/api/session/${encodeURIComponent(sessionId)}/model`, {
      directory,
      method: 'POST',
      body: { model },
    });
    if (agent) {
      await openCodeFetch(`/api/session/${encodeURIComponent(sessionId)}/agent`, {
        directory,
        method: 'POST',
        body: { agent },
      });
    }

    // Resolved before anything is admitted: a failed command lookup fails the
    // whole send, so a retry does not admit the context twice.
    const command = await resolveSlashCommand(item.text, directory, Boolean(receipt));
    if (receipt && command) {
      request.blocked = 'command_identity_unsupported';
      throw queueRequestError('OpenCode commands do not support a stable delivery identity', 409, 'command_identity_unsupported');
    }

    // Standing project context is written into the session first, exactly as
    // a UI send does it; a failed write sends without it rather than not at all.
    // The response style is left out: a queued message is the user's own, and
    // this server delivery has no style text of its own, so the entry the
    // session already holds stays as it is.
    if (sessionKnowledgeRuntime) await sessionKnowledgeRuntime.sync(sessionId, directory);

    // A message queued as a goal starts it now, as a direct send would. The
    // goal is saved before anything is admitted, so a failed save leaves
    // nothing in the session for the retry to duplicate.
    if (item.goal) {
      await createSessionGoal({
        persistSessionGoal,
        sessionID: sessionId,
        directory,
        objective: item.goal.objective || item.text,
        tokenBudget: item.goal.tokenBudget,
        providerID: model.providerID,
        modelID: model.id,
        onWarning: (message, error) => console.warn(`[message-queue] ${message}:`, error?.message || error),
      });
    }

    // Same order as a UI send: everything attached to the message is admitted
    // before the message itself, so the model reads it as background. This
    // holds for a command too: its route takes file attachments only, and
    // sending "/name args" as a prompt instead would skip the template
    // OpenCode 2.x expands only on the command route.
    for (const [index, synthetic] of contextMessages.entries()) {
      if (request?.attempt > 0 && await observeAdmission(sessionId, directory, receipt.contextIds[index], 'synthetic')) continue;
      const body = { ...synthetic };
      if (receipt) Object.assign(body, { id: receipt.contextIds[index], delivery: 'queue' });
      const admitted = await openCodeFetch(`/api/session/${encodeURIComponent(sessionId)}/synthetic`, {
        directory,
        method: 'POST',
        body,
      });
      if (receipt) requireAdmission(admitted, sessionId, receipt.contextIds[index], 'synthetic');
    }

    // The goal briefing goes last, right before the message the goal is about.
    if (item.goal) {
      await openCodeFetch(`/api/session/${encodeURIComponent(sessionId)}/synthetic`, {
        directory,
        method: 'POST',
        body: { text: buildGoalIntroText(item.goal.tokenBudget), resume: false },
      });
    }

    if (command) {
      // OpenCode 2.0.8 renamed the command body field `command` to `name`.
      const body = { name: command.name, text: command.arguments };
      if (promptFiles.length > 0) body.files = promptFiles;
      await openCodeFetch(`/api/session/${encodeURIComponent(sessionId)}/command`, {
        directory,
        method: 'POST',
        body,
      });
    } else {
      const admitted = await openCodeFetch(`/api/session/${encodeURIComponent(sessionId)}/prompt`, {
        directory,
        method: 'POST',
        ...(streamBody ? { streamBody } : { body: promptBody }),
      });
      if (receipt) requireAdmission(admitted, sessionId, receipt.messageId, 'user');
    }
  };

  const sendItem = async (sessionId, directory, item) => {
    const receipt = requests.get(item.requestId)?.receipt;
    if (requests.get(item.requestId)?.attempt > 0 && await observeAdmission(sessionId, directory, receipt.messageId, 'user')) return;
    if (item.format !== 'payload-v1') return sendPreparedItem(sessionId, directory, item);
    return withPayloadOperation(async () => {
      const logical = await materializePayload(item, sessionId, directory, receipt);
      if (item.payloadIndex.admissionBytes > MANIFEST_PROMPT_BYTE_LIMIT) {
        if (requests.has(item.requestId)) requests.get(item.requestId).blocked = 'prompt_body_too_large';
        assertManifestPromptBudget(item.payloadIndex.admissionBytes);
      }
      await sendPreparedItem(sessionId, directory, { ...logical, ...itemIdentity(item) });
    });
  };

  const requireAdmission = (value, sessionId, messageId, type) => {
    const parsed = (type === 'user' ? inboxUserSchema : inboxSyntheticSchema).parse(value);
    if (parsed.id !== messageId || parsed.sessionID !== sessionId || parsed.delivery !== 'queue') throw new TypeError('conflicting delivery admission');
  };

  const observeAdmission = async (sessionId, directory, messageId, type) => {
    const prefix = `/api/session/${encodeURIComponent(sessionId)}`;
    const inbox = await openCodeFetch(`${prefix}/inbox`, { directory });
    if (!Array.isArray(inbox)) throw new TypeError('invalid delivery inbox');
    const matching = inbox.filter((entry) => entry?.id === messageId);
    if (matching.length) {
      if (matching.length !== 1) throw new TypeError('duplicate delivery inbox identity');
      requireAdmission(matching[0], sessionId, messageId, type);
      return true;
    }
    let message;
    try {
      message = await openCodeFetch(`${prefix}/message/${encodeURIComponent(messageId)}`, { directory });
    } catch (error) {
      if (error?.status === 404) return false;
      throw error;
    }
    const parsed = (type === 'user' ? historyUserSchema : historySyntheticSchema).parse(message);
    if (parsed.id !== messageId || (message.sessionID !== undefined && message.sessionID !== sessionId)) throw new TypeError('conflicting delivery history identity');
    return true;
  };

  // --- dispatch loop -------------------------------------------------------

  const clearTimer = (sessionId) => {
    const existing = timers.get(sessionId);
    if (existing) {
      clearTimeout(existing);
      timers.delete(sessionId);
    }
  };

  const armDispatch = (sessionId, delayMs = dispatchQuietMs) => {
    if (stopped || !queues.has(sessionId)) return;
    clearTimer(sessionId);
    const timer = setTimeout(() => {
      timers.delete(sessionId);
      tick(sessionId).catch((error) => {
        console.warn('[message-queue] dispatch tick failed:', error?.message ?? error);
      });
    }, Math.max(0, delayMs));
    timer.unref?.();
    timers.set(sessionId, timer);
  };

  async function tick(sessionId) {
    if (stopped) return;
    if (reservations.has(sessionId)) {
      await reconcileReservation(sessionId);
      return;
    }
    const queue = queues.get(sessionId);
    if (!queue || queue.items.length === 0 || sending.has(sessionId) || preparingTransfers.has(sessionId) || isDeliveryHeld(sessionId)) return;

    const abortHoldUntil = (abortedAt.get(sessionId) ?? 0) + abortHoldMs;
    if (abortHoldUntil > now()) {
      armDispatch(sessionId, abortHoldUntil - now());
      return;
    }

    const head = queue.items[0];
    const request = requests.get(head.requestId);
    if (request && (pendingRequests.has(head.requestId) || request.state !== 'queued')) return;
    if (request?.blocked === 'prompt_body_too_large' && request.attempt === 0) return;
    if (request?.attempt === 0 && head.format
        && (head.format === 'payload-v1' ? head.payloadIndex.admissionBytes : preparePrompt(head, request.receipt).streamBody.contentLength) > MANIFEST_PROMPT_BYTE_LIMIT) {
      // A restored never-attempted item can predate the size policy. Refuse
      // before write-ahead uncertainty, routing or any OpenCode call.
      request.blocked = 'prompt_body_too_large';
      revision += 1;
      await persistRequest(head.requestId);
      broadcast(sessionId);
      return;
    }
    const failure = failures.get(sessionId);
    if (failure && failure.itemId !== head.id) failures.delete(sessionId);
    else if (failure && failure.nextAttemptAt > now()) {
      armDispatch(sessionId, failure.nextAttemptAt - now());
      return;
    }

    const idle = await isSessionIdle(sessionId, queue.directory);
    if (idle === null) {
      if (head.scheduledTask && validateScheduledTaskTarget) {
        try {
          await validateScheduledTaskTarget(head.scheduledTask.projectId, sessionId);
        } catch (error) {
          if (error?.statusCode === 404 || error?.statusCode === 409) {
            const after = queues.get(sessionId);
            if (after) setQueueItems(sessionId, after.directory, after.items.filter((entry) => entry.id !== head.id), 'failed', error.message);
            commit(sessionId);
            armDispatch(sessionId);
            return;
          }
        }
      }
      armDispatch(sessionId, retryDelayMs(1));
      return;
    }
    // Busy: the next idle status event re-arms the loop.
    if (!idle) return;

    // A queued message waits for the whole turn. A parent idles while a
    // background subagent works and runs again when OpenCode hands the
    // result back, so the turn is only over once no subagent runs. That rerun
    // re-arms through its idle event; the recheck covers a missed one.
    const subagentsWorking = await hasWorkingSubagents(sessionId);
    if (subagentsWorking !== false) {
      armDispatch(sessionId, subagentsWorking === null ? retryDelayMs(1) : SUBAGENT_RECHECK_MS);
      return;
    }

    // Re-read after the awaits — the user may have edited the queue meanwhile.
    const current = queues.get(sessionId);
    const item = current?.items[0];
    if (!item || item.id !== head.id || sending.has(sessionId) || reservations.has(sessionId) || preparingTransfers.has(sessionId)
        || (request && (pendingRequests.has(item.requestId) || request.state !== 'queued'))) return;

    sending.set(sessionId, item.id);
    broadcast(sessionId);
    try {
      if (request) {
        // Write-ahead uncertainty closes the crash window between the first
        // byte sent to OpenCode and a local delivery checkpoint. A restart or
        // failed checkpoint always holds this identity for explicit retry.
        request.state = 'uncertain';
        revision += 1;
        await persistRequest(item.requestId);
        if (queues.get(sessionId)?.items[0] !== item || reservations.has(sessionId)) {
          sending.delete(sessionId);
          return;
        }
      }
      if (item.scheduledTask) {
        try {
          await beforeScheduledTaskSend?.(sessionId, current.directory, item);
        } catch (error) {
          if (error?.statusCode !== 404 && error?.statusCode !== 409) throw error;
          sending.delete(sessionId);
          const after = queues.get(sessionId);
          if (after) setQueueItems(sessionId, after.directory, after.items.filter((entry) => entry.id !== item.id), 'failed', error.message);
          failures.delete(sessionId);
          commit(sessionId);
          armDispatch(sessionId);
          return;
        }
      }
      await sendItem(sessionId, current.directory, item);
      const after = queues.get(sessionId);
      if (after) setQueueItems(sessionId, after.directory, after.items.filter((entry) => entry.id !== item.id), 'sent');
      failures.delete(sessionId);
      if (request) {
        revision += 1;
        await persistRequest(item.requestId);
        sending.delete(sessionId);
        broadcast(sessionId);
        armDispatch(sessionId);
      } else {
        sending.delete(sessionId);
        commit(sessionId);
      }
      try {
        onPromptSent?.(sessionId);
      } catch {
        // bookkeeping only
      }
      console.log(`[message-queue] sent queued message to ${sessionId}`);
    } catch (error) {
      if (request) {
        // Even a refusal before prompt admission needs the user's retry. No
        // transient send failure changes these captured message identities.
        revision += 1;
        const recovered = await persistRequest(item.requestId).then(() => true, () => false);
        sending.delete(sessionId);
        broadcast(sessionId);
        if (recovered && request.state === 'dispatched') armDispatch(sessionId);
        return;
      }
      sending.delete(sessionId);
      const count = (failure?.itemId === item.id ? failure.failures : 0) + 1;
      const nextAttemptAt = now() + retryDelayMs(count);
      failures.set(sessionId, { itemId: item.id, failures: count, nextAttemptAt });
      console.warn(`[message-queue] send to ${sessionId} failed (attempt ${count}):`, error?.message ?? error);
      broadcast(sessionId);
      armDispatch(sessionId, nextAttemptAt - now());
    }
  }

  const reconcileAll = () => {
    for (const sessionId of queues.keys()) {
      if (!timers.has(sessionId)) armDispatch(sessionId, dispatchQuietMs);
    }
  };

  // --- public mutations ----------------------------------------------------

  const requireSessionId = (sessionId) => {
    if (!isValidSessionId(sessionId)) throw new TypeError('sessionId is invalid');
    return sessionId;
  };

  const persistRequest = (requestId) => {
    pendingRequests.add(requestId);
    const write = persist().then(() => {
      if (requestWrites.get(requestId) === write) pendingRequests.delete(requestId);
    });
    requestWrites.set(requestId, write);
    // A rejected write keeps exclusion. Only an exact repeat or an explicit
    // owner operation can retry persistence; timers never retry delivery.
    void write.finally(() => {
      if (requestWrites.get(requestId) === write) requestWrites.delete(requestId);
    }).catch(() => {});
    return write;
  };

  // Receipt recovery must remain bounded even when later queue items grow.
  // Queue projections use the existing snapshot and update routes separately.
  const requestResponse = (record) => ({ ...publicQueueRequest(record), revision });
  const canTransferItem = (item) => {
    const record = requests.get(item.requestId);
    return !record || (record.state === 'queued' && record.attempt === 0);
  };
  const requireOwnedRequest = (sessionId, requestId, directory) => {
    requireReceiptHistory();
    parseRequestId(requestId);
    const owner = parseRequestOwner(sessionId, directory);
    const record = requests.get(requestId);
    if (!record) throw queueRequestError('queue request receipt is unknown', 404, 'queue_request_unknown');
    if (record.receipt.sessionId !== owner.sessionId || record.receipt.directory !== owner.directory) {
      throw queueRequestError('requestId belongs to a different owner', 409, 'queue_request_conflict');
    }
    return record;
  };

  const requireReceiptHistory = () => {
    if (receiptRecoveryRequired) throw queueRequestError('queue receipt history requires recovery from the retained file', 503, 'queue_request_recovery_required');
  };

  const getRequest = async (sessionId, requestId, directory) => {
    await load();
    const record = requireOwnedRequest(sessionId, requestId, directory);
    if (pendingRequests.has(requestId)) {
      const unresolved = () => queueRequestError('queue request persistence is unresolved', 503, 'queue_request_pending');
      if (requestWrites.has(requestId)) throw unresolved();
      // An accepted caller may already have released its captured draft.
      // Lookup can recover a failed checkpoint without needing that body.
      try { await persistRequest(requestId); } catch { throw unresolved(); }
      if (record.kind === 'take' || ['queued', 'dispatched'].includes(record.state)) armDispatch(sessionId);
    }
    return requestResponse(record);
  };

  const enqueueRequestImpl = async (sessionId, requestId, captured) => {
    await load();
    requireReceiptHistory();
    let record = requests.get(requestId);
    if (record) {
      requireMatchingRequest(record, captured, 'enqueue');
      await persistRequest(requestId);
      if (['queued', 'dispatched'].includes(record.state)) armDispatch(sessionId);
      return requestResponse(record);
    }
    if ([...activeUploads.values()].some(({ metadata }) => metadata.requestId === requestId)) {
      throw queueRequestError('request attachment upload is in progress', 409, 'queue_request_pending');
    }
    if (captured.input.item.format !== undefined && !['manifest-v1', 'payload-v1'].includes(captured.input.item.format)) throw new TypeError('unsupported durable item format');
    if (captured.input.item.attachmentManifest !== undefined
        && captured.input.item.format !== 'manifest-v1') throw new TypeError('manifest payload requires manifest-v1');
    if ((captured.input.item.payloadManifest !== undefined || captured.input.item.payloadIndex !== undefined || captured.input.item.payloadIndexHash !== undefined)
        && captured.input.item.format !== 'payload-v1') throw new TypeError('structured payload requires payload-v1');
    const pointer = captured.input.item.format === 'payload-v1' ? parsePayloadPointer(captured.input.item) : null;
    const parsed = pointer ? await readPayload(pointer, sessionId, captured.directory)
      : captured.input.item.format === 'manifest-v1' ? parseManifestItem(captured.input.item)
        : captured.input.item.promptParts !== undefined ? parseStructuredItem(captured.input.item) : parseQueuedItemInput(captured.input.item);
    if (parsed.goal || parsed.scheduledTask) throw new TypeError('durable enqueue supports ordinary captured prompts only');
    if (parsed.attachments.some((file) => !/^data:[^,]*,/.test(file.dataUrl))) {
      throw new TypeError('durable enqueue requires inline attachment bytes in dataUrl');
    }
    const contexts = capturedContext(parsed);
    const count = contexts.length;
    if (count > 4096 || parsed.context.some((part) => !part.text.trim())) throw new TypeError('invalid durable context');
    requireUploadOwner(sessionId, requestId, captured.directory);
    if (parsed.format === 'manifest-v1' && !pointer) {
      await verifyManifestFiles(parsed, sessionId, captured.directory);
    }
    const queue = queues.get(sessionId);
    if (queue && queue.directory !== captured.directory) throw queueRequestError('queue belongs to another directory', 409, 'queue_request_conflict');
    if ((!queue && queues.size >= MAX_SESSIONS) || (queue?.items.length ?? 0) >= MAX_ITEMS_PER_SESSION) {
      throw httpError('message queue is full', 409);
    }
    let item = { id: `queued-${randomUUID()}`, createdAt: now(), ...parsed, requestId };
    const receipt = createEnqueueReceipt(captured, item, count, now(), contexts.map((entry) => entry.id));
    const ids = [receipt.messageId, ...receipt.contextIds];
    const reservedIds = reservedMessageIds();
    if (new Set(ids).size !== ids.length || ids.some((id) => reservedIds.has(id)) || contextOwners.has(receipt.messageId)
        || [...requests.values()].some((entry) => entry.kind === 'enqueue' && entry.receipt.messageId === receipt.messageId)
        || prefixPromptOwners.has(receipt.messageId)) {
      throw queueRequestError('captured context identity is already owned', 409, 'queue_request_conflict');
    }
    const contextClaims = captureContextClaims({ sessionId, directory: captured.directory }, receipt.contextIds, { kind: 'enqueue', requestId });
    record = { kind: 'enqueue', receipt, state: 'queued', attempt: 0 };
    if (item.format === 'manifest-v1' && preparePrompt(item, receipt).streamBody.contentLength > MANIFEST_PROMPT_BYTE_LIMIT) {
      record.blocked = 'prompt_body_too_large';
    }
    if (pointer) item = requireCompactPayload({ id: item.id, createdAt: item.createdAt, requestId, ...pointer,
      payloadIndex: indexPayload(parsed, captured.directory, receipt.contextIds, receipt) });
    if (pointer && item.payloadIndex.admissionBytes > MANIFEST_PROMPT_BYTE_LIMIT) record.blocked = 'prompt_body_too_large';
    applyContextClaims(contextClaims);
    requests.set(requestId, record);
    setQueueItems(sessionId, captured.directory, [...(queue?.items ?? []), item]);
    revision += 1;
    await persistRequest(requestId);
    broadcast(sessionId);
    armDispatch(sessionId);
    return requestResponse(record);
  };
  const enqueueRequest = async (sessionId, requestId, input) => {
    const captured = captureQueueRequest('enqueue', sessionId, requestId, input);
    return withBlobOwnership(() => captured.input.item.format === 'payload-v1'
      ? withPayloadOperation(() => enqueueRequestImpl(sessionId, requestId, captured))
      : enqueueRequestImpl(sessionId, requestId, captured));
  };

  const takeRequestImpl = async (sessionId, requestId, captured) => {
    await load();
    requireReceiptHistory();
    requireUploadOwner(sessionId, requestId, captured.directory);
    let record = requests.get(requestId);
    if (record) {
      requireMatchingRequest(record, captured, 'take');
      await persistRequest(requestId);
      armDispatch(sessionId);
      return requestResponse(record);
    }
    if ([...activeUploads.values()].some(({ metadata }) => metadata.requestId === requestId)) {
      throw queueRequestError('request attachment upload is in progress', 409, 'queue_request_pending');
    }
    requireUnreserved(sessionId);
    const queue = queues.get(sessionId);
    if (queue && queue.directory !== captured.directory) throw queueRequestError('queue belongs to another directory', 409, 'queue_request_conflict');
    let items = captured.input.itemIds.map((id) => queue?.items.find((entry) => entry.id === id));
    if (items.some((item) => !item)) throw httpError('queued message not found', 404);
    if (items.some((item) => item.id === sending.get(sessionId) || item.goal || item.scheduledTask || !canTransferItem(item))
        || (!captured.input.format && !nativeTransferFits(items))) {
      throw httpError('queued message cannot be transferred', 409);
    }
    if (captured.input.format) {
      if (items.reduce((count, item) => count + capturedEntryCount(item), 0) > 4096) {
        throw httpError('queued messages exceed manifest entry limits', 409);
      }
      preparingTransfers.add(sessionId);
      try {
        const converted = await (captured.input.format === 'payload-v1' ? capturePayloadItems : captureManifestItems)(items, sessionId, requestId, captured.directory);
        const current = queues.get(sessionId);
        if (current?.directory !== captured.directory || items.some((item) => !current.items.includes(item) || sending.get(sessionId) === item.id)
            || reservations.has(sessionId)) throw httpError('queue changed during manifest transfer preparation', 409);
        items = converted;
      } finally {
        preparingTransfers.delete(sessionId);
        armDispatch(sessionId);
      }
    }
    record = { kind: 'take', receipt: createTakeReceipt(captured, now()), state: 'transferring', items: structuredClone(items) };
    if (Buffer.byteLength(JSON.stringify({ ...publicQueueRequest(record), revision: Number.MAX_SAFE_INTEGER })) > RECEIPT_BYTE_LIMIT) {
      throw queueRequestError('take payload exceeds receipt limits', 413, 'queue_request_too_large');
    }
    const legacyClaims = new Map();
    for (const item of items) {
      if (item.requestId) continue;
      const ids = transferredContextIds(item);
      if (new Set(ids).size !== ids.length || ids.some((id) => contextOwners.has(id) || legacyClaims.has(id))) {
        throw queueRequestError('transferred context identity is already owned', 409, 'queue_request_conflict');
      }
      for (const [id, owner] of captureContextClaims({ sessionId, directory: captured.directory }, ids,
        { kind: 'take', requestId, itemId: item.id })) legacyClaims.set(id, owner);
    }
    applyContextClaims(legacyClaims);
    requests.set(requestId, record);
    const current = queues.get(sessionId);
    setQueueItems(sessionId, current.directory, current.items.filter((item) => !captured.input.itemIds.includes(item.id)), 'taken');
    revision += 1;
    await persistRequest(requestId);
    broadcast(sessionId);
    armDispatch(sessionId);
    return requestResponse(record);
  };
  const takeRequest = async (sessionId, requestId, input) => {
    const captured = captureQueueRequest('take', sessionId, requestId, input);
    return withBlobOwnership(() => takeRequestImpl(sessionId, requestId, captured));
  };

  const acknowledgeTake = (sessionId, requestId, directory) => withBlobOwnership(async () => {
    await load();
    const record = requireOwnedRequest(sessionId, requestId, directory);
    if (record.kind !== 'take') throw queueRequestError('request is not a take', 409, 'queue_request_conflict');
    if (record.state !== 'acknowledged') {
      const updates = new Map();
      for (const item of record.items) {
        for (const id of transferredContextIds(item)) {
          const owner = contextOwners.get(id);
          const current = owner?.claims.at(-1);
          if (!owner || !sameContextOwner(owner, record.receipt) || (item.requestId
            ? current.kind !== 'enqueue' || current.requestId !== item.requestId
            : current.kind !== 'take' || current.requestId !== requestId || current.itemId !== item.id)) {
            throw queueRequestError('take no longer owns its captured context', 409, 'queue_request_conflict');
          }
          if (item.requestId) updates.set(id, { ...owner, claims: [...owner.claims, { kind: 'take', requestId, itemId: item.id }] });
        }
      }
      applyContextClaims(updates);
      record.state = 'acknowledged';
      delete record.items;
      revision += 1;
    }
    await persistRequest(requestId);
    return requestResponse(record);
  });

  const retryRequest = async (sessionId, requestId, directory, attempt) => {
    await load();
    const record = requireOwnedRequest(sessionId, requestId, directory);
    if (record.kind !== 'enqueue' || !Number.isSafeInteger(attempt) || attempt < 1) throw new TypeError('invalid enqueue retry attempt');
    if (reservations.has(sessionId)) throw httpError('queue prefix is reserved for a prompt', 409);
    if (attempt !== record.attempt) {
      if (attempt !== record.attempt + 1 || record.state !== 'uncertain' || sending.has(sessionId)
          || (record.blocked && record.blocked !== 'prompt_body_too_large')) {
        throw queueRequestError('delivery is not available for this retry attempt', 409, 'queue_request_conflict');
      }
      record.attempt = attempt;
      record.state = 'queued';
      revision += 1;
    }
    await persistRequest(requestId);
    if (record.state === 'queued') armDispatch(sessionId);
    return requestResponse(record);
  };

  const enqueue = async (sessionIdInput, directoryInput, itemInput) => {
    const sessionId = requireSessionId(sessionIdInput);
    const directory = asNonEmptyString(directoryInput);
    if (!directory) throw new TypeError('directory is required');
    if (itemInput?.format !== undefined || itemInput?.attachmentManifest !== undefined
        || itemInput?.payloadManifest !== undefined || itemInput?.payloadIndex !== undefined || itemInput?.payloadIndexHash !== undefined) {
      throw new TypeError('manifest payloads require a durable enqueue request');
    }
    const parsed = parseQueuedItemInput(itemInput);
    await load();
    if (parsed.scheduledTask) {
      for (const queue of queues.values()) {
        if (queue.items.some((entry) => entry.scheduledTask?.projectId === parsed.scheduledTask.projectId && entry.scheduledTask.taskId === parsed.scheduledTask.taskId)) {
          throw httpError('task is already queued', 409);
        }
      }
      if ((queues.get(sessionId)?.items.length ?? 0) >= MAX_ITEMS_PER_SESSION || (!queues.has(sessionId) && queues.size >= MAX_SESSIONS)) {
        throw httpError('message queue is full', 409);
      }
    }
    const item = {
      id: `queued-${now()}-${Math.random().toString(36).slice(2, 9)}`,
      createdAt: now(),
      ...parsed,
    };
    const existing = queues.get(sessionId);
    if (existing && existing.directory !== directory && (existing.items.some((item) => item.requestId || item.format)
        || reservations.has(sessionId) || preparingTransfers.has(sessionId))) {
      throw httpError('queue belongs to another directory', 409);
    }
    if (existing?.items.length >= MAX_ITEMS_PER_SESSION && existing.items[0].requestId) throw httpError('message queue is full', 409);
    if (!existing && queues.size >= MAX_SESSIONS
        && !Array.from(queues.entries()).some(([id, queue]) => !sending.has(id) && !reservations.has(id) && !preparingTransfers.has(id) && !queue.items.some((item) => item.requestId))) {
      throw httpError('message queue is full', 409);
    }
    if ((reservations.has(sessionId) || preparingTransfers.has(sessionId)) && existing.items.length >= MAX_ITEMS_PER_SESSION) {
      throw httpError('reserved message queue is full', 409);
    }
    const items = [...(existing?.items ?? []), item].slice(-MAX_ITEMS_PER_SESSION);
    setQueueItems(sessionId, directory, items);
    directories.set(sessionId, directory);
    if (queues.size > MAX_SESSIONS) {
      const oldest = Array.from(queues.entries())
        .filter(([id, queue]) => id !== sessionId && !sending.has(id) && !reservations.has(id) && !preparingTransfers.has(id) && !queue.items.some((item) => item.requestId))
        .sort((left, right) => (left[1].items[0]?.createdAt ?? 0) - (right[1].items[0]?.createdAt ?? 0))
        .slice(0, queues.size - MAX_SESSIONS);
      for (const [staleId] of oldest) {
        setQueueItems(staleId, directories.get(staleId), []);
        clearTimer(staleId);
        broadcast(staleId);
        directories.delete(staleId);
      }
    }
    const result = commit(sessionId);
    // The session may already be idle (queued from a busy-looking composer
    // right as the turn ended); the tick verifies before sending.
    armDispatch(sessionId);
    return { ...result, itemId: item.id };
  };

  const remove = async (sessionIdInput, itemId) => {
    const sessionId = requireSessionId(sessionIdInput);
    await load();
    requireUnreserved(sessionId);
    if (sending.get(sessionId) === itemId) throw httpError('message is being sent', 409);
    const queue = queues.get(sessionId);
    if (!queue || !queue.items.some((item) => item.id === itemId)) {
      return { revision, session: sessionSnapshot(sessionId) };
    }
    const headRequest = requests.get(queue.items[0].requestId);
    const resume = queue.items[0].id === itemId && (headRequest?.state === 'uncertain' || headRequest?.blocked === 'prompt_body_too_large');
    setQueueItems(sessionId, queue.directory, queue.items.filter((item) => item.id !== itemId));
    const result = commit(sessionId);
    if (resume) armDispatch(sessionId);
    return result;
  };

  /** Removes the item and hands its full payload (attachments included) back. */
  const take = async (sessionIdInput, itemId) => {
    const sessionId = requireSessionId(sessionIdInput);
    await load();
    requireUnreserved(sessionId);
    if (sending.get(sessionId) === itemId) throw httpError('message is being sent', 409);
    const queue = queues.get(sessionId);
    const item = queue?.items.find((entry) => entry.id === itemId);
    if (!queue || !item) throw httpError('queued message not found', 404);
    if (item.scheduledTask) throw httpError('scheduled messages cannot be taken', 409);
    if (item.format) throw httpError('blob-backed messages require a durable typed take', 409);
    if (!canTransferItem(item)) throw httpError('uncertain delivery requires its original retry identity', 409);
    setQueueItems(sessionId, queue.directory, queue.items.filter((entry) => entry.id !== itemId), 'taken');
    return { ...commit(sessionId), item };
  };

  /** Removes every item not currently being sent and hands them back in order. */
  const takeAll = async (sessionIdInput) => {
    const sessionId = requireSessionId(sessionIdInput);
    await load();
    requireUnreserved(sessionId);
    const queue = queues.get(sessionId);
    if (!queue) return { revision, session: sessionSnapshot(sessionId), items: [] };
    const sendingId = sending.get(sessionId) ?? null;
    const items = queue.items.filter((item) => item.id !== sendingId && !item.scheduledTask && !item.format && canTransferItem(item));
    if (items.length === 0) return { revision, session: sessionSnapshot(sessionId), items: [] };
    setQueueItems(sessionId, queue.directory, queue.items.filter((item) => !items.includes(item)), 'taken');
    return { ...commit(sessionId), items };
  };

  const requireUnreserved = (sessionId) => {
    if (preparingTransfers.has(sessionId)) throw httpError('queue transfer is being prepared', 409);
    if (reservations.has(sessionId)) throw httpError('queue prefix is reserved for a prompt', 409);
    for (const requestId of pendingRequests) {
      if (requests.get(requestId)?.receipt.sessionId === sessionId) throw httpError('queue request persistence is unresolved', 409);
    }
  };

  // No await occurs between the authoritative prefix check and exclusion.
  // Original items stay in the queue file until admission of the stable ID.
  const reservePrefixLegacy = async (sessionIdInput, input, preview = false) => {
    const sessionId = requireSessionId(sessionIdInput);
    const requested = parseReservation(input, sessionId);
    await load();
    if (preparingTransfers.has(sessionId)) throw httpError('queue capture is being prepared', 409);
    for (const requestId of pendingRequests) {
      if (requests.get(requestId)?.receipt.sessionId === sessionId) throw httpError('queue request persistence is unresolved', 409);
    }
    let reservation = reservations.get(sessionId);
    const queue = queues.get(sessionId);
    const unavailable = (message) => Object.assign(httpError(message, 409), {
      code: 'queue_prefix_unavailable', sessionID: sessionId, messageID: requested.id,
    });
    if (reservation && JSON.stringify(reservation) !== JSON.stringify(requested)) {
      throw unavailable('queue prefix is reserved for another captured prompt');
    }
    if (!reservation) {
      if (contextOwners.has(requested.id) || prefixPromptOwners.has(requested.id)
          || [...requests.values()].some((record) => record.kind === 'enqueue' && record.receipt.messageId === requested.id)) {
        throw queueRequestError('captured prefix identity is already owned', 409, 'queue_request_conflict');
      }
      if (!queue || sending.has(sessionId)
          || !requested.itemIds.every((id, index) => queue.items[index]?.id === id && nativeTakeable(queue.items[index]) && canTransferItem(queue.items[index]))) {
        throw unavailable('queue prefix changed or cannot be taken');
      }
      const prefix = queue.items.slice(0, requested.itemIds.length);
      const texts = [...prefix.map((item) => item.text).filter((text) => text.trim()), requested.composer].filter((text) => text.trim());
      if (Buffer.byteLength(texts.join('\n\n')) > 1048576
          || prefix.some((item) => (item.context ?? []).some((part) => !part.text.trim() || (part.instructions && !part.instructions.trim())))
          || prefix.reduce((count, item) => count + item.attachments.length + (item.agentMention ? 1 : 0) + (item.context ?? []).reduce((n, part) => n + (part.instructions ? 2 : 1), 0), 0) > 4096
          // Bound the complete receipt, including both full payloads and its
          // public snapshot. Leave room for native prompt/draft copies and
          // JSON escaping within its 16 MiB journal and response limits.
          || Buffer.byteLength(JSON.stringify({ reservation: requested, items: prefix, session: sessionSnapshot(sessionId) })) > 4 * 1024 * 1024) {
        throw unavailable('queue prefix exceeds native journal limits');
      }
      reservation = requested;
      if (preview) return { revision, session: sessionSnapshot(sessionId), reservation, items: prefix };
      reservations.set(sessionId, reservation);
      clearTimer(sessionId);
      revision += 1;
    }
    if (preview) return { revision, session: sessionSnapshot(sessionId), reservation, items: queue.items.slice(0, reservation.itemIds.length) };
    // A failed write retains the exclusion, and a repeated call retries it.
    // An answer is handed out only after the receipt and original items are durable.
    await persist();
    broadcast(sessionId);
    armDispatch(sessionId, retryDelayMs(1));
    return {
      revision, session: sessionSnapshot(sessionId), reservation,
      items: queue.items.slice(0, reservation.itemIds.length),
    };
  };

  const reserveManifestPrefix = async (sessionIdInput, requested, preview) => {
    const sessionId = requireSessionId(sessionIdInput);
    await load();
    requireReceiptHistory();
    const queue = queues.get(sessionId);
    if (queue && queue.directory !== requested.directory) {
      throw queueRequestError('queue prefix belongs to another directory', 409, 'queue_request_conflict');
    }
    for (const id of pendingRequests) {
      if (requests.get(id)?.receipt.sessionId === sessionId) throw httpError('queue request persistence is unresolved', 409);
    }
    const previous = reservations.get(sessionId);
    if (previous && reservationFingerprint(previous) !== reservationFingerprint(requested)) throw httpError('queue prefix belongs to another captured request', 409);
    const promptOwner = prefixPromptOwners.get(requested.id);
    if (previous ? !promptOwner || promptOwner.state !== 'reserved' || !sameContextOwner(promptOwner, { sessionId, directory: requested.directory }) : promptOwner) {
      throw queueRequestError('captured prefix identity is already owned', 409, 'queue_request_conflict');
    }
    const source = queue?.items.slice(0, requested.itemIds.length);
    if (sending.has(sessionId) || preparingTransfers.has(sessionId)) throw httpError('queue prefix changed or cannot be captured', 409);
    const prefixMatches = Boolean(source) && requested.itemIds.every((id, index) => source[index]?.id === id && !source[index].goal
      && !source[index].scheduledTask && canTransferItem(source[index]));
    if (!prefixMatches) {
      // Without a reservation nothing is held for this request and nothing can
      // be, so the refusal is definite. A held reservation keeps the plain 409.
      if (!previous) throw queueRequestError('queue prefix changed', 409, 'queue_prefix_stale');
      throw httpError('queue prefix changed or cannot be captured', 409);
    }
    // Exclude dispatch before the first byte read. The queue stays the owner
    // until exact prompt admission, including after a lost response/restart.
    preparingTransfers.add(sessionId);
    try {
      const uploadRequestId = `prefix_${createHash('sha256').update(requested.id).digest('hex')}`;
      const payload = requested.format === 'payload-v1';
      const items = await (payload ? capturePayloadItems : captureManifestItems)(source, sessionId, uploadRequestId, queue.directory);
      const accepted = payload ? { ...requested, composer: await withPayloadOperation(async () => {
        const receipt = { messageId: requested.id };
        if (previous) {
          await materializePayload(previous.composer, sessionId, queue.directory, receipt, { allowEmpty: true });
          return previous.composer;
        }
        const logical = await readPayload(requested.composer, sessionId, queue.directory, { allowEmpty: true });
        return requireCompactPayload({ ...requested.composer, payloadIndex: indexPayload(logical, queue.directory,
          freezePayloadContextIds(logical, JSON.stringify([requested.id, 'composer'])), receipt) });
      }) } : requested;
      const current = queues.get(sessionId);
      if (!current || current.directory !== requested.directory || source.some((item, index) => current.items[index] !== item)
          || reservations.get(sessionId) !== previous) throw httpError('queue prefix changed during capture', 409);
      const contextIds = prefixContextIds(accepted, items);
      const parts = items.reduce((count, item) => count + capturedEntryCount(item), 0)
        + (payload ? capturedEntryCount(accepted.composer)
          : (requested.promptParts?.agents.length ?? 0) + (requested.promptParts?.skills.length ?? 0) + (requested.promptParts?.context.length ?? 0)
            + (requested.promptParts?.fileReferences?.length ?? 0));
      if (payload) assertStructuredBudget(items.reduce((size, item) => size + item.payloadIndex.structuredBytes, accepted.composer.payloadIndex.structuredBytes));
      if (parts > 4096 || new Set([requested.id, ...contextIds]).size !== contextIds.length + 1) throw new TypeError('invalid captured prefix entries or context identities');
      const selectedRequests = new Set(items.map((item) => item.requestId).filter(Boolean));
      const capturedIds = new Set([requested.id, ...contextIds]);
      const otherReservedIds = reservedMessageIds(sessionId);
      if ([...capturedIds].some((id) => otherReservedIds.has(id)) || contextOwners.has(requested.id)
          || [...requests.values()].some((record) => record.kind === 'enqueue' && record.receipt.messageId === requested.id)) {
        throw queueRequestError('captured prefix identity is already owned', 409, 'queue_request_conflict');
      }
      const contextClaims = captureContextClaims({ sessionId, directory: requested.directory }, contextIds,
        { kind: 'prefix', id: requested.id }, selectedRequests, Boolean(previous));
      const response = { revision, reservation: previous ?? accepted, items, contextIds };
      if (Buffer.byteLength(JSON.stringify({ ...response, revision: Number.MAX_SAFE_INTEGER })) > RECEIPT_BYTE_LIMIT) {
        throw queueRequestError('prefix manifest exceeds receipt limits', 413, 'queue_request_too_large');
      }
      if (preview) return structuredClone(response);
      applyContextClaims(contextClaims);
      if (!previous) prefixPromptOwners.set(requested.id, { sessionId, directory: requested.directory, state: 'reserved' });
      queues.set(sessionId, { directory: current.directory, items: [...items, ...current.items.slice(items.length)] });
      reservations.set(sessionId, previous ?? accepted);
      clearTimer(sessionId);
      revision += 1;
      await persist();
      broadcast(sessionId);
      armDispatch(sessionId, retryDelayMs(1));
      return structuredClone({ ...response, revision });
    } finally {
      preparingTransfers.delete(sessionId);
      if (preview || !reservations.has(sessionId)) armDispatch(sessionId);
    }
  };
  const reservePrefix = async (sessionId, input, preview = false) => {
    const requested = parseReservation(input, sessionId);
    return requested.format
      ? withBlobOwnership(() => reserveManifestPrefix(sessionId, requested, preview))
      : reservePrefixLegacy(sessionId, input, preview);
  };

  const reconcileReservation = async (sessionId) => {
    const reservation = reservations.get(sessionId);
    if (!reservation) return;
    try {
      const prefix = `/api/session/${encodeURIComponent(sessionId)}`;
      const inbox = await openCodeFetch(`${prefix}/inbox`);
      if (!Array.isArray(inbox)) throw new TypeError('invalid inbox');
      const matching = inbox.filter((item) => item?.id === reservation.id);
      let admitted = false;
      if (matching.length === 1) {
        const user = inboxUserSchema.parse(matching[0]);
        admitted = user.id === reservation.id && user.sessionID === sessionId;
      }
      if (matching.length && !admitted) throw new TypeError('conflicting inbox identity');
      if (!admitted) {
        const message = await openCodeFetch(`${prefix}/message/${encodeURIComponent(reservation.id)}`);
        const user = historyUserSchema.parse(message);
        admitted = user.id === reservation.id;
      }
      if (admitted && reservations.get(sessionId) === reservation) {
        const queue = queues.get(sessionId);
        consumePrefixPrompt(reservation, sessionId, queue.directory);
        setQueueItems(sessionId, queue.directory, queue.items.slice(reservation.itemIds.length), 'taken');
        reservations.delete(sessionId);
        failures.delete(sessionId);
        commit(sessionId);
        // The next item waits behind this admitted prompt's live turn.
        armDispatch(sessionId);
        return;
      }
    } catch {
      // Missing, failed and malformed observations cannot release ownership.
    }
    if (reservations.get(sessionId) !== reservation) return;
    const previous = failures.get(sessionId);
    const count = previous?.itemId === reservation.id ? previous.failures + 1 : 1;
    failures.set(sessionId, { itemId: reservation.id, failures: count, nextAttemptAt: now() + retryDelayMs(count) });
    armDispatch(sessionId, retryDelayMs(count));
  };

  const reorder = async (sessionIdInput, itemIds) => {
    const sessionId = requireSessionId(sessionIdInput);
    if (!asList(itemIds) || itemIds.some((id) => !asNonEmptyString(id))) {
      throw new TypeError('itemIds must be a list of ids');
    }
    await load();
    requireUnreserved(sessionId);
    const queue = queues.get(sessionId);
    if (!queue) return { revision, session: sessionSnapshot(sessionId) };
    const byId = new Map(queue.items.map((item) => [item.id, item]));
    if (itemIds.length !== byId.size || new Set(itemIds).size !== itemIds.length || itemIds.some((id) => !byId.has(id))) {
      throw new TypeError('itemIds must list every queued message exactly once');
    }
    const resume = requests.get(queue.items[0].requestId)?.blocked === 'prompt_body_too_large';
    queues.set(sessionId, { directory: queue.directory, items: itemIds.map((id) => byId.get(id)) });
    const result = commit(sessionId);
    if (resume) armDispatch(sessionId);
    return result;
  };

  const clear = async (sessionIdInput) => {
    const sessionId = requireSessionId(sessionIdInput);
    await load();
    requireUnreserved(sessionId);
    const queue = queues.get(sessionId);
    if (!queue) return { revision, session: sessionSnapshot(sessionId) };
    // Never drop a message already handed to OpenCode: its send resolves and
    // must find its entry.
    const sendingId = sending.get(sessionId) ?? null;
    setQueueItems(sessionId, queue.directory, queue.items.filter((item) => item.id === sendingId));
    clearTimer(sessionId);
    return commit(sessionId);
  };

  /** What held the session's delivery let go: deliver whatever waited. */
  const release = (sessionId) => {
    armDispatch(sessionId);
  };

  // --- events --------------------------------------------------------------

  const processPayload = (value) => {
    const payload = asRecord(value);
    if (stopped || !payload) return;

    const deletedSessionId = extractDeletedSessionId(payload);
    if (deletedSessionId) {
      if (!queues.has(deletedSessionId)) return;
      const reservation = reservations.get(deletedSessionId);
      if (reservation) consumePrefixPrompt(reservation, deletedSessionId, queues.get(deletedSessionId).directory);
      reservations.delete(deletedSessionId);
      setQueueItems(deletedSessionId, queues.get(deletedSessionId)?.directory ?? directories.get(deletedSessionId), [], 'deleted');
      clearTimer(deletedSessionId);
      failures.delete(deletedSessionId);
      commit(deletedSessionId);
      directories.delete(deletedSessionId);
      return;
    }

    const status = extractSessionStatus(payload);
    if (status) {
      if (!queues.has(status.sessionId)) return;
      if (status.type === 'idle' || reservations.has(status.sessionId)) armDispatch(status.sessionId);
      else clearTimer(status.sessionId);
      return;
    }

    const abortedSessionId = extractAbortedSessionId(payload);
    if (abortedSessionId) {
      if (queues.has(abortedSessionId)) abortedAt.set(abortedSessionId, now());
      return;
    }

    const assistant = extractAssistantMessageUpdate(payload);
    if (assistant && queues.has(assistant.sessionId)) {
      // A completed reply without a following idle status (missed event)
      // must still drain the queue; the tick verifies idleness itself.
      if (assistant.completed && !timers.has(assistant.sessionId)) armDispatch(assistant.sessionId);
    }
  };

  const processEvent = (event) => {
    // The hub translates v2 wire events into the server's vocabulary once.
    for (const payload of event?.translated?.() ?? []) processPayload(payload);
  };

  const start = () => {
    const unsubscribeEvent = globalEventHub.subscribeEvent(processEvent);
    const unsubscribeStatus = globalEventHub.subscribeStatus((status) => {
      if (status?.type === 'connect') reconcileAll();
    });
    void load()
      .then(async () => {
        if (queues.size > 0) console.log(`[message-queue] restored queues for ${queues.size} session(s)`);
        // A tick that ran before this saw every session held and armed nothing.
        await deliveryHoldReady;
        reconcileAll();
      })
      .catch((error) => {
        console.warn('[message-queue] failed to load queue file:', error?.message ?? error);
      });
    return () => {
      unsubscribeEvent();
      unsubscribeStatus();
    };
  };

  const stop = () => {
    stopped = true;
    for (const timer of timers.values()) clearTimeout(timer);
    timers.clear();
  };

  const cancelScheduledTask = async (projectId, taskId) => {
    await load();
    for (const [sessionId, queue] of queues) {
      const items = queue.items.filter((item) => item.scheduledTask?.projectId !== projectId || item.scheduledTask.taskId !== taskId || sending.get(sessionId) === item.id);
      if (items.length === queue.items.length) continue;
      setQueueItems(sessionId, queue.directory, items);
      commit(sessionId);
      armDispatch(sessionId);
    }
  };

  return {
    load,
    snapshot,
    sessionSnapshot,
    enqueue,
    enqueueRequest,
    takeRequest,
    getRequest,
    acknowledgeTake,
    retryRequest,
    uploadAttachment,
    getAttachment,
    releaseAttachment,
    cancelScheduledTask,
    remove,
    take,
    takeAll,
    reservePrefix,
    previewPrefix: (sessionId, input) => reservePrefix(sessionId, input, true),
    reorder,
    clear,
    release,
    processPayload,
    start,
    stop,
    /** Drains the pending write; tests and shutdown use it. */
    flush: () => writePromise,
  };
}

export function registerMessageQueueRoutes(app, runtime) {
  const respondError = (res, error, fallback) => {
    const status = error instanceof TypeError ? 400 : (Number.isInteger(error?.status) ? error.status : 500);
    const body = { error: error?.message ?? fallback };
    if (error?.code === 'queue_prefix_unavailable') {
      Object.assign(body, { code: error.code, sessionID: error.sessionID, messageID: error.messageID });
    } else if (error?.code === 'queue_prefix_stale' || /^queue_request_/.test(error?.code ?? '')) {
      body.code = error.code;
    }
    res.status(status).json(body);
  };

  const requestPath = '/api/message-queue/sessions/:sessionId/requests/:requestId';
  const attachmentPath = `${requestPath}/attachments/:attachmentId`;
  const attachmentDirectory = (req) => {
    const result = z.object({ directory: z.string() }).strict().safeParse(req.query);
    if (!result.success) throw new TypeError('attachment requires an exact directory query');
    return result.data.directory;
  };
  const requireNoAttachmentBody = (req) => {
    if ((req.body !== undefined && !z.object({}).strict().safeParse(req.body).success)
        || Number(req.headers?.['content-length'] ?? 0) !== 0 || req.headers?.['transfer-encoding']) {
      throw new TypeError('attachment read or release does not accept a body');
    }
  };
  app.put(attachmentPath, async (req, res) => {
    res.setHeader('Cache-Control', 'no-store');
    try {
      if (String(req.headers?.['content-type'] ?? '').split(';')[0].trim().toLowerCase() !== 'application/octet-stream'
          || (req.headers?.['content-encoding'] && req.headers['content-encoding'] !== 'identity')) {
        throw httpError('attachment upload requires uncompressed application/octet-stream', 415);
      }
      const declared = req.headers?.['x-openchamber-size'];
      // A repeated header arrives as an array, which asText rejects.
      if (!/^(0|[1-9][0-9]*)$/.test(asText(declared))) throw new TypeError('invalid attachment size header');
      const size = Number(declared);
      if (size > BLOB_BYTE_LIMIT || Number(req.headers?.['content-length'] ?? 0) > BLOB_BYTE_LIMIT) throw httpError('attachment exceeds prepared file limit', 413);
      let filename;
      try { filename = decodeURIComponent(req.headers?.['x-openchamber-filename'] ?? ''); } catch { throw new TypeError('invalid attachment filename encoding'); }
      res.json(await runtime.uploadAttachment(req.params.sessionId, req.params.requestId, req.params.attachmentId, {
        directory: attachmentDirectory(req), filename, size,
        mimeType: req.headers?.['x-openchamber-mime-type'], sha256: req.headers?.['x-openchamber-sha256'],
      }, req));
    } catch (error) {
      req.resume?.();
      respondError(res, error, 'Failed to upload queue attachment');
    }
  });
  app.get(attachmentPath, async (req, res) => {
    res.setHeader('Cache-Control', 'no-store');
    try {
      requireNoAttachmentBody(req);
      const result = await runtime.getAttachment(req.params.sessionId, req.params.requestId, req.params.attachmentId, attachmentDirectory(req));
      res.setHeader('Content-Type', 'application/octet-stream');
      res.setHeader('Content-Disposition', 'attachment');
      res.setHeader('X-Content-Type-Options', 'nosniff');
      res.setHeader('Content-Length', String(result.bytes.length));
      res.setHeader('X-OpenChamber-SHA256', result.attachment.sha256);
      res.send(result.bytes);
    } catch (error) { respondError(res, error, 'Failed to read queue attachment'); }
  });
  app.delete(attachmentPath, async (req, res) => {
    res.setHeader('Cache-Control', 'no-store');
    try {
      requireNoAttachmentBody(req);
      res.json(await runtime.releaseAttachment(req.params.sessionId, req.params.requestId, req.params.attachmentId, attachmentDirectory(req)));
    } catch (error) { respondError(res, error, 'Failed to release queue attachment'); }
  });
  const registerRequestPost = (suffix, schema, handle) => {
    app.post(`${requestPath}/${suffix}`, async (req, res) => {
      res.setHeader('Cache-Control', 'no-store');
      try {
        if (!z.object({}).strict().safeParse(req.query ?? {}).success) throw new TypeError('queue request does not accept query parameters');
        const parsed = schema.safeParse(req.body);
        if (!parsed.success) throw new TypeError('invalid queue request body');
        res.json(await handle(req.params.sessionId, req.params.requestId, parsed.data));
      } catch (error) {
        respondError(res, error, 'Queue request failed');
      }
    });
  };
  app.get(requestPath, async (req, res) => {
    res.setHeader('Cache-Control', 'no-store');
    try {
      if (req.body !== undefined && !z.object({}).strict().safeParse(req.body).success) throw new TypeError('receipt lookup does not accept a body');
      const parsed = z.object({ directory: z.string() }).strict().safeParse(req.query);
      if (!parsed.success) throw new TypeError('receipt lookup requires an exact directory query');
      res.json(await runtime.getRequest(req.params.sessionId, req.params.requestId, parsed.data.directory));
    } catch (error) {
      respondError(res, error, 'Failed to look up queue request');
    }
  });
  registerRequestPost('enqueue', z.object({ directory: z.string(), item: z.record(z.string(), z.unknown()) }).strict(),
    (sessionId, requestId, body) => runtime.enqueueRequest(sessionId, requestId, body));
  registerRequestPost('take', z.object({ directory: z.string(), itemIds: z.array(z.string()), format: z.enum(['manifest-v1', 'payload-v1']).optional() }).strict(),
    (sessionId, requestId, body) => runtime.takeRequest(sessionId, requestId, body));
  registerRequestPost('acknowledge', z.object({ directory: z.string() }).strict(),
    (sessionId, requestId, body) => runtime.acknowledgeTake(sessionId, requestId, body.directory));
  registerRequestPost('retry', z.object({ directory: z.string(), attempt: z.number().int().positive().safe() }).strict(),
    (sessionId, requestId, body) => runtime.retryRequest(sessionId, requestId, body.directory, body.attempt));

  app.get('/api/message-queue', async (_req, res) => {
    try {
      await runtime.load();
      res.json(runtime.snapshot());
    } catch (error) {
      respondError(res, error, 'Failed to load message queue');
    }
  });

  app.post('/api/message-queue/sessions/:sessionId/items', async (req, res) => {
    try {
      if (req.body?.item?.scheduledTask) throw new TypeError('scheduledTask is server-owned');
      res.json(await runtime.enqueue(req.params.sessionId, req.body?.directory, req.body?.item));
    } catch (error) {
      respondError(res, error, 'Failed to queue message');
    }
  });

  app.post('/api/message-queue/sessions/:sessionId/take', async (req, res) => {
    try {
      res.json(await runtime.takeAll(req.params.sessionId));
    } catch (error) {
      respondError(res, error, 'Failed to take queued messages');
    }
  });

  app.post('/api/message-queue/sessions/:sessionId/reserve-prefix', async (req, res) => {
    try {
      res.json(await runtime.reservePrefix(req.params.sessionId, req.body));
    } catch (error) {
      respondError(res, error, 'Failed to reserve queued messages');
    }
  });

  app.post('/api/message-queue/sessions/:sessionId/preview-prefix', async (req, res) => {
    try {
      res.json(await runtime.previewPrefix(req.params.sessionId, req.body));
    } catch (error) {
      respondError(res, error, 'Failed to preview queued messages');
    }
  });

  app.put('/api/message-queue/sessions/:sessionId/order', async (req, res) => {
    try {
      res.json(await runtime.reorder(req.params.sessionId, req.body?.itemIds));
    } catch (error) {
      respondError(res, error, 'Failed to reorder queue');
    }
  });

  app.delete('/api/message-queue/sessions/:sessionId', async (req, res) => {
    try {
      res.json(await runtime.clear(req.params.sessionId));
    } catch (error) {
      respondError(res, error, 'Failed to clear queue');
    }
  });

  app.post('/api/message-queue/sessions/:sessionId/items/:itemId/take', async (req, res) => {
    try {
      res.json(await runtime.take(req.params.sessionId, req.params.itemId));
    } catch (error) {
      respondError(res, error, 'Failed to take queued message');
    }
  });

  app.delete('/api/message-queue/sessions/:sessionId/items/:itemId', async (req, res) => {
    try {
      res.json(await runtime.remove(req.params.sessionId, req.params.itemId));
    } catch (error) {
      respondError(res, error, 'Failed to remove queued message');
    }
  });
}
