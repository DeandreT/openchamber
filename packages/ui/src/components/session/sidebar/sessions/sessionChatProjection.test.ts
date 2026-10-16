import { describe, expect, test } from 'bun:test';
import type { Session } from '@/lib/opencode/model';
import { opencodeClient } from '@/lib/opencode/client';
import { ensureChatsRootDirectory, isChatDirectoryPath } from '@/lib/chatDirectories';
import { getRuntimeKey } from '@/lib/runtime-switch';
import { isSessionInWork } from '@/lib/sessionWorkMetadata';
import { getPinnedSessionKey, isSessionPinned } from '@/lib/sessionPins';
import { orderSessionsByLifecycleScopes } from '@/sync/session-ordering';
import { buildSidebarSessionProjection, getDescendantIds, partitionSidebarSessions } from '../list/sessionCollection';
import { buildSessionSidebarRowModel, type SessionSidebarRowModelArgs } from '../sessionSidebarRowModel';
import type { SessionGroup } from '../types';
import type { ProjectSection } from '../projects/sessionProjectRender';
import { createSessionOwnershipIndex, type SessionOwnershipRecord } from './sessionOwnership';
import { buildSidebarChatGroup, projectSidebarChats } from './sessionChatProjection';

const chatsRoot = '/home/.config/openchamber/chats';
const managedDirectory = `${chatsRoot}/day/session`;
const worktreeDirectory = 'C:/worktrees/unregistered';
const projects = [{ id: 'app', normalizedPath: 'C:/projects/app' }];
const session = (id: string, overrides: Partial<Session> = {}): Session => ({
  id,
  projectID: 'opencode-app',
  directory: worktreeDirectory,
  title: id,
  cost: 0,
  tokens: { input: 0, output: 0, reasoning: 0, cache: { read: 0, write: 0 } },
  time: { created: 1, updated: 1 },
  ...overrides,
});
const ids = (sessions: readonly Session[]): string[] => sessions.map((entry) => entry.id);
const rowArgs = (chatGroup: SessionGroup | null): SessionSidebarRowModelArgs => ({
  mode: 'normal',
  sections: [],
  authoritativeSections: [],
  chatGroup,
  foldersMap: {},
  groupSearchDataByGroup: new WeakMap(),
  normalizedQuery: '',
  collapsedProjects: new Set(),
  collapsedGroups: new Set(),
  collapsedFolders: new Set(),
  collapsedActivities: new Set(),
  expandedParents: new Set(),
  visibleCountByContainer: new Map(),
  pinnedSessionIds: new Set(),
  sessionOrderIndex: new Map(),
  groupStatusByKey: new Map(),
  folderAuthorityByOwner: new Map(),
  activeProjectId: null,
  singleProjectMode: false,
  singleProjectId: null,
  showOnlyMainWorkspace: false,
  hideDirectoryControls: false,
});

