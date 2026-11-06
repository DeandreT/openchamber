import { z } from 'zod';

export const BLOB_BYTE_LIMIT = 20 * 1024 * 1024;
export const attachmentIdSchema = z.string().regex(/^[A-Za-z0-9_-]{1,128}$/);
const identifier = z.string().min(1).max(4096).refine((value) => !/[\u0000-\u001f\u007f-\u009f]/.test(value));
const filename = identifier.refine((value) => value !== '.' && value !== '..' && !/[\\/]/.test(value));
// Upload labels are single components. Keep the original model-facing identity
// separately when a path or literal filename cannot use that strict vocabulary.
export const queueUploadFilename = (value, fallback) => {
  const separated = /^(?:[A-Za-z]:\\|\\\\)/.test(value) ? value.replaceAll('\\', '/') : value;
  const basename = separated.split('/').at(-1);
  return filename.safeParse(basename).success ? basename : filename.parse(fallback);
};
const mimeType = z.string().max(255).regex(/^[A-Za-z0-9!#$&^_.+-]+\/[A-Za-z0-9!#$&^_.+-]+$/);
const digest = z.string().regex(/^[a-f0-9]{64}$/);
const metadata = z.record(z.string(), z.json());
const mention = z.object({ start: z.number().int().nonnegative().safe(), end: z.number().int().nonnegative().safe(), text: z.string() }).strict()
  .refine((value) => value.end >= value.start);
// A reference carries an OpenCode file identity only. It grants no queue byte
// ownership and must never be passed to the blob store or read by this owner.
const fileReferenceUri = z.string().max(65536).refine((value) => {
  if (!value.startsWith('file:///') || value.startsWith('file:////')
      || /[\\?#\s\u0000-\u001f\u007f-\u009f]/.test(value) || Buffer.byteLength(value) > 65536) return false;
  try {
    // Check the captured path before URL parsing can remove dot segments.
    const path = decodeURIComponent(value.slice('file://'.length));
    const url = new URL(value);
    return url.protocol === 'file:' && !url.host && !url.search && !url.hash && path.startsWith('/')
      && !/[\u0000-\u001f\u007f-\u009f]/.test(path);
  } catch { return false; }
});
const fileReferenceSchema = z.object({
  uri: fileReferenceUri, name: identifier.refine((value) => Buffer.byteLength(value) <= 4096),
  description: z.string().optional(), mention: mention.optional(),
}).strict();
const blobMetadataSchema = z.object({
  attachmentId: attachmentIdSchema, filename, mimeType,
  size: z.number().int().nonnegative().max(BLOB_BYTE_LIMIT), sha256: digest,
}).strict();
export const blobDescriptorSchema = blobMetadataSchema.extend({
  blobId: digest, requestId: z.string().regex(/^[A-Za-z0-9_-]{8,128}$/),
}).strict();
const blobManifestSchema = blobDescriptorSchema.extend({
  description: z.string().optional(), mention: mention.optional(),
  // Prepared-file identities keep the legacy name vocabulary and grant no reads.
  agentPath: z.string().min(1).optional(),
}).strict();
export const storedBlobSchema = z.object({
  metadata: blobMetadataSchema.extend({
    version: z.literal(1), blobId: digest,
    sessionId: z.string().regex(/^[A-Za-z0-9_-]{4,128}$/),
    directory: z.string().min(1).max(16384).refine((value) => value === value.trim()), requestId: z.string().regex(/^[A-Za-z0-9_-]{8,128}$/),
  }).strict(),
  state: z.enum(['available', 'released']),
}).strict();
const context = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('context'), text: z.string(), metadata, instructions: z.string().optional() }).strict(),
  z.object({ kind: z.literal('instruction'), text: z.string() }).strict(),
  z.object({ kind: z.literal('synthetic'), text: z.string() }).strict(),
]);
export const promptPartsSchema = z.object({
  agents: z.array(z.object({ name: identifier, mention: mention.optional() }).strict()).max(4096).default([]),
  skills: z.array(z.object({ id: identifier, mention: mention.optional() }).strict()).max(4096).default([]),
  metadata: metadata.optional(),
  context: z.array(z.object({
    id: z.string().regex(/^msg_[A-Za-z0-9_-]{1,120}$/).optional(), text: z.string().min(1),
    description: z.string().optional(), metadata: metadata.optional(),
  }).strict()).max(4096).default([]),
  // Keep omitted and empty references absent after normalization so old
  // captures and their verified root-index hashes keep their exact defaults.
  fileReferences: z.array(fileReferenceSchema).max(4096).optional(),
}).strict().transform(({ fileReferences, ...parts }) => fileReferences?.length ? { ...parts, fileReferences } : parts);
// Default adapters can hand file references back with ordinary inline items.
// Other typed prompt parts retain their existing manifest-only handoff boundary.
export const referencePromptPartsSchema = promptPartsSchema.refine((parts) => !parts.agents.length && !parts.skills.length
  && !parts.context.length && parts.metadata === undefined);
