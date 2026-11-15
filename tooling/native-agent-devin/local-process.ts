import { spawn } from 'node:child_process';
import { PassThrough } from 'node:stream';
import { LocalDevin, type LocalDevinOptions } from '@cognition-ai/sdk';
import { NativeFailure } from './errors.ts';

class DrainedStdout extends PassThrough {
  override emit(event: string | symbol, ...args: unknown[]): boolean {
    // ACP response handlers must settle before the SDK sees transport EOF.
    if (event === 'end') { setImmediate(() => super.emit(event, ...args)); return true; }
    return super.emit(event, ...args);
  }
}

/** The public SDK retains process custody through connect's child argument. */
export const localProcess = {
  async spawn(cliPath: string, options: Omit<LocalDevinOptions, 'apiKey' | 'stateDir'> = {}, env = process.env): Promise<LocalDevin> {
    if (process.platform !== 'win32') return LocalDevin.spawn(cliPath, options, env);
    const child = spawn(cliPath, ['acp'], {
      cwd: options.cwd ?? process.cwd(), env, windowsHide: true, shell: false,
      stdio: ['pipe', 'pipe', 'ignore'],
    });
    // A write racing process exit is reported by the SDK transport closing.
    child.stdin.on('error', () => {});
    const failure = new Promise<never>((_, reject) => {
      child.once('error', () => reject(new NativeFailure('unavailable')));
      child.once('exit', () => reject(new NativeFailure('unavailable')));
    });
    // An ordinary exit after the handshake also settles this promise.
    void failure.catch(() => {});
    // Keep final response continuations ahead of EOF, including a buffered
    // response resumed after exit. The SDK's EOF handler closes active turns.
    const readable = new DrainedStdout();
    child.stdout.pipe(readable, { end: false });
    child.stdout.once('end', () => readable.end());
    child.stdout.once('error', (error) => readable.destroy(error));
    try {
      const client = await LocalDevin.connect({ writable: child.stdin, readable }, options, child, failure);
      // Exit can precede the final stdout response. Only an idle, empty pipe
      // needs the fallback for Bun's missing Windows EOF.
      let drainTimeout: ReturnType<typeof setTimeout> | undefined;
      const cleanup = () => {
        clearTimeout(drainTimeout);
        readable.off('data', progress);
        readable.off('end', cleanup);
        readable.off('close', cleanup);
        child.off('exit', exited);
      };
      const progress = () => {
        clearTimeout(drainTimeout);
        drainTimeout = setTimeout(() => {
          // Buffered bytes can survive an event-loop stall or paused consumer.
          if (readable.readableLength > 0 || child.stdout.readableLength > 0) { progress(); return; }
          client.close();
        }, 1_000);
      };
      const exited = () => {
        if (readable.readableEnded) return;
        readable.on('data', progress);
        progress();
      };
      readable.once('end', cleanup);
      readable.once('close', cleanup);
      child.once('exit', exited);
      const close = client.close.bind(client);
      client.close = () => { cleanup(); close(); readable.destroy(); };
      if (child.exitCode !== null || child.signalCode !== null) exited();
      return client;
    } catch {
      readable.destroy();
      child.kill();
      throw new NativeFailure('unavailable');
    }
  },
};
