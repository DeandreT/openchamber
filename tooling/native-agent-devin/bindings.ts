import { createHash } from 'node:crypto';
import type { Plugin } from '@opencode/plugin';
import type { NativeAgentConfig } from '../../packages/sdk/src/native-agent.ts';
import { isMode, NativeFailure, type Mode } from './runtime.ts';
import { nativeAgentConfigSchema } from '../../packages/sdk/src/native-agent-schemas.ts';

type Binding = { mode: Mode; config: NativeAgentConfig[] };
type Store = { read(): ReturnType<Plugin.Context['storage']['get']>; write(value: Record<string, Binding>): Promise<void>; reload(): Promise<void> };
const tokenFor = (value: Binding) => `native-${createHash('sha256').update(JSON.stringify(value)).digest('hex')}`;
function validateConfig(value: unknown): asserts value is NativeAgentConfig[] {
  nativeAgentConfigSchema.parse(value);
}

/** Retained variants are immutable so an old queue item survives plugin reloads. */
export class Bindings {
  private values: Record<string, Binding> = {};
  private queue: Promise<unknown> = Promise.resolve();
  private published = new Set<string>();
  constructor(private store: Store) {}
  async load() {
    const saved = await this.store.read();
    if (saved === undefined) return;
    if (!saved || typeof saved !== 'object' || Array.isArray(saved)) throw new NativeFailure('unavailable');
    for (const [token, value] of Object.entries(saved)) {
      if (!value || typeof value !== 'object' || !('mode' in value) || !isMode(value.mode) || !('config' in value) || !Array.isArray(value.config)) throw new NativeFailure('unavailable');
      validateConfig(value.config);
      const binding: Binding = { ...value, mode: value.mode, config: value.config };
      if (tokenFor(binding) !== token) throw new NativeFailure('unavailable');
      this.values[token] = structuredClone(binding);
    }
  }
  variants(mode: Mode) {
    return Object.entries(this.values).filter(([, value]) => value.mode === mode)
      .map(([id]) => ({ id, settings: { nativeConfigToken: id } }));
  }
  async bind(config: NativeAgentConfig[], mode: Mode, signal?: AbortSignal) {
    const value = { mode, config: structuredClone(config) };
    const token = tokenFor(value);
    const commit = async () => {
      if (signal?.aborted) throw new NativeFailure('unavailable');
      if (!this.values[token]) {
        const next = { ...this.values, [token]: value };
        await this.store.write(next);
        if (signal?.aborted) throw new NativeFailure('unavailable');
        this.values = next;
      }
      if (!this.published.has(token)) {
        if (signal?.aborted) throw new NativeFailure('unavailable');
        await this.store.reload();
        if (signal?.aborted) throw new NativeFailure('unavailable');
        this.published.add(token);
      }
      return { providerID: 'devin', modelID: mode, variant: token };
    };
    const result = this.queue.then(commit, commit);
    this.queue = result.catch(() => {});
    return result;
  }
  async resolve(token: string): Promise<Binding> {
    const value = this.values[token];
    if (!value) throw new NativeFailure('invalid_config');
    return structuredClone(value);
  }
}
