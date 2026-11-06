import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { test, vi } from 'vitest';
import { createMessageQueueRuntime, registerMessageQueueRoutes } from './runtime.js';
import { RECEIPT_BYTE_LIMIT } from './receipts.js';
import { BLOB_BYTE_LIMIT } from './payloads.js';
import { MANIFEST_PROMPT_BYTE_LIMIT } from './prompt-body.js';
import { STRUCTURED_BYTE_LIMIT } from './structured-payloads.js';

const SESSION = 'ses_receipt_test';
const DIRECTORY = '/captured/repository';
const item = (overrides = {}) => ({
  content: 'captured draft', text: 'captured draft', attachments: [], context: [],
  sendConfig: { providerID: 'provider', modelID: 'model', agent: 'build' }, ...overrides,
});
const input = (overrides = {}) => ({ directory: DIRECTORY, item: item(overrides) });
const deferred = () => {
  let resolve;
  const promise = new Promise((done) => { resolve = done; });
  return { promise, resolve };
};
const until = async (predicate) => {
  const deadline = Date.now() + 5000;
  while (!predicate()) {
    if (Date.now() > deadline) assert.fail('observable queue transition did not complete');
    await new Promise((resolve) => setTimeout(resolve, 2));
  }
};

const fixture = async (t, options = {}) => {
  const root = '/tmp/opencode';
  await fs.promises.mkdir(root, { recursive: true });
  const dataDir = options.dataDir ?? await fs.promises.mkdtemp(path.join(root, 'queue-receipts-'));
  const state = options.state ?? { held: true, inbox: new Map(), posts: [], commands: [], fail: null, badInbox: null };
  const fetchImpl = async (url, init = {}) => {
    if (options.onRequest) {
      const response = await options.onRequest(url, init);
      if (response !== undefined) return response;
    }
    const pathname = new URL(url).pathname;
    const method = init.method ?? 'GET';
    if (pathname === '/api/session/active') {
      state.activeReads = (state.activeReads ?? 0) + 1;
      return Response.json({ data: state.active ?? {} });
    }
    if (pathname.endsWith('/message')) return Response.json({ data: [], cursor: {} });
    if (pathname === '/api/command') {
      (state.commandReads ??= []).push(new URL(url));
      const scoped = new URL(url).searchParams.get('location[directory]') === DIRECTORY;
      return Response.json({ data: !state.commandDirectoryOnly || scoped ? state.commands : [] });
    }
    if (pathname.endsWith('/inbox')) {
      const session = pathname.split('/')[3];
      return Response.json({ data: state.badInbox ?? [...state.inbox.values()].filter((entry) => entry.sessionID === session) });
    }
    if (pathname.includes('/message/')) return new Response('not found', { status: 404 });
    if (method === 'POST') {
      const body = await new Response(init.body).json();
      if (pathname.endsWith('/model') || pathname.endsWith('/agent')) return new Response(null, { status: 204 });
      const type = pathname.endsWith('/synthetic') ? 'synthetic' : 'user';
      state.posts.push({ pathname, body });
      const admitted = { id: body.id ?? `msg_legacy_${state.posts.length}`, type,
        sessionID: pathname.split('/')[3], delivery: body.delivery ?? 'queue', time: { created: 1 }, payload: {} };
      if (state.fail?.before && state.fail.match(pathname, body)) {
        state.fail = null;
        throw new Error('request failed before admission');
      }
      state.inbox.set(admitted.id, admitted);
      if (state.fail?.match(pathname, body)) {
        state.fail = null;
        throw new Error('admitted response was lost');
      }
      return Response.json({ data: admitted });
    }
    return new Response('not found', { status: 404 });
  };
  const broadcasts = [];
  const runtime = createMessageQueueRuntime({
    globalEventHub: { subscribeEvent: () => () => {}, subscribeStatus: () => () => {} },
    buildOpenCodeUrl: (pathname) => `http://opencode.test${pathname}`,
    getOpenCodeAuthHeaders: () => ({}), fetchImpl, dataDir,
    dispatchQuietMs: 0, abortHoldMs: 0, retryDelayMs: () => 1,
    isDeliveryHeld: () => state.held, broadcastGlobalUiEvent: (event) => broadcasts.push(event),
  });
  await runtime.load();
  t.onTestFinished(async () => {
    runtime.stop();
    await runtime.flush();
    if (!options.dataDir) await fs.promises.rm(dataDir, { recursive: true, force: true });
  });
  return { runtime, state, dataDir, broadcasts, file: path.join(dataDir, 'message-queue.json') };
};

// Observe the queue's zero-delay callback itself, including callbacks that
// bail before any OpenCode read. No sleep guesses whether an idle signal ran.
const fireIdleTimer = async (runtime, sessionId = SESSION) => {
  const fired = deferred();
  const original = globalThis.setTimeout;
  const mock = vi.spyOn(globalThis, 'setTimeout').mockImplementation((callback, delay, ...args) => original((...values) => {
    callback(...values);
    if (delay === 0) fired.resolve();
  }, delay, ...args));
  try {
    runtime.processPayload({ type: 'session.status', properties: { sessionID: sessionId, status: { type: 'idle' } } });
    await fired.promise;
  } finally { mock.mockRestore(); }
};

test('lost enqueue response, concurrent repeat and restart retain one immutable receipt and full bytes', async (t) => {
  const first = await fixture(t);
  const attachment = { id: 'file', filename: 'bytes.txt', mimeType: 'text/plain', size: 3,
    source: 'server', serverPath: '/owned/file', dataUrl: 'data:text/plain;base64,YWJj' };
  const body = input({ attachments: [attachment], context: [{ kind: 'context', text: 'diff', instructions: 'read it', metadata: { kind: 'test' } }] });
  const replies = await Promise.all(Array.from({ length: 5 }, () => first.runtime.enqueueRequest(SESSION, 'enqueue-request-1', body)));
  assert.equal(first.runtime.sessionSnapshot(SESSION).items.length, 1);
  for (const reply of replies) assert.deepEqual(reply.receipt, replies[0].receipt);
  assert.equal(replies[0].receipt.contextIds.length, 2);
  assert.deepEqual(JSON.parse(await fs.promises.readFile(first.file, 'utf8')).sessions[SESSION].items[0].attachments, [attachment]);
  first.runtime.stop();
  const restored = await fixture(t, { dataDir: first.dataDir });
  const reply = await restored.runtime.enqueueRequest(SESSION, 'enqueue-request-1', body);
  assert.deepEqual(reply.receipt, replies[0].receipt);
  assert.equal(restored.runtime.sessionSnapshot(SESSION).items.length, 1);
  assert.deepEqual((await restored.runtime.getRequest(SESSION, 'enqueue-request-1', DIRECTORY)).receipt, reply.receipt);
  for (const changed of [input({ text: 'changed' }), { ...body, directory: '/other' }, { ...body, item: { ...body.item, ignored: true } }]) {
    await assert.rejects(restored.runtime.enqueueRequest(SESSION, 'enqueue-request-1', changed), { code: 'queue_request_conflict' });
  }
  await assert.rejects(restored.runtime.enqueueRequest('ses_other', 'enqueue-request-1', body), { code: 'queue_request_conflict' });
  await assert.rejects(restored.runtime.takeRequest(SESSION, 'enqueue-request-1', { directory: DIRECTORY, itemIds: [reply.receipt.itemId] }), { code: 'queue_request_conflict' });
  await assert.rejects(restored.runtime.getRequest(SESSION, 'unknown-request-1', DIRECTORY), { code: 'queue_request_unknown', status: 404 });
});

test('enqueue acknowledges and dispatches only after file and directory sync complete', async (t) => {
  const f = await fixture(t);
  const entered = deferred();
  const release = deferred();
  const original = fs.promises.open.bind(fs.promises);
  let paused = false;
  const mock = vi.spyOn(fs.promises, 'open').mockImplementation(async (...args) => {
    const handle = await original(...args);
    if (String(args[0]).endsWith('.tmp') && !paused) {
      paused = true;
      const sync = handle.sync.bind(handle);
      handle.sync = async () => { entered.resolve(); await release.promise; await sync(); };
    }
    return handle;
  });
  t.onTestFinished(() => mock.mockRestore());
  f.state.held = false;
  let answered = false;
  const accepted = f.runtime.enqueueRequest(SESSION, 'enqueue-sync-1', input()).then((reply) => { answered = true; return reply; });
  await entered.promise;
  f.runtime.release(SESSION);
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(answered, false);
  assert.deepEqual(f.state.posts, []);
  await assert.rejects(f.runtime.getRequest(SESSION, 'enqueue-sync-1', DIRECTORY), { code: 'queue_request_pending' });
  release.resolve();
  const reply = await accepted;
  await until(() => f.runtime.sessionSnapshot(SESSION).items.length === 0);
  assert.equal(f.state.posts[0].body.id, reply.receipt.messageId);
});

test('persistence faults at start, middle and end preserve other queues and retry only the exact request', async (t) => {
  for (const failedIndex of [0, 1, 2]) {
    const f = await fixture(t);
    const rename = fs.promises.rename.bind(fs.promises);
    let fail = false;
    const mock = vi.spyOn(fs.promises, 'rename').mockImplementation(async (...args) => {
      if (fail && args[1] === f.file) throw Object.assign(new Error('disk write refused'), { code: 'EIO' });
      return rename(...args);
    });
    t.onTestFinished(() => mock.mockRestore());
    for (let index = 0; index < 3; index += 1) {
      fail = index === failedIndex;
      const call = f.runtime.enqueueRequest(`ses_independent_${index}`, `enqueue-${failedIndex}-${index}`, input());
      if (fail) await assert.rejects(call, /disk write refused/);
      else await call;
    }
    fail = false;
    const recovery = await f.runtime.enqueueRequest(`ses_independent_${failedIndex}`, `enqueue-${failedIndex}-${failedIndex}`, input());
    for (let index = 0; index < 3; index += 1) assert.equal(f.runtime.sessionSnapshot(`ses_independent_${index}`).items.length, 1);
    assert.equal((await f.runtime.getRequest(`ses_independent_${failedIndex}`, `enqueue-${failedIndex}-${failedIndex}`, DIRECTORY)).receipt.itemId, recovery.receipt.itemId);
    mock.mockRestore();
  }
});

