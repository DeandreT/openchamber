import { createHash } from 'node:crypto';
import { z } from 'zod';
import { blobDescriptorSchema, manifestSendConfigSchema, parseStructuredItem, capturedEntryCount } from './payloads.js';
import { RECEIPT_BYTE_LIMIT, queueRequestError } from './receipts.js';
import { jsonByteLength, jsonValueKind } from './prompt-body.js';

// The existing JSON ingress budget, applied to each original structured capture.
export const STRUCTURED_BYTE_LIMIT = 50 * 1024 * 1024;
const digest = z.string().regex(/^[a-f0-9]{64}$/);
const messageId = z.string().regex(/^msg_[A-Za-z0-9_-]{1,120}$/);
const directory = z.string().min(1).max(16384).refine((value) => value === value.trim());
const chunkSchema = blobDescriptorSchema.extend({ ordinal: z.number().int().nonnegative().safe() }).strict();
const payloadManifestSchema = z.object({
  version: z.literal(1), encoding: z.literal('utf-8-json'),
  totalBytes: z.number().int().positive().safe(), sha256: digest,
  chunks: z.array(chunkSchema).min(1).max(4096),
}).strict();
const pointerSchema = z.object({ format: z.literal('payload-v1'), payloadManifest: payloadManifestSchema }).strict();
const indexSchema = z.object({
  directory, sendConfig: manifestSendConfigSchema, preview: z.string().max(103), contextPreview: z.string().max(103).optional(),
  entryCount: z.number().int().nonnegative().max(4096), contextIds: z.array(messageId).max(4096),
  attachmentBlobIds: z.array(digest).max(4096), promptBytes: z.number().int().nonnegative().safe(),
  admissionBytes: z.number().int().nonnegative().safe(),
  structuredBytes: z.number().int().nonnegative().max(STRUCTURED_BYTE_LIMIT),
  legacyAttachmentIds: z.array(z.string().min(1).max(256).nullable()).max(4096).optional(),
}).strict();
const storedPointerSchema = pointerSchema.extend({ payloadIndex: indexSchema, payloadIndexHash: digest }).strict();
const indexHash = (pointer) => createHash('sha256').update(JSON.stringify({
  payloadManifest: payloadManifestSchema.parse(pointer.payloadManifest), payloadIndex: indexSchema.parse(pointer.payloadIndex),
})).digest('hex');
export const sealPayloadPointer = (pointer) => ({ ...pointer, payloadIndexHash: indexHash(pointer) });

export const assertStructuredBudget = (bytes) => {
  if (!Number.isSafeInteger(bytes) || bytes < 0 || bytes > STRUCTURED_BYTE_LIMIT) {
    throw queueRequestError('structured capture exceeds 50 MiB', 413, 'queue_request_too_large');
  }
};

export const parsePayloadPointer = (value, { stored = false } = {}) => {
  const parsed = (stored ? storedPointerSchema : pointerSchema).safeParse(value);
  if (!parsed.success) throw new TypeError('invalid structured payload pointer');
  const pointer = parsed.data;
  if (Buffer.byteLength(JSON.stringify(pointer)) > RECEIPT_BYTE_LIMIT) throw queueRequestError('payload pointer exceeds receipt limits', 413, 'queue_request_too_large');
  const manifest = pointer.payloadManifest;
  // Count declared occurrences, including aliases, before any allocation or read.
  assertStructuredBudget(manifest.totalBytes);
  let size = 0;
  const ids = new Set();
  for (const [ordinal, chunk] of manifest.chunks.entries()) {
    size += chunk.size;
    assertStructuredBudget(size);
    if (chunk.ordinal !== ordinal || !chunk.size || ids.has(chunk.blobId)) throw new TypeError('invalid payload chunk ordinal, length or duplicate');
    ids.add(chunk.blobId);
  }
  if (size !== manifest.totalBytes) throw new TypeError('payload chunk lengths differ from totalBytes');
  if (stored) {
    const index = pointer.payloadIndex;
    if (new Set(index.contextIds).size !== index.contextIds.length
        || new Set(index.attachmentBlobIds).size !== index.attachmentBlobIds.length
        || index.entryCount < index.contextIds.length + index.attachmentBlobIds.length + (index.legacyAttachmentIds?.length ?? 0)
        || indexHash(pointer) !== pointer.payloadIndexHash) {
      throw new TypeError('invalid structured payload root index');
    }
  }
  return pointer;
};