export const manifestSendConfigSchema = z.object({ providerID: identifier, modelID: identifier, agent: identifier.optional(), variant: identifier.optional() }).strict();
const manifestItemSchema = z.object({
  format: z.literal('manifest-v1'), content: z.string().max(200000), text: z.string(),
  attachmentManifest: z.array(blobManifestSchema).max(4096),
  context: z.array(context).max(4096).default([]), contextPreview: z.string().max(103).optional(),
  sendConfig: manifestSendConfigSchema,
  promptParts: promptPartsSchema.default({ agents: [], skills: [], context: [] }),
}).strict();

export const capturedEntryCount = (item) => item.format === 'payload-v1' ? item.payloadIndex.entryCount : (item.format === 'manifest-v1' ? item.attachmentManifest.length : item.attachments.length)
  + (item.agentMention ? 1 : 0) + (item.promptParts?.agents.length ?? 0) + (item.promptParts?.skills.length ?? 0)
  + (item.promptParts?.fileReferences?.length ?? 0)
  + item.context.reduce((count, entry) => count + (entry.instructions ? 2 : 1), 0) + (item.promptParts?.context.length ?? 0);

export const parseManifestItem = (value, { allowEmpty = false } = {}) => {
  const parsed = manifestItemSchema.safeParse(value);
  if (!parsed.success) throw new TypeError('invalid manifest item or promptParts');
  const item = parsed.data;
  if (!allowEmpty && !item.text.trim() && !item.attachmentManifest.length && !item.context.length && !item.promptParts.context.length && !item.promptParts.fileReferences?.length) {
    throw new TypeError('manifest item needs text, attachments, or context');
  }
  if (item.context.some((entry) => !entry.text.trim() || (entry.instructions !== undefined && !entry.instructions.trim()))
      || item.promptParts.context.some((entry) => !entry.text.trim())) throw new TypeError('blank captured context');
  if (capturedEntryCount(item) > 4096) {
    throw new TypeError('captured prompt exceeds entry limit');
  }
  return { ...item, attachments: [] };
};

// A strict typed legacy capture avoids adding manifest envelope/descriptor
// overhead to an original request already at the 50 MiB JSON boundary.
const legacyPayloadItemSchema = z.object({
  content: z.string().max(200000).optional(), text: z.string().optional(),
  agentMention: identifier.optional(), contextPreview: z.string().max(103).optional(),
  context: z.array(context).max(4096).default([]), sendConfig: manifestSendConfigSchema,
  promptParts: referencePromptPartsSchema.optional(),
  attachments: z.array(z.object({
    id: z.string().min(1).optional(), filename: z.string().min(1), mimeType: z.string().min(1),
    size: z.number().int().nonnegative().default(0), source: z.enum(['local', 'server', 'vscode']).default('local'),
    serverPath: z.string().min(1).optional(), agentPath: z.string().min(1).optional(), dataUrl: z.string().min(1),
  }).strict()).max(4096).default([]),
}).strict();

