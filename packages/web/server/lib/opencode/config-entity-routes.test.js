import { describe, it, expect } from 'vitest';
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
