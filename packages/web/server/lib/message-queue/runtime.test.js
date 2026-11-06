import fs from 'fs';
import os from 'os';
import path from 'path';
import { createHash } from 'node:crypto';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { createMessageQueueRuntime, parseQueuedItemInput, registerMessageQueueRoutes } from './runtime.js';

const SESSION = 'ses_queue_test_1';
const DIRECTORY = '/repo';

const item = (overrides = {}) => ({
  content: 'follow up',
  text: 'follow up',
  attachments: [],
  sendConfig: { providerID: 'anthropic', modelID: 'claude', agent: 'build' },
  ...overrides,
});

const tempDirs = [];
const makeDataDir = () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'openchamber-message-queue-'));
  tempDirs.push(dir);
  return dir;
};

afterEach(() => {
  vi.useRealTimers();
  for (const dir of tempDirs.splice(0)) fs.rmSync(dir, { recursive: true, force: true });
});

/**
 * A fake OpenCode: status map, message tail, command list, and a log of every
 * prompt/command it received.
 */
const createOpenCode = () => {
  const state = {
    // `/api/session/active` lists only running sessions; an absent id is idle.
    active: {},
    // Subagent sessions per parent, as `GET /api/session?parentID=` lists them.
    children: {},
    tail: [],
    commands: [],
    sent: [],
    switched: [],
    failNext: null,
    failStatus: 500,
    htmlNext: null,
  };
  // v2 wraps `/api/*` payloads in `{ location, data }`; a message page is
  // `{ data, cursor }` and lists newest first.
  const wrapped = (data) => Response.json({ location: { directory: DIRECTORY }, data });
  const fetchImpl = vi.fn(async (url, init = {}) => {
    const { pathname } = new URL(url);
    const method = init.method ?? 'GET';
    if (state.failNext && state.failNext.test(pathname)) {
      state.failNext = null;
      return new Response('boom', { status: state.failStatus });
    }
    if (method === 'POST' && state.htmlNext?.test(pathname)) {
      state.htmlNext = null;
      return new Response('<!doctype html><title>OpenChamber</title>', { status: 200, headers: { 'content-type': 'text/html' } });
    }
    // Real shape: `{ data }` without a `location`, unlike directory-scoped routes.
    if (pathname === '/api/session/active') return Response.json({ data: state.active });
    if (pathname === '/api/session' && method === 'GET') {
      const parentID = new URL(url).searchParams.get('parentID');
      return Response.json({ data: (state.children[parentID] ?? []).map((id) => ({ id, parentID })), cursor: {} });
    }
    if (/^\/api\/session\/[^/]+$/.test(pathname) && method === 'GET') {
      const id = decodeURIComponent(pathname.split('/').at(-1));
      const parentID = Object.keys(state.children).find((parent) => state.children[parent].includes(id));
      return wrapped({ id, parentID });
    }
    if (pathname.endsWith('/message')) return Response.json({ data: state.tail, cursor: {} });
    if (pathname === '/api/command') return wrapped(state.commands);
    if (method === 'POST' && (pathname.endsWith('/model') || pathname.endsWith('/agent'))) {
      // Kept apart from `sent`: switching the session is not a message.
      state.switched.push({ path: pathname, body: JSON.parse(init.body) });
      return new Response(null, { status: 204 });
    }
    if (method === 'POST' && (pathname.endsWith('/prompt') || pathname.endsWith('/command') || pathname.endsWith('/synthetic'))) {
      state.sent.push({ path: pathname, body: JSON.parse(init.body) });
      return wrapped({ id: `msg_${state.sent.length}` });
    }
    return new Response('not found', { status: 404 });
  });
  return { state, fetchImpl };
};

const createRuntime = ({ dataDir = makeDataDir(), openCode = createOpenCode(), knowledge = null, retryDelayMs, resolveAutoSelection, now, beforeScheduledTaskSend, onScheduledTaskResult, persistSessionGoal, isDeliveryHeld, deliveryHoldReady, onBroadcast } = {}) => {
  let eventHandler = () => {};
  let statusHandler = () => {};
  const broadcasts = [];
  const promptSent = [];
  const options = {
    globalEventHub: {
      subscribeEvent(handler) { eventHandler = handler; return () => {}; },
      subscribeStatus(handler) { statusHandler = handler; return () => {}; },
    },
    buildOpenCodeUrl: (fetchPath) => `http://opencode.test${fetchPath}`,
    getOpenCodeAuthHeaders: () => ({}),
    sessionKnowledgeRuntime: knowledge,
    broadcastGlobalUiEvent: (event) => { broadcasts.push(event); onBroadcast?.(event); },
    onPromptSent: (sessionId) => promptSent.push(sessionId),
    dataDir,
    fetchImpl: openCode.fetchImpl,
    dispatchQuietMs: 0,
    abortHoldMs: 50,
    beforeScheduledTaskSend,
    onScheduledTaskResult,
  };
  if (retryDelayMs) options.retryDelayMs = retryDelayMs;
  if (resolveAutoSelection) options.resolveAutoSelection = resolveAutoSelection;
  if (now) options.now = now;
  if (persistSessionGoal) options.persistSessionGoal = persistSessionGoal;
  if (isDeliveryHeld) options.isDeliveryHeld = isDeliveryHeld;
  if (deliveryHoldReady) options.deliveryHoldReady = deliveryHoldReady;
  const runtime = createMessageQueueRuntime(options);
  return {
    runtime,
    openCode,
    dataDir,
    broadcasts,
    promptSent,
    // The hub hands server-side subscribers already-translated events.
    emit: (payload, directory = DIRECTORY) => eventHandler({ payload, directory, translated: () => [payload] }),
    connect: () => statusHandler({ type: 'connect' }),
  };
};

const settle = async (ms = 30) => {
  await new Promise((resolve) => setTimeout(resolve, ms));
};

