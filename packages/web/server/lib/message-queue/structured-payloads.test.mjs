import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { test } from 'vitest';
import { BLOB_BYTE_LIMIT, parseStructuredItem, compactLegacyPayloadItem, parseManifestItem, capturedEntryCount, parseUploadMetadata } from './payloads.js';
import { parseQueuedItemInput } from './runtime.js';
import { captureQueueRequest, RECEIPT_BYTE_LIMIT } from './receipts.js';
import { STRUCTURED_BYTE_LIMIT, parsePayloadPointer, readStructuredCapture, assertStructuredBudget, encodeLegacyCapture, makePayloadIndex, sealPayloadPointer } from './structured-payloads.js';
import { jsonByteLength, createManifestPromptBody, MANIFEST_PROMPT_BYTE_LIMIT } from './prompt-body.js';

const DIRECTORY = '/structured/repository';
const hash = (bytes) => createHash('sha256').update(bytes).digest('hex');
const capture = () => ({ directory: DIRECTORY, item: {
  format: 'manifest-v1', content: 'draft', text: '', context: [], attachmentManifest: [],
  sendConfig: { providerID: 'provider', modelID: 'model' }, promptParts: { agents: [], skills: [], context: [] },
} });
const encoded = (bytes, chunkSize = BLOB_BYTE_LIMIT) => {
  const chunks = [];
  const parts = [];
  for (let offset = 0; offset < bytes.length; offset += chunkSize) {
    const ordinal = parts.length;
    const part = bytes.subarray(offset, offset + chunkSize);
    parts.push(part);
    chunks.push({ ordinal, blobId: hash(String(ordinal)), requestId: 'unit-payload-upload', attachmentId: `chunk-${ordinal}`,
      filename: `chunk-${ordinal}.json`, mimeType: 'application/json', size: part.length, sha256: hash(part) });
  }
  return { pointer: { format: 'payload-v1', payloadManifest: { version: 1, encoding: 'utf-8-json', totalBytes: bytes.length, sha256: hash(bytes), chunks } }, parts };
};

test('50 MiB typed capture equality and 20 MiB chunks are accepted without raising the 4 MiB pointer budget', async () => {
  const value = capture();
  value.item.text = 'x'.repeat(STRUCTURED_BYTE_LIMIT - Buffer.byteLength(JSON.stringify(value)));
  const bytes = Buffer.from(JSON.stringify(value));
  assert.equal(bytes.length, STRUCTURED_BYTE_LIMIT);
  const { pointer, parts } = encoded(bytes);
  assert.deepEqual(pointer.payloadManifest.chunks.map((chunk) => chunk.size), [BLOB_BYTE_LIMIT, BLOB_BYTE_LIMIT, STRUCTURED_BYTE_LIMIT - 2 * BLOB_BYTE_LIMIT]);
  assert.ok(Buffer.byteLength(JSON.stringify(pointer)) < RECEIPT_BYTE_LIMIT);
  const read = [];
  const logical = await readStructuredCapture(pointer, DIRECTORY, async (chunk) => { read.push(chunk.ordinal); return parts[chunk.ordinal]; });
  assert.equal(logical.text.length, value.item.text.length);
  assert.deepEqual(read, [0, 1, 2]);
  assertStructuredBudget(STRUCTURED_BYTE_LIMIT);
  assert.throws(() => assertStructuredBudget(STRUCTURED_BYTE_LIMIT + 1), { status: 413 });
}, 30000);

test('aggregate bytes and chunk ordinals, aliases and per-blob lengths fail before reads or concatenation', async () => {
  const value = capture();
  value.item.text = 'small valid capture';
  const { pointer } = encoded(Buffer.from(JSON.stringify(value)));
  const mutations = [
    (manifest) => { manifest.totalBytes = STRUCTURED_BYTE_LIMIT + 1; },
    (manifest) => { manifest.totalBytes += 1; },
    (manifest) => { manifest.chunks[0].size = BLOB_BYTE_LIMIT + 1; manifest.totalBytes = BLOB_BYTE_LIMIT + 1; },
    (manifest) => { manifest.chunks[0].ordinal = 2; },
    (manifest) => { manifest.chunks.push({ ...manifest.chunks[0], ordinal: 1 }); manifest.totalBytes *= 2; },
    (manifest) => { manifest.chunks = Array.from({ length: 3 }, (_, ordinal) => ({ ...manifest.chunks[0], ordinal, blobId: hash(String(ordinal)), size: BLOB_BYTE_LIMIT })); manifest.totalBytes = STRUCTURED_BYTE_LIMIT; },
    (manifest) => { manifest.chunks[0].fetchUrl = 'https://untrusted.invalid/data'; },
  ];
  let reads = 0;
  for (const mutation of mutations) {
    const changed = structuredClone(pointer);
    mutation(changed.payloadManifest);
    await assert.rejects(readStructuredCapture(changed, DIRECTORY, async () => { reads += 1; return Buffer.alloc(0); }));
  }
  assert.equal(reads, 0);
  const oversizedPointer = { ...pointer, unexpected: 'x'.repeat(RECEIPT_BYTE_LIMIT) };
  assert.throws(() => captureQueueRequest('enqueue', 'ses_structured', 'oversize-pointer-key', { directory: DIRECTORY, item: oversizedPointer }), { status: 413 });
});

