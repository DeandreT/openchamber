/** OpenCode native-agent RPC v1. No provider implementation or credentials live here. */
export const NATIVE_AGENT_PROTOCOL_VERSION = 1;
export const NATIVE_AGENT_METHODS = ['getState', 'prepare', 'authenticate', 'setConfig', 'answer'] as const;
export type NativeAgentMethod = (typeof NATIVE_AGENT_METHODS)[number];
export const NATIVE_AGENT_ERROR_CODES = ['invalid_request', 'stale_activation', 'stale_revision', 'not_prepared', 'authentication_required', 'unsupported', 'policy_denied', 'request_not_found', 'invalid_answer', 'invalid_config', 'unavailable', 'outcome_unknown'] as const;
export type NativeAgentErrorCode = (typeof NATIVE_AGENT_ERROR_CODES)[number];

export const nativeAgentRpcID = (providerID: string): string => `openchamber.native-agent.${providerID}.v1`;

/** Server identity belongs to the captured transport, never an address supplied by a plugin. */
export type NativeAgentScope = { directory: string; providerID: string; sessionID: string | null };
export type NativeAgentCapabilities = {
  authenticate: boolean; setConfig: boolean; answer: boolean; toolProgress: boolean;
  /** Absent/false keeps draft preparation. True makes draft prepare config-only;
   * execution preparation requires an existing OpenCode session and provider.use. */
  prepareAfterSession?: boolean;
};
/** Immutable routing binding published by the plugin through OpenCode's model variant catalog. */
export type NativeAgentExecution = { providerID: string; modelID: string; variant: string };
export type NativeAgentConfig = {
  id: string;
  label: string;
  role: 'model' | 'effort' | 'mode' | 'other';
  value: string;
  choices: { id: string; label: string }[];
};
export type NativeAgentQuestionField = {
  id: string;
  label: string;
  required: boolean;
  multiple: boolean;
  allowText: boolean;
  choices: { id: string; label: string }[];
};
export type NativeAgentPendingRequest = {
  id: string;
  title: string;
  description: string;
} & (
  | { kind: 'permission'; choices: { id: string; label: string; decision: 'allow' | 'deny' }[] }
  | { kind: 'question'; fields: NativeAgentQuestionField[] }
);
export type NativeAgentAnswer =
  | { kind: 'permission'; requestID: string; choiceID: string }
  | { kind: 'question'; requestID: string; values: { fieldID: string; choiceIDs: string[]; text?: string }[] };

/** A complete snapshot. Revisions are safe integers, monotonic within one activation. */
export type NativeAgentState = {
  protocolVersion: 1;
  scope: NativeAgentScope;
  activationID: string;
  revision: number;
  status: 'unprepared' | 'preparing' | 'ready' | 'stopped' | 'error';
  authentication: 'unknown' | 'required' | 'authenticated';
  permissionOwner: 'opencode' | 'native';
  capabilities: NativeAgentCapabilities;
  /** True only for a complete validated current exposed config catalog.
   * Session-first providers treat absent/false as partial or unreported. */
  configComplete?: boolean;
  /** Null until the credential-free snapshot is durable and its variant is published. */
  execution: NativeAgentExecution | null;
  config: NativeAgentConfig[];
  requests: NativeAgentPendingRequest[];
  submission: { status: 'none' } | { status: 'pending' | 'outcome-unknown'; id: string };
  /** Allowlisted codes only. Never forward exception text, paths, tokens or subprocess output. */
  diagnostic: { code: NativeAgentErrorCode; retryable: boolean } | null;
};

type ReadParameters = { protocolVersion: 1; requestID: string; scope: NativeAgentScope; activationID: string | null };
type MutationParameters = Omit<ReadParameters, 'activationID'> & { activationID: string; expectedRevision: number };
export type NativeAgentParameters = {
  getState: ReadParameters;
  prepare: ReadParameters;
  authenticate: MutationParameters & { userInitiated: true };
  setConfig: MutationParameters & { changes: { id: string; value: string }[] };
  answer: MutationParameters & { answer: NativeAgentAnswer };
};
export type NativeAgentResult =
  | { protocolVersion: 1; requestID: string; ok: true; state: NativeAgentState }
  | { protocolVersion: 1; requestID: string; ok: false; error: { code: NativeAgentErrorCode; outcome: 'rejected' | 'unknown' }; state?: NativeAgentState };

export type NativeAgentToolProgress = {
  protocolVersion: 1;
  type: 'toolProgress';
  scope: NativeAgentScope;
  activationID: string;
  revision: number;
  messageID: string;
  partID: string;
  callID: string;
  sequence: number;
  text: string;
};
export type NativeAgentEvent = { protocolVersion: 1; type: 'stateChanged'; state: NativeAgentState } | NativeAgentToolProgress;

export const sameNativeAgentScope = (a: NativeAgentScope, b: NativeAgentScope): boolean => (
  a.directory === b.directory && a.providerID === b.providerID && a.sessionID === b.sessionID
);

/** Contextual validation after schema parsing, also required inside the plugin before execution. */
export const validateNativeAgentAnswer = (request: NativeAgentPendingRequest, answer: NativeAgentAnswer): boolean => {
  if (request.id !== answer.requestID || request.kind !== answer.kind) return false;
  if (request.kind === 'permission' && answer.kind === 'permission') {
    return request.choices.some((choice) => choice.id === answer.choiceID);
  }
  if (request.kind !== 'question' || answer.kind !== 'question') return false;
  if (new Set(answer.values.map((value) => value.fieldID)).size !== answer.values.length) return false;
  if (answer.values.some((value) => !request.fields.some((field) => field.id === value.fieldID))) return false;
  return request.fields.every((field) => {
    const value = answer.values.find((entry) => entry.fieldID === field.id);
    if (!value) return !field.required;
    if (new Set(value.choiceIDs).size !== value.choiceIDs.length) return false;
    if (!field.multiple && value.choiceIDs.length > 1) return false;
    if (!field.allowText && value.text !== undefined) return false;
    if (value.choiceIDs.some((id) => !field.choices.some((choice) => choice.id === id))) return false;
    return !field.required || value.choiceIDs.length > 0 || Boolean(value.text?.trim());
  });
};

/** Validate the entire change atomically before the plugin commits any value. */
export const validateNativeAgentConfig = (config: NativeAgentConfig[], changes: { id: string; value: string }[]): boolean => (
  changes.length > 0 && new Set(changes.map((change) => change.id)).size === changes.length
  && changes.every((change) => config.some((field) => field.id === change.id && field.choices.some((choice) => choice.id === change.value)))
);
