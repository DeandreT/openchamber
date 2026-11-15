import { expect, test } from 'bun:test';
import { EventEmitter } from 'node:events';
import { watchSmokeChild } from './smoke-child.mjs';

class Child extends EventEmitter {
  pid = 123;
  kills = 0;
  acceptsKill = true;
  kill() { this.kills++; return this.acceptsKill; }
}

test('smoke cleanup cannot report success before the exact child acknowledges exit', async () => {
  const child = new Child();
  const watched = watchSmokeChild(child);
  let complete = false;
  const stopped = watched.stop().then((result) => { complete = true; return result; });
  await Promise.resolve();
  expect(complete).toBe(false);
  expect(child.kills).toBe(1);
  child.emit('exit', null, 'SIGTERM');
  expect(await stopped).toEqual({ pid: 123, code: null, signal: 'SIGTERM', acknowledged: true });
  expect(child.listenerCount('exit')).toBe(0);
  expect(child.listenerCount('error')).toBe(0);
});

test('smoke cleanup accepts an already observed exit without another kill', async () => {
  const child = new Child();
  const watched = watchSmokeChild(child);
  child.emit('exit', 0, null);
  expect(await watched.stop()).toEqual({ pid: 123, code: 0, signal: null, acknowledged: true });
  expect(child.kills).toBe(0);
});

test('smoke startup error is handled and propagated without a false exit acknowledgement', async () => {
  const child = new Child();
  const watched = watchSmokeChild(child);
  const failure = new Error('fixture startup failure');
  child.emit('error', failure);
  expect(watched.outcome).toEqual({ error: failure });
  await expect(watched.stop()).rejects.toBe(failure);
  expect(child.kills).toBe(0);
});

test('smoke cleanup fails when killing the retained child is refused', async () => {
  const child = new Child();
  const watched = watchSmokeChild(child);
  child.acceptsKill = false;
  await expect(watched.stop()).rejects.toThrow('could not be stopped');
  child.emit('exit', 1, null);
});

test('smoke cleanup fails within its bound when exit is never acknowledged', async () => {
  const child = new Child();
  const watched = watchSmokeChild(child);
  await expect(watched.stop(1)).rejects.toThrow('exit was not acknowledged');
  child.emit('exit', null, 'SIGTERM');
});