test('uncertain prompt admission stays held through events and restart until explicit retry observes its stable ID', async (t) => {
  const f = await fixture(t);
  const body = input({ context: [{ kind: 'synthetic', text: 'captured context' }] });
  const accepted = await f.runtime.enqueueRequest(SESSION, 'uncertain-prompt-1', body);
  f.state.fail = { match: (pathname) => pathname.endsWith('/prompt') };
  f.state.held = false;
  f.runtime.release(SESSION);
  await until(() => f.state.posts.length === 2 && f.runtime.sessionSnapshot(SESSION).sendingId === null);
  f.runtime.processPayload({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
  f.runtime.release(SESSION);
  const held = await f.runtime.getRequest(SESSION, 'uncertain-prompt-1', DIRECTORY);
  assert.equal(held.state, 'uncertain');
  assert.equal(f.state.posts.length, 2);
  await assert.rejects(f.runtime.take(SESSION, accepted.receipt.itemId), { status: 409 });
  await assert.rejects(f.runtime.takeRequest(SESSION, 'unsafe-take-1', { directory: DIRECTORY, itemIds: [accepted.receipt.itemId] }), { status: 409 });
  f.runtime.stop();
  await f.runtime.flush();
  const restored = await fixture(t, { dataDir: f.dataDir, state: f.state });
  restored.runtime.start();
  assert.equal((await restored.runtime.enqueueRequest(SESSION, 'uncertain-prompt-1', body)).state, 'uncertain');
  await restored.runtime.retryRequest(SESSION, 'uncertain-prompt-1', DIRECTORY, 1);
  await until(() => restored.runtime.sessionSnapshot(SESSION).items.length === 0);
  assert.equal(f.state.posts.length, 2);
  const done = await restored.runtime.enqueueRequest(SESSION, 'uncertain-prompt-1', body);
  assert.deepEqual(done.receipt, accepted.receipt);
  assert.equal(done.state, 'dispatched');
  await restored.runtime.retryRequest(SESSION, 'uncertain-prompt-1', DIRECTORY, 1);
  assert.equal(f.state.posts.length, 2);
});

test('partial context retry reuses every ID, skips observed admissions, and leaves unrelated queues dispatchable', async (t) => {
  for (const failAt of [0, 1, 2]) {
    const f = await fixture(t);
    const contexts = ['first', 'middle', 'last'].map((text) => ({ kind: 'synthetic', text }));
    const accepted = await f.runtime.enqueueRequest(SESSION, `context-retry-${failAt}`, input({ context: contexts }));
    f.state.fail = { match: (_pathname, body) => body.id === accepted.receipt.contextIds[failAt] };
    f.state.held = false;
    f.runtime.release(SESSION);
    await until(() => f.state.posts.length === failAt + 1 && f.runtime.sessionSnapshot(SESSION).sendingId === null);
    await f.runtime.enqueueRequest('ses_unrelated', `unrelated-${failAt}`, input());
    await until(() => f.runtime.sessionSnapshot('ses_unrelated').items.length === 0);
    assert.equal(f.runtime.sessionSnapshot(SESSION).items.length, 1);
    await f.runtime.retryRequest(SESSION, `context-retry-${failAt}`, DIRECTORY, 1);
    await until(() => f.runtime.sessionSnapshot(SESSION).items.length === 0);
    const own = f.state.posts.filter((entry) => entry.pathname.includes(SESSION));
    assert.deepEqual(own.map((entry) => entry.body.id), [...accepted.receipt.contextIds, accepted.receipt.messageId]);
    await f.runtime.flush();
    const stored = JSON.parse(await fs.promises.readFile(f.file, 'utf8'));
    assert.equal(stored.requests[`context-retry-${failAt}`].state, 'dispatched');
    assert.equal(JSON.stringify(stored.requests[`context-retry-${failAt}`]).includes('captured draft'), false);
  }
});

test('a repeated failed retry authorization never schedules another attempt or replaces stable IDs', async (t) => {
  const f = await fixture(t);
  const accepted = await f.runtime.enqueueRequest(SESSION, 'repeated-retry-1', input());
  f.state.fail = { before: true, match: (pathname) => pathname.endsWith('/prompt') };
  f.state.held = false;
  f.runtime.release(SESSION);
  await until(() => f.state.posts.length === 1 && f.runtime.sessionSnapshot(SESSION).sendingId === null);
  f.state.fail = { before: true, match: (pathname) => pathname.endsWith('/prompt') };
  await f.runtime.retryRequest(SESSION, 'repeated-retry-1', DIRECTORY, 1);
  await until(() => f.state.posts.length === 2 && f.runtime.sessionSnapshot(SESSION).sendingId === null);
  await f.runtime.retryRequest(SESSION, 'repeated-retry-1', DIRECTORY, 1);
  assert.equal(f.runtime.sessionSnapshot(SESSION).items[0].queueDelivery.state, 'uncertain');
  assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [accepted.receipt.messageId, accepted.receipt.messageId]);
  await assert.rejects(f.runtime.retryRequest(SESSION, 'repeated-retry-1', DIRECTORY, 3), { status: 409 });
});

test('receipt survives removal, clear, session deletion and restart without restoring full payloads', async (t) => {
  for (const operation of ['remove', 'clear', 'delete']) {
    const f = await fixture(t);
    const body = input({ attachments: [{ id: 'a', filename: 'private.txt', mimeType: 'text/plain', size: 7, source: 'local', dataUrl: 'data:text/plain,PRIVATE' }] });
    const accepted = await f.runtime.enqueueRequest(SESSION, `terminal-${operation}`, body);
    if (operation === 'remove') await f.runtime.remove(SESSION, accepted.receipt.itemId);
    if (operation === 'clear') await f.runtime.clear(SESSION);
    if (operation === 'delete') f.runtime.processPayload({ type: 'session.deleted', properties: { sessionID: SESSION } });
    const reply = await f.runtime.enqueueRequest(SESSION, `terminal-${operation}`, body);
    assert.deepEqual(reply.receipt, accepted.receipt);
    assert.equal(reply.state, operation === 'delete' ? 'deleted' : 'removed');
    assert.equal(JSON.stringify(JSON.parse(await fs.promises.readFile(f.file, 'utf8')).requests).includes('PRIVATE'), false);
    f.runtime.stop();
    const restored = await fixture(t, { dataDir: f.dataDir });
    assert.deepEqual((await restored.runtime.enqueueRequest(SESSION, `terminal-${operation}`, body)).receipt, accepted.receipt);
    assert.deepEqual(restored.runtime.snapshot().sessions, []);
  }
});

test('take lost response and restart replay full bytes until durable local-ownership acknowledgement', async (t) => {
  const f = await fixture(t);
  const a = await f.runtime.enqueueRequest(SESSION, 'take-source-1', input({ attachments: [{ id: 'a', filename: 'bytes.txt', mimeType: 'text/plain', size: 3, source: 'server', serverPath: '/owned/a', dataUrl: 'data:text/plain,abc' }] }));
  const b = await f.runtime.enqueue(SESSION, DIRECTORY, item({ text: 'leave queued' }));
  await f.runtime.flush();
  const body = { directory: DIRECTORY, itemIds: [a.receipt.itemId] };
  const taken = await f.runtime.takeRequest(SESSION, 'take-transfer-1', body);
  assert.equal(taken.items[0].attachments[0].dataUrl, 'data:text/plain,abc');
  assert.deepEqual(f.runtime.sessionSnapshot(SESSION).items.map((entry) => entry.id), [b.itemId]);
  f.runtime.stop();
  const restored = await fixture(t, { dataDir: f.dataDir });
  const repeated = await restored.runtime.takeRequest(SESSION, 'take-transfer-1', body);
  assert.deepEqual(repeated.receipt, taken.receipt);
  assert.deepEqual(repeated.items, taken.items);
  await assert.rejects(restored.runtime.takeRequest(SESSION, 'take-transfer-1', { ...body, itemIds: [b.itemId] }), { status: 409 });
  await assert.rejects(restored.runtime.acknowledgeTake('ses_other', 'take-transfer-1', DIRECTORY), { status: 409 });
  const ack = await restored.runtime.acknowledgeTake(SESSION, 'take-transfer-1', DIRECTORY);
  assert.equal(ack.state, 'acknowledged');
  assert.equal('items' in ack, false);
  assert.equal(JSON.stringify(JSON.parse(await fs.promises.readFile(f.file, 'utf8')).requests['take-transfer-1']).includes('data:text/plain,abc'), false);
  assert.deepEqual((await restored.runtime.takeRequest(SESSION, 'take-transfer-1', body)).receipt, taken.receipt);
  assert.equal((await restored.runtime.acknowledgeTake(SESSION, 'take-transfer-1', DIRECTORY)).state, 'acknowledged');
});

test('failed take checkpoint keeps a replayable receipt and never acknowledges a destructive handoff early', async (t) => {
  const f = await fixture(t);
  const queued = await f.runtime.enqueue(SESSION, DIRECTORY, item());
  await f.runtime.flush();
  const rename = fs.promises.rename.bind(fs.promises);
  let fail = true;
  const mock = vi.spyOn(fs.promises, 'rename').mockImplementation(async (...args) => {
    if (fail && args[1] === f.file) throw new Error('take checkpoint failed');
    return rename(...args);
  });
  t.onTestFinished(() => mock.mockRestore());
  const body = { directory: DIRECTORY, itemIds: [queued.itemId] };
  await assert.rejects(f.runtime.takeRequest(SESSION, 'faulted-take-1', body), /take checkpoint failed/);
  await assert.rejects(f.runtime.getRequest(SESSION, 'faulted-take-1', DIRECTORY), { status: 503 });
  fail = false;
  const recovered = await f.runtime.takeRequest(SESSION, 'faulted-take-1', body);
  assert.equal(recovered.items[0].id, queued.itemId);
  assert.deepEqual(f.runtime.sessionSnapshot(SESSION).items, []);
});

test('lost acknowledgement response and failed acknowledgement write preserve the local ownership decision', async (t) => {
  const f = await fixture(t);
  const queued = await f.runtime.enqueue(SESSION, DIRECTORY, item());
  const body = { directory: DIRECTORY, itemIds: [queued.itemId] };
  const transfer = await f.runtime.takeRequest(SESSION, 'ack-fault-transfer-1', body);
  const rename = fs.promises.rename.bind(fs.promises);
  let fail = true;
  const mock = vi.spyOn(fs.promises, 'rename').mockImplementation(async (...args) => {
    if (fail && args[1] === f.file) throw new Error('acknowledgement checkpoint failed');
    return rename(...args);
  });
  t.onTestFinished(() => mock.mockRestore());
  await assert.rejects(f.runtime.acknowledgeTake(SESSION, 'ack-fault-transfer-1', DIRECTORY), /acknowledgement checkpoint failed/);
  assert.equal(JSON.parse(await fs.promises.readFile(f.file, 'utf8')).requests['ack-fault-transfer-1'].state, 'transferring');
  fail = false;
  await f.runtime.acknowledgeTake(SESSION, 'ack-fault-transfer-1', DIRECTORY);
  f.runtime.stop();
  const restored = await fixture(t, { dataDir: f.dataDir });
  const reply = await restored.runtime.takeRequest(SESSION, 'ack-fault-transfer-1', body);
  assert.deepEqual(reply.receipt, transfer.receipt);
  assert.equal(reply.state, 'acknowledged');
  assert.equal('items' in reply, false);
});

test('corrupt JSON keeps opt-in receipt recovery closed across legacy writes and restart', async (t) => {
  const f = await fixture(t);
  await fs.promises.writeFile(f.file, '{ invalid JSON');
  f.runtime.stop();
  const restored = await fixture(t, { dataDir: f.dataDir });
  assert.equal((await fs.promises.readdir(f.dataDir)).some((name) => name.includes('.corrupt-')), true);
  await assert.rejects(restored.runtime.enqueueRequest(SESSION, 'unknown-old-key-1', input()), { code: 'queue_request_recovery_required' });
  await restored.runtime.enqueue(SESSION, DIRECTORY, item());
  await restored.runtime.flush();
  restored.runtime.stop();
  const next = await fixture(t, { dataDir: f.dataDir });
  await assert.rejects(next.runtime.getRequest(SESSION, 'unknown-old-key-1', DIRECTORY), { code: 'queue_request_recovery_required' });
});

test('invalid, oversized or conflicting receipts grant no empty authority and preserve retained bytes', async (t) => {
  const f = await fixture(t);
  await assert.rejects(f.runtime.enqueueRequest(SESSION, 'bad', input()), TypeError);
  await assert.rejects(f.runtime.enqueueRequest(SESSION, 'bad-owner-1', { ...input(), directory: ['not a string'] }), TypeError);
  await assert.rejects(f.runtime.enqueueRequest(SESSION, 'oversized-1', input({ text: 'x'.repeat(RECEIPT_BYTE_LIMIT) })), { status: 413 });
  await assert.rejects(f.runtime.enqueueRequest(SESSION, 'missing-bytes-1', input({ attachments: [{ id: 'a', filename: 'a.txt', mimeType: 'text/plain', source: 'server', serverPath: '/a' }] })), TypeError);
  const queued = await f.runtime.enqueue(SESSION, DIRECTORY, item({ attachments: [{ id: 'a', filename: 'large.txt', mimeType: 'text/plain', source: 'local', size: 70000, dataUrl: `data:text/plain,${'a'.repeat(70000)}` }] }));
  await assert.rejects(f.runtime.takeRequest(SESSION, 'oversized-take-1', { directory: DIRECTORY, itemIds: [queued.itemId] }), { status: 409 });
  assert.equal(f.runtime.sessionSnapshot(SESSION).items.length, 1);
  await f.runtime.enqueueRequest('ses_valid', 'valid-receipt-1', input());
  await f.runtime.flush();
  const bytes = JSON.parse(await fs.promises.readFile(f.file, 'utf8'));
  bytes.requests['valid-receipt-1'].receipt.contextIds = [bytes.requests['valid-receipt-1'].receipt.messageId];
  await fs.promises.writeFile(f.file, JSON.stringify(bytes));
  f.runtime.stop();
  const raw = await fs.promises.readFile(f.file, 'utf8');
  const broken = createMessageQueueRuntime({ dataDir: f.dataDir });
  await assert.rejects(broken.load(), /stored queue request receipt/);
  await assert.rejects(broken.enqueueRequest(SESSION, 'refuse-overwrite-1', input()), /stored queue request receipt/);
  assert.equal(await fs.promises.readFile(f.file, 'utf8'), raw);
  broken.stop();
});

test('malformed admission holds delivery and recognized commands never use an unstable command ID', async (t) => {
  const f = await fixture(t);
  const accepted = await f.runtime.enqueueRequest(SESSION, 'malformed-admission-1', input());
  f.state.fail = { before: true, match: () => true };
  f.state.held = false;
  f.runtime.release(SESSION);
  await until(() => f.state.posts.length === 1 && f.runtime.sessionSnapshot(SESSION).sendingId === null);
  f.state.badInbox = [{ id: accepted.receipt.messageId, type: 'user', sessionID: 'ses_foreign', delivery: 'queue', time: { created: 1 } }];
  await f.runtime.retryRequest(SESSION, 'malformed-admission-1', DIRECTORY, 1);
  await until(() => f.runtime.sessionSnapshot(SESSION).items[0].queueDelivery.state === 'uncertain' && f.runtime.sessionSnapshot(SESSION).sendingId === null);
  assert.equal(f.state.posts.length, 1);
  f.state.commands = [{ name: 'review' }];
  f.state.badInbox = null;
  await f.runtime.enqueueRequest('ses_command', 'command-limitation-1', input({ text: '/review code' }));
  await until(() => f.runtime.sessionSnapshot('ses_command').items[0]?.queueDelivery.blocked === 'command_identity_unsupported');
  assert.equal(f.state.posts.some((entry) => entry.pathname.endsWith('/command')), false);
  await assert.rejects(f.runtime.retryRequest('ses_command', 'command-limitation-1', DIRECTORY, 1), { status: 409 });
});

test('new routes validate owner, body and query without changing the legacy route contract', async (t) => {
  const f = await fixture(t);
  const routes = new Map();
  const app = Object.fromEntries(['get', 'post', 'put', 'delete'].map((method) => [method, (route, handler) => routes.set(`${method} ${route}`, handler)]));
  registerMessageQueueRoutes(app, f.runtime);
  const base = '/api/message-queue/sessions/:sessionId/requests/:requestId';
  const invoke = async (route, body, query = {}) => {
    const response = { statusCode: 200, setHeader() {}, status(code) { this.statusCode = code; return this; }, json(data) { this.data = data; } };
    await routes.get(route)({ params: { sessionId: SESSION, requestId: 'route-request-1' }, body, query }, response);
    return response;
  };
  assert.equal((await invoke(`post ${base}/enqueue`, input(), { directory: '/wrong' })).statusCode, 400);
  assert.equal((await invoke(`post ${base}/enqueue`, { ...input(), requestId: 'different' })).statusCode, 400);
  const reply = await invoke(`post ${base}/enqueue`, input());
  assert.equal(reply.statusCode, 200);
  assert.equal((await invoke(`get ${base}`, undefined, { directory: DIRECTORY, unexpected: 'query' })).statusCode, 400);
  assert.equal((await invoke(`get ${base}`, undefined, { directory: '/wrong' })).statusCode, 409);
  assert.deepEqual((await invoke(`get ${base}`, undefined, { directory: DIRECTORY })).data.receipt, reply.data.receipt);
  const legacy = await invoke('post /api/message-queue/sessions/:sessionId/items', input());
  assert.match(legacy.data.itemId, /^queued-\d+-[a-z0-9]*$/);
  assert.equal('receipt' in legacy.data, false);
});

test('observing an admitted retry resumes its queued successor without another status event', async (t) => {
  const f = await fixture(t);
  const first = await f.runtime.enqueueRequest(SESSION, 'observed-head-1', input({ text: 'first' }));
  const second = await f.runtime.enqueueRequest(SESSION, 'observed-next-1', input({ text: 'second' }));
  f.state.fail = { match: (_pathname, body) => body.id === first.receipt.messageId };
  f.state.held = false;
  f.runtime.release(SESSION);
  await until(() => f.state.posts.length === 1 && f.runtime.sessionSnapshot(SESSION).sendingId === null);
  await f.runtime.retryRequest(SESSION, 'observed-head-1', DIRECTORY, 1);
  await until(() => f.runtime.sessionSnapshot(SESSION).items.length === 0);
  await f.runtime.flush();
  assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [first.receipt.messageId, second.receipt.messageId]);
});

test('catch checkpoint recovery resumes an observed head successor only after durable completion and live idleness', async (t) => {
  for (const busy of [false, true]) {
    const f = await fixture(t);
    const requestId = 'catch-terminal-head-1';
    const first = await f.runtime.enqueueRequest(SESSION, requestId, input({ text: 'first' }));
    const second = await f.runtime.enqueueRequest(SESSION, 'catch-terminal-next-1', input({ text: 'second' }));
    f.state.fail = { match: (_pathname, body) => body.id === first.receipt.messageId };
    f.state.held = false;
    f.runtime.release(SESSION);
    await until(() => f.state.posts.length === 1 && f.runtime.sessionSnapshot(SESSION).sendingId === null);
    const entered = deferred();
    const release = deferred();
    const rename = fs.promises.rename.bind(fs.promises);
    let terminalWrites = 0;
    let readsBeforeRecovery;
    const mock = vi.spyOn(fs.promises, 'rename').mockImplementation(async (...args) => {
      if (args[1] === f.file) {
        const stored = JSON.parse(await fs.promises.readFile(args[0], 'utf8'));
        if (stored.requests[requestId]?.state === 'dispatched') {
          terminalWrites += 1;
          if (terminalWrites === 1) throw new Error('completion checkpoint failed');
          if (terminalWrites === 2) {
            if (busy) f.state.active = { [SESSION]: { type: 'running' } };
            readsBeforeRecovery = f.state.activeReads;
            entered.resolve();
            await release.promise;
          }
        }
      }
      return rename(...args);
    });
    t.onTestFinished(() => mock.mockRestore());
    try {
      await f.runtime.retryRequest(SESSION, requestId, DIRECTORY, 1);
      try {
        await entered.promise;
        assert.equal(JSON.parse(await fs.promises.readFile(f.file, 'utf8')).requests[requestId].state, 'uncertain');
        assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [first.receipt.messageId]);
        assert.deepEqual(f.runtime.sessionSnapshot(SESSION).items.map((entry) => entry.id), [second.receipt.itemId]);
      } finally {
        release.resolve();
      }
      if (busy) {
        await until(() => f.state.activeReads > readsBeforeRecovery && f.runtime.sessionSnapshot(SESSION).sendingId === null);
        assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [first.receipt.messageId]);
        assert.equal(JSON.parse(await fs.promises.readFile(f.file, 'utf8')).requests[requestId].state, 'dispatched');
        f.state.active = {};
        f.runtime.release(SESSION);
      }
      await until(() => f.runtime.sessionSnapshot(SESSION).items.length === 0);
      await f.runtime.flush();
      assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [first.receipt.messageId, second.receipt.messageId]);
    } finally {
      release.resolve();
      mock.mockRestore();
    }
  }
});

test('GET recovery of two failed completion checkpoints resumes the observed head successor with a live busy guard', async (t) => {
  for (const busy of [false, true]) {
    const f = await fixture(t);
    const requestId = 'lookup-terminal-head-1';
    const first = await f.runtime.enqueueRequest(SESSION, requestId, input({ text: 'first' }));
    const second = await f.runtime.enqueueRequest(SESSION, 'lookup-terminal-next-1', input({ text: 'second' }));
    f.state.fail = { match: (_pathname, body) => body.id === first.receipt.messageId };
    f.state.held = false;
    f.runtime.release(SESSION);
    await until(() => f.state.posts.length === 1 && f.runtime.sessionSnapshot(SESSION).sendingId === null);
    const rename = fs.promises.rename.bind(fs.promises);
    let failedWrites = 0;
    const mock = vi.spyOn(fs.promises, 'rename').mockImplementation(async (...args) => {
      if (args[1] === f.file) {
        const stored = JSON.parse(await fs.promises.readFile(args[0], 'utf8'));
        if (stored.requests[requestId]?.state === 'dispatched' && failedWrites < 2) {
          failedWrites += 1;
          throw new Error('completion checkpoint failed');
        }
      }
      return rename(...args);
    });
    t.onTestFinished(() => mock.mockRestore());
    try {
      await f.runtime.retryRequest(SESSION, requestId, DIRECTORY, 1);
      await until(() => failedWrites === 2 && f.runtime.sessionSnapshot(SESSION).sendingId === null);
      assert.equal(JSON.parse(await fs.promises.readFile(f.file, 'utf8')).requests[requestId].state, 'uncertain');
      assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [first.receipt.messageId]);
      if (busy) f.state.active = { [SESSION]: { type: 'running' } };
      const readsBeforeRecovery = f.state.activeReads;
      const recovered = await f.runtime.getRequest(SESSION, requestId, DIRECTORY);
      assert.equal(recovered.state, 'dispatched');
      assert.deepEqual(recovered.receipt, first.receipt);
      assert.equal(JSON.parse(await fs.promises.readFile(f.file, 'utf8')).requests[requestId].state, 'dispatched');
      if (busy) {
        await until(() => f.state.activeReads > readsBeforeRecovery && f.runtime.sessionSnapshot(SESSION).sendingId === null);
        assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [first.receipt.messageId]);
        f.state.active = {};
        f.runtime.release(SESSION);
      }
      await until(() => f.runtime.sessionSnapshot(SESSION).items.length === 0);
      await f.runtime.flush();
      assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [first.receipt.messageId, second.receipt.messageId]);
    } finally {
      mock.mockRestore();
    }
  }
});

test('removing an uncertain head resumes its queued successor without another status event', async (t) => {
  const f = await fixture(t);
  const first = await f.runtime.enqueueRequest(SESSION, 'removed-head-1', input({ text: 'first' }));
  const second = await f.runtime.enqueueRequest(SESSION, 'removed-next-1', input({ text: 'second' }));
  f.state.fail = { before: true, match: (_pathname, body) => body.id === first.receipt.messageId };
  f.state.held = false;
  f.runtime.release(SESSION);
  await until(() => f.state.posts.length === 1 && f.runtime.sessionSnapshot(SESSION).sendingId === null);
  await f.runtime.remove(SESSION, first.receipt.itemId);
  await until(() => f.runtime.sessionSnapshot(SESSION).items.length === 0);
  assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [first.receipt.messageId, second.receipt.messageId]);
});

test('lookup of an accepted receipt is read-only even when writes fail', async (t) => {
  const f = await fixture(t);
  const accepted = await f.runtime.enqueueRequest(SESSION, 'read-only-lookup-1', input());
  const snapshot = await fs.promises.readFile(f.file, 'utf8');
  let writes = 0;
  const mock = vi.spyOn(fs.promises, 'rename').mockImplementation(async () => { writes += 1; throw new Error('writes unavailable'); });
  t.onTestFinished(() => mock.mockRestore());
  for (let index = 0; index < 3; index += 1) {
    assert.deepEqual((await f.runtime.getRequest(SESSION, 'read-only-lookup-1', DIRECTORY)).receipt, accepted.receipt);
  }
  assert.equal(writes, 0);
  assert.equal(await fs.promises.readFile(f.file, 'utf8'), snapshot);
});

test('lookup alone recovers a failed delivery checkpoint without the released enqueue body', async (t) => {
  const f = await fixture(t);
  const accepted = await f.runtime.enqueueRequest(SESSION, 'lookup-checkpoint-1', input());
  const rename = fs.promises.rename.bind(fs.promises);
  let fail = true;
  const mock = vi.spyOn(fs.promises, 'rename').mockImplementation(async (...args) => {
    if (fail && args[1] === f.file) throw new Error('checkpoint unavailable');
    return rename(...args);
  });
  t.onTestFinished(() => mock.mockRestore());
  f.state.held = false;
  f.runtime.release(SESSION);
  await until(() => f.runtime.sessionSnapshot(SESSION).items[0].queueDelivery.state === 'uncertain'
    && f.runtime.sessionSnapshot(SESSION).sendingId === null);
  await assert.rejects(f.runtime.getRequest(SESSION, 'lookup-checkpoint-1', DIRECTORY), { code: 'queue_request_pending', status: 503 });
  fail = false;
  const recovered = await f.runtime.getRequest(SESSION, 'lookup-checkpoint-1', DIRECTORY);
  assert.deepEqual(recovered.receipt, accepted.receipt);
  assert.equal(recovered.state, 'uncertain');
  assert.deepEqual(f.state.posts, []);
  assert.deepEqual((await f.runtime.getRequest(SESSION, 'lookup-checkpoint-1', DIRECTORY)).receipt, accepted.receipt);
});

