import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';
import { z } from 'zod';

import { nativeAgentAnswerSchema, nativeAgentCapabilitySchema, nativeAgentConfigSchema, nativeAgentEventSchema, nativeAgentParametersSchemas, nativeAgentRecordedSubmissionSchema, nativeAgentResultSchema, nativeAgentScopeSchema, nativeAgentStateSchema } from './native-agent-schemas.ts';
import { NATIVE_AGENT_METHODS, nativeAgentRpcID, sameNativeAgentScope, validateNativeAgentAnswer, validateNativeAgentConfig } from './native-agent.ts';

const fixture = z.object({
  protocolVersion: z.literal(1),
  providers: z.array(z.object({ name: z.string(), discovery: nativeAgentCapabilitySchema, state: nativeAgentStateSchema })).min(2),
  sessionFirstDraft: nativeAgentStateSchema,
  parameters: z.object(nativeAgentParametersSchemas),
  answers: z.array(nativeAgentAnswerSchema),
  progress: nativeAgentEventSchema,
  failure: nativeAgentResultSchema,
}).parse(JSON.parse(readFileSync(new URL('../fixtures/native-agent-v1.json', import.meta.url), 'utf8')));
const [devin, synthetic] = fixture.providers;
if (!devin || !synthetic) throw new Error('Both protocol providers are required');

