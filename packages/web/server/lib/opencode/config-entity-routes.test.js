import { describe, it, expect, vi } from 'vitest';
import express from 'express';
import request from 'supertest';
import { registerConfigEntityRoutes } from './config-entity-routes.js';

const createApp = (overrides = {}) => {
  const calls = [];
  const record = (action) => (name) => {
    calls.push({ action, name });
    return {};
  };
  const app = express();
  app.use(express.json());
  registerConfigEntityRoutes(app, {
    resolveProjectDirectory: async () => ({ directory: '/work/project' }),
    resolveOptionalProjectDirectory: async () => ({ directory: '/work/project' }),
    getAgentSources: () => ({ md: { exists: false }, json: { exists: false } }),
    createAgent: record('createAgent'),
    deleteAgent: record('deleteAgent'),
    createCommand: record('createCommand'),
    deleteCommand: record('deleteCommand'),
    ...overrides,
  });
  return { app, calls };
};

describe('config write logging', () => {
  it.each([
    ['post', 'agents', 'createAgent', 'prompt'],
    ['patch', 'agents', 'updateAgent', 'prompt'],
    ['post', 'commands', 'createCommand', 'template'],
    ['patch', 'commands', 'updateCommand', 'template'],
  ])('keeps %s %s content out of the server log', async (method, entity, writeName, field) => {
    const text = 'private text from a test fixture';
    const body = { [field]: text };
    const write = vi.fn(() => ({}));
    const log = vi.spyOn(console, 'log').mockImplementation(() => {});
    try {
      const { app } = createApp({ [writeName]: write });
      await request(app)[method](`/api/config/${entity}/fixture`).send(body).expect(200);
      expect(write.mock.calls[0]?.[1]).toEqual(body);
      expect(JSON.stringify(log.mock.calls)).not.toContain(text);
    } finally {
      log.mockRestore();
    }
  });
});

describe('MCP saves from a remote origin', () => {
  const createMcpApp = () => {
    const saved = [];
    const app = express();
    app.use(express.json());
    const record = (name, config) => {
      saved.push({ name, config });
      return {};
    };
    registerConfigEntityRoutes(app, {
      resolveProjectDirectory: async () => ({ directory: '/work/project' }),
      resolveOptionalProjectDirectory: async () => ({ directory: '/work/project' }),
      createMcpConfig: record,
      updateMcpConfig: record,
    });
    return { app, saved };
  };

  it.each([
    ['nothing', undefined],
    ['oauth off', false],
    ['an empty block', {}],
    ['a custom redirect with an explicit port', { redirect_uri: 'https://proxy.example.com:8443/cb' }],
    ['a custom origin on the callback path', {
      redirect_uri: 'https://proxy.example.com/api/mcp/oauth/docs/callback',
      callback_port: 19876,
    }],
  ])('stores %s as sent; sign-in prepares the redirect', async (_label, oauth) => {
    const { app, saved } = createMcpApp();
    const body = { type: 'remote', url: 'https://mcp.example.com', oauth };

    await request(app)
      .post('/api/config/mcp/docs')
      .set('Host', 'chamber.example.com')
      .set('X-Forwarded-Proto', 'https')
      .send(body)
      .expect(200);
    await request(app)
      .patch('/api/config/mcp/docs')
      .set('Host', 'chamber.example.com')
      .set('X-Forwarded-Proto', 'https')
      .send(body)
      .expect(200);

    expect(saved.map((entry) => entry.config.oauth)).toEqual([oauth, oauth]);
  });
});

