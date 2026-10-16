/**
 * Project knowledge a session still owes, as decided by the server.
 *
 * The client neither assembles this text nor tracks what it has sent. It used
 * to do both, which meant a session started without a UI got nothing, and a
 * conversation that was compacted kept a tab-local belief that the agent still
 * had context the summary had just removed.
 *
 * Nothing here throws. A message must go out even when its background cannot
 * be fetched: sending without the block costs the agent some context, failing
 * the send costs the user their message.
 */

import { runtimeFetch } from './runtime-fetch';
import { z } from 'zod';

interface SessionKnowledge {
  /** Empty when the session already carries what it needs. */
  text: string;
  /** Reported back once the message carrying the text has actually gone out. */
  signature: string;
}

const EMPTY: SessionKnowledge = { text: '', signature: '' };

/**
 * Delivery reports still on their way to the server, by session. Until one
 * lands the server still says the session owes the block, so a send right
 * behind the one that carried it would carry it a second time.
 */
const deliveryReports = new Map<string, Promise<void>>();

export const fetchSessionKnowledge = async (
  directory: string | null,
  sessionId: string | null,
): Promise<SessionKnowledge> => {
  if (!directory) {
    return EMPTY;
  }

  if (sessionId) {
    await deliveryReports.get(sessionId);
  }

  try {
    const params = new URLSearchParams({ directory });
    if (sessionId) {
      params.set('sessionId', sessionId);
    }
    const response = await runtimeFetch(`/api/session-knowledge?${params.toString()}`, {
      cache: 'no-store',
    });
    if (!response.ok) {
      return EMPTY;
    }
    const payload = await response.json() as Partial<SessionKnowledge> | null;
    return {
      text: typeof payload?.text === 'string' ? payload.text : '',
      signature: typeof payload?.signature === 'string' ? payload.signature : '',
    };
  } catch {
    return EMPTY;
  }
};

/**
 * Recorded after the send resolves, never before: a failed send must carry the
 * block again rather than assume the agent already saw it.
 */
export const reportSessionKnowledgeDelivered = async (
  directory: string | null,
  sessionId: string | null,
  signature: string,
): Promise<void> => {
  if (!directory || !sessionId || !signature) {
    return;
  }

  invalidateSummary(directory, sessionId);
  const report = (async () => {
    try {
      await runtimeFetch('/api/session-knowledge/delivered', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ directory, sessionId, signature }),
        directory,
      });
    } catch {
      // Only means the block may be sent once more.
    }
  })();
  deliveryReports.set(sessionId, report);
  await report;
  if (deliveryReports.get(sessionId) === report) deliveryReports.delete(sessionId);
};

/**
 * The line every block opens with: `KNOWLEDGE_HEADER` in
 * `packages/web/server/lib/session-knowledge/runtime.js`, whose test pins this
 * copy to it. A synthetic message starting with it carries the block.
 */
export const SESSION_KNOWLEDGE_HEADER = 'Standing context for this session, added by OpenChamber.'
  + ' It is background, not a request: act on the user\'s messages, and carry on'
  + ' with any task already in progress.';

/**
 * After a revert hid the message carrying the block. Without this the server
 * still believes the session holds it and the next send carries nothing.
 */
export const forgetSessionKnowledgeDelivered = async (
  directory: string | null | undefined,
  sessionId: string,
): Promise<void> => {
  if (!directory) return;
  invalidateSummary(directory, sessionId);
  try {
    await runtimeFetch('/api/session-knowledge/forget', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ directory, sessionId }),
    });
  } catch {
    // Only means the next send goes without the block, as it did before.
  }
};

export interface SessionKnowledgeSummary {
  notes: Array<{ id: string; body: string }>;
  plans: Array<{ id: string; title: string }>;
  memory: { global: number; project: number };
}

const EMPTY_SUMMARY: SessionKnowledgeSummary = { notes: [], plans: [], memory: { global: 0, project: 0 } };

// Several panels ask for the same session's summary at the same moment when a
// session opens. One request serves all of them, and its answer stays good for
// a few seconds; local pin changes drop it immediately.
const SUMMARY_FRESH_MS = 5_000;
const summaryCache = new Map<string, { at: number; request: Promise<SessionKnowledgeSummary> }>();
const summaryCacheKey = (directory: string, sessionId: string | null | undefined): string => (
  `${directory}\u0000${sessionId ?? ''}`
);

const invalidateSummary = (directory: string, sessionId: string | null | undefined): void => {
  summaryCache.delete(summaryCacheKey(directory, sessionId));
};

/** What the session is carrying, for display. Never throws; shows nothing instead. */
export const fetchSessionKnowledgeSummary = (
  directory: string | null,
  sessionId?: string | null,
): Promise<SessionKnowledgeSummary> => {
  if (!directory) {
    return Promise.resolve(EMPTY_SUMMARY);
  }
  const key = summaryCacheKey(directory, sessionId);
  const cached = summaryCache.get(key);
  const now = Date.now();
  if (cached && now - cached.at < SUMMARY_FRESH_MS) {
    return cached.request;
  }
  const request = requestSessionKnowledgeSummary(directory, sessionId);
  summaryCache.set(key, { at: now, request });
  return request;
};

const requestSessionKnowledgeSummary = async (
  directory: string,
  sessionId?: string | null,
): Promise<SessionKnowledgeSummary> => {
  try {
    const params = new URLSearchParams({ directory });
    if (sessionId) params.set('sessionId', sessionId);
    const response = await runtimeFetch(
      `/api/session-knowledge/summary?${params.toString()}`,
      { cache: 'no-store' },
    );
    if (!response.ok) {
      return EMPTY_SUMMARY;
    }
    const payload = await response.json() as Partial<SessionKnowledgeSummary> | null;
    return {
      notes: Array.isArray(payload?.notes) ? payload.notes : [],
      plans: Array.isArray(payload?.plans) ? payload.plans : [],
      memory: {
        global: typeof payload?.memory?.global === 'number' ? payload.memory.global : 0,
        project: typeof payload?.memory?.project === 'number' ? payload.memory.project : 0,
      },
    };
  } catch {
    return EMPTY_SUMMARY;
  }
};

export type SessionProjectContextPins = { notes: string[]; plans: string[] };

const sessionProjectContextPinsResponseSchema = z.object({
  pins: z.object({ notes: z.array(z.string()), plans: z.array(z.string()) }),
});

export const setSessionProjectContextPin = async (
  directory: string,
  sessionId: string,
  kind: 'note' | 'plan',
  id: string,
  pinned: boolean,
): Promise<SessionProjectContextPins | null> => {
  invalidateSummary(directory, sessionId);
  try {
    const response = await runtimeFetch('/api/session-knowledge/pin', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ directory, sessionId, kind, id, pinned }),
      directory,
    });
    if (!response.ok) return null;
    return sessionProjectContextPinsResponseSchema.parse(await response.json()).pins;
  } catch {
    return null;
  }
};