// One active operation owns this buffer and parsed value. Startup uses only the
// verified root index; no expanded payload is cached or written into receipts.
export const readStructuredCapture = async (pointer, expectedDirectory, readChunk, { allowEmpty = false } = {}) => {
  const { payloadManifest: manifest } = parsePayloadPointer({ format: pointer.format, payloadManifest: pointer.payloadManifest });
  const bytes = Buffer.alloc(manifest.totalBytes);
  const whole = createHash('sha256');
  let offset = 0;
  for (const chunk of manifest.chunks) {
    const part = await readChunk(chunk);
    if (part.length !== chunk.size || createHash('sha256').update(part).digest('hex') !== chunk.sha256) throw new TypeError('payload chunk length or SHA-256 differs');
    part.copy(bytes, offset);
    whole.update(part);
    offset += part.length;
  }
  if (whole.digest('hex') !== manifest.sha256) throw new TypeError('payload whole SHA-256 differs');
  let capture;
  try {
    // Preserve a BOM so JSON.parse rejects it; malformed UTF-8 must not acquire
    // authority after replacement-character decoding.
    capture = JSON.parse(new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(bytes));
  } catch { throw new TypeError('payload must contain strict UTF-8 JSON'); }
  const parsed = z.object({ directory, item: z.record(z.string(), z.unknown()) }).strict().safeParse(capture);
  if (!parsed.success) throw new TypeError('invalid typed structured capture');
  if (parsed.data.directory !== expectedDirectory) throw queueRequestError('payload belongs to another directory', 409, 'queue_request_conflict');
  return parseStructuredItem(parsed.data.item, { allowEmpty, attachmentIds: pointer.payloadIndex?.legacyAttachmentIds,
    seed: manifest.sha256.slice(0, 24) });
};

export const makePayloadIndex = (item, directory, contextIds, promptBytes, admissionBytes = promptBytes) => {
  const structuredBytes = logicalStructuredBytes(item);
  assertStructuredBudget(structuredBytes);
  // The index is hashed and persisted, so optional fields stay absent.
  const index = { directory, sendConfig: item.sendConfig, preview: previewOf(item.content || item.text) };
  if (item.contextPreview !== undefined) index.contextPreview = item.contextPreview;
  Object.assign(index, {
    entryCount: capturedEntryCount(item), contextIds,
    attachmentBlobIds: [...new Set((item.attachmentManifest ?? []).map((file) => file.blobId))], promptBytes, admissionBytes, structuredBytes,
  });
  if (!item.format) index.legacyAttachmentIds = item.attachments.map((file) => file.id.length <= 256 ? file.id : null);
  return indexSchema.parse(index);
};

// Legacy parsing already discarded number spellings and inserted defaults.
// Use the shortest round-tripping number spelling to avoid expanding, e.g.,
// original `1e20` metadata into 21 decimal digits at the 50 MiB boundary.
export const encodeLegacyCapture = (capture) => {
  const walk = (value, emit) => {
    const kind = jsonValueKind(value);
    if (kind === 'null' || kind === 'string' || kind === 'boolean') return emit(JSON.stringify(value));
    if (kind === 'number' && Number.isFinite(value)) {
      const decimal = JSON.stringify(value);
      const exponential = value.toExponential().replace('e+', 'e');
      return emit(exponential.length < decimal.length ? exponential : decimal);
    }
    if (kind === 'array') {
      emit('[');
      for (let index = 0; index < value.length; index += 1) { if (index) emit(','); walk(value[index], emit); }
      return emit(']');
    }
    if (kind === 'object') {
      emit('{');
      let first = true;
      for (const key of Object.keys(value)) {
        if (!first) emit(',');
        first = false;
        emit(JSON.stringify(key)); emit(':'); walk(value[key], emit);
      }
      return emit('}');
    }
    throw new TypeError('legacy structured capture must contain JSON values');
  };
  let size = 0;
  walk(capture, (part) => { size += Buffer.byteLength(part); assertStructuredBudget(size); });
  const groups = [];
  let pending = [];
  let pendingBytes = 0;
  walk(capture, (part) => {
    pending.push(part);
    pendingBytes += Buffer.byteLength(part);
    if (pendingBytes >= 65536) { groups.push(pending.join('')); pending = []; pendingBytes = 0; }
  });
  if (pending.length) groups.push(pending.join(''));
  return groups.join('');
};
const previewOf = (text) => text.length > 100 ? `${text.slice(0, 100)}...` : text;

// Raw structured content, excluding binary attachments and protocol envelopes.
// Count every occurrence: sharing a blob never discounts delivered aliases.
const logicalStructuredBytes = (item) => {
  const text = (value) => Buffer.byteLength(value ?? '');
  const json = (value) => value === undefined ? 0 : jsonByteLength(value, true);
  return text(item.text) + item.context.reduce((size, part) => size + text(part.text) + text(part.instructions) + json(part.metadata), 0)
    + (item.promptParts?.context ?? []).reduce((size, part) => size + text(part.text) + text(part.description) + json(part.metadata), 0)
    + json(item.promptParts?.metadata) + json(item.promptParts?.agents) + json(item.promptParts?.skills)
    + json(item.promptParts?.fileReferences)
    + (item.agentMention ? json([{ name: item.agentMention }]) : 0)
    // Preserve historical legacy counters and seals. Only the additive manifest
    // field changes a manifest occurrence's delivered model-facing name.
    + (item.attachmentManifest ?? item.attachments).reduce((size, file) => size
      + text(item.format === 'manifest-v1' ? file.agentPath ?? file.filename : file.filename)
      + text(file.description) + json(file.mention), 0);
};

export const referencesPayloadBlob = (item, id) => item.format === 'payload-v1'
  ? item.payloadManifest.chunks.some((chunk) => chunk.blobId === id) || item.payloadIndex.attachmentBlobIds.includes(id)
  : (item.attachmentManifest ?? []).some((file) => file.blobId === id);
