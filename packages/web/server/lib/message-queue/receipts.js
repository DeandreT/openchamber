import { createHash } from 'node:crypto';
import { SessionMessage } from '@opencode/schema/session-message';
import { z } from 'zod';
import { jsonValueKind } from './prompt-body.js';

export const RECEIPT_BYTE_LIMIT = 4 * 1024 * 1024;
const requestID = z.string().regex(/^[A-Za-z0-9_-]{8,128}$/);
const messageID = z.string().regex(/^msg_[A-Za-z0-9_-]{1,120}$/);
const owner = { sessionId: z.string().regex(/^[A-Za-z0-9_-]{4,128}$/), directory: z.string().min(1).max(16384).refine((value) => value === value.trim()) };
const itemID = z.string().min(1).max(256);
const common = {
  version: z.literal(1), requestId: requestID, ...owner,
  requestHash: z.string().regex(/^[a-f0-9]{64}$/), acceptedAt: z.number().int().nonnegative().safe(),
};
const enqueueReceipt = z.object({
  ...common, kind: z.literal('enqueue'), itemId: itemID,
  messageId: messageID, contextIds: z.array(messageID).max(4096),
}).strict();
const takeReceipt = z.object({
  ...common, kind: z.literal('take'), itemIds: z.array(itemID).min(1).max(20),
}).strict();
const recordSchema = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('enqueue'), receipt: enqueueReceipt,
    state: z.enum(['queued', 'uncertain', 'dispatched', 'removed', 'taken', 'deleted']),
    attempt: z.number().int().nonnegative().safe(),
    // Size refusals stay queued before admission; command refusals retain the
    // existing uncertainty because that route has no stable delivery identity.
    blocked: z.enum(['command_identity_unsupported', 'prompt_body_too_large']).optional(),
    selection: z.object({
      model: z.object({ id: z.string().min(1), providerID: z.string().min(1), variant: z.string().optional() }).strict(),
      agent: z.string().optional(),
    }).strict().optional(),
  }).strict(),
  z.object({ kind: z.literal('take'), receipt: takeReceipt,
    state: z.enum(['transferring', 'acknowledged']), items: z.array(z.unknown()).max(20).optional(),
  }).strict(),
]);

export const queueRequestError = (message, status, code) => Object.assign(new Error(message), { status, code });

export const parseRequestId = (value) => {
  const result = requestID.safeParse(value);
  if (!result.success) throw new TypeError('requestId is invalid');
  return result.data;
};

// Non-blank with no surrounding whitespace: a string equal to its trim is blank only when empty.
const exactDirectory = z.string().min(1).refine((value) => value === value.trim());

export const parseRequestOwner = (sessionId, directory) => {
  if (!exactDirectory.safeParse(directory).success) {
    throw new TypeError('directory must be an exact non-empty string');
  }
  const parsed = z.object(owner).safeParse({ sessionId, directory });
  if (!parsed.success) throw new TypeError('request owner is invalid');
  return parsed.data;
};

const canonicalJSON = (value, depth = 0) => {
  if (depth > 64) throw new TypeError('request nesting is too deep');
  const kind = jsonValueKind(value);
  if (kind === 'null' || kind === 'string' || kind === 'boolean') return JSON.stringify(value);
  if (kind === 'number' && Number.isFinite(value)) return JSON.stringify(value);
  if (kind === 'array') return `[${value.map((entry) => canonicalJSON(entry, depth + 1)).join(',')}]`;
  if (kind === 'object') {
    return `{${Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${canonicalJSON(value[key], depth + 1)}`).join(',')}}`;
  }
  throw new TypeError('request must contain JSON values');
};

export const captureQueueRequest = (kind, sessionId, requestId, input) => {
  parseRequestId(requestId);
  const schema = kind === 'enqueue'
    ? z.object({ directory: z.string(), item: z.record(z.string(), z.unknown()) }).strict()
    : z.object({ directory: z.string(), itemIds: z.array(itemID).min(1).max(20), format: z.enum(['manifest-v1', 'payload-v1']).optional() }).strict();
  const parsed = schema.safeParse(input);
  if (!parsed.success) throw new TypeError(`invalid ${kind} request`);
  const owned = parseRequestOwner(sessionId, parsed.data.directory);
  if (kind === 'take' && new Set(parsed.data.itemIds).size !== parsed.data.itemIds.length) {
    throw new TypeError('itemIds must be unique');
  }
  const encoded = canonicalJSON({ kind, ...owned, input: parsed.data });
  if (Buffer.byteLength(encoded) > RECEIPT_BYTE_LIMIT) throw queueRequestError('queue request exceeds receipt limits', 413, 'queue_request_too_large');
  return { ...owned, requestId, requestHash: createHash('sha256').update(encoded).digest('hex'), input: JSON.parse(JSON.stringify(parsed.data)) };
};

export const createEnqueueReceipt = (captured, item, contextCount, acceptedAt, capturedContextIds = []) => {
  const contextIds = Array.from({ length: contextCount }, (_, index) => capturedContextIds[index] ?? SessionMessage.ID.create());
  return {
    version: 1, kind: 'enqueue', requestId: captured.requestId,
    sessionId: captured.sessionId, directory: captured.directory, requestHash: captured.requestHash, acceptedAt,
    itemId: item.id, messageId: SessionMessage.ID.create(), contextIds,
  };
};

export const createTakeReceipt = (captured, acceptedAt) => ({
  version: 1, kind: 'take', requestId: captured.requestId,
  sessionId: captured.sessionId, directory: captured.directory, requestHash: captured.requestHash, acceptedAt,
  itemIds: captured.input.itemIds,
});

export const parseStoredQueueRequest = (value, requestId) => {
  const result = recordSchema.safeParse(value);
  if (!result.success) throw new TypeError('invalid stored queue request receipt');
  const record = result.data;
  if (record.receipt.requestId !== requestId || record.receipt.kind !== record.kind
      || (record.kind === 'take' && ((record.state === 'transferring') !== Array.isArray(record.items)))
      || (record.kind === 'enqueue' && new Set([record.receipt.messageId, ...record.receipt.contextIds]).size !== record.receipt.contextIds.length + 1)
      || (record.kind === 'take' && new Set(record.receipt.itemIds).size !== record.receipt.itemIds.length)) {
    throw new TypeError('inconsistent stored queue request receipt');
  }
  return record;
};

export const requireMatchingRequest = (record, captured, kind) => {
  if (record.kind !== kind || record.receipt.sessionId !== captured.sessionId
      || record.receipt.directory !== captured.directory || record.receipt.requestHash !== captured.requestHash) {
    throw queueRequestError('requestId belongs to a different immutable request', 409, 'queue_request_conflict');
  }
};

// Response JSON: absent fields stay absent rather than serializing as null.
export const publicQueueRequest = (record) => {
  const result = { receipt: record.receipt, state: record.state };
  if (record.kind === 'enqueue') {
    result.attempt = record.attempt;
    if (record.blocked) result.blocked = record.blocked;
  }
  if (record.kind === 'take' && record.state === 'transferring') result.items = record.items;
  return structuredClone(result);
};
