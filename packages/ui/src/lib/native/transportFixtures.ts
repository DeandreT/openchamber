/**
 * Differential fixtures for the native desktop's transport.
 *
 * The official `@opencode/client` runs against recorded server responses
 * through a `fetch` that also records every request it sends. Its decoded
 * results go through the UI's own projection. The output pins three things
 * the Rust transport must reproduce: the requests (method, path, query,
 * directory header), the projected sessions and the projected messages. See
 * `docs/rust-desktop/OPENCODE_ADAPTER.md`.
 *
 *   bun run native:transport-fixtures
 */
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { OpenCode } from '@opencode/client';
import { z } from 'zod';

import type { SessionPatch, SyncEvent } from '@/lib/opencode/events';
import { projectMessages, projectSession } from '@/lib/opencode/projection';
import { compact, type Metadata, type Part, type Session } from '@/lib/opencode/model';
import { PART_SHELLS_HEADER, PART_SHELLS_VERSION, toolContentShellChars } from '@/lib/opencode/toolContentShells';
import { isBtwSession } from '@/lib/sessionBtwMetadata';
import { isSessionPinnedInMetadata } from '@/lib/sessionPins';
import { isSessionInWork } from '@/lib/sessionWorkMetadata';
import { classifyStreamPayload } from '@/sync/event-pipeline';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, '..', '..', '..', '..', '..');
const wireDirectory = 'native/crates/transport/tests/fixtures/wire';

export const TRANSPORT_FIXTURE_PATH = 'native/crates/transport/tests/fixtures/transport.json';

const BASE_URL = 'http://127.0.0.1:4799';
const RECORDED_SESSION = 'ses_e9b49c443ffefXcWWPhCOga6nH';
const LONG_SESSION = 'ses_e9b4b0242ffeULkFFTS9CiS3wW';
const TOOLS_SESSION = 'ses_e9b3bfe12ffeMuiZLLg25eNcms';
const TOOLS_MESSAGE = 'msg_164c422cf001JfI01Q6WJWl0lr';
/** A directory with a space and non-ASCII, to pin the header's encoding. */
const SCOPED_DIRECTORY = '/home/synthetic/my project/ünï';

const readWire = (name: string): string => readFileSync(resolve(repoRoot, wireDirectory, `${name}.json`), 'utf8');

type RecordedRequest = {
  name: string;
  method: string;
  path: string;
  query: string[][];
  directoryHeader: string | null;
  partShellsHeader: string | null;
};

const responseFor = (path: string, shelled: boolean): string => {
  if (path === '/api/session') return readWire('sessions');
  if (path === `/api/session/${RECORDED_SESSION}/message`) return readWire('messages-recorded');
  if (path === `/api/session/${LONG_SESSION}/message`) return readWire('messages-long');
  if (path === `/api/session/${TOOLS_SESSION}/message`) return readWire(shelled ? 'messages-tools-shelled' : 'messages-tools');
  if (path === `/api/session/${TOOLS_SESSION}/message/${TOOLS_MESSAGE}`) return readWire('message-tools-single');
  throw new Error(`no recorded response for ${path}`);
};

const createRecordingClient = (requests: RecordedRequest[], headers?: Record<string, string>) => {
  let pendingName = '';
  const client = OpenCode.make({
    baseUrl: BASE_URL,
    headers,
    fetch: async (input: string | URL | Request, init?: RequestInit) => {
      const request = new Request(input, init);
      const url = new URL(request.url);
      requests.push({
        name: pendingName,
        method: request.method,
        path: url.pathname,
        query: [...url.searchParams.entries()].sort(([left], [right]) => (left < right ? -1 : 1)),
        directoryHeader: request.headers.get('x-opencode-directory'),
        partShellsHeader: request.headers.get(PART_SHELLS_HEADER),
      });
      const shelled = request.headers.get(PART_SHELLS_HEADER) === PART_SHELLS_VERSION;
      return new Response(responseFor(url.pathname, shelled), { headers: { 'content-type': 'application/json' } });
    },
  });
  return {
    client,
    named: <T,>(name: string, call: () => Promise<T>): Promise<T> => {
      pendingName = name;
      return call();
    },
  };
};