describe('transferred context ownership', () => {
  const capture = () => ({ directory: DIRECTORY, item: {
    format: 'manifest-v1', content: 'editable draft', text: 'editable draft', attachmentManifest: [],
    context: [{ kind: 'context', text: 'attached context', instructions: 'read this first', metadata: { retained: true } }],
    sendConfig: item().sendConfig,
    promptParts: { context: [{ id: 'msg_transfer_explicit_context', text: 'official context', metadata: { original: true } }] },
  } });
  // Use the same real upload and capture protocol as receipts.test.mjs. The
  // runtime creates and verifies every root index; tests never supply one.
  const uploadCapture = async (runtime, requestId, value, sessionId = SESSION) => {
    const bytes = Buffer.from(JSON.stringify(value));
    const sha256 = createHash('sha256').update(bytes).digest('hex');
    const { attachment } = await runtime.uploadAttachment(sessionId, requestId, 'capture', {
      directory: value.directory, filename: 'capture.json', mimeType: 'application/json', size: bytes.length, sha256,
    }, bytes);
    return { format: 'payload-v1', payloadManifest: { version: 1, encoding: 'utf-8-json', totalBytes: bytes.length,
      sha256, chunks: [{ ordinal: 0, ...attachment }] } };
  };
  const editCapture = async (runtime, transferred) => {
    const buffers = [];
    for (const descriptor of transferred.payloadManifest.chunks) {
      const { bytes } = await runtime.getAttachment(SESSION, descriptor.requestId, descriptor.attachmentId, DIRECTORY);
      expect(createHash('sha256').update(bytes).digest('hex')).toBe(descriptor.sha256);
      buffers.push(bytes);
    }
    const bytes = Buffer.concat(buffers);
    expect(bytes.length).toBe(transferred.payloadManifest.totalBytes);
    expect(createHash('sha256').update(bytes).digest('hex')).toBe(transferred.payloadManifest.sha256);
    const original = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes));
    const contexts = original.item.context.flatMap((entry) => {
      const part = { text: entry.text };
      if (entry.metadata) part.metadata = entry.metadata;
      return [...(entry.instructions ? [{ text: entry.instructions }] : []), part];
    }).concat(original.item.promptParts.context);
    return { directory: DIRECTORY, item: { ...original.item, text: 'edited draft', content: 'edited draft', context: [],
      promptParts: { ...original.item.promptParts, context: contexts.map((entry, index) => ({ ...entry, id: transferred.payloadIndex.contextIds[index] })) },
    } };
  };
  const takeOriginal = async (runtime, prefix) => {
    const pointer = await uploadCapture(runtime, `${prefix}-upload`, capture());
    const accepted = await runtime.enqueueRequest(SESSION, `${prefix}-enqueue`, { directory: DIRECTORY, item: pointer });
    const transfer = await runtime.takeRequest(SESSION, `${prefix}-take`, {
      directory: DIRECTORY, itemIds: [accepted.receipt.itemId], format: 'payload-v1',
    });
    return { accepted, transfer, edited: await editCapture(runtime, transfer.items[0]) };
  };

  it('retains acknowledged Take context IDs in a composer prefix across restart', async () => {
    vi.useFakeTimers();
    const first = createRuntime({ isDeliveryHeld: () => true });
    const pointer = await uploadCapture(first.runtime, 'context-original-upload', capture());
    const accepted = await first.runtime.enqueueRequest(SESSION, 'context-original-enqueue', { directory: DIRECTORY, item: pointer });
    const transfer = await first.runtime.takeRequest(SESSION, 'context-original-take', {
      directory: DIRECTORY, itemIds: [accepted.receipt.itemId], format: 'payload-v1',
    });
    expect(transfer.items[0].payloadIndex.contextIds).toEqual(accepted.receipt.contextIds);
    const edited = await editCapture(first.runtime, transfer.items[0]);
    await first.runtime.acknowledgeTake(SESSION, 'context-original-take', DIRECTORY);
    first.runtime.stop();
    await first.runtime.flush();

    const restored = createRuntime({ dataDir: first.dataDir, isDeliveryHeld: () => true });
    const successor = await restored.runtime.enqueue(SESSION, DIRECTORY, item({ text: 'queued successor' }));
    const composer = await uploadCapture(restored.runtime, 'context-composer-upload', edited);
    const request = { format: 'payload-v1', directory: DIRECTORY, id: 'msg_transferred_composer_prefix',
      itemIds: [successor.itemId], composer };
    const preview = await restored.runtime.previewPrefix(SESSION, request);
    expect(preview.contextIds).toEqual(accepted.receipt.contextIds);
    const reserved = await restored.runtime.reservePrefix(SESSION, request);
    expect(reserved.contextIds).toEqual(accepted.receipt.contextIds);
    expect(reserved.reservation.composer.payloadIndex.contextIds).toEqual(accepted.receipt.contextIds);
    restored.runtime.stop();
    await restored.runtime.flush();

    const recovered = createRuntime({ dataDir: first.dataDir, isDeliveryHeld: () => true });
    expect((await recovered.runtime.reservePrefix(SESSION, request)).contextIds).toEqual(accepted.receipt.contextIds);
    recovered.runtime.stop();
    await recovered.runtime.flush();
  });

  it('re-enqueues an acknowledged edit with its original IDs and prevents competing ownership after restart', async () => {
    vi.useFakeTimers();
    const first = createRuntime({ isDeliveryHeld: () => true });
    const pointer = await uploadCapture(first.runtime, 'reenqueue-original-upload', capture());
    const original = await first.runtime.enqueueRequest(SESSION, 'reenqueue-original-request', { directory: DIRECTORY, item: pointer });
    const transfer = await first.runtime.takeRequest(SESSION, 'reenqueue-original-take', {
      directory: DIRECTORY, itemIds: [original.receipt.itemId], format: 'payload-v1',
    });
    const edited = await editCapture(first.runtime, transfer.items[0]);
    await first.runtime.acknowledgeTake(SESSION, 'reenqueue-original-take', DIRECTORY);
    const editedPointer = await uploadCapture(first.runtime, 'reenqueue-edited-upload', edited);
    const input = { directory: DIRECTORY, item: editedPointer };
    const accepted = await first.runtime.enqueueRequest(SESSION, 'reenqueue-edited-request', input);
    expect(accepted.receipt.contextIds).toEqual(original.receipt.contextIds);
    expect(accepted.receipt.messageId).not.toBe(original.receipt.messageId);
    await expect(first.runtime.enqueueRequest(SESSION, 'reenqueue-competing-request', input)).rejects.toMatchObject({ code: 'queue_request_conflict' });
    first.runtime.stop();
    await first.runtime.flush();

    const restored = createRuntime({ dataDir: first.dataDir, isDeliveryHeld: () => true });
    expect((await restored.runtime.enqueueRequest(SESSION, 'reenqueue-edited-request', input)).receipt).toEqual(accepted.receipt);
    await expect(restored.runtime.enqueueRequest(SESSION, 'reenqueue-competing-request', input)).rejects.toMatchObject({ code: 'queue_request_conflict' });
    // The selected queue already owns these IDs. A composer cannot alias them.
    await expect(restored.runtime.previewPrefix(SESSION, { format: 'payload-v1', directory: DIRECTORY,
      id: 'msg_duplicate_selected_context', itemIds: [accepted.receipt.itemId], composer: editedPointer })).rejects.toBeDefined();
    restored.runtime.stop();
    await restored.runtime.flush();
  });

  it('refuses active and unacknowledged context custody without changing queue ownership', async () => {
    vi.useFakeTimers();
    const { runtime } = createRuntime({ isDeliveryHeld: () => true });
    const original = await runtime.enqueueRequest(SESSION, 'active-context-original', { directory: DIRECTORY, item: capture().item });
    const explicit = { directory: DIRECTORY, item: { ...capture().item, context: [],
      promptParts: { context: original.receipt.contextIds.map((id) => ({ id, text: 'captured context' })) } } };
    const composer = await uploadCapture(runtime, 'active-context-composer', explicit);
    const successor = await runtime.enqueue(SESSION, DIRECTORY, item());
    const request = { format: 'payload-v1', directory: DIRECTORY, id: 'msg_unacknowledged_context_prefix', itemIds: [successor.itemId], composer };
    await expect(runtime.enqueueRequest(SESSION, 'active-context-competing', { directory: DIRECTORY, item: composer })).rejects.toMatchObject({ code: 'queue_request_conflict' });
    // A prefix can own the selected original, but its composer cannot reuse the
    // same identities as another queued item outside that selected prefix.
    await runtime.reorder(SESSION, [successor.itemId, original.receipt.itemId]);
    await expect(runtime.previewPrefix(SESSION, request)).rejects.toMatchObject({ code: 'queue_request_conflict' });
    const transfer = await runtime.takeRequest(SESSION, 'active-context-take', {
      directory: DIRECTORY, itemIds: [original.receipt.itemId], format: 'payload-v1',
    });
    expect(transfer.state).toBe('transferring');
    await expect(runtime.previewPrefix(SESSION, request)).rejects.toMatchObject({ code: 'queue_request_conflict' });
    await expect(runtime.enqueueRequest(SESSION, 'unack-context-competing', { directory: DIRECTORY, item: composer })).rejects.toMatchObject({ code: 'queue_request_conflict' });
    expect(runtime.sessionSnapshot(SESSION).items.map((entry) => entry.id)).toEqual([successor.itemId]);
    expect((await runtime.getRequest(SESSION, 'active-context-take', DIRECTORY)).items).toEqual(transfer.items);
    runtime.stop();
    await runtime.flush();
  });

  it('rejects foreign owners and original prompt aliases after durable acknowledgement', async () => {
    vi.useFakeTimers();
    const { runtime } = createRuntime({ isDeliveryHeld: () => true });
    const { accepted, edited } = await takeOriginal(runtime, 'foreign-context');
    await runtime.acknowledgeTake(SESSION, 'foreign-context-take', DIRECTORY);
    for (const [index, sessionId, directory] of [[0, 'ses_foreign_context', DIRECTORY], [1, SESSION, '/other']]) {
      // These bytes really belong to the competing owner, so blob validation
      // cannot hide a missing context-ownership check.
      const pointer = await uploadCapture(runtime, `foreign-context-upload-${index}`, { ...edited, directory }, sessionId);
      await expect(runtime.enqueueRequest(sessionId, `foreign-context-request-${index}`, { directory, item: pointer })).rejects.toMatchObject({ code: 'queue_request_conflict' });
      const successor = await runtime.enqueue(sessionId, directory, item());
      await expect(runtime.previewPrefix(sessionId, { format: 'payload-v1', directory,
        id: `msg_foreign_context_prefix_${index}`, itemIds: [successor.itemId], composer: pointer })).rejects.toMatchObject({ code: 'queue_request_conflict' });
      await runtime.clear(sessionId);
    }
    const alias = { ...edited, item: { ...edited.item,
      promptParts: { context: [{ id: accepted.receipt.messageId, text: 'prompt cannot become context' }] } } };
    const pointer = await uploadCapture(runtime, 'prompt-alias-upload', alias);
    await expect(runtime.enqueueRequest(SESSION, 'prompt-alias-enqueue', { directory: DIRECTORY, item: pointer })).rejects.toMatchObject({ code: 'queue_request_conflict' });
    const successor = await runtime.enqueue(SESSION, DIRECTORY, item());
    await expect(runtime.previewPrefix(SESSION, { format: 'payload-v1', directory: DIRECTORY,
      id: 'msg_prompt_alias_prefix', itemIds: [successor.itemId], composer: pointer })).rejects.toMatchObject({ code: 'queue_request_conflict' });
    const valid = await uploadCapture(runtime, 'context-alias-upload', edited);
    await expect(runtime.previewPrefix(SESSION, { format: 'payload-v1', directory: DIRECTORY,
      id: accepted.receipt.contextIds[0], itemIds: [successor.itemId], composer: valid })).rejects.toBeDefined();
    runtime.stop();
    await runtime.flush();
  });

  it('keeps uncertain contexts excluded through restart and refuses transfer', async () => {
    vi.useFakeTimers();
    let held = true;
    let observed;
    const completed = new Promise((resolve) => { observed = resolve; });
    const first = createRuntime({ isDeliveryHeld: () => held, onBroadcast: (event) => {
      const session = event.properties.session;
      if (session.sendingId === null && session.items[0]?.queueDelivery?.state === 'uncertain') observed();
    } });
    const value = capture();
    const accepted = await first.runtime.enqueueRequest(SESSION, 'uncertain-context-original', value);
    held = false;
    first.runtime.release(SESSION);
    await vi.advanceTimersByTimeAsync(0);
    await completed;
    expect((await first.runtime.getRequest(SESSION, 'uncertain-context-original', DIRECTORY)).state).toBe('uncertain');
    await expect(first.runtime.takeRequest(SESSION, 'uncertain-context-take', {
      directory: DIRECTORY, itemIds: [accepted.receipt.itemId], format: 'payload-v1',
    })).rejects.toMatchObject({ status: 409 });
    first.runtime.stop();
    await first.runtime.flush();
    const restored = createRuntime({ dataDir: first.dataDir, isDeliveryHeld: () => true });
    const successor = await restored.runtime.enqueue(SESSION, DIRECTORY, item());
    await restored.runtime.reorder(SESSION, [successor.itemId, accepted.receipt.itemId]);
    const composer = await uploadCapture(restored.runtime, 'uncertain-context-upload', { directory: DIRECTORY,
      item: { ...value.item, context: [], promptParts: { context: accepted.receipt.contextIds.map((id) => ({ id, text: 'same identity' })) } } });
    await expect(restored.runtime.enqueueRequest(SESSION, 'uncertain-context-competing', { directory: DIRECTORY, item: composer })).rejects.toMatchObject({ code: 'queue_request_conflict' });
    await expect(restored.runtime.previewPrefix(SESSION, { format: 'payload-v1', directory: DIRECTORY,
      id: 'msg_uncertain_context_prefix', itemIds: [successor.itemId], composer })).rejects.toMatchObject({ code: 'queue_request_conflict' });
    restored.runtime.stop();
    await restored.runtime.flush();
  });

  it('holds context custody when acknowledgement persistence fails until exact recovery', async () => {
    vi.useFakeTimers();
    const { runtime, dataDir } = createRuntime({ isDeliveryHeld: () => true });
    const { edited } = await takeOriginal(runtime, 'pending-context');
    const composer = await uploadCapture(runtime, 'pending-context-composer', edited);
    const successor = await runtime.enqueue(SESSION, DIRECTORY, item());
    await runtime.flush();
    const file = path.join(dataDir, 'message-queue.json');
    fs.renameSync(file, `${file}.retained`);
    fs.mkdirSync(file);
    const request = { format: 'payload-v1', directory: DIRECTORY, id: 'msg_pending_context_prefix', itemIds: [successor.itemId], composer };
    await expect(runtime.acknowledgeTake(SESSION, 'pending-context-take', DIRECTORY)).rejects.toBeDefined();
    await expect(runtime.previewPrefix(SESSION, request)).rejects.toMatchObject({ status: 409 });
    await expect(runtime.enqueueRequest(SESSION, 'pending-context-competing', { directory: DIRECTORY, item: composer })).rejects.toMatchObject({ code: 'queue_request_conflict' });
    await expect(runtime.getRequest(SESSION, 'pending-context-take', DIRECTORY)).rejects.toMatchObject({ code: 'queue_request_pending' });
    fs.rmdirSync(file);
    fs.renameSync(`${file}.retained`, file);
    expect((await runtime.getRequest(SESSION, 'pending-context-take', DIRECTORY)).state).toBe('acknowledged');
    const reserved = await runtime.reservePrefix(SESSION, request);
    expect(reserved.contextIds).toEqual(edited.item.promptParts.context.map((entry) => entry.id));
    runtime.stop();
    await runtime.flush();
  });

  it('retains consumed prefix context fencing after admission and restart', async () => {
    vi.useFakeTimers();
    let admitted = null;
    let observed;
    const completed = new Promise((resolve) => { observed = resolve; });
    const openCode = createOpenCode();
    const fetch = openCode.fetchImpl;
    openCode.fetchImpl = vi.fn((url, init) => new URL(url).pathname.endsWith('/inbox')
      ? Promise.resolve(Response.json({ data: admitted ? [admitted] : [] })) : fetch(url, init));
    const first = createRuntime({ openCode, isDeliveryHeld: () => true,
      onBroadcast: (event) => { if (event.properties.session.items.length === 0 && admitted) observed(); } });
    const { edited } = await takeOriginal(first.runtime, 'consumed-context');
    await first.runtime.acknowledgeTake(SESSION, 'consumed-context-take', DIRECTORY);
    const composer = await uploadCapture(first.runtime, 'consumed-context-composer', edited);
    const successor = await first.runtime.enqueue(SESSION, DIRECTORY, item());
    const request = { format: 'payload-v1', directory: DIRECTORY, id: 'msg_consumed_context_prefix', itemIds: [successor.itemId], composer };
    await first.runtime.reservePrefix(SESSION, request);
    admitted = { id: request.id, type: 'user', sessionID: SESSION, delivery: 'queue', time: { created: 1 } };
    first.runtime.release(SESSION);
    await vi.advanceTimersByTimeAsync(0);
    await completed;
    first.runtime.stop();
    await first.runtime.flush();
    const restored = createRuntime({ dataDir: first.dataDir, isDeliveryHeld: () => true });
    await expect(restored.runtime.enqueueRequest(SESSION, 'consumed-context-reenqueue', { directory: DIRECTORY, item: composer })).rejects.toMatchObject({ code: 'queue_request_conflict' });
    const later = await restored.runtime.enqueue(SESSION, DIRECTORY, item());
    await expect(restored.runtime.reservePrefix(SESSION, { ...request, id: 'msg_competing_consumed_prefix', itemIds: [later.itemId] })).rejects.toMatchObject({ code: 'queue_request_conflict' });
    // Replaying an old acknowledgement cannot return custody to the journal.
    await restored.runtime.acknowledgeTake(SESSION, 'consumed-context-take', DIRECTORY);
    await expect(restored.runtime.enqueueRequest(SESSION, 'consumed-context-reenqueue', { directory: DIRECTORY, item: composer })).rejects.toMatchObject({ code: 'queue_request_conflict' });
    restored.runtime.stop();
    await restored.runtime.flush();
  });

  it('keeps a second acknowledged edit chain and selected prefix stable across restart', async () => {
    vi.useFakeTimers();
    const first = createRuntime({ isDeliveryHeld: () => true });
    const { accepted: original, edited } = await takeOriginal(first.runtime, 'chain-context');
    await first.runtime.acknowledgeTake(SESSION, 'chain-context-take', DIRECTORY);
    const pointer = await uploadCapture(first.runtime, 'chain-context-edited', edited);
    const second = await first.runtime.enqueueRequest(SESSION, 'chain-context-second-enqueue', { directory: DIRECTORY, item: pointer });
    const transfer = await first.runtime.takeRequest(SESSION, 'chain-context-second-take', {
      directory: DIRECTORY, itemIds: [second.receipt.itemId], format: 'payload-v1',
    });
    expect(transfer.items[0].payloadIndex.contextIds).toEqual(original.receipt.contextIds);
    await first.runtime.acknowledgeTake(SESSION, 'chain-context-second-take', DIRECTORY);
    const third = await first.runtime.enqueueRequest(SESSION, 'chain-context-third-enqueue', { directory: DIRECTORY, item: pointer });
    const emptyComposer = await uploadCapture(first.runtime, 'chain-context-empty-composer', { directory: DIRECTORY,
      item: { ...edited.item, content: '', text: '', context: [], promptParts: { context: [] } } });
    const request = { format: 'payload-v1', directory: DIRECTORY, id: 'msg_chain_context_prefix',
      itemIds: [third.receipt.itemId], composer: emptyComposer };
    expect((await first.runtime.reservePrefix(SESSION, request)).contextIds).toEqual(original.receipt.contextIds);
    first.runtime.stop();
    await first.runtime.flush();
    const restored = createRuntime({ dataDir: first.dataDir, isDeliveryHeld: () => true });
    expect((await restored.runtime.reservePrefix(SESSION, request)).contextIds).toEqual(original.receipt.contextIds);
    // Replaying older request receipts and acknowledgements never rewinds custody.
    expect((await restored.runtime.getRequest(SESSION, 'chain-context-enqueue', DIRECTORY)).receipt).toEqual(original.receipt);
    await restored.runtime.acknowledgeTake(SESSION, 'chain-context-take', DIRECTORY);
    await expect(restored.runtime.enqueueRequest(SESSION, 'chain-context-competing', { directory: DIRECTORY, item: pointer })).rejects.toMatchObject({ code: 'queue_request_conflict' });
    restored.runtime.stop();
    await restored.runtime.flush();
  });

  it.each(['enqueue', 'prefix'])('keeps a failed %s custody checkpoint excluded until the exact operation recovers', async (operation) => {
    vi.useFakeTimers();
    const { runtime, dataDir, openCode } = createRuntime({ isDeliveryHeld: () => true });
    const { accepted, edited } = await takeOriginal(runtime, `failed-${operation}-context`);
    await runtime.acknowledgeTake(SESSION, `failed-${operation}-context-take`, DIRECTORY);
    const composer = await uploadCapture(runtime, `failed-${operation}-composer`, edited);
    // Pre-capture the successor so reservation reaches its custody checkpoint.
    // Converting a legacy item would write its new blob before acquiring any
    // context ownership and fail earlier on the deliberately blocked rename.
    const successorPointer = await uploadCapture(runtime, `failed-${operation}-successor-upload`, { directory: DIRECTORY,
      item: { ...capture().item, context: [], promptParts: { context: [] } } });
    const successor = await runtime.enqueueRequest(SESSION, `failed-${operation}-successor-enqueue`, { directory: DIRECTORY, item: successorPointer });
    const request = { format: 'payload-v1', directory: DIRECTORY, id: `msg_failed_${operation}_prefix`, itemIds: [successor.receipt.itemId], composer };
    const acquire = () => operation === 'enqueue'
      ? runtime.enqueueRequest(SESSION, 'failed-acquisition-enqueue', { directory: DIRECTORY, item: composer })
      : runtime.reservePrefix(SESSION, request);
    await runtime.flush();
    const file = path.join(dataDir, 'message-queue.json');
    const retained = fs.readFileSync(file, 'utf8');
    fs.renameSync(file, `${file}.retained`);
    fs.mkdirSync(file);
    await expect(acquire()).rejects.toBeDefined();
    await expect(runtime.enqueueRequest(SESSION, 'failed-acquisition-competing', { directory: DIRECTORY, item: composer })).rejects.toMatchObject({ code: 'queue_request_conflict' });
    await expect(runtime.getRequest(SESSION, 'failed-acquisition-competing', DIRECTORY)).rejects.toMatchObject({ code: 'queue_request_unknown' });
    await expect(runtime.previewPrefix(SESSION, { ...request, id: 'msg_failed_competing_prefix' })).rejects.toMatchObject({ status: 409 });
    expect(runtime.sessionSnapshot(SESSION).items).toHaveLength(operation === 'enqueue' ? 2 : 1);
    if (operation === 'prefix') await expect(runtime.takeRequest(SESSION, 'failed-prefix-competing-take', {
      directory: DIRECTORY, itemIds: [successor.receipt.itemId], format: 'payload-v1',
    })).rejects.toMatchObject({ status: 409 });
    expect(fs.readFileSync(`${file}.retained`, 'utf8')).toBe(retained);
    expect(openCode.state.sent).toEqual([]);
    fs.rmdirSync(file);
    fs.renameSync(`${file}.retained`, file);
    const recovered = await acquire();
    expect(operation === 'enqueue' ? recovered.receipt.contextIds : recovered.contextIds).toEqual(accepted.receipt.contextIds);
    runtime.stop();
    await runtime.flush();
    const restored = createRuntime({ dataDir, isDeliveryHeld: () => true });
    await restored.runtime.load();
    await expect(restored.runtime.enqueueRequest(SESSION, 'failed-acquisition-competing', { directory: DIRECTORY, item: composer })).rejects.toMatchObject({ code: 'queue_request_conflict' });
    restored.runtime.stop();
    await restored.runtime.flush();
  });

  it('derives only proven old custody while keeping an old acknowledged legacy take scoped', async () => {
    vi.useFakeTimers();
    const first = createRuntime({ isDeliveryHeld: () => true });
    const { accepted, edited } = await takeOriginal(first.runtime, 'migrated-context');
    await first.runtime.acknowledgeTake(SESSION, 'migrated-context-take', DIRECTORY);
    const legacy = await first.runtime.enqueue(SESSION, DIRECTORY, item({ context: [{ kind: 'synthetic', text: 'legacy context' }] }));
    const transfer = await first.runtime.takeRequest(SESSION, 'migrated-legacy-take', {
      directory: DIRECTORY, itemIds: [legacy.itemId], format: 'payload-v1',
    });
    const legacyId = transfer.items[0].payloadIndex.contextIds[0];
    await first.runtime.acknowledgeTake(SESSION, 'migrated-legacy-take', DIRECTORY);
    first.runtime.stop();
    await first.runtime.flush();
    const file = path.join(first.dataDir, 'message-queue.json');
    const old = JSON.parse(fs.readFileSync(file, 'utf8'));
    delete old.contextOwners;
    fs.writeFileSync(file, JSON.stringify(old));
    const restored = createRuntime({ dataDir: first.dataDir, isDeliveryHeld: () => true });
    await restored.runtime.load();
    const pointer = await uploadCapture(restored.runtime, 'migrated-context-edited', edited);
    const next = await restored.runtime.enqueueRequest(SESSION, 'migrated-context-reenqueue', { directory: DIRECTORY, item: pointer });
    expect(next.receipt.contextIds).toEqual(accepted.receipt.contextIds);
    await expect(restored.runtime.enqueueRequest(SESSION, 'migrated-legacy-unproven', { directory: DIRECTORY,
      item: { ...capture().item, context: [], promptParts: { context: [{ id: legacyId, text: 'unproven old custody' }] } } })).rejects.toMatchObject({ code: 'queue_request_conflict' });
    expect((await restored.runtime.getRequest(SESSION, 'migrated-legacy-take', DIRECTORY)).state).toBe('acknowledged');
    // Unrelated fresh contexts and queue owners still work.
    await restored.runtime.enqueueRequest('ses_unrelated_migration', 'migrated-unrelated-enqueue', { directory: DIRECTORY,
      item: { ...capture().item, context: [], promptParts: { context: [{ id: 'msg_migrated_unrelated_context', text: 'unrelated' }] } } });
    restored.runtime.stop();
    await restored.runtime.flush();
  });

  it('grants no load or overwrite authority to missing or inconsistent transfer provenance', async () => {
    vi.useFakeTimers();
    const first = createRuntime({ isDeliveryHeld: () => true });
    const { edited } = await takeOriginal(first.runtime, 'malformed-context');
    await first.runtime.acknowledgeTake(SESSION, 'malformed-context-take', DIRECTORY);
    const pointer = await uploadCapture(first.runtime, 'malformed-context-edited', edited);
    await first.runtime.enqueueRequest(SESSION, 'malformed-context-reenqueue', { directory: DIRECTORY, item: pointer });
    first.runtime.stop();
    await first.runtime.flush();
    const file = path.join(first.dataDir, 'message-queue.json');
    const valid = JSON.parse(fs.readFileSync(file, 'utf8'));
    const id = edited.item.promptParts.context[0].id;
    for (const mutate of [
      (root) => { root.contextOwners = null; },
      (root) => { delete root.contextOwners; },
      (root) => { root.contextOwners[id].directory = '/foreign'; },
      (root) => { root.contextOwners[id].claims.splice(1, 1); },
      (root) => { root.contextOwners[id].claims[1].requestId = 'missing-take-evidence'; },
      (root) => { root.requests['malformed-context-take'].state = 'transferring'; root.requests['malformed-context-take'].items = []; },
    ]) {
      const broken = structuredClone(valid);
      mutate(broken);
      const bytes = JSON.stringify(broken);
      fs.writeFileSync(file, bytes);
      const restored = createRuntime({ dataDir: first.dataDir, isDeliveryHeld: () => true });
      await expect(restored.runtime.load()).rejects.toBeDefined();
      await expect(restored.runtime.enqueueRequest(SESSION, 'malformed-context-new-request', { directory: DIRECTORY, item: item() })).rejects.toBeDefined();
      expect(fs.readFileSync(file, 'utf8')).toBe(bytes);
      expect(restored.openCode.state.sent).toEqual([]);
      restored.runtime.stop();
    }
  });

  it.each(['transferring', 'acknowledged'])('rejects a forged initial legacy Take context claim while %s', async (state) => {
    vi.useFakeTimers();
    const first = createRuntime({ isDeliveryHeld: () => true });
    const queued = await first.runtime.enqueue(SESSION, DIRECTORY, item({ context: [{ kind: 'synthetic', text: 'one actual context' }] }));
    const transfer = await first.runtime.takeRequest(SESSION, 'reverse-membership-take', {
      directory: DIRECTORY, itemIds: [queued.itemId], format: 'payload-v1',
    });
    const actualId = transfer.items[0].payloadIndex.contextIds[0];
    expect(actualId.endsWith('_000000')).toBe(true);
    if (state === 'acknowledged') await first.runtime.acknowledgeTake(SESSION, 'reverse-membership-take', DIRECTORY);
    first.runtime.stop();
    await first.runtime.flush();
    const file = path.join(first.dataDir, 'message-queue.json');
    const stored = JSON.parse(fs.readFileSync(file, 'utf8'));
    // The pending forgery even has the right deterministic namespace, but its
    // ordinal is absent from the real retained one-context payload index.
    const forgedId = state === 'transferring' ? actualId.replace(/000000$/, '000001') : 'msg_foreign_initial_take_claim';
    stored.contextOwners[forgedId] = { sessionId: SESSION, directory: DIRECTORY,
      claims: [{ kind: 'take', requestId: 'reverse-membership-take', itemId: queued.itemId }] };
    const bytes = JSON.stringify(stored);
    fs.writeFileSync(file, bytes);
    const restored = createRuntime({ dataDir: first.dataDir, isDeliveryHeld: () => true });
    await expect(restored.runtime.load()).rejects.toBeDefined();
    await expect(restored.runtime.acknowledgeTake(SESSION, 'reverse-membership-take', DIRECTORY)).rejects.toBeDefined();
    expect(fs.readFileSync(file, 'utf8')).toBe(bytes);
    expect(restored.openCode.state.sent).toEqual([]);
    restored.runtime.stop();
  });

  it('loads a genuine multi-edit history within a linear membership-read budget', async () => {
    vi.useFakeTimers();
    const first = createRuntime({ isDeliveryHeld: () => true });
    const contextCount = 256;
    const cycles = 4;
    const value = { directory: DIRECTORY, item: { ...capture().item, context: [], promptParts: {
      context: Array.from({ length: contextCount }, (_, index) => ({ id: `msg_loader_budget_ctx_${index}`, text: 'context' })),
    } } };
    const pointer = await uploadCapture(first.runtime, 'loader-budget-upload', value);
    for (let cycle = 0; cycle < cycles; cycle += 1) {
      const accepted = await first.runtime.enqueueRequest(SESSION, `loader-budget-enqueue-${cycle}`, { directory: DIRECTORY, item: pointer });
      await first.runtime.takeRequest(SESSION, `loader-budget-take-${cycle}`, {
        directory: DIRECTORY, itemIds: [accepted.receipt.itemId], format: 'payload-v1',
      });
      await first.runtime.acknowledgeTake(SESSION, `loader-budget-take-${cycle}`, DIRECTORY);
    }
    first.runtime.stop();
    await first.runtime.flush();
    const restored = createRuntime({ dataDir: first.dataDir, isDeliveryHeld: () => true });
    const originalIncludes = Array.prototype.includes;
    const originalFilter = Array.prototype.filter;
    const reads = { receipt: 0, claims: 0 };
    // Array elements and length are data properties, so plain access forwards them.
    const counted = (array, kind) => new Proxy(array, { get(target, key) {
      if (/^\d+$/.test(String(key))) reads[kind] += 1;
      return target[key];
    } });
    // Count the indexed reads performed by the real native array methods in
    // the loader. This measures operations, not elapsed startup time.
    Array.prototype.includes = function (...args) {
      const tracked = typeof this[0] === 'string' && this[0].startsWith('msg_loader_budget_ctx_');
      return originalIncludes.apply(tracked ? counted(this, 'receipt') : this, args);
    };
    Array.prototype.filter = function (...args) {
      const tracked = this[0]?.kind === 'enqueue' && this[0].requestId?.startsWith('loader-budget-');
      return originalFilter.apply(tracked ? counted(this, 'claims') : this, args);
    };
    try {
      ['msg_loader_budget_ctx_control'].includes('msg_loader_budget_ctx_control');
      [{ kind: 'enqueue', requestId: 'loader-budget-control' }].filter(() => true);
      expect(reads).toEqual({ receipt: 1, claims: 1 });
      reads.receipt = 0;
      reads.claims = 0;
      await restored.runtime.load();
    } finally {
      Array.prototype.includes = originalIncludes;
      Array.prototype.filter = originalFilter;
      restored.runtime.stop();
    }
    expect(reads.receipt + reads.claims).toBeLessThanOrEqual(contextCount * cycles * 8);
    expect((await restored.runtime.getRequest(SESSION, 'loader-budget-take-3', DIRECTORY)).state).toBe('acknowledged');
  });

  it.each(['manifest-v1', 'payload-v1'])('fences an admitted zero-context %s prefix prompt through restart', async (format) => {
    vi.useFakeTimers();
    let admitted = null;
    let observed;
    const completed = new Promise((resolve) => { observed = resolve; });
    const openCode = createOpenCode();
    const fetch = openCode.fetchImpl;
    openCode.fetchImpl = vi.fn((url, init) => new URL(url).pathname.endsWith('/inbox')
      ? Promise.resolve(Response.json({ data: admitted ? [admitted] : [] })) : fetch(url, init));
    const first = createRuntime({ openCode, isDeliveryHeld: () => true,
      onBroadcast: (event) => { if (event.properties.session.items.length === 0 && admitted) observed(); } });
    const queued = await first.runtime.enqueue(SESSION, DIRECTORY, item());
    const composer = format === 'payload-v1' ? await uploadCapture(first.runtime, 'zero-context-composer-upload', { directory: DIRECTORY,
      item: { ...capture().item, context: [], promptParts: { context: [] } } }) : 'typed text';
    const request = { format, directory: DIRECTORY, id: 'msg_zero_context_consumed_prefix', itemIds: [queued.itemId], composer };
    if (format === 'manifest-v1') request.sendConfig = item().sendConfig;
    expect((await first.runtime.reservePrefix(SESSION, request)).contextIds).toEqual([]);
    admitted = { id: request.id, type: 'user', sessionID: SESSION, delivery: 'queue', time: { created: 1 } };
    first.runtime.release(SESSION);
    await vi.advanceTimersByTimeAsync(0);
    await completed;
    first.runtime.stop();
    await first.runtime.flush();
    const restored = createRuntime({ dataDir: first.dataDir, isDeliveryHeld: () => true });
    const alias = { directory: DIRECTORY, item: { ...capture().item, context: [],
      promptParts: { context: [{ id: request.id, text: 'prompt cannot become context' }] } } };
    await expect(restored.runtime.enqueueRequest(SESSION, 'zero-context-prompt-alias', alias)).rejects.toMatchObject({ code: 'queue_request_conflict' });
    await expect(restored.runtime.enqueueRequest('ses_zero_context_foreign', 'zero-context-foreign-alias', alias)).rejects.toMatchObject({ code: 'queue_request_conflict' });
    const later = await restored.runtime.enqueue(SESSION, DIRECTORY, item());
    await expect(restored.runtime.reservePrefix(SESSION, { ...request, itemIds: [later.itemId] })).rejects.toMatchObject({ code: 'queue_request_conflict' });
    await expect(restored.runtime.reservePrefix(SESSION, { id: request.id, itemIds: [later.itemId],
      composer: 'legacy alias', sendConfig: item().sendConfig })).rejects.toMatchObject({ code: 'queue_request_conflict' });
    await expect(restored.runtime.getRequest(SESSION, 'zero-context-prompt-alias', DIRECTORY)).rejects.toMatchObject({ code: 'queue_request_unknown' });
    expect(restored.openCode.state.sent).toEqual([]);
    restored.runtime.stop();
    await restored.runtime.flush();
  });

  it('retains a genuine acknowledged initial legacy Take namespace through re-enqueue and restart', async () => {
    vi.useFakeTimers();
    const first = createRuntime({ isDeliveryHeld: () => true });
    const queued = await first.runtime.enqueue(SESSION, DIRECTORY, item({ context: [{ kind: 'synthetic', text: 'original legacy context' }] }));
    const transfer = await first.runtime.takeRequest(SESSION, 'legacy-namespace-take', {
      directory: DIRECTORY, itemIds: [queued.itemId], format: 'payload-v1',
    });
    const contextId = transfer.items[0].payloadIndex.contextIds[0];
    await first.runtime.acknowledgeTake(SESSION, 'legacy-namespace-take', DIRECTORY);
    first.runtime.stop();
    await first.runtime.flush();
    const restored = createRuntime({ dataDir: first.dataDir, isDeliveryHeld: () => true });
    const input = { directory: DIRECTORY, item: { ...capture().item, context: [],
      promptParts: { context: [{ id: contextId, text: 'original legacy context' }] } } };
    expect((await restored.runtime.enqueueRequest(SESSION, 'legacy-namespace-reenqueue', input)).receipt.contextIds).toEqual([contextId]);
    restored.runtime.stop();
    await restored.runtime.flush();
    const recovered = createRuntime({ dataDir: first.dataDir, isDeliveryHeld: () => true });
    expect((await recovered.runtime.getRequest(SESSION, 'legacy-namespace-reenqueue', DIRECTORY)).receipt.contextIds).toEqual([contextId]);
    recovered.runtime.stop();
    await recovered.runtime.flush();
  });

  it('keeps zero-context prompt fences through failed reserve, exact recovery, migration and session deletion', async () => {
    vi.useFakeTimers();
    const first = createRuntime({ isDeliveryHeld: () => true });
    const pointer = await uploadCapture(first.runtime, 'zero-fence-capture-upload', { directory: DIRECTORY,
      item: { ...capture().item, context: [], promptParts: { context: [] } } });
    const queued = await first.runtime.enqueueRequest(SESSION, 'zero-fence-captured-enqueue', { directory: DIRECTORY, item: pointer });
    const request = { format: 'payload-v1', directory: DIRECTORY, id: 'msg_zero_context_checkpoint', itemIds: [queued.receipt.itemId], composer: pointer };
    await first.runtime.flush();
    const file = path.join(first.dataDir, 'message-queue.json');
    const retained = fs.readFileSync(file, 'utf8');
    fs.renameSync(file, `${file}.retained`);
    fs.mkdirSync(file);
    await expect(first.runtime.reservePrefix(SESSION, request)).rejects.toBeDefined();
    const alias = { directory: DIRECTORY, item: { ...capture().item, context: [],
      promptParts: { context: [{ id: request.id, text: 'cannot acquire a pending prompt identity' }] } } };
    await expect(first.runtime.enqueueRequest(SESSION, 'zero-fence-pending-alias', alias)).rejects.toMatchObject({ code: 'queue_request_conflict' });
    await expect(first.runtime.takeRequest(SESSION, 'zero-fence-pending-take', {
      directory: DIRECTORY, itemIds: [queued.receipt.itemId], format: 'payload-v1',
    })).rejects.toMatchObject({ status: 409 });
    expect(fs.readFileSync(`${file}.retained`, 'utf8')).toBe(retained);
    fs.rmdirSync(file);
    fs.renameSync(`${file}.retained`, file);
    expect((await first.runtime.reservePrefix(SESSION, request)).contextIds).toEqual([]);
    first.runtime.stop();
    await first.runtime.flush();
    const valid = JSON.parse(fs.readFileSync(file, 'utf8'));
    expect(valid.prefixPromptOwners[request.id]).toEqual({ sessionId: SESSION, directory: DIRECTORY, state: 'reserved' });
    for (const mutate of [
      (root) => { root.prefixPromptOwners = null; },
      (root) => { root.prefixPromptOwners[request.id].directory = '/foreign'; },
      (root) => { root.prefixPromptOwners[request.id].state = 'consumed'; },
      (root) => { delete root.prefixPromptOwners[request.id]; },
    ]) {
      const broken = structuredClone(valid);
      mutate(broken);
      const bytes = JSON.stringify(broken);
      fs.writeFileSync(file, bytes);
      const runtime = createRuntime({ dataDir: first.dataDir, isDeliveryHeld: () => true }).runtime;
      await expect(runtime.load()).rejects.toBeDefined();
      expect(fs.readFileSync(file, 'utf8')).toBe(bytes);
      runtime.stop();
    }
    // An absent old field derives this still-active reservation's exact owner.
    // No completed zero-context history can be invented from an empty queue.
    delete valid.prefixPromptOwners;
    fs.writeFileSync(file, JSON.stringify(valid));
    const migrated = createRuntime({ dataDir: first.dataDir, isDeliveryHeld: () => true });
    expect((await migrated.runtime.reservePrefix(SESSION, request)).contextIds).toEqual([]);
    migrated.runtime.processPayload({ type: 'session.deleted', properties: { sessionID: SESSION } });
    migrated.runtime.stop();
    await migrated.runtime.flush();
    expect(JSON.parse(fs.readFileSync(file, 'utf8')).prefixPromptOwners[request.id].state).toBe('consumed');
    const recovered = createRuntime({ dataDir: first.dataDir, isDeliveryHeld: () => true });
    await expect(recovered.runtime.enqueueRequest(SESSION, 'zero-fence-deleted-alias', alias)).rejects.toMatchObject({ code: 'queue_request_conflict' });
    expect(recovered.openCode.state.sent).toEqual([]);
    recovered.runtime.stop();
    await recovered.runtime.flush();
  });
});