describe('projectSidebarChats', () => {
  test('keeps a managed Chat reachable after it moves to an unregistered worktree', () => {
    const managed = session('moved', { directory: managedDirectory });
    const before = projectSidebarChats([managed], createSessionOwnershipIndex([], projects, new Map(), false), false);
    expect(ids(before.chatSessions)).toEqual(['moved']);

    const moved = { ...managed, directory: worktreeDirectory };
    const ownership = createSessionOwnershipIndex([moved], projects, new Map(), false);
    const after = projectSidebarChats([moved], ownership, false);

    expect(after.chatSessions).toEqual([moved]);
    expect(after.chatSessions[0]).toBe(moved);
    expect(after.chatSessions[0]?.directory).toBe(worktreeDirectory);
    expect(isChatDirectoryPath(worktreeDirectory)).toBe(false);
    expect(ownership.bySessionId.has('moved')).toBe(false);
    expect(after.projectSessions).toEqual([]);
  });

  for (const resolution of ['registered-root', 'worktree-topology', 'canonical-project'] as const) {
    test(`leaves Chats as soon as ${resolution} resolves an owner`, () => {
      const moved = session('moved');
      const unresolved = createSessionOwnershipIndex([moved], projects, new Map(), false);
      expect(ids(projectSidebarChats([moved], unresolved, false).chatSessions)).toEqual(['moved']);

      const configured = resolution === 'registered-root'
        ? [...projects, { id: 'worktree-project', normalizedPath: worktreeDirectory }]
        : projects;
      const worktrees = resolution === 'worktree-topology'
        ? new Map([['C:/projects/app', [{ path: worktreeDirectory }]]])
        : new Map();
      const metadata = resolution === 'canonical-project'
        ? [{ id: 'opencode-app', worktree: 'C:/projects/app' }]
        : [];
      const resolved = createSessionOwnershipIndex([moved], configured, worktrees, false, [], metadata);
      const projection = projectSidebarChats([moved], resolved, false);
      const owner = resolved.bySessionId.get(moved.id);

      expect(owner).toBeDefined();
      expect(projection.chatSessions).toEqual([]);
      expect(projection.projectSessions).toEqual([moved]);
      expect(projection.projectSessionsByProject.get(owner?.projectId ?? '')).toEqual([moved]);
      expect(moved.directory).toBe(worktreeDirectory);
    });
  }

  test('keeps missing, global and ambiguous canonical metadata reachable without guessing an owner', () => {
    const records: SessionOwnershipRecord[] = [
      session('missing', { projectID: '' }),
      session('global', { projectID: 'global' }),
      session('ambiguous', { projectID: 'conflict' }),
      { ...session('embedded-one', { directory: 'C:/projects/app' }), project: { id: 'conflict', worktree: 'C:/projects/app' } },
      { ...session('embedded-two', { directory: 'C:/projects/other' }), project: { id: 'conflict', worktree: 'C:/projects/other' } },
    ];
    const ownership = createSessionOwnershipIndex(records, [...projects, { id: 'other', normalizedPath: 'C:/projects/other' }], new Map(), false,
      [], [{ id: 'global', worktree: 'C:/projects/app' }]);
    const projection = projectSidebarChats(records, ownership, false);

    expect(ids(projection.chatSessions)).toEqual(['missing', 'global', 'ambiguous']);
    expect(ids(projection.projectSessions)).toEqual(['embedded-one', 'embedded-two']);
    expect(ownership.bySessionId.has('ambiguous')).toBe(false);
  });

  test('preserves access while metadata and topology are unavailable and uses retained ownership when present', () => {
    const records = [session('moved')];
    const unavailable = createSessionOwnershipIndex(records, projects, new Map(), false);
    const fallback = projectSidebarChats(records, unavailable, false);
    expect(ids(fallback.chatSessions)).toEqual(['moved']);
    expect(buildSidebarChatGroup(fallback.chatSessions, new Map(), null)?.sessions[0]?.session).toBe(records[0]);

    const retained = createSessionOwnershipIndex(records, projects,
      new Map([['C:/projects/app', [{ path: worktreeDirectory }]]]), false);
    expect(projectSidebarChats(records, retained, false).chatSessions).toEqual([]);
    expect(ids(projectSidebarChats(records, unavailable, false).chatSessions)).toEqual(['moved']);
  });

  test('keeps the full fallback tree together across directories without changing authoritative ownership', () => {
    const root = session('root');
    const child = session('child', { parentID: root.id, directory: 'C:/projects/app' });
    const grandchild = session('grandchild', { parentID: child.id, directory: 'C:/elsewhere' });
    const orphan = session('orphan', { parentID: 'not-loaded' });
    const projectRoot = session('project-root', { directory: 'C:/projects/app' });
    const projectChild = session('project-child', { parentID: projectRoot.id });
    const records = [grandchild, child, orphan, root, projectRoot, projectChild];
    const desktop = buildSidebarSessionProjection({
      globalActiveSessions: records, liveSessions: [], knownDirectories: new Set(), isVSCode: false,
      pinnedSessionIds: new Set(), sessionOrderRanks: new Map(),
    });
    const ownership = createSessionOwnershipIndex(desktop.projectSessions, projects, new Map(), false);
    const originalBucket = ownership.sessionsByProject.get('app');
    const projection = projectSidebarChats(desktop.orderedSessions, ownership, false);
    const group = buildSidebarChatGroup(projection.chatSessions, desktop.childrenMap, chatsRoot);

    expect(new Set(ids(projection.chatSessions))).toEqual(new Set(['root', 'child', 'grandchild']));
    expect(group?.sessions.map((node) => node.session.id)).toEqual(['root']);
    expect(group?.sessions[0]?.children[0]?.session).toBe(child);
    expect(group?.sessions[0]?.children[0]?.children[0]?.session).toBe(grandchild);
    expect(getDescendantIds(desktop.childrenMap, root.id)).toEqual(['child', 'grandchild']);
    expect(ids(projection.projectSessionsByProject.get('app') ?? [])).toEqual(['child', 'project-root']);
    expect(projection.projectSessions).toContain(child);
    expect(ownership.sessionsByProject.get('app')).toBe(originalBucket);
    expect(ids(originalBucket ?? [])).toEqual(['child', 'project-root']);
    expect(ownership.bySessionId.get('child')?.projectId).toBe('app');
    expect(projection.chatSessions).not.toContain(orphan);
    expect(projection.chatSessions).not.toContain(projectChild);
  });

  test('excludes archived roots, temporary /btw roots and their children without promoting subsessions', () => {
    const records = [
      session('active'),
      session('archived', { time: { created: 1, updated: 1, archived: 2 } }),
      session('archived-child', { parentID: 'archived' }),
      session('btw', { metadata: { openchamber: { kind: 'btw', originalSessionID: 'active' } } }),
      session('btw-child', { parentID: 'btw' }),
      session('temporary-child', { parentID: 'active', metadata: { openchamber: { kind: 'btw', originalSessionID: 'active' } } }),
      session('missing-parent-child', { parentID: 'missing-parent' }),
      session('archived-managed', { directory: managedDirectory, time: { created: 1, updated: 1, archived: 2 } }),
    ];
    const ownership = createSessionOwnershipIndex(records, projects, new Map(), false);
    const projection = projectSidebarChats(records, ownership, false);

    expect(ids(projection.chatSessions)).toEqual(['active']);
    expect(projection.projectSessions.some((record) => record.id === 'btw' || record.id === 'archived')).toBe(false);
  });

  test('desktop and mobile project the same managed and fallback membership, including complete child trees', () => {
    const records = [
      session('project', { directory: 'C:/projects/app' }),
      session('managed', { directory: managedDirectory }),
      session('fallback'),
      session('child', { parentID: 'fallback', directory: 'C:/child-worktree' }),
    ];
    const desktop = buildSidebarSessionProjection({
      globalActiveSessions: records, liveSessions: [], knownDirectories: new Set(['C:/projects/app']), isVSCode: false,
      pinnedSessionIds: new Set(), sessionOrderRanks: new Map(),
    });
    const desktopProjection = projectSidebarChats(desktop.orderedSessions,
      createSessionOwnershipIndex(desktop.projectSessions, projects, new Map(), false), false);
    const mobilePartition = partitionSidebarSessions(records, false);
    const mobileProjection = projectSidebarChats(records,
      createSessionOwnershipIndex(mobilePartition.projectSessions, projects, new Map(), false), false);

    expect(new Set(ids(mobileProjection.chatSessions))).toEqual(new Set(ids(desktopProjection.chatSessions)));
    expect(new Set(ids(mobileProjection.chatSessions))).toEqual(new Set(['managed', 'fallback', 'child']));
    expect(ids(mobileProjection.projectSessions)).toEqual(['project']);
    expect(mobileProjection.chatSessions.find((entry) => entry.id === 'fallback')?.directory).toBe(worktreeDirectory);
    const mobileOrdered = orderSessionsByLifecycleScopes(mobileProjection.chatSessions, new Set(), new Map());
    expect(ids(mobileOrdered)).toEqual(ids(desktopProjection.chatSessions));
  });

  test('keeps pinned and In work fallback conversations exclusively in Chats with the real pin identity', () => {
    const moved = session('moved', { metadata: { openchamber: { work: { state: 'open' } } } });
    const key = getPinnedSessionKey(getRuntimeKey(), moved.directory, moved.id);
    const pinnedIds = new Set(key ? [key] : []);
    const projection = projectSidebarChats([moved], createSessionOwnershipIndex([moved], projects, new Map(), false), false);

    expect(isSessionPinned(pinnedIds, projection.chatSessions[0]?.directory ?? null, moved.id)).toBe(true);
    expect(projection.projectSessions.filter((entry) => isSessionInWork(entry))).toEqual([]);
    expect(projection.projectSessions.filter((entry) => isSessionPinned(pinnedIds, entry.directory, entry.id))).toEqual([]);
    expect(moved.metadata).toEqual({ openchamber: { work: { state: 'open' } } });

    const resolved = createSessionOwnershipIndex([moved], projects, new Map(), false,
      [], [{ id: moved.projectID, worktree: 'C:/projects/app' }]);
    const owned = projectSidebarChats([moved], resolved, false);
    expect(owned.chatSessions).toEqual([]);
    expect(owned.projectSessions.filter((entry) => isSessionInWork(entry))).toEqual([moved]);
    expect(owned.projectSessions.filter((entry) => isSessionPinned(pinnedIds, entry.directory, entry.id))).toEqual([moved]);
  });

  test('leaves exact VS Code workspace scope unchanged and never supplies fallback Chats', () => {
    const records = [
      session('workspace', { directory: 'C:/projects/app' }),
      session('nested', { directory: 'C:/projects/app/subdirectory' }),
      session('outside'),
      session('managed', { directory: managedDirectory }),
    ];
    const desktop = buildSidebarSessionProjection({
      globalActiveSessions: records, liveSessions: [], knownDirectories: new Set(['C:/projects/app']), isVSCode: true,
      pinnedSessionIds: new Set(), sessionOrderRanks: new Map(),
    });
    const ownership = createSessionOwnershipIndex(desktop.projectSessions, projects, new Map(), true,
      [], [{ id: 'opencode-app', worktree: 'C:/projects/app' }]);
    const projection = projectSidebarChats(desktop.orderedSessions, ownership, true);

    expect(projection.chatSessions).toEqual([]);
    expect(ids(projection.projectSessions)).toEqual(['workspace']);
    expect(projection.projectSessionsByProject).toBe(ownership.sessionsByProject);
    expect(ownership.bySessionId.size).toBe(1);
    expect(projectSidebarChats(records, ownership, true).chatSessions).toEqual([]);
  });
});

