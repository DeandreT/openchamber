import { z } from 'zod';
import { NATIVE_AGENT_ERROR_CODES, type NativeAgentEvent, type NativeAgentParameters, type NativeAgentResult, type NativeAgentState } from './native-agent.ts';

const id = z.string().min(1).max(256).refine((value) => {
  for (let index = 0; index < value.length; index++) {
    const code = value.charCodeAt(index);
    if (code <= 31 || code === 127) return false;
  }
  return true;
}, 'control characters are forbidden');
const label = z.string().min(1).max(512);
const revision = z.number().int().nonnegative().max(Number.MAX_SAFE_INTEGER);
const uniqueIDs = (values: { id: string }[]): boolean => new Set(values.map((value) => value.id)).size === values.length;
export const nativeAgentScopeSchema = z.object({
  directory: z.string().min(1).max(4096).refine((value) => /^(?:\/|[A-Za-z]:[\\/]|\\\\)/.test(value) && !value.includes('\0'), 'absolute working directory required'),
  providerID: id,
  sessionID: id.nullable(),
}).strict();
export const nativeAgentCapabilitySchema = z.object({ protocolVersion: z.literal(1) }).strict();
const choices = z.array(z.object({ id, label }).strict()).max(256).refine(uniqueIDs, 'duplicate choice IDs');
const config = z.object({ id, label, role: z.enum(['model', 'effort', 'mode', 'other']), value: id, choices: choices.refine((values) => values.length > 0) }).strict()
  .refine((field) => field.choices.some((choice) => choice.id === field.value), 'value must be an advertised choice');
export const nativeAgentConfigSchema = z.array(config).max(32).refine(uniqueIDs, 'duplicate config IDs');
const questionField = z.object({ id, label, required: z.boolean(), multiple: z.boolean(), allowText: z.boolean(), choices }).strict()
  .refine((field) => field.allowText || field.choices.length > 0, 'question needs choices or text');
const pendingRequest = z.discriminatedUnion('kind', [
  z.object({ id, title: label, description: z.string().max(16_384), kind: z.literal('permission'), choices: z.array(z.object({ id, label, decision: z.enum(['allow', 'deny']) }).strict()).min(1).max(16).refine(uniqueIDs, 'duplicate choice IDs') }).strict(),
  z.object({ id, title: label, description: z.string().max(16_384), kind: z.literal('question'), fields: z.array(questionField).min(1).max(32).refine(uniqueIDs, 'duplicate field IDs') }).strict(),
]);
export const nativeAgentAnswerSchema = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('permission'), requestID: id, choiceID: id }).strict(),
  z.object({ kind: z.literal('question'), requestID: id, values: z.array(z.object({ fieldID: id, choiceIDs: z.array(id).max(256), text: z.string().max(32_000).optional() }).strict()).max(32) }).strict(),
]);
export const nativeAgentRecordedSubmissionSchema = z.object({ status: z.enum(['pending', 'outcome-unknown']), id }).strict();
export const nativeAgentStateSchema = z.object({
  protocolVersion: z.literal(1), scope: nativeAgentScopeSchema, activationID: id, revision,
  status: z.enum(['unprepared', 'preparing', 'ready', 'stopped', 'error']),
  authentication: z.enum(['unknown', 'required', 'authenticated']), permissionOwner: z.enum(['opencode', 'native']),
  capabilities: z.object({ authenticate: z.boolean(), setConfig: z.boolean(), answer: z.boolean(), toolProgress: z.boolean(), prepareAfterSession: z.boolean().optional() }).strict(),
  configComplete: z.boolean().optional(),
  execution: z.object({ providerID: id, modelID: id, variant: id }).strict().nullable(),
  config: nativeAgentConfigSchema,
  requests: z.array(pendingRequest).max(64).refine(uniqueIDs, 'duplicate request IDs'),
  submission: z.discriminatedUnion('status', [z.object({ status: z.literal('none') }).strict(), nativeAgentRecordedSubmissionSchema]),
  diagnostic: z.object({ code: z.enum(NATIVE_AGENT_ERROR_CODES), retryable: z.boolean() }).strict().nullable(),
}).strict()
  .refine((state) => state.permissionOwner === 'native' || state.requests.length === 0, 'OpenCode owns its own requests')
  .refine((state) => state.execution === null || state.execution.providerID === state.scope.providerID, 'execution provider must match scope')
  .refine((state) => !state.capabilities.prepareAfterSession || state.scope.sessionID !== null || (state.status === 'unprepared' && state.execution === null && state.requests.length === 0 && state.submission.status === 'none' && !state.configComplete), 'session-first draft is config-only')
  .refine((state) => !state.capabilities.prepareAfterSession || state.execution === null || state.configComplete === true, 'session-first execution requires complete config');
const read = z.object({ protocolVersion: z.literal(1), requestID: id, scope: nativeAgentScopeSchema, activationID: id.nullable() }).strict();
const mutation = read.extend({ activationID: id, expectedRevision: revision });
export const nativeAgentParametersSchemas = {
  getState: read,
  prepare: read,
  authenticate: mutation.extend({ userInitiated: z.literal(true) }),
  setConfig: mutation.extend({ changes: z.array(z.object({ id, value: id }).strict()).min(1).max(32).refine(uniqueIDs, 'duplicate config IDs') }),
  answer: mutation.extend({ answer: nativeAgentAnswerSchema }),
};
export const nativeAgentResultSchema = z.discriminatedUnion('ok', [
  z.object({ protocolVersion: z.literal(1), requestID: id, ok: z.literal(true), state: nativeAgentStateSchema }).strict(),
  z.object({ protocolVersion: z.literal(1), requestID: id, ok: z.literal(false), error: z.object({ code: z.enum(NATIVE_AGENT_ERROR_CODES), outcome: z.enum(['rejected', 'unknown']) }).strict(), state: nativeAgentStateSchema.optional() }).strict(),
]);
export const nativeAgentEventSchema = z.discriminatedUnion('type', [
  z.object({ protocolVersion: z.literal(1), type: z.literal('stateChanged'), state: nativeAgentStateSchema }).strict(),
  z.object({ protocolVersion: z.literal(1), type: z.literal('toolProgress'), scope: nativeAgentScopeSchema, activationID: id, revision, messageID: id, partID: id, callID: id, sequence: revision, text: z.string().max(16_384) }).strict(),
]);

// Both directions keep the schema and the zod-free public contract in sync.
const stateParsed: (value: z.output<typeof nativeAgentStateSchema>) => NativeAgentState = (value) => value;
const stateContract: (value: NativeAgentState) => z.output<typeof nativeAgentStateSchema> = (value) => value;
const resultParsed: (value: z.output<typeof nativeAgentResultSchema>) => NativeAgentResult = (value) => value;
const resultContract: (value: NativeAgentResult) => z.output<typeof nativeAgentResultSchema> = (value) => value;
const eventParsed: (value: z.output<typeof nativeAgentEventSchema>) => NativeAgentEvent = (value) => value;
const eventContract: (value: NativeAgentEvent) => z.output<typeof nativeAgentEventSchema> = (value) => value;
type ParsedParameters = { [K in keyof NativeAgentParameters]: z.output<(typeof nativeAgentParametersSchemas)[K]> };
const parametersParsed: (value: ParsedParameters) => NativeAgentParameters = (value) => value;
const parametersContract: (value: NativeAgentParameters) => ParsedParameters = (value) => value;
void stateParsed; void stateContract; void resultParsed; void resultContract;
void eventParsed; void eventContract; void parametersParsed; void parametersContract;
