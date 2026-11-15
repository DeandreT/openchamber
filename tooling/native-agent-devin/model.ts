import { randomUUID } from 'node:crypto';
import { isToolCallInputFinal, type SessionEvent, type ToolCall } from '@cognition-ai/sdk';
import type { NativeAgentScope } from '../../packages/sdk/src/native-agent.ts';
import { DevinRuntime, NativeFailure } from './runtime.ts';
import { toDevinPrompt, type UserPart } from './prompt.ts';
import { captureContext, captureRenderedContext } from './context.ts';

type CallOptions = {
  prompt: { role: string; content: string | UserPart[] }[];
  headers?: Record<string, string | undefined>;
  providerOptions?: { devin?: { nativeConfigToken?: unknown; nativeContext?: unknown } };
  abortSignal?: AbortSignal;
};
type Part =
  | { type: 'text-start' | 'text-end' | 'reasoning-start' | 'reasoning-end'; id: string }
  | { type: 'text-delta'; id: string; delta: string }
  | { type: 'reasoning-delta'; id: string; delta: string }
  | { type: 'tool-call'; toolCallId: string; toolName: string; input: string; providerExecuted: true }
  | { type: 'tool-result'; toolCallId: string; toolName: string; result: ToolCall['rawOutput']; isError: boolean; providerExecuted: true }
  | { type: 'finish'; finishReason: { unified: 'length' | 'stop'; raw: string }; usage: { inputTokens: Record<string, never>; outputTokens: Record<string, never> } };

/** OpenCode's supported AI SDK adapter persists provider-executed tool parts. */
export function languageModel(modelID: string, directory: string, runtime: DevinRuntime) {
  return {
    specificationVersion: 'v3', provider: 'devin', modelId: modelID, supportedUrls: {},
    async doGenerate() { throw new NativeFailure('unsupported'); },
    async doStream(options: CallOptions) {
      const sessionID = options.headers?.['x-openchamber-devin-session'];
      const kind = options.headers?.['x-openchamber-devin-kind'];
      const token = options.providerOptions?.devin?.nativeConfigToken;
      if (!sessionID || kind !== 'primary' || typeof token !== 'string') throw new NativeFailure('not_prepared');
      const scope: NativeAgentScope = { directory, providerID: 'devin', sessionID };
      const lastUser = options.prompt.findLast((message) => message.role === 'user');
      if (!lastUser) throw new NativeFailure('invalid_request');
      const prompt = toDevinPrompt(lastUser.content);
      const nativeContext = options.providerOptions?.devin?.nativeContext;
      const context = nativeContext === undefined
        ? captureContext(options.prompt.filter((message) => message.role === 'system'), options.prompt.filter((message) => message.role !== 'system'), 'aisdk-prompt')
        : captureRenderedContext(nativeContext, options.prompt);
      const controller = new AbortController();
      const signal = options.abortSignal ? AbortSignal.any([controller.signal, options.abortSignal]) : controller.signal;
      const iterator = runtime.execute(scope, token, prompt, randomUUID(), signal, modelID, context);
      const project = projector();
      let done = false;
      return {
        stream: new ReadableStream<Part>({
          async pull(stream) {
            try {
              while (true) {
                const next = await iterator.next();
                if (next.done) {
                  for (const part of project.finish(next.value.stopReason)) stream.enqueue(part);
                  done = true;
                  stream.close();
                  return;
                }
                const parts = project.event(next.value);
                for (const part of parts) stream.enqueue(part);
                // Buffered text and observation-only events produce no stream part yet.
                if (parts.length) return;
              }
            } catch (error) {
              done = true;
              stream.error(error instanceof NativeFailure ? new NativeFailure(error.code, error.uncertain) : new NativeFailure('unavailable'));
            }
          },
          async cancel() {
            controller.abort();
            // Drain the SDK's cancellation result so stop is recorded only after confirmation.
            if (!done) {
              try { while (!(await iterator.next()).done) {} }
              catch { /* The runtime retains an unknown outcome if cancellation cannot be confirmed. */ }
            }
          },
        }),
      };
    },
  };
}

export function projector() {
  const fragments = new Map<string, { type: 'text' | 'reasoning'; text: string }>();
  const tools = new Map<string, { name: string; settled: boolean }>();
  return {
    event(event: SessionEvent): Part[] {
      if (event.type === 'message_delta' || event.type === 'thought_delta') {
        if (event.type === 'message_delta' && event.chain === 'side') return [];
        const type = event.type === 'message_delta' ? 'text' : 'reasoning';
        const id = event.messageId ?? type;
        if (event.type === 'message_delta' && event.aborted) { fragments.delete(id); return []; }
        const previous = fragments.get(id)?.text ?? '';
        fragments.set(id, { type, text: event.overwrite ? event.text : previous + event.text });
        return [];
      }
      if (event.type !== 'tool_call') return [];
      const call = event.call;
      if (!isToolCallInputFinal(call)) return [];
      const prior = tools.get(call.id);
      const name = prior?.name ?? call.toolName ?? call.kind ?? 'devin_tool';
      const parts: Part[] = [];
      if (!prior) {
        tools.set(call.id, { name, settled: false });
        parts.push({ type: 'tool-call', toolCallId: call.id, toolName: name, input: JSON.stringify(call.rawInput ?? {}), providerExecuted: true });
      }
      if (event.settled && !prior?.settled) {
        tools.set(call.id, { name, settled: true });
        parts.push({ type: 'tool-result', toolCallId: call.id, toolName: name,
          result: call.rawOutput ?? call.content ?? [], isError: call.status === 'failed' || call.rejected || call.canceled, providerExecuted: true });
      }
      return parts;
    },
    finish(reason: string): Part[] {
      return [
        ...Array.from(fragments, ([id, fragment]) => [
          { type: `${fragment.type}-start` as const, id }, { type: `${fragment.type}-delta` as const, id, delta: fragment.text }, { type: `${fragment.type}-end` as const, id },
        ]).flat(),
        { type: 'finish', finishReason: { unified: reason === 'max_tokens' ? 'length' : 'stop', raw: reason },
          usage: { inputTokens: {}, outputTokens: {} } },
      ];
    },
  };
}