describe('config entity names', () => {
  it.each([
    ['post', '/api/config/agents/..%2F..%2Fescape'],
    ['delete', '/api/config/agents/%2Ftmp%2Fescape'],
    ['post', '/api/config/commands/group%2F..%2F..%2Fescape'],
    ['delete', '/api/config/commands/..%5Cescape'],
  ])('refuses %s %s before it reaches the filesystem', async (method, url) => {
    const { app, calls } = createApp();
    const response = await request(app)[method](url).send({});
    expect(response.status).toBe(400);
    expect(calls).toEqual([]);
  });

  it('accepts plain and nested names', async () => {
    const { app, calls } = createApp();
    expect((await request(app).post('/api/config/agents/reviewer').send({})).status).toBe(200);
    expect((await request(app).post('/api/config/commands/git%2Fcommit').send({})).status).toBe(200);
    expect(calls).toEqual([
      { action: 'createAgent', name: 'reviewer' },
      { action: 'createCommand', name: 'git/commit' },
    ]);
  });
});

describe('built-in purpose model writes', () => {
  const ordinary = { id: 'ordinary', activation: 'enabled', package: '@opencode/ai/providers/openai', settings: {} };
  const model = { id: 'text', providerID: 'ordinary', enabled: true, type: 'chat', capabilities: { output: ['text'] }, variants: [] };
  const setup = (catalog, overrides = {}) => {
    const saved = [];
    const { app } = createApp({
      readPurposeModelCatalog: async () => { if (catalog instanceof Error) throw catalog; return catalog; },
      updateAgent: (name, value) => { saved.push({ name, value }); return {}; },
      ...overrides,
    });
    return { app, saved };
  };
  it('refuses native providers for purpose models before writing', async () => {
    const f = setup({ providers: [{ ...ordinary, settings: { openchamber: { nativeAgent: { protocolVersion: 1 } } } }], models: [model] });
    expect((await request(f.app).patch('/api/config/agents/compaction').send({ model: 'ordinary/text' })).status).toBe(403);
    expect(f.saved).toEqual([]);
  });
  it('refuses unavailable catalogs without writing a fallback', async () => {
    const f = setup(new Error('unavailable'));
    expect((await request(f.app).patch('/api/config/agents/title').send({ model: 'ordinary/text' })).status).toBe(503);
    expect(f.saved).toEqual([]);
  });
  it('keeps primary native choices and unrelated or null purpose edits independent of catalogs', async () => {
    const f = setup(new Error('unavailable'));
    for (const [name, update] of [['build', { model: 'devin/local' }], ['title', { description: 'Keep this title instruction' }], ['compaction', { model: null }]]) {
      expect((await request(f.app).patch(`/api/config/agents/${name}`).send(update)).status).toBe(200);
    }
    expect(f.saved).toHaveLength(3);
  });
  it('accepts an explicitly chosen configured ordinary text model', async () => {
    const f = setup({ providers: [ordinary], models: [model] });
    expect((await request(f.app).patch('/api/config/agents/title').send({ model: 'ordinary/text' })).status).toBe(200);
    expect(f.saved).toHaveLength(1);
  });
  it.each([[], null, { models: [], providers: [ordinary] }, { models: [model], providers: [] }])('refuses a cold or malformed catalog', async (catalog) => {
    const f = setup(catalog);
    expect((await request(f.app).patch('/api/config/agents/title').send({ model: 'ordinary/text' })).status).toBe(503);
    expect(f.saved).toEqual([]);
  });
  it('allows an unchanged unavailable purpose model without reading a catalog', async () => {
    const f = setup(new Error('must not read'), { getAgentConfig: () => ({ config: { model: 'missing/saved#high' } }) });
    expect((await request(f.app).patch('/api/config/agents/title').send({ model: 'missing/saved#high', description: 'Keep' })).status).toBe(200);
    expect(f.saved).toHaveLength(1);
  });
  it.each(['team/title', 'compaction-custom', 'primary'])('leaves %s native model choices alone', async (name) => {
    const f = setup(new Error('must not read'));
    expect((await request(f.app).patch(`/api/config/agents/${encodeURIComponent(name)}`).send({ model: 'devin/local' })).status).toBe(200);
  });
  it.each(['@opencode/ai/providers/claude-agent', 'aisdk:openchamber-devin', 'aisdk:unknown-agent'])('refuses native or unknown package %s for purpose changes', async (pkg) => {
    const f = setup({ models: [model], providers: [{ ...ordinary, package: pkg }] });
    expect((await request(f.app).patch('/api/config/agents/compaction').send({ model: 'ordinary/text' })).status).toBe(403);
    expect(f.saved).toEqual([]);
  });
  it('accepts catalog variants and rejects missing variants', async () => {
    const f = setup({ models: [{ ...model, variants: [{ id: 'high' }] }], providers: [ordinary] });
    expect((await request(f.app).patch('/api/config/agents/title').send({ model: 'ordinary/text#high' })).status).toBe(200);
    expect((await request(f.app).patch('/api/config/agents/title').send({ model: 'ordinary/text#missing' })).status).toBe(403);
    expect(f.saved).toHaveLength(1);
  });
  it.each(['@opencode/ai/providers/openai/responses', '@opencode/ai/providers/anthropic-compatible', 'aisdk:@ai-sdk/openai-compatible'])('accepts ordinary text protocol %s', async (pkg) => {
    const f = setup({ models: [model], providers: [{ ...ordinary, package: pkg }] });
    expect((await request(f.app).patch('/api/config/agents/title').send({ model: 'ordinary/text' })).status).toBe(200);
  });
  it('does not let a model package disguise a native provider', async () => {
    const f = setup({ models: [{ ...model, package: ordinary.package }], providers: [{ ...ordinary, package: 'aisdk:openchamber-devin' }] });
    expect((await request(f.app).patch('/api/config/agents/title').send({ model: 'ordinary/text' })).status).toBe(403);
    expect(f.saved).toEqual([]);
  });
  it.each(['invalid', '', 4, false])('refuses malformed model %s before writing', async (selection) => {
    const f = setup(new Error('must not read'));
    expect((await request(f.app).patch('/api/config/agents/title').send({ model: selection })).status).toBe(400);
    expect(f.saved).toEqual([]);
  });
  it('reports a failed purpose write without a success receipt', async () => {
    const f = setup({ models: [model], providers: [ordinary] }, { updateAgent: () => { throw new Error('fixture write failure'); } });
    expect((await request(f.app).patch('/api/config/agents/title').send({ model: 'ordinary/text' })).status).toBe(500);
  });
  it('checks captured source immediately before the model-only write', async () => {
    let current = { source: 'json', scope: 'project', path: '/work/opencode.jsonc', config: { model: 'old/text' } };
    const f = setup({ models: [model], providers: [ordinary] }, { getAgentConfig: () => current });
    const before = await request(f.app).get('/api/config/agents/title/config');
    expect(before.status).toBe(200);
    const revision = before.body.modelRevision;
    expect(revision).toMatch(/^[a-f0-9]{64}$/u);
    current = { ...current, scope: 'user', path: '/user/agents/title.md' };
    expect((await request(f.app).patch('/api/config/agents/title').set('If-Match', `"${revision}"`).send({ model: null })).status).toBe(409);
    expect(f.saved).toEqual([]);
    const refreshed = await request(f.app).get('/api/config/agents/title/config');
    expect((await request(f.app).patch('/api/config/agents/title').set('If-Match', `"${refreshed.body.modelRevision}"`).send({ model: null })).status).toBe(200);
    expect(f.saved).toHaveLength(1);
  });
  it('reads eligibility through the current guarded fetch with captured auth and directory', async () => {
    const reads = [];
    const fetching = vi.spyOn(globalThis, 'fetch').mockImplementation(async (input, init) => {
      const outgoing = new Request(input, init);
      const pathname = new URL(outgoing.url).pathname;
      reads.push(pathname);
      expect(outgoing.headers.get('authorization')).toBe('Basic fixture-only');
      expect(outgoing.headers.get('x-opencode-directory')).toBe(encodeURIComponent('/work/project'));
      return new Response(JSON.stringify({ data: pathname.endsWith('/model') ? [model] : [ordinary] }),
        { status: 200, headers: { 'content-type': 'application/json' } });
    });
    try {
      const { app, calls } = createApp({
        buildOpenCodeUrl: () => 'https://fixture.invalid/prefix/',
        getOpenCodeAuthHeaders: () => ({ Authorization: 'Basic fixture-only' }),
        updateAgent: (name) => { calls.push({ action: 'updateAgent', name }); return {}; },
      });
      expect((await request(app).patch('/api/config/agents/title').send({ model: 'ordinary/text' })).status).toBe(200);
      expect(reads.sort()).toEqual(['/prefix/api/model', '/prefix/api/provider']);
      expect(calls).toEqual([{ action: 'updateAgent', name: 'title' }]);
    } finally { fetching.mockRestore(); }
  });
  it('keeps unrelated changes independent of purpose source/model preconditions', async () => {
    const current = { source: 'json', scope: 'project', path: '/work/opencode.jsonc', config: { model: 'old/text' } };
    const f = setup(new Error('must not read'), { getAgentConfig: () => current });
    const before = await request(f.app).get('/api/config/agents/compaction/config');
    current.config.description = 'Unrelated change';
    expect((await request(f.app).patch('/api/config/agents/compaction').set('If-Match', `"${before.body.modelRevision}"`).send({ model: null })).status).toBe(200);
  });
});