describe('native-agent protocol v1', () => {
  test('session-first preparation is explicit, boolean and config-only before creation', () => {
    expect(fixture.sessionFirstDraft.capabilities.prepareAfterSession).toBe(true);
    for (const prepareAfterSession of [undefined, false, true]) {
      const state = { ...fixture.sessionFirstDraft, capabilities: { ...fixture.sessionFirstDraft.capabilities, prepareAfterSession } };
      expect(nativeAgentStateSchema.safeParse(state).success).toBe(true);
    }
    for (const prepareAfterSession of [null, 0, 1, 'true', {}]) {
      expect(nativeAgentStateSchema.safeParse({ ...fixture.sessionFirstDraft, capabilities: { ...fixture.sessionFirstDraft.capabilities, prepareAfterSession } }).success).toBe(false);
    }
    for (const changed of [{ status: 'ready' }, { execution: { providerID: 'session-first-native', modelID: 'route', variant: 'invented' } }, { submission: { status: 'pending', id: 'message' } }]) {
      expect(nativeAgentStateSchema.safeParse({ ...fixture.sessionFirstDraft, ...changed }).success).toBe(false);
    }
    expect(nativeAgentStateSchema.safeParse({ ...synthetic.state, capabilities: { ...synthetic.state.capabilities, prepareAfterSession: true }, configComplete: true }).success).toBe(true);
  });

  test('config completeness is explicit and never inferred from success, readiness or authentication', () => {
    const state = { ...synthetic.state, execution: null, capabilities: { ...synthetic.state.capabilities, prepareAfterSession: true } };
    for (const configComplete of [undefined, false, true]) {
      const parsed = nativeAgentStateSchema.parse({ ...state, configComplete });
      expect(parsed.configComplete ?? false).toBe(configComplete ?? false);
      const reply = nativeAgentResultSchema.parse({ protocolVersion: 1, requestID: 'catalog', ok: true, state: { ...state, configComplete } });
      expect(reply.ok && (reply.state.configComplete ?? false)).toBe(configComplete ?? false);
    }
    for (const configComplete of [null, 0, 'true', {}]) {
      expect(nativeAgentStateSchema.safeParse({ ...state, configComplete }).success).toBe(false);
    }
    expect(nativeAgentStateSchema.safeParse({ ...state, execution: synthetic.state.execution }).success).toBe(false);
    expect(nativeAgentStateSchema.safeParse({ ...fixture.sessionFirstDraft, configComplete: true }).success).toBe(false);
    expect(nativeAgentStateSchema.safeParse(synthetic.state).success).toBe(true);
  });
  test('both providers round-trip through the same state, event and result schemas', () => {
    for (const { state } of fixture.providers) {
      expect(nativeAgentStateSchema.parse(JSON.parse(JSON.stringify(state)))).toEqual(state);
      const result = { protocolVersion: 1, requestID: 'read-fixture', ok: true, state };
      expect(nativeAgentResultSchema.parse(result)).toEqual(result);
      const event = { protocolVersion: 1, type: 'stateChanged', state };
      expect(nativeAgentEventSchema.parse(event)).toEqual(event);
      expect(nativeAgentRpcID(state.scope.providerID)).toBe(`openchamber.native-agent.${state.scope.providerID}.v1`);
    }
    expect(nativeAgentEventSchema.parse(fixture.progress)).toEqual(fixture.progress);
    expect(nativeAgentResultSchema.parse(fixture.failure)).toEqual(fixture.failure);
  });

  test('every method has an exact parameter fixture', () => {
    expect(Object.keys(fixture.parameters)).toEqual([...NATIVE_AGENT_METHODS]);
    for (const method of NATIVE_AGENT_METHODS) {
      const parameters = fixture.parameters[method];
      expect(nativeAgentParametersSchemas[method].parse(JSON.parse(JSON.stringify(parameters)))).toEqual(parameters);
      expect(nativeAgentParametersSchemas[method].safeParse({ ...parameters, secret: 'must-not-be-forwarded' }).success).toBe(false);
      expect(nativeAgentParametersSchemas[method].safeParse({ ...parameters, protocolVersion: 2 }).success).toBe(false);
    }
  });

  test('discovery requires exactly version 1 without probing an ordinary provider', () => {
    for (const value of [null, {}, { protocolVersion: 0 }, { protocolVersion: '1' }, { protocolVersion: 2 }]) {
      expect(nativeAgentCapabilitySchema.safeParse(value).success).toBe(false);
    }
    expect(nativeAgentCapabilitySchema.parse({ protocolVersion: 1 })).toEqual({ protocolVersion: 1 });
  });

  test('preparation carries the actual draft directory and accepts no authentication input', () => {
    expect(fixture.parameters.prepare.scope).toEqual(devin.state.scope);
    expect(fixture.parameters.prepare.scope.sessionID).toBeNull();
    expect(nativeAgentParametersSchemas.prepare.safeParse({ ...fixture.parameters.prepare, userInitiated: true }).success).toBe(false);
    for (const directory of ['', '.', 'relative/project', 'C:relative', '/work\0bad']) {
      expect(nativeAgentScopeSchema.safeParse({ ...devin.state.scope, directory }).success).toBe(false);
    }
    for (const directory of ['/work/project', 'C:\\work\\project', '\\\\server\\share\\project']) {
      expect(nativeAgentScopeSchema.safeParse({ ...devin.state.scope, directory }).success).toBe(true);
    }
  });

  test('authentication is explicit and mutations require an activation and safe revision', () => {
    expect(nativeAgentParametersSchemas.authenticate.safeParse({ ...fixture.parameters.authenticate, userInitiated: false }).success).toBe(false);
    const { userInitiated: _omitted, ...withoutAction } = fixture.parameters.authenticate;
    expect(nativeAgentParametersSchemas.authenticate.safeParse(withoutAction).success).toBe(false);
    for (const method of ['authenticate', 'setConfig', 'answer'] as const) {
      const parameters = fixture.parameters[method];
      expect(nativeAgentParametersSchemas[method].safeParse({ ...parameters, activationID: null }).success).toBe(false);
      for (const expectedRevision of [-1, 0.1, Number.MAX_SAFE_INTEGER + 1, Number.NaN]) {
        expect(nativeAgentParametersSchemas[method].safeParse({ ...parameters, expectedRevision }).success).toBe(false);
      }
    }
  });

  test('scope equality includes directory, provider and nullable session identity', () => {
    const scope = devin.state.scope;
    for (let code = 0; code <= 31; code++) {
      expect(nativeAgentScopeSchema.safeParse({ ...scope, providerID: `provider${String.fromCharCode(code)}` }).success).toBe(false);
    }
    expect(nativeAgentScopeSchema.safeParse({ ...scope, providerID: `provider${String.fromCharCode(127)}` }).success).toBe(false);
    expect(nativeAgentScopeSchema.safeParse({ ...scope, providerID: `provider${String.fromCharCode(128)}` }).success).toBe(true);
    expect(sameNativeAgentScope(scope, { ...scope })).toBe(true);
    for (const changed of [{ directory: '/different' }, { providerID: 'other' }, { sessionID: 'created-session' }]) {
      expect(sameNativeAgentScope(scope, { ...scope, ...changed })).toBe(false);
    }
  });

  test('validation leaves source objects untouched and rejects unknown nested security fields', () => {
    const source = structuredClone(synthetic.state);
    const before = JSON.stringify(source);
    const container = z.union([z.record(z.string(), z.unknown()), z.array(z.unknown())]);
    const freeze = (value: z.output<typeof container>): void => {
      Object.freeze(value);
      for (const child of Object.values(value)) if (container.safeParse(child).success) freeze(child);
    };
    freeze(source);
    expect(nativeAgentStateSchema.parse(source)).toEqual(source);
    expect(JSON.stringify(source)).toBe(before);
    for (const changed of [
      { scope: { ...source.scope, serverURL: 'https://unexpected.example' } },
      { capabilities: { ...source.capabilities, bypassPolicy: true } },
      { requests: [{ ...source.requests[0], permissionOwner: 'opencode' }] },
      { submission: { status: 'none', retry: true } },
    ]) {
      expect(nativeAgentStateSchema.safeParse({ ...source, ...changed }).success).toBe(false);
    }
    expect(nativeAgentParametersSchemas.answer.safeParse({ ...fixture.parameters.answer, answer: { kind: 'permission', requestID: 'permission-fixture', choiceID: 'yes-once', always: true } }).success).toBe(false);
    expect(JSON.stringify(source)).toBe(before);
  });

  test('stopped state preserves all advertised config choices and opaque values', () => {
    expect(devin.state.status).toBe('stopped');
    expect(devin.state.config.find((field) => field.role === 'model')?.choices).toHaveLength(2);
    expect(validateNativeAgentConfig(devin.state.config, fixture.parameters.setConfig.changes)).toBe(true);
    expect(validateNativeAgentConfig(synthetic.state.config, [{ id: 'flavour', value: 'quick' }])).toBe(true);
    for (const changes of [[], [{ id: 'runtime-model', value: 'unknown' }], [{ id: 'unknown', value: 'opaque-model-a' }], [{ id: 'runtime-model', value: 'opaque-model-a' }, { id: 'runtime-model', value: 'opaque-model-b' }]]) {
      expect(validateNativeAgentConfig(devin.state.config, changes)).toBe(false);
    }
    const field = devin.state.config[0];
    if (!field) throw new Error('Model config required');
    expect(nativeAgentConfigSchema.parse(devin.state.config)).toEqual(devin.state.config);
    for (const config of [[{ ...field, value: 'missing' }], [field, field], [{ ...field, choices: [field.choices[0], field.choices[0]] }]]) {
      expect(nativeAgentConfigSchema.safeParse(config).success).toBe(false);
      expect(nativeAgentStateSchema.safeParse({ ...devin.state, config }).success).toBe(false);
    }
    const maximum = Array.from({ length: 32 }, (_, index) => ({ ...field, id: `model-${index}` }));
    expect(nativeAgentConfigSchema.safeParse(maximum).success).toBe(true);
    expect(nativeAgentConfigSchema.safeParse([...maximum, { ...field, id: 'model-32' }]).success).toBe(false);
  });

  test('OpenCode permission ownership cannot carry native requests', () => {
    expect(nativeAgentStateSchema.safeParse({ ...synthetic.state, permissionOwner: 'opencode' }).success).toBe(false);
    expect(nativeAgentStateSchema.safeParse({ ...synthetic.state, permissionOwner: 'opencode', requests: [] }).success).toBe(true);
  });

  test('execution bindings use opaque routing variants and require the scoped provider', () => {
    expect(devin.state.execution).toEqual({ providerID: 'devin', modelID: 'local', variant: 'immutable-config-3' });
    expect(devin.state.execution?.modelID).not.toBe(devin.state.config.find((field) => field.role === 'model')?.value);
    expect(nativeAgentStateSchema.parse({ ...devin.state, execution: null }).execution).toBeNull();
    const { execution: _omitted, ...withoutBinding } = devin.state;
    expect(nativeAgentStateSchema.safeParse(withoutBinding).success).toBe(false);
    for (const execution of [
      { providerID: 'another-provider', modelID: 'local', variant: 'immutable-config-3' },
      { providerID: 'devin', modelID: 'local' },
      { providerID: 'devin', modelID: 'local', variant: '' },
      { providerID: 'devin', modelID: 'local', variant: 'immutable-config-3', nativeConfigToken: 'private-registry-key' },
      { providerID: 'devin', modelID: 'local', variant: 'immutable-config-3', apiKey: 'credentials-are-forbidden' },
    ]) {
      expect(nativeAgentStateSchema.safeParse({ ...devin.state, execution }).success).toBe(false);
    }
    const prior = structuredClone(devin.state);
    const next = { ...prior, revision: prior.revision + 1, execution: { providerID: 'devin', modelID: 'local', variant: 'immutable-config-4' } };
    nativeAgentStateSchema.parse(next);
    expect(prior.execution?.variant).toBe('immutable-config-3');
  });

  test('answers preserve offered opaque IDs and validate against the current request', () => {
    for (const answer of fixture.answers) {
      const request = synthetic.state.requests.find((entry) => entry.id === answer.requestID);
      if (!request) throw new Error('Answer request missing');
      expect(validateNativeAgentAnswer(request, answer)).toBe(true);
      expect(validateNativeAgentAnswer(request, { ...answer, requestID: 'stale-request' })).toBe(false);
    }
    const permission = synthetic.state.requests.find((entry) => entry.kind === 'permission');
    const question = synthetic.state.requests.find((entry) => entry.kind === 'question');
    if (!permission || !question) throw new Error('Both request kinds required');
    expect(validateNativeAgentAnswer(permission, { kind: 'permission', requestID: permission.id, choiceID: 'invented-allow' })).toBe(false);
    expect(validateNativeAgentAnswer(permission, { kind: 'question', requestID: permission.id, values: [] })).toBe(false);
    for (const values of [
      [],
      [{ fieldID: 'target', choiceIDs: [] }],
      [{ fieldID: 'target', choiceIDs: ['unknown'] }],
      [{ fieldID: 'target', choiceIDs: ['test', 'preview'] }],
      [{ fieldID: 'target', choiceIDs: ['test', 'test'] }],
      [{ fieldID: 'target', choiceIDs: ['test'], text: 'not allowed' }],
      [{ fieldID: 'target', choiceIDs: ['test'] }, { fieldID: 'target', choiceIDs: ['preview'] }],
      [{ fieldID: 'target', choiceIDs: ['test'] }, { fieldID: 'unknown', choiceIDs: [] }],
    ]) {
      expect(validateNativeAgentAnswer(question, { kind: 'question', requestID: question.id, values })).toBe(false);
    }
    const freeText = { id: 'text-only', kind: 'question' as const, title: 'Text', description: '', fields: [{ id: 'text', label: 'Text', required: true, allowText: true, multiple: false, choices: [] }] };
    expect(validateNativeAgentAnswer(freeText, { kind: 'question', requestID: 'text-only', values: [{ fieldID: 'text', choiceIDs: [], text: '   ' }] })).toBe(false);
    expect(validateNativeAgentAnswer(freeText, { kind: 'question', requestID: 'text-only', values: [{ fieldID: 'text', choiceIDs: [], text: 'A real answer' }] })).toBe(true);
  });

  test('diagnostics refuse raw errors and uncertain submissions survive serialization', () => {
    expect(nativeAgentStateSchema.parse(JSON.parse(JSON.stringify(synthetic.state))).submission).toEqual({ status: 'outcome-unknown', id: 'submission-fixture' });
    for (const status of ['pending', 'outcome-unknown']) {
      expect(nativeAgentRecordedSubmissionSchema.safeParse({ status, id: 'submission-fixture' }).success).toBe(true);
    }
    expect(nativeAgentRecordedSubmissionSchema.safeParse({ status: 'pending', id: 'x'.repeat(256) }).success).toBe(true);
    expect(nativeAgentRecordedSubmissionSchema.safeParse({ status: 'none' }).success).toBe(false);
    expect(nativeAgentStateSchema.safeParse({ ...synthetic.state, submission: { status: 'none' } }).success).toBe(true);
    for (const submission of [{ status: 'pending', id: '' }, { status: 'pending', id: 'x'.repeat(257) },
      { status: 'pending', id: '\n' }, { status: 'pending', id: 'valid', extra: true }]) {
      expect(nativeAgentRecordedSubmissionSchema.safeParse(submission).success).toBe(false);
      expect(nativeAgentStateSchema.safeParse({ ...synthetic.state, submission }).success).toBe(false);
    }
    for (const diagnostic of [{ code: 'unavailable', retryable: true, message: 'raw command output' }, { code: 'exception details', retryable: false }]) {
      expect(nativeAgentStateSchema.safeParse({ ...devin.state, diagnostic }).success).toBe(false);
    }
    expect(nativeAgentResultSchema.safeParse({ protocolVersion: 1, requestID: 'request', ok: false, error: { code: 'unavailable', outcome: 'rejected', message: 'raw response body' } }).success).toBe(false);
    expect(nativeAgentResultSchema.safeParse({ protocolVersion: 1, requestID: 'request', ok: true }).success).toBe(false);
  });

  test('progress needs full tool correlation and bounded non-final text', () => {
    if (fixture.progress.type !== 'toolProgress') throw new Error('Tool fixture required');
    const progress = fixture.progress;
    for (const changes of [{ messageID: '' }, { partID: '' }, { callID: '' }, { activationID: '' }, { sequence: -1 }, { text: 'x'.repeat(16_385) }, { final: true }]) {
      expect(nativeAgentEventSchema.safeParse({ ...progress, ...changes }).success).toBe(false);
    }
    expect(nativeAgentStateSchema.safeParse({ ...devin.state, revision: Number.MAX_SAFE_INTEGER + 1 }).success).toBe(false);
    expect(nativeAgentStateSchema.safeParse({ ...devin.state, requests: Array(65).fill(synthetic.state.requests[0]) }).success).toBe(false);
  });
});
