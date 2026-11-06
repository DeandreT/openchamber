import assert from 'node:assert/strict';
import { test } from 'vitest';
import { createManifestPromptBody, MANIFEST_PROMPT_BYTE_LIMIT } from './prompt-body.js';
import { BLOB_BYTE_LIMIT } from './payloads.js';

test('streamed manifest JSON has exact byte length and preserves padding, Unicode and official file fields', async () => {
  const source = [Buffer.alloc(0), Buffer.from([1]), Buffer.from([1, 2]), Buffer.from([1, 2, 3]), Buffer.alloc(49153, 0xff)];
  const files = source.map((bytes, index) => ({ attachmentId: String(index), size: bytes.length, filename: `文書-${index}.txt`, mimeType: 'text/plain',
    description: 'quoted "description"\n', mention: { start: 0, end: 4, text: '文書😀' } }));
  const body = { id: 'msg_exact_stream', delivery: 'queue', text: 'Unicode 😀\n"text"', agents: [{ name: 'reviewer' }], skills: [{ id: 'skill_preserved' }], metadata: { nested: [true, null, '文書'] } };
  let reads = 0;
  const plan = createManifestPromptBody(body, files, async (file) => { reads += 1; return source[Number(file.attachmentId)]; });
  assert.equal(reads, 0);
  const response = new Response(plan.createStream(new AbortController().signal));
  const wire = await response.text();
  const expected = { ...body, files: files.map((file, index) => ({ uri: `data:${file.mimeType};base64,${source[index].toString('base64')}`,
    name: file.filename, description: file.description, mention: file.mention })) };
  assert.equal(plan.contentLength, Buffer.byteLength(wire));
  assert.deepEqual(JSON.parse(wire), expected);
  assert.equal(reads, source.length);
});

test('manifest model-facing names stay literal per occurrence and never select the bytes read', async () => {
  const names = ['/original/文書?#%2F.txt', 'C:\\original folder\\source.txt', '\\\\server\\share\\source.txt',
    'relative/../literal\\name\t.txt', `long-${'a'.repeat(5000)}`];
  const files = names.map((agentPath) => ({ blobId: 'same-owned-blob', filename: 'safe.txt', agentPath, mimeType: 'text/plain', size: 3 }));
  files.push({ blobId: 'same-owned-blob', filename: 'safe.txt', mimeType: 'text/plain', size: 3 });
  let reads = 0;
  const plan = createManifestPromptBody({ text: 'captured' }, files, async (file) => {
    assert.equal(file.blobId, 'same-owned-blob');
    assert.equal(file.filename, 'safe.txt');
    reads += 1;
    return Buffer.from('abc');
  });
  assert.equal(reads, 0);
  const wire = await new Response(plan.createStream(new AbortController().signal)).text();
  assert.equal(plan.contentLength, Buffer.byteLength(wire));
  assert.deepEqual(JSON.parse(wire).files, [...names, 'safe.txt'].map((name) => ({ uri: 'data:text/plain;base64,YWJj', name })));
  assert.equal(reads, files.length);
});

test('2 full 40 MiB document-image bundles fit the 128 MiB policy and stream in bounded chunks', async () => {
  const bytes = Buffer.alloc(BLOB_BYTE_LIMIT, 0x42);
  const files = [0, 1, 2, 3].map((index) => ({ attachmentId: `image-${index}`, filename: `${index}.png`, mimeType: 'image/png', size: bytes.length }));
  let reads = 0;
  const plan = createManifestPromptBody({ text: 'document images' }, files, async () => { reads += 1; return bytes; });
  assert.ok(plan.contentLength > 50 * 1024 * 1024);
  assert.ok(plan.contentLength <= MANIFEST_PROMPT_BYTE_LIMIT);
  assert.equal(reads, 0);
  const reader = plan.createStream(new AbortController().signal).getReader();
  let length = 0;
  let maximum = 0;
  while (true) {
    const part = await reader.read();
    if (part.done) break;
    length += part.value.length;
    maximum = Math.max(maximum, part.value.length);
  }
  assert.equal(length, plan.contentLength);
  assert.ok(maximum <= 65536);
  assert.equal(reads, 4);
});