test('UTF-8 code points split across chunks survive while malformed bytes, digests, schemas and owners fail closed', async () => {
  const value = capture();
  value.item.text = 'é🙂漢字';
  value.item.context = [{ kind: 'context', text: 'typed context', instructions: 'read it', metadata: { nested: ['é🙂', null, true] } }];
  const { pointer, parts } = encoded(Buffer.from(JSON.stringify(value)), 3);
  assert.deepEqual(await readStructuredCapture(pointer, DIRECTORY, async (chunk) => parts[chunk.ordinal]), { ...value.item, attachments: [] });
  await assert.rejects(readStructuredCapture(pointer, '/other/owner', async (chunk) => parts[chunk.ordinal]), { code: 'queue_request_conflict' });
  await assert.rejects(readStructuredCapture(pointer, DIRECTORY, async () => Buffer.alloc(1)), /length or SHA-256/);
  const badWhole = structuredClone(pointer);
  badWhole.payloadManifest.sha256 = '0'.repeat(64);
  await assert.rejects(readStructuredCapture(badWhole, DIRECTORY, async (chunk) => parts[chunk.ordinal]), /whole SHA-256/);
  for (const bytes of [Buffer.from([0xff]), Buffer.from('\ufeff{}'), Buffer.from('{'), Buffer.from(JSON.stringify({ ...value, unexpected: true }))]) {
    const bad = encoded(bytes);
    await assert.rejects(readStructuredCapture(bad.pointer, DIRECTORY, async (chunk) => bad.parts[chunk.ordinal]), TypeError);
  }
  assert.throws(() => parsePayloadPointer({ ...pointer, payloadIndex: {} }), TypeError);
});

test('strict legacy captures retain defaults, inline bytes, source metadata and frozen short or long attachment identities', async () => {
  const original = {
    content: 'draft', text: 'draft', agentMention: 'captured-agent', contextPreview: 'captured preview',
    context: [{ kind: 'context', text: 'context', instructions: 'read first', metadata: { numbers: [1e20, 1e23, 1e-7] } }],
    sendConfig: { providerID: 'provider', modelID: 'model', agent: 'frozen-agent', variant: 'frozen-variant' },
    attachments: [
      { id: 'original-short-id', filename: 'first.txt', mimeType: 'text/plain', size: 0, source: 'local', agentPath: '/metadata/only/original first.txt', dataUrl: 'data:text/plain;base64,YWJj' },
      { id: 'long'.repeat(100), filename: 'second.txt', mimeType: 'text/plain', size: 3, source: 'server', serverPath: '/captured/server/path', dataUrl: 'data:text/plain;base64,YWJj' },
    ],
  };
  const wire = { directory: DIRECTORY, item: compactLegacyPayloadItem(original) };
  const json = encodeLegacyCapture(wire);
  assert.ok(json.includes('1e20'));
  assert.ok(Buffer.byteLength(json) < Buffer.byteLength(JSON.stringify({ directory: DIRECTORY, item: original })));
  const { pointer, parts } = encoded(Buffer.from(json));
  const stored = sealPayloadPointer({ ...pointer, payloadIndex: makePayloadIndex(original, DIRECTORY, ['msg_instruction', 'msg_context'], 0) });
  assert.deepEqual(stored.payloadIndex.legacyAttachmentIds, ['original-short-id', null]);
  assert.deepEqual(await readStructuredCapture(parsePayloadPointer(stored, { stored: true }), DIRECTORY, async (chunk) => parts[chunk.ordinal]), original);
  for (const changed of [{ ...wire.item, unknown: true }, { ...wire.item, goal: { tokenBudget: null } },
    { ...wire.item, attachments: [{ ...wire.item.attachments[0], fetchUrl: 'https://invalid.example/file' }] }]) {
    assert.throws(() => parseStructuredItem(changed), TypeError);
  }
});

