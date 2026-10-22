/**
 * Differential fixtures for the native desktop's sidebar membership port.
 *
 * Each scenario runs the reference `createSessionOwnershipIndex` and
 * `projectSidebarChats` and records their ordered results next to the inputs.
 * The Rust core replays the same inputs in
 * `native/crates/core/tests/sidebar_parity.rs`. The fixture is checked in;
 * `sidebarParityFixtures.test.ts` fails when it is stale.
 *
 *   bun run native:fixtures
 */
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { opencodeClient } from '@/lib/opencode/client';
import type { Session } from '@/lib/opencode/model';
import { ensureChatsRootDirectory } from '@/lib/chatDirectories';
import type { SpaceMark } from '@/lib/spaces/spaces-store';
import { projectSidebarChats } from '@/components/session/sidebar/sessions/sessionChatProjection';
import {
  createSessionOwnershipIndex,
  type DirectoryOwner,
  type SessionOwnershipRecord,
} from '@/components/session/sidebar/sessions/sessionOwnership';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, '..', '..', '..', '..', '..');

export const SIDEBAR_PARITY_FIXTURE_PATH = 'native/crates/core/tests/fixtures/sidebar-projection.json';

/** One server home for every scenario: the reference caches chat roots per runtime. */
const FIXTURE_HOME = '/home/user';
const CHATS_ROOT = `${FIXTURE_HOME}/.config/openchamber/chats`;

type FixtureSession = {
  id: string;
  parentID?: string;
  projectID?: string;
  directory?: string;
  project?: { id?: string; worktree?: string };
  archived?: boolean;
  btw?: boolean;
};

type FixtureInput = {
  surface: 'desktop' | 'vscode';
  sessions: FixtureSession[];
  archivedSessions: FixtureSession[];
  projects: { id: string; path: string }[];
  worktrees: { projectPath: string; worktreePaths: string[] }[];
  authoritativeProjects: { id: string; worktree: string }[];
  spaces: { id: string; directory: string | null; projectDirectory: string | null }[];
};

type Scenario = { name: string; input: FixtureInput };

const emptyInput = (): FixtureInput => ({
  surface: 'desktop',
  sessions: [],
  archivedSessions: [],
  projects: [],
  worktrees: [],
  authoritativeProjects: [],
  spaces: [],
});

const scenario = (name: string, input: Partial<FixtureInput>): Scenario => ({
  name,
  input: { ...emptyInput(), ...input },
});

const appProjects = [
  { id: 'app', path: '/projects/app' },
  { id: 'lib', path: '/projects/lib' },
];

/** Deterministic linear congruential generator, so the fixture never drifts by chance. */
const seededRandom = (seed: number) => {
  let state = seed >>> 0;
  return () => {
    state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
    return state / 0x1_0000_0000;
  };
};

const generatedScenario = (name: string, seed: number, count: number): Scenario => {
  const random = seededRandom(seed);
  const pick = <T,>(values: readonly T[]): T => {
    const value = values[Math.floor(random() * values.length)];
    if (value === undefined) throw new Error('pick needs a non-empty list');
    return value;
  };
  const directories = [
    '/projects/app',
    '/projects/app/src',
    '/projects/lib',
    '/worktrees/app-feature',
    '/worktrees/app-feature/nested',
    '/spaces/one/app',
    '/tmp/scratch',
    'C:/work/app',
    'c:\\work\\app\\sub\\',
    `${CHATS_ROOT}/2026-10-01/session-a`,
    `${CHATS_ROOT}/2026-10-02/session-b`,
    '/',
    '',
  ];
  const projectIds = ['oc-app', 'oc-lib', 'global', 'oc-unknown'];
  const sessions: FixtureSession[] = [];
  for (let index = 0; index < count; index += 1) {
    const id = `s${String(index).padStart(4, '0')}`;
    const session: FixtureSession = { id, directory: pick(directories) };
    if (index > 0 && random() < 0.45) {
      // Parents may point forward, backward, at themselves' ancestors or nowhere.
      session.parentID = random() < 0.1 ? `missing-${index}` : `s${String(Math.floor(random() * count)).padStart(4, '0')}`;
    }
    if (random() < 0.5) session.projectID = pick(projectIds);
    if (random() < 0.3) {
      session.project = {
        id: pick(projectIds),
        worktree: pick(['/projects/app', '/projects/lib', '/tmp/scratch', 'C:/work/app']),
      };
    }
    if (random() < 0.12) session.archived = true;
    if (random() < 0.05) session.btw = true;
    sessions.push(session);
  }
  return scenario(name, {
    sessions,
    archivedSessions: sessions.filter((session) => session.archived),
    projects: [...appProjects, { id: 'win', path: 'C:\\work\\app' }],
    worktrees: [{ projectPath: '/projects/app', worktreePaths: ['/worktrees/app-feature'] }],
    authoritativeProjects: [{ id: 'oc-lib', worktree: '/projects/lib' }],
    spaces: [{ id: 'space-1', directory: '/spaces/one/app', projectDirectory: '/projects/app' }],
  });
};

