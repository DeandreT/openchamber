/** Differential inputs for the native port of sidebar works. */
import type { Session } from '@/lib/opencode/model';
import type { ProjectSection } from '@/components/session/sidebar/projects/sessionProjectRender';
import type { SessionGroup, SessionNode } from '@/components/session/sidebar/types';
import { buildSidebarWorkIndex, orderSidebarWorks } from '@/components/session/sidebar/works/sidebarWorks';

type Node = { id: string; children?: Node[] };
type Group = { id: string; kind: 'root' | 'worktree' | 'space' | 'archive'; nodes: Node[] };
type Input = { projects: { id: string; groups: Group[] }[]; order: string[]; include: string[] };

const projects: Input['projects'] = [{
  id: 'project',
  groups: [
    { id: 'root', kind: 'root', nodes: [{ id: 'root', children: [{ id: 'review' }] }, { id: 'other' }] },
    { id: 'feature', kind: 'worktree', nodes: [{ id: 'one', children: [{ id: 'subagent' }] }, { id: 'two' }] },
    { id: 'empty', kind: 'worktree', nodes: [] },
    { id: 'space', kind: 'space', nodes: [{ id: 'isolated', children: [{ id: 'space-child' }] }, { id: 'isolated-two' }] },
    { id: 'empty-space', kind: 'space', nodes: [] },
    { id: 'archive', kind: 'archive', nodes: [{ id: 'archived' }] },
  ],
}];

const cases: { name: string; input: Input }[] = [
  { name: 'space roots and descendants share a work with empty spaces retained', input: {
    projects, order: ['isolated-two', 'two', 'space-child', 'isolated'],
    include: ['isolated-two', 'two', 'space-child', 'isolated'],
  } },
  { name: 'empty', input: { projects: [], order: [], include: [] } },
  { name: 'worktree sessions share a work and archives stay out', input: {
    projects, order: ['two', 'review', 'one', 'other', 'root', 'isolated', 'archived'],
    include: ['two', 'review', 'one', 'other', 'root', 'isolated', 'archived'],
  } },
  { name: 'filtering keeps first matching session order', input: {
    projects, order: ['review', 'two', 'other', 'subagent', 'root', 'isolated'],
    include: ['two', 'root', 'isolated'],
  } },
  { name: 'duplicate sessions keep the first owner', input: {
    projects: [...projects, { id: 'second', groups: [{ id: 'feature', kind: 'worktree', nodes: [{ id: 'root' }, { id: 'second-only' }] }] }],
    order: ['root', 'second-only', 'two', 'root'], include: ['root', 'second-only', 'two'],
  } },
  { name: 'unknown sessions do not fabricate work', input: {
    projects, order: ['unknown', 'archived'], include: ['unknown', 'archived'],
  } },
];

export const sidebarWorkFixtures = (session: (id: string) => Session) => cases.map(({ name, input }) => {
  const node = (source: Node): SessionNode => ({
    session: session(source.id), children: (source.children ?? []).map(node), worktree: null,
  });
  const sections: ProjectSection[] = input.projects.map((project) => ({
    project: { id: project.id, normalizedPath: `/projects/${project.id}` },
    groups: project.groups.map((group): SessionGroup => ({
      id: group.id, label: group.id, branch: null, description: null,
      directory: `/projects/${project.id}/${group.id}`, isMain: group.kind === 'root',
      isArchivedBucket: group.kind === 'archive',
      worktree: group.kind === 'worktree' ? { path: `/worktrees/${group.id}`, projectDirectory: `/projects/${project.id}`, branch: group.id, label: group.id } : null,
      space: group.kind === 'space' ? { id: group.id, name: group.id, state: 'complete', directory: `/spaces/${group.id}`, projectDirectory: `/projects/${project.id}` } : undefined,
      sessions: group.nodes.map(node),
    })),
  }));
  const index = buildSidebarWorkIndex(sections);
  const included = new Set(input.include);
  return { name, input, expected: {
    bySession: Object.fromEntries([...index.bySessionId].map(([id, work]) => [id, work.key])),
    groups: index.groupWorks.map((work) => work.key),
    ordered: orderSidebarWorks(input.order.map(session), index, (entry) => included.has(entry.id)).map((work) => work.key),
  } };
});