describe('recoverable prefix reservation', () => {
  const request = (ids, id = 'msg_reserved_prefix') => ({
    id, itemIds: ids, composer: 'typed', sendConfig: item().sendConfig, delivery: 'steer',
  });

  it('holds the whole owner across probes and retains the exact prefix through a lost reply and restart', async () => {
    const fixture = createRuntime();
    const { runtime, openCode, dataDir } = fixture;
    const a = await runtime.enqueue(SESSION, DIRECTORY, item({ content: 'A' }));
    const b = await runtime.enqueue(SESSION, DIRECTORY, item({ content: 'B' }));
    const goal = await runtime.enqueue(SESSION, DIRECTORY, item({ goal: { tokenBudget: null } }));
    const c = await runtime.enqueue(SESSION, DIRECTORY, item({ content: 'C' }));
    const captured = request([a.itemId, b.itemId]);
    const receipt = await runtime.reservePrefix(SESSION, captured);
    await settle();
    expect(openCode.state.sent).toEqual([]);
    expect(receipt.items.map((entry) => entry.id)).toEqual(captured.itemIds);
    expect(runtime.sessionSnapshot(SESSION).items.map((entry) => entry.id)).toEqual([a.itemId, b.itemId, goal.itemId, c.itemId]);
    await expect(runtime.takeAll(SESSION)).rejects.toMatchObject({ status: 409 });
    await expect(runtime.take(SESSION, c.itemId)).rejects.toMatchObject({ status: 409 });
    await expect(runtime.remove(SESSION, a.itemId)).rejects.toMatchObject({ status: 409 });
    await runtime.flush();
    runtime.stop();
    const restored = createRuntime({ dataDir, openCode });
    await restored.runtime.load();
    // Recovery uses the same identity that was journaled before reserving.
    await expect(restored.runtime.reservePrefix(SESSION, request(captured.itemIds, 'msg_new_attempt'))).rejects.toMatchObject({ status: 409 });
    const recovered = await restored.runtime.reservePrefix(SESSION, captured);
    expect(recovered.reservation).toEqual(receipt.reservation);
    expect(recovered.items).toEqual(receipt.items);
    restored.runtime.start();
    await settle();
    expect(openCode.state.sent).toEqual([]);
    restored.runtime.stop();
  });

  it('rejects a stale or ineligible prefix without consuming or skipping anything', async () => {
    const { runtime } = createRuntime();
    const a = await runtime.enqueue(SESSION, DIRECTORY, item());
    const goal = await runtime.enqueue(SESSION, DIRECTORY, item({ goal: { tokenBudget: null } }));
    const b = await runtime.enqueue(SESSION, DIRECTORY, item());
    await expect(runtime.reservePrefix(SESSION, request([a.itemId, b.itemId]))).rejects.toMatchObject({ status: 409 });
    await expect(runtime.reservePrefix(SESSION, request([a.itemId, goal.itemId, b.itemId]))).rejects.toMatchObject({ status: 409 });
    await runtime.remove(SESSION, a.itemId);
    await expect(runtime.reservePrefix(SESSION, request([a.itemId]))).rejects.toMatchObject({ status: 409 });
    expect(runtime.sessionSnapshot(SESSION).items.map((entry) => entry.id)).toEqual([goal.itemId, b.itemId]);
    runtime.stop();
  });

  it('marks a stale manifest prefix without a reservation as a definite refusal', async () => {
    const { runtime } = createRuntime({ isDeliveryHeld: () => true });
    const manifest = (ids) => ({ ...request(ids), format: 'manifest-v1', directory: DIRECTORY });
    const a = await runtime.enqueue(SESSION, DIRECTORY, item());
    const goal = await runtime.enqueue(SESSION, DIRECTORY, item({ goal: { tokenBudget: null } }));
    const b = await runtime.enqueue(SESSION, DIRECTORY, item());
    const stale = { status: 409, code: 'queue_prefix_stale' };
    await expect(runtime.reservePrefix(SESSION, manifest([a.itemId, b.itemId]))).rejects.toMatchObject(stale);
    await expect(runtime.reservePrefix(SESSION, manifest([a.itemId, goal.itemId]))).rejects.toMatchObject(stale);
    await expect(runtime.reservePrefix('ses_queue_without_items', manifest([a.itemId]))).rejects.toMatchObject(stale);

    const routes = new Map();
    registerMessageQueueRoutes(Object.fromEntries(['get', 'post', 'put', 'delete']
      .map((method) => [method, (route, handler) => routes.set(`${method} ${route}`, handler)])), runtime);
    const response = { statusCode: 200, setHeader() {}, status(code) { this.statusCode = code; return this; }, json(data) { this.data = data; } };
    await routes.get('post /api/message-queue/sessions/:sessionId/reserve-prefix')(
      { params: { sessionId: SESSION }, body: manifest([a.itemId, b.itemId]), query: {} }, response);
    expect(response).toMatchObject({ statusCode: 409, data: { code: 'queue_prefix_stale' } });
    expect(runtime.sessionSnapshot(SESSION).items.map((entry) => entry.id)).toEqual([a.itemId, goal.itemId, b.itemId]);
    runtime.stop();
    await runtime.flush();
  });

  it('keeps the plain 409 for a prefix blocked by an in-flight send', async () => {
    const openCode = createOpenCode();
    const originalFetch = openCode.fetchImpl;
    let promptStarted;
    let releasePrompt;
    const started = new Promise((resolve) => { promptStarted = resolve; });
    const held = new Promise((resolve) => { releasePrompt = resolve; });
    openCode.fetchImpl = vi.fn(async (url, init) => {
      if (new URL(url).pathname.endsWith('/prompt')) {
        promptStarted();
        await held;
      }
      return originalFetch(url, init);
    });
    const { runtime } = createRuntime({ openCode });
    const a = await runtime.enqueue(SESSION, DIRECTORY, item({ text: 'A' }));
    runtime.start();
    await started;
    const refused = await runtime.reservePrefix(SESSION, { ...request([a.itemId]), format: 'manifest-v1', directory: DIRECTORY })
      .then(() => null, (error) => error);
    expect(refused).toMatchObject({ status: 409, message: 'queue prefix changed or cannot be captured' });
    expect(refused.code).toBeUndefined();
    releasePrompt();
    await settle();
    runtime.stop();
    await runtime.flush();
  });

  it('rechecks exclusion after an already-running idle probe and releases only the exact admitted ID', async () => {
    vi.useFakeTimers();
    const openCode = createOpenCode();
    const originalFetch = openCode.fetchImpl;
    let releaseProbe;
    let probeStarted;
    const started = new Promise((resolve) => { probeStarted = resolve; });
    const held = new Promise((resolve) => { releaseProbe = resolve; });
    let first = true;
    let admitted = null;
    let failObservation = false;
    openCode.fetchImpl = vi.fn(async (url, init) => {
      const pathname = new URL(url).pathname;
      if (pathname === '/api/session/active' && first) {
        first = false;
        probeStarted();
        await held;
      }
      if (pathname.endsWith('/inbox')) {
        if (failObservation) return new Response('failed', { status: 500 });
        return Response.json({ data: admitted ? [admitted] : [] });
      }
      return originalFetch(url, init);
    });
    const { runtime } = createRuntime({ openCode });
    const a = await runtime.enqueue(SESSION, DIRECTORY, item({ text: 'A' }));
    const b = await runtime.enqueue(SESSION, DIRECTORY, item({ text: 'B' }));
    await vi.advanceTimersByTimeAsync(0);
    await started;
    const receipt = await runtime.reservePrefix(SESSION, request([a.itemId]));
    releaseProbe();
    await vi.advanceTimersByTimeAsync(0);
    expect(openCode.state.sent).toEqual([]);
    for (const candidate of [null, { id: receipt.reservation.id, type: 'synthetic', sessionID: SESSION, time: { created: 1 } }]) {
      admitted = candidate;
      runtime.release(SESSION);
      await vi.advanceTimersByTimeAsync(0);
      expect(runtime.sessionSnapshot(SESSION).items).toHaveLength(2);
    }
    admitted = { id: receipt.reservation.id, type: 'user', sessionID: SESSION, time: { created: 1 }, delivery: 'queue', payload: {} };
    failObservation = true;
    runtime.release(SESSION);
    await vi.advanceTimersByTimeAsync(0);
    expect(runtime.sessionSnapshot(SESSION).items).toHaveLength(2);
    failObservation = false;
    openCode.state.active[SESSION] = { type: 'running' };
    runtime.release(SESSION);
    await vi.advanceTimersByTimeAsync(0);
    expect(runtime.sessionSnapshot(SESSION).items.map((entry) => entry.id)).toEqual([b.itemId]);
    expect(openCode.state.sent).toEqual([]);
    runtime.stop();
    await runtime.flush();
  });

  it('never acknowledges a reservation whose persistence failed', async () => {
    const { runtime, dataDir, openCode } = createRuntime();
    openCode.state.active[SESSION] = { type: 'running' };
    const a = await runtime.enqueue(SESSION, DIRECTORY, item());
    await runtime.flush();
    // Make the atomic rename fail without changing the retained queue file.
    const target = path.join(dataDir, 'message-queue.json');
    fs.renameSync(target, `${target}.retained`);
    fs.mkdirSync(target);
    await expect(runtime.reservePrefix(SESSION, request([a.itemId]))).rejects.toBeDefined();
    await expect(runtime.take(SESSION, a.itemId)).rejects.toMatchObject({ status: 409 });
    fs.rmdirSync(target);
    fs.renameSync(`${target}.retained`, target);
    const recovered = await runtime.reservePrefix(SESSION, request([a.itemId]));
    expect(recovered.reservation.id).toBe('msg_reserved_prefix');
    expect(JSON.parse(fs.readFileSync(target, 'utf8')).sessions[SESSION].reservation.id).toBe('msg_reserved_prefix');
    runtime.stop();
    await runtime.flush();
  });

  it('refuses scheduled and oversized inline items without reserving or taking later items', async () => {
    for (const blocked of [
      item({ scheduledTask: { projectId: 'p', taskId: 't' } }),
      item({ attachments: [{ id: 'file', filename: 'large.txt', mimeType: 'text/plain', source: 'local', size: 1, dataUrl: `data:text/plain,${'a'.repeat(70000)}` }] }),
    ]) {
      const { runtime } = createRuntime();
      const a = await runtime.enqueue(SESSION, DIRECTORY, item());
      const middle = await runtime.enqueue(SESSION, DIRECTORY, blocked);
      const c = await runtime.enqueue(SESSION, DIRECTORY, item());
      await expect(runtime.reservePrefix(SESSION, request([a.itemId, middle.itemId, c.itemId]))).rejects.toMatchObject({ status: 409 });
      expect(runtime.sessionSnapshot(SESSION).items).toHaveLength(3);
      const receipt = await runtime.reservePrefix(SESSION, request([a.itemId]));
      expect(receipt.items.map((entry) => entry.id)).toEqual([a.itemId]);
      runtime.stop();
      await runtime.flush();
    }
  });

  it('previews without reserving, and refuses invalid or oversized complete payloads', async () => {
    const { runtime, openCode } = createRuntime();
    openCode.state.active[SESSION] = { type: 'running' };
    const a = await runtime.enqueue(SESSION, DIRECTORY, item({ text: 'A' }));
    const captured = request([a.itemId]);
    const preview = await runtime.reservePrefix(SESSION, captured, true);
    expect(preview.reservation).toEqual(captured);
    // Preview neither removes the item nor blocks the existing clients.
    await runtime.remove(SESSION, a.itemId);
    expect(runtime.sessionSnapshot(SESSION).items).toEqual([]);
    for (const context of [
      [{ kind: 'synthetic', text: '' }],
      Array.from({ length: 17 }, () => ({ kind: 'synthetic', text: 'a'.repeat(1024 * 1024) })),
    ]) {
      const queued = await runtime.enqueue(SESSION, DIRECTORY, item({ context }));
      await expect(runtime.reservePrefix(SESSION, request([queued.itemId]))).rejects.toMatchObject({ status: 409 });
      // Failed validation grants no reservation and leaves normal delivery available.
      await runtime.remove(SESSION, queued.itemId);
    }
    runtime.stop();
    await runtime.flush();
  });

  it('keeps the reservation on malformed inbox and whole-message observations', async () => {
    vi.useFakeTimers();
    const openCode = createOpenCode();
    openCode.state.active[SESSION] = { type: 'running' };
    const original = openCode.fetchImpl;
    let inbox = [];
    let history;
    openCode.fetchImpl = vi.fn(async (url, init) => {
      const pathname = new URL(url).pathname;
      if (pathname.endsWith('/inbox')) return Response.json({ data: inbox });
      if (pathname.includes('/message/')) return Response.json({ data: history });
      return original(url, init);
    });
    const { runtime } = createRuntime({ openCode });
    const queued = await runtime.enqueue(SESSION, DIRECTORY, item());
    const captured = request([queued.itemId]);
    await runtime.reservePrefix(SESSION, captured);
    const malformed = { id: captured.id, type: 'user', sessionID: SESSION, time: { created: 1 } };
    for (const [items, message] of [[[malformed], null], [[], malformed]]) {
      inbox = items;
      history = message;
      runtime.release(SESSION);
      await vi.advanceTimersByTimeAsync(100000);
      expect(runtime.sessionSnapshot(SESSION).items).toHaveLength(1);
    }
    history = { ...malformed, text: 'plugin-prepared text' };
    runtime.release(SESSION);
    await vi.advanceTimersByTimeAsync(100000);
    expect(runtime.sessionSnapshot(SESSION).items).toEqual([]);
    runtime.stop();
    await runtime.flush();
  });
});

