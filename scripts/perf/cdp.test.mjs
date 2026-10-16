import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import test from 'node:test';
import { stopChrome } from './cdp.mjs';

test('profiling an attached browser never closes it', async () => {
  await stopChrome(null, { send: () => assert.fail('The attached browser belongs to the user') });
});

test('owned browser finishes graceful shutdown before process termination', async () => {
  const child = Object.assign(new EventEmitter(), {
    exitCode: null,
    signalCode: null,
    kill: () => assert.fail('Graceful shutdown was interrupted'),
  });
  let closed = false;
  await stopChrome(child, {
    send: async (method) => {
      assert.equal(method, 'Browser.close');
      setTimeout(() => {
        child.exitCode = 0;
        closed = true;
        child.emit('exit', 0);
      }, 10);
    },
  });
  assert.equal(closed, true);
  assert.equal(child.listenerCount('exit'), 0);
});

test('an exited launcher still closes the browser through CDP', async () => {
  let closed = false;
  await stopChrome({ exitCode: 0 }, { send: async (method) => {
    assert.equal(method, 'Browser.close');
    closed = true;
  } });
  assert.equal(closed, true);
});
