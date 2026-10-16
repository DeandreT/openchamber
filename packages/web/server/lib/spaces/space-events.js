// One event connection per space, with its own state and its own backoff, attached to the
// host's event hub after the v2 translation layer: a space's events enter the hub as the
// host's do, numbered, coalesced, replayed and fanned out, marked with the space's id. They
// feed the same watcher, so live status, unread marks and notifications work for a space's
// sessions, and the same browser streams, so a client keeps one cursor for everything.
//
// Every event is untrusted and passes the session index first: a directory outside the
// space's root or a session id of the host's drops it. When a connection is made the host
// asks the space for its live status and enters it the same way. A gap is reported to the
// clients as `openchamber:space-stream`, so a client re-reads that one space.
//
// A second connection per space reads its OpenChamber control stream for one kind of event, a
// change to a session's message queue: the queue of a space's session lives in that space's
// server, which sees its OpenCode go idle and delivers. Each change passes the same rules and goes
// to the browsers marked with the space's id; a connection announces itself as
// `openchamber:space-queue-stream`, so a client re-reads that space's queue after a gap.

import { Readable } from 'node:stream';

import { z } from 'zod';

import { UPSTREAM_STALL_TIMEOUT_CONCURRENT_MS, createUpstreamSseReader } from '../event-stream/upstream-reader.js';

const EVENT_PATH = '/api/event';
const CONTROL_PATH = '/api/openchamber/events';
const QUEUE_UPDATED = 'openchamber:message-queue.updated';
// The control stream inside is quiet but for a heartbeat every 25 seconds, so a quiet minute is
// a dead stream; the reader's default of 20 seconds would drop and remake a healthy idle one.
const QUEUE_STALL_TIMEOUT_MS = UPSTREAM_STALL_TIMEOUT_CONCURRENT_MS;
const STATUS_PATH = '/api/sessions/status';
const STATUS_TIMEOUT_MS = 10_000;
const MAX_STATUS_BYTES = 4 * 1024 * 1024;

// Backoff between attempts: doubling from one second to a minute, reset by a connection.
const RECONNECT_BASE_MS = 1_000;
export const RECONNECT_CAP_MS = 60_000;
export const reconnectDelayAfter = (failures) => Math.min(RECONNECT_CAP_MS, RECONNECT_BASE_MS * 2 ** Math.max(0, failures - 1));

// What the host reads of an answer from inside; the rest is carried, not read.
const liveStatusSchema = z.object({ status: z.enum(['busy', 'idle']) });
const statusSchema = z.object({ sessions: z.record(z.string(), z.unknown()).default({}) }).passthrough();
const eventPayloadSchema = z.object({ location: z.object({ directory: z.string() }).passthrough().optional() }).passthrough();
// A queue change inside, rebuilt from these fields only; the items are the space's own, carried for display.
const queueUpdateSchema = z.object({
  type: z.literal(QUEUE_UPDATED),
  properties: z.object({
    revision: z.number(),
    session: z.object({
      sessionId: z.string(),
      directory: z.string(),
      items: z.array(z.unknown()),
      sendingId: z.string().nullable(),
    }),
  }),
});

/** Reads a whole body up to a cap, as text. */
const readBody = (response, cap) => new Promise((resolve, reject) => {
  const chunks = [];
  let size = 0;
  response.on('data', (chunk) => {
    size += chunk.length;
    if (size > cap) {
      response.destroy();
      reject(new Error(`the answer exceeds ${cap} bytes`));
      return;
    }
    chunks.push(chunk);
  });
  response.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
  response.on('error', reject);
});

/**
 * `requestInside(spaceId, { path, headers, timeoutMs })` is the dispatcher's; `index` the
 * session index; `hub` the global event hub; `broadcastUiEvent` the host's control stream, and
 * without it no queue connection is made. `stallTimeoutMs`, `queueStallTimeoutMs` and `now` are
 * for the tests.
 */