describe('scheduled task items', () => {
  const scheduled = (projectId = 'project-1', overrides = {}) => item({
    scheduledTask: { projectId, taskId: 'task-1' }, sendConfig: {}, ...overrides,
  });

  it('deduplicates project and task across sessions, but not across projects', async () => {
    const { runtime, openCode } = createRuntime();
    openCode.state.active = { [SESSION]: { type: 'running' } };
    await runtime.enqueue(SESSION, DIRECTORY, scheduled());
    await expect(runtime.enqueue('ses_other', DIRECTORY, scheduled())).rejects.toMatchObject({ status: 409 });
    await runtime.enqueue(SESSION, DIRECTORY, scheduled('project-2'));
    expect(runtime.sessionSnapshot(SESSION).items).toHaveLength(2);
    runtime.stop();
    await runtime.flush();
  });

  it('inherits without a model POST and reports sent without waiting for completion', async () => {
    const results = [];
    const { runtime, openCode } = createRuntime({ onScheduledTaskResult: (task, status) => results.push({ task, status }) });
    await runtime.enqueue(SESSION, DIRECTORY, scheduled());
    await settle();
    expect(openCode.state.switched).toEqual([]);
    expect(openCode.state.sent).toHaveLength(1);
    expect(openCode.state.sent[0].body).toEqual({ text: 'follow up' });
    expect(runtime.sessionSnapshot(SESSION).items).toEqual([]);
    expect(results).toEqual([{ task: { projectId: 'project-1', taskId: 'task-1' }, status: 'sent' }]);
    runtime.stop();
    await runtime.flush();
  });

  it('applies explicit selection and keeps the existing directory query', async () => {
    const { runtime, openCode } = createRuntime();
    await runtime.enqueue(SESSION, DIRECTORY, scheduled('project-1', { sendConfig: { providerID: 'openai', modelID: 'chosen', agent: 'plan' } }));
    await settle();
    expect(openCode.state.switched.map((entry) => entry.body)).toEqual([{ model: { providerID: 'openai', id: 'chosen' } }, { agent: 'plan' }]);
    const promptCall = openCode.fetchImpl.mock.calls.find(([url]) => new URL(url).pathname.endsWith('/prompt'));
    expect(new URL(promptCall[0]).searchParams.get('directory')).toBe(DIRECTORY);
    runtime.stop();
    await runtime.flush();
  });

  it('supports an agent override without switching the target model', async () => {
    const { runtime, openCode } = createRuntime();
    await runtime.enqueue(SESSION, DIRECTORY, scheduled('project-1', { sendConfig: { agent: 'plan' } }));
    await settle();
    expect(openCode.state.switched.map((entry) => entry.body)).toEqual([{ agent: 'plan' }]);
    runtime.stop();
    await runtime.flush();
  });

  it('keeps an agent-only override across persistence and restart', async () => {
    const first = createRuntime();
    first.openCode.state.active = { [SESSION]: { type: 'running' } };
    await first.runtime.enqueue(SESSION, DIRECTORY, scheduled('project-1', { sendConfig: { agent: 'plan' } }));
    first.runtime.stop();
    await first.runtime.flush();
    const second = createRuntime({ dataDir: first.dataDir });
    await second.runtime.load();
    expect(second.runtime.sessionSnapshot(SESSION).items[0].sendConfig).toEqual({ agent: 'plan' });
    second.runtime.start();
    await settle();
    expect(second.openCode.state.switched.map((entry) => entry.body)).toEqual([{ agent: 'plan' }]);
    expect(second.openCode.state.sent).toHaveLength(1);
    second.runtime.stop();
    await second.runtime.flush();
  });

  it.each([404, 409])('drops a target refusal %s without holding ordinary items behind it', async (statusCode) => {
    const results = [];
    const { runtime, openCode } = createRuntime({
      beforeScheduledTaskSend: async () => { throw Object.assign(new Error('Scheduled target is unavailable'), { statusCode }); },
      onScheduledTaskResult: (_task, status) => results.push(status),
    });
    await runtime.enqueue(SESSION, DIRECTORY, scheduled());
    await runtime.enqueue(SESSION, DIRECTORY, item({ text: 'ordinary' }));
    await settle();
    expect(results).toEqual(['failed']);
    expect(openCode.state.sent[0].body.text).toBe('ordinary');
    runtime.stop();
    await runtime.flush();
  });

  it.each([
    { operation: 'prompt', status: 500 },
    { operation: 'model', status: 500 },
    { operation: 'model', status: 404 },
    { operation: 'model', status: 409 },
    { operation: 'target lookup', status: 503 },
  ])('uses the ordinary retry policy for scheduled $operation failure $status', async ({ operation, status }) => {
    const results = [];
    let checks = 0;
    const { runtime, openCode, emit } = createRuntime({
      retryDelayMs: () => 100,
      beforeScheduledTaskSend: async () => {
        checks += 1;
        if (operation === 'target lookup' && checks === 1) throw Object.assign(new Error('upstream unavailable'), { statusCode: 503 });
      },
      onScheduledTaskResult: (_task, status) => results.push(status),
    });
    runtime.start();
    openCode.state.failStatus = status;
    if (operation === 'prompt') openCode.state.failNext = /\/prompt$/;
    if (operation === 'model') openCode.state.failNext = /\/model$/;
    await runtime.enqueue(SESSION, DIRECTORY, operation === 'model'
      ? scheduled('project-1', { sendConfig: { providerID: 'openai', modelID: 'chosen' } })
      : scheduled());
    await runtime.enqueue(SESSION, DIRECTORY, item({ text: 'ordinary' }));
    await settle();
    expect(runtime.sessionSnapshot(SESSION).items).toHaveLength(2);
    expect(results).toEqual([]);
    expect(openCode.state.sent).toEqual([]);
    await expect(runtime.enqueue(SESSION, DIRECTORY, scheduled())).rejects.toMatchObject({ status: 409 });
    await settle(130);
    expect(checks).toBe(2);
    expect(results).toEqual(['sent']);
    expect(openCode.state.sent.map((entry) => entry.body.text)).toEqual(['follow up']);
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    expect(openCode.state.sent.map((entry) => entry.body.text)).toEqual(['follow up', 'ordinary']);
    runtime.stop();
    await runtime.flush();
  });

  it('rejects scheduled takes, leaves them in bulk take, and allows reorder and remove', async () => {
    const results = [];
    const { runtime, openCode } = createRuntime({ onScheduledTaskResult: (task, status) => results.push({ task, status }) });
    openCode.state.active = { [SESSION]: { type: 'running' } };
    const first = await runtime.enqueue(SESSION, DIRECTORY, scheduled());
    const ordinary = await runtime.enqueue(SESSION, DIRECTORY, item());
    const second = await runtime.enqueue(SESSION, DIRECTORY, scheduled('project-1', { scheduledTask: { projectId: 'project-1', taskId: 'task-2' } }));
    await runtime.reorder(SESSION, [second.itemId, ordinary.itemId, first.itemId]);
    await expect(runtime.take(SESSION, first.itemId)).rejects.toMatchObject({ status: 409 });
    expect((await runtime.takeAll(SESSION)).items.map((entry) => entry.id)).toEqual([ordinary.itemId]);
    expect(runtime.sessionSnapshot(SESSION).items.map((entry) => entry.id)).toEqual([second.itemId, first.itemId]);
    expect(results).toEqual([]);
    await runtime.remove(SESSION, second.itemId);
    expect(results).toEqual([{ task: { projectId: 'project-1', taskId: 'task-2' }, status: 'cancelled' }]);
    runtime.stop();
    await runtime.flush();
  });

  it('ignores creation-only policy fields on stored scheduled provenance', () => {
    const parsed = parseQueuedItemInput(scheduled('project-1', {
      scheduledTask: { projectId: 'project-1', taskId: 'task-1', goalEnabled: true, goalTokenBudget: 5000, permissionAutoAccept: true },
    }));
    expect(parsed.scheduledTask).toEqual({ projectId: 'project-1', taskId: 'task-1' });
  });

  it('cancels only the requested task and survives a restart with provenance', async () => {
    const results = [];
    const first = createRuntime({ onScheduledTaskResult: (_task, status) => results.push(status) });
    first.openCode.state.active = { [SESSION]: { type: 'running' } };
    await first.runtime.enqueue(SESSION, DIRECTORY, scheduled());
    await first.runtime.enqueue(SESSION, DIRECTORY, item());
    first.runtime.stop();
    await first.runtime.flush();
    const second = createRuntime({ dataDir: first.dataDir, onScheduledTaskResult: (_task, status) => results.push(status) });
    await second.runtime.load();
    expect(second.runtime.sessionSnapshot(SESSION).items[0].scheduledTask).toEqual({ projectId: 'project-1', taskId: 'task-1' });
    await second.runtime.cancelScheduledTask('project-1', 'task-1');
    expect(second.runtime.sessionSnapshot(SESSION).items).toHaveLength(1);
    expect(results).toEqual(['cancelled']);
    second.runtime.stop();
    await second.runtime.flush();
  });
});

