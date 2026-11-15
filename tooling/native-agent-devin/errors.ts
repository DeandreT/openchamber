import type { NativeAgentErrorCode } from '../../packages/sdk/src/native-agent.ts';

/** Only protocol allowlisted codes can cross the provider diagnostic boundary. */
export class NativeFailure extends Error {
  constructor(readonly code: NativeAgentErrorCode, readonly uncertain = false) { super(code); }
}