test('opt-in command safety scopes the catalog to its captured directory and refuses malformed catalogs', async (t) => {
  for (const commands of [[{ name: 'project-only' }], null, {}, [{}]]) {
    const f = await fixture(t);
    f.state.commandDirectoryOnly = true;
    f.state.commands = commands;
    await f.runtime.enqueueRequest(SESSION, 'scoped-command-1', input({ text: '/project-only check' }));
    f.state.held = false;
    f.runtime.release(SESSION);
    await until(() => f.state.commandReads?.length === 1 && f.runtime.sessionSnapshot(SESSION).sendingId === null);
    assert.equal(f.state.commandReads[0].searchParams.get('location[directory]'), DIRECTORY);
    assert.equal(f.state.commandReads[0].searchParams.has('directory'), false);
    assert.deepEqual(f.state.posts, []);
    assert.equal(f.runtime.sessionSnapshot(SESSION).items[0].queueDelivery.state, 'uncertain');
    if (Array.isArray(commands) && commands[0]?.name) {
      assert.equal(f.runtime.sessionSnapshot(SESSION).items[0].queueDelivery.blocked, 'command_identity_unsupported');
    }
  }
});

test('durable take checks UTF-8 text and combined prompt entry limits before moving any items', async (t) => {
  const attachment = (index) => ({ id: `a-${index}`, filename: 'a.txt', mimeType: 'text/plain', size: 1, source: 'local', dataUrl: 'data:text/plain,a' });
  for (const items of [
    [item({ text: 'x'.repeat(1024 * 1024 + 1) })],
    [item({ text: 'é'.repeat(524289) })],
    [item({ text: 'a'.repeat(600000) }), item({ text: 'b'.repeat(600000) })],
    [item({ context: Array.from({ length: 4097 }, () => ({ kind: 'synthetic', text: 'context' })) })],
    [item({ attachments: Array.from({ length: 4097 }, (_, index) => attachment(index)) })],
    [item({ attachments: Array.from({ length: 2048 }, (_, index) => attachment(index)) }),
      item({ context: Array.from({ length: 2049 }, () => ({ kind: 'synthetic', text: 'context' })) })],
    [item({ sendConfig: { providerID: 'p'.repeat(4097), modelID: 'model' } })],
    [item({ attachments: [{ ...attachment(0), filename: 'invalid\nname' }] })],
  ]) {
    const f = await fixture(t);
    const ids = [];
    for (const value of items) ids.push((await f.runtime.enqueue(SESSION, DIRECTORY, value)).itemId);
    await f.runtime.flush();
    await assert.rejects(f.runtime.takeRequest(SESSION, 'bounded-native-take-1', { directory: DIRECTORY, itemIds: ids }), { status: 409 });
    assert.deepEqual(f.runtime.sessionSnapshot(SESSION).items.map((entry) => entry.id), ids);
    await assert.rejects(f.runtime.getRequest(SESSION, 'bounded-native-take-1', DIRECTORY), { code: 'queue_request_unknown' });
  }
});

test('receipt replay and lookup stay bounded independently of later queue growth', async (t) => {
  const f = await fixture(t);
  const body = input();
  const accepted = await f.runtime.enqueueRequest(SESSION, 'bounded-replay-enqueue-1', body);
  const takeBody = { directory: DIRECTORY, itemIds: [accepted.receipt.itemId] };
  const transfer = await f.runtime.takeRequest(SESSION, 'bounded-replay-take-1', takeBody);
  for (let index = 0; index < 3; index += 1) {
    await f.runtime.enqueue(SESSION, DIRECTORY, item({ text: 'large later queue'.repeat(100000) }));
  }
  await f.runtime.flush();
  assert.ok(Buffer.byteLength(JSON.stringify(f.runtime.sessionSnapshot(SESSION))) > RECEIPT_BYTE_LIMIT);
  for (const reply of [
    await f.runtime.enqueueRequest(SESSION, 'bounded-replay-enqueue-1', body),
    await f.runtime.getRequest(SESSION, 'bounded-replay-enqueue-1', DIRECTORY),
    await f.runtime.takeRequest(SESSION, 'bounded-replay-take-1', takeBody),
    await f.runtime.getRequest(SESSION, 'bounded-replay-take-1', DIRECTORY),
  ]) {
    assert.ok(Buffer.byteLength(JSON.stringify(reply)) <= RECEIPT_BYTE_LIMIT);
    assert.equal('session' in reply, false);
  }
  assert.deepEqual((await f.runtime.getRequest(SESSION, 'bounded-replay-take-1', DIRECTORY)).items, transfer.items);
});

test('parseable malformed queue roots never become empty enqueue authority or get overwritten', async (t) => {
  for (const root of [null, [], 'invalid', 42, {}, { version: 1, revision: 0, sessions: [] }]) {
    const f = await fixture(t);
    const raw = JSON.stringify(root);
    await fs.promises.writeFile(f.file, raw);
    f.runtime.stop();
    const broken = createMessageQueueRuntime({ dataDir: f.dataDir });
    await assert.rejects(broken.load(), /invalid stored queue root/);
    await assert.rejects(broken.enqueueRequest(SESSION, 'invalid-root-key-1', input()), /invalid stored queue root/);
    assert.equal(await fs.promises.readFile(f.file, 'utf8'), raw);
    broken.stop();
  }
});

const manifestInput = (attachmentManifest = [], promptParts = {}) => ({ directory: DIRECTORY, item: {
  format: 'manifest-v1', content: 'captured draft', text: 'captured draft', attachmentManifest,
  context: [], sendConfig: item().sendConfig, promptParts,
} });
const upload = async (runtime, requestId, attachmentId, bytes, extra = {}) => (await runtime.uploadAttachment(SESSION, requestId, attachmentId, {
  directory: DIRECTORY, filename: `${attachmentId}.png`, mimeType: 'image/png', size: bytes.length,
  sha256: createHash('sha256').update(bytes).digest('hex'), ...extra,
}, bytes)).attachment;

const uploadPayload = async (runtime, requestId, capture, chunkBytes = BLOB_BYTE_LIMIT) => {
  const bytes = Buffer.isBuffer(capture) ? capture : Buffer.from(JSON.stringify(capture));
  const chunks = [];
  for (let offset = 0; offset < bytes.length; offset += chunkBytes) {
    const ordinal = chunks.length;
    const part = bytes.subarray(offset, offset + chunkBytes);
    chunks.push({ ordinal, ...await upload(runtime, requestId, `payload-${ordinal}`, part, {
      filename: `payload-${ordinal}.json`, mimeType: 'application/json',
    }) });
  }
  return { format: 'payload-v1', payloadManifest: { version: 1, encoding: 'utf-8-json', totalBytes: bytes.length,
    sha256: createHash('sha256').update(bytes).digest('hex'), chunks } };
};
const downloadPayload = async (runtime, pointer) => {
  const chunks = [];
  for (const descriptor of pointer.payloadManifest.chunks) {
    const result = await runtime.getAttachment(SESSION, descriptor.requestId, descriptor.attachmentId, DIRECTORY);
    assert.deepEqual(result.attachment, Object.fromEntries(Object.entries(descriptor).filter(([key]) => key !== 'ordinal')));
    assert.equal(createHash('sha256').update(result.bytes).digest('hex'), descriptor.sha256);
    chunks.push(result.bytes);
  }
  const bytes = Buffer.concat(chunks);
  assert.equal(bytes.length, pointer.payloadManifest.totalBytes);
  assert.equal(createHash('sha256').update(bytes).digest('hex'), pointer.payloadManifest.sha256);
  return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes));
};

test('per-use manifest model paths survive enqueue, transfer restart, acknowledgement, requeue and dispatch with shared bytes', async (t) => {
  for (const format of ['manifest-v1', 'payload-v1']) {
    const f = await fixture(t);
    const requestId = `model-paths-${format}`;
    const bytes = Buffer.from('original prepared bytes');
    const uploaded = await upload(f.runtime, requestId, 'file', bytes, { filename: 'safe.txt', mimeType: 'text/plain' });
    const names = ['/never/read/original?#%2F.txt', 'C:\\original folder\\source.txt', 'relative/../literal\\name\t.txt'];
    const files = names.map((agentPath) => ({ ...uploaded, agentPath }));
    const capture = manifestInput(files);
    const body = format === 'manifest-v1' ? capture
      : { directory: DIRECTORY, item: await uploadPayload(f.runtime, requestId, capture) };
    const accepted = await f.runtime.enqueueRequest(SESSION, requestId, body);
    assert.equal(Object.hasOwn((await f.runtime.getAttachment(SESSION, requestId, 'file', DIRECTORY)).attachment, 'agentPath'), false);
    await assert.rejects(f.runtime.take(SESSION, accepted.receipt.itemId), { status: 409 });
    f.runtime.stop();
    const restored = await fixture(t, { dataDir: f.dataDir, state: f.state });
    assert.deepEqual((await restored.runtime.enqueueRequest(SESSION, requestId, body)).receipt, accepted.receipt);
    const takeId = `model-path-take-${format}`;
    const takeBody = { directory: DIRECTORY, itemIds: [accepted.receipt.itemId], format };
    const taken = await restored.runtime.takeRequest(SESSION, takeId, takeBody);
    const logical = format === 'manifest-v1' ? taken.items[0] : (await downloadPayload(restored.runtime, taken.items[0])).item;
    assert.deepEqual(logical.attachmentManifest, files);
    if (format === 'payload-v1') assert.deepEqual(taken.items[0].payloadIndex.attachmentBlobIds, [uploaded.blobId]);
    restored.runtime.stop();
    const afterTake = await fixture(t, { dataDir: f.dataDir, state: f.state });
    assert.deepEqual((await afterTake.runtime.getRequest(SESSION, takeId, DIRECTORY)).items, taken.items);
    assert.deepEqual((await afterTake.runtime.takeRequest(SESSION, takeId, takeBody)).items, taken.items);
    await assert.rejects(afterTake.runtime.releaseAttachment(SESSION, requestId, 'file', DIRECTORY), { status: 409 });
    await afterTake.runtime.acknowledgeTake(SESSION, takeId, DIRECTORY);
    await afterTake.runtime.enqueueRequest(SESSION, `model-path-requeue-${format}`, body);
    f.state.held = false;
    afterTake.runtime.release(SESSION);
    await until(() => afterTake.runtime.sessionSnapshot(SESSION).items.length === 0);
    await afterTake.runtime.flush();
    assert.deepEqual(f.state.posts.at(-1).body.files, names.map((name) => ({ uri: `data:text/plain;base64,${bytes.toString('base64')}`, name })));
    assert.deepEqual((await afterTake.runtime.getAttachment(SESSION, requestId, 'file', DIRECTORY)).bytes, bytes);
  }
});

test('legacy manifest conversion keeps exact model identity with safe basename or internal upload labels', async (t) => {
  const f = await fixture(t);
  const originals = [
    { filename: 'notes.txt', agentPath: '/original/source/notes.txt', expected: 'notes.txt' },
    { filename: '/original/source/path-name.txt', expected: 'path-name.txt' },
    { filename: 'C:\\original folder\\windows.txt', expected: 'windows.txt' },
    { filename: '\\\\server\\share\\unc.txt', expected: 'unc.txt' },
    { filename: '/posix/literal\\name.txt', expected: 'item-0-file-4.bin' },
    { filename: 'literal\tname.txt', expected: 'item-0-file-5.bin' },
    { filename: `${'x'.repeat(5000)}.txt`, expected: 'item-0-file-6.bin' },
    { filename: 'unchanged.txt', expected: 'unchanged.txt' },
  ];
  const queued = await f.runtime.enqueue(SESSION, DIRECTORY, item({ attachments: originals.map(({ expected: _expected, ...file }, index) => ({
    ...file, id: `legacy-${index}`, mimeType: 'text/plain', source: 'local', size: 3, dataUrl: 'data:text/plain;base64,YWJj',
  })) }));
  const request = { id: 'msg_model_path_conversion', itemIds: [queued.itemId], composer: '',
    sendConfig: item().sendConfig, format: 'manifest-v1', directory: DIRECTORY };
  const preview = await f.runtime.previewPrefix(SESSION, request);
  const reserved = await f.runtime.reservePrefix(SESSION, request);
  assert.deepEqual(reserved.items, preview.items);
  for (const [index, original] of originals.entries()) {
    const file = reserved.items[0].attachmentManifest[index];
    assert.equal(file.filename, original.expected);
    assert.equal(file.agentPath ?? file.filename, original.agentPath ?? original.filename);
    if (index === originals.length - 1) assert.equal(Object.hasOwn(file, 'agentPath'), false);
    const retained = await f.runtime.getAttachment(SESSION, file.requestId, file.attachmentId, DIRECTORY);
    assert.equal(Object.hasOwn(retained.attachment, 'agentPath'), false);
    assert.deepEqual(retained.bytes, Buffer.from('abc'));
  }
  f.runtime.stop();
  const restored = await fixture(t, { dataDir: f.dataDir });
  assert.deepEqual((await restored.runtime.reservePrefix(SESSION, request)).items, reserved.items);
});

test('invalid manifest model identities refuse independently and malformed durable identities never disappear on load', async (t) => {
  const f = await fixture(t);
  const uploaded = await upload(f.runtime, 'model-path-validation', 'file', Buffer.from('abc'));
  const before = await f.runtime.enqueueRequest('ses_valid_before', 'model-path-before', manifestInput());
  for (const agentPath of [null, '', false, 42, [], {}]) {
    await assert.rejects(f.runtime.enqueueRequest(SESSION, 'invalid-model-path', manifestInput([{ ...uploaded, agentPath }])), TypeError);
  }
  const after = await f.runtime.enqueueRequest('ses_valid_after', 'model-path-after', manifestInput());
  assert.equal(f.runtime.sessionSnapshot('ses_valid_before').items[0].id, before.receipt.itemId);
  assert.equal(f.runtime.sessionSnapshot('ses_valid_after').items[0].id, after.receipt.itemId);
  const valid = await f.runtime.enqueueRequest(SESSION, 'invalid-model-path', manifestInput([{ ...uploaded, agentPath: '/original/source.txt' }]));
  await assert.rejects(f.runtime.enqueueRequest(SESSION, 'invalid-model-path', manifestInput([{ ...uploaded, agentPath: '/changed/source.txt' }])), { status: 409 });
  assert.equal(f.runtime.sessionSnapshot(SESSION).items[0].id, valid.receipt.itemId);
  f.runtime.stop();
  await f.runtime.flush();
  const stored = JSON.parse(await fs.promises.readFile(f.file, 'utf8'));
  stored.sessions[SESSION].items[0].attachmentManifest[0].agentPath = null;
  const corrupted = JSON.stringify(stored);
  await fs.promises.writeFile(f.file, corrupted);
  const broken = createMessageQueueRuntime({ dataDir: f.dataDir });
  await assert.rejects(broken.load(), /invalid stored durable queue item/);
  await assert.rejects(broken.enqueueRequest(SESSION, 'must-not-overwrite', manifestInput()));
  assert.equal(await fs.promises.readFile(f.file, 'utf8'), corrupted);
  broken.stop();
});

test('durable 2 and 12 MiB uploads survive lost replies, enqueue restart, manifest take and journal acknowledgement', async (t) => {
  for (const size of [2 * 1024 * 1024, 12 * 1024 * 1024]) {
    const f = await fixture(t);
    const bytes = Buffer.alloc(size, 0x61);
    const file = await upload(f.runtime, 'large-upload-1', 'prepared', bytes);
    assert.deepEqual(await upload(f.runtime, 'large-upload-1', 'prepared', bytes), file);
    const body = manifestInput([file]);
    const accepted = await f.runtime.enqueueRequest(SESSION, 'large-upload-1', body);
    const raw = await fs.promises.readFile(f.file, 'utf8');
    assert.ok(raw.length < 16384);
    assert.equal(raw.includes('data:image/png;base64,'), false);
    await assert.rejects(f.runtime.releaseAttachment(SESSION, 'large-upload-1', 'prepared', DIRECTORY), { status: 409 });
    f.runtime.stop();
    const restored = await fixture(t, { dataDir: f.dataDir });
    assert.deepEqual((await restored.runtime.enqueueRequest(SESSION, 'large-upload-1', body)).receipt, accepted.receipt);
    assert.deepEqual((await restored.runtime.getAttachment(SESSION, 'large-upload-1', 'prepared', DIRECTORY)).bytes, bytes);
    const takeBody = { directory: DIRECTORY, itemIds: [accepted.receipt.itemId], format: 'manifest-v1' };
    const transfer = await restored.runtime.takeRequest(SESSION, 'large-transfer-1', takeBody);
    assert.deepEqual(transfer.items[0].attachmentManifest, [file]);
    assert.deepEqual(transfer.items[0].attachments, []);
    assert.ok(Buffer.byteLength(JSON.stringify(transfer)) < RECEIPT_BYTE_LIMIT);
    assert.deepEqual((await restored.runtime.takeRequest(SESSION, 'large-transfer-1', takeBody)).items, transfer.items);
    await assert.rejects(restored.runtime.releaseAttachment(SESSION, 'large-upload-1', 'prepared', DIRECTORY), { status: 409 });
    restored.runtime.stop();
    const afterTake = await fixture(t, { dataDir: f.dataDir });
    assert.deepEqual((await afterTake.runtime.takeRequest(SESSION, 'large-transfer-1', takeBody)).items, transfer.items);
    await afterTake.runtime.acknowledgeTake(SESSION, 'large-transfer-1', DIRECTORY);
    await afterTake.runtime.acknowledgeTake(SESSION, 'large-transfer-1', DIRECTORY);
    await afterTake.runtime.releaseAttachment(SESSION, 'large-upload-1', 'prepared', DIRECTORY);
    await afterTake.runtime.releaseAttachment(SESSION, 'large-upload-1', 'prepared', DIRECTORY);
    await assert.rejects(afterTake.runtime.getAttachment(SESSION, 'large-upload-1', 'prepared', DIRECTORY), { status: 410 });
    await assert.rejects(fs.promises.stat(path.join(f.dataDir, 'message-queue-blobs', `${file.blobId}.blob`)), { code: 'ENOENT' });
  }
});