export const parseStructuredItem = (value, { allowEmpty = false, attachmentIds, seed } = {}) => {
  if (value?.format !== undefined) return parseManifestItem(value, { allowEmpty });
  const parsed = legacyPayloadItemSchema.safeParse(value);
  if (!parsed.success) throw new TypeError('invalid typed legacy payload item');
  const raw = parsed.data;
  const content = raw.content ?? '';
  const text = raw.text ?? content;
  if (attachmentIds && attachmentIds.length !== raw.attachments.length) throw new TypeError('legacy attachment identity index differs');
  const attachments = raw.attachments.map((file, index) => {
    const captured = attachmentIds?.[index];
    if (captured && file.id && captured !== file.id) throw new TypeError('legacy attachment identity differs');
    return { ...file, id: captured ?? file.id ?? `attachment-${seed}-${index}` };
  });
  const item = { ...raw, content, text, attachments };
  if ((!allowEmpty && !text.trim() && !attachments.length && !raw.context.length && !raw.promptParts?.fileReferences?.length && !raw.promptParts?.context.length)
      || raw.context.some((part) => !part.text.trim() || (part.instructions !== undefined && !part.instructions.trim()))
      || capturedEntryCount(item) > 4096) throw new TypeError('invalid typed legacy payload content');
  return item;
};

// The compact capture is encoded and hashed: which keys are present, and their
// order, are part of its identity.
const compactLegacyAttachment = ({ id, size, source, ...file }) => {
  const compact = { ...file };
  if (size) compact.size = size;
  if (source !== 'local') compact.source = source;
  // Short IDs include identities generated by the legacy parser. Their
  // verified root bindings avoid adding bytes to the original JSON budget.
  if (id.length > 256) compact.id = id;
  return compact;
};

export const compactLegacyPayloadItem = (item) => {
  const compact = {};
  if (item.content) compact.content = item.content;
  if (item.text !== item.content) compact.text = item.text;
  if (item.agentMention) compact.agentMention = item.agentMention;
  if (item.contextPreview !== undefined) compact.contextPreview = item.contextPreview;
  if (item.context.length) compact.context = item.context;
  compact.sendConfig = item.sendConfig;
  if (item.promptParts) compact.promptParts = item.promptParts;
  if (item.attachments.length) compact.attachments = item.attachments.map(compactLegacyAttachment);
  return compact;
};

export const parseUploadMetadata = (sessionId, requestId, attachmentId, input) => {
  const parsed = blobMetadataSchema.safeParse({ ...input, attachmentId });
  if (!parsed.success) throw new TypeError('invalid upload metadata');
  return { version: 1, sessionId, requestId, ...parsed.data };
};

export const decodeInlineAttachment = (file) => {
  const match = /^data:([^,]*),(.*)$/s.exec(file.dataUrl);
  if (!match) throw new TypeError('transfer requires actual inline attachment bytes');
  let bytes;
  if (match[1].endsWith(';base64')) {
    if (match[2].length > Math.ceil(BLOB_BYTE_LIMIT / 3) * 4) throw Object.assign(new Error('attachment exceeds prepared file limit'), { status: 413 });
    if (match[2].length % 4 !== 0 || /[^A-Za-z0-9+/=]/.test(match[2])) throw new TypeError('invalid attachment base64');
    bytes = Buffer.from(match[2], 'base64');
    if (bytes.toString('base64') !== match[2]) throw new TypeError('invalid attachment base64');
  } else {
    try { bytes = Buffer.from(decodeURIComponent(match[2]), 'utf8'); } catch { throw new TypeError('invalid attachment dataUrl'); }
  }
  if (bytes.length > BLOB_BYTE_LIMIT) throw Object.assign(new Error('attachment exceeds prepared file limit'), { status: 413 });
  return bytes;
};