test('manifest model-facing identities preserve literal prepared-file names and reject malformed present fields', async () => {
  const file = { attachmentId: 'file', filename: 'safe.txt', mimeType: 'text/plain', size: 3,
    sha256: hash('abc'), blobId: hash('file'), requestId: 'original-upload' };
  const source = { ...capture().item, attachmentManifest: [file] };
  const omitted = parseManifestItem(source);
  assert.equal(Object.hasOwn(omitted.attachmentManifest[0], 'agentPath'), false);
  assert.deepEqual(omitted.attachmentManifest, [file]);
  for (const agentPath of ['relative/../source.txt', 'C:\\original folder\\source.txt', '\\\\server\\share\\source.txt',
    '/posix/literal\\name?#%2F.txt', ' \tunchanged name\t ', `/model/${'a'.repeat(5000)}.txt`]) {
    const named = { ...source, attachmentManifest: [{ ...file, agentPath }] };
    assert.equal(parseManifestItem(named).attachmentManifest[0].agentPath, agentPath);
    const { pointer, parts } = encoded(Buffer.from(JSON.stringify({ directory: DIRECTORY, item: named })));
    const restored = await readStructuredCapture(pointer, DIRECTORY, async (chunk) => parts[chunk.ordinal]);
    assert.equal(restored.attachmentManifest[0].agentPath, agentPath);
    assert.equal(restored.attachmentManifest[0].filename, 'safe.txt');
  }
  for (const agentPath of [null, '', 1, true, [], {}]) {
    assert.throws(() => parseManifestItem({ ...source, attachmentManifest: [{ ...file, agentPath }] }), TypeError);
  }
  const { blobId: _blobId, requestId: _requestId, ...metadata } = file;
  assert.throws(() => parseUploadMetadata('ses_structured', 'original-upload', 'file', { ...metadata, agentPath: '/original/source.txt' }), TypeError,
    'per-use model identity must not enter immutable upload metadata');
  const { pointer } = encoded(Buffer.from(JSON.stringify({ directory: DIRECTORY, item: source })));
  pointer.payloadManifest.chunks[0].agentPath = '/not/a/chunk/field';
  assert.throws(() => parsePayloadPointer(pointer), TypeError);
});

test('manifest identities count every delivered occurrence while legacy counters and shared blob roots stay unchanged', () => {
  const file = { attachmentId: 'file', filename: 'safe.txt', mimeType: 'text/plain', size: 3,
    sha256: hash('abc'), blobId: hash('file'), requestId: 'original-upload' };
  const names = ['/original/文書.txt', 'relative/literal\\name\t.txt'];
  const baseline = makePayloadIndex(parseManifestItem({ ...capture().item, attachmentManifest: [file, file] }), DIRECTORY, [], 0);
  const named = makePayloadIndex(parseManifestItem({ ...capture().item,
    attachmentManifest: names.map((agentPath) => ({ ...file, agentPath })) }), DIRECTORY, [], 0);
  assert.equal(named.entryCount, 2);
  assert.deepEqual(named.attachmentBlobIds, [file.blobId]);
  assert.equal(named.structuredBytes - baseline.structuredBytes,
    names.reduce((size, name) => size + Buffer.byteLength(name) - Buffer.byteLength(file.filename), 0));
  const legacy = parseStructuredItem({ content: 'legacy', sendConfig: capture().item.sendConfig,
    attachments: [{ id: 'legacy', filename: 'safe.txt', mimeType: 'text/plain', dataUrl: 'data:,abc' }] });
  const originalIndex = makePayloadIndex(legacy, DIRECTORY, [], 0);
  legacy.attachments[0].agentPath = names[0];
  assert.deepEqual(makePayloadIndex(legacy, DIRECTORY, [], 0), originalIndex,
    'historical legacy accounting is part of retained root seals');
});

