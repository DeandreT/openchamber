import { beforeEach, describe, expect, mock, test } from 'bun:test';
import type { McpDraft } from './useMcpConfigStore';

/** The parts of the MCP write body these tests make claims about. */
type CapturedMcpBody = {
  protocol?: string | null;
  codemode?: boolean | null;
  oauth?: {
    client_id?: string;
    callback_port?: number;
    auth_server_metadata_url?: string;
  } | false;
};

type CapturedRequest = { url: string; method: string; body: CapturedMcpBody };

let captured: CapturedRequest[] = [];
let listResponse: unknown[] = [];
const queuedReads: Array<Response | Promise<Response>> = [];

const runtimeFetchMock = async (url: string, init?: RequestInit) => {
  const method = init?.method ?? 'GET';
  if (method === 'GET') {
    return queuedReads.shift() ?? Response.json(listResponse);
  }
  captured.push({ url, method, body: JSON.parse(String(init?.body ?? '{}')) });
  return new Response(JSON.stringify({}), { headers: { 'Content-Type': 'application/json' } });
};

mock.module('@/lib/runtime-fetch', () => ({ runtimeFetch: runtimeFetchMock }));

mock.module('@/lib/opencode/client', () => ({
  opencodeClient: { getDirectory: () => '/workspace/project' },
}));

mock.module('@/stores/useProjectsStore', () => ({
  useProjectsStore: {
    getState: () => ({ getActiveProject: () => ({ path: '/workspace/project' }) }),
  },
}));

mock.module('@/lib/configUpdate', () => ({
  startConfigUpdate: mock(() => undefined),
  finishConfigUpdate: mock(() => undefined),
  updateConfigUpdateMessage: mock(() => undefined),
}));

mock.module('@/stores/useAgentsStore', () => ({
  refreshAfterOpenCodeRestart: mock(async () => undefined),
}));

const { hasLoadedMcpConfigsForDirectory, useMcpConfigStore } = await import('./useMcpConfigStore');

/** What the MCP page hands the store for a remote server with nothing set. */
const remoteDraft = (overrides: Partial<McpDraft> = {}): McpDraft => ({
  name: 'example',
  scope: 'user',
  type: 'remote',
  command: [],
  url: 'https://mcp.example.com',
  environment: [],
  headers: [],
  oauthEnabled: true,
  oauthClientId: '',
  oauthClientSecret: '',
  oauthScope: '',
  oauthRedirectUri: '',
  oauthCallbackPort: '',
  oauthAuthServerMetadataUrl: '',
  protocol: 'legacy',
  timeoutStartup: '',
  timeoutCatalog: '',
  timeoutExecution: '',
  codemode: 'default',
  disabled: false,
  ...overrides,
});

const lastBody = () => captured[captured.length - 1].body;

describe('useMcpConfigStore MCP body', () => {
  beforeEach(() => {
    captured = [];
    listResponse = [];
  });

  test('saving legacy writes a removal so the config file stays clean', async () => {
    await useMcpConfigStore.getState().updateMcp('example', remoteDraft({ protocol: 'legacy' }));
    expect(lastBody().protocol).toBeNull();
  });

  test('Code Mode default writes a removal so OpenCode decides; on and off write the value', async () => {
    await useMcpConfigStore.getState().updateMcp('example', remoteDraft({ codemode: 'default' }));
    expect(lastBody().codemode).toBeNull();

    await useMcpConfigStore.getState().updateMcp('example', remoteDraft({ codemode: 'on' }));
    expect(lastBody().codemode).toBe(true);

    await useMcpConfigStore.getState().updateMcp('example', remoteDraft({ codemode: 'off' }));
    expect(lastBody().codemode).toBe(false);
  });

  test('saving a non-default protocol writes the value', async () => {
    await useMcpConfigStore.getState().updateMcp('example', remoteDraft({ protocol: 'auto' }));
    expect(lastBody().protocol).toBe('auto');

    await useMcpConfigStore.getState().updateMcp('example', remoteDraft({ protocol: '2026-07-28' }));
    expect(lastBody().protocol).toBe('2026-07-28');
  });

  test('a local server carries the protocol too', async () => {
    await useMcpConfigStore.getState().createMcp(remoteDraft({
      type: 'local',
      url: '',
      command: ['npx', 'some-mcp'],
      protocol: 'auto',
    }));
    expect(lastBody().protocol).toBe('auto');
  });

  test('the metadata URL is written into the oauth block', async () => {
    await useMcpConfigStore.getState().updateMcp('example', remoteDraft({
      oauthAuthServerMetadataUrl: '  https://auth.example.com/.well-known/oauth-authorization-server  ',
    }));
    expect(lastBody().oauth).toEqual({
      auth_server_metadata_url: 'https://auth.example.com/.well-known/oauth-authorization-server',
    });
  });

  test('emptying the metadata URL removes the key but keeps the rest of the block', async () => {
    await useMcpConfigStore.getState().updateMcp('example', remoteDraft({
      oauthClientId: 'id',
      oauthCallbackPort: '4242',
      oauthAuthServerMetadataUrl: '',
    }));
    expect(lastBody().oauth).toEqual({ client_id: 'id', callback_port: 4242 });
  });

  test('a server with OAuth turned off stays off', async () => {
    await useMcpConfigStore.getState().updateMcp('example', remoteDraft({
      oauthEnabled: false,
      oauthAuthServerMetadataUrl: 'https://auth.example.com/meta',
    }));
    expect(lastBody().oauth).toBe(false);
  });
});

