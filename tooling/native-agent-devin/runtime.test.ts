import { describe, test, expect, spyOn } from 'bun:test';
import { readFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { PassThrough } from 'node:stream';
import type { NativeAgentScope, NativeAgentState } from '../../packages/sdk/src/native-agent.ts';
import { nativeAgentResultSchema } from '../../packages/sdk/src/native-agent-schemas.ts';
import { Bindings } from './bindings.ts';
import { DevinRuntime, NativeFailure, type Backend, type SessionHandle } from './runtime.ts';
import { projector } from './model.ts';
import { assertPromptSupported, toDevinPrompt } from './prompt.ts';
import { LocalDevin, SessionEventReducer, type InitializeResult, type PermissionRequest, type SessionConfigOption, type SessionNotification, type SessionUpdate } from '@cognition-ai/sdk';
import type { Plugin } from '@opencode/plugin';
import { localProcess } from './local-process.ts';
import type { DevinNativeAgent } from './rpc.ts';

type FixtureFrame =
  | { id: string | number | undefined; result: InitializeResult | { sessionId: string; configOptions?: SessionConfigOption[] } | { stopReason: string } | Record<string, never> }
  | { id: string | number; method: 'session/request_permission'; params: PermissionRequest }
  | { method: 'session/update'; params: SessionNotification };
type FixtureClient = { promptCapabilities: LocalDevin['promptCapabilities']; close: LocalDevin['close']; createSession(): Promise<SessionHandle> };

const scope: NativeAgentScope = { directory: 'C:/project', providerID: 'devin', sessionID: 'ses_test' };
const read = (id: string) => ({ protocolVersion: 1 as const, requestID: id, scope, activationID: null });
function fixture(saved = new Map<string, unknown>()) {
  let opens = 0;
  let sends = 0;
  let auth = 0;
  let allowed = true;
  let available = true;
  let selectedModel = '';
  const snapshots = new Map<string, { config: NativeAgentState['config']; mode: 'local' | 'cloud' }>();
  const session: SessionHandle = {
    id: 'devin-test', configOptions: [{ id: 'model', currentValue: 'actual-a', options: [{ value: 'actual-a' }, { value: 'actual-b' }] }],
    onPermission: undefined,
    async *stream() { sends++; yield { type: 'message_delta' as const, text: 'hello', messageId: 'reply', overwrite: false, aborted: false, chain: 'main' as const, update: { sessionUpdate: 'agent_message_chunk' } }; return { stopReason: 'end_turn' }; },
    cancel() {}, async setMode() {}, async setModel(id: string) { selectedModel = id; },
  };
  const backend: Backend = {
    hasCredential: () => available, authAllowed: () => allowed,
    async checkScope() {}, async authorize() {},
    async authenticate() { auth++; return 'ready'; },
    async open() { opens++; return { session, close() {} }; },
    async read(key) { return structuredClone(saved.get(key)); },
    async write(key, value) { saved.set(key, structuredClone(value)); },
    async emit() {},
    async bind(config, mode) { const token = createHash('sha256').update(JSON.stringify(config)).digest('hex'); snapshots.set(token, { config: structuredClone(config), mode }); return { providerID: 'devin', modelID: mode, variant: token }; },
    async resolveBinding(token) { const value = snapshots.get(token); if (!value) throw Error('unknown'); return structuredClone(value); },
  };
  return { runtime: new DevinRuntime(scope.directory, backend), backend, session, saved,
    counts: () => ({ opens, sends, auth, selectedModel }), policy: (value: boolean) => { allowed = value; }, credential: (value: boolean) => { available = value; } };
}
function state(result: Awaited<ReturnType<DevinRuntime['call']>>) {
  nativeAgentResultSchema.parse(result);
  if (!result.ok) throw Error(result.error.code);
  return result.state;
}

describe('Devin native-agent state', () => {
  test('draft preparation and execution cannot allocate an SDK session even with a permissive backend', async () => {
    const f = fixture();
    const draftScope = { ...scope, sessionID: null };
    expect(state(await f.runtime.call('prepare', { ...read('draft-prepare'), scope: draftScope }))).toMatchObject({ status: 'unprepared', execution: null });
    await expect(f.runtime.execute(draftScope, 'unpublished', 'hello', 'draft-submission', new AbortController().signal).next()).rejects.toThrow('not_prepared');
    expect(f.counts()).toMatchObject({ opens: 0, sends: 0, auth: 0 });
  });
  test('getState is side-effect free and preparation never sends a prompt', async () => {
    const f = fixture();
    expect(state(await f.runtime.call('getState', read('get'))).execution).toBeNull();
    expect(f.counts()).toMatchObject({ opens: 0, sends: 0, auth: 0 });
    const ready = state(await f.runtime.call('prepare', read('prepare')));
    expect(ready.authentication).toBe('authenticated');
    expect(ready.execution?.modelID).toBe('local');
    expect(f.counts()).toMatchObject({ opens: 1, sends: 0, auth: 0 });
  });
  test('missing credentials and enterprise sign-in refusal stay truthful', async () => {
    const f = fixture(); f.credential(false);
    const initial = state(await f.runtime.call('getState', read('get')));
    expect(initial.authentication).toBe('required');
    expect(await f.runtime.call('prepare', read('prepare'))).toMatchObject({ ok: false, error: { code: 'authentication_required', outcome: 'rejected' } });
    const current = state(await f.runtime.call('getState', read('again')));
    f.policy(false);
    expect(await f.runtime.call('authenticate', { ...read('auth'), activationID: current.activationID, expectedRevision: current.revision, userInitiated: true })).toMatchObject({ ok: false, error: { code: 'policy_denied' } });
    expect(f.counts().auth).toBe(0);
  });
  test('mutations are idempotent and stale/different requests fail before side effects', async () => {
    const f = fixture();
    const ready = state(await f.runtime.call('prepare', read('prepare')));
    const input = { ...read('config'), activationID: ready.activationID, expectedRevision: ready.revision, changes: [{ id: 'model', value: 'actual-b' }] };
    const first = await f.runtime.call('setConfig', input);
    expect(await f.runtime.call('setConfig', input)).toEqual(first);
    expect(await f.runtime.call('setConfig', { ...input, changes: [{ id: 'model', value: 'actual-a' }] })).toMatchObject({ ok: false, error: { code: 'invalid_request' } });
    expect(await f.runtime.call('setConfig', { ...input, requestID: 'stale' })).toMatchObject({ ok: false, error: { code: 'stale_revision' } });
    expect(await f.runtime.call('prepare', { ...read('stale-activation'), activationID: 'old' })).toMatchObject({ ok: false, error: { code: 'stale_activation' } });
  });
  test('queued execution uses captured immutable config after settings change', async () => {
    const f = fixture();
    const ready = state(await f.runtime.call('prepare', read('prepare')));
    await f.runtime.call('setConfig', { ...read('config'), activationID: ready.activationID, expectedRevision: ready.revision, changes: [{ id: 'model', value: 'actual-b' }] });
    const events = [];
    for await (const event of f.runtime.execute(scope, ready.execution!.variant, 'hello', 'submission', new AbortController().signal)) events.push(event);
    expect(f.counts().selectedModel).toBe('actual-a');
    expect(events).toHaveLength(1);
    expect(state(await f.runtime.call('getState', read('get'))).submission.status).toBe('none');
  });
  test('offered permission IDs round-trip and standing choices are omitted', async () => {
    const f = fixture();
    await f.runtime.call('prepare', read('prepare'));
    const permission = f.session.onPermission!({ sessionId: f.session.id, toolCall: { toolCallId: 'call' }, options: [
      { optionId: 'service-allow', name: 'Allow', kind: 'allow_once' }, { optionId: 'forever', name: 'Always', kind: 'allow_always' },
      { optionId: 'service-deny', name: 'Deny', kind: 'reject_once' },
    ] });
    await new Promise<void>((resolve) => setImmediate(resolve));
    const current = state(await f.runtime.call('getState', read('pending')));
    expect(current.requests[0].kind === 'permission' && current.requests[0].choices.map((choice) => choice.id)).toEqual(['service-allow', 'service-deny']);
    const request = { ...read('answer'), activationID: current.activationID, expectedRevision: current.revision,
      answer: { kind: 'permission' as const, requestID: current.requests[0].id, choiceID: 'service-allow' } };
    expect((await f.runtime.call('answer', request)).ok).toBe(true);
    expect(await permission).toEqual({ optionId: 'service-allow' });
    expect((await f.runtime.call('answer', request)).ok).toBe(true);
  });
  test('scope mismatch and unknown execution tokens fail closed', async () => {
    const f = fixture();
    expect(await f.runtime.call('prepare', { ...read('bad'), scope: { ...scope, directory: 'C:/other' } })).toMatchObject({ ok: false, error: { code: 'invalid_request' } });
    const iterator = f.runtime.execute(scope, 'missing', 'hello', 'sub', new AbortController().signal);
    await expect(iterator.next()).rejects.toThrow();
    expect(f.counts().sends).toBe(0);
  });
  test('provider policy denies captured execution before opening or submitting', async () => {
    const f = fixture();
    const ready = state(await f.runtime.call('prepare', read('prepare')));
    f.backend.authorize = async () => { throw new NativeFailure('policy_denied'); };
    const iterator = f.runtime.execute(scope, ready.execution!.variant, 'hello', 'sub', new AbortController().signal);
    await expect(iterator.next()).rejects.toThrow('policy_denied');
    expect(f.counts()).toMatchObject({ opens: 1, sends: 0 });
  });
  test('connection failure retains uncertainty across reload and never replays a turn', async () => {
    const f = fixture(); let attempts = 0;
    const ready = state(await f.runtime.call('prepare', read('prepare')));
    f.session.stream = async function* () { attempts++; throw Error('connection dropped with private service text'); };
    await expect(f.runtime.execute(scope, ready.execution!.variant, 'hello', 'sub', new AbortController().signal).next()).rejects.toThrow('outcome_unknown');
    const recovered = new DevinRuntime(scope.directory, f.backend);
    const snapshot = state(await recovered.call('getState', read('reload')));
    expect(snapshot.activationID).not.toBe(ready.activationID);
    expect(snapshot.submission).toEqual({ status: 'outcome-unknown', id: 'sub' });
    await recovered.call('prepare', read('reconnect'));
    await expect(recovered.execute(scope, ready.execution!.variant, 'hello', 'again', new AbortController().signal).next()).rejects.toThrow('outcome_unknown');
    expect(attempts).toBe(1);
    expect(JSON.stringify(snapshot)).not.toContain('private service text');
  });
  test('prepare preserves persisted account model selections across SDK reload defaults', async () => {
    const f = fixture();
    const ready = state(await f.runtime.call('prepare', read('prepare')));
    state(await f.runtime.call('setConfig', { ...read('select'), activationID: ready.activationID, expectedRevision: ready.revision, changes: [{ id: 'model', value: 'actual-b' }] }));
    await f.runtime.dispose();
    const restored = new DevinRuntime(scope.directory, f.backend);
    const prepared = state(await restored.call('prepare', read('restore')));
    expect(prepared.config.find((field) => field.id === 'model')?.value).toBe('actual-b');
  });
  test('dispose closes an in-flight late backend without ready state, writes or events', async () => {
    const f = fixture();
    let release!: () => void;
    const opened = new Promise<void>((resolve) => { release = resolve; });
    let closeCount = 0; let writes = 0; let events = 0;
    f.backend.open = async () => { await opened; return { session: f.session, close() { closeCount++; } }; };
    f.backend.write = async () => { writes++; };
    f.backend.emit = async () => { events++; };
    const preparing = f.runtime.call('prepare', read('prepare'));
    await new Promise<void>((resolve) => setImmediate(resolve));
    const disposed = f.runtime.dispose();
    const before = { writes, events };
    release();
    const result = await preparing;
    await disposed;
    expect(result).toMatchObject({ ok: false, error: { code: 'unavailable' } });
    expect(closeCount).toBe(1);
    expect({ writes, events }).toEqual(before);
  });
  test('an invalid persisted choice requires explicit reselection from refreshed choices', async () => {
    const f = fixture();
    const ready = state(await f.runtime.call('prepare', read('prepare')));
    await f.runtime.call('setConfig', { ...read('select'), activationID: ready.activationID, expectedRevision: ready.revision, changes: [{ id: 'model', value: 'actual-b' }] });
    await f.runtime.dispose();
    Object.defineProperty(f.session, 'configOptions', { value: [{ id: 'model', currentValue: 'new-a', options: [{ value: 'new-a' }, { value: 'new-b' }] }] });
    const restored = new DevinRuntime(scope.directory, f.backend);
    const invalid = await restored.call('prepare', read('restore'));
    expect(invalid).toMatchObject({ ok: false, error: { code: 'invalid_config' }, state: { status: 'error', execution: null } });
    if (!invalid.state) throw Error('missing failure snapshot');
    expect(invalid.state.config.find((field) => field.id === 'model')?.choices.map((choice) => choice.id)).toEqual(['new-a', 'new-b']);
    await restored.call('setConfig', { ...read('reselect'), activationID: invalid.state.activationID, expectedRevision: invalid.state.revision, changes: [{ id: 'model', value: 'new-b' }] });
    expect(state(await restored.call('prepare', read('retry'))).config.find((field) => field.id === 'model')?.value).toBe('new-b');
  });
  test('confirmed SDK cancellation clears the submission and preserves selected choices', async () => {
    const f = fixture();
    const ready = state(await f.runtime.call('prepare', read('prepare')));
    f.session.stream = async function* (_prompt, options) {
      await new Promise<void>((resolve) => {
        if (options?.signal?.aborted) resolve();
        else options?.signal?.addEventListener('abort', () => resolve(), { once: true });
      });
      return { stopReason: 'cancelled' };
    };
    const abort = new AbortController();
    const result = f.runtime.execute(scope, ready.execution!.variant, 'hello', 'sub', abort.signal).next();
    await new Promise<void>((resolve) => setImmediate(resolve));
    abort.abort();
    expect((await result).done).toBe(true);
    const stopped = state(await f.runtime.call('getState', read('stopped')));
    expect(stopped.status).toBe('stopped');
    expect(stopped.submission.status).toBe('none');
    expect(stopped.config.find((field) => field.id === 'model')?.value).toBe('actual-a');
  });
  test('configured deny rejects ACP permission before native approval becomes visible', async () => {
    const f = fixture();
    await f.runtime.call('prepare', read('prepare'));
    f.backend.authorize = async (_scope, permission) => { if (permission) throw Error('denied'); };
    const denied = await f.session.onPermission!({ sessionId: f.session.id, toolCall: {}, options: [{ optionId: 'once', name: 'Allow once', kind: 'allow_once' }] });
    expect(denied).toHaveProperty('optionId');
    expect(denied).not.toEqual({ optionId: 'once' });
    expect(state(await f.runtime.call('getState', read('after'))).requests).toEqual([]);
  });
  test('live progress requires the persisted provider-executed call identity and never revives settled tools', async () => {
    const f = fixture();
    const emitted: import('../../packages/sdk/src/native-agent.ts').NativeAgentEvent[] = [];
    f.backend.emit = async (event) => { emitted.push(event); };
    const ready = state(await f.runtime.call('prepare', read('prepare')));
    f.session.stream = async function* () {
      const reducer = new SessionEventReducer();
      yield reducer.handle({ sessionUpdate: 'tool_call', toolCallId: 'call-1', title: 'Running test', kind: 'execute', status: 'in_progress', rawInput: {} });
      yield reducer.handle({ sessionUpdate: 'tool_call_update', toolCallId: 'call-1', status: 'completed', rawOutput: 'passed' });
      return { stopReason: 'end_turn' };
    };
    const run = f.runtime.execute(scope, ready.execution!.variant, 'test', 'submission', new AbortController().signal);
    await run.next();
    await f.runtime.observeTool(scope.sessionID!, 'message', 'unrelated', true, true);
    expect(emitted.filter((event) => event.type === 'toolProgress')).toHaveLength(0);
    await f.runtime.observeTool(scope.sessionID!, 'message', 'call-1', true, true);
    expect(emitted.find((event) => event.type === 'toolProgress')).toMatchObject({ messageID: 'message', partID: 'call-1', callID: 'call-1', sequence: 1, text: 'Running test' });
    await run.next();
    await f.runtime.observeTool(scope.sessionID!, 'message', 'call-1', true, true);
    expect(emitted.filter((event) => event.type === 'toolProgress')).toHaveLength(1);
    await run.next();
  });
});

test('the model stream preserves definite NativeFailure policy rejection', async () => {
  const f = fixture();
  const ready = state(await f.runtime.call('prepare', read('prepare')));
  f.backend.authorize = async () => { throw new NativeFailure('policy_denied'); };
  const { languageModel } = await import('./model.ts');
  const result = await languageModel('local', scope.directory, f.runtime).doStream({
    prompt: [{ role: 'user', content: [{ type: 'text', text: 'fixture' }] }],
    headers: { 'x-openchamber-devin-session': scope.sessionID!, 'x-openchamber-devin-kind': 'primary' },
    providerOptions: { devin: { nativeConfigToken: ready.execution!.variant } },
  });
  const reader = result.stream.getReader();
  try { await reader.read(); throw Error('expected rejection'); }
  catch (error) { expect(error).toMatchObject({ code: 'policy_denied', uncertain: false }); }
});

test('post-submission uncertainty survives a failed recovery write through the model stream', async () => {
  const f = fixture();
  const ready = state(await f.runtime.call('prepare', read('prepare')));
  let submissions = 0;
  f.session.stream = async function* () { submissions++; throw Error('backend disconnected after submission'); };
  const write = f.backend.write;
  f.backend.write = async (key, value) => {
    if (value.submission?.status === 'outcome-unknown') throw Error('recovery storage unavailable');
    await write(key, value);
  };
  const { languageModel } = await import('./model.ts');
  const result = await languageModel('local', scope.directory, f.runtime).doStream({
    prompt: [{ role: 'user', content: [{ type: 'text', text: 'fixture' }] }],
    headers: { 'x-openchamber-devin-session': scope.sessionID!, 'x-openchamber-devin-kind': 'primary' },
    providerOptions: { devin: { nativeConfigToken: ready.execution!.variant } },
  });
  try { await result.stream.getReader().read(); throw Error('expected rejection'); }
  catch (error) { expect(error).toMatchObject({ code: 'outcome_unknown', uncertain: true }); }
  expect(submissions).toBe(1);
  const unknown = state(await f.runtime.call('getState', read('unknown')));
  expect(unknown.submission.status).toBe('outcome-unknown');
  await expect(f.runtime.execute(scope, ready.execution!.variant, 'again', 'duplicate', new AbortController().signal).next()).rejects.toThrow('outcome_unknown');
  expect(submissions).toBe(1);
});

for (const hostileID of ['allow', 'reject']) test(`opaque permission option '${hostileID}' reaches the official SDK unchanged`, async () => {
  const f = fixture();
  await f.runtime.call('prepare', read('prepare'));
  const readable = new PassThrough();
  const send = (message: FixtureFrame) => readable.write(JSON.stringify({ jsonrpc: '2.0', ...message }) + '\n');
  let replied!: (value: unknown) => void;
  const reply = new Promise((resolve) => { replied = resolve; });
  const client = await LocalDevin.connect({ readable, writable: { write(data: string) {
    // SAFETY: the connected SDK writes JSON-RPC messages with an optional ID and result.
    const request = JSON.parse(data) as { id?: string | number; method?: string; result?: unknown };
    queueMicrotask(() => {
      if (request.method === 'initialize') send({ id: request.id, result: { protocolVersion: 1, agentCapabilities: {} } });
      else if (request.method === 'session/new') send({ id: request.id, result: { sessionId: f.session.id } });
      else if (request.id === 'hostile-permission') replied(request.result);
      else if (request.id !== undefined) send({ id: request.id, result: {} });
    });
  } } });
  try {
    const session = await client.createSession({ onPermission: f.session.onPermission });
    send({ id: 'hostile-permission', method: 'session/request_permission', params: {
      sessionId: session.id, toolCall: {}, options: [
        { optionId: hostileID, name: 'Opaque service choice', kind: hostileID === 'allow' ? 'reject_once' : 'allow_once' },
        { optionId: 'different-choice', name: 'Other service choice', kind: hostileID === 'allow' ? 'allow_once' : 'reject_once' },
      ],
    } });
    await new Promise<void>((resolve) => setImmediate(resolve));
    const pending = state(await f.runtime.call('getState', read('pending')));
    await f.runtime.call('answer', { ...read('answer'), activationID: pending.activationID, expectedRevision: pending.revision,
      answer: { kind: 'permission', requestID: pending.requests[0].id, choiceID: hostileID } });
    expect(await reply).toEqual({ outcome: { outcome: 'selected', optionId: hostileID } });
  } finally { client.close(); readable.end(); }
});

test('binding publication failure keeps durable snapshots and retries publication', async () => {
  let persisted: Awaited<ReturnType<Plugin.Context['storage']['get']>>; let fail = true;
  const bindings = new Bindings({ read: async () => persisted, write: async (value) => { persisted = structuredClone(value); }, reload: async () => { if (fail) throw Error('offline'); } });
  const config: NativeAgentState['config'] = [{ id: 'm', role: 'model', label: 'Model', value: 'a', choices: [{ id: 'a', label: 'A' }] }];
  await expect(bindings.bind(config, 'local')).rejects.toThrow();
  fail = false;
  const binding = await bindings.bind(config, 'local');
  const restored = new Bindings({ read: async () => persisted, write: async () => {}, reload: async () => {} });
  await restored.load();
  expect(await restored.resolve(binding.variant)).toEqual({ config, mode: 'local' });
  await expect(restored.resolve('unknown')).rejects.toThrow();
});

test('persisted bindings reject modes that are not valid mode strings', async () => {
  for (const mode of [['local'], 1, null, '', 'unexpected']) {
    const value = { mode, config: [] };
    const token = `native-${createHash('sha256').update(JSON.stringify(value)).digest('hex')}`;
    const bindings = new Bindings({ read: async () => ({ [token]: value }), write: async () => {}, reload: async () => {} });
    await expect(bindings.load()).rejects.toMatchObject({ code: 'unavailable' });
    expect(bindings.variants('local')).toEqual([]);
  }
});

test('persisted runtime config requires a valid mode and choices', async () => {
  const runtime = { id: 'runtime', role: 'other', label: 'Execution', value: 'local', choices: [{ id: 'local', label: 'Local' }] };
  for (const config of [[], [{ ...runtime, value: 'unexpected', choices: [{ id: 'unexpected', label: 'Unexpected' }] }],
    [{ ...runtime, choices: [...runtime.choices, { id: 'unexpected', label: 'Unexpected' }] }]]) {
    const f = fixture();
    const journal = { mutations: {}, consumedUsers: [], config };
    let writes = 0;
    f.backend.read = async () => structuredClone(journal);
    f.backend.write = async () => { writes++; };
    expect(await f.runtime.call('getState', read('bad-runtime'))).toMatchObject({ ok: false, error: { code: 'unavailable' } });
    expect(writes).toBe(0);
    expect(journal).toEqual({ mutations: {}, consumedUsers: [], config });
    await f.runtime.dispose();
  }
});

test('persisted malformed submissions are unavailable and never written', async () => {
  for (const submission of [false, null, 0, '', true, [], {}, { id: 's', status: 'none' }, { id: 7, status: 'pending' },
    { id: '', status: 'pending' }, { id: 'x'.repeat(257), status: 'pending' }, { id: '\n', status: 'pending' },
    { id: 's', status: 'pending', unexpected: true }]) {
    const f = fixture();
    const journal = { mutations: {}, consumedUsers: [], submission };
    let writes = 0;
    f.backend.read = async () => structuredClone(journal);
    f.backend.write = async () => { writes++; };
    expect(await f.runtime.call('getState', read('bad-submission'))).toMatchObject({ ok: false, error: { code: 'unavailable' } });
    expect(writes).toBe(0);
    expect(journal).toEqual({ mutations: {}, consumedUsers: [], submission });
    await f.runtime.dispose();
  }
});

test('falsy mutation results are malformed', async () => {
  for (const result of [false, null, 0, '']) {
    const f = fixture();
    let writes = 0;
    f.backend.read = async () => ({ mutations: { previous: { fingerprint: 'f', result } }, consumedUsers: [] });
    f.backend.write = async () => { writes++; };
    expect(await f.runtime.call('getState', read('falsy-result'))).toMatchObject({ ok: false, error: { code: 'unavailable' } });
    expect(writes).toBe(0);
    await f.runtime.dispose();
  }
});

test('SDK rich tool events become provider-executed OpenCode calls/results with original IDs', () => {
  const reducer = new SessionEventReducer();
  const project = projector();
  const call = reducer.handle({ sessionUpdate: 'tool_call', toolCallId: 'acp-call', kind: 'execute', status: 'in_progress', rawInput: { command: 'test' } });
  expect(project.event(call)[0]).toMatchObject({ type: 'tool-call', toolCallId: 'acp-call', providerExecuted: true });
  const result = reducer.handle({ sessionUpdate: 'tool_call_update', toolCallId: 'acp-call', status: 'completed', rawOutput: 'passed' });
  expect(project.event(result)[0]).toMatchObject({ type: 'tool-result', toolCallId: 'acp-call', result: 'passed', providerExecuted: true });
});

test('message overwrites and aborts preserve the final authoritative text', () => {
  const project = projector();
  const base = { type: 'message_delta' as const, messageId: 'm', chain: 'main' as const, update: { sessionUpdate: 'agent_message_chunk' }, aborted: false };
  project.event({ ...base, text: 'discard', overwrite: false });
  project.event({ ...base, text: 'final', overwrite: true });
  expect(project.finish('end_turn').filter((part) => part.type === 'text-delta').map((part) => part.delta)).toEqual(['final']);
});

test('OpenCode inline attachments use official ACP builders and negotiated byte limits', () => {
  const content = toDevinPrompt([
    { type: 'text', text: 'Inspect these files' },
    { type: 'file', mediaType: 'image/png', data: new Uint8Array([1, 2, 3]) },
    { type: 'file', mediaType: 'application/pdf', data: 'AQID', filename: 'report.pdf' },
  ]);
  expect(content).toMatchObject([{ type: 'text' }, { type: 'image', data: 'AQID', mimeType: 'image/png' }, { type: 'resource', resource: { blob: 'AQID', mimeType: 'application/pdf' } }]);
  expect(() => assertPromptSupported(content, { image: true, audio: false, embeddedContext: true, meta: { 'cognition.ai/maxEmbeddedResourceBytes': 3 } })).not.toThrow();
  expect(() => assertPromptSupported(content, { image: true, audio: false, embeddedContext: true, meta: { 'cognition.ai/maxEmbeddedResourceBytes': 2 } })).toThrow('unsupported');
  expect(() => assertPromptSupported(content, { image: false, audio: false, embeddedContext: true, meta: {} })).toThrow('unsupported');
  expect(() => toDevinPrompt([{ type: 'file', mediaType: 'image/png', data: new URL('https://example.com/private.png') }])).toThrow('unsupported');
});

test('valid 12 MB inline bytes and supplied base64 fit a 16 MiB negotiated limit', () => {
  const bytes = Buffer.alloc(12_000_000, 171);
  const encoded = bytes.toString('base64');
  const capabilities = { image: true, audio: false, embeddedContext: false, meta: { 'cognition.ai/maxEmbeddedResourceBytes': 16 * 1024 * 1024 } };
  for (const data of [bytes, encoded]) {
    const content = toDevinPrompt([{ type: 'file', mediaType: 'image/png', data }]);
    expect(() => assertPromptSupported(content, capabilities)).not.toThrow();
    expect(Array.isArray(content) && content[0].type === 'image' && Buffer.from(content[0].data, 'base64').equals(bytes)).toBe(true);
    expect(() => assertPromptSupported(content, { ...capabilities, meta: { 'cognition.ai/maxEmbeddedResourceBytes': bytes.length - 1 } })).toThrow('unsupported');
  }
  for (const malformed of ['A', 'AAA', 'AAAAA', 'A===', '=AAA', 'AA=A', 'AA==AAAA', 'AAAA====', 'AA-_', 'AA\n=', 'AA =']) {
    expect(() => toDevinPrompt([{ type: 'file', mediaType: 'image/png', data: malformed }])).toThrow('invalid_request');
  }
  for (const valid of ['', 'AAAA', 'AQ==', 'AQI=', 'AQID']) {
    expect(() => toDevinPrompt([{ type: 'file', mediaType: 'image/png', data: valid }])).not.toThrow();
  }
});

test('real plugin setup publishes newly negotiated input capabilities for an existing immutable token', async () => {
  const plugin = (await import('./server.ts')).default;
  type Context = Parameters<typeof plugin.setup>[0];
  type Models = Awaited<ReturnType<Context['model']['list']>>['data'];
  const store = new Map<string, unknown>();
  let base: Models = [];
  let catalog: Models = [];
  let dirty = true;
  let reloads = 0;
  let transform!: Parameters<Context['model']['transform']>[0];
  let rpc!: { [K in keyof import('../../packages/sdk/src/native-agent.ts').NativeAgentParameters]: (input: import('../../packages/sdk/src/native-agent.ts').NativeAgentParameters[K]) => Promise<import('../../packages/sdk/src/native-agent.ts').NativeAgentResult> };
  const session: SessionHandle = { id: 'fixture-setup', configOptions: [], onPermission: undefined, cancel() {}, async setMode() {}, async *stream() { throw Error('fixture cannot submit'); } };
  let closed = 0;
  const sdk: FixtureClient =
    { promptCapabilities: { image: false, audio: false, embeddedContext: false, meta: {} }, async createSession() { return session; }, close() { closed++; } };
  // SAFETY: this test exercises only the client methods implemented by the fixture.
  const spawn = spyOn(localProcess, 'spawn').mockResolvedValue(sdk as LocalDevin);
  const registration = { async dispose() {} };
  const context = {
    location: { directory: scope.directory }, options: {},
    storage: { async get(key: string) { return structuredClone(store.get(key)); }, async set(key: string, value: Parameters<Context['storage']['set']>[1]) { store.set(key, structuredClone(value)); } },
    provider: { async transform(callback: (editor: { add(value: { models: Models }): void }) => void) { callback({ add(value) { base = structuredClone(value.models); } }); return registration; } },
    model: {
      async transform(callback: typeof transform) { transform = callback; return registration; },
      async reload() { reloads++; dirty = true; },
      async list() {
        if (dirty) {
          catalog = structuredClone(base);
          const editor = { update(providerID: string, modelID: string, update: (model: Models[number]) => void) { const model = catalog.find((item) => item.providerID === providerID && item.id === modelID); if (model) update(model); } };
          // SAFETY: this plugin's model transform uses only update on the fixture editor.
          transform(editor as Parameters<typeof transform>[0]);
          dirty = false;
        }
        return { data: catalog };
      },
    },
    rpc: { async register(_definition: typeof DevinNativeAgent, handlers: typeof rpc) { rpc = handlers; return { events: { async emit() {} } }; } },
    permission: { async assert() {} },
    integration: { async transform(callback: (editor: { method: { update(): void } }) => void) { callback({ method: { update() {} } }); return registration; } },
    aisdk: { async hook() { return registration; } },
    session: { async hook() { return registration; }, async get() { return { location: { directory: scope.directory } }; } },
    event: { async *subscribe({ signal }: { signal: AbortSignal }) { await new Promise<void>((resolve) => signal.addEventListener('abort', () => resolve(), { once: true })); } },
  };
  // SAFETY: this fixture supplies every host member used by the plugin setup and hooks.
  const setupFixture = plugin.setup as (host: Context | typeof context) => ReturnType<typeof plugin.setup>;
  const cleanup = await setupFixture(context);
  try {
    state(await rpc.prepare({ ...read('mint-ready-token'), scope: { ...scope, sessionID: 'baseline-capabilities' } }));
    const target = { ...read('get-setup'), scope: { ...scope, sessionID: 'new-capabilities' } };
    const initial = state(await rpc.getState(target));
    state(await rpc.setConfig({ ...target, requestID: 'saved-config', activationID: initial.activationID, expectedRevision: initial.revision, changes: [{ id: 'runtime', value: 'local' }] }));
    expect((await context.model.list()).data.find((model) => model.id === 'local')?.capabilities.input).toEqual(['text']);
    const before = reloads;
    sdk.promptCapabilities = { image: true, audio: true, embeddedContext: true, meta: {} };
    state(await rpc.prepare({ ...target, requestID: 'prepare-capabilities' }));
    expect((await context.model.list()).data.find((model) => model.id === 'local')?.capabilities.input).toEqual(['text', 'image', 'audio', 'pdf']);
    expect(reloads).toBe(before + 1);
    state(await rpc.prepare({ ...read('same-capabilities'), scope: { ...scope, sessionID: 'second-fixture' } }));
    expect(reloads).toBe(before + 1);
    const reload = context.model.reload;
    context.model.reload = async () => { throw Error('catalog publication failed'); };
    sdk.promptCapabilities = { image: false, audio: true, embeddedContext: true, meta: {} };
    const failedScope = { ...scope, sessionID: 'publication-failure' };
    expect(await rpc.prepare({ ...read('failed-publication'), scope: failedScope })).toMatchObject({ ok: false, error: { code: 'unavailable' }, state: { status: 'error', execution: null } });
    expect(closed).toBe(1);
    context.model.reload = reload;
    state(await rpc.prepare({ ...read('retry-publication'), scope: failedScope }));
    expect((await context.model.list()).data.find((model) => model.id === 'local')?.capabilities.input).toEqual(['text', 'audio', 'pdf']);
    expect(reloads).toBe(before + 2);
    let failSharedPublication!: () => void;
    let enteredSharedPublication!: () => void;
    const sharedPublication = new Promise<void>((resolve) => { failSharedPublication = resolve; });
    const sharedPublicationEntered = new Promise<void>((resolve) => { enteredSharedPublication = resolve; });
    let sharedReloads = 0;
    context.model.reload = async () => { sharedReloads++; enteredSharedPublication(); await sharedPublication; throw Error('shared capability publication failed'); };
    sdk.promptCapabilities = { image: true, audio: true, embeddedContext: true, meta: {} };
    const closedBeforeSharedFailure = closed;
    const firstConcurrent = rpc.prepare({ ...read('concurrent-a'), scope: { ...scope, sessionID: 'concurrent-a' } });
    await sharedPublicationEntered;
    let secondSettled = false;
    const secondConcurrent = rpc.prepare({ ...read('concurrent-b'), scope: { ...scope, sessionID: 'concurrent-b' } }).then((result) => { secondSettled = true; return result; });
    await new Promise<void>((resolve) => setImmediate(resolve));
    const secondSettledBeforePublication = secondSettled;
    failSharedPublication();
    const concurrentResults = await Promise.all([firstConcurrent, secondConcurrent]);
    expect(secondSettledBeforePublication).toBe(false);
    expect(concurrentResults).toMatchObject([{ ok: false, error: { code: 'unavailable' } }, { ok: false, error: { code: 'unavailable' } }]);
    expect(sharedReloads).toBe(1);
    expect(closed).toBe(closedBeforeSharedFailure + 2);
    expect((await context.model.list()).data.find((model) => model.id === 'local')?.capabilities.input).toEqual(['text', 'audio', 'pdf']);
    expect(state(await rpc.getState({ ...read('prior-success'), scope: failedScope })).status).toBe('ready');
    let finishDifferentPublication!: () => void;
    let enteredDifferentPublication!: () => void;
    const differentPublication = new Promise<void>((resolve) => { finishDifferentPublication = resolve; });
    const differentPublicationEntered = new Promise<void>((resolve) => { enteredDifferentPublication = resolve; });
    let differentReloads = 0;
    context.model.reload = async () => {
      differentReloads++;
      if (differentReloads > 1) throw Error('later different capabilities failed');
      enteredDifferentPublication(); await differentPublication; await reload();
    };
    sdk.promptCapabilities = { image: true, audio: false, embeddedContext: true, meta: {} };
    const differentA = rpc.prepare({ ...read('different-a'), scope: { ...scope, sessionID: 'different-a' } });
    await differentPublicationEntered;
    sdk.promptCapabilities = { image: false, audio: false, embeddedContext: true, meta: {} };
    const closedBeforeDifferentFailure = closed;
    const differentB = rpc.prepare({ ...read('different-b'), scope: { ...scope, sessionID: 'different-b' } });
    await new Promise<void>((resolve) => setImmediate(resolve));
    finishDifferentPublication();
    expect((await differentA).ok).toBe(true);
    expect(await differentB).toMatchObject({ ok: false, error: { code: 'unavailable' } });
    expect(differentReloads).toBe(2);
    expect(closed).toBe(closedBeforeDifferentFailure + 1);
    expect((await context.model.list()).data.find((model) => model.id === 'local')?.capabilities.input).toEqual(['text', 'image', 'pdf']);
    let finishPublication!: () => void;
    const publication = new Promise<void>((resolve) => { finishPublication = resolve; });
    context.model.reload = async () => { await publication; };
    sdk.promptCapabilities = { image: false, audio: false, embeddedContext: false, meta: {} };
    const pending = rpc.prepare({ ...read('dispose-publication'), scope: { ...scope, sessionID: 'dispose-fixture' } });
    await new Promise<void>((resolve) => setImmediate(resolve));
    if (cleanup) await cleanup();
    const closedBeforeLatePublication = closed;
    finishPublication();
    expect(await pending).toMatchObject({ ok: false, error: { code: 'unavailable' } });
    expect(closed).toBe(closedBeforeLatePublication + 1);
  } finally { if (cleanup) await cleanup(); spawn.mockRestore(); }
});

test('shared v1 fixtures parse under the plugin RPC output schema', async () => {
  const fixtures = JSON.parse(await readFile(new URL('../../packages/sdk/fixtures/native-agent-v1.json', import.meta.url), 'utf8'));
  for (const fixture of fixtures.providers) nativeAgentResultSchema.parse({ protocolVersion: 1, requestID: 'fixture', ok: true, state: fixture.state });
});

for (const version of [1, 2] as const) test(`official SDK negotiates ACP ${version} and settles a streamed fixture turn`, async () => {
  const readable = new PassThrough();
  const send = (message: FixtureFrame) => readable.write(JSON.stringify({ jsonrpc: '2.0', ...message }) + '\n');
  const notify = (update: SessionUpdate) => send({ method: 'session/update', params: { sessionId: 'fixture-session', update } });
  const methods: string[] = [];
  const client = await LocalDevin.connect({ readable, writable: { write(data: string) {
    // SAFETY: the connected SDK writes JSON-RPC messages with a method and optional ID.
    const request = JSON.parse(data) as { id?: number; method: string };
    methods.push(request.method);
    queueMicrotask(() => {
      if (request.method === 'initialize') send({ id: request.id, result: { protocolVersion: version, ...(version === 1 ? { agentCapabilities: {} } : { capabilities: {} }) } });
      else if (request.method === 'session/new') send({ id: request.id, result: { sessionId: 'fixture-session' } });
      else if (request.method === 'session/prompt') {
        if (version === 2) send({ id: request.id, result: {} });
        notify({ sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'SDK fixture output' } });
        notify({ sessionUpdate: 'tool_call', toolCallId: 'sdk-call', kind: 'execute', status: 'in_progress', rawInput: { command: 'fixture-only' } });
        notify({ sessionUpdate: 'tool_call_update', toolCallId: 'sdk-call', status: 'completed', rawOutput: 'fixture result' });
        if (version === 1) send({ id: request.id, result: { stopReason: 'end_turn' } });
        else notify({ sessionUpdate: 'state_update', state: 'idle', stopReason: 'end_turn' });
      } else if (request.id !== undefined) send({ id: request.id, result: {} });
    });
  } } }, { acpVersion: version });
  try {
    expect(client.protocolVersion).toBe(version);
    const session = await client.createSession();
    const output = projector();
    const parts = [];
    const stream = session.stream('fixture only');
    while (true) {
      const next = await stream.next();
      if (next.done) { parts.push(...output.finish(next.value.stopReason)); break; }
      parts.push(...output.event(next.value));
    }
    expect(parts.find((part) => part.type === 'text-delta')?.delta).toBe('SDK fixture output');
    expect(parts.find((part) => part.type === 'tool-result')).toMatchObject({ toolCallId: 'sdk-call', providerExecuted: true });
    expect(methods).toContain('session/prompt');
  } finally { client.close(); readable.end(); }
});
