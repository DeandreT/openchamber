import assert from 'node:assert/strict';
import { LocalDevin } from '@cognition-ai/sdk';
import { localProcess } from './local-process.ts';
import type { ChildProcess } from 'node:child_process';
import { Readable } from 'node:stream';

const executable = process.argv[2];
if (process.platform !== 'win32' || !executable) throw Error('Windows fake CLI path required');
const baseline = await LocalDevin.spawn(executable, {}, {});
try {
  const session = await baseline.createSession();
  const consoleWindow = Number(session.id.split('-')[3]);
  console.log(JSON.stringify({ proof: 'node-sdk-baseline', consoleWindowPresent: consoleWindow > 0 }));
} finally { baseline.close(); }
const client = await localProcess.spawn(executable, {}, {});
try {
  const session = await client.createSession();
  assert.equal(Number(session.id.split('-')[3]), 0);
  console.log(JSON.stringify({ proof: 'node-hidden-public-connect', consoleWindow: 0 }));
} finally { client.close(); }

for (const drain of ['flowing', 'paused', 'slow', 'missing-eof']) {
  const paused = drain !== 'flowing';
  let child: ChildProcess | undefined;
  let readable: Readable | undefined;
  const connect = LocalDevin.connect.bind(LocalDevin);
  LocalDevin.connect = async (...args) => {
    child = args[2] ?? undefined;
    if (args[0].readable instanceof Readable) readable = args[0].readable;
    return connect(...args);
  };
  let finalClient: LocalDevin | undefined;
  try {
    finalClient = await localProcess.spawn(executable, {}, { FAKE_ACP_EXIT: drain === 'missing-eof' ? 'turn' : 'final' });
    const session = await finalClient.createSession();
    if (!child || !readable) throw Error('Missing SDK child and readable stream');
    const stdout = readable;
    if (paused) stdout.pause();
    const exited = new Promise<void>((resolve) => child!.once('exit', () => resolve()));
    const result = session.stream('fixture only').next().then((value) => ({ value }), (error: unknown) => ({ error }));
    if (paused) await exited;
    const started = performance.now();
    if (drain === 'slow') {
      await new Promise((resolve) => setTimeout(resolve, 1_250));
      while (stdout.readableLength > 0) {
        stdout.read(Math.min(10, stdout.readableLength));
        await new Promise((resolve) => setTimeout(resolve, 250));
      }
      assert.ok(performance.now() - started > 1_000);
    }
    if (drain === 'missing-eof') {
      assert.ok('error' in await result);
      stdout.resume();
    } else {
      if (paused) stdout.resume();
      assert.deepEqual(await result, { value: { done: true, value: { stopReason: 'end_turn' } } });
    }
    console.log(JSON.stringify({ proof: 'node-exit-drain', drain, drainMilliseconds: Math.round(performance.now() - started), outcome: drain === 'missing-eof' ? 'rejected' : 'end_turn' }));
  } finally {
    finalClient?.close();
    if (readable) assert.equal(readable.listenerCount('data'), 0);
    LocalDevin.connect = connect;
  }
}
const disconnected = await localProcess.spawn(executable, {}, { FAKE_ACP_EXIT: 'turn' });
try {
  const session = await disconnected.createSession();
  await assert.rejects(session.stream('fixture only').next());
  console.log(JSON.stringify({ proof: 'node-no-response-exit', rejected: true }));
} finally { disconnected.close(); }