describe('snippet expansion source mapping', () => {
  it('keeps the default response bytes and passes opt-in mapping to the same captured directory', async () => {
    const calls = [];
    const mapped = { text: 'expanded', sourceRanges: [] };
    const { app } = createApp({
      resolveOptionalProjectDirectory: async (req) => ({ directory: req.query.directory }),
      expandSnippets: (text, directory, options) => {
        calls.push({ text, directory, options });
        return options?.sourceMap ? mapped : 'expanded';
      },
    });
    const ordinary = await request(app).post('/api/config/snippets/expand?directory=%2Fowner%2Fone').send({ text: '#snippet' });
    expect(ordinary.status).toBe(200);
    expect(ordinary.text).toBe('{"text":"expanded"}');
    const response = await request(app).post('/api/config/snippets/expand?directory=%2Fowner%2Ftwo').send({ text: '#snippet', sourceMap: true });
    expect(response.status).toBe(200);
    expect(response.body).toEqual(mapped);
    expect(calls).toEqual([
      { text: '#snippet', directory: '/owner/one', options: undefined },
      { text: '#snippet', directory: '/owner/two', options: { sourceMap: true } },
    ]);
  });

  it.each([{ sourceMap: true }, { text: {}, sourceMap: true }, { text: 'x', content: 'preview', sourceMap: true }])('refuses invalid mapped request %j without expanding', async (body) => {
    const calls = [];
    const { app } = createApp({ expandSnippets: (...args) => calls.push(args) });
    const response = await request(app).post('/api/config/snippets/expand').send(body);
    expect(response.status).toBe(400);
    expect(response.body.code).toBe('snippet_source_map_invalid_request');
    expect(calls).toEqual([]);
  });

  it('preserves directory refusal and distinguishes a bounded mapping refusal from empty success', async () => {
    const calls = [];
    const refused = createApp({
      resolveOptionalProjectDirectory: async () => ({ error: 'owner unavailable' }),
      expandSnippets: (...args) => calls.push(args),
    });
    const wrongOwner = await request(refused.app).post('/api/config/snippets/expand').send({ text: 'text', sourceMap: true });
    expect(wrongOwner.status).toBe(400);
    expect(calls).toEqual([]);
    const limited = createApp({ expandSnippets: () => { throw Object.assign(new Error('bounded map'), { code: 'snippet_source_map_limit' }); } });
    const oversized = await request(limited.app).post('/api/config/snippets/expand').send({ text: 'text', sourceMap: true });
    expect(oversized.status).toBe(413);
    expect(oversized.body).toEqual({ error: 'Snippet source map exceeds its limit', code: 'snippet_source_map_limit' });
  });
});
