import { createHash, randomUUID } from 'node:crypto';
import { ModelRejectedError, toolCallText, type AcpSession, type PermissionRequest, type SessionConfigOption, type SessionEvent, type PromptInput, type PromptCapabilities } from '@cognition-ai/sdk';
import { assertPromptSupported } from './prompt.ts';
import { NativeFailure } from './errors.ts';
import { readContextCheckpoint, transferContext, type CapturedContext, type ContextCheckpoint } from './context.ts';
export { NativeFailure } from './errors.ts';
import {
  validateNativeAgentAnswer, validateNativeAgentConfig,
  type NativeAgentConfig, type NativeAgentErrorCode, type NativeAgentEvent,
  type NativeAgentMethod, type NativeAgentParameters, type NativeAgentResult,
  type NativeAgentScope, type NativeAgentState,
} from '../../packages/sdk/src/native-agent.ts';
import { nativeAgentConfigSchema, nativeAgentParametersSchemas, nativeAgentRecordedSubmissionSchema, nativeAgentResultSchema, nativeAgentStateSchema } from '../../packages/sdk/src/native-agent-schemas.ts';

export type Mode = 'local' | 'cloud';
export const isMode = (value: unknown): value is Mode => value === 'local' || value === 'cloud';
const isRecordedSubmission = (value: unknown): value is NonNullable<Journal['submission']> => nativeAgentRecordedSubmissionSchema.safeParse(value).success;
export type SessionHandle = Pick<AcpSession, 'id' | 'configOptions' | 'onPermission' | 'stream' | 'cancel' | 'setMode'> & { setModel?: (id: string) => Promise<void> };
export type Backend = {
  open(scope: NativeAgentScope, mode: Mode, existingID?: string): Promise<{ session: SessionHandle; promptCapabilities?: PromptCapabilities; close(): void }>;
  authenticate(scope: NativeAgentScope, mode: Mode): Promise<'pending' | 'ready'>;
  hasCredential(mode: Mode): boolean;
  checkScope(scope: NativeAgentScope): Promise<Mode | void>;
  authorize(scope: NativeAgentScope, request: PermissionRequest | undefined, mode: Mode): Promise<void>;
  authAllowed(): boolean;
  read(key: string): Promise<unknown>;
  write(key: string, value: Journal): Promise<void>;
  emit(event: NativeAgentEvent): Promise<void>;
  bind(config: NativeAgentConfig[], mode: Mode, signal: AbortSignal): Promise<{ providerID: string; modelID: string; variant: string }>;
  resolveBinding(token: string): Promise<{ config: NativeAgentConfig[]; mode: Mode }>;
};
type Journal = {
  sessionID?: string;
  mode?: Mode;
  config?: NativeAgentConfig[];
  submission?: { id: string; status: 'pending' | 'outcome-unknown' };
  context?: ContextCheckpoint;
  consumedUsers: string[];
  mutations: Record<string, { fingerprint: string; result?: NativeAgentResult }>;
};
type ExactPermissionChoice = { optionId: string };
type Entry = {
  state: NativeAgentState;
  mode: Mode;
  journal: Journal;
  handle?: Awaited<ReturnType<Backend['open']>>;
  pending: Map<string, { request: PermissionRequest; resolve: (choice: ExactPermissionChoice) => void }>;
  queue: Promise<unknown>;
  executing: boolean;
  liveCalls: Map<string, { messageID?: string; text?: string; sequence: number }>;
};
type FingerprintInput = NativeAgentScope | NativeAgentConfig[] | { method: NativeAgentMethod; input: NativeAgentParameters[NativeAgentMethod] };
const fingerprint = (value: FingerprintInput) => createHash('sha256').update(JSON.stringify(value)).digest('hex');
const clone = <T>(value: T): T => structuredClone(value);

/** One instance belongs to one OpenCode location. Only opaque recovery IDs are persisted. */
export class DevinRuntime {
  private entries = new Map<string, Promise<Entry>>();
  private disposed = false;
  private active = new Map<string, Entry>();
  private liveEntries = new Set<Entry>();
  private effects = new Set<Promise<unknown>>();
  private lifecycle = new AbortController();
  constructor(private directory: string, private backend: Backend, private defaultMode: Mode = 'local') {}

