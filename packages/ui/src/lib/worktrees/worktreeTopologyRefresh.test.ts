import { afterEach, beforeEach, describe, expect, mock, test } from 'bun:test';
import React, { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import type { WorktreeMetadata } from '@/types/worktree';
import { switchRuntimeEndpoint } from '@/lib/runtime-switch';
import { sessionEvents } from '@/lib/sessionEvents';
import { installHookTestDom } from '@/components/session/sidebar/test-utils/testDom';
import { useProjectsStore } from '@/stores/useProjectsStore';
import { useDirectoryStore } from '@/stores/useDirectoryStore';

type WorktreeListEntry = { path: string; branch: string };

const listCalls: string[] = [];
let listImplementation: (directory: string) => Promise<WorktreeListEntry[]> = () => Promise.resolve([]);

type SessionState = {
  availableWorktreesByProject: Map<string, WorktreeMetadata[]>;
  availableWorktrees: WorktreeMetadata[];
  currentSessionDirectory: string;
};
const sessionState: SessionState = {
  availableWorktreesByProject: new Map(),
  availableWorktrees: [],
  currentSessionDirectory: '/selected-session',
};
let setStateCalls = 0;

const projects = [
  { id: 'repo', path: '/repo' },
  { id: 'other', path: '/other' },
];

mock.module('@/lib/openchamberConfig', () => ({ substituteCommandVariables: (command: string) => command }));
mock.module('@/components/ui', () => ({ toast: { warning: () => undefined } }));
mock.module('@/lib/i18n', () => ({ formatMessage: () => '', useI18nStore: { getState: () => ({ dictionary: {} }) } }));
mock.module('@/lib/worktrees/worktreeBootstrap', () => ({
  clearWorktreeBootstrapState: mock(),
  markWorktreeBootstrapPending: mock(),
  setWorktreeBootstrapState: mock(),
  startWorktreeBootstrapWatcher: mock(),
}));
mock.module('@/sync/session-worktree-store', () => ({ useSessionWorktreeStore: { setState: mock() } }));
mock.module('@/lib/worktrees/worktreeStatus', () => ({
  invalidateResolvedProjectRootCache: mock(),
  resolveProjectRoot: (directory: string) => Promise.resolve(directory === '/repo-linked' ? '/repo' : directory),
}));
mock.module('@/sync/session-ui-store', () => ({
  useSessionUIStore: {
    getState: () => sessionState,
    setState: (patch: Partial<typeof sessionState>) => {
      setStateCalls += 1;
      Object.assign(sessionState, patch);
    },
  },
}));
mock.module('@/lib/gitApi', () => ({
  deleteRemoteBranch: mock(),
  git: {
    worktree: {
      list: (directory: string) => {
        listCalls.push(directory);
        return listImplementation(directory);
      },
      create: mock(),
      validate: mock(),
      remove: mock(),
    },
  },
}));
// The real resolver consults stores; the contract exercised here is that any
// directory it maps to a project is refreshed once for that project.
mock.module('@/lib/worktreeSessionCreator', () => ({
  resolveProjectRef: (directory: string) => {
    if (directory === '/repo' || directory === '/repo-feature') return { id: 'repo', path: '/repo' };
    if (directory === '/other') return { id: 'other', path: '/other' };
    if (directory === '/repo-linked') return { id: 'linked', path: '/repo-linked' };
    return null;
  },
}));

const { refreshWorktreeTopologyForChange, refreshWorktreeTopologyForSessionDirectories, resolveProjectsForSessionDirectories, resolveProjectsForWorktreeChange } = await import('./worktreeTopologyRefresh');
const { listProjectWorktrees } = await import('./worktreeManager');
const { useSessionMoveWorktreeRefresh } = await import('./useSessionMoveWorktreeRefresh');

const metadata = (path: string, projectDirectory: string, status: WorktreeMetadata['worktreeStatus'] = 'ready'): WorktreeMetadata => ({
  path,
  projectDirectory,
  branch: 'feature',
  label: 'feature',
  worktreeStatus: status,
});

describe('worktree topology refresh from control events', () => {
  beforeEach(() => {
    switchRuntimeEndpoint({ apiBaseUrl: '', runtimeKey: 'topology-test' });
    listCalls.length = 0;
    setStateCalls = 0;
    sessionState.availableWorktreesByProject = new Map([
      ['/repo', [metadata('/repo-stale', '/repo')]],
      ['/other', [metadata('/other-feature', '/other')]],
    ]);
    sessionState.availableWorktrees = [...sessionState.availableWorktreesByProject.values()].flat();
  });

  test('maps event directories onto registered projects once each', () => {
    expect(resolveProjectsForWorktreeChange(['/repo', '/repo-feature', '/unknown'])).toEqual([{ id: 'repo', path: '/repo' }]);
  });

  test('places a created session only when its project is certain', () => {
    sessionState.availableWorktreesByProject.set('/repo', [metadata('/repo-feature', '/repo')]);
    const withHome = [...projects, { id: 'home', path: '/home' }];

    // A project root, and a worktree the client already knows.
    expect(resolveProjectsForSessionDirectories(['/other/'], withHome)).toEqual([{ id: 'other', path: '/other' }]);
    expect(resolveProjectsForSessionDirectories(['/repo-feature', '/repo-feature'], withHome)).toEqual([{ id: 'repo', path: '/repo' }]);

    // A new worktree under a broader registered folder may belong to any
    // project, and a directory nothing registers belongs to none.
    expect(resolveProjectsForSessionDirectories(['/home/.local/share/opencode/worktree/abc/feature'], withHome)).toBeNull();
    expect(resolveProjectsForSessionDirectories(['/repo/src'], withHome)).toBeNull();
    expect(resolveProjectsForSessionDirectories(['/unknown'], withHome)).toBeNull();
  });

  test('force-lists the affected project and replaces only its buckets', async () => {
    listImplementation = () => Promise.resolve([
      { path: '/repo', branch: 'main' },
      { path: '/repo-feature', branch: 'feature' },
    ]);

    await refreshWorktreeTopologyForChange(projects, ['/repo-feature']);

    expect(listCalls).toEqual(['/repo']);
    expect(sessionState.availableWorktreesByProject.get('/repo')?.map((worktree) => worktree.path)).toEqual(['/repo-feature']);
    expect(sessionState.availableWorktreesByProject.get('/other')?.map((worktree) => worktree.path)).toEqual(['/other-feature']);
    expect(sessionState.availableWorktrees.map((worktree) => worktree.path)).toEqual(['/repo-feature', '/other-feature']);
  });

  test('keeps the last known topology when the listing fails or the surface unmounted', async () => {
    listImplementation = () => Promise.reject(new Error('git failed'));
    await refreshWorktreeTopologyForChange(projects, ['/repo']);
    expect(setStateCalls).toBe(0);
    expect(sessionState.availableWorktreesByProject.get('/repo')?.map((worktree) => worktree.path)).toEqual(['/repo-stale']);

    listImplementation = () => Promise.resolve([{ path: '/repo', branch: 'main' }]);
    await refreshWorktreeTopologyForChange(projects, ['/repo'], () => true);
    expect(setStateCalls).toBe(0);
  });

  test('keeps a bootstrap status this client still tracks', async () => {
    sessionState.availableWorktreesByProject.set('/repo', [metadata('/repo-feature', '/repo', 'pending')]);
    listImplementation = () => Promise.resolve([
      { path: '/repo', branch: 'main' },
      { path: '/repo-feature', branch: 'feature' },
    ]);

    await refreshWorktreeTopologyForChange(projects, ['/repo']);

    expect(sessionState.availableWorktreesByProject.get('/repo')?.[0]?.worktreeStatus).toBe('pending');
  });

  test('a move into a known worktree refreshes only its registered repository', async () => {
    sessionState.availableWorktreesByProject.set('/repo', [metadata('/repo-feature', '/repo')]);
    listImplementation = () => Promise.resolve([{ path: '/repo', branch: 'main' }]);

    await refreshWorktreeTopologyForSessionDirectories(projects, ['/repo-feature', '/repo-feature']);

    expect(listCalls).toEqual(['/repo']);
  });

  test('a move into an undiscovered worktree discovers registered repositories', async () => {
    listImplementation = (directory) => Promise.resolve([
      { path: directory, branch: 'main' },
      { path: `${directory}-new`, branch: 'new' },
    ]);

    await refreshWorktreeTopologyForSessionDirectories(projects, ['/repo-new']);

    expect(listCalls).toEqual(['/repo', '/other']);
    expect(sessionState.availableWorktreesByProject.get('/repo')?.map((worktree) => worktree.path)).toEqual(['/repo-new']);
  });

  test('failed move discovery preserves the last topology and cancellation publishes nothing', async () => {
    listImplementation = () => Promise.reject(new Error('unavailable'));
    await refreshWorktreeTopologyForSessionDirectories(projects, ['/unknown']);
    expect(setStateCalls).toBe(0);

    listImplementation = (directory) => Promise.resolve([{ path: directory, branch: 'main' }]);
    await refreshWorktreeTopologyForSessionDirectories(projects, ['/unknown'], () => true);
    expect(setStateCalls).toBe(0);
  });

  test('move discovery cannot publish into a different runtime', async () => {
    listImplementation = (directory) => {
      switchRuntimeEndpoint({ apiBaseUrl: '', runtimeKey: 'different-runtime' });
      return Promise.resolve([{ path: directory, branch: 'main' }]);
    };
    await refreshWorktreeTopologyForSessionDirectories(projects, ['/unknown']);
    expect(setStateCalls).toBe(0);
  });

  test('unknown destinations force discovery through a warm worktree cache', async () => {
    listImplementation = (directory) => Promise.resolve([{ path: directory, branch: 'main' }]);
    await listProjectWorktrees(projects[0]!);
    listCalls.length = 0;
    listImplementation = (directory) => Promise.resolve([
      { path: directory, branch: 'main' }, { path: `${directory}-new`, branch: 'feature' },
    ]);

    await refreshWorktreeTopologyForSessionDirectories(projects, ['/repo-new']);

    expect(listCalls).toEqual(['/repo', '/other']);
    expect(sessionState.availableWorktreesByProject.get('/repo')?.map((worktree) => worktree.path)).toEqual(['/repo-new']);
  });

  test('a slow repository cannot make an older batch overwrite a newer repository listing', async () => {
    let finishOther: (entries: WorktreeListEntry[]) => void = () => undefined;
    const blockedOther = new Promise<WorktreeListEntry[]>((resolve) => { finishOther = resolve; });
    let repoReads = 0;
    listImplementation = (directory) => {
      if (directory === '/other') return blockedOther;
      repoReads += 1;
      return Promise.resolve([
        { path: '/repo', branch: 'main' },
        { path: repoReads === 1 ? '/repo-old' : '/repo-new', branch: 'feature' },
      ]);
    };
    const older = refreshWorktreeTopologyForSessionDirectories(projects, ['/unknown']);
    await new Promise<void>((resolve) => setImmediate(resolve));
    await refreshWorktreeTopologyForSessionDirectories(projects, ['/repo']);
    expect(sessionState.availableWorktreesByProject.get('/repo')?.map((worktree) => worktree.path)).toEqual(['/repo-new']);

    finishOther([{ path: '/other', branch: 'main' }, { path: '/other-new', branch: 'feature' }]);
    await older;

    expect(sessionState.availableWorktreesByProject.get('/repo')?.map((worktree) => worktree.path)).toEqual(['/repo-new']);
    expect(sessionState.availableWorktreesByProject.get('/other')?.map((worktree) => worktree.path)).toEqual(['/other-new']);
  });

  test('registered aliases of one repository share publication ordering', async () => {
    const withAlias = [...projects, { id: 'linked', path: '/repo-linked' }];
    sessionState.availableWorktreesByProject.set('/repo-linked', [metadata('/repo-linked-stale', '/repo')]);
    let finishOther: (entries: WorktreeListEntry[]) => void = () => undefined;
    const blockedOther = new Promise<WorktreeListEntry[]>((resolve) => { finishOther = resolve; });
    listImplementation = (directory) => {
      if (directory === '/other') return blockedOther;
      return Promise.resolve([
        { path: '/repo', branch: 'main' },
        { path: directory === '/repo-linked' ? '/repo-new' : '/repo-old', branch: 'feature' },
      ]);
    };
    const older = refreshWorktreeTopologyForChange(withAlias, ['/repo', '/other']);
    await new Promise<void>((resolve) => setImmediate(resolve));
    await refreshWorktreeTopologyForSessionDirectories(withAlias, ['/repo-linked']);
    finishOther([{ path: '/other', branch: 'main' }]);
    await older;

    expect(sessionState.availableWorktreesByProject.get('/repo')?.some((worktree) => worktree.path === '/repo-new')).toBe(true);
    expect(sessionState.availableWorktrees.some((worktree) => worktree.path === '/repo-old')).toBe(false);
  });
});

describe('mobile and mini-chat move subscription', () => {
  let root: Root | null;
  let dom: ReturnType<typeof installHookTestDom>;
  const Probe = ({ enabled = true }: { enabled?: boolean }) => {
    useSessionMoveWorktreeRefresh(projects, enabled);
    return null;
  };
  const flush = () => new Promise<void>((resolve) => setImmediate(resolve));

  beforeEach(() => {
    dom = installHookTestDom();
    root = createRoot(dom.container);
    switchRuntimeEndpoint({ apiBaseUrl: '', runtimeKey: 'consumer-test' });
    listCalls.length = 0;
    setStateCalls = 0;
    sessionState.availableWorktreesByProject = new Map();
    sessionState.availableWorktrees = [];
    sessionState.currentSessionDirectory = '/selected-session';
    useProjectsStore.setState({ projects, activeProjectId: 'other' });
    useDirectoryStore.setState({ currentDirectory: '/selected-directory' });
    listImplementation = (directory) => Promise.resolve([
      { path: directory, branch: 'main' }, { path: `${directory}-new`, branch: 'feature' },
    ]);
  });
  afterEach(() => {
    act(() => root?.unmount());
    dom.restore();
  });

  test('mounted move listeners refresh topology without changing project or directory selection', async () => {
    act(() => root?.render(React.createElement(Probe)));
    await act(async () => {
      sessionEvents.notifyMoved(['/repo-new']);
      await flush();
    });

    expect(listCalls).toEqual(['/repo', '/other']);
    expect(sessionState.availableWorktreesByProject.get('/repo')?.map((worktree) => worktree.path)).toEqual(['/repo-new']);
    expect(useProjectsStore.getState().activeProjectId).toBe('other');
    expect(useDirectoryStore.getState().currentDirectory).toBe('/selected-directory');
    expect(sessionState.currentSessionDirectory).toBe('/selected-session');
  });

  test('disabled and unmounted consumers do not refresh', async () => {
    act(() => root?.render(React.createElement(Probe, { enabled: false })));
    sessionEvents.notifyMoved(['/repo']);
    await flush();
    expect(listCalls).toEqual([]);

    act(() => root?.render(React.createElement(Probe)));
    act(() => root?.unmount());
    root = null;
    sessionEvents.notifyMoved(['/repo']);
    await flush();
    expect(listCalls).toEqual([]);
  });

  test('unmounting discards an in-flight result', async () => {
    let finish: (entries: WorktreeListEntry[]) => void = () => undefined;
    listImplementation = () => new Promise((resolve) => { finish = resolve; });
    act(() => root?.render(React.createElement(Probe)));
    sessionEvents.notifyMoved(['/repo']);
    act(() => root?.unmount());
    root = null;
    finish([{ path: '/repo', branch: 'main' }, { path: '/repo-late', branch: 'late' }]);
    await flush();
    expect(setStateCalls).toBe(0);
  });

  test('an old mounted listener ignores moves after the runtime switches', async () => {
    act(() => root?.render(React.createElement(Probe)));
    switchRuntimeEndpoint({ apiBaseUrl: '', runtimeKey: 'next-runtime' });
    sessionEvents.notifyMoved(['/repo']);
    await flush();
    expect(listCalls).toEqual([]);
    expect(setStateCalls).toBe(0);
  });
});