test('two 20 MiB image manifests retain 40 MiB of actual bytes behind a small immutable prefix receipt', async (t) => {
  const f = await fixture(t);
  const bytes = Buffer.alloc(BLOB_BYTE_LIMIT, 0x42);
  const first = await upload(f.runtime, 'document-images-1', 'image-1', bytes);
  const second = await upload(f.runtime, 'document-images-1', 'image-2', bytes);
  const body = manifestInput([first, second], { agents: [{ name: 'reviewer' }], metadata: { document: 'prepared' } });
  const accepted = await f.runtime.enqueueRequest(SESSION, 'document-images-1', body);
  const request = { id: 'msg_document_prefix', itemIds: [accepted.receipt.itemId], composer: 'follow up',
    sendConfig: item().sendConfig, format: 'manifest-v1', directory: DIRECTORY, delivery: 'steer',
    promptParts: { agents: [{ name: 'reviewer' }], skills: [{ id: 'skill_reference' }], metadata: { captured: true } },
  };
  const preview = await f.runtime.previewPrefix(SESSION, request);
  const reserved = await f.runtime.reservePrefix(SESSION, request);
  assert.deepEqual(reserved.items, preview.items);
  assert.deepEqual(reserved.contextIds, preview.contextIds);
  assert.deepEqual(reserved.items[0].attachmentManifest, [first, second]);
  assert.deepEqual(reserved.items[0].promptParts.metadata, { document: 'prepared' });
  assert.ok(Buffer.byteLength(JSON.stringify(reserved)) < RECEIPT_BYTE_LIMIT);
  await assert.rejects(f.runtime.takeRequest(SESSION, 'blocked-transfer-1', { directory: DIRECTORY, itemIds: request.itemIds, format: 'manifest-v1' }), { status: 409 });
  await assert.rejects(f.runtime.remove(SESSION, accepted.receipt.itemId), { status: 409 });
  await assert.rejects(f.runtime.releaseAttachment(SESSION, 'document-images-1', 'image-1', DIRECTORY), { status: 409 });
  f.runtime.stop();
  const restored = await fixture(t, { dataDir: f.dataDir, state: f.state });
  assert.deepEqual((await restored.runtime.reservePrefix(SESSION, request)).items, reserved.items);
  assert.deepEqual((await restored.runtime.getAttachment(SESSION, 'document-images-1', 'image-2', DIRECTORY)).bytes, bytes);
  f.state.inbox.set(request.id, { id: request.id, type: 'user', sessionID: SESSION, delivery: 'steer', time: { created: 1 } });
  f.state.held = false;
  restored.runtime.release(SESSION);
  await until(() => restored.runtime.sessionSnapshot(SESSION).items.length === 0);
  await restored.runtime.flush();
  assert.deepEqual(f.state.posts, []);
  await restored.runtime.releaseAttachment(SESSION, 'document-images-1', 'image-1', DIRECTORY);
});

test('manifest dispatch preserves bytes, frozen configuration, multiple agents, skills, metadata and exact context IDs', async (t) => {
  const f = await fixture(t);
  const bytes = Buffer.alloc(2 * 1024 * 1024, 0x43);
  const file = await upload(f.runtime, 'full-payload-1', 'image', bytes);
  const mention = { start: 0, end: 4, text: 'test' };
  const body = manifestInput([{ ...file, description: 'prepared image', mention }], {
    agents: [{ name: 'first', mention }, { name: 'second' }],
    skills: [{ id: 'skill_one', mention }, { id: 'skill_two' }],
    metadata: { nested: { valid: true }, array: [1, null, 'value'] },
    context: [{ id: 'msg_captured_official_context', text: 'official context', description: 'captured description', metadata: { preserved: true } }],
  });
  body.item.context = [{ kind: 'context', text: 'diff', instructions: 'read it first', metadata: { legacyContext: true } }];
  const frozen = structuredClone(body);
  const accepted = await f.runtime.enqueueRequest(SESSION, 'full-payload-1', body);
  body.item.sendConfig.modelID = 'changed';
  body.item.promptParts.agents[0].name = 'changed';
  f.state.held = false;
  f.runtime.release(SESSION);
  await until(() => f.runtime.sessionSnapshot(SESSION).items.length === 0);
  await f.runtime.flush();
  assert.deepEqual(f.state.posts.slice(0, 3).map((entry) => entry.body.id), accepted.receipt.contextIds);
  assert.equal(accepted.receipt.contextIds[2], 'msg_captured_official_context');
  assert.deepEqual(f.state.posts[2].body, { id: 'msg_captured_official_context', text: 'official context', description: 'captured description', metadata: { preserved: true }, resume: false, delivery: 'queue' });
  const sent = f.state.posts.at(-1).body;
  assert.deepEqual(sent.agents, frozen.item.promptParts.agents);
  assert.deepEqual(sent.skills, frozen.item.promptParts.skills);
  assert.deepEqual(sent.metadata, frozen.item.promptParts.metadata);
  assert.deepEqual(sent.files[0].mention, mention);
  assert.equal(sent.files[0].description, 'prepared image');
  assert.deepEqual(Buffer.from(sent.files[0].uri.split(',')[1], 'base64'), bytes);
  assert.deepEqual((await f.runtime.enqueueRequest(SESSION, 'full-payload-1', frozen)).receipt, accepted.receipt);
});

test('strict manifest promptParts reject malformed or unknown fields without affecting valid siblings', async (t) => {
  const f = await fixture(t);
  const valid = await f.runtime.enqueueRequest('ses_sibling', 'valid-parts-1', manifestInput());
  for (const promptParts of [
    { agents: 'bad' }, { agents: [{ name: 'x', source: {} }] }, { skills: [{ name: 'missing-id' }] },
    { metadata: [] }, { metadata: null }, { context: [{ text: 'x', metadata: [] }] }, { unknown: true },
  ]) {
    await assert.rejects(f.runtime.enqueueRequest(SESSION, 'invalid-parts-1', manifestInput([], promptParts)), TypeError);
  }
  const unsupported = manifestInput();
  unsupported.item.format = 'manifest-v2';
  await assert.rejects(f.runtime.enqueueRequest(SESSION, 'invalid-format-1', unsupported), TypeError);
  const missingFormat = manifestInput();
  delete missingFormat.item.format;
  await assert.rejects(f.runtime.enqueueRequest(SESSION, 'missing-format-1', missingFormat), TypeError);
  assert.deepEqual(f.runtime.sessionSnapshot('ses_sibling').items.map((entry) => entry.id), [valid.receipt.itemId]);
  assert.deepEqual(f.runtime.sessionSnapshot(SESSION).items, []);
});

test('binary upload rejects traversal, invalid metadata, mismatched digests and streamed overruns while siblings remain usable', async (t) => {
  const f = await fixture(t);
  const bytes = Buffer.from('real bytes');
  for (const [attachmentId, extra] of [
    ['../escape', {}], ['file', { filename: '/etc/passwd' }], ['file', { filename: '..' }],
    ['file', { filename: 'bad\nname' }], ['file', { mimeType: 'text/plain\r\nInjected: yes' }], ['file', { sha256: 'BAD' }],
  ]) await assert.rejects(upload(f.runtime, 'invalid-upload-1', attachmentId, bytes, extra), TypeError);
  await assert.rejects(upload(f.runtime, 'wrong-digest-1', 'file', bytes, { sha256: '0'.repeat(64) }), TypeError);
  const maximum = Buffer.alloc(BLOB_BYTE_LIMIT, 0x44);
  async function* overrun() { yield maximum; yield Buffer.from([1]); }
  await assert.rejects(f.runtime.uploadAttachment(SESSION, 'overrun-upload-1', 'file', {
    directory: DIRECTORY, filename: 'file.bin', mimeType: 'application/octet-stream', size: maximum.length,
    sha256: createHash('sha256').update(maximum).digest('hex'),
  }, overrun()), { status: 413 });
  const good = await upload(f.runtime, 'good-sibling-upload-1', 'file', bytes);
  await assert.rejects(upload(f.runtime, 'good-sibling-upload-1', 'file', bytes, { filename: 'different.png' }), { status: 409 });
  await assert.rejects(f.runtime.getAttachment('ses_foreign', 'good-sibling-upload-1', 'file', DIRECTORY), { status: 404 });
  await assert.rejects(f.runtime.getAttachment(SESSION, 'good-sibling-upload-1', 'file', '/different'), { status: 404 });
  await f.runtime.enqueueRequest(SESSION, 'good-sibling-upload-1', manifestInput([good]));
});

test('upload root checkpoint failure retains immutable bytes for exact repeat after restart', async (t) => {
  const f = await fixture(t);
  const bytes = Buffer.alloc(2 * 1024 * 1024, 0x45);
  const rename = fs.promises.rename.bind(fs.promises);
  let fail = true;
  const mock = vi.spyOn(fs.promises, 'rename').mockImplementation(async (...args) => {
    if (fail && args[1] === f.file) throw new Error('root receipt checkpoint failed');
    return rename(...args);
  });
  t.onTestFinished(() => mock.mockRestore());
  await assert.rejects(upload(f.runtime, 'faulted-upload-1', 'image', bytes), /root receipt checkpoint failed/);
  fail = false;
  f.runtime.stop();
  const restored = await fixture(t, { dataDir: f.dataDir });
  await assert.rejects(upload(restored.runtime, 'faulted-upload-1', 'image', Buffer.alloc(bytes.length, 0x46)), { code: 'queue_request_conflict' });
  const recovered = await upload(restored.runtime, 'faulted-upload-1', 'image', bytes);
  assert.deepEqual((await restored.runtime.getAttachment(SESSION, 'faulted-upload-1', 'image', DIRECTORY)).bytes, bytes);
  assert.equal(recovered.sha256, createHash('sha256').update(bytes).digest('hex'));
});

test('owned blob guards reject symlink roots, hard links, missing bytes and digest corruption', async (t) => {
  const linked = await fixture(t);
  const target = path.join(linked.dataDir, 'outside');
  await fs.promises.mkdir(target, { mode: 0o700 });
  await fs.promises.symlink(target, path.join(linked.dataDir, 'message-queue-blobs'));
  await assert.rejects(upload(linked.runtime, 'symlink-root-1', 'file', Buffer.from('a')), /unsafe queue blob storage/);
  for (const damage of ['hard-link', 'missing', 'corrupt']) {
    const f = await fixture(t);
    const file = await upload(f.runtime, 'guarded-upload-1', 'file', Buffer.from('actual bytes'));
    const filename = path.join(f.dataDir, 'message-queue-blobs', `${file.blobId}.blob`);
    if (damage === 'hard-link') await fs.promises.link(filename, path.join(f.dataDir, 'linked-copy'));
    if (damage === 'missing') await fs.promises.unlink(filename);
    if (damage === 'corrupt') {
      const handle = await fs.promises.open(filename, 'r+');
      const stat = await handle.stat();
      await handle.write(Buffer.from([0]), 0, 1, stat.size - 1);
      await handle.close();
    }
    await assert.rejects(f.runtime.getAttachment(SESSION, 'guarded-upload-1', 'file', DIRECTORY));
    await assert.rejects(f.runtime.enqueueRequest(SESSION, 'guarded-upload-1', manifestInput([file])));
    await f.runtime.enqueueRequest('ses_independent', 'independent-file-1', manifestInput());
  }
});

test('shared blob ownership survives removal of one item and cannot be released while another item owns it', async (t) => {
  const f = await fixture(t);
  const file = await upload(f.runtime, 'shared-source-1', 'file', Buffer.from('shared bytes'));
  const a = await f.runtime.enqueueRequest(SESSION, 'shared-source-1', manifestInput([file]));
  const b = await f.runtime.enqueueRequest(SESSION, 'shared-second-1', manifestInput([file]));
  await f.runtime.remove(SESSION, a.receipt.itemId);
  await assert.rejects(f.runtime.releaseAttachment(SESSION, 'shared-source-1', 'file', DIRECTORY), { status: 409 });
  await f.runtime.remove(SESSION, b.receipt.itemId);
  await f.runtime.releaseAttachment(SESSION, 'shared-source-1', 'file', DIRECTORY);
});

test('manifest prefix conversion retains legacy inline bytes and captured fields until exact admission', async (t) => {
  const f = await fixture(t);
  const bytes = Buffer.alloc(2 * 1024 * 1024, 0x46);
  const queued = await f.runtime.enqueue(SESSION, DIRECTORY, item({ agentMention: 'reviewer', attachments: [{
    id: 'old-inline', filename: 'old.png', mimeType: 'image/png', size: bytes.length, source: 'local', dataUrl: `data:image/png;base64,${bytes.toString('base64')}`,
  }], context: [{ kind: 'synthetic', text: 'captured context' }] }));
  const request = { id: 'msg_convert_prefix', itemIds: [queued.itemId], composer: 'typed', sendConfig: item().sendConfig, format: 'manifest-v1', directory: DIRECTORY };
  const preview = await f.runtime.previewPrefix(SESSION, request);
  const captured = await f.runtime.reservePrefix(SESSION, request);
  assert.deepEqual(captured.items, preview.items);
  assert.deepEqual(captured.contextIds, preview.contextIds);
  assert.deepEqual(captured.items[0].promptParts.agents, [{ name: 'reviewer' }]);
  assert.equal(f.runtime.sessionSnapshot(SESSION).items[0].id, queued.itemId);
  const file = captured.items[0].attachmentManifest[0];
  assert.deepEqual((await f.runtime.getAttachment(SESSION, file.requestId, file.attachmentId, DIRECTORY)).bytes, bytes);
  await assert.rejects(f.runtime.take(SESSION, queued.itemId), { status: 409 });
  await assert.rejects(f.runtime.reservePrefix(SESSION, { ...request, composer: 'changed' }), { status: 409 });
});

test('oversized manifest takes and prefixes reject before removing items or establishing a reservation', async (t) => {
  const f = await fixture(t);
  const queued = await f.runtime.enqueue(SESSION, DIRECTORY, item({ context: Array.from({ length: 5 }, () => ({ kind: 'synthetic', text: 'x'.repeat(900000) })) }));
  await f.runtime.flush();
  await assert.rejects(f.runtime.takeRequest(SESSION, 'too-large-manifest-1', { directory: DIRECTORY, itemIds: [queued.itemId], format: 'manifest-v1' }), { status: 413 });
  const request = { id: 'msg_oversized_manifest_prefix', itemIds: [queued.itemId], composer: 'typed', sendConfig: item().sendConfig, format: 'manifest-v1', directory: DIRECTORY };
  await assert.rejects(f.runtime.reservePrefix(SESSION, request), { status: 413 });
  assert.deepEqual(f.runtime.sessionSnapshot(SESSION).items.map((entry) => entry.id), [queued.itemId]);
  await f.runtime.remove(SESSION, queued.itemId);
  assert.deepEqual(f.runtime.sessionSnapshot(SESSION).items, []);
});

test('binary queue routes retain strict owner queries, encoded filename metadata and bounded octet-stream responses', async (t) => {
  const f = await fixture(t);
  const routes = new Map();
  const app = Object.fromEntries(['get', 'post', 'put', 'delete'].map((method) => [method, (route, handler) => routes.set(`${method} ${route}`, handler)]));
  registerMessageQueueRoutes(app, f.runtime);
  const route = '/api/message-queue/sessions/:sessionId/requests/:requestId/attachments/:attachmentId';
  const bytes = Buffer.from('owned attachment bytes');
  const sha256 = createHash('sha256').update(bytes).digest('hex');
  const headers = { 'content-type': 'application/octet-stream', 'content-length': String(bytes.length),
    'x-openchamber-size': String(bytes.length), 'x-openchamber-filename': encodeURIComponent('prepared image.png'),
    'x-openchamber-mime-type': 'image/png', 'x-openchamber-sha256': sha256,
  };
  const invoke = async (method, query, requestHeaders = method === 'put' ? headers : {}) => {
    const response = { statusCode: 200, headers: {}, setHeader(key, value) { this.headers[key] = value; },
      status(code) { this.statusCode = code; return this; }, json(value) { this.data = value; }, send(value) { this.bytes = value; },
    };
    const request = { params: { sessionId: SESSION, requestId: 'binary-route-upload-1', attachmentId: 'prepared' },
      query, headers: requestHeaders, async *[Symbol.asyncIterator]() { yield bytes; },
    };
    await routes.get(`${method} ${route}`)(request, response);
    return response;
  };
  assert.equal((await invoke('put', { directory: DIRECTORY }, { ...headers, 'content-type': 'application/json' })).statusCode, 415);
  assert.equal((await invoke('put', { directory: DIRECTORY, ignored: 'wrong' })).statusCode, 400);
  assert.equal((await invoke('put', { directory: DIRECTORY }, { ...headers, 'content-length': String(BLOB_BYTE_LIMIT + 1) })).statusCode, 413);
  const accepted = await invoke('put', { directory: DIRECTORY });
  assert.equal(accepted.statusCode, 200);
  assert.equal(accepted.data.attachment.filename, 'prepared image.png');
  const downloaded = await invoke('get', { directory: DIRECTORY });
  assert.deepEqual(downloaded.bytes, bytes);
  assert.equal(downloaded.headers['Content-Type'], 'application/octet-stream');
  assert.equal(downloaded.headers['X-Content-Type-Options'], 'nosniff');
  assert.equal(downloaded.headers['X-OpenChamber-SHA256'], sha256);
  assert.equal((await invoke('delete', { directory: DIRECTORY })).data.state, 'released');
});

test('a stalled upload excludes only its identity and never holds unrelated durable requests', async (t) => {
  const f = await fixture(t);
  const bytes = Buffer.from('streamed bytes');
  const entered = deferred();
  const release = deferred();
  async function* body() { entered.resolve(); await release.promise; yield bytes; }
  const metadata = { directory: DIRECTORY, filename: 'stream.bin', mimeType: 'application/octet-stream', size: bytes.length,
    sha256: createHash('sha256').update(bytes).digest('hex') };
  const pending = f.runtime.uploadAttachment(SESSION, 'stalled-upload-1', 'file', metadata, body());
  try {
    await entered.promise;
    let sibling;
    const siblingWrite = f.runtime.enqueueRequest('ses_unblocked', 'unblocked-request-1', manifestInput()).then((value) => { sibling = value; });
    await until(() => sibling !== undefined);
    await siblingWrite;
    await assert.rejects(f.runtime.enqueueRequest(SESSION, 'stalled-upload-1', manifestInput()), { code: 'queue_request_pending' });
    await assert.rejects(f.runtime.uploadAttachment('ses_foreign', 'stalled-upload-1', 'other', metadata, bytes), { code: 'queue_request_conflict' });
    await assert.rejects(f.runtime.uploadAttachment(SESSION, 'stalled-upload-1', 'file', metadata, bytes), { code: 'queue_request_pending' });
  } finally { release.resolve(); await pending; }
  const descriptor = (await pending).attachment;
  await f.runtime.enqueueRequest(SESSION, 'stalled-upload-1', manifestInput([descriptor]));
});

