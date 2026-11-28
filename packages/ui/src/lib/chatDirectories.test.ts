import { afterEach, beforeEach, describe, expect, spyOn, test } from 'bun:test';
import { opencodeClient } from '@/lib/opencode/client';
import { switchRuntimeEndpoint } from '@/lib/runtime-switch';
import { useSessionUIStore } from '@/sync/session-ui-store';
import { createChatDirectory, deleteChatDirectory, ensureChatsRootDirectory, getChatsRoot, getChatsRootFromDirectory, isChatDirectoryForHome, isChatDirectoryPath, subscribeChatsRoot, warmChatsRootDirectory } from './chatDirectories';

let runtime = 0;
const nextRuntime = () => switchRuntimeEndpoint({ apiBaseUrl: 'https://chats.test', runtimeKey: `chats-${++runtime}` });
let home = spyOn(opencodeClient, 'getFilesystemHomeInfo');
let mkdir = spyOn(opencodeClient, 'createDirectory');
let request = spyOn(globalThis, 'fetch');
const deleteRequests = () => request.mock.calls.filter(([input]) =>
  String(input instanceof Request ? input.url : input).endsWith('/api/fs/delete'));

beforeEach(() => {
  nextRuntime();
  home = spyOn(opencodeClient, 'getFilesystemHomeInfo').mockResolvedValue({ home: '/home/user', chatsRoot: '/srv/chats' });
  mkdir = spyOn(opencodeClient, 'createDirectory').mockResolvedValue({ success: true, path: '/unused' });
  request = spyOn(globalThis, 'fetch').mockResolvedValue(new Response('{}'));
});
afterEach(() => { home.mockRestore(); mkdir.mockRestore(); request.mockRestore(); });

describe('leaving an unsent Chat', () => {
  const initial = useSessionUIStore.getState();
  beforeEach(() => useSessionUIStore.setState({
    newSessionDraft: { ...initial.newSessionDraft, open: true, target: 'chat', preparedChatDirectory: null },
  }));
  afterEach(() => useSessionUIStore.setState(initial));

  for (const action of ['close', 'switch to project']) {
    test(`${action} releases the prepared path without deleting it`, async () => {
      const store = useSessionUIStore.getState();
      const directory = await store.prepareChatDraftDirectory();
      expect(directory?.startsWith('/srv/chats/')).toBe(true);
      if (action === 'close') store.closeNewSessionDraft();
      else store.setNewSessionDraftTarget({ projectId: 'project', directoryOverride: '/project' });

      expect(useSessionUIStore.getState().newSessionDraft).toMatchObject({
        open: action !== 'close', preparedChatDirectory: null,
      });
      // Both actions return before a fire-and-forget delete would reach fetch.
      await new Promise(resolve => setTimeout(resolve, 0));
      expect(deleteRequests()).toHaveLength(0);
    });
  }

  test('closing while mkdir is in flight neither adopts nor deletes the late directory', async () => {
    let started!: () => void;
    const creating = new Promise<void>(resolve => { started = resolve; });
    let finish!: () => void;
    const reply = new Promise<void>(resolve => { finish = resolve; });
    mkdir.mockImplementationOnce(async path => {
      started();
      await reply;
      return { success: true, path };
    });
    const store = useSessionUIStore.getState();
    const prepared = store.prepareChatDraftDirectory();
    await creating;
    store.closeNewSessionDraft();
    finish();

    expect(await prepared).toBeNull();
    expect(useSessionUIStore.getState().newSessionDraft).toMatchObject({ open: false, preparedChatDirectory: null });
    expect(deleteRequests()).toHaveLength(0);
  });
});