const normalizeSession = (session: Session) => ({
  id: session.id,
  parentId: session.parentID ?? null,
  directory: session.directory,
  title: session.title,
  created: session.time.created,
  updated: session.time.updated,
  idle: session.time.idle ?? null,
  archived: session.time.archived ?? null,
  pinned: isSessionPinnedInMetadata(session),
  inWork: isSessionInWork(session),
  btw: isBtwSession(session),
});

const normalizePart = (part: Part) => {
  switch (part.type) {
    case 'text':
    case 'reasoning':
      return { id: part.id, type: part.type, text: part.text };
    case 'tool':
      return {
        id: part.id,
        type: 'tool',
        tool: part.tool,
        status: part.state.status,
        output: part.state.status === 'completed' ? part.state.output : part.state.status === 'error' ? part.state.error : null,
        elided: toolContentShellChars(part),
      };
    case 'file':
      return { id: part.id, type: 'file', filename: part.filename ?? '', mime: part.mime };
    case 'agent':
      return { id: part.id, type: 'agent', name: part.name };
  }
};

/** The installed client's version; its package exports do not include the manifest. */
const installedClientVersion = (): string =>
  z.object({ version: z.string() }).parse(JSON.parse(readFileSync(resolve(repoRoot, 'node_modules/@opencode/client/package.json'), 'utf8'))).version;

type Overlay = { pinned: boolean; inWork: boolean; btw: boolean };

/** One session-level effect, as the Rust parity test normalizes its events. */
type SessionEffect =
  | { type: 'created'; session: ReturnType<typeof normalizeSession> }
  | {
      type: 'patched';
      sessionId: string;
      title?: string;
      directory?: string;
      projectId?: string;
      updated?: number;
      idle?: number;
      archived?: number | null;
      overlay?: Overlay;
    }
  | { type: 'deleted'; sessionId: string }
  | { type: 'status'; sessionId: string; status: string };

const frameSchema = z.object({ type: z.string(), payload: z.unknown().optional(), directory: z.string().optional() });

/**
 * The session-level effects of the recorded frames, as the UI classifies them:
 * session records, the patch fields the native index models, removals and
 * live status. A patch that changes none of those fields is left out.
 */
const sessionEffects = (frames: string, base: Session) => {
  const overlay = (metadata: Metadata | undefined): Overlay => {
    const session = { ...base, metadata };
    return { pinned: isSessionPinnedInMetadata(session), inWork: isSessionInWork(session), btw: isBtwSession(session) };
  };
  const patched = (sessionID: string, patch: SessionPatch): SessionEffect[] => {
    const fields = compact({
      title: patch.title,
      directory: patch.directory,
      projectId: patch.projectID,
      updated: patch.time?.updated,
      idle: patch.time?.idle,
      archived: patch.time?.archived,
      overlay: patch.metadata === undefined ? undefined : overlay(patch.metadata),
    });
    return Object.keys(fields).length > 0 ? [{ type: 'patched', sessionId: sessionID, ...fields }] : [];
  };
  const effects = (event: SyncEvent): SessionEffect[] => {
    switch (event.type) {
      case 'session.created':
        return [{ type: 'created', session: normalizeSession(event.properties.info) }];
      case 'session.patched':
        return patched(event.properties.sessionID, event.properties.patch);
      case 'session.deleted':
        return [{ type: 'deleted', sessionId: event.properties.sessionID }];
      case 'session.status':
        return [{ type: 'status', sessionId: event.properties.sessionID, status: event.properties.status.type }];
      case 'session.idle':
      case 'session.error':
        return [{ type: 'status', sessionId: event.properties.sessionID, status: 'idle' }];
      default:
        return [];
    }
  };
  return z.array(frameSchema).parse(JSON.parse(frames)).flatMap((frame) => {
    if (frame.type !== 'event') return [];
    const classified = classifyStreamPayload(frame.payload, frame.directory);
    return classified.kind === 'events' ? classified.events.flatMap(({ event }) => effects(event)) : [];
  });
};

