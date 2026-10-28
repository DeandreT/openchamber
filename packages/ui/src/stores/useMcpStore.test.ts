import { afterEach, beforeEach, describe, expect, spyOn, test } from 'bun:test';
import { opencodeClient } from '@/lib/opencode/client';
import type { McpServerStatus } from '@/lib/opencode/model';
import { useMcpStore } from './useMcpStore';

const DIRECTORY = '/workspace/project';

type ServerState = McpServerStatus['status'];

// OpenCode's transitions as a real 2.2.0 host shows them: connect starts the
// server, disconnect leaves it `disabled` until someone connects it again.
let server: ServerState | null;
const spies: Array<{ mockRestore: () => void }> = [];
let disconnects = 0;

beforeEach(() => {
  disconnects = 0;
  useMcpStore.getState().resetForRuntimeSwitch();
  spies.push(
    spyOn(opencodeClient, 'listMcpServers').mockImplementation(async () => (server ? [{ name: 'lab', status: server }] : [])),
    spyOn(opencodeClient, 'connectMcpServer').mockImplementation(async () => {
      server = { status: 'connected' };
    }),
    spyOn(opencodeClient, 'disconnectMcpServer').mockImplementation(async () => {
      disconnects += 1;
      server = { status: 'disabled' };
    }),
  );
});

afterEach(() => {
  for (const spy of spies.splice(0)) spy.mockRestore();
});

const testFrom = async (before: ServerState | null) => {
  server = before;
  await useMcpStore.getState().refresh({ directory: DIRECTORY, silent: true });
  const result = await useMcpStore.getState().testConnection('lab', DIRECTORY);
  return { result, after: useMcpStore.getState().getStatusForDirectory(DIRECTORY).lab?.status.status };
};

describe('testConnection', () => {
  const repairable: ServerState[] = [
    { status: 'failed', error: 'process exited with code 1' },
    { status: 'needs_auth', error: 'sign in required' },
    { status: 'pending' },
  ];
  for (const before of repairable) {
    test(`a ${before.status} server that now connects stays connected`, async () => {
      const { result, after } = await testFrom(before);

      expect(result.status?.status.status).toBe('connected');
      expect(disconnects).toBe(0);
      expect(after).toBe('connected');
    });
  }

  test('a server with no status held yet stays connected', async () => {
    const { after } = await testFrom(null);

    expect(disconnects).toBe(0);
    expect(after).toBe('connected');
  });

  test('a server the user turned off is turned off again after the test', async () => {
    const { result, after } = await testFrom({ status: 'disabled' });

    expect(result.status?.status.status).toBe('connected');
    expect(disconnects).toBe(1);
    expect(after).toBe('disabled');
  });

  test('a connected server is left connected', async () => {
    const { after } = await testFrom({ status: 'connected' });

    expect(disconnects).toBe(0);
    expect(after).toBe('connected');
  });
});