test('upload request owners remain exact before enqueue and through take without forbidding shared attachment labels', async (t) => {
  const f = await fixture(t);
  const bytes = Buffer.from('same label, independent upload');
  const first = await upload(f.runtime, 'owner-upload-first', 'file', bytes);
  const second = await upload(f.runtime, 'owner-upload-second', 'file', bytes);
  const metadata = { directory: '/other', filename: 'file.png', mimeType: 'image/png', size: bytes.length, sha256: first.sha256 };
  await assert.rejects(f.runtime.uploadAttachment(SESSION, 'owner-upload-first', 'new', metadata, bytes), { code: 'queue_request_conflict' });
  const accepted = await f.runtime.enqueueRequest(SESSION, 'owner-upload-target', manifestInput([first, second]));
  await assert.rejects(f.runtime.takeRequest('ses_foreign', 'owner-upload-first', { directory: DIRECTORY, itemIds: [accepted.receipt.itemId], format: 'manifest-v1' }), { code: 'queue_request_conflict' });
  assert.equal(f.runtime.sessionSnapshot(SESSION).items[0].attachments.length, 2);
});

test('text-only manifest reservations sync their ownership and refuse directory changes from legacy appends', async (t) => {
  const f = await fixture(t);
  const queued = await f.runtime.enqueue(SESSION, DIRECTORY, item());
  await f.runtime.flush();
  const open = fs.promises.open.bind(fs.promises);
  let synced = 0;
  const mock = vi.spyOn(fs.promises, 'open').mockImplementation(async (...args) => {
    const handle = await open(...args);
    if (String(args[0]).endsWith('.tmp')) {
      const sync = handle.sync.bind(handle);
      handle.sync = async () => { synced += 1; await sync(); };
    }
    return handle;
  });
  t.onTestFinished(() => mock.mockRestore());
  const request = { id: 'msg_x', itemIds: [queued.itemId], composer: 'follow up', sendConfig: item().sendConfig, format: 'manifest-v1', directory: DIRECTORY };
  const reserved = await f.runtime.reservePrefix(SESSION, request);
  assert.equal(synced, 1);
  await assert.rejects(f.runtime.enqueue(SESSION, '/another/repository', item()), { status: 409 });
  f.runtime.stop();
  const restored = await fixture(t, { dataDir: f.dataDir });
  assert.deepEqual((await restored.runtime.reservePrefix(SESSION, request)).items, reserved.items);
});

test('release checkpoints precede unlink and failed unlink can be retried after restart', async (t) => {
  const f = await fixture(t);
  const descriptor = await upload(f.runtime, 'release-fault-1', 'file', Buffer.from('retained bytes'));
  const filename = path.join(f.dataDir, 'message-queue-blobs', `${descriptor.blobId}.blob`);
  const rename = fs.promises.rename.bind(fs.promises);
  const renameMock = vi.spyOn(fs.promises, 'rename').mockImplementation(async (...args) => {
    if (args[1] === f.file) throw new Error('release checkpoint failed');
    return rename(...args);
  });
  try {
    await assert.rejects(f.runtime.releaseAttachment(SESSION, 'release-fault-1', 'file', DIRECTORY), /release checkpoint failed/);
    assert.equal(JSON.parse(await fs.promises.readFile(f.file, 'utf8')).blobs[descriptor.blobId].state, 'available');
    assert.ok((await fs.promises.stat(filename)).isFile());
  } finally { renameMock.mockRestore(); }
  const unlink = fs.promises.unlink.bind(fs.promises);
  const unlinkMock = vi.spyOn(fs.promises, 'unlink').mockImplementation(async (target) => {
    if (target === filename) throw Object.assign(new Error('unlink failed'), { code: 'EIO' });
    return unlink(target);
  });
  try {
    await assert.rejects(f.runtime.releaseAttachment(SESSION, 'release-fault-1', 'file', DIRECTORY), { status: 503 });
    assert.equal(JSON.parse(await fs.promises.readFile(f.file, 'utf8')).blobs[descriptor.blobId].state, 'released');
    assert.ok((await fs.promises.stat(filename)).isFile());
  } finally { unlinkMock.mockRestore(); }
  f.runtime.stop();
  const restored = await fixture(t, { dataDir: f.dataDir });
  await restored.runtime.releaseAttachment(SESSION, 'release-fault-1', 'file', DIRECTORY);
  await assert.rejects(fs.promises.stat(filename), { code: 'ENOENT' });
});

test('malformed converted manifest items cannot disappear from a parseable root without a request receipt', async (t) => {
  const f = await fixture(t);
  const queued = await f.runtime.enqueue(SESSION, DIRECTORY, item());
  await f.runtime.reservePrefix(SESSION, { id: 'msg_converted_corruption', itemIds: [queued.itemId], composer: '', sendConfig: item().sendConfig, format: 'manifest-v1', directory: DIRECTORY });
  f.runtime.stop();
  const stored = JSON.parse(await fs.promises.readFile(f.file, 'utf8'));
  stored.sessions[SESSION].items[0].sendConfig = null;
  const raw = JSON.stringify(stored);
  await fs.promises.writeFile(f.file, raw);
  const broken = createMessageQueueRuntime({ dataDir: f.dataDir });
  await assert.rejects(broken.load(), /invalid stored durable queue item/);
  assert.equal(await fs.promises.readFile(f.file, 'utf8'), raw);
  broken.stop();
});

test('a bad manifest at each batch position preserves valid siblings and corrected retry never duplicates them', async (t) => {
  for (const failedIndex of [0, 1, 2]) {
    const f = await fixture(t);
    const descriptor = await upload(f.runtime, 'batch-upload-source', 'file', Buffer.from('verified batch bytes'));
    const accepted = new Map();
    for (let index = 0; index < 3; index += 1) {
      const requestId = `manifest-batch-${failedIndex}-${index}`;
      const body = manifestInput([index === failedIndex ? { ...descriptor, sha256: '0'.repeat(64) } : { ...descriptor }]);
      if (index === failedIndex) await assert.rejects(f.runtime.enqueueRequest(SESSION, requestId, body), { code: 'queue_request_conflict' });
      else accepted.set(requestId, (await f.runtime.enqueueRequest(SESSION, requestId, body)).receipt);
    }
    const correctedId = `manifest-batch-${failedIndex}-${failedIndex}`;
    await f.runtime.enqueueRequest(SESSION, correctedId, manifestInput([descriptor]));
    for (const [requestId, receipt] of accepted) {
      assert.deepEqual((await f.runtime.enqueueRequest(SESSION, requestId, manifestInput([descriptor]))).receipt, receipt);
    }
    assert.equal(f.runtime.sessionSnapshot(SESSION).items.length, 3);
  }
});

test('manifest prefix context IDs preserve captured ownership and refuse IDs belonging to unrelated receipts', async (t) => {
  const f = await fixture(t);
  const queued = await f.runtime.enqueueRequest(SESSION, 'context-prefix-own', manifestInput([], {
    context: [{ id: 'msg_owned_prefix_context', text: 'original context', metadata: { original: true } }],
  }));
  const other = await f.runtime.enqueueRequest('ses_other_owner', 'context-prefix-other', manifestInput([], {
    context: [{ id: 'msg_unrelated_prefix_context', text: 'unrelated context' }],
  }));
  const request = { id: 'msg_prefix_context_capture', itemIds: [queued.receipt.itemId], composer: 'new text', sendConfig: item().sendConfig,
    format: 'manifest-v1', directory: DIRECTORY, promptParts: { context: [{ id: other.receipt.contextIds[0], text: 'conflicting context' }] } };
  await assert.rejects(f.runtime.reservePrefix(SESSION, request), { code: 'queue_request_conflict' });
  const capture = await f.runtime.reservePrefix(SESSION, { ...request, promptParts: { context: [{ text: 'composer context', description: 'preserved' }] } });
  assert.equal(capture.contextIds[0], queued.receipt.contextIds[0]);
  assert.equal(capture.contextIds.length, 2);
  assert.deepEqual((await f.runtime.previewPrefix(SESSION, capture.reservation)).contextIds, capture.contextIds);
});

test('lost manifest prompt admission retains blob ownership through restart until its explicit same-ID retry', async (t) => {
  const f = await fixture(t);
  const bytes = Buffer.from('uncertain delivery bytes');
  const descriptor = await upload(f.runtime, 'manifest-uncertain-1', 'file', bytes);
  const accepted = await f.runtime.enqueueRequest(SESSION, 'manifest-uncertain-1', manifestInput([descriptor]));
  f.state.fail = { match: (pathname) => pathname.endsWith('/prompt') };
  f.state.held = false;
  f.runtime.release(SESSION);
  await until(() => f.state.posts.length === 1 && f.runtime.sessionSnapshot(SESSION).sendingId === null);
  await f.runtime.flush();
  await assert.rejects(f.runtime.releaseAttachment(SESSION, 'manifest-uncertain-1', 'file', DIRECTORY), { status: 409 });
  f.runtime.stop();
  const restored = await fixture(t, { dataDir: f.dataDir, state: f.state });
  assert.equal((await restored.runtime.getRequest(SESSION, 'manifest-uncertain-1', DIRECTORY)).state, 'uncertain');
  assert.deepEqual((await restored.runtime.getAttachment(SESSION, 'manifest-uncertain-1', 'file', DIRECTORY)).bytes, bytes);
  await restored.runtime.retryRequest(SESSION, 'manifest-uncertain-1', DIRECTORY, 1);
  await until(() => restored.runtime.sessionSnapshot(SESSION).items.length === 0);
  await restored.runtime.flush();
  assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [accepted.receipt.messageId]);
  await restored.runtime.releaseAttachment(SESSION, 'manifest-uncertain-1', 'file', DIRECTORY);
});

test('an in-budget streamed manifest honors the RPC deadline and preserves valid neighbors', async (t) => {
  const deadline = new AbortController();
  let observed = false;
  const f = await fixture(t, { onRequest: async (url, init) => {
    const size = Number(init.headers?.['Content-Length'] ?? 0);
    if (!new URL(url).pathname.endsWith('/prompt') || size < 100 * 1024 ** 2) return;
    assert.ok(size <= MANIFEST_PROMPT_BYTE_LIMIT);
    assert.ok(init.body instanceof ReadableStream);
    assert.equal(init.duplex, 'half');
    const reader = init.body.getReader();
    try {
      for (let index = 0; index < 3; index += 1) assert.ok((await reader.read()).value.length <= 65536);
      deadline.abort(new Error('simulated RPC deadline'));
      assert.equal(init.signal.aborted, true);
      await assert.rejects(reader.read(), /simulated RPC deadline/);
      observed = true;
      throw init.signal.reason;
    } finally { reader.releaseLock(); }
  } });
  const before = await f.runtime.enqueueRequest(SESSION, 'stream-neighbor-before', input({ text: 'before' }));
  const descriptor = await upload(f.runtime, 'stream-amplification-upload', 'file', Buffer.alloc(BLOB_BYTE_LIMIT));
  let blobReads = 0;
  const open = fs.promises.open.bind(fs.promises);
  const openMock = vi.spyOn(fs.promises, 'open').mockImplementation(async (...args) => {
    if (String(args[0]).endsWith(`${descriptor.blobId}.blob`)) {
      blobReads += 1;
      assert.ok(blobReads <= 2, 'stop repeated large-file reads before a regression can exhaust memory');
    }
    return open(...args);
  });
  t.onTestFinished(() => openMock.mockRestore());
  const huge = await f.runtime.enqueueRequest(SESSION, 'stream-amplification-target', manifestInput(Array(4).fill(descriptor)));
  assert.equal(blobReads, 1);
  const after = await f.runtime.enqueueRequest(SESSION, 'stream-neighbor-after', input({ text: 'after' }));
  const timeoutMock = vi.spyOn(AbortSignal, 'timeout').mockImplementation(() => deadline.signal);
  try {
    f.state.held = false;
    f.runtime.release(SESSION);
    await until(() => observed && f.runtime.sessionSnapshot(SESSION).sendingId === null);
    assert.ok(timeoutMock.mock.calls.length > 0);
    assert.ok(timeoutMock.mock.calls.every(([milliseconds]) => milliseconds === 15000));
    assert.equal(blobReads, 2);
    assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [before.receipt.messageId]);
    assert.deepEqual(f.runtime.sessionSnapshot(SESSION).items.map((entry) => entry.id), [huge.receipt.itemId, after.receipt.itemId]);
    assert.equal((await f.runtime.getRequest(SESSION, 'stream-amplification-target', DIRECTORY)).state, 'uncertain');
  } finally {
    timeoutMock.mockRestore();
    deadline.abort();
  }
  await f.runtime.remove(SESSION, huge.receipt.itemId);
  await until(() => f.runtime.sessionSnapshot(SESSION).items.length === 0);
  await f.runtime.flush();
  assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [before.receipt.messageId, after.receipt.messageId]);
}, 20000);

test('4096 copies of a 20 MiB blob stay queued with a durable size error and no upstream calls', async (t) => {
  let calls = 0;
  const f = await fixture(t, { onRequest: async () => { calls += 1; } });
  const descriptor = await upload(f.runtime, 'budget-repeated-source', 'file', Buffer.alloc(BLOB_BYTE_LIMIT));
  const body = manifestInput(Array(4096).fill(descriptor));
  const accepted = await f.runtime.enqueueRequest(SESSION, 'budget-repeated-target', body);
  assert.equal(accepted.state, 'queued');
  assert.equal(accepted.attempt, 0);
  assert.equal(accepted.blocked, 'prompt_body_too_large');
  f.state.held = false;
  await fireIdleTimer(f.runtime);
  await fireIdleTimer(f.runtime);
  assert.equal(calls, 0);
  assert.equal(f.runtime.sessionSnapshot(SESSION).items[0].queueDelivery.blocked, 'prompt_body_too_large');
  await assert.rejects(f.runtime.retryRequest(SESSION, 'budget-repeated-target', DIRECTORY, 1), { status: 409 });
  assert.equal((await f.runtime.enqueueRequest(SESSION, 'budget-repeated-target', body)).blocked, 'prompt_body_too_large');
  await f.runtime.flush();
  f.runtime.stop();
  // Restore the queued record without the new flag to cover accepted snapshots
  // from before this policy. The preflight must still precede uncertainty.
  const stored = JSON.parse(await fs.promises.readFile(f.file, 'utf8'));
  delete stored.requests['budget-repeated-target'].blocked;
  await fs.promises.writeFile(f.file, JSON.stringify(stored));
  const restored = await fixture(t, { dataDir: f.dataDir, state: f.state, onRequest: async () => { calls += 1; } });
  await fireIdleTimer(restored.runtime);
  await restored.runtime.flush();
  const recovered = await restored.runtime.getRequest(SESSION, 'budget-repeated-target', DIRECTORY);
  assert.deepEqual(recovered.receipt, accepted.receipt);
  assert.equal(recovered.state, 'queued');
  assert.equal(recovered.attempt, 0);
  assert.equal(recovered.blocked, 'prompt_body_too_large');
  assert.equal(calls, 0);
  const transfer = await restored.runtime.takeRequest(SESSION, 'budget-repeated-edit', { directory: DIRECTORY, itemIds: [accepted.receipt.itemId], format: 'manifest-v1' });
  assert.equal(transfer.items[0].attachmentManifest.length, 4096);
  await restored.runtime.acknowledgeTake(SESSION, 'budget-repeated-edit', DIRECTORY);
  assert.equal(calls, 0);
}, 20000);

test('size refusal precedes all synthetic admission, preserves valid neighbors and remains editable', async (t) => {
  const posted = [];
  const f = await fixture(t, { onRequest: async (url, init) => { if (init.method === 'POST') posted.push(new URL(url).pathname); } });
  const before = await f.runtime.enqueueRequest(SESSION, 'budget-neighbor-before', input({ text: 'before' }));
  const descriptor = await upload(f.runtime, 'budget-context-source', 'file', Buffer.alloc(BLOB_BYTE_LIMIT));
  const body = manifestInput(Array(6).fill(descriptor), { context: [{ text: 'official queued context', metadata: { preserved: true } }] });
  body.item.context = [{ kind: 'context', text: 'legacy queued context', instructions: 'read before the prompt', metadata: { original: true } }];
  const blocked = await f.runtime.enqueueRequest(SESSION, 'budget-context-target', body);
  const after = await f.runtime.enqueueRequest(SESSION, 'budget-neighbor-after', input({ text: 'after' }));
  assert.equal(blocked.blocked, 'prompt_body_too_large');
  f.state.held = false;
  f.runtime.release(SESSION);
  await until(() => f.runtime.sessionSnapshot(SESSION).items[0]?.id === blocked.receipt.itemId && f.runtime.sessionSnapshot(SESSION).sendingId === null);
  await fireIdleTimer(f.runtime);
  assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [before.receipt.messageId]);
  assert.equal(posted.filter((pathname) => pathname.includes(`/${SESSION}/`)).length, 3);
  assert.equal(posted.some((pathname) => pathname.endsWith('/synthetic')), false);
  const healthy = await f.runtime.enqueueRequest('ses_budget_healthy', 'budget-independent', input({ text: 'healthy' }));
  await until(() => f.runtime.sessionSnapshot('ses_budget_healthy').items.length === 0);
  assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [before.receipt.messageId, healthy.receipt.messageId]);
  const transfer = await f.runtime.takeRequest(SESSION, 'budget-context-edit', { directory: DIRECTORY, itemIds: [blocked.receipt.itemId], format: 'manifest-v1' });
  assert.deepEqual(transfer.items[0].context, body.item.context);
  assert.deepEqual(transfer.items[0].promptParts.context, body.item.promptParts.context);
  await until(() => f.runtime.sessionSnapshot(SESSION).items.length === 0);
  await f.runtime.flush();
  assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [before.receipt.messageId, healthy.receipt.messageId, after.receipt.messageId]);
  assert.equal(posted.some((pathname) => pathname.endsWith('/synthetic')), false);
  await assert.rejects(f.runtime.releaseAttachment(SESSION, 'budget-context-source', 'file', DIRECTORY), { status: 409 });
  await f.runtime.acknowledgeTake(SESSION, 'budget-context-edit', DIRECTORY);
}, 20000);