export const SIDEBAR_PARITY_SCENARIOS: readonly Scenario[] = [
  scenario('empty inputs', {}),
  scenario('managed chat and unowned root fall back to Chats', {
    sessions: [
      { id: 'managed', directory: `${CHATS_ROOT}/2026-10-01/session-x` },
      { id: 'unowned', directory: '/tmp/elsewhere' },
      { id: 'owned', directory: '/projects/app' },
    ],
    projects: appProjects,
  }),
  scenario('fallback root keeps descendants across directories', {
    sessions: [
      { id: 'root', directory: '/tmp/root' },
      { id: 'child-owned', parentID: 'root', directory: '/projects/app/src' },
      { id: 'child-unowned', parentID: 'root', directory: '/tmp/other' },
      { id: 'grandchild', parentID: 'child-owned', directory: '/projects/lib' },
    ],
    projects: appProjects,
  }),
  scenario('missing, archived and btw parents never promote children', {
    sessions: [
      { id: 'orphan', parentID: 'gone', directory: '/tmp/a' },
      { id: 'archived-parent', directory: '/tmp/a', archived: true },
      { id: 'archived-child', parentID: 'archived-parent', directory: '/tmp/a' },
      { id: 'btw-parent', directory: '/tmp/a', btw: true },
      { id: 'btw-child', parentID: 'btw-parent', directory: '/tmp/a' },
    ],
    archivedSessions: [{ id: 'archived-parent', directory: '/tmp/a', archived: true }],
    projects: appProjects,
  }),
  scenario('parent cycles without a root stay out of Chats', {
    sessions: [
      { id: 'a', parentID: 'b', directory: '/tmp/a' },
      { id: 'b', parentID: 'a', directory: '/tmp/a' },
      { id: 'self', parentID: 'self', directory: '/tmp/a' },
    ],
  }),
  scenario('worktree topology resolves ownership', {
    sessions: [
      { id: 'in-worktree', directory: '/worktrees/app-feature/pkg' },
      { id: 'unknown-worktree', directory: '/worktrees/other' },
    ],
    projects: appProjects,
    worktrees: [
      { projectPath: '/projects/app/', worktreePaths: ['/worktrees/app-feature'] },
      { projectPath: '/projects/unregistered', worktreePaths: ['/worktrees/other'] },
    ],
  }),
  scenario('canonical metadata bridges only configured roots', {
    sessions: [
      { id: 'bridged', directory: '/tmp/x', projectID: 'oc-app' },
      { id: 'session-metadata', directory: '/tmp/y', project: { id: 'oc-lib', worktree: '/projects/lib' } },
      { id: 'global', directory: '/tmp/z', projectID: 'global', project: { id: 'global', worktree: '/projects/app' } },
      { id: 'unconfigured', directory: '/tmp/w', projectID: 'oc-elsewhere' },
    ],
    projects: appProjects,
    authoritativeProjects: [
      { id: 'oc-app', worktree: '/projects/app' },
      { id: 'oc-elsewhere', worktree: '/elsewhere' },
      { id: 'global', worktree: '/projects/app' },
    ],
  }),
  scenario('conflicting canonical metadata stays unresolved', {
    sessions: [{ id: 'conflict', directory: '/tmp/x', projectID: 'oc-shared' }],
    projects: appProjects,
    authoritativeProjects: [
      { id: 'oc-shared', worktree: '/projects/app' },
      { id: 'oc-shared', worktree: '/projects/lib' },
    ],
  }),
  scenario('ownership change moves a root out of Chats', {
    sessions: [
      { id: 'moved', directory: '/worktrees/new' },
      { id: 'moved-child', parentID: 'moved', directory: '/worktrees/new' },
    ],
    projects: [...appProjects, { id: 'new', path: '/worktrees/new' }],
  }),
  scenario('nested project roots prefer the longest root', {
    sessions: [
      { id: 'outer', directory: '/projects/mono/README' },
      { id: 'inner', directory: '/projects/mono/packages/web/src' },
    ],
    projects: [
      { id: 'mono', path: '/projects/mono' },
      { id: 'web', path: '/projects/mono/packages/web' },
    ],
  }),
  scenario('spaces belong to registered projects only', {
    sessions: [
      { id: 'in-space', directory: '/spaces/one/app' },
      { id: 'lost-space', directory: '/spaces/two/app' },
    ],
    projects: appProjects,
    spaces: [
      { id: 'one', directory: '/spaces/one/app', projectDirectory: '/projects/app' },
      { id: 'two', directory: '/spaces/two/app', projectDirectory: '/projects/gone' },
      { id: 'three', directory: null, projectDirectory: null },
    ],
  }),
  scenario('windows paths normalize and stop at drive roots', {
    sessions: [
      { id: 'drive', directory: 'c:\\work\\app\\src\\' },
      { id: 'drive-root', directory: 'C:\\' },
      { id: 'unc', directory: '\\\\server\\share\\app' },
    ],
    projects: [
      { id: 'win', path: 'C:/work/app' },
      { id: 'unc', path: '//server/share' },
    ],
  }),
  scenario('archived roots are owned, archived children are not', {
    sessions: [{ id: 'active', directory: '/projects/app' }],
    archivedSessions: [
      { id: 'archived-root', directory: '/projects/app', archived: true },
      { id: 'archived-child', parentID: 'archived-root', directory: '/projects/app', archived: true },
    ],
    projects: appProjects,
  }),
  scenario('duplicate project roots keep the smaller id', {
    sessions: [{ id: 'dup', directory: '/projects/app' }],
    projects: [
      { id: 'zeta', path: '/projects/app' },
      { id: 'alpha', path: '/projects/app/' },
    ],
  }),
  scenario('vs code keeps exact workspace scope', {
    surface: 'vscode',
    sessions: [
      { id: 'exact', directory: '/projects/app' },
      { id: 'nested', directory: '/projects/app/src' },
      { id: 'managed', directory: `${CHATS_ROOT}/d/s` },
      { id: 'bridged', directory: '/tmp/x', projectID: 'oc-app' },
    ],
    projects: appProjects,
    worktrees: [{ projectPath: '/projects/app', worktreePaths: ['/worktrees/app-feature'] }],
    authoritativeProjects: [{ id: 'oc-app', worktree: '/projects/app' }],
  }),
  scenario('directory falls back to project worktree', {
    sessions: [
      { id: 'no-directory', project: { worktree: '/projects/lib' } },
      { id: 'blank-directory', directory: '   ', project: { worktree: '/projects/app' } },
    ],
    projects: appProjects,
  }),
  generatedScenario('generated mixed topology (seed 7)', 7, 240),
  generatedScenario('generated mixed topology (seed 2026)', 2026, 240),
];