/** Renders the checked-in fixture with the official client and the UI projection. */
export const renderTransportFixtures = async (): Promise<string> => {
  const requests: RecordedRequest[] = [];
  const global = createRecordingClient(requests);
  const scoped = createRecordingClient(requests, { 'x-opencode-directory': encodeURIComponent(SCOPED_DIRECTORY) });

  const sessions = await global.named('global sessions page', () => global.client.session.list({ limit: 500 }));
  await global.named('global sessions next page', () => global.client.session.list({ limit: 500, cursor: 'CURSOR_FROM_PREVIOUS_PAGE' }));
  await scoped.named('directory sessions page', () => scoped.client.session.list({ directory: SCOPED_DIRECTORY, limit: 500 }));
  const recorded = await global.named('latest messages', () => global.client.message.list({ sessionID: RECORDED_SESSION, limit: 50 }));
  const long = await global.named('latest messages, long session', () => global.client.message.list({ sessionID: LONG_SESSION, limit: 20 }));
  const tools = await global.named('latest messages, tools session', () => global.client.message.list({ sessionID: TOOLS_SESSION, limit: 50 }));
  const shelled = await global.named('latest messages, shelled', () =>
    global.client.message.list({ sessionID: TOOLS_SESSION, limit: 50 }, { headers: { [PART_SHELLS_HEADER]: PART_SHELLS_VERSION } }),
  );
  const single = await global.named('whole message', () => global.client.session.message.get({ sessionID: TOOLS_SESSION, messageID: TOOLS_MESSAGE }));

  const normalizeMessages = (data: Parameters<typeof projectMessages>[0], sessionID: string) =>
    projectMessages(data, sessionID).map(({ message, parts }) => ({
      id: message.id,
      role: message.role,
      created: message.time.created,
      completed: 'completed' in message.time ? message.time.completed ?? null : null,
      parts: parts.map(normalizePart),
    }));
  // `getSessionMessages`: a page shorter than its limit is the oldest one.
  const page = (response: typeof recorded, sessionID: string, limit: number) => {
    const items = normalizeMessages(response.data, sessionID);
    return { limit, items, older: items.length < limit ? null : response.cursor.next ?? null };
  };

  const fixture = {
    client: `@opencode/client@${installedClientVersion()}`,
    baseUrl: BASE_URL,
    scopedDirectory: SCOPED_DIRECTORY,
    requests,
    sessions: sessions.data.map(projectSession).map(normalizeSession),
    nextCursor: sessions.cursor.next ?? null,
    messages: {
      recorded: page(recorded, RECORDED_SESSION, 50),
      long: page(long, LONG_SESSION, 20),
      tools: page(tools, TOOLS_SESSION, 50),
      shelled: page(shelled, TOOLS_SESSION, 50),
    },
    wholeMessage: normalizeMessages([single], TOOLS_SESSION)[0],
    sessionEvents: {
      recorded: sessionEffects(readWire('frames-recorded'), projectSession(sessions.data[0])),
      tools: sessionEffects(readWire('frames-tools'), projectSession(sessions.data[0])),
    },
  };
  return `${JSON.stringify(fixture, null, 2)}\n`;
};

const writeTransportFixtures = async (): Promise<string> => {
  const target = resolve(repoRoot, TRANSPORT_FIXTURE_PATH);
  mkdirSync(dirname(target), { recursive: true });
  writeFileSync(target, await renderTransportFixtures(), 'utf8');
  return target;
};

if (process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  console.log(`wrote ${await writeTransportFixtures()}`);
}
