import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { captureError, capturePageState, navigateForCapture, recordCaptureAttempt, resolveProfileDir, stopChrome } from './cdp.mjs';
import { createNavigationRecorder, createNetworkRecorder, summarizeRequests } from './network.mjs';

test('profiling an attached browser never closes it', async () => {
  await stopChrome(null, { send: () => assert.fail('The attached browser belongs to the user') });
});

test('temporary profile cleanup removes owned data and preserves an explicit profile', async () => {
  const retained = await mkdtemp(join(tmpdir(), 'profile-owner-'));
  const temporary = resolveProfileDir(null, 'profile-owner');
  try {
    for (const dir of [retained, temporary.dir]) await writeFile(join(dir, 'data'), 'keep');
    await resolveProfileDir(retained, 'profile-owner').remove();
    assert.equal(await readFile(join(retained, 'data'), 'utf8'), 'keep');
    await temporary.remove();
    await assert.rejects(readFile(join(temporary.dir, 'data')), { code: 'ENOENT' });
    await temporary.remove();
  } finally {
    await rm(retained, { recursive: true, force: true });
    await rm(temporary.dir, { recursive: true, force: true });
  }
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
    await recordCaptureAttempt({
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
  await assert.rejects(recordCaptureAttempt({
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

test('navigation receipts retain replay, live starts, command identity and clock anchors in receipt order', async () => {
  const client = new CaptureClient();
  const url = 'http://localhost/private?token=secret';
  const recorder = createNavigationRecorder(client, url);
  client.emit('Page.lifecycleEvent', { name: 'load', frameId: 'main', loaderId: 'old', timestamp: 1 });
  client.reply = async () => {
    client.emit('Page.frameRequestedNavigation', { frameId: 'main', url, reason: 'scriptInitiated', disposition: 'currentTab' });
    client.emit('Page.frameStartedNavigating', { frameId: 'main', loaderId: 'new', url, navigationType: 'differentDocument' });
    client.emit('Network.requestWillBeSent', { requestId: 'request', frameId: 'main', loaderId: 'new', type: 'Document', request: { url }, timestamp: 100, wallTime: 2000 });
    client.emit('Network.loadingFailed', { requestId: 'request', canceled: true, errorText: 'net::ERR_ABORTED', timestamp: 101 });
    return { frameId: 'main', loaderId: 'new', errorText: 'net::ERR_ABORTED' };
  };
  await assert.rejects(navigateForCapture(client, 'Page.navigate', { url }, { phase: 'park-navigation' }, { recorder, waitForLoad: false }), /net::ERR_ABORTED/);
  const data = recorder.read();
  assert.deepEqual(data.events.map(event => event.type), [
    'lifecycle', 'command-sent', 'navigation-requested', 'navigation-started',
    'document-started', 'document-failed', 'command-result', 'navigation-error',
  ]);
  assert.deepEqual(data.events.map(event => event.sequence), [1, 2, 3, 4, 5, 6, 7, 8]);
  assert.ok(data.events.every(event => Number.isFinite(event.receivedAtMs)));
  assert.equal(data.events[1].phase, 'park-navigation');
  assert.equal(data.events[3].loaderId, 'new');
  assert.equal(data.events[3].navigationType, 'differentDocument');
  assert.equal(data.events[6].result.loaderId, 'new');
  assert.equal(data.events[6].result.errorText, 'net::ERR_ABORTED');
  assert.equal(data.documents[0].timestamp, 100);
  assert.equal(data.documents[0].wallTime, 2000);
  assert.equal(data.mainFrameId, null, 'A start does not establish a committed main frame');
  assert.ok(!JSON.stringify(data).includes('secret'));
  assert.ok(!JSON.stringify(data).includes('/private'));
  recorder.stop();
  assert.equal(client.listenerCount('Page.frameStartedNavigating'), 0);
  assert.equal(client.listenerCount('Page.frameRequestedNavigation'), 0);
});

test('load timeout is recorded separately from a document cancellation', async () => {
  const client = new CaptureClient();
  const recorder = createNavigationRecorder(client, 'http://localhost/');
  client.reply = async () => {
    client.emit('Network.requestWillBeSent', { requestId: 'request', type: 'Document', request: { url: 'http://localhost/' } });
    return { frameId: 'main', loaderId: 'new' };
  };
  const result = await attempt(client, { recorder });
  assert.equal(result.summary.valid, false);
  assert.equal(result.summary.navigation.result.errorText, null);
  const data = recorder.read();
  assert.equal(data.events.at(-1).type, 'navigation-error');
  assert.equal(data.events.at(-1).error, 'Timed out waiting for Page.loadEventFired');
  assert.equal(data.documents[0].failed, false);
  assert.deepEqual(data.documents[0].terminalEvents, []);
  recorder.stop();
});

test('ordered receipts stay bounded with explicit loss and preserve sequence across eviction', () => {
  const client = new CaptureClient();
  const recorder = createNavigationRecorder(client, 'http://localhost/');
  for (let index = 0; index < 150; index += 1) {
    client.emit('Page.frameStartedNavigating', { frameId: 'main', loaderId: String(index), url: 'http://localhost/' });
  }
  const data = recorder.read();
  assert.equal(data.events.length, 128);
  assert.equal(data.droppedEvents, 22);
  assert.equal(data.events[0].sequence, 23);
  assert.equal(data.events.at(-1).sequence, 150);
  recorder.stop();
});

test('a failed capture keeps completed workload entries alongside the failing phase', async () => {
  for (const summary of [
    { phase: 'switch-2', switches: [{ index: 0, visible: 300 }, { index: 1, visible: 400 }] },
    { phase: 'line-3', linesTyped: 2 },
  ]) {
    const original = structuredClone(summary);
    const saved = [];
    await assert.rejects(recordCaptureAttempt({
      summary,
      run: async () => { throw new Error('net::ERR_ABORTED'); },
      onFailure: async () => {},
      cleanup: [],
      persist: async result => saved.push(structuredClone(result)),
    }), /net::ERR_ABORTED/);
    assert.deepEqual(saved, [{ ...original, valid: false, cleanupErrors: [], error: 'net::ERR_ABORTED' }]);
  }
});
