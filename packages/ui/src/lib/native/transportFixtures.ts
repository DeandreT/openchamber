/**
 * Differential fixtures for the native desktop's transport.
 *
 * The official `@opencode/client` runs against recorded server responses
 * through a `fetch` that also records every request it sends. Its decoded
 * results go through the UI's own projection. The output pins three things
 * the Rust transport must reproduce: the requests (method, path, query,
 * directory header), the projected sessions and messages, and the catalogs
 * the composer selects from (agents, models, providers, the folded OpenCode
 * config and MCP status). See `docs/rust-desktop/OPENCODE_ADAPTER.md`.
 *
 *   bun run native:transport-fixtures
 */
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { OpenCode } from '@opencode/client';
import { z } from 'zod';

import type { SessionPatch, SyncEvent } from '@/lib/opencode/events';
import { configModelIdentifier, mergeConfigDocuments, projectAgent, projectMessages, projectSession } from '@/lib/opencode/projection';
import { compact, type Agent, type Metadata, type Part, type Session } from '@/lib/opencode/model';
import { listModelVariantIds } from '@/lib/modelVariants';
import { isAgentHidden } from '@/stores/useAgentsStore';
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
  contentType: string | null;
  body: string | null;
};

const responseFor = (path: string, shelled: boolean): string => {
  if (path === '/api/session') return readWire('sessions');
  if (path === '/api/agent') return readWire('agents');
  if (path === '/api/model') return readWire('models');
  if (path === '/api/model/default') return readWire('model-default');
  if (path === '/api/provider') return readWire('providers');
  if (path === '/api/config') return readWire('config');
  if (path === '/api/mcp') return readWire('mcp');
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
        contentType: request.headers.get('content-type'),
        body: request.body ? await request.text() : null,
      });
      const shelled = request.headers.get(PART_SHELLS_HEADER) === PART_SHELLS_VERSION;
      if (request.method === 'POST') {
        if (url.pathname.endsWith('/model') || url.pathname.endsWith('/agent')) return new Response(null, { status: 204 });
        if (url.pathname.endsWith('/prompt')) return new Response(readWire('prompt-admitted'), { headers: { 'content-type': 'application/json' } });
        if (url.pathname.endsWith('/interrupt')) return Response.json({ interrupted: pendingName !== 'interrupt idle session' });
      }
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

const normalizeModelRef = (model: Session['model'] | undefined) =>
  model ? { providerId: model.providerID, modelId: model.id, variant: model.variant ?? null } : null;

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
  model: normalizeModelRef(session.model),
  agent: session.agent || null,
});

const normalizePart = (part: Part) => {
  switch (part.type) {
    case 'text':
    case 'reasoning':
      return {
        id: part.id,
        type: part.type,
        text: part.text,
        time: { start: part.time?.start ?? null, end: part.time?.end ?? null },
      };
    case 'tool':
      return {
        id: part.id,
        type: 'tool',
        tool: part.tool,
        status: part.state.status,
        output: part.state.status === 'completed' ? part.state.output : part.state.status === 'error' ? part.state.error : null,
        elided: toolContentShellChars(part),
        time: {
          start: 'time' in part.state ? part.state.time.start : null,
          end: 'time' in part.state && 'end' in part.state.time ? part.state.time.end : null,
        },
      };
    case 'file':
      return { id: part.id, type: 'file', filename: part.filename ?? '', mime: part.mime };
    case 'agent':
      return { id: part.id, type: 'agent', name: part.name };
  }
};