describe('auto routing', () => {
  it('switches a queued prompt and a queued command onto the routed model and agent', async () => {
    const resolveAutoSelection = vi.fn(async ({ model }) => (model?.id === 'auto'
      ? { model: { providerID: 'openai', id: 'gpt-6-astra' }, agent: 'plan', decision: {} }
      : null));
    const { runtime, openCode, emit } = createRuntime({ resolveAutoSelection });
    runtime.start();
    openCode.state.statuses = { [SESSION]: { type: 'busy' } };
    openCode.state.commands = [{ name: 'review', template: 'Review $ARGUMENTS' }];
    const auto = { providerID: 'openchamber', modelID: 'auto', agent: 'build' };
    await runtime.enqueue(SESSION, DIRECTORY, item({ content: 'plain', text: 'plain', sendConfig: auto }));
    await runtime.enqueue(SESSION, DIRECTORY, item({ content: '/review src', text: '/review src', sendConfig: auto }));

    openCode.state.statuses = {};
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();

    // v2 carries no model in a prompt body: the session is switched first.
    const switched = openCode.state.switched.filter((entry) => entry.path.endsWith('/model'));
    expect(switched.map((entry) => entry.body.model)).toEqual([
      { providerID: 'openai', id: 'gpt-6-astra' },
      { providerID: 'openai', id: 'gpt-6-astra' },
    ]);
    expect(openCode.state.switched.filter((entry) => entry.path.endsWith('/agent')).map((entry) => entry.body.agent))
      .toEqual(['plan', 'plan']);
    expect(resolveAutoSelection).toHaveBeenCalledTimes(2);
    expect(resolveAutoSelection.mock.calls[0][0]).toMatchObject({ sessionId: SESSION, directory: DIRECTORY, requestText: 'plain' });
  });

  it('tells the classifier about an image attached to a queued send', async () => {
    const resolveAutoSelection = vi.fn(async () => null);
    const { runtime, openCode, emit } = createRuntime({ resolveAutoSelection });
    runtime.start();
    openCode.state.statuses = { [SESSION]: { type: 'busy' } };
    const auto = { providerID: 'openchamber', modelID: 'auto', agent: 'build' };
    const image = { id: 'a', filename: 'shot.png', mimeType: 'image/png', size: 1, source: 'local', dataUrl: 'data:image/png;base64,AAAA' };
    await runtime.enqueue(SESSION, DIRECTORY, item({ content: "what's wrong?", text: "what's wrong?", attachments: [image], sendConfig: auto }));

    openCode.state.statuses = {};
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();

    expect(resolveAutoSelection.mock.calls[0][0].requestText).toBe("what's wrong?\n\n[The user attached an image: shot.png]");
  });
});