describe('server-owned chat directories', () => {
  test('creates beneath the relocated root and uses the legacy root only for an older response', async () => {
    expect((await createChatDirectory(new Date(2026, 8, 5))).startsWith('/srv/chats/2026-09-05/session-')).toBe(true);
    nextRuntime();
    home.mockResolvedValue({ home: '/home/user' });
    expect((await createChatDirectory(new Date(2026, 8, 5))).startsWith('/home/user/.config/openchamber/chats/2026-09-05/session-')).toBe(true);
  });

  test('names no root until the server does, then tells subscribers', async () => {
    // The home's legacy folder is not a stand-in: with OPENCHAMBER_DATA_DIR it
    // does not exist, and opening it showed "Could not initialize workspace."
    expect(getChatsRoot()).toBeNull();
    let notified = 0;
    const unsubscribe = subscribeChatsRoot(() => { notified += 1; });
    await ensureChatsRootDirectory();
    unsubscribe();
    expect(getChatsRoot()).toBe('/srv/chats');
    expect(notified).toBe(1);
  });

  test('classifies only exact configured and actual legacy roots after warming', async () => {
    expect(isChatDirectoryPath('/work/backup/.config/openchamber/chats/project')).toBe(false);
    await ensureChatsRootDirectory();
    expect(isChatDirectoryPath('/srv/chats/day/session-a')).toBe(true);
    expect(isChatDirectoryPath('/home/user/.config/openchamber/chats/day/session-a')).toBe(true);
    expect(isChatDirectoryPath('/work/backup/.config/openchamber/chats/project')).toBe(false);
    expect(isChatDirectoryPath('/srv/chats-other/session-a')).toBe(false);
    expect(getChatsRootFromDirectory('/srv/chats/day/session-a')).toBe('/srv/chats');
    expect(isChatDirectoryForHome('/other/.config/openchamber/chats/session-a', '/home/user')).toBe(false);
  });

  test('explicit deletion accepts real descendants but never shared roots, lookalikes, or traversal paths', async () => {
    for (const path of ['/srv/chats', '/home/user/.config/openchamber/chats', '/work/backup/.config/openchamber/chats/project', '/srv/chats/../project']) {
      await deleteChatDirectory(path);
    }
    expect(deleteRequests()).toHaveLength(0);
    await deleteChatDirectory('/srv/chats/day/session-a');
    await deleteChatDirectory('/home/user/.config/openchamber/chats/day/session-b');
    expect(deleteRequests()).toHaveLength(2);
  });

  test('accepts an already deleted directory and reports other deletion failures', async () => {
    request.mockResolvedValue(new Response('', { status: 404 }));
    await deleteChatDirectory('/srv/chats/day/session-a');
    request.mockResolvedValue(new Response('', { status: 500 }));
    await expect(deleteChatDirectory('/srv/chats/day/session-b')).rejects.toThrow('Failed to delete chat directory (500)');
  });

  test('does not adopt or delete a distinct case-variant project beside a server-owned root', async () => {
    await ensureChatsRootDirectory();
    expect(isChatDirectoryPath('/srv/Chats/project')).toBe(false);
    expect(getChatsRootFromDirectory('/srv/Chats/project')).toBeNull();
    await deleteChatDirectory('/srv/Chats/project');
    expect(deleteRequests()).toHaveLength(0);
  });

  test('recognizes server-confirmed aliases without changing folder scope identities', async () => {
    home.mockResolvedValue({
      home: '/Users/wayne', chatsRoot: '/srv/chat-alias',
      canonicalChatsRoot: '/storage/Chats',
      canonicalLegacyChatsRoot: '/Users/Wayne/.config/openchamber/chats',
    });
    await ensureChatsRootDirectory();
    expect(getChatsRootFromDirectory('/storage/Chats/day/session-a')).toBe('/srv/chat-alias');
    expect(getChatsRootFromDirectory('/Users/Wayne/.config/openchamber/chats/day/session-a')).toBe('/Users/wayne/.config/openchamber/chats');
    expect(isChatDirectoryPath('/storage/chats/project')).toBe(false);
    expect(isChatDirectoryPath('/storage/Chats-other/project')).toBe(false);
    for (const root of ['/srv/chat-alias', '/storage/Chats', '/Users/wayne/.config/openchamber/chats', '/Users/Wayne/.config/openchamber/chats']) {
      await deleteChatDirectory(root);
    }
    expect(deleteRequests()).toHaveLength(0);
    await deleteChatDirectory('/storage/Chats/day/session-a');
    expect(deleteRequests()).toHaveLength(1);
  });

  test('parses filesystem aliases from the home API and rejects relative aliases', async () => {
    home.mockRestore();
    request.mockResolvedValue(new Response(JSON.stringify({
      home: '/Users/wayne', chatsRoot: '/Users/wayne/.config/openchamber/chats',
      canonicalChatsRoot: '/Users/Wayne/.config/openchamber/chats',
    })));
    await ensureChatsRootDirectory();
    expect(isChatDirectoryPath('/Users/Wayne/.config/openchamber/chats/session-a')).toBe(true);
    nextRuntime();
    request.mockResolvedValue(new Response(JSON.stringify({ home: '/home/user', canonicalChatsRoot: '../project' })));
    await expect(deleteChatDirectory('/home/user/.config/openchamber/chats/session-a')).rejects.toThrow();
    expect(deleteRequests()).toHaveLength(0);
  });

  test('failed root lookup never creates or deletes, and the next attempt retries', async () => {
    home.mockRejectedValueOnce(new Error('offline'));
    await expect(deleteChatDirectory('/srv/chats/day/session-a')).rejects.toThrow('offline');
    expect(deleteRequests()).toHaveLength(0);
    home.mockRejectedValueOnce(new Error('offline'));
    await warmChatsRootDirectory();
    await createChatDirectory();
    expect(mkdir.mock.calls).toHaveLength(1);
    expect(home.mock.calls).toHaveLength(3);
  });

  test('runtime switch during root lookup cannot delete on the destination runtime', async () => {
    let resolve!: (value: { home: string; chatsRoot: string }) => void;
    home.mockImplementationOnce(() => new Promise((done) => { resolve = done; }));
    const deletion = deleteChatDirectory('/srv/chats/day/session-a');
    nextRuntime();
    resolve({ home: '/home/user', chatsRoot: '/srv/chats' });
    await expect(deletion).rejects.toThrow('Runtime changed');
    expect(deleteRequests()).toHaveLength(0);
  });
});