describe('buildSidebarChatGroup', () => {
  test('a cross-directory child keeps its project navigation while its root uses Chats', () => {
    const parent = session('fallback-parent');
    const child = session('owned-child', { parentID: parent.id, directory: 'C:/projects/app' });
    const records = [parent, child];
    const ownership = createSessionOwnershipIndex(records, projects, new Map(), false);
    const projection = projectSidebarChats(records, ownership, false);
    const chatGroup = buildSidebarChatGroup(projection.chatSessions, new Map([[parent.id, [child]]]), chatsRoot);
    const projectGroup: SessionGroup = {
      id: 'main', label: 'app', branch: null, description: null, isMain: true,
      worktree: null, directory: 'C:/projects/app', folderScopeKey: 'C:/projects/app',
      sessions: (projection.projectSessionsByProject.get('app') ?? []).map((entry) => ({ session: entry, children: [], worktree: null })),
    };
    const section: ProjectSection = { project: projects[0]!, groups: [projectGroup] };
    const model = buildSessionSidebarRowModel({ ...rowArgs(chatGroup), sections: [section], authoritativeSections: [section] });
    const rendered = model.rows.filter((row) => row.kind === 'session');

    expect(rendered.filter((row) => row.node.session.id === parent.id)).toHaveLength(1);
    expect(rendered.filter((row) => row.node.session.id === child.id && row.projectId === 'app')).toHaveLength(1);
    expect(projection.projectSessions).toContain(child);
  });

  test('desktop search renders a fallback descendant once and counts one matching session', () => {
    const parent = session('fallback-parent');
    const child = session('owned-child', { parentID: parent.id, directory: 'C:/projects/app' });
    const records = [parent, child];
    const ownership = createSessionOwnershipIndex(records, projects, new Map(), false);
    const projection = projectSidebarChats(records, ownership, false);
    const chatGroup = buildSidebarChatGroup(projection.chatSessions, new Map([[parent.id, [child]]]), chatsRoot);
    if (!chatGroup) throw new Error('Missing fallback group');
    const projectGroup: SessionGroup = {
      id: 'main', label: 'app', branch: null, description: null, isMain: true,
      worktree: null, directory: 'C:/projects/app', folderScopeKey: 'C:/projects/app',
      sessions: (projection.projectSessionsByProject.get('app') ?? []).map((entry) => ({ session: entry, children: [], worktree: null })),
    };
    const section: ProjectSection = { project: projects[0]!, groups: [projectGroup] };
    const args = rowArgs(chatGroup);
    args.groupSearchDataByGroup.set(chatGroup, {
      filteredNodes: chatGroup.sessions, matchedSessionCount: 1,
      folderNameMatchCount: 0, groupMatches: false, hasMatch: true,
    });
    args.groupSearchDataByGroup.set(projectGroup, {
      filteredNodes: projectGroup.sessions, matchedSessionCount: 1,
      folderNameMatchCount: 0, groupMatches: false, hasMatch: true,
    });
    const model = buildSessionSidebarRowModel({
      ...args, mode: 'search', normalizedQuery: child.id, sections: [section], authoritativeSections: [section],
    });

    expect(model.rows.filter((row) => row.kind === 'session' && row.node.session.id === child.id)).toHaveLength(1);
    expect(model.searchMatchCount).toBe(1);
    expect(chatGroup.sessions[0]?.children[0]?.session).toBe(child);
    // Hidden Chats leave the owned child's project search entry available.
    const hidden = buildSessionSidebarRowModel({
      ...args, chatGroup: null, mode: 'search', normalizedQuery: child.id, sections: [section], authoritativeSections: [section],
    });
    expect(hidden.rows.filter((row) => row.kind === 'session' && row.node.session.id === child.id)).toHaveLength(1);
    expect(hidden.searchMatchCount).toBe(1);

    // Project-label matches absent from Chats still render in the project.
    args.groupSearchDataByGroup.set(chatGroup, {
      filteredNodes: [], matchedSessionCount: 0,
      folderNameMatchCount: 0, groupMatches: false, hasMatch: false,
    });
    args.groupSearchDataByGroup.set(projectGroup, {
      filteredNodes: projectGroup.sessions, matchedSessionCount: 0,
      folderNameMatchCount: 0, groupMatches: true, hasMatch: true,
    });
    const projectLabel = buildSessionSidebarRowModel({
      ...args, mode: 'search', normalizedQuery: 'project-label', sections: [section], authoritativeSections: [section],
    });
    expect(projectLabel.rows.filter((row) => row.kind === 'session' && row.node.session.id === child.id)).toHaveLength(1);
    expect(projectLabel.searchMatchCount).toBe(1);
  });

  for (const viewMode of ['projects', 'timeline'] as const) {
    test(`${viewMode} preserves Chats visibility, collapse and real-directory routing for fallback rows`, () => {
      const moved = session('moved');
      const projection = projectSidebarChats([moved], createSessionOwnershipIndex([moved], projects, new Map(), false), false);
      const group = buildSidebarChatGroup(projection.chatSessions, new Map(), chatsRoot);
      const args = { ...rowArgs(group), viewMode };
      const shown = buildSessionSidebarRowModel(args);
      const rows = shown.rows.filter((row) => row.kind === 'session');

      expect(rows).toHaveLength(1);
      expect(rows[0]?.node.session).toBe(moved);
      expect(rows[0]?.node.session.directory).toBe(worktreeDirectory);
      expect(shown.rows.some((row) => row.kind === 'activity-header' && row.activityKey === 'chats')).toBe(true);
      expect(buildSessionSidebarRowModel({ ...args, chatGroup: null }).rows.some((row) => row.kind === 'session')).toBe(false);
      expect(buildSessionSidebarRowModel({ ...args, collapsedActivities: new Set(['chats']) }).rows.some((row) => row.kind === 'session')).toBe(false);
      expect(group?.directory).toBe(chatsRoot);
      expect(group?.folderScopes?.some((scope) => scope.scopeKey === worktreeDirectory && scope.directory === worktreeDirectory)).toBe(true);
      expect(isChatDirectoryPath(moved.directory)).toBe(false);
    });
  }

  test('shows fallback conversations before the managed Chats root has loaded', () => {
    const moved = session('moved');
    const group = buildSidebarChatGroup([moved], new Map(), null);

    expect(group?.directory).toBeNull();
    expect(group?.sessions[0]?.session).toBe(moved);
    expect(buildSessionSidebarRowModel(rowArgs(group)).rows.some((row) => row.kind === 'session')).toBe(true);
    expect(buildSidebarChatGroup([], new Map(), null)).toBeNull();
  });
});

const originalHomeInfo = opencodeClient.getFilesystemHomeInfo;
opencodeClient.getFilesystemHomeInfo = async () => ({ home: '/home' });
try {
  await ensureChatsRootDirectory();
} finally {
  opencodeClient.getFilesystemHomeInfo = originalHomeInfo;
}