describe('parseQueuedItemInput', () => {
  it('rejects an item the server could not deliver later', () => {
    expect(() => parseQueuedItemInput({ content: 'x' })).toThrow(TypeError);
    expect(() => parseQueuedItemInput(item({ content: '', text: '' }))).toThrow(TypeError);
    expect(() => parseQueuedItemInput(item({ attachments: [{ filename: 'a.png' }] }))).toThrow(TypeError);
  });

  it('keeps delivery fields and trims blank edges of the content', () => {
    const parsed = parseQueuedItemInput(item({ content: '\n\nhello\n', text: 'hello', agentMention: 'reviewer' }));
    expect(parsed).toEqual({
      content: 'hello',
      text: 'hello',
      agentMention: 'reviewer',
      attachments: [],
      context: [],
      sendConfig: { providerID: 'anthropic', modelID: 'claude', agent: 'build' },
    });
  });

  it('keeps captured context and rejects a malformed part', () => {
    const context = [
      { kind: 'context', text: 'Comment on `a.ts`', metadata: { openchamberContext: { kind: 'code-comment' } }, instructions: '' },
      { kind: 'instruction', text: 'use the skill' },
      { kind: 'synthetic', text: 'conflict payload' },
    ];
    expect(parseQueuedItemInput(item({ context })).context).toEqual([
      { kind: 'context', text: 'Comment on `a.ts`', metadata: { openchamberContext: { kind: 'code-comment' } } },
      { kind: 'instruction', text: 'use the skill' },
      { kind: 'synthetic', text: 'conflict payload' },
    ]);
    expect(() => parseQueuedItemInput(item({ context: [{ kind: 'context', text: 'no metadata' }] }))).toThrow(TypeError);
    expect(() => parseQueuedItemInput(item({ context: [{ kind: 'other', text: 'x' }] }))).toThrow(TypeError);
  });

  it('accepts an item that is only context', () => {
    const parsed = parseQueuedItemInput(item({ content: '', text: '', context: [{ kind: 'synthetic', text: 'just context' }] }));
    expect(parsed.text).toBe('');
    expect(parsed.context).toHaveLength(1);
  });

  it('keeps a goal captured with the message and rejects one it could not start', () => {
    expect(parseQueuedItemInput(item({ goal: { tokenBudget: null } })).goal).toEqual({ tokenBudget: null });
    expect(parseQueuedItemInput(item({ goal: { objective: ' the plan ', tokenBudget: 5000 } })).goal)
      .toEqual({ objective: 'the plan', tokenBudget: 5000 });
    expect(parseQueuedItemInput(item()).goal).toBeUndefined();
    expect(() => parseQueuedItemInput(item({ goal: { tokenBudget: -1 } }))).toThrow(TypeError);
    expect(() => parseQueuedItemInput(item({ goal: 'yes' }))).toThrow(TypeError);
    expect(() => parseQueuedItemInput(item({ content: '', text: '', context: [{ kind: 'synthetic', text: 'x' }], goal: { tokenBudget: null } }))).toThrow(TypeError);
  });
});

describe('a message queued as a goal', () => {
  const previousDataDir = process.env.OPENCHAMBER_DATA_DIR;
  afterEach(() => {
    if (previousDataDir === undefined) delete process.env.OPENCHAMBER_DATA_DIR;
    else process.env.OPENCHAMBER_DATA_DIR = previousDataDir;
  });

  it('starts the goal when that message goes out, not with the message before it', async () => {
    const goalsDataDir = makeDataDir();
    process.env.OPENCHAMBER_DATA_DIR = goalsDataDir;
    const persisted = [];
    const { runtime, openCode, emit } = createRuntime({
      persistSessionGoal: async (sessionID, directory, goal) => {
        persisted.push({ sessionID, directory, goal, sentBefore: openCode.state.sent.length });
      },
    });
    runtime.start();
    openCode.state.active = { [SESSION]: { type: 'running' } };
    await runtime.enqueue(SESSION, DIRECTORY, item({ content: 'plain first', text: 'plain first' }));
    await runtime.enqueue(SESSION, DIRECTORY, item({ content: 'build the parser', text: 'build the parser', goal: { tokenBudget: 5000 } }));
    expect(runtime.sessionSnapshot(SESSION).items.map((entry) => entry.goal)).toEqual([undefined, { tokenBudget: 5000 }]);

    openCode.state.active = {};
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    expect(openCode.state.sent.map((entry) => entry.body.text)).toEqual(['plain first']);
    expect(persisted).toEqual([]);

    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'busy' } } });
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();

    expect(persisted).toHaveLength(1);
    expect(persisted[0]).toMatchObject({
      sessionID: SESSION,
      directory: DIRECTORY,
      goal: { status: 'active', objectiveFile: true, tokenBudget: 5000 },
      sentBefore: 1,
    });
    expect(fs.readFileSync(path.join(goalsDataDir, 'goals', `${SESSION}.md`), 'utf8')).toBe('build the parser');
    // The briefing goes ahead of the message, as for a scheduled goal.
    expect(openCode.state.sent.slice(1).map((entry) => [entry.path, entry.body])).toEqual([
      [`/api/session/${SESSION}/synthetic`, { text: expect.stringContaining('Goal mode is active'), resume: false }],
      [`/api/session/${SESSION}/prompt`, { text: 'build the parser' }],
    ]);
    expect(openCode.state.sent[1].body.text).toContain('A token budget of 5000 tokens');
  });

  it('admits nothing when the goal cannot be saved, so a retry cannot duplicate the context', async () => {
    process.env.OPENCHAMBER_DATA_DIR = makeDataDir();
    const { runtime, openCode, emit } = createRuntime({
      retryDelayMs: () => 60_000,
      persistSessionGoal: async () => { throw new Error('metadata store down'); },
    });
    runtime.start();
    await runtime.enqueue(SESSION, DIRECTORY, item({ context: [{ kind: 'synthetic', text: 'attached' }], goal: { tokenBudget: null } }));
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    expect(openCode.state.sent).toEqual([]);
    expect(runtime.sessionSnapshot(SESSION).items).toHaveLength(1);
  });

  it('uses the objective the arming flow supplied', async () => {
    process.env.OPENCHAMBER_DATA_DIR = makeDataDir();
    const persisted = [];
    const { runtime, emit } = createRuntime({
      persistSessionGoal: async (_sessionID, _directory, goal) => { persisted.push(goal); },
    });
    runtime.start();
    await runtime.enqueue(SESSION, DIRECTORY, item({ content: 'Implement this plan: X', text: 'Implement this plan: X', goal: { objective: 'the whole plan', tokenBudget: null } }));
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await vi.waitFor(() => {
      expect(runtime.sessionSnapshot(SESSION).items).toEqual([]);
      expect(persisted).toHaveLength(1);
    });
    expect(fs.readFileSync(path.join(process.env.OPENCHAMBER_DATA_DIR, 'goals', `${SESSION}.md`), 'utf8')).toBe('the whole plan');
    runtime.stop();
    await runtime.flush();
  });
});