test('a legacy-valid 50 MiB context retains the original aggregate boundary after typed capture', async () => {
  const value = { directory: DIRECTORY, item: { context: [{ kind: 'synthetic', text: '' }], sendConfig: { providerID: 'provider', modelID: 'model' } } };
  value.item.context[0].text = 'x'.repeat(STRUCTURED_BYTE_LIMIT - Buffer.byteLength(JSON.stringify(value)));
  assert.equal(Buffer.byteLength(JSON.stringify(value)), STRUCTURED_BYTE_LIMIT);
  const parsedLegacy = parseQueuedItemInput(value.item);
  const json = encodeLegacyCapture({ directory: DIRECTORY, item: compactLegacyPayloadItem(parsedLegacy) });
  assert.equal(Buffer.byteLength(json), STRUCTURED_BYTE_LIMIT);
  const { pointer, parts } = encoded(Buffer.from(json));
  const captured = await readStructuredCapture(pointer, DIRECTORY, async (chunk) => parts[chunk.ordinal]);
  assert.equal(captured.context[0].text, value.item.context[0].text);
  assert.equal(captured.content, '');
  assert.equal(captured.text, '');
}, 30000);

test('JSON preflight measures numeric amplification before allocating an encoded prompt', async () => {
  const numbers = Array(10000).fill(1e20);
  const body = { text: 'captured', metadata: { numbers }, optional: undefined };
  assert.equal(jsonByteLength(body), Buffer.byteLength(JSON.stringify(body)));
  assert.equal(jsonByteLength(body, true), Buffer.byteLength(encodeLegacyCapture(bodyWithoutUndefined(body))));
  assert.equal(createManifestPromptBody(body, [], async () => assert.fail('no file read expected')).contentLength, jsonByteLength(body));
  const expected = JSON.parse(JSON.stringify(body));
  const frozen = createManifestPromptBody(body, [], async () => assert.fail('no file read expected'));
  body.text = 'changed after capture';
  const sent = await new Response(frozen.createStream(new AbortController().signal)).text();
  assert.equal(Buffer.byteLength(sent), frozen.contentLength);
  assert.deepEqual(JSON.parse(sent), expected);
  // A compact source can fit 50 MiB while an actual encoded request exceeds
  // 128 MiB. Measure it without constructing that amplified JSON string.
  const amplified = { text: 'captured', metadata: { numbers: Array(7_000_000).fill(1e20) } };
  assert.ok(jsonByteLength(amplified, true) < STRUCTURED_BYTE_LIMIT);
  const plan = createManifestPromptBody(amplified, [], async () => assert.fail('no file read expected'));
  assert.ok(plan.contentLength > MANIFEST_PROMPT_BYTE_LIMIT);
  assert.throws(() => plan.createStream(new AbortController().signal), { status: 413 });
}, 30000);

const bodyWithoutUndefined = ({ optional: _optional, ...body }) => body;

test('absent and empty reference lists retain old normalized captures and index hashes', () => {
  const original = capture().item;
  original.text = 'old byte-compatible capture';
  const old = parseManifestItem(original);
  const empty = parseManifestItem({ ...original, promptParts: { ...original.promptParts, fileReferences: [] } });
  assert.deepEqual(empty, old);
  assert.equal(Object.hasOwn(old.promptParts, 'fileReferences'), false);
  const { pointer } = encoded(Buffer.from(JSON.stringify({ directory: DIRECTORY, item: original })));
  assert.deepEqual(sealPayloadPointer({ ...pointer, payloadIndex: makePayloadIndex(empty, DIRECTORY, [], 42) }),
    sealPayloadPointer({ ...pointer, payloadIndex: makePayloadIndex(old, DIRECTORY, [], 42) }));
  const legacy = parseStructuredItem({ content: 'old', sendConfig: original.sendConfig });
  assert.equal(Object.hasOwn(legacy, 'promptParts'), false);
  assert.equal(JSON.stringify(compactLegacyPayloadItem(legacy)), JSON.stringify({ content: 'old', sendConfig: original.sendConfig }));
});