describe('useMcpConfigStore round trip', () => {
  beforeEach(() => {
    captured = [];
    listResponse = [];
  });

  test('a stored entry exposes protocol and the metadata URL to the editor', async () => {
    listResponse = [{
      name: 'example',
      type: 'remote',
      url: 'https://mcp.example.com',
      protocol: '2026-07-28',
      oauth: {
        client_id: 'id',
        auth_server_metadata_url: 'https://auth.example.com/meta',
      },
    }];
    await useMcpConfigStore.getState().loadMcpConfigs({ force: true });

    const entry = useMcpConfigStore.getState().getMcpByName('example');
    expect(entry?.protocol).toBe('2026-07-28');
    expect(entry?.type === 'remote' && entry.oauth !== false ? entry.oauth?.auth_server_metadata_url : null)
      .toBe('https://auth.example.com/meta');
  });

  test('a v1 entry without either key still loads', async () => {
    listResponse = [{ name: 'old', type: 'local', command: ['a'], legacy: true }];
    await useMcpConfigStore.getState().loadMcpConfigs({ force: true });

    const entry = useMcpConfigStore.getState().getMcpByName('old');
    expect(entry?.legacy).toBe(true);
    expect(entry?.protocol).toBeUndefined();
  });
});

describe('saved MCP list reads', () => {
  const directory = '/workspace/project';
  const server = { name: 'paused', type: 'local', command: ['example'], disabled: true };
  const store = useMcpConfigStore.getState;
  const failed = () => Response.json({ error: 'Invalid JSONC at offset 7' }, { status: 500 });

  beforeEach(() => {
    store().resetForRuntimeSwitch();
    queuedReads.length = 0;
    listResponse = [];
  });

  test('a failed first read is unknown; a successful empty read is authoritative', async () => {
    queuedReads.push(failed());
    expect(await store().loadMcpConfigs({ directory })).toBe(false);
    expect(hasLoadedMcpConfigsForDirectory(store(), directory)).toBe(false);
    expect(store().getReadErrorForDirectory(directory)).toBe('Invalid JSONC at offset 7');
    expect(store().getReadErrorForDirectory('/other')).toBeUndefined();
    expect(await store().loadMcpConfigs({ directory })).toBe(true);
    expect(hasLoadedMcpConfigsForDirectory(store(), directory)).toBe(true);
    expect(store().getReadErrorForDirectory(directory)).toBeUndefined();
  });

  test('a failed forced read keeps the row and a normal revisit retries even within its cache lifetime', async () => {
    listResponse = [server];
    await store().loadMcpConfigs({ directory });
    queuedReads.push(failed());
    expect(await store().loadMcpConfigs({ directory, force: true })).toBe(false);
    expect(store().getMcpByName(server.name, directory)).toEqual(server);
    listResponse = [];
    expect(await store().loadMcpConfigs({ directory })).toBe(true);
    expect(store().getMcpByName(server.name, directory)).toBeUndefined();
    expect(store().getReadErrorForDirectory(directory)).toBeUndefined();
  });

  test('an unreadable list without a host diagnostic leaves the error wording to the localized grid', async () => {
    queuedReads.push(Response.json({}));
    expect(await store().loadMcpConfigs({ directory })).toBe(false);
    expect(hasLoadedMcpConfigsForDirectory(store(), directory)).toBe(false);
    expect(store().getReadErrorForDirectory(directory)).toBe('');
  });

  test('a late failure cannot replace the result of a newer read', async () => {
    let finish!: (response: Response) => void;
    queuedReads.push(new Promise<Response>((resolve) => { finish = resolve; }));
    const older = store().loadMcpConfigs({ directory });
    listResponse = [server];
    expect(await store().loadMcpConfigs({ directory, force: true })).toBe(true);
    finish(failed());
    expect(await older).toBe(false);
    expect(store().getReadErrorForDirectory(directory)).toBeUndefined();
    expect(store().getMcpByName(server.name, directory)).toEqual(server);
  });

  test('switching hosts drops same-path rows and errors, including a late response from the old host', async () => {
    listResponse = [server];
    await store().loadMcpConfigs({ directory });
    queuedReads.push(failed());
    await store().loadMcpConfigs({ directory, force: true });
    let finish!: (response: Response) => void;
    queuedReads.push(new Promise<Response>((resolve) => { finish = resolve; }));
    const oldHost = store().loadMcpConfigs({ directory });
    store().resetForRuntimeSwitch();
    expect(hasLoadedMcpConfigsForDirectory(store(), directory)).toBe(false);
    expect(store().getReadErrorForDirectory(directory)).toBeUndefined();
    listResponse = [];
    expect(await store().loadMcpConfigs({ directory })).toBe(true);
    finish(Response.json([server]));
    expect(await oldHost).toBe(false);
    expect(store().getMcpServersForDirectory(directory)).toEqual([]);
    expect(store().mcpServers).toEqual([]);
  });
});