  private key(scope: NativeAgentScope) { return `native-v1/${fingerprint(scope)}`; }
  private assertActive() { if (this.disposed) throw new NativeFailure('unavailable'); }
  private async write(key: string, value: Journal) {
    this.assertActive();
    const effect = this.backend.write(key, value);
    this.effects.add(effect);
    try { await effect; this.assertActive(); } finally { this.effects.delete(effect); }
  }
  private async emit(event: NativeAgentEvent) {
    this.assertActive();
    const effect = this.backend.emit(event);
    this.effects.add(effect);
    try { await effect; } finally { this.effects.delete(effect); }
  }
  private async bind(config: NativeAgentConfig[], mode: Mode) {
    this.assertActive();
    const effect = this.backend.bind(config, mode, this.lifecycle.signal);
    this.effects.add(effect);
    try { const result = await effect; this.assertActive(); return result; }
    finally { this.effects.delete(effect); }
  }
  private async entry(scope: NativeAgentScope): Promise<Entry> {
    if (this.disposed) throw new NativeFailure('unavailable');
    if (scope.providerID !== 'devin' || scope.directory !== this.directory) throw new NativeFailure('invalid_request');
    const scopedMode = await this.backend.checkScope(scope);
    this.assertActive();
    const key = this.key(scope);
    let pending = this.entries.get(key);
    if (!pending) {
      pending = (async () => {
        const saved = await this.backend.read(key);
        this.assertActive();
        const journal = readJournal(saved);
        if (journal.submission) journal.submission.status = 'outcome-unknown';
        const mode = journal.mode ?? scopedMode ?? this.defaultMode;
        return {
          mode, journal, queue: Promise.resolve(), pending: new Map(), executing: false, liveCalls: new Map(),
          state: {
            protocolVersion: 1, scope: clone(scope), activationID: randomUUID(), revision: 0,
            status: 'unprepared', authentication: this.backend.hasCredential(mode) ? 'unknown' : 'required',
            permissionOwner: 'native',
            capabilities: { authenticate: true, setConfig: true, answer: true, toolProgress: true, prepareAfterSession: true },
            config: journal.config ?? [{ id: 'runtime', label: 'Execution', role: 'other', value: mode,
              choices: [{ id: 'local', label: 'Devin Local' }, { id: 'cloud', label: 'Devin Cloud' }] }],
            requests: [], submission: journal.submission ?? { status: 'none' }, diagnostic: null, execution: null, configComplete: false,
          } satisfies NativeAgentState,
        };
      })().then((entry) => { this.assertActive(); this.liveEntries.add(entry); return entry; });
      this.entries.set(key, pending);
      void pending.catch(() => { if (this.entries.get(key) === pending) this.entries.delete(key); });
    }
    return pending;
  }
  private serial<T>(entry: Entry, fn: () => Promise<T>): Promise<T> {
    const next = entry.queue.then(fn, fn);
    entry.queue = next.catch(() => {});
    return next;
  }
  private async changed(entry: Entry, patch: Partial<NativeAgentState>) {
    this.assertActive();
    const next = { ...entry.state, ...patch };
    if (next.configComplete === true && (next.status === 'ready' || next.status === 'stopped') && entry.handle) {
      try {
        const refreshed = configuration(entry, entry.handle.session);
        next.config = refreshed.config;
        if (refreshed.invalid) {
          next.status = 'error';
          next.diagnostic = { code: 'invalid_config', retryable: false };
        }
      } catch {
        next.config = entry.state.config;
        next.configComplete = false;
        next.status = 'error';
        next.diagnostic = { code: 'unavailable', retryable: true };
      }
    }
    const execution = next.configComplete === true && (next.status === 'ready' || next.status === 'stopped') ? await this.bind(next.config, entry.mode) : null;
    this.assertActive();
    const state = { ...next, execution, revision: entry.state.revision + 1 };
    nativeAgentStateSchema.parse(state);
    entry.state = state;
    // Delivery is live-only. The full snapshot remains available if an observer disconnects.
    await this.emit({ protocolVersion: 1, type: 'stateChanged', state: clone(state) }).catch(() => {});
    this.assertActive();
  }
  private result(entry: Entry, requestID: string): NativeAgentResult {
    return { protocolVersion: 1, requestID, ok: true, state: clone(entry.state) };
  }
  private failure(requestID: string, code: NativeAgentErrorCode, entry?: Entry, uncertain = false): NativeAgentResult {
    const result: Extract<NativeAgentResult, { ok: false }> = {
      protocolVersion: 1, requestID, ok: false, error: { code, outcome: uncertain ? 'unknown' : 'rejected' },
    };
    if (entry) result.state = clone(entry.state);
    return result;
  }
  async call<M extends NativeAgentMethod>(method: M, raw: NativeAgentParameters[M]): Promise<NativeAgentResult> {
    const parsed = nativeAgentParametersSchemas[method].safeParse(raw);
    if (!parsed.success) {
      const id = nativeAgentParametersSchemas.getState.pick({ requestID: true }).safeParse({ requestID: raw?.requestID });
      return this.failure(id.success ? id.data.requestID : 'invalid', 'invalid_request');
    }
    const input = parsed.data;
    const changes = method === 'setConfig' ? nativeAgentParametersSchemas.setConfig.parse(raw).changes : undefined;
    const answer = method === 'answer' ? nativeAgentParametersSchemas.answer.parse(raw).answer : undefined;
    let entry: Entry;
    try { entry = await this.entry(input.scope); }
    catch (error) { return this.failure(input.requestID, error instanceof NativeFailure ? error.code : 'unavailable'); }
    return this.serial(entry, async () => {
      if (this.disposed) return this.failure(input.requestID, 'unavailable');
      if (input.activationID !== null && input.activationID !== entry.state.activationID) return this.failure(input.requestID, 'stale_activation', entry);
      if (method === 'getState') return this.result(entry, input.requestID);
      // A draft has no authoritative session permission owner. Preparation is configuration-only.
      if (input.scope.sessionID === null && method === 'prepare') return this.result(entry, input.requestID);
      const mutation = method !== 'prepare';
      const hash = fingerprint({ method, input });
      const previous = Object.hasOwn(entry.journal.mutations, input.requestID) ? entry.journal.mutations[input.requestID] : undefined;
      if (mutation && previous) {
        if (previous.fingerprint !== hash) return this.failure(input.requestID, 'invalid_request', entry);
        return previous.result ? clone(previous.result) : this.failure(input.requestID, 'outcome_unknown', entry, true);
      }
      if ('expectedRevision' in input && input.expectedRevision !== entry.state.revision) return this.failure(input.requestID, 'stale_revision', entry);
      if (entry.executing && method !== 'answer') return this.failure(input.requestID, 'unavailable', entry);
      let started = false;
      try {
        if (input.scope.sessionID === null && method === 'authenticate') throw new NativeFailure('not_prepared');
        if (method === 'authenticate' && !this.backend.authAllowed()) throw new NativeFailure('policy_denied');
        if (input.scope.sessionID !== null) await this.backend.authorize(input.scope, undefined, entry.mode);
        this.assertActive();
        if (method === 'prepare' || method === 'authenticate') {
          const routeChanged = await this.reconcileUnallocatedRoute(entry);
          // A sign-in click captured the previous displayed configuration. Refresh before another click.
          if (method === 'authenticate' && routeChanged) throw new NativeFailure('stale_revision');
        }
        if (changes && entry.handle) await this.refreshCatalog(entry, true);
        if ('expectedRevision' in input && input.expectedRevision !== entry.state.revision) throw new NativeFailure('stale_revision');
        if (changes && !validateNativeAgentConfig(entry.state.config, changes)) throw new NativeFailure('invalid_config');
        if (answer) {
          const request = entry.state.requests.find((request) => request.id === answer.requestID);
          if (!request) throw new NativeFailure('request_not_found');
          if (!validateNativeAgentAnswer(request, answer)) throw new NativeFailure('invalid_answer');
          if (answer.kind !== 'permission') throw new NativeFailure('unsupported');
          const pending = entry.pending.get(answer.requestID);
          if (!pending) throw new NativeFailure('request_not_found');
          const choice = request.kind === 'permission' && request.choices.find((choice) => answer.kind === 'permission' && choice.id === answer.choiceID);
          if (choice && choice.decision === 'allow') await this.backend.authorize(input.scope, pending.request, entry.mode);
          this.assertActive();
        }
        // Record admission before crossing a process/network boundary. A crash cannot replay a mutation.
        if (mutation) {
          entry.journal.mutations = { ...entry.journal.mutations, [input.requestID]: { fingerprint: hash } };
          await this.write(this.key(input.scope), entry.journal);
        }
        started = true;
        if (method === 'prepare') await this.prepare(entry);
        if (method === 'authenticate') {
          const authentication = await this.backend.authenticate(input.scope, entry.mode);
          this.assertActive();
          if (authentication === 'ready') await this.prepare(entry);
          else await this.changed(entry, { authentication: 'required', diagnostic: { code: 'authentication_required', retryable: true } });
        }
        if (changes) {
          const config = entry.state.config.map((field) => ({ ...field, value: changes.find((change) => change.id === field.id)?.value ?? field.value }));
          const mode = config.find((field) => field.id === 'runtime')?.value;
          if (!isMode(mode)) throw new NativeFailure('invalid_config');
          if (mode !== entry.mode && entry.journal.sessionID && input.scope.sessionID !== null) throw new NativeFailure('invalid_config');
          if (input.scope.sessionID !== null && entry.state.configComplete === true && (entry.state.status === 'ready' || entry.state.status === 'stopped')) await this.bind(config, mode);
          this.assertActive();
          const journal = { ...entry.journal, config: clone(config), mode };
          await this.write(this.key(input.scope), journal);
          entry.journal = journal;
          const modeChanged = entry.mode !== mode;
          entry.mode = mode;
          const update: Partial<NativeAgentState> = { config };
          if (modeChanged) update.authentication = this.backend.hasCredential(mode) ? 'unknown' : 'required';
          await this.changed(entry, update);
        }
        if (answer?.kind === 'permission') {
          const pending = entry.pending.get(answer.requestID)!;
          entry.pending.delete(answer.requestID);
          pending.resolve({ optionId: answer.choiceID });
          await this.changed(entry, { requests: entry.state.requests.filter((request) => request.id !== answer.requestID) });
        }
        const result = this.result(entry, input.requestID);
        if (mutation) {
          entry.journal.mutations = { ...entry.journal.mutations, [input.requestID]: { fingerprint: hash, result } };
          await this.write(this.key(input.scope), entry.journal);
        }
        return result;
      } catch (error) {
        if (this.disposed) return this.failure(input.requestID, 'unavailable', undefined, started);
        const uncertain = error instanceof NativeFailure ? error.uncertain : started;
        const code = error instanceof NativeFailure ? error.code : 'unavailable';
        const result = this.failure(input.requestID, code, entry, uncertain);
        if (mutation && Object.hasOwn(entry.journal.mutations, input.requestID)) {
          entry.journal.mutations = { ...entry.journal.mutations, [input.requestID]: { fingerprint: hash, result } };
          await this.write(this.key(input.scope), entry.journal).catch(() => {});
        }
        return result;
      }
    });
  }
  private async reconcileUnallocatedRoute(entry: Entry) {
    if (entry.journal.sessionID) return false;
    const mode = await this.backend.checkScope(entry.state.scope);
    this.assertActive();
    if (!mode || mode === entry.mode) return false;
    // Saved intent is explicit. A new route must not silently rewrite a captured configuration.
    if (entry.journal.mode !== undefined) throw new NativeFailure('invalid_config');
    entry.mode = mode;
    await this.changed(entry, {
      config: entry.state.config.map((field) => field.id === 'runtime' ? { ...field, value: mode } : field),
      authentication: this.backend.hasCredential(mode) ? 'unknown' : 'required', diagnostic: null, configComplete: false,
    });
    return true;
  }
  private async prepare(entry: Entry) {
    this.assertActive();
    if (entry.handle) { await this.refreshCatalog(entry); return; }
    if (!this.backend.hasCredential(entry.mode)) {
      await this.changed(entry, { authentication: 'required', diagnostic: { code: 'authentication_required', retryable: true } });
      throw new NativeFailure('authentication_required');
    }
    await this.changed(entry, { status: 'preparing', configComplete: false });
    let handle: Awaited<ReturnType<Backend['open']>> | undefined;
    try {
      handle = await this.backend.open(entry.state.scope, entry.mode, entry.journal.sessionID);
      this.assertActive();
      handle.session.onPermission = (request) => this.permission(entry, request);
      entry.journal.sessionID = handle.session.id;
      entry.journal.mode = entry.mode;
      await this.write(this.key(entry.state.scope), entry.journal);
      entry.handle = handle;
      const refreshed = configuration(entry, handle.session);
      if (refreshed.invalid) {
        await this.changed(entry, { status: 'error', authentication: 'authenticated', config: refreshed.config, configComplete: true, execution: null, diagnostic: { code: 'invalid_config', retryable: false } });
        throw new NativeFailure('invalid_config');
      }
      await this.changed(entry, { status: 'ready', authentication: 'authenticated', config: refreshed.config, configComplete: true, diagnostic: null });
    } catch (error) {
      handle?.close();
      entry.handle = undefined;
      if (this.disposed) throw new NativeFailure('unavailable');
      const update: Partial<NativeAgentState> = {
        status: 'error', configComplete: error instanceof NativeFailure && error.code === 'invalid_config' && entry.state.configComplete === true,
      };
      if (error instanceof NativeFailure && error.code === 'authentication_required') update.authentication = 'required';
      update.diagnostic = { code: error instanceof NativeFailure ? error.code : 'unavailable', retryable: true };
      await this.changed(entry, update);
      throw error;
    }
  }
  private async refreshCatalog(entry: Entry, allowCorrection = false) {
    let refreshed: ReturnType<typeof configuration>;
    try { refreshed = configuration(entry, entry.handle!.session); }
    catch {
      await this.changed(entry, { status: 'error', configComplete: false, diagnostic: { code: 'unavailable', retryable: true } });
      throw new NativeFailure('unavailable');
    }
    // Avoid revising an unchanged snapshot merely because a caller rechecks it.
    const recovered = !allowCorrection && entry.state.status === 'error' && !refreshed.invalid;
    if (!entry.state.configComplete || fingerprint(refreshed.config) !== fingerprint(entry.state.config)
      || (refreshed.invalid && entry.state.diagnostic?.code !== 'invalid_config') || recovered) {
      await this.changed(entry, { config: refreshed.config, configComplete: true, authentication: 'authenticated',
        ...(refreshed.invalid ? { status: 'error' as const, diagnostic: { code: 'invalid_config' as const, retryable: false } }
          : recovered ? { status: 'ready' as const, diagnostic: null } : {}) });
    }
    if (refreshed.invalid && !allowCorrection) throw new NativeFailure('invalid_config');
  }
  private async permission(entry: Entry, request: PermissionRequest): Promise<ExactPermissionChoice> {
    if (request.sessionId !== entry.handle?.session.id || entry.pending.size >= 64) return rejectionChoice(request);
    try { await this.backend.authorize(entry.state.scope, request, entry.mode); }
    catch { return rejectionChoice(request); }
    const choices = request.options.filter((option) => option.kind === 'allow_once' || option.kind === 'reject_once')
      .map((option) => ({ id: option.optionId, label: option.name.slice(0, 512), decision: option.kind === 'allow_once' ? 'allow' as const : 'deny' as const }));
    if (!choices.length || choices.length > 16 || new Set(choices.map((choice) => choice.id)).size !== choices.length || this.disposed) return rejectionChoice(request);
    const id = randomUUID();
    const result = new Promise<ExactPermissionChoice>((resolve) => entry.pending.set(id, { request, resolve }));
    await this.serial(entry, () => this.changed(entry, { requests: [...entry.state.requests, {
      id, kind: 'permission', title: (request.call?.title || request.toolCall.title || 'Devin requests permission').slice(0, 512),
      description: '', choices,
    }] }));
    return result;
  }
  async *execute(scope: NativeAgentScope, token: string, prompt: PromptInput, submissionID: string, signal: AbortSignal, route?: string, context?: CapturedContext): AsyncGenerator<SessionEvent, { stopReason: string }, undefined> {
    if (scope.sessionID === null) throw new NativeFailure('not_prepared');
    const captured = await this.backend.resolveBinding(token);
    const { mode } = captured;
    if (route !== undefined && route !== mode) throw new NativeFailure('invalid_config');
    const entry = await this.entry(scope);
    let checkpoint: ContextCheckpoint | undefined;
    let executionPrompt = prompt;
    await this.serial(entry, async () => {
      if (entry.executing || entry.journal.submission) throw new NativeFailure('outcome_unknown');
      await this.backend.authorize(scope, undefined, mode);
      this.assertActive();
      if (entry.journal.sessionID && mode !== entry.mode) throw new NativeFailure('invalid_config');
      entry.mode = mode;
      if (context) {
        if (entry.journal.consumedUsers.includes(context.user)) throw new NativeFailure('invalid_request');
        const transfer = transferContext(context, entry.journal.context, prompt);
        executionPrompt = transfer.prompt;
        checkpoint = transfer.checkpoint;
      }
      await this.prepare(entry);
      assertPromptSupported(executionPrompt, entry.handle?.promptCapabilities);
      const session = entry.handle!.session;
      const validateCaptured = () => {
        if (!validateNativeAgentConfig(entry.state.config, captured.config)) throw new NativeFailure('invalid_config');
      };
      validateCaptured();
      try {
        for (const field of captured.config) {
          if (field.role === 'model' && session.setModel) await session.setModel(field.value);
          if (field.role === 'mode') await session.setMode(field.value);
          this.assertActive();
          await this.refreshCatalog(entry);
          validateCaptured();
        }
      } catch (error) {
        if (error instanceof NativeFailure) throw error;
        const code = error instanceof ModelRejectedError ? 'invalid_config' : 'unavailable';
        if (error instanceof ModelRejectedError) await this.refreshCatalog(entry, true);
        await this.changed(entry, { status: 'error', configComplete: code === 'invalid_config' && entry.state.configComplete === true,
          diagnostic: { code, retryable: false } });
        throw new NativeFailure(code);
      }
      if (signal.aborted) throw new NativeFailure('unavailable');
      entry.journal.submission = { id: submissionID, status: 'pending' };
      await this.write(this.key(scope), entry.journal);
      await this.changed(entry, { submission: clone(entry.journal.submission), status: 'ready' });
      if (!entry.state.configComplete || !validateNativeAgentConfig(entry.state.config, captured.config)) {
        const code = entry.state.configComplete ? 'invalid_config' : 'unavailable';
        delete entry.journal.submission;
        await this.write(this.key(scope), entry.journal);
        await this.changed(entry, { submission: { status: 'none' } });
        throw new NativeFailure(code);
      }
      entry.executing = true;
      if (scope.sessionID) this.active.set(scope.sessionID, entry);
    });
    const session = entry.handle!.session;
    const cancel = () => { session.cancel(); this.rejectPermissions(entry); };
    signal.addEventListener('abort', cancel, { once: true });
    try {
      const stream = session.stream(executionPrompt, { signal, meta: { 'cognition.ai/clientMessageId': submissionID } });
      while (true) {
        const next = await stream.next();
        this.assertActive();
        if (!next.done) {
          if (next.value.type === 'tool_call') {
            const event = next.value;
            if (event.settled) entry.liveCalls.delete(event.call.id);
            else {
              const call = entry.liveCalls.get(event.call.id) ?? { sequence: 0 };
              const text = (toolCallText(event.call) ?? event.call.title ?? '').slice(0, 16_384);
              if (call.text !== text) { call.text = text; entry.liveCalls.set(event.call.id, call); await this.progress(entry, event.call.id); }
            }
          }
          yield next.value; continue;
        }
        await this.serial(entry, async () => {
          delete entry.journal.submission;
          if (checkpoint) {
            entry.journal.context = checkpoint;
            entry.journal.consumedUsers.push(checkpoint.user);
          }
          await this.write(this.key(scope), entry.journal);
          await this.changed(entry, { submission: { status: 'none' }, status: next.value.stopReason === 'cancelled' ? 'stopped' : 'ready' });
        });
        return next.value;
      }
    } catch {
      if (this.disposed) throw new NativeFailure('unavailable', true);
      await this.serial(entry, () => this.retainUnknown(entry, submissionID));
      throw new NativeFailure('outcome_unknown', true);
    } finally {
      signal.removeEventListener('abort', cancel);
      try {
        if (!this.disposed && entry.journal.submission?.status === 'pending') {
          await this.retainUnknown(entry, submissionID);
        }
      } finally {
        entry.executing = false;
        if (scope.sessionID && this.active.get(scope.sessionID) === entry) this.active.delete(scope.sessionID);
        entry.liveCalls.clear();
        this.rejectPermissions(entry);
      }
    }
  }
  private async retainUnknown(entry: Entry, id: string) {
    entry.journal.submission = { id, status: 'outcome-unknown' };
    // The admission record is already durable. Recovery/publication failure cannot
    // turn a possibly executed operation into a definite rejection or unblock retry.
    await this.changed(entry, { submission: clone(entry.journal.submission), status: 'error', diagnostic: { code: 'outcome_unknown', retryable: false } }).catch(() => {});
    await this.write(this.key(entry.state.scope), entry.journal).catch(() => {});
  }
  /** The engine supplies the authoritative message and tool identity after persisting its call. */
  async observeTool(sessionID: string, messageID: string, callID: string, running: boolean, executed: boolean) {
    const entry = this.active.get(sessionID);
    if (!entry) return;
    if (!running || !executed) { entry.liveCalls.delete(callID); return; }
    const call = entry.liveCalls.get(callID);
    // An unrelated provider's tool cannot establish an ACP call in this runtime.
    if (!call) return;
    call.messageID = messageID;
    await this.progress(entry, callID);
  }
  private async progress(entry: Entry, callID: string) {
    const call = entry.liveCalls.get(callID);
    if (!call?.messageID || call.text === undefined || !entry.executing || this.disposed) return;
    await this.emit({ protocolVersion: 1, type: 'toolProgress', scope: clone(entry.state.scope),
      activationID: entry.state.activationID, revision: entry.state.revision, messageID: call.messageID,
      partID: callID, callID, sequence: ++call.sequence, text: call.text }).catch(() => {});
  }
  private rejectPermissions(entry: Entry) {
    for (const pending of entry.pending.values()) pending.resolve(rejectionChoice(pending.request));
    entry.pending.clear();
    if (!this.disposed && entry.state.requests.length) void this.serial(entry, () => this.changed(entry, { requests: [] })).catch(() => {});
  }
  async dispose() {
    this.disposed = true;
    this.lifecycle.abort();
    for (const entry of this.liveEntries) {
      entry.handle?.session.cancel();
      this.rejectPermissions(entry);
      entry.handle?.close();
      entry.handle = undefined;
    }
    // In-flight opens are invalidated and close themselves on arrival. Only already-started
    // persistence/publication effects drain here; a permission wait cannot block unload.
    await Promise.allSettled([...this.effects]);
  }
}