export function createSpaceEventSources({ requestInside, index, hub, broadcastUiEvent = null, logger = console, stallTimeoutMs, queueStallTimeoutMs = QUEUE_STALL_TIMEOUT_MS, now = Date.now }) {
  const sources = new Map();

  const inject = (spaceId, payload, directory) => hub.injectEvent({ payload, directory, spaceId });

  const announce = (spaceId, status, wasReady) => {
    inject(spaceId, { type: 'openchamber:space-stream', properties: { spaceId, status, wasReady, timestamp: now() } }, 'global');
  };

  /** The live status inside, entered as status events, so a busy session shows busy from the first moment. */
  const seedStatus = async (spaceId) => {
    const response = await requestInside(spaceId, { path: STATUS_PATH, headers: { accept: 'application/json' }, timeoutMs: STATUS_TIMEOUT_MS });
    if (response.statusCode !== 200) {
      response.resume();
      throw new Error(`status ${response.statusCode}`);
    }
    const parsed = statusSchema.safeParse(JSON.parse(await readBody(response, MAX_STATUS_BYTES)));
    if (!parsed.success) return;
    for (const [sessionID, state] of Object.entries(parsed.data.sessions)) {
      const live = liveStatusSchema.safeParse(state);
      if (!live.success) continue;
      const payload = { type: 'session.status', data: { sessionID, status: { type: live.data.status } } };
      if (index.acceptSpaceEvent(spaceId, payload)) inject(spaceId, payload, '');
    }
  };

  /** A stream inside, over the dispatcher's channel, in the shape the reader expects. */
  const fetchInside = (spaceId, path) => async (_url, { headers, signal }) => {
    const response = await requestInside(spaceId, { path, headers });
    const stop = () => response.destroy();
    if (signal?.aborted) { stop(); throw new Error('aborted'); }
    signal?.addEventListener('abort', stop, { once: true });
    const contentType = String(response.headers['content-type'] ?? '');
    const ok = response.statusCode === 200 && contentType.includes('text/event-stream');
    return { ok, status: response.statusCode, body: ok ? Readable.toWeb(response) : null };
  };

  const start = (spaceId) => {
    if (sources.has(spaceId)) return;
    const controller = new AbortController();
    const source = { failures: 0, everConnected: false, reader: null, controller };
    source.reader = createUpstreamSseReader({
      signal: controller.signal,
      fetchImpl: fetchInside(spaceId, EVENT_PATH),
      stallTimeoutMs,
      reconnectDelayMs: () => reconnectDelayAfter(source.failures),
      buildUrl: () => new URL(`http://space${EVENT_PATH}`),
      onConnect() {
        source.failures = 0;
        const wasReady = source.everConnected;
        source.everConnected = true;
        announce(spaceId, 'connected', wasReady);
        seedStatus(spaceId).catch((error) => logger.warn?.(`[spaces] could not read the status of space ${spaceId}: ${error?.code ?? error?.message ?? error}`));
      },
      onDisconnect({ reason }) {
        source.failures += 1;
        if (source.everConnected && reason !== 'stopped') announce(spaceId, 'disconnected', true);
      },
      onEvent({ payload }) {
        const parsed = eventPayloadSchema.safeParse(payload);
        if (!parsed.success || !index.acceptSpaceEvent(spaceId, payload)) return;
        inject(spaceId, payload, parsed.data.location?.directory ?? '');
      },
      // Every failed attempt ends in `onDisconnect`, which is where it is counted.
      onError(error) {
        if (controller.signal.aborted) return;
        logger.warn?.(`[spaces] event stream of space ${spaceId}: ${error?.type ?? 'error'} ${error?.status ?? error?.error?.code ?? ''}`.trim());
      },
    });
    if (broadcastUiEvent) source.queueReader = startQueueReader(spaceId, controller.signal);
    sources.set(spaceId, source);
    void source.reader.start();
  };

  /** A queue change of a space's session, to the browsers, when the space may speak for that session. */
  const forwardQueueUpdate = (spaceId, payload) => {
    const parsed = queueUpdateSchema.safeParse(payload);
    if (!parsed.success) return;
    const { revision, session } = parsed.data.properties;
    if (!index.acceptSpaceQueueSession(spaceId, session.sessionId, session.directory)) return;
    broadcastUiEvent({ type: QUEUE_UPDATED, properties: { revision, session, spaceId } });
  };

  const startQueueReader = (spaceId, signal) => {
    const state = { failures: 0 };
    const reader = createUpstreamSseReader({
      signal,
      fetchImpl: fetchInside(spaceId, CONTROL_PATH),
      stallTimeoutMs: queueStallTimeoutMs,
      reconnectDelayMs: () => reconnectDelayAfter(state.failures),
      buildUrl: () => new URL(`http://space${CONTROL_PATH}`),
      onConnect() {
        state.failures = 0;
        broadcastUiEvent({ type: 'openchamber:space-queue-stream', properties: { spaceId, status: 'connected' } });
      },
      onDisconnect() { state.failures += 1; },
      onEvent({ payload }) {
        if (payload?.type === QUEUE_UPDATED) forwardQueueUpdate(spaceId, payload);
      },
      onError(error) {
        if (signal.aborted) return;
        logger.warn?.(`[spaces] control stream of space ${spaceId}: ${error?.type ?? 'error'} ${error?.status ?? error?.error?.code ?? ''}`.trim());
      },
    });
    void reader.start();
    return reader;
  };

  const stop = (spaceId) => {
    const source = sources.get(spaceId);
    if (!source) return;
    sources.delete(spaceId);
    source.controller.abort();
    source.reader.stop();
    source.queueReader?.stop();
  };

  return {
    /**
     * Follows the list of spaces: a connection for each new one, none for one that is gone. A
     * space in `paused` is kept, with its index, and not connected: its place cannot be asked, and
     * an attempt would only wait on a machine that is off.
     */
    sync(spaceIds, { paused = new Set() } = {}) {
      const wanted = new Set(spaceIds);
      for (const spaceId of Array.from(sources.keys())) {
        if (!wanted.has(spaceId)) {
          stop(spaceId);
          index.forget(spaceId);
        } else if (paused.has(spaceId)) {
          stop(spaceId);
        }
      }
      for (const spaceId of wanted) {
        if (!paused.has(spaceId)) start(spaceId);
      }
    },
    /** For the tests: whether a space has a connection right now. */
    has: (spaceId) => sources.has(spaceId),
    close() {
      for (const spaceId of Array.from(sources.keys())) stop(spaceId);
    },
  };
}