const toSession = (fixture: FixtureSession): SessionOwnershipRecord => ({
  id: fixture.id,
  parentID: fixture.parentID,
  projectID: fixture.projectID ?? '',
  directory: fixture.directory ?? '',
  title: fixture.id,
  cost: 0,
  tokens: { input: 0, output: 0, reasoning: 0, cache: { read: 0, write: 0 } },
  time: { created: 1, updated: 1, archived: fixture.archived ? 2 : undefined },
  project: fixture.project,
  metadata: fixture.btw ? { openchamber: { kind: 'btw', originalSessionID: 'origin' } } : undefined,
});

const toSpace = (space: FixtureInput['spaces'][number]): SpaceMark => ({
  id: space.id,
  name: space.id,
  state: 'complete',
  directory: space.directory,
  projectDirectory: space.projectDirectory,
});

// JSON omits an undefined `spaceId`, as it is absent outside spaces.
const describeOwner = (owner: DirectoryOwner) => ({
  projectId: owner.projectId,
  projectRoot: owner.projectRoot,
  scopeDirectory: owner.scopeDirectory,
  kind: owner.kind,
  spaceId: owner.spaceId,
});

const sortedRecord = <T,>(entries: Iterable<[string, T]>): Record<string, T> =>
  Object.fromEntries([...entries].sort(([left], [right]) => (left < right ? -1 : left > right ? 1 : 0)));