test('reordering and explicitly removing a size-blocked head resume valid queued successors', async (t) => {
  const f = await fixture(t);
  const descriptor = await upload(f.runtime, 'budget-order-source', 'file', Buffer.alloc(BLOB_BYTE_LIMIT));
  const blocked = await f.runtime.enqueueRequest(SESSION, 'budget-order-blocked', manifestInput(Array(6).fill(descriptor)));
  const before = await f.runtime.enqueueRequest(SESSION, 'budget-order-before', input({ text: 'before' }));
  const after = await f.runtime.enqueueRequest(SESSION, 'budget-order-after', input({ text: 'after' }));
  f.state.held = false;
  await f.runtime.reorder(SESSION, [before.receipt.itemId, blocked.receipt.itemId, after.receipt.itemId]);
  await until(() => f.runtime.sessionSnapshot(SESSION).items[0]?.id === blocked.receipt.itemId && f.runtime.sessionSnapshot(SESSION).sendingId === null);
  assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [before.receipt.messageId]);
  await f.runtime.remove(SESSION, blocked.receipt.itemId);
  await until(() => f.runtime.sessionSnapshot(SESSION).items.length === 0);
  await f.runtime.flush();
  assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [before.receipt.messageId, after.receipt.messageId]);
}, 20000);

test('a size-blocked queue item can join a native file-URI prefix without a hypothetical base64 rejection', async (t) => {
  const f = await fixture(t);
  const descriptor = await upload(f.runtime, 'budget-native-source', 'file', Buffer.alloc(BLOB_BYTE_LIMIT));
  const body = manifestInput(Array(6).fill(descriptor), { context: [{ text: 'native context', metadata: { retained: true } }] });
  const accepted = await f.runtime.enqueueRequest(SESSION, 'budget-native-target', body);
  assert.equal(accepted.blocked, 'prompt_body_too_large');
  const request = { id: 'msg_native_staged_files', itemIds: [accepted.receipt.itemId], composer: 'native file references', sendConfig: item().sendConfig, format: 'manifest-v1', directory: DIRECTORY };
  const preview = await f.runtime.previewPrefix(SESSION, request);
  const reserved = await f.runtime.reservePrefix(SESSION, request);
  assert.deepEqual(reserved.items, preview.items);
  assert.deepEqual(reserved.contextIds, preview.contextIds);
  assert.equal(reserved.items[0].attachmentManifest.length, 6);
  const nativeBody = { id: request.id, text: body.item.text, files: Array.from({ length: 6 }, (_, index) => ({ uri: `file:///native-private-spool/file-${index}.bin` })) };
  assert.ok(Buffer.byteLength(JSON.stringify(nativeBody)) < MANIFEST_PROMPT_BYTE_LIMIT);
  f.state.inbox.set(request.id, { id: request.id, type: 'user', sessionID: SESSION, delivery: 'queue', time: { created: 1 } });
  f.runtime.release(SESSION);
  await until(() => f.runtime.sessionSnapshot(SESSION).items.length === 0);
  await f.runtime.flush();
  assert.deepEqual(f.state.posts, []);
}, 20000);

test('prior uncertain oversized admissions retain same-ID observation recovery and never send new context', async (t) => {
  const f = await fixture(t);
  const descriptor = await upload(f.runtime, 'budget-uncertain-source', 'file', Buffer.alloc(BLOB_BYTE_LIMIT));
  const accepted = await f.runtime.enqueueRequest(SESSION, 'budget-uncertain-target', manifestInput(Array(6).fill(descriptor), { context: [{ text: 'held context' }] }));
  f.runtime.stop();
  const stored = JSON.parse(await fs.promises.readFile(f.file, 'utf8'));
  stored.requests['budget-uncertain-target'].state = 'uncertain';
  delete stored.requests['budget-uncertain-target'].blocked;
  await fs.promises.writeFile(f.file, JSON.stringify(stored));
  f.state.held = false;
  const restored = await fixture(t, { dataDir: f.dataDir, state: f.state });
  await restored.runtime.retryRequest(SESSION, 'budget-uncertain-target', DIRECTORY, 1);
  await until(() => restored.runtime.sessionSnapshot(SESSION).items[0]?.queueDelivery.blocked === 'prompt_body_too_large' && restored.runtime.sessionSnapshot(SESSION).sendingId === null);
  assert.equal((await restored.runtime.getRequest(SESSION, 'budget-uncertain-target', DIRECTORY)).state, 'uncertain');
  assert.deepEqual(f.state.posts, []);
  f.state.inbox.set(accepted.receipt.messageId, { id: accepted.receipt.messageId, type: 'user', sessionID: SESSION, delivery: 'queue', time: { created: 1 } });
  await restored.runtime.retryRequest(SESSION, 'budget-uncertain-target', DIRECTORY, 2);
  await until(() => restored.runtime.sessionSnapshot(SESSION).items.length === 0);
  await restored.runtime.flush();
  assert.deepEqual(f.state.posts, []);
}, 20000);

test('legacy 9 by 1 MiB contexts exceed manifest metadata bounds and retain explicit legacy recovery', async (t) => {
  const f = await fixture(t);
  const context = Array.from({ length: 9 }, () => ({ kind: 'synthetic', text: 'x'.repeat(1024 * 1024) }));
  const body = item({ context });
  const bytes = Buffer.byteLength(JSON.stringify({ directory: DIRECTORY, item: body }));
  assert.ok(bytes > RECEIPT_BYTE_LIMIT && bytes < 50 * 1024 * 1024);
  const queued = await f.runtime.enqueue(SESSION, DIRECTORY, body);
  await f.runtime.flush();
  await assert.rejects(f.runtime.takeRequest(SESSION, 'large-context-manifest-take', { directory: DIRECTORY, itemIds: [queued.itemId], format: 'manifest-v1' }), { status: 413 });
  const request = { id: 'msg_large_context_prefix', itemIds: [queued.itemId], composer: 'native capture', sendConfig: item().sendConfig, format: 'manifest-v1', directory: DIRECTORY };
  await assert.rejects(f.runtime.previewPrefix(SESSION, request), { status: 413 });
  await assert.rejects(f.runtime.reservePrefix(SESSION, request), { status: 413 });
  assert.deepEqual(f.runtime.sessionSnapshot(SESSION).items.map((entry) => entry.id), [queued.itemId]);
  await assert.rejects(f.runtime.getRequest(SESSION, 'large-context-manifest-take', DIRECTORY), { code: 'queue_request_unknown' });
  const recovered = await f.runtime.take(SESSION, queued.itemId);
  assert.equal(recovered.item.context.length, 9);
  assert.ok(recovered.item.context.every((part) => Buffer.byteLength(part.text) === 1024 * 1024));
});

test('normal durable completion wakes its successor after an idle timer was consumed during the disk checkpoint', async (t) => {
  for (const busy of [false, true]) {
    const f = await fixture(t);
    const first = await f.runtime.enqueueRequest(SESSION, 'normal-checkpoint-first', input({ text: 'first' }));
    const second = await f.runtime.enqueueRequest(SESSION, 'normal-checkpoint-second', input({ text: 'second' }));
    const entered = deferred();
    const release = deferred();
    const rename = fs.promises.rename.bind(fs.promises);
    let paused = false;
    const mock = vi.spyOn(fs.promises, 'rename').mockImplementation(async (...args) => {
      if (args[1] === f.file && !paused) {
        const stored = JSON.parse(await fs.promises.readFile(args[0], 'utf8'));
        if (stored.requests['normal-checkpoint-first']?.state === 'dispatched') {
          paused = true;
          entered.resolve();
          await release.promise;
        }
      }
      return rename(...args);
    });
    try {
      f.state.held = false;
      f.runtime.release(SESSION);
      await entered.promise;
      await fireIdleTimer(f.runtime);
      assert.equal(f.runtime.sessionSnapshot(SESSION).sendingId, first.receipt.itemId);
      assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [first.receipt.messageId]);
      const reads = f.state.activeReads;
      if (busy) f.state.active = { [SESSION]: { type: 'running' } };
      release.resolve();
      if (busy) {
        await until(() => f.state.activeReads > reads && f.runtime.sessionSnapshot(SESSION).sendingId === null);
        assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [first.receipt.messageId]);
        f.state.active = {};
        f.runtime.release(SESSION);
      }
      await until(() => f.runtime.sessionSnapshot(SESSION).items.length === 0);
      await f.runtime.flush();
      assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [first.receipt.messageId, second.receipt.messageId]);
    } finally { release.resolve(); mock.mockRestore(); }
  }
});

test('exact enqueue recovery of a failed normal completion checkpoint wakes the queued successor', async (t) => {
  const f = await fixture(t);
  const body = input({ text: 'first' });
  const first = await f.runtime.enqueueRequest(SESSION, 'repeat-completion-first', body);
  const second = await f.runtime.enqueueRequest(SESSION, 'repeat-completion-second', input({ text: 'second' }));
  const rename = fs.promises.rename.bind(fs.promises);
  let fail = true;
  let failures = 0;
  const mock = vi.spyOn(fs.promises, 'rename').mockImplementation(async (...args) => {
    if (fail && args[1] === f.file) {
      const stored = JSON.parse(await fs.promises.readFile(args[0], 'utf8'));
      if (stored.requests['repeat-completion-first']?.state === 'dispatched') {
        failures += 1;
        throw new Error('normal completion checkpoint failed');
      }
    }
    return rename(...args);
  });
  try {
    f.state.held = false;
    f.runtime.release(SESSION);
    await until(() => failures === 2 && f.runtime.sessionSnapshot(SESSION).sendingId === null);
    assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [first.receipt.messageId]);
    fail = false;
    assert.deepEqual((await f.runtime.enqueueRequest(SESSION, 'repeat-completion-first', body)).receipt, first.receipt);
    await until(() => f.runtime.sessionSnapshot(SESSION).items.length === 0);
    await f.runtime.flush();
    assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [first.receipt.messageId, second.receipt.messageId]);
  } finally { mock.mockRestore(); }
});

test('pending manifest reservations fence prompt and context IDs against new enqueues through restart and exact recovery', async (t) => {
  const f = await fixture(t);
  const legacy = await f.runtime.enqueue(SESSION, DIRECTORY, item({ context: [{ kind: 'context', text: 'queued context', instructions: 'queued instructions', metadata: {} }] }));
  const sourceBody = manifestInput([], { context: [{ id: 'msg_selected_receipt_context', text: 'selected context' }],
    fileReferences: [{ uri: 'file:///server/reference.pdf', name: 'reference.pdf' }] });
  const selected = await f.runtime.enqueueRequest(SESSION, 'reserved-existing-source', sourceBody);
  const request = { id: 'msg_fenced_prefix_prompt', itemIds: [legacy.itemId, selected.receipt.itemId], composer: 'composer', sendConfig: item().sendConfig,
    format: 'manifest-v1', directory: DIRECTORY, promptParts: { context: [{ text: 'composer context' }, { id: 'msg_explicit_composer_context', text: 'explicit composer context' }] } };
  const captured = await f.runtime.reservePrefix(SESSION, request);
  const reserved = [request.id, ...captured.contextIds];
  for (const restored of [false, true]) {
    let runtime = f.runtime;
    if (restored) {
      f.runtime.stop();
      await f.runtime.flush();
      runtime = (await fixture(t, { dataDir: f.dataDir })).runtime;
    }
    for (const [index, id] of reserved.entries()) {
      const target = index % 2 ? 'ses_conflicting_other' : SESSION;
      const key = `prefix-conflict-${restored}-${index}`;
      await assert.rejects(runtime.enqueueRequest(target, key, manifestInput([], { context: [{ id, text: 'conflicting append' }] })), { code: 'queue_request_conflict' });
      await assert.rejects(runtime.getRequest(target, key, DIRECTORY), { code: 'queue_request_unknown' });
      const repeated = await runtime.reservePrefix(SESSION, request);
      assert.deepEqual(repeated.items, captured.items);
      assert.deepEqual(repeated.contextIds, captured.contextIds);
    }
    assert.deepEqual((await runtime.enqueueRequest(SESSION, 'reserved-existing-source', sourceBody)).receipt, selected.receipt);
    await runtime.enqueueRequest('ses_valid_neighbor', `prefix-valid-${restored}`, manifestInput());
    const neighbor = (await runtime.enqueue('ses_other_prefix', DIRECTORY, item())).itemId;
    await assert.rejects(runtime.reservePrefix('ses_other_prefix', { id: `msg_other_prefix_${restored}`, itemIds: [neighbor], composer: 'another composer',
      sendConfig: item().sendConfig, format: 'manifest-v1', directory: DIRECTORY, promptParts: { context: [{ id: captured.contextIds[0], text: 'conflicting reservation' }] } }), { code: 'queue_request_conflict' });
    await runtime.remove('ses_other_prefix', neighbor);
  }
});

test('failed legacy inline take recovery arms actual successors from exact repeat and lookup while preserving live busy guards', async (t) => {
  for (const recovery of ['repeat', 'lookup']) for (const busy of [false, true]) {
    const f = await fixture(t);
    const first = await f.runtime.enqueue(SESSION, DIRECTORY, item({ text: 'transferred' }));
    const next = await f.runtime.enqueue(SESSION, DIRECTORY, item({ text: 'next legacy message' }));
    await f.runtime.flush();
    await fireIdleTimer(f.runtime);
    const rename = fs.promises.rename.bind(fs.promises);
    let fail = true;
    const mock = vi.spyOn(fs.promises, 'rename').mockImplementation(async (...args) => {
      if (fail && args[1] === f.file) throw new Error('inline take checkpoint failed');
      return rename(...args);
    });
    try {
      const body = { directory: DIRECTORY, itemIds: [first.itemId] };
      await assert.rejects(f.runtime.takeRequest(SESSION, 'inline-take-recovery', body), /inline take checkpoint failed/);
      assert.deepEqual(f.runtime.sessionSnapshot(SESSION).items.map((entry) => entry.id), [next.itemId]);
      assert.deepEqual(f.state.posts, []);
      fail = false;
      f.state.held = false;
      if (busy) f.state.active = { [SESSION]: { type: 'running' } };
      const reply = recovery === 'repeat'
        ? await f.runtime.takeRequest(SESSION, 'inline-take-recovery', body)
        : await f.runtime.getRequest(SESSION, 'inline-take-recovery', DIRECTORY);
      assert.equal(reply.items[0].id, first.itemId);
      if (busy) {
        await until(() => f.state.activeReads > 0);
        assert.deepEqual(f.state.posts, []);
        f.state.active = {};
        f.runtime.release(SESSION);
      }
      await until(() => f.runtime.sessionSnapshot(SESSION).items.length === 0);
      await f.runtime.flush();
      assert.deepEqual(f.state.posts.map((entry) => entry.body.text), ['next legacy message']);
      assert.deepEqual((await f.runtime.takeRequest(SESSION, 'inline-take-recovery', body)).items, reply.items);
    } finally { mock.mockRestore(); }
  }
});

test('manifest prefix directory rejects A after a legacy reassignment to B, then recovers only under B through restart', async (t) => {
  const f = await fixture(t);
  const movedDirectory = '/moved/repository';
  const bytes = Buffer.from('actual bytes');
  const queued = await f.runtime.enqueue(SESSION, DIRECTORY, item({
    context: [{ kind: 'synthetic', text: 'captured under the original queue' }],
    attachments: [{ id: 'legacy-file', filename: 'owned.txt', mimeType: 'text/plain', size: bytes.length, source: 'local', dataUrl: `data:text/plain;base64,${bytes.toString('base64')}` }],
  }));
  const capturedA = { id: 'msg_directory_bound_prefix', directory: DIRECTORY, itemIds: [queued.itemId], composer: 'typed under A', sendConfig: item().sendConfig, format: 'manifest-v1' };
  const tail = await f.runtime.enqueue(SESSION, movedDirectory, item({ text: 'new owner B' }));
  await f.runtime.flush();
  const snapshot = structuredClone(f.runtime.snapshot());
  assert.deepEqual(f.runtime.sessionSnapshot(SESSION).items.map((entry) => entry.id), [queued.itemId, tail.itemId]);
  assert.equal(f.runtime.sessionSnapshot(SESSION).directory, movedDirectory);
  const raw = await fs.promises.readFile(f.file, 'utf8');
  const routes = new Map();
  const app = Object.fromEntries(['get', 'post', 'put', 'delete'].map((method) => [method, (route, handler) => routes.set(`${method} ${route}`, handler)]));
  registerMessageQueueRoutes(app, f.runtime);
  for (const suffix of ['preview-prefix', 'reserve-prefix']) {
    const response = { statusCode: 200, status(code) { this.statusCode = code; return this; }, json(value) { this.data = value; } };
    await routes.get(`post /api/message-queue/sessions/:sessionId/${suffix}`)({ params: { sessionId: SESSION }, body: capturedA, headers: {} }, response);
    assert.equal(response.statusCode, 409);
    assert.equal(response.data.code, 'queue_request_conflict');
    assert.deepEqual(f.runtime.snapshot(), snapshot);
    assert.equal(await fs.promises.readFile(f.file, 'utf8'), raw);
  }
  const capturedB = { ...capturedA, directory: movedDirectory };
  const preview = await f.runtime.previewPrefix(SESSION, capturedB);
  const reserved = await f.runtime.reservePrefix(SESSION, capturedB);
  assert.equal(reserved.reservation.directory, movedDirectory);
  assert.deepEqual(reserved.items, preview.items);
  assert.deepEqual(reserved.contextIds, preview.contextIds);
  const file = reserved.items[0].attachmentManifest[0];
  await assert.rejects(f.runtime.getAttachment(SESSION, file.requestId, file.attachmentId, DIRECTORY), { status: 404 });
  assert.deepEqual((await f.runtime.getAttachment(SESSION, file.requestId, file.attachmentId, movedDirectory)).bytes, bytes);
  const verify = async (runtime) => {
    await assert.rejects(runtime.previewPrefix(SESSION, capturedA), { status: 409, code: 'queue_request_conflict' });
    await assert.rejects(runtime.reservePrefix(SESSION, capturedA), { status: 409, code: 'queue_request_conflict' });
    const repeated = await runtime.reservePrefix(SESSION, capturedB);
    assert.deepEqual(repeated.reservation, reserved.reservation);
    assert.deepEqual(repeated.items, reserved.items);
    assert.deepEqual(repeated.contextIds, reserved.contextIds);
    assert.equal(runtime.sessionSnapshot(SESSION).directory, movedDirectory);
    assert.deepEqual(runtime.sessionSnapshot(SESSION).items.map((entry) => entry.id), [queued.itemId, tail.itemId]);
  };
  await verify(f.runtime);
  f.runtime.stop();
  const restored = await fixture(t, { dataDir: f.dataDir });
  await verify(restored.runtime);
});

