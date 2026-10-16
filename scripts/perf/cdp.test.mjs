import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import test from 'node:test';
import { captureError, capturePageState, navigateForCapture, stopChrome } from './cdp.mjs';
import { createNavigationRecorder, createNetworkRecorder, summarizeRequests } from './network.mjs';
import { recordHeapAttempt } from '../profile-heap.mjs';

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

class CaptureClient extends EventEmitter {
  calls = [];
  reply = async () => ({});
  send(method, params, options) {
    this.calls.push({ method, params, options });
    return this.reply(method, params, options);
  }
  on(method, listener) {
    super.on(method, listener);
    return () => this.off(method, listener);
  }
  onClose(listener) { return this.on('close', listener); }
}

const attempt = async (client, options = {}) => {
  const summary = { phase: 'initial-navigation', navigation: {}, readings: {}, rows: 0, opened: 0 };
  const writes = [];
  let snapshots = 0;
  let cleanupCalls = 0;
  let error;
  try {
    await recordHeapAttempt({
      summary,
      run: () => navigateForCapture(client, 'Page.navigate', { url: 'http://localhost/' }, summary.navigation, { timeoutMs: 20, ...options }),
      onFailure: async () => { snapshots += 1; },
      cleanup: [() => { cleanupCalls += 1; }],
      persist: async value => { writes.push(structuredClone(value)); },
    });
  } catch (caught) { error = caught; }
  assert.equal(writes.length, 1, 'One result per attempt');
  assert.equal(cleanupCalls, 1);
  assert.equal(client.listenerCount('Page.loadEventFired'), 0);
  assert.equal(client.listenerCount('close'), 0);
  assert.equal(client.calls.filter(call => call.method === 'Page.navigate').length, 1, 'No retry');
  return { summary: writes[0], snapshots, error };
};

test('result-level navigation failure is persisted with its original error and identity', async () => {
  const client = new CaptureClient();
  client.reply = async () => ({ frameId: 'main', loaderId: 'load', errorText: 'net::ERR_CONNECTION_RESET' });
  const result = await attempt(client);
  assert.equal(result.summary.valid, false);
  assert.equal(result.summary.navigation.result.frameId, 'main');
  assert.equal(result.summary.navigation.result.loaderId, 'load');
  assert.equal(result.summary.navigation.result.errorText, 'net::ERR_CONNECTION_RESET');
  assert.equal(result.error.message, 'net::ERR_CONNECTION_RESET');
  assert.equal(result.snapshots, 1);
});

test('load before the command response is observed without an extra navigation', async () => {
  const client = new CaptureClient();
  client.reply = async () => {
    client.emit('Page.loadEventFired', { timestamp: 42 });
    return { frameId: 'main', loaderId: 'new' };
  };
  const result = await attempt(client);
  assert.equal(result.summary.valid, true);
  assert.equal(result.summary.navigation.loadEvent.timestamp, 42);
  assert.equal(result.snapshots, 0);
});

test('protocol rejection, missing load and connection close each save a failed attempt', async () => {
  for (const failure of ['command', 'load', 'close']) {
    const client = new CaptureClient();
    const original = new Error('Protocol command rejected');
    client.reply = async () => {
      if (failure === 'command') throw original;
      if (failure === 'close') client.emit('close');
      return { frameId: 'main' };
    };
    const result = await attempt(client);
    assert.equal(result.summary.valid, false, failure);
    assert.equal(result.snapshots, 1, failure);
    assert.ok(result.error, failure);
    if (failure === 'command') assert.equal(result.error, original);
  }
});

test('a command outlasting the load timeout is handled and cannot mutate the saved failure', async () => {
  const client = new CaptureClient();
  let rejectCommand;
  client.reply = () => new Promise((_, reject) => { rejectCommand = reject; });
  const result = await attempt(client);
  assert.equal(result.summary.valid, false);
  assert.equal(result.summary.navigation.result, null);
  rejectCommand(new Error('Late command rejection'));
  // node:test reports any unhandled rejection. Drain the late settlement too.
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(result.summary.navigation.result, null);
});