test('4096 references to a 20 MiB blob are refused before stream creation or any file read', () => {
  let reads = 0;
  const file = { filename: 'repeated.png', mimeType: 'image/png', size: BLOB_BYTE_LIMIT };
  const plan = createManifestPromptBody({ text: 'bounded preparation' }, Array(4096).fill(file), async () => { reads += 1; assert.fail('oversized plans must not read files'); });
  assert.ok(plan.contentLength > 100 * 1024 ** 3);
  assert.throws(() => plan.createStream(new AbortController().signal), { status: 413, code: 'queue_request_prompt_body_too_large' });
  assert.equal(reads, 0);
});

test('exact encoded lengths at 128 MiB minus 1, 128 MiB, and 128 MiB plus 1 use metadata-only budget checks', async () => {
  assert.equal(MANIFEST_PROMPT_BYTE_LIMIT, 128 * 1024 * 1024);
  let reads = 0;
  const read = async () => { reads += 1; assert.fail('boundary planning must not read or encode file bytes'); };
  const file = { filename: 'cap.bin', agentPath: '/original/cap.bin', mimeType: 'application/octet-stream', size: BLOB_BYTE_LIMIT };
  const files = [...Array(4).fill(file), { ...file, size: 0 }];
  const initial = createManifestPromptBody({ text: '' }, files, read).contentLength;
  const body = { text: 'x'.repeat((MANIFEST_PROMPT_BYTE_LIMIT - initial) % 4) };
  const overhead = createManifestPromptBody(body, files, read).contentLength;
  files[4] = { ...file, size: ((MANIFEST_PROMPT_BYTE_LIMIT - overhead) / 4) * 3 };
  assert.ok(Number.isSafeInteger(files[4].size) && files[4].size <= BLOB_BYTE_LIMIT);
  for (const delta of [-1, 0, 1]) {
    const last = { ...files[4], agentPath: delta < 0 ? file.agentPath.slice(1) : delta > 0 ? `${file.agentPath}x` : file.agentPath };
    const plan = createManifestPromptBody(body, [...files.slice(0, 4), last], read);
    assert.equal(plan.contentLength, MANIFEST_PROMPT_BYTE_LIMIT + delta);
    if (delta > 0) assert.throws(() => plan.createStream(new AbortController().signal), { status: 413 });
    else await plan.createStream(new AbortController().signal).cancel();
  }
  assert.equal(reads, 0);
});

test('an in-budget manifest still aborts under its RPC signal before another file allocation', async () => {
  const file = { filename: 'repeated.png', mimeType: 'image/png', size: BLOB_BYTE_LIMIT };
  let reads = 0;
  const plan = createManifestPromptBody({ text: 'bounded preparation' }, Array(4).fill(file), async () => {
    reads += 1;
    assert.equal(reads, 1, 'a regression must fail before allocating repeated large files');
    return Buffer.alloc(BLOB_BYTE_LIMIT);
  });
  assert.ok(plan.contentLength <= MANIFEST_PROMPT_BYTE_LIMIT);
  assert.equal(reads, 0);
  const controller = new AbortController();
  const reader = plan.createStream(controller.signal).getReader();
  assert.ok((await reader.read()).value.length <= 65536);
  assert.equal(reads, 0);
  assert.ok((await reader.read()).value.length <= 65536);
  assert.equal(reads, 0);
  assert.equal((await reader.read()).value.length, 65536);
  assert.equal(reads, 1);
  controller.abort(new Error('RPC deadline'));
  await assert.rejects(reader.read(), /RPC deadline/);
  assert.equal(reads, 1);
});

test('prepared bytes precede ordered references with exact metadata and no reference reads', async () => {
  const references = [
    { uri: 'file:///server/report.pdf', name: '/server/report.pdf', description: 'PDF "description"', mention: { start: 2, end: 6, text: '@pdf' } },
    { uri: 'file:///server/source.ts', name: 'source.ts' },
    { uri: 'file:///server/report.pdf', name: '/server/report.pdf', description: 'second occurrence' },
  ];
  for (const files of [[], [{ filename: 'prepared.txt', mimeType: 'text/plain', size: 3 }]]) {
    let reads = 0;
    const plan = createManifestPromptBody({ text: 'captured' }, files, async () => { reads += 1; return Buffer.from('abc'); }, references);
    const wire = await new Response(plan.createStream(new AbortController().signal)).text();
    assert.equal(plan.contentLength, Buffer.byteLength(wire));
    assert.deepEqual(JSON.parse(wire), { text: 'captured', files: [
      ...files.map((file) => ({ uri: 'data:text/plain;base64,YWJj', name: file.filename })), ...references,
    ] });
    assert.equal(reads, files.length);
  }
});