test('manifest prefix directory is required, exact and bounded while empty queues retain their owner projection', async (t) => {
  const f = await fixture(t);
  const queued = await f.runtime.enqueue(SESSION, DIRECTORY, item());
  await f.runtime.flush();
  const base = { id: 'msg_strict_prefix_owner', itemIds: [queued.itemId], composer: 'typed', sendConfig: item().sendConfig, format: 'manifest-v1' };
  const snapshot = structuredClone(f.runtime.snapshot());
  for (const directory of [undefined, '', ' ', ` ${DIRECTORY}`, `${DIRECTORY} `, null, 42, 'x'.repeat(16385)]) {
    await assert.rejects(f.runtime.previewPrefix(SESSION, { ...base, directory }), TypeError);
    await assert.rejects(f.runtime.reservePrefix(SESSION, { ...base, directory }), TypeError);
    assert.deepEqual(f.runtime.snapshot(), snapshot);
  }
  await assert.rejects(f.runtime.previewPrefix(SESSION, { ...base, directory: 'x'.repeat(16384) }), { status: 409 });
  assert.equal(JSON.parse(await fs.promises.readFile(f.file, 'utf8')).sessions[SESSION].reservation, undefined);
  await f.runtime.remove(SESSION, queued.itemId);
  const empty = structuredClone(f.runtime.sessionSnapshot(SESSION));
  for (const directory of [DIRECTORY, '/wrong/owner']) {
    await assert.rejects(f.runtime.previewPrefix(SESSION, { ...base, directory }), { status: 409 });
    await assert.rejects(f.runtime.reservePrefix(SESSION, { ...base, directory }), { status: 409 });
    assert.deepEqual(f.runtime.sessionSnapshot(SESSION), empty);
  }
  assert.equal(empty.directory, DIRECTORY);
  assert.deepEqual(empty.items, []);
});

test('persisted manifest prefix owners must agree with their queue before loading grants any authority', async (t) => {
  const f = await fixture(t);
  const queued = await f.runtime.enqueue(SESSION, DIRECTORY, item());
  await f.runtime.reservePrefix(SESSION, { id: 'msg_stored_prefix_owner', directory: DIRECTORY, itemIds: [queued.itemId], composer: 'typed', sendConfig: item().sendConfig, format: 'manifest-v1' });
  f.runtime.stop();
  const stored = JSON.parse(await fs.promises.readFile(f.file, 'utf8'));
  stored.sessions[SESSION].reservation.directory = '/different/stored/owner';
  const raw = JSON.stringify(stored);
  await fs.promises.writeFile(f.file, raw);
  const broken = createMessageQueueRuntime({ dataDir: f.dataDir });
  await assert.rejects(broken.load(), /stored queue reservation does not match its prefix/);
  assert.equal(await fs.promises.readFile(f.file, 'utf8'), raw);
  broken.stop();
});

test('malformed session entries cannot silently discard legacy-origin manifest reservations or grant write authority', async (t) => {
  for (const damage of ['missing-directory', 'null-directory', 'empty-directory', 'blank-directory', 'missing-items', 'empty-items', 'null-session']) {
    const f = await fixture(t);
    const healthy = await f.runtime.enqueueRequest('ses_preserved_healthy', 'preserved-healthy-request', input({ text: 'unrelated durable item' }));
    const queued = await f.runtime.enqueue(SESSION, DIRECTORY, item({ text: 'legacy-origin reserved item' }));
    await f.runtime.reservePrefix(SESSION, { id: 'msg_preserved_legacy_prefix', directory: DIRECTORY, itemIds: [queued.itemId], composer: 'captured composer', sendConfig: item().sendConfig, format: 'manifest-v1' });
    f.runtime.stop();
    await f.runtime.flush();
    const valid = await fs.promises.readFile(f.file, 'utf8');
    const original = JSON.parse(valid);
    assert.equal(original.sessions[SESSION].items[0].requestId, undefined);
    const damaged = JSON.parse(valid);
    if (damage === 'missing-directory') delete damaged.sessions[SESSION].directory;
    if (damage === 'null-directory') damaged.sessions[SESSION].directory = null;
    if (damage === 'empty-directory') damaged.sessions[SESSION].directory = '';
    if (damage === 'blank-directory') damaged.sessions[SESSION].directory = ' \t\n ';
    if (damage === 'missing-items') delete damaged.sessions[SESSION].items;
    if (damage === 'empty-items') damaged.sessions[SESSION].items = [];
    if (damage === 'null-session') damaged.sessions[SESSION] = null;
    const raw = JSON.stringify(damaged);
    await fs.promises.writeFile(f.file, raw);
    const broken = createMessageQueueRuntime({ dataDir: f.dataDir });
    t.onTestFinished(() => broken.stop());
    const failure = /invalid stored queue (session|directory|items)|stored queue reservation lost its items/;
    await assert.rejects(broken.load(), failure);
    const mutations = [
      () => broken.enqueue('ses_denied_legacy', DIRECTORY, item()),
      () => broken.enqueueRequest('ses_preserved_healthy', 'denied-durable-write', input()),
      () => broken.clear(SESSION),
      () => broken.takeRequest(SESSION, 'denied-transfer-write', { directory: DIRECTORY, itemIds: [queued.itemId], format: 'manifest-v1' }),
      () => broken.getRequest('ses_preserved_healthy', 'preserved-healthy-request', DIRECTORY),
    ];
    for (const mutation of mutations) {
      await assert.rejects(mutation(), failure);
      await broken.flush();
      assert.equal(await fs.promises.readFile(f.file, 'utf8'), raw);
    }
    const retained = JSON.parse(await fs.promises.readFile(f.file, 'utf8'));
    assert.deepEqual(retained.sessions.ses_preserved_healthy, original.sessions.ses_preserved_healthy);
    assert.deepEqual(retained.requests, original.requests);
    // Explicit repair restores the complete original root, including the
    // reservation that had no enqueue receipt to protect it indirectly.
    await fs.promises.writeFile(f.file, valid);
    await broken.load();
    assert.deepEqual(broken.sessionSnapshot(SESSION), f.runtime.sessionSnapshot(SESSION));
    assert.deepEqual(broken.sessionSnapshot('ses_preserved_healthy'), f.runtime.sessionSnapshot('ses_preserved_healthy'));
    assert.deepEqual((await broken.getRequest('ses_preserved_healthy', 'preserved-healthy-request', DIRECTORY)).receipt, healthy.receipt);
    await assert.rejects(broken.getRequest('ses_preserved_healthy', 'denied-durable-write', DIRECTORY), { code: 'queue_request_unknown' });
    assert.equal(await fs.promises.readFile(f.file, 'utf8'), valid);
    broken.stop();
  }
});

test('payload-backed 9 MiB contexts and large typed metadata survive compact enqueue, restart, take, downloads and acknowledgement', async (t) => {
  const f = await fixture(t);
  const file = await upload(f.runtime, 'payload-hidden-file', 'image', Buffer.from('actual attachment bytes'));
  const mention = { start: 0, end: 4, text: 'test' };
  const capture = manifestInput([{ ...file, description: 'original description', mention }], {
    agents: [{ name: 'reviewer', mention }], skills: [{ id: 'skill_saved', mention }],
    metadata: { large: 'm'.repeat(1024 * 1024), nested: [null, { preserved: true }] },
    context: [{ id: 'msg_full_payload_explicit', text: 'official synthetic', description: 'official description', metadata: { key: 'value' } }],
  });
  capture.item.context = Array.from({ length: 9 }, (_, index) => ({ kind: 'context', text: String(index).repeat(1024 * 1024),
    instructions: `instruction-${index}`, metadata: { index, original: true } }));
  capture.item.contextPreview = 'frozen context preview';
  capture.item.sendConfig = { providerID: 'frozen-provider', modelID: 'frozen-model', agent: 'frozen-agent', variant: 'frozen-variant' };
  const pointer = await uploadPayload(f.runtime, 'full-structured-upload', capture, 2 * 1024 * 1024);
  const body = { directory: DIRECTORY, item: pointer };
  const accepted = await f.runtime.enqueueRequest(SESSION, 'full-structured-enqueue', body);
  assert.equal(accepted.receipt.contextIds.length, 19);
  assert.equal(accepted.receipt.contextIds.at(-1), 'msg_full_payload_explicit');
  assert.ok(Buffer.byteLength(await fs.promises.readFile(f.file, 'utf8')) < 65536);
  assert.equal(f.runtime.sessionSnapshot(SESSION).items[0].format, 'payload-v1');
  assert.deepEqual(f.runtime.sessionSnapshot(SESSION).items[0].sendConfig, capture.item.sendConfig);
  await assert.rejects(f.runtime.take(SESSION, accepted.receipt.itemId), { status: 409 });
  for (const descriptor of [file, ...pointer.payloadManifest.chunks]) {
    await assert.rejects(f.runtime.releaseAttachment(SESSION, descriptor.requestId, descriptor.attachmentId, DIRECTORY), { status: 409 });
  }
  f.runtime.stop();
  await f.runtime.flush();
  let blobReads = 0;
  const open = fs.promises.open.bind(fs.promises);
  const spy = vi.spyOn(fs.promises, 'open').mockImplementation((...args) => {
    if (String(args[0]).endsWith('.blob')) blobReads += 1;
    return open(...args);
  });
  let restored;
  try {
    restored = await fixture(t, { dataDir: f.dataDir });
    assert.deepEqual((await restored.runtime.getRequest(SESSION, 'full-structured-enqueue', DIRECTORY)).receipt, accepted.receipt);
    assert.equal(blobReads, 0, 'startup and receipt lookup must not materialize payloads');
  } finally { spy.mockRestore(); }
  const takeBody = { directory: DIRECTORY, itemIds: [accepted.receipt.itemId], format: 'payload-v1' };
  const transfer = await restored.runtime.takeRequest(SESSION, 'full-structured-take', takeBody);
  const captured = transfer.items[0];
  assert.equal(captured.format, 'payload-v1');
  assert.equal('context' in captured, false);
  assert.equal('text' in captured, false);
  assert.deepEqual(captured.payloadIndex.contextIds, accepted.receipt.contextIds);
  assert.deepEqual(captured.payloadIndex.attachmentBlobIds, [file.blobId]);
  assert.ok(Buffer.byteLength(JSON.stringify(transfer)) < RECEIPT_BYTE_LIMIT);
  assert.deepEqual(await downloadPayload(restored.runtime, captured), capture);
  restored.runtime.stop();
  await restored.runtime.flush();
  const afterTake = await fixture(t, { dataDir: f.dataDir });
  assert.deepEqual((await afterTake.runtime.takeRequest(SESSION, 'full-structured-take', takeBody)).items, transfer.items);
  assert.deepEqual((await afterTake.runtime.getRequest(SESSION, 'full-structured-take', DIRECTORY)).items, transfer.items);
  for (const descriptor of [file, ...pointer.payloadManifest.chunks]) {
    await assert.rejects(afterTake.runtime.releaseAttachment(SESSION, descriptor.requestId, descriptor.attachmentId, DIRECTORY), { status: 409 });
  }
  await afterTake.runtime.acknowledgeTake(SESSION, 'full-structured-take', DIRECTORY);
  for (const descriptor of [file, ...pointer.payloadManifest.chunks]) {
    await afterTake.runtime.releaseAttachment(SESSION, descriptor.requestId, descriptor.attachmentId, DIRECTORY);
  }
  assert.equal((await afterTake.runtime.getRequest(SESSION, 'full-structured-take', DIRECTORY)).items, undefined);
  assert.deepEqual((await afterTake.runtime.enqueueRequest(SESSION, 'full-structured-enqueue', body)).receipt, accepted.receipt);
});

test('payload prefix converts a real 9 MiB legacy capture and retains a typed large composer behind compact restart-stable pointers', async (t) => {
  const f = await fixture(t);
  const context = Array.from({ length: 9 }, (_, index) => ({ kind: 'synthetic', text: String(index).repeat(1024 * 1024) }));
  const legacyFile = { id: 'legacy-file-identity', filename: 'legacy.txt', mimeType: 'text/plain', size: 3,
    source: 'server', serverPath: '/captured/legacy/file', dataUrl: 'data:text/plain;base64,YWJj' };
  const legacy = await f.runtime.enqueue(SESSION, DIRECTORY, item({ context, agentMention: 'legacy-agent', attachments: [legacyFile] }));
  const file = await upload(f.runtime, 'composer-hidden-file', 'file', Buffer.from('composer file'));
  const composer = manifestInput([file], { context: [{ id: 'msg_composer_payload_context', text: 'c'.repeat(9 * 1024 * 1024), metadata: { composer: true } }],
    skills: [{ id: 'skill_composer' }], metadata: { composer: { preserved: true } } });
  const composerPointer = await uploadPayload(f.runtime, 'composer-structured-upload', composer);
  const request = { format: 'payload-v1', directory: DIRECTORY, id: 'msg_large_typed_prefix', itemIds: [legacy.itemId], composer: composerPointer, delivery: 'steer' };
  const preview = await f.runtime.previewPrefix(SESSION, request);
  assert.equal(JSON.parse(await fs.promises.readFile(f.file, 'utf8')).sessions[SESSION].items[0].format, undefined);
  const legacyCapture = (await downloadPayload(f.runtime, preview.items[0])).item;
  assert.deepEqual(legacyCapture.context, context);
  assert.equal(legacyCapture.agentMention, 'legacy-agent');
  assert.equal(legacyCapture.attachments[0].dataUrl, legacyFile.dataUrl);
  assert.equal(legacyCapture.attachments[0].serverPath, legacyFile.serverPath);
  assert.deepEqual(preview.items[0].payloadIndex.legacyAttachmentIds, [legacyFile.id]);
  const reserved = await f.runtime.reservePrefix(SESSION, request);
  assert.deepEqual(reserved.items, preview.items);
  assert.deepEqual(reserved.contextIds, preview.contextIds);
  assert.equal(reserved.contextIds.at(-1), 'msg_composer_payload_context');
  assert.deepEqual(await downloadPayload(f.runtime, reserved.reservation.composer), composer);
  assert.ok(Buffer.byteLength(JSON.stringify(reserved)) < RECEIPT_BYTE_LIMIT);
  assert.ok(Buffer.byteLength(await fs.promises.readFile(f.file, 'utf8')) < 65536);
  await assert.rejects(f.runtime.releaseAttachment(SESSION, file.requestId, file.attachmentId, DIRECTORY), { status: 409 });
  await assert.rejects(f.runtime.remove(SESSION, legacy.itemId), { status: 409 });
  await assert.rejects(f.runtime.reservePrefix(SESSION, { ...request, directory: '/wrong/owner' }), { code: 'queue_request_conflict' });
  f.runtime.stop();
  await f.runtime.flush();
  const restored = await fixture(t, { dataDir: f.dataDir, state: f.state });
  assert.deepEqual((await restored.runtime.reservePrefix(SESSION, request)).items, reserved.items);
  assert.deepEqual((await restored.runtime.previewPrefix(SESSION, request)).contextIds, reserved.contextIds);
  await assert.rejects(restored.runtime.reservePrefix(SESSION, { ...request, directory: '/wrong/owner' }), { code: 'queue_request_conflict' });
  await assert.rejects(restored.runtime.enqueueRequest('ses_other_payload', 'payload-fenced-context', {
    directory: DIRECTORY, item: { ...manifestInput([], { context: [{ id: reserved.contextIds[0], text: 'collision' }] }).item },
  }), { code: 'queue_request_conflict' });
  for (const descriptor of [file, ...composerPointer.payloadManifest.chunks, ...reserved.items[0].payloadManifest.chunks]) {
    await assert.rejects(restored.runtime.releaseAttachment(SESSION, descriptor.requestId, descriptor.attachmentId, DIRECTORY), { status: 409 });
  }
  f.state.inbox.set(request.id, { id: request.id, type: 'user', sessionID: SESSION, delivery: 'steer', time: { created: 1 } });
  restored.runtime.release(SESSION);
  await until(() => restored.runtime.sessionSnapshot(SESSION).items.length === 0);
  await restored.runtime.flush();
  assert.deepEqual(f.state.posts, []);
  for (const descriptor of [file, ...composerPointer.payloadManifest.chunks, ...reserved.items[0].payloadManifest.chunks]) {
    await restored.runtime.releaseAttachment(SESSION, descriptor.requestId, descriptor.attachmentId, DIRECTORY);
  }
});

test('payload schema and chunk refusals leave ownership untouched and corrected requests preserve valid siblings', async (t) => {
  const f = await fixture(t);
  const before = await f.runtime.enqueueRequest(SESSION, 'payload-sibling-before', input({ text: 'before' }));
  const pointer = await uploadPayload(f.runtime, 'valid-structured-upload', manifestInput());
  const changed = (change) => { const copy = structuredClone(pointer); change(copy.payloadManifest); return copy; };
  const invalidPointers = [
    { ...item(), payloadManifest: pointer.payloadManifest },
    changed((manifest) => { manifest.totalBytes = STRUCTURED_BYTE_LIMIT + 1; }),
    changed((manifest) => { manifest.totalBytes += 1; }),
    changed((manifest) => { manifest.chunks[0].ordinal = 1; }),
    changed((manifest) => { manifest.chunks.push({ ...manifest.chunks[0], ordinal: 1 }); manifest.totalBytes *= 2; }),
    changed((manifest) => { manifest.sha256 = '0'.repeat(64); }),
    changed((manifest) => { manifest.chunks[0].sha256 = '0'.repeat(64); }),
    changed((manifest) => { manifest.chunks[0].blobId = '0'.repeat(64); }),
    changed((manifest) => { manifest.chunks[0].url = 'https://untrusted.invalid/payload'; }),
    { ...pointer, payloadIndex: { directory: DIRECTORY } },
  ];
  const wrongCapture = manifestInput();
  wrongCapture.directory = '/foreign/payload/owner';
  const unknown = manifestInput();
  unknown.item.context = [{ kind: 'synthetic', text: 'context', hidden: true }];
  const badConfig = manifestInput();
  badConfig.item.sendConfig.extra = true;
  const badParts = manifestInput();
  badParts.item.promptParts = { unknown: true };
  const invalidBytes = [Buffer.from('{'), Buffer.from([0x7b, 0xff, 0x7d]), Buffer.from('\ufeff{}'),
    wrongCapture, unknown, badConfig, badParts, { ...manifestInput(), extra: true }];
  for (const [index, capture] of invalidBytes.entries()) invalidPointers.push(await uploadPayload(f.runtime, `invalid-payload-${index}`, capture));
  const snapshot = structuredClone(f.runtime.snapshot());
  for (const invalid of invalidPointers) {
    await assert.rejects(f.runtime.enqueueRequest(SESSION, 'payload-correctable-request', { directory: DIRECTORY, item: invalid }));
    assert.deepEqual(f.runtime.snapshot(), snapshot);
    await assert.rejects(f.runtime.getRequest(SESSION, 'payload-correctable-request', DIRECTORY), { code: 'queue_request_unknown' });
    assert.deepEqual(f.state.posts, []);
  }
  const corrected = await f.runtime.enqueueRequest(SESSION, 'payload-correctable-request', { directory: DIRECTORY, item: pointer });
  const after = await f.runtime.enqueueRequest(SESSION, 'payload-sibling-after', input({ text: 'after' }));
  f.state.held = false;
  f.runtime.release(SESSION);
  await until(() => f.runtime.sessionSnapshot(SESSION).items.length === 0);
  await f.runtime.flush();
  assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [before.receipt.messageId, corrected.receipt.messageId, after.receipt.messageId]);
});