test('cleanup and snapshot failures do not replace the original failure or prevent persistence', async () => {
  const original = new Error('net::ERR_ABORTED');
  const summary = { phase: 'reload' };
  let writes = 0;
  let secondCleanup = false;
  await assert.rejects(recordHeapAttempt({
    summary,
    run: async () => { throw original; },
    onFailure: async () => { throw new Error('Snapshot unavailable'); },
    cleanup: [() => { throw new Error('Cleanup unavailable'); }, () => { secondCleanup = true; }],
    persist: async () => { writes += 1; },
  }), error => error === original);
  assert.equal(writes, 1);
  assert.equal(secondCleanup, true);
  assert.equal(summary.error, 'net::ERR_ABORTED');
  assert.deepEqual(summary.cleanupErrors, ['Error']);
  assert.equal(summary.snapshotError, 'Error');
});

test('document diagnostics distinguish unrelated frames and failed terminal requests without private URLs', () => {
  const client = new CaptureClient();
  const recorder = createNavigationRecorder(client, 'http://localhost/private?token=secret');
  const network = createNetworkRecorder(client);
  client.emit('Page.frameNavigated', { frame: { id: 'main', loaderId: 'new', url: 'http://localhost/private?token=secret' } });
  client.emit('Page.lifecycleEvent', { frameId: 'child', loaderId: 'other', name: 'load', timestamp: 1 });
  client.emit('Network.requestWillBeSent', { requestId: 'request', frameId: 'main', loaderId: 'new', type: 'Document', request: { method: 'GET', url: 'http://localhost/private?token=secret' }, wallTime: 1 });
  client.emit('Network.responseReceived', { requestId: 'request', response: { status: 503 } });
  client.emit('Network.loadingFailed', { requestId: 'request', canceled: true, errorText: 'net::ERR_ABORTED http://localhost/private?token=secret', timestamp: 2 });
  client.emit('Network.loadingFinished', { requestId: 'request', encodedDataLength: 100, timestamp: 3 });
  const data = recorder.read();
  assert.equal(data.documents[0].main, true);
  assert.equal(data.documents[0].status, 503);
  assert.equal(data.documents[0].failed, true);
  assert.equal(data.documents[0].terminalEvents[0].type, 'failed');
  assert.equal(data.documents[0].terminalEvents[0].cancelled, true);
  assert.equal(data.documents[0].terminalEvents[0].error, 'net::ERR_ABORTED');
  assert.equal(data.documents[0].terminalEvents[1].type, 'finished', 'An error document can finish under the failed request ID');
  assert.equal(data.lifecycle[0].frameId, 'child', 'An unrelated load is not recorded as main-document evidence');
  assert.ok(!JSON.stringify(data).includes('secret'));
  assert.ok(!JSON.stringify(data).includes('/private'));
  const totals = summarizeRequests([...network.requests.values()], 'http://localhost');
  assert.equal(totals.unfinished, 0);
  assert.equal(totals.failed, 1);
  recorder.stop();
  assert.equal(client.listenerCount('Page.frameNavigated'), 0);
});

test('navigation diagnostics bound event history and do not infer failed zero-byte responses', () => {
  const client = new CaptureClient();
  const recorder = createNavigationRecorder(client, 'http://localhost/');
  for (let index = 0; index < 30; index += 1) {
    client.emit('Network.requestWillBeSent', { requestId: String(index), type: 'Document', request: { url: 'http://localhost/' } });
    client.emit('Page.lifecycleEvent', { name: 'load', frameId: 'frame', loaderId: String(index) });
  }
  client.emit('Network.loadingFinished', { requestId: '29', encodedDataLength: 0 });
  const result = recorder.read();
  assert.equal(result.documents.length, 24);
  assert.equal(result.lifecycle.length, 24);
  assert.equal(result.droppedDocuments, 6);
  assert.equal(result.documents.at(-1).terminalEvents[0].type, 'finished');
  assert.equal(result.documents.at(-1).failed, false);
  assert.equal(result.documents.at(-1).status, null);
  assert.equal(result.documents.at(-1).main, null);
  recorder.stop();
});

test('page-state snapshot is bounded and discards page exception content', async () => {
  const client = new CaptureClient();
  client.reply = async () => ({ exceptionDetails: { exception: { description: 'Private document text' } } });
  assert.deepEqual(await capturePageState(client, 'http://localhost/'), { evaluationError: 'Page evaluation failed' });
  assert.equal(client.calls[0].options.timeoutMs, 3_000);
  client.reply = async () => { throw new Error('http://user:secret@localhost/private?token=secret'); };
  assert.deepEqual(await capturePageState(client, 'http://localhost/'), { evaluationError: 'Error' });
  assert.equal(captureError('Private document text'), 'Capture error');
});