test('typed file references round-trip in both capture forms and count occurrences without acquiring attachment roots', async () => {
  const reference = { uri: 'file:///server/path/report.pdf', name: '/server/path/report.pdf', description: 'original PDF', mention: { start: 0, end: 4, text: '@pdf' } };
  for (const source of [
    { ...capture().item, text: '', promptParts: { fileReferences: [reference, reference] } },
    { content: '', sendConfig: capture().item.sendConfig, promptParts: { fileReferences: [reference, reference] } },
  ]) {
    const logical = parseStructuredItem(source);
    assert.equal(capturedEntryCount(logical), 2);
    const stored = encoded(Buffer.from(JSON.stringify({ directory: DIRECTORY, item: source })));
    assert.deepEqual((await readStructuredCapture(stored.pointer, DIRECTORY, async (chunk) => stored.parts[chunk.ordinal])).promptParts.fileReferences, [reference, reference]);
    const index = makePayloadIndex(logical, DIRECTORY, [], 0);
    assert.deepEqual(index.attachmentBlobIds, []);
    assert.deepEqual(index.contextIds, []);
    assert.ok(index.structuredBytes >= Buffer.byteLength(JSON.stringify([reference, reference])));
    assert.throws(() => parseStructuredItem({ ...source, promptParts: { fileReferences: Array(4097).fill(reference) } }), TypeError);
  }
});

test('typed references reject unsafe URIs, unknown fields and malformed names or mention bounds', () => {
  const reference = { uri: 'file:///server/report.pdf', name: 'report.pdf' };
  const invalid = [
    ...['https://example.test/report.pdf', 'data:application/pdf;base64,YQ==', 'file://host/report.pdf', 'file://localhost/report.pdf',
      'file:relative.pdf', 'file:////host/report.pdf', 'file:///report.pdf?', 'file:///report.pdf#', 'file:///bad%ZZ', 'file:///bad%00', 'file:///raw space.pdf', 'file:///bad\\path'].map((uri) => ({ ...reference, uri })),
    { ...reference, name: '' }, { ...reference, name: '🙂'.repeat(1025) }, { ...reference, blobId: '0'.repeat(64) },
    ...[{ start: -1, end: 2, text: 'bad' }, { start: 3, end: 2, text: 'bad' }, { start: 0.5, end: 2, text: 'bad' },
      { start: 0, end: Number.MAX_SAFE_INTEGER + 1, text: 'bad' }].map((mention) => ({ ...reference, mention })),
  ];
  for (const file of invalid) assert.throws(() => parseStructuredItem({ content: '', sendConfig: capture().item.sendConfig, promptParts: { fileReferences: [file] } }), TypeError);
});

test('reference URI dot-segment normalization cannot erase malformed escapes or encoded controls at public parsers', () => {
  const parse = [
    (uri) => parseManifestItem({ ...capture().item, text: 'inspect', promptParts: { fileReferences: [{ uri, name: 'report.pdf' }] } }),
    (uri) => parseStructuredItem({ content: 'inspect', sendConfig: capture().item.sendConfig, promptParts: { fileReferences: [{ uri, name: 'report.pdf' }] } }),
    (uri) => parseQueuedItemInput({ content: 'inspect', sendConfig: capture().item.sendConfig, promptParts: { fileReferences: [{ uri, name: 'report.pdf' }] } }),
  ];
  for (const uri of [
    'file:///bad%ZZ/../report.pdf', 'file:///bad%00/../report.pdf', 'file:///bad%0a/../report.pdf',
    'file:///bad%C2%80/../report.pdf', 'file:///bad%ZZ/%2e%2e/report.pdf', 'file:///bad%00/./../report.pdf',
    'file:///raw\u00a0space.pdf', 'file:///raw\u2003space/../report.pdf', 'file:///raw\u2028space.pdf',
  ]) {
    for (const parser of parse) assert.throws(() => parser(uri), TypeError, `the original URI must be validated before normalization: ${uri}`);
  }
});

test('reference URI dot-segment positive controls preserve encoded spaces, Unicode and ordinary path segments', () => {
  for (const uri of [
    'file:///folder%20name/report.pdf', 'file:///caf%C3%A9/%E6%96%87%E6%9B%B8.pdf',
    'file:///ordinary/../report.pdf', 'file:///ordinary/./report.pdf', 'file:///ordinary/%2e%2e/report.pdf',
  ]) {
    const value = { content: 'inspect', sendConfig: capture().item.sendConfig, promptParts: { fileReferences: [{ uri, name: 'report.pdf' }] } };
    assert.equal(parseStructuredItem(value).promptParts.fileReferences[0].uri, uri);
    assert.equal(parseQueuedItemInput(value).promptParts.fileReferences[0].uri, uri);
    assert.equal(parseManifestItem({ ...capture().item, text: 'inspect', promptParts: value.promptParts }).promptParts.fileReferences[0].uri, uri);
  }
});