const normalizeAgent = (agent: Agent) => ({
  name: agent.name,
  displayName: agent.displayName ?? null,
  mode: agent.mode ?? null,
  hidden: isAgentHidden(agent),
  model: normalizeModelRef(agent.model),
  color: agent.color ?? null,
});

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
      model?: ReturnType<typeof normalizeModelRef>;
      agent?: string;
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
      model: patch.model === undefined ? undefined : normalizeModelRef(patch.model),
      agent: patch.agent || undefined,
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
  await global.named('earlier messages, shelled', () =>
    global.client.message.list(
      { sessionID: TOOLS_SESSION, limit: 50, cursor: 'CURSOR_FROM_PREVIOUS_PAGE' },
      { headers: { [PART_SHELLS_HEADER]: PART_SHELLS_VERSION } },
    ),
  );
  const single = await global.named('whole message', () => global.client.session.message.get({ sessionID: TOOLS_SESSION, messageID: TOOLS_MESSAGE }));
  // The composer's catalogs, read in the active project as `getProvidersForConfig`,
  // `listAgents`, `getConfig` and `listMcpServers` do.
  const agents = await scoped.named('agents', () => scoped.client.agent.list());
  const models = await scoped.named('models', () => scoped.client.model.list());
  const defaultModel = await scoped.named('default model', () => scoped.client.model.default());
  const providers = await scoped.named('providers', () => scoped.client.provider.list());
  const config = mergeConfigDocuments(await scoped.named('config', () => scoped.client.config.get()));
  const mcp = await scoped.named('mcp servers', () => scoped.client.mcp.list());

  // Synthetic protocol samples, decoded by the pinned SDK. No live mutation.
  await global.named('select prompt model', () => global.client.session.switchModel({
    sessionID: RECORDED_SESSION, model: { providerID: 'fixture', id: 'text-model', variant: 'high' },
  }));
  await global.named('clear prompt variant', () => global.client.session.switchModel({
    sessionID: RECORDED_SESSION, model: { providerID: 'fixture', id: 'text-model' },
  }));
  await global.named('select prompt agent', () => global.client.session.switchAgent({ sessionID: RECORDED_SESSION, agent: 'build' }));
  const admitted = await global.named('submit text prompt', () => global.client.session.prompt({
    sessionID: RECORDED_SESSION, id: 'msg_native_fixture_exact', text: 'Hello\n世界 ', delivery: 'queue',
  }));
  await global.named('submit default delivery', () => global.client.session.prompt({
    sessionID: RECORDED_SESSION, id: 'msg_native_fixture_exact', text: 'Hello\n世界 ',
  }));
  const interrupted = await global.named('interrupt named session', () => global.client.session.interrupt({ sessionID: RECORDED_SESSION }));
  const idle = await global.named('interrupt idle session', () => global.client.session.interrupt({ sessionID: RECORDED_SESSION }));

  const normalizeMessages = (data: Parameters<typeof projectMessages>[0], sessionID: string) =>
    projectMessages(data, sessionID).map(({ message, parts }) => ({
      id: message.id,
      role: message.role,
      created: message.time.created,
      completed: 'completed' in message.time ? message.time.completed ?? null : null,
      assistant: message.role === 'assistant'
        ? {
            agent: message.agent || null,
            model: normalizeModelRef(message.providerID && message.modelID
              ? { providerID: message.providerID, id: message.modelID, variant: message.variant }
              : undefined),
            finish: message.finish ?? null,
            error: message.error?.message ?? null,
            // Billionths of a dollar, as the native client keeps costs.
            cost: message.cost === undefined ? null : Math.round(message.cost * 1e9),
            tokens: message.tokens
              ? {
                  input: message.tokens.input,
                  output: message.tokens.output,
                  reasoning: message.tokens.reasoning,
                  cacheRead: message.tokens.cache.read,
                  cacheWrite: message.tokens.cache.write,
                }
              : null,
          }
        : null,
      compaction: message.role === 'compaction' ? message.status ?? null : null,
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
    mutations: { admitted, interrupted, idle },
    sessions: sessions.data.map(projectSession).map(normalizeSession),
    nextCursor: sessions.cursor.next ?? null,
    messages: {
      recorded: page(recorded, RECORDED_SESSION, 50),
      long: page(long, LONG_SESSION, 20),
      tools: page(tools, TOOLS_SESSION, 50),
      shelled: page(shelled, TOOLS_SESSION, 50),
    },
    wholeMessage: normalizeMessages([single], TOOLS_SESSION)[0],
    catalog: {
      agents: agents.data.map(projectAgent).map(normalizeAgent),
      models: models.data.map((model) => ({
        id: model.id,
        modelId: model.modelID,
        providerId: model.providerID,
        name: model.name ?? null,
        enabled: model.enabled,
        variants: listModelVariantIds(model.variants),
        // `limitsForAnsweringModel` reads only a positive window.
        contextLimit: (model.limit?.context ?? 0) > 0 ? model.limit.context : null,
      })),
      providers: providers.data.map((provider) => ({ id: provider.id, name: provider.name ?? null, enabled: provider.activation !== 'disabled' })),
      defaultModel: defaultModel.data ? { providerId: defaultModel.data.providerID, modelId: defaultModel.data.modelID } : null,
      opencodeDefaults: {
        defaultAgent: typeof config.default_agent === 'string' && config.default_agent.trim() ? config.default_agent.trim() : null,
        model: configModelIdentifier(config.model) ?? null,
      },
      mcpConnected: mcp.data.filter((server) => server.status.status === 'connected').length,
    },
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