const sessionIds = (sessions: readonly Session[]): string[] => sessions.map((session) => session.id);

const runScenario = ({ input }: Scenario) => {
  const sessions = input.sessions.map(toSession);
  const archivedSessions = input.archivedSessions.map(toSession);
  const isVSCode = input.surface === 'vscode';
  const ownership = createSessionOwnershipIndex(
    sessions,
    input.projects.map((project) => ({ id: project.id, normalizedPath: project.path })),
    new Map(input.worktrees.map((entry) => [entry.projectPath, entry.worktreePaths.map((path) => ({ path }))])),
    isVSCode,
    archivedSessions,
    input.authoritativeProjects,
    input.spaces.map(toSpace),
  );
  const chats = projectSidebarChats(sessions, ownership, isVSCode);
  return {
    owners: sortedRecord([...ownership.bySessionId].map(([id, owner]) => [id, describeOwner(owner)])),
    sessionsByProject: sortedRecord([...ownership.sessionsByProject].map(([id, bucket]) => [id, sessionIds(bucket)])),
    archivedSessionsByProject: sortedRecord(
      [...ownership.archivedSessionsByProject].map(([id, bucket]) => [id, sessionIds(bucket)]),
    ),
    sessionsByScope: sortedRecord([...ownership.sessionsByScope].map(([scope, ids]) => [scope, [...ids].sort()])),
    directoryResolutions: ownership.directoryResolutions,
    chatSessions: sessionIds(chats.chatSessions),
    projectSessions: sessionIds(chats.projectSessions),
    projectSessionsByProject: sortedRecord(
      [...chats.projectSessionsByProject].map(([id, bucket]) => [id, sessionIds(bucket)]),
    ),
  };
};

/** Renders the checked-in fixture with the reference implementation. */
export const renderSidebarParityFixtures = async (): Promise<string> => {
  const originalHomeInfo = opencodeClient.getFilesystemHomeInfo;
  opencodeClient.getFilesystemHomeInfo = async () => ({ home: FIXTURE_HOME });
  try {
    await ensureChatsRootDirectory();
  } finally {
    opencodeClient.getFilesystemHomeInfo = originalHomeInfo;
  }
  const scenarios = SIDEBAR_PARITY_SCENARIOS.map((entry) => ({
    name: entry.name,
    input: entry.input,
    expected: runScenario(entry),
  }));
  return `${JSON.stringify({ home: FIXTURE_HOME, scenarios }, null, 2)}\n`;
};

const writeSidebarParityFixtures = async (): Promise<string> => {
  const target = resolve(repoRoot, SIDEBAR_PARITY_FIXTURE_PATH);
  mkdirSync(dirname(target), { recursive: true });
  writeFileSync(target, await renderSidebarParityFixtures(), 'utf8');
  return target;
};

if (process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  console.log(`wrote ${await writeSidebarParityFixtures()}`);
}