describe('message queue runtime', () => {
  it('delivers the head of the queue when the session goes idle, in order', async () => {
    const { runtime, openCode, emit, promptSent, broadcasts } = createRuntime();
    runtime.start();
    openCode.state.active = { [SESSION]: { type: 'running' } };

    await runtime.enqueue(SESSION, DIRECTORY, item({ content: 'first', text: 'first' }));
    await runtime.enqueue(SESSION, DIRECTORY, item({ content: 'second', text: 'second' }));
    await settle();
    expect(openCode.state.sent).toHaveLength(0);

    openCode.state.active = {};
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();

    expect(openCode.state.sent).toHaveLength(1);
    expect(openCode.state.sent[0].path).toBe(`/api/session/${SESSION}/prompt`);
    // v2 selects model and agent on the session, so the prompt carries text
    // and attachments only.
    expect(openCode.state.sent[0].body).toEqual({ text: 'first' });
    expect(promptSent).toEqual([SESSION]);
    expect(runtime.sessionSnapshot(SESSION).items.map((entry) => entry.content)).toEqual(['second']);
    // Clients learned about the in-flight item and then the removal.
    expect(broadcasts.at(-1)).toMatchObject({
      type: 'openchamber:message-queue.updated',
      properties: { session: { sessionId: SESSION, sendingId: null } },
    });

    // The next turn: busy, then idle again — the second message goes out.
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'busy' } } });
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    expect(openCode.state.sent).toHaveLength(2);
    expect(runtime.sessionSnapshot(SESSION).items).toEqual([]);
  });

  it('waits for a background subagent before sending the queued message', async () => {
    const { runtime, openCode, emit } = createRuntime();
    runtime.start();
    // The parent paused: it is idle while its background subagent works.
    openCode.state.children = { [SESSION]: ['ses_child'] };
    openCode.state.active = { ses_child: { type: 'running' } };

    await runtime.enqueue(SESSION, DIRECTORY, item({ content: 'next', text: 'next' }));
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    expect(openCode.state.sent).toHaveLength(0);

    // The subagent finished and OpenCode ran the parent again with its result.
    openCode.state.active = {};
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'busy' } } });
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    expect(openCode.state.sent).toHaveLength(1);
    expect(openCode.state.sent[0].body).toEqual({ text: 'next' });
  });

  it('keeps queued messages until a nested background subagent finishes', async () => {
    const { runtime, openCode, emit } = createRuntime();
    runtime.start();
    openCode.state.children = { [SESSION]: ['ses_child'], ses_child: ['ses_grandchild'] };
    openCode.state.active = { ses_grandchild: { type: 'running' } };
    await runtime.enqueue(SESSION, DIRECTORY, item({ content: 'first', text: 'first' }));
    await runtime.enqueue(SESSION, DIRECTORY, item({ content: 'second', text: 'second' }));
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    expect(openCode.state.sent).toHaveLength(0);
    expect(runtime.sessionSnapshot(SESSION).items).toHaveLength(2);

    openCode.state.active = {};
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    expect(openCode.state.sent.map((entry) => entry.body.text)).toEqual(['first']);
    expect(runtime.sessionSnapshot(SESSION).items).toHaveLength(1);
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'busy' } } });
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    expect(openCode.state.sent.map((entry) => entry.body.text)).toEqual(['first', 'second']);
    runtime.stop();
  });

  it('keeps the queue intact when a nested lineage read fails', async () => {
    const { runtime, openCode, emit } = createRuntime();
    runtime.start();
    openCode.state.children = { [SESSION]: ['ses_child'], ses_child: ['ses_grandchild'] };
    openCode.state.active = { ses_grandchild: { type: 'running' } };
    openCode.state.failNext = /\/api\/session\/ses_child$/;
    await runtime.enqueue(SESSION, DIRECTORY, item());
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    expect(openCode.state.sent).toHaveLength(0);
    expect(runtime.sessionSnapshot(SESSION).items).toHaveLength(1);
    openCode.state.active = {};
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    expect(openCode.state.sent).toHaveLength(1);
    runtime.stop();
  });

  it('does not send into a running turn even when the status event says idle', async () => {
    const { runtime, openCode, emit } = createRuntime({ now: () => 10_000 });
    runtime.start();
    // Live unfinished turn: created after this runtime started.
    openCode.state.tail = [{ type: 'assistant', time: { created: 10_001 } }];
    await runtime.enqueue(SESSION, DIRECTORY, item());
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    expect(openCode.state.sent).toHaveLength(0);

    // The reply completes: that alone drains the queue (a missed idle event
    // must not strand it).
    openCode.state.tail = [{ type: 'assistant', time: { created: 10_001, completed: 10_002 } }];
    emit({ type: 'message.updated', properties: { info: { role: 'assistant', sessionID: SESSION, time: { created: 10_001, completed: 10_002 } } } });
    await settle();
    expect(openCode.state.sent).toHaveLength(1);
  });

  it('delivers past an unfinished tail that predates this runtime (dead pre-restart run)', async () => {
    const { runtime, openCode, emit } = createRuntime({ now: () => 10_000 });
    runtime.start();
    // Assistant reply interrupted by a server restart: unfinished, but older
    // than this runtime — no completion event will ever arrive for it, so it
    // must not block a restored queue forever.
    openCode.state.tail = [{ type: 'assistant', time: { created: 1 } }];
    await runtime.enqueue(SESSION, DIRECTORY, item());
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    expect(openCode.state.sent).toHaveLength(1);
    expect(openCode.state.sent[0].path).toBe(`/api/session/${SESSION}/prompt`);
  });

  it('treats an unreachable OpenCode as unknown, not idle', async () => {
    const { runtime, openCode, emit } = createRuntime({ retryDelayMs: () => 10 });
    runtime.start();
    await runtime.enqueue(SESSION, DIRECTORY, item());
    openCode.state.failNext = /\/api\/session\/active$/;
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle(5);
    expect(openCode.state.sent).toHaveLength(0);
    // Retried after the status fetch recovers.
    await settle(40);
    expect(openCode.state.sent).toHaveLength(1);
  });

  it('keeps a failed item and retries with backoff', async () => {
    const { runtime, openCode, emit, broadcasts } = createRuntime({ retryDelayMs: () => 20 });
    runtime.start();
    await runtime.enqueue(SESSION, DIRECTORY, item());
    openCode.state.failNext = /\/prompt$/;
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle(10);
    expect(openCode.state.sent).toHaveLength(0);
    expect(runtime.sessionSnapshot(SESSION).items).toHaveLength(1);
    expect(runtime.sessionSnapshot(SESSION).sendingId).toBeNull();
    expect(broadcasts.at(-1).properties.session.sendingId).toBeNull();
    await settle(40);
    expect(openCode.state.sent).toHaveLength(1);
    expect(runtime.sessionSnapshot(SESSION).items).toHaveLength(0);
  });

  it('keeps a queued prompt when the runtime returns an HTML app shell', async () => {
    const { runtime, openCode, emit, promptSent } = createRuntime({ retryDelayMs: () => 1_000 });
    runtime.start();
    await runtime.enqueue(SESSION, DIRECTORY, item());
    openCode.state.htmlNext = /prompt$/;
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();

    expect(openCode.state.sent).toHaveLength(0);
    expect(runtime.sessionSnapshot(SESSION).items).toHaveLength(1);
    expect(promptSent).toEqual([]);
    runtime.stop();
  });

  it('keeps a queued command when the runtime returns an HTML app shell', async () => {
    const { runtime, openCode, emit, promptSent } = createRuntime({ retryDelayMs: () => 1_000 });
    runtime.start();
    openCode.state.commands = [{ name: 'review' }];
    await runtime.enqueue(SESSION, DIRECTORY, item({ content: '/review src', text: '/review src' }));
    openCode.state.htmlNext = /command$/;
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();

    expect(openCode.state.sent).toHaveLength(0);
    expect(runtime.sessionSnapshot(SESSION).items).toHaveLength(1);
    expect(promptSent).toEqual([]);
    runtime.stop();
  });

  it('holds delivery briefly after a user abort', async () => {
    const { runtime, openCode, emit } = createRuntime();
    runtime.start();
    await runtime.enqueue(SESSION, DIRECTORY, item());
    // v2 reports a user abort as `session.execution.interrupted`, which the
    // translator turns into an aborted `session.idle`.
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    emit({ type: 'session.idle', properties: { sessionID: SESSION, aborted: true, reason: 'user' } });
    await settle(10);
    expect(openCode.state.sent).toHaveLength(0);
    await settle(80);
    expect(openCode.state.sent).toHaveLength(1);
  });

  it('waits while an auto-review holds the session and delivers on release', async () => {
    let held = true;
    const { runtime, openCode, emit } = createRuntime({ isDeliveryHeld: (sessionId) => held && sessionId === SESSION });
    runtime.start();
    await runtime.enqueue(SESSION, DIRECTORY, item());
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    expect(openCode.state.sent).toHaveLength(0);

    held = false;
    runtime.release(SESSION);
    await settle();
    expect(openCode.state.sent).toHaveLength(1);
  });

  it('delivers a restored queue only once the hold owner has loaded', async () => {
    const dataDir = makeDataDir();
    const first = createRuntime({ dataDir });
    first.openCode.state.active = { [SESSION]: { type: 'running' } };
    first.runtime.start();
    await first.runtime.enqueue(SESSION, DIRECTORY, item());
    await first.runtime.flush();
    first.runtime.stop();

    let ready = () => {};
    let held = true;
    const second = createRuntime({
      dataDir,
      isDeliveryHeld: () => held,
      deliveryHoldReady: new Promise((resolve) => { ready = resolve; }),
    });
    second.runtime.start();
    await settle();
    expect(second.openCode.state.sent).toHaveLength(0);

    held = false;
    ready();
    await settle();
    expect(second.openCode.state.sent).toHaveLength(1);
    second.runtime.stop();
  });

  it('survives a restart and delivers once OpenCode reconnects', async () => {
    const dataDir = makeDataDir();
    const first = createRuntime({ dataDir });
    first.runtime.start();
    first.openCode.state.active = { [SESSION]: { type: 'running' } };
    await first.runtime.enqueue(SESSION, DIRECTORY, item({ content: 'persisted', text: 'persisted', contextPreview: 'Saved context preview' }));
    await first.runtime.flush();
    first.runtime.stop();

    const second = createRuntime({ dataDir });
    second.runtime.start();
    await second.runtime.load();
    expect(second.runtime.sessionSnapshot(SESSION).items.map((entry) => entry.content)).toEqual(['persisted']);
    expect(second.runtime.sessionSnapshot(SESSION).items[0].contextPreview).toBe('Saved context preview');
    second.connect();
    await settle();
    expect(second.openCode.state.sent).toHaveLength(1);
    expect(second.openCode.state.sent[0].body).toEqual({ text: 'persisted' });
  });

  it('moves an unreadable queue file aside instead of treating it as empty', async () => {
    const dataDir = makeDataDir();
    fs.writeFileSync(path.join(dataDir, 'message-queue.json'), '{ not json');
    const { runtime } = createRuntime({ dataDir });
    await runtime.load();
    expect(runtime.snapshot().sessions).toEqual([]);
    expect(fs.readdirSync(dataDir).some((name) => name.startsWith('message-queue.json.corrupt-'))).toBe(true);
  });

  it('refuses to remove or take the item currently being sent', async () => {
    const { runtime, openCode, emit } = createRuntime();
    runtime.start();
    let release;
    // active map, message tail, the subagent check (empty active map),
    // model switch, then the prompt itself (held open until released)
    openCode.fetchImpl.mockImplementationOnce(async () => Response.json({ location: {}, data: {} }))
      .mockImplementationOnce(async () => Response.json({ data: [], cursor: {} }))
      .mockImplementationOnce(async () => Response.json({ data: {} }))
      .mockImplementationOnce(async () => new Response(null, { status: 204 }))
      .mockImplementationOnce(() => new Promise((resolve) => { release = () => resolve(new Response(null, { status: 204 })); }));
    const { itemId } = await runtime.enqueue(SESSION, DIRECTORY, item());
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    expect(runtime.sessionSnapshot(SESSION).sendingId).toBe(itemId);

    await expect(runtime.remove(SESSION, itemId)).rejects.toMatchObject({ status: 409 });
    await expect(runtime.take(SESSION, itemId)).rejects.toMatchObject({ status: 409 });
    const taken = await runtime.takeAll(SESSION);
    expect(taken.items).toEqual([]);
    expect(runtime.sessionSnapshot(SESSION).items).toHaveLength(1);

    release();
    await settle();
    expect(runtime.sessionSnapshot(SESSION).items).toHaveLength(0);
  });

  it('take hands back the full payload and leaves the rest queued', async () => {
    const { runtime } = createRuntime();
    runtime.start();
    const attachment = { id: 'a1', filename: 'shot.png', mimeType: 'image/png', size: 3, source: 'local', dataUrl: 'data:image/png;base64,AAA=' };
    const first = await runtime.enqueue(SESSION, DIRECTORY, item({ content: 'with image', attachments: [attachment] }));
    await runtime.enqueue(SESSION, DIRECTORY, item({ content: 'plain' }));

    expect(runtime.sessionSnapshot(SESSION).items[0].attachments[0]).not.toHaveProperty('dataUrl');
    const taken = await runtime.take(SESSION, first.itemId);
    expect(taken.item.attachments[0].dataUrl).toBe(attachment.dataUrl);
    expect(runtime.sessionSnapshot(SESSION).items.map((entry) => entry.content)).toEqual(['plain']);

    const all = await runtime.takeAll(SESSION);
    expect(all.items.map((entry) => entry.content)).toEqual(['plain']);
    expect(runtime.snapshot().sessions).toEqual([]);
  });

  it('retains a bounded context preview in snapshots and broadcasts without exposing the full payload', async () => {
    const { runtime, broadcasts } = createRuntime();
    runtime.start();
    const context = [{ kind: 'context', text: 'Full quoted content', metadata: { openchamberContext: { kind: 'chat-quote', quote: 'Original answer', text: 'Explain this' } } }];
    const { itemId } = await runtime.enqueue(SESSION, DIRECTORY, item({ content: '', text: '', context, contextPreview: 'Explain this' }));
    const projected = runtime.sessionSnapshot(SESSION).items[0];
    expect(projected.contextPreview).toBe('Explain this');
    expect(projected.content).toBe('');
    expect(projected.text).toBe('');
    expect(projected).not.toHaveProperty('context');
    expect(broadcasts.at(-1).properties.session.items[0].contextPreview).toBe('Explain this');
    const taken = await runtime.take(SESSION, itemId);
    expect(taken.item.context).toEqual(context);
    expect(taken.item.content).toBe('');

    await runtime.enqueue(SESSION, DIRECTORY, item({ content: '', text: '', context, contextPreview: 'a'.repeat(5000) }));
    expect(runtime.sessionSnapshot(SESSION).items[0].contextPreview).toBe('a'.repeat(100) + '...');
  });

  it('derives a preview for older queued annotations without a saved summary', async () => {
    const { runtime } = createRuntime();
    runtime.start();
    await runtime.enqueue(SESSION, DIRECTORY, item({ content: '', text: '', context: [
      { kind: 'instruction', text: 'Use the skill' },
      { kind: 'context', text: 'Model-facing wrapper', metadata: { openchamberContext: { kind: 'browser-annotation', text: 'Fix the button\nMore detail' } } },
    ] }));
    expect(runtime.sessionSnapshot(SESSION).items[0].contextPreview).toBe('Fix the button...');
  });

  it('names the directory in the broadcast that empties a queue', async () => {
    // The UI keys its projection by directory; without it the client cannot
    // tell which queue just delivered its last message and keeps showing it.
    const { runtime, emit, broadcasts, openCode } = createRuntime();
    runtime.start();
    await runtime.enqueue(SESSION, DIRECTORY, item());
    openCode.state.active = {};
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();

    expect(openCode.state.sent).toHaveLength(1);
    expect(runtime.snapshot().sessions).toEqual([]);
    expect(broadcasts.at(-1).properties.session).toEqual({ sessionId: SESSION, directory: DIRECTORY, items: [], sendingId: null });
  });

  it('reorders only with a complete permutation', async () => {
    const { runtime } = createRuntime();
    runtime.start();
    const a = await runtime.enqueue(SESSION, DIRECTORY, item({ content: 'a' }));
    const b = await runtime.enqueue(SESSION, DIRECTORY, item({ content: 'b' }));
    await expect(runtime.reorder(SESSION, [b.itemId])).rejects.toThrow(TypeError);
    await runtime.reorder(SESSION, [b.itemId, a.itemId]);
    expect(runtime.sessionSnapshot(SESSION).items.map((entry) => entry.content)).toEqual(['b', 'a']);
  });

  it('drops the queue of a deleted session', async () => {
    const { runtime, emit, broadcasts } = createRuntime();
    runtime.start();
    await runtime.enqueue(SESSION, DIRECTORY, item());
    emit({ type: 'session.deleted', properties: { info: { id: SESSION } } });
    expect(runtime.snapshot().sessions).toEqual([]);
    expect(broadcasts.at(-1).properties.session).toMatchObject({ sessionId: SESSION, items: [] });
  });

  it('dispatches a queued slash command through the command endpoint', async () => {
    const { runtime, openCode, emit } = createRuntime();
    runtime.start();
    openCode.state.commands = [{ name: 'review' }];
    await runtime.enqueue(SESSION, DIRECTORY, item({ content: '/review src', text: '/review src', sendConfig: { providerID: 'p', modelID: 'm', agent: 'build', variant: 'max' } }));
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    expect(openCode.state.sent).toHaveLength(1);
    expect(openCode.state.sent[0].path).toBe(`/api/session/${SESSION}/command`);
    // v2's command route takes `text`, and the selection lives on the session.
    expect(openCode.state.sent[0].body).toEqual({ name: 'review', text: 'src' });
  });

  it('delivers captured context as synthetic messages, instructions first', async () => {
    const { runtime, openCode, emit } = createRuntime();
    runtime.start();
    const metadata = { openchamberContext: { kind: 'github-pr', number: 7, title: 'PR', url: 'https://x/pr/7' } };
    await runtime.enqueue(SESSION, DIRECTORY, item({
      agentMention: 'reviewer',
      attachments: [{ id: 'a', filename: 'f.txt', mimeType: 'text/plain', size: 1, source: 'local', dataUrl: 'data:text/plain,hi' }],
      context: [
        { kind: 'context', text: 'the diff', metadata, instructions: 'how to read it' },
        { kind: 'synthetic', text: 'conflict payload' },
        { kind: 'instruction', text: 'use the skill' },
      ],
    }));
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();

    // v2 has no inline synthetic parts: everything attached to the message is
    // admitted as its own synthetic message, before the prompt.
    expect(openCode.state.sent.map((entry) => entry.path)).toEqual([
      `/api/session/${SESSION}/synthetic`,
      `/api/session/${SESSION}/synthetic`,
      `/api/session/${SESSION}/synthetic`,
      `/api/session/${SESSION}/synthetic`,
      `/api/session/${SESSION}/prompt`,
    ]);
    expect(openCode.state.sent.slice(0, 4).map((entry) => entry.body)).toEqual([
      { text: 'how to read it', resume: false },
      { text: 'the diff', resume: false, metadata },
      { text: 'conflict payload', resume: false },
      { text: 'use the skill', resume: false },
    ]);
    expect(openCode.state.sent.at(-1).body).toEqual({
      text: 'follow up',
      files: [{ uri: 'data:text/plain,hi', name: 'f.txt' }],
      agents: [{ name: 'reviewer' }],
    });
  });

  it('keeps files on the command route, which is all that route accepts', async () => {
    const { runtime, openCode, emit } = createRuntime();
    runtime.start();
    openCode.state.commands = [{ name: 'review' }];
    await runtime.enqueue(SESSION, DIRECTORY, item({
      content: '/review',
      text: '/review',
      attachments: [{ id: 'a', filename: 'f.txt', mimeType: 'text/plain', size: 1, source: 'local', dataUrl: 'data:text/plain,hi' }],
    }));
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    expect(openCode.state.sent[0].path).toBe(`/api/session/${SESSION}/command`);
    expect(openCode.state.sent[0].body).toEqual({
      name: 'review',
      text: '',
      files: [{ uri: 'data:text/plain,hi', name: 'f.txt' }],
    });
  });

  it('hands the model an attachment\'s resolved on-disk path as its name', async () => {
    const { runtime, openCode, emit } = createRuntime();
    runtime.start();
    await runtime.enqueue(SESSION, DIRECTORY, item({
      attachments: [{
        id: 'a', filename: 'report.txt', mimeType: 'text/plain', size: 1, source: 'local',
        dataUrl: 'data:text/plain,hi', agentPath: '/Users/me/Documents/report.txt',
      }],
    }));
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    expect(openCode.state.sent.at(-1).body).toEqual({
      text: 'follow up',
      files: [{ uri: 'data:text/plain,hi', name: '/Users/me/Documents/report.txt' }],
    });
  });

  it('admits captured context as synthetic messages and still runs a queued command through the command route', async () => {
    // The command route takes attachments only, so the context goes ahead as
    // synthetic messages; the command itself keeps its route, because sending
    // "/review ..." as a prompt would skip the template OpenCode expands there.
    const { runtime, openCode, emit } = createRuntime();
    runtime.start();
    openCode.state.commands = [{ name: 'review', description: 'Review' }];
    const metadata = { openchamberContext: { kind: 'chat-quote', quote: 'q', text: 'why?' } };
    await runtime.enqueue(SESSION, DIRECTORY, item({
      content: '/review src "error handling"',
      text: '/review src "error handling"',
      context: [{ kind: 'context', text: 'quoted', metadata }],
    }));
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();

    expect(openCode.state.sent.map((entry) => entry.path)).toEqual([
      `/api/session/${SESSION}/synthetic`,
      `/api/session/${SESSION}/command`,
    ]);
    expect(openCode.state.sent[0].body).toEqual({ text: 'quoted', resume: false, metadata });
    expect(openCode.state.sent[1].body).toEqual({ name: 'review', text: 'src "error handling"' });
  });

  it('writes the project knowledge into the session before a queued command', async () => {
    const openCode = createOpenCode();
    const knowledge = {
      sync: async (sessionId, directory, responseStyleInstruction) => {
        openCode.state.sent.push({ path: 'knowledge', body: { sessionId, directory, responseStyleInstruction } });
      },
    };
    const { runtime, emit } = createRuntime({ openCode, knowledge });
    runtime.start();
    openCode.state.commands = [{ name: 'review' }];
    await runtime.enqueue(SESSION, DIRECTORY, item({ content: '/review', text: '/review' }));
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();

    expect(openCode.state.sent).toEqual([
      { path: 'knowledge', body: { sessionId: SESSION, directory: DIRECTORY } },
      expect.objectContaining({ path: `/api/session/${SESSION}/command` }),
    ]);
  });

  it('admits nothing when the command lookup fails, so a retry cannot duplicate the context', async () => {
    const { runtime, openCode, emit } = createRuntime();
    runtime.start();
    openCode.state.failNext = /^\/api\/command$/;
    await runtime.enqueue(SESSION, DIRECTORY, item({
      content: '/review',
      text: '/review',
      context: [{ kind: 'context', text: 'quoted', metadata: {} }],
    }));
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();

    expect(openCode.state.sent).toEqual([]);
    expect(runtime.sessionSnapshot(SESSION).items).toHaveLength(1);
  });

  it('keeps captured context out of snapshots and broadcasts, and hands it back on take', async () => {
    const { runtime, broadcasts } = createRuntime();
    runtime.start();
    const context = [{ kind: 'synthetic', text: 'a large diff' }];
    const { itemId } = await runtime.enqueue(SESSION, DIRECTORY, item({ context }));
    expect(runtime.sessionSnapshot(SESSION).items[0]).not.toHaveProperty('context');
    expect(runtime.sessionSnapshot(SESSION).items[0].text).toBe('follow up');
    expect(broadcasts.at(-1).properties.session.items[0]).not.toHaveProperty('context');
    const taken = await runtime.take(SESSION, itemId);
    expect(taken.item.context).toEqual(context);
  });

  it('writes the project knowledge into the session before the prompt, and sends anyway when that fails', async () => {
    const openCode = createOpenCode();
    const knowledge = {
      // `sync` reports failure by resolving false; it never throws.
      sync: async (sessionId, directory, responseStyleInstruction) => {
        openCode.state.sent.push({ path: 'knowledge', body: { sessionId, directory, responseStyleInstruction } });
        return false;
      },
    };
    const { runtime, emit } = createRuntime({ openCode, knowledge });
    runtime.start();
    await runtime.enqueue(SESSION, DIRECTORY, item({ context: [{ kind: 'context', text: 'quoted', metadata: {} }] }));
    emit({ type: 'session.status', properties: { sessionID: SESSION, status: { type: 'idle' } } });
    await settle();
    expect(openCode.state.sent.map((entry) => entry.path)).toEqual([
      'knowledge',
      `/api/session/${SESSION}/synthetic`,
      `/api/session/${SESSION}/prompt`,
    ]);
    expect(openCode.state.sent[0].body).toEqual({ sessionId: SESSION, directory: DIRECTORY });
  });
});