test('a corrupt retained payload fails only its item before any POST and needs explicit same-ID retry after repair', async (t) => {
  const f = await fixture(t);
  const mention = { start: 0, end: 2, text: '@a' };
  const file = await upload(f.runtime, 'payload-retry-file', 'file', Buffer.from('verified bytes'));
  const capture = manifestInput([{ ...file, mention, description: 'file description' }], {
    agents: [{ name: 'frozen-agent', mention }], skills: [{ id: 'frozen-skill', mention }], metadata: { prompt: true },
    context: [{ id: 'msg_payload_retry_explicit', text: 'official', description: 'context description', metadata: { keep: true } }],
  });
  capture.item.context = [{ kind: 'context', text: 'x'.repeat(9 * 1024 * 1024), instructions: 'read first', metadata: { retained: true } }];
  const pointer = await uploadPayload(f.runtime, 'payload-retry-upload', capture);
  const accepted = await f.runtime.enqueueRequest(SESSION, 'payload-retry-enqueue', { directory: DIRECTORY, item: pointer });
  f.runtime.stop();
  await f.runtime.flush();
  const chunk = pointer.payloadManifest.chunks[0];
  const target = path.join(f.dataDir, 'message-queue-blobs', `${chunk.blobId}.blob`);
  const original = await fs.promises.readFile(target);
  const damaged = Buffer.from(original);
  damaged[damaged.length - 1] ^= 1;
  await fs.promises.writeFile(target, damaged);
  const restored = await fixture(t, { dataDir: f.dataDir, state: f.state });
  f.state.held = false;
  restored.runtime.release(SESSION);
  await until(() => restored.runtime.sessionSnapshot(SESSION).items[0]?.queueDelivery.state === 'uncertain'
    && restored.runtime.sessionSnapshot(SESSION).sendingId === null);
  assert.deepEqual(f.state.posts, []);
  const healthy = await restored.runtime.enqueueRequest('ses_payload_healthy', 'payload-healthy-neighbor', input());
  await until(() => restored.runtime.sessionSnapshot('ses_payload_healthy').items.length === 0);
  for (const descriptor of [file, ...pointer.payloadManifest.chunks]) {
    await assert.rejects(restored.runtime.releaseAttachment(SESSION, descriptor.requestId, descriptor.attachmentId, DIRECTORY), { status: 409 });
  }
  await fs.promises.writeFile(target, original);
  await fireIdleTimer(restored.runtime);
  assert.equal(f.state.posts.length, 1);
  await restored.runtime.retryRequest(SESSION, 'payload-retry-enqueue', DIRECTORY, 1);
  await until(() => restored.runtime.sessionSnapshot(SESSION).items.length === 0);
  await restored.runtime.flush();
  const own = f.state.posts.filter((entry) => entry.body.id !== healthy.receipt.messageId);
  assert.deepEqual(own.map((entry) => entry.body.id), [...accepted.receipt.contextIds, accepted.receipt.messageId]);
  assert.equal(own[0].body.text, 'read first');
  assert.deepEqual(own[1].body.metadata, { retained: true });
  assert.equal(own[2].body.description, 'context description');
  assert.deepEqual(own.at(-1).body.agents, capture.item.promptParts.agents);
  assert.deepEqual(own.at(-1).body.skills, capture.item.promptParts.skills);
  assert.deepEqual(own.at(-1).body.metadata, capture.item.promptParts.metadata);
  assert.equal(own.at(-1).body.files[0].description, 'file description');
  assert.deepEqual(own.at(-1).body.files[0].mention, mention);
});

test('a damaged compact payload root index cannot release hidden references or acquire write authority after restart', async (t) => {
  const f = await fixture(t);
  const file = await upload(f.runtime, 'index-integrity-file', 'file', Buffer.from('hidden bytes'));
  const pointer = await uploadPayload(f.runtime, 'index-integrity-upload', manifestInput([file]));
  await f.runtime.enqueueRequest(SESSION, 'index-integrity-enqueue', { directory: DIRECTORY, item: pointer });
  f.runtime.stop();
  await f.runtime.flush();
  const stored = JSON.parse(await fs.promises.readFile(f.file, 'utf8'));
  stored.sessions[SESSION].items[0].payloadIndex.attachmentBlobIds = [];
  const raw = JSON.stringify(stored);
  await fs.promises.writeFile(f.file, raw);
  const broken = createMessageQueueRuntime({ dataDir: f.dataDir });
  await assert.rejects(broken.load(), /invalid stored durable queue item/);
  await assert.rejects(broken.releaseAttachment(SESSION, file.requestId, file.attachmentId, DIRECTORY), /invalid stored durable queue item/);
  await assert.rejects(broken.enqueueRequest(SESSION, 'denied-payload-index-write', input()), /invalid stored durable queue item/);
  assert.equal(await fs.promises.readFile(f.file, 'utf8'), raw);
  assert.ok((await fs.promises.stat(path.join(f.dataDir, 'message-queue-blobs', `${file.blobId}.blob`))).size > 0);
  broken.stop();
});

test('payload-backed attachment aliases obey the 128 MiB preflight before synthetic admission and stay takeable', async (t) => {
  const f = await fixture(t);
  const file = await upload(f.runtime, 'payload-amplified-file', 'file', Buffer.alloc(BLOB_BYTE_LIMIT, 0x61));
  const capture = manifestInput(Array(5).fill(file));
  capture.item.context = [{ kind: 'synthetic', text: 'must not be admitted' }];
  const pointer = await uploadPayload(f.runtime, 'payload-amplified-capture', capture);
  const accepted = await f.runtime.enqueueRequest(SESSION, 'payload-amplified-enqueue', { directory: DIRECTORY, item: pointer });
  assert.equal(accepted.state, 'queued');
  assert.equal(accepted.attempt, 0);
  assert.equal(accepted.blocked, 'prompt_body_too_large');
  f.state.held = false;
  await fireIdleTimer(f.runtime);
  assert.deepEqual(f.state.posts, []);
  const transferred = await f.runtime.takeRequest(SESSION, 'payload-amplified-take', { directory: DIRECTORY, itemIds: [accepted.receipt.itemId], format: 'payload-v1' });
  assert.deepEqual((await downloadPayload(f.runtime, transferred.items[0])).item.attachmentManifest, Array(5).fill(file));
  assert.ok(transferred.items[0].payloadIndex.promptBytes > MANIFEST_PROMPT_BYTE_LIMIT);
});

test('manifest takes enforce the combined entry bound before conversion and preserve complete valid siblings', async (t) => {
  for (const mixed of [false, true]) {
    const f = await fixture(t);
    const before = await f.runtime.enqueueRequest(SESSION, 'entry-neighbor-before', input({ text: 'before' }));
    let first;
    let second;
    if (mixed) {
      const descriptor = await upload(f.runtime, 'entry-file-source', 'file', Buffer.from('file'));
      const body = manifestInput(Array(50).fill(descriptor), {
        agents: Array.from({ length: 800 }, (_, index) => ({ name: `agent-${index}` })),
        skills: Array.from({ length: 600 }, (_, index) => ({ id: `skill-${index}` })),
        context: Array.from({ length: 600 }, () => ({ text: 'official context' })),
      });
      body.item.context = [{ kind: 'context', text: 'legacy context', instructions: 'legacy instruction', metadata: {} }];
      first = (await f.runtime.enqueueRequest(SESSION, 'entry-mixed-first', body)).receipt.itemId;
      second = (await f.runtime.enqueueRequest(SESSION, 'entry-mixed-second', body)).receipt.itemId;
    } else {
      const body = item({ context: Array.from({ length: 3000 }, () => ({ kind: 'synthetic', text: 'context' })) });
      first = (await f.runtime.enqueue(SESSION, DIRECTORY, body)).itemId;
      second = (await f.runtime.enqueue(SESSION, DIRECTORY, body)).itemId;
    }
    const after = await f.runtime.enqueueRequest(SESSION, 'entry-neighbor-after', input({ text: 'after' }));
    const ids = [before.receipt.itemId, first, second, after.receipt.itemId];
    await assert.rejects(f.runtime.takeRequest(SESSION, 'entry-overflow-take', { directory: DIRECTORY, itemIds: [first, second], format: 'manifest-v1' }), { status: 409 });
    assert.deepEqual(f.runtime.sessionSnapshot(SESSION).items.map((entry) => entry.id), ids);
    await assert.rejects(f.runtime.getRequest(SESSION, 'entry-overflow-take', DIRECTORY), { code: 'queue_request_unknown' });
    const taken = await f.runtime.takeRequest(SESSION, 'entry-corrected-take', { directory: DIRECTORY, itemIds: [first], format: 'manifest-v1' });
    assert.equal(taken.items[0].id, first);
    if (mixed) {
      assert.equal(taken.items[0].attachmentManifest.length, 50);
      assert.equal(taken.items[0].promptParts.agents.length, 800);
      assert.equal(taken.items[0].promptParts.skills.length, 600);
      assert.equal(taken.items[0].promptParts.context.length, 600);
      assert.equal(taken.items[0].context[0].instructions, 'legacy instruction');
    } else assert.equal(taken.items[0].context.length, 3000);
    await f.runtime.acknowledgeTake(SESSION, 'entry-corrected-take', DIRECTORY);
    await f.runtime.remove(SESSION, second);
    f.state.held = false;
    f.runtime.release(SESSION);
    await until(() => f.runtime.sessionSnapshot(SESSION).items.length === 0);
    await f.runtime.flush();
    assert.deepEqual(f.state.posts.map((entry) => entry.body.id), [before.receipt.messageId, after.receipt.messageId]);
  }
});

const serverFileReferences = () => [
  { uri: 'file:///server/missing/report.pdf', name: '/server/missing/report.pdf', description: 'PDF source', mention: { start: 0, end: 4, text: '@pdf' } },
  { uri: 'file:///server/missing/source.ts', name: 'source.ts' },
  { uri: 'file:///server/missing/report.pdf', name: '/server/missing/report.pdf', description: 'second occurrence' },
];

test('typed manifest and legacy payload references survive restart and automatic delivery after prepared bytes', async (t) => {
  for (const legacy of [false, true]) {
    const f = await fixture(t);
    const references = serverFileReferences();
    const prepared = legacy ? null : await upload(f.runtime, 'reference-file-upload', 'prepared', Buffer.from('abc'));
    const capture = legacy ? input({ attachments: [{ id: 'prepared', filename: 'prepared.txt', mimeType: 'text/plain', source: 'local', size: 3,
      dataUrl: 'data:text/plain;base64,YWJj' }], promptParts: { fileReferences: references } })
      : manifestInput([prepared], { fileReferences: references });
    const pointer = legacy ? await uploadPayload(f.runtime, 'reference-payload-upload', capture) : null;
    const request = pointer ? { directory: DIRECTORY, item: pointer } : capture;
    const accepted = await f.runtime.enqueueRequest(SESSION, 'reference-delivery-request', request);
    await f.runtime.flush();
    if (legacy) {
      const root = JSON.parse(await fs.promises.readFile(f.file, 'utf8'));
      const index = root.sessions[SESSION].items[0].payloadIndex;
      assert.deepEqual(index.attachmentBlobIds, []);
      assert.equal(index.entryCount, references.length + 1);
    }
    f.runtime.stop();
    const restored = await fixture(t, { dataDir: f.dataDir });
    assert.deepEqual((await restored.runtime.enqueueRequest(SESSION, 'reference-delivery-request', request)).receipt, accepted.receipt);
    restored.state.held = false;
    restored.runtime.release(SESSION);
    await until(() => restored.runtime.sessionSnapshot(SESSION).items.length === 0);
    const sent = restored.state.posts.find((entry) => entry.pathname.endsWith('/prompt')).body;
    assert.equal(sent.id, accepted.receipt.messageId);
    assert.deepEqual(sent.files.slice(1), references);
    assert.match(sent.files[0].uri, /^data:[^,]*;base64,YWJj$/);
  }
});

test('reference-only manifest take retains metadata through restart and acknowledgement without blob or byte proofs', async (t) => {
  const f = await fixture(t);
  const body = manifestInput([], { fileReferences: serverFileReferences() });
  body.item.content = '';
  body.item.text = '';
  const accepted = await f.runtime.enqueueRequest(SESSION, 'reference-only-enqueue', body);
  await assert.rejects(f.runtime.take(SESSION, accepted.receipt.itemId), { status: 409 });
  const takeBody = { directory: DIRECTORY, itemIds: [accepted.receipt.itemId], format: 'manifest-v1' };
  const taken = await f.runtime.takeRequest(SESSION, 'reference-only-take', takeBody);
  assert.deepEqual(taken.items[0].promptParts.fileReferences, body.item.promptParts.fileReferences);
  assert.deepEqual(taken.items[0].attachmentManifest, []);
  await f.runtime.flush();
  const root = JSON.parse(await fs.promises.readFile(f.file, 'utf8'));
  assert.deepEqual(Object.keys(root.blobs ?? {}), []);
  f.runtime.stop();
  const restored = await fixture(t, { dataDir: f.dataDir });
  assert.deepEqual((await restored.runtime.getRequest(SESSION, 'reference-only-take', DIRECTORY)).items, taken.items);
  assert.equal((await restored.runtime.acknowledgeTake(SESSION, 'reference-only-take', DIRECTORY)).state, 'acknowledged');
  assert.equal((await restored.runtime.getRequest(SESSION, 'reference-only-take', DIRECTORY)).items, undefined);
  const reenqueued = await restored.runtime.enqueueRequest(SESSION, 'reference-only-new-enqueue', body);
  assert.notEqual(reenqueued.receipt.itemId, accepted.receipt.itemId);
});

test('payload prefix freezes queued and composer references without changing context or prompt ownership fences', async (t) => {
  const f = await fixture(t);
  const queuedCapture = manifestInput([], { fileReferences: serverFileReferences(), context: [{ id: 'msg_reference_context', text: 'frozen context' }] });
  const queued = await f.runtime.enqueueRequest(SESSION, 'reference-prefix-source', { directory: DIRECTORY,
    item: await uploadPayload(f.runtime, 'reference-prefix-upload', queuedCapture) });
  const composerCapture = manifestInput([], { fileReferences: serverFileReferences().slice(0, 1) });
  const request = { id: 'msg_reference_prefix', itemIds: [queued.receipt.itemId], format: 'payload-v1', directory: DIRECTORY,
    composer: await uploadPayload(f.runtime, 'reference-composer-upload', composerCapture) };
  const preview = await f.runtime.previewPrefix(SESSION, request);
  const reserved = await f.runtime.reservePrefix(SESSION, request);
  assert.deepEqual(preview.contextIds, reserved.contextIds);
  assert.deepEqual((await downloadPayload(f.runtime, reserved.items[0])).item.promptParts.fileReferences, queuedCapture.item.promptParts.fileReferences);
  assert.deepEqual((await downloadPayload(f.runtime, reserved.reservation.composer)).item.promptParts.fileReferences, composerCapture.item.promptParts.fileReferences);
  await f.runtime.flush();
  f.runtime.stop();
  const restored = await fixture(t, { dataDir: f.dataDir });
  const repeated = await restored.runtime.reservePrefix(SESSION, request);
  assert.deepEqual(repeated.reservation, reserved.reservation);
  assert.deepEqual(repeated.items, reserved.items);
  await assert.rejects(restored.runtime.enqueueRequest(SESSION, 'reference-context-conflict', manifestInput([], {
    fileReferences: serverFileReferences(), context: [{ id: reserved.contextIds[0], text: 'conflicting context' }],
  })), { code: 'queue_request_conflict' });
  const root = JSON.parse(await fs.promises.readFile(f.file, 'utf8'));
  assert.equal(root.prefixPromptOwners[request.id].state, 'reserved');
  assert.equal(root.contextOwners[reserved.contextIds[0]].claims.at(-1).kind, 'prefix');
});

test('existing full non-blob take preserves references while legacy prefix and untyped context injection remain refused', async (t) => {
  const f = await fixture(t);
  const references = serverFileReferences();
  const accepted = await f.runtime.enqueueRequest(SESSION, 'inline-reference-request', input({ promptParts: { fileReferences: references } }));
  await assert.rejects(f.runtime.reservePrefix(SESSION, { id: 'msg_untyped_reference_prefix', itemIds: [accepted.receipt.itemId], composer: '', sendConfig: item().sendConfig }), { status: 409 });
  const taken = await f.runtime.take(SESSION, accepted.receipt.itemId);
  assert.deepEqual(taken.item.promptParts.fileReferences, references);
  const compatible = await f.runtime.enqueue(SESSION, DIRECTORY, item({ promptParts: { fileReferences: references } }));
  assert.deepEqual((await f.runtime.take(SESSION, compatible.itemId)).item.promptParts.fileReferences, references);
  await assert.rejects(f.runtime.enqueue(SESSION, DIRECTORY, item({ promptParts: { context: [{ id: 'msg_unowned_context', text: 'injected' }] } })), TypeError);
});