function rejectionChoice(request: PermissionRequest): ExactPermissionChoice {
  const deny = request.options.find((option) => option.kind === 'reject_once');
  if (deny) return { optionId: deny.optionId };
  // The official SDK turns an unmatched exact option into ACP's cancelled outcome.
  let absent = randomUUID();
  while (request.options.some((option) => option.optionId === absent)) absent = randomUUID();
  return { optionId: absent };
}

function readJournal(raw: unknown): Journal {
  if (raw === undefined) return { mutations: {}, consumedUsers: [] };
  if (!raw || typeof raw !== 'object' || !('mutations' in raw) || !raw.mutations || typeof raw.mutations !== 'object') throw new NativeFailure('unavailable');
  // Persisted data has no authority over an activation or live permission requests.
  const mutations = Object.fromEntries(Object.entries(raw.mutations).map(([key, record]): [string, Journal['mutations'][string]] => {
    if (!record || typeof record !== 'object' || !('fingerprint' in record) || typeof record.fingerprint !== 'string') throw new NativeFailure('unavailable');
    if (!('result' in record) || record.result === undefined) return [key, { fingerprint: record.fingerprint }];
    const parsed = nativeAgentResultSchema.safeParse(record.result);
    if (!parsed.success) throw new NativeFailure('unavailable');
    return [key, { fingerprint: record.fingerprint, result: parsed.data }];
  }));
  const mode = 'mode' in raw ? raw.mode : undefined;
  if (mode !== undefined && !isMode(mode)) throw new NativeFailure('unavailable');
  const config = 'config' in raw && raw.config !== undefined ? nativeAgentConfigSchema.safeParse(raw.config) : undefined;
  if (config && !config.success) throw new NativeFailure('unavailable');
  if (config?.success) {
    const runtime = config.data.find((field) => field.id === 'runtime');
    if (!runtime || !isMode(runtime.value) || runtime.choices.some((choice) => !isMode(choice.id))) throw new NativeFailure('unavailable');
  }
  const sessionID = 'sessionID' in raw ? raw.sessionID : undefined;
  if (sessionID !== undefined && typeof sessionID !== 'string') throw new NativeFailure('unavailable');
  const submission = 'submission' in raw ? raw.submission : undefined;
  if (submission !== undefined && !isRecordedSubmission(submission)) throw new NativeFailure('unavailable');
  const context = 'context' in raw && raw.context !== undefined ? readContextCheckpoint(raw.context) : undefined;
  let consumedUsers = 'consumedUsers' in raw ? raw.consumedUsers : undefined;
  // Older context-only journals cannot prove which earlier users were consumed.
  if (consumedUsers === undefined) {
    if (context) throw new NativeFailure('unavailable');
    consumedUsers = [];
  }
  if (!Array.isArray(consumedUsers) || !consumedUsers.every((value): value is string => typeof value === 'string' && /^[a-f0-9]{64}$/.test(value))
    || new Set(consumedUsers).size !== consumedUsers.length || (context && !consumedUsers.includes(context.user))) throw new NativeFailure('unavailable');
  const journal: Journal = { ...raw, mutations, consumedUsers };
  if (mode !== undefined) journal.mode = mode;
  if (config?.success) journal.config = config.data;
  if (sessionID !== undefined) journal.sessionID = sessionID;
  if (submission !== undefined) journal.submission = submission;
  if (context !== undefined) journal.context = context;
  return clone(journal);
}
function configuration(entry: Entry, session: SessionHandle) {
  const runtime = { ...entry.state.config[0], value: entry.mode };
  const fields: NativeAgentConfig[] = [runtime];
  let invalid = false;
  if (!Array.isArray(session.configOptions)) throw new NativeFailure('unavailable');
  for (const option of session.configOptions) {
    if (!option || typeof option !== 'object') throw new NativeFailure('unavailable');
    const id = option.configId ?? option.id;
    const supported = (value: SessionConfigOption['id']) => value === 'mode' || (value === 'model' && !!session.setModel);
    if ((supported(option.id) || supported(option.configId)) && option.id !== undefined && option.configId !== undefined && option.id !== option.configId) throw new NativeFailure('unavailable');
    if (id !== 'mode' && !(id === 'model' && session.setModel)) continue;
    if (fields.some((field) => field.id === id) || typeof option.currentValue !== 'string'
      || !Array.isArray(option.options) || !option.options.length
      || (option.name !== undefined && (typeof option.name !== 'string' || !option.name.length))) throw new NativeFailure('unavailable');
    const choices = option.options.map((choice) => {
      if (!choice || typeof choice !== 'object' || typeof choice.value !== 'string'
        || 'options' in choice || 'group' in choice
        || (choice.name !== undefined && (typeof choice.name !== 'string' || !choice.name.length))) throw new NativeFailure('unavailable');
      return { id: choice.value, label: choice.name ?? choice.value };
    });
    if (!choices.some((choice) => choice.id === option.currentValue)) throw new NativeFailure('unavailable');
    const persisted = entry.journal.config?.find((field) => field.id === id);
    const valid = persisted && choices.some((choice) => choice.id === persisted.value);
    if (persisted && !valid) invalid = true;
    fields.push({ id, role: id === 'model' ? 'model' : 'mode', label: option.name ?? id, value: valid ? persisted.value : option.currentValue, choices });
  }
  if (!nativeAgentConfigSchema.safeParse(fields).success) throw new NativeFailure('unavailable');
  if (entry.journal.config?.some((field) => field.id !== 'runtime' && !fields.some((next) => next.id === field.id))) invalid = true;
  return { config: fields, invalid };
}
