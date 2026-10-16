import { resolveProjectRef } from '@/lib/worktreeSessionCreator';
import {
  listProjectWorktrees,
  getRepositoryWorktreeRoot,
  partitionWorktreesByRegisteredProject,
  preserveClientTrackedWorktreeStatus,
  replaceRepositoryWorktrees,
  worktreeMapsEqual,
  type ProjectRef,
} from '@/lib/worktrees/worktreeManager';
import { normalizePath } from '@/lib/pathNormalization';
import { getRuntimeKey } from '@/lib/runtime-switch';
import { useSessionUIStore } from '@/sync/session-ui-store';
import type { WorktreeMetadata } from '@/types/worktree';

let refreshRevision = 0;
let pendingRefreshes = 0;
const latestRefreshByRepository = new Map<string, number>();

/**
 * Registered projects that own any of the directories a `worktree-changed`
 * event names. Directories are matched through the known worktree map first,
 * so a linked worktree resolves to the project that registered its repository.
 * Each project appears once.
 */
export const resolveProjectsForWorktreeChange = (directories: readonly string[]): ProjectRef[] => {
  const byId = new Map<string, ProjectRef>();
  for (const directory of directories) {
    const project = resolveProjectRef(directory);
    if (project && !byId.has(project.id)) byId.set(project.id, project);
  }
  return [...byId.values()];
};

/**
 * The registered project a created or moved session certainly belongs to: the
 * project whose root is the session's directory, or the owner of a known
 * worktree that holds it. A directory that only sits under a registered
 * folder can be a worktree nobody has listed yet, of that project or of
 * another one, so it resolves to `null` and the caller runs full discovery.
 */
const resolveProjectForSessionDirectory = (
  directory: string,
  projects: ReadonlyArray<ProjectRef>,
): ProjectRef | null => {
  const normalizedDirectory = normalizePath(directory);
  if (!normalizedDirectory) return null;
  const root = projects.find((project) => normalizePath(project.path) === normalizedDirectory);
  if (root) return { id: root.id, path: root.path };
  for (const worktrees of useSessionUIStore.getState().availableWorktreesByProject.values()) {
    for (const worktree of worktrees) {
      const worktreePath = normalizePath(worktree.path);
      if (worktreePath && (normalizedDirectory === worktreePath || normalizedDirectory.startsWith(`${worktreePath}/`))) {
        return resolveProjectRef(directory);
      }
    }
  }
  return null;
};

/** Unknown destinations require discovery; known owners are refreshed once. */
export const resolveProjectsForSessionDirectories = (
  directories: readonly string[],
  projects: ReadonlyArray<ProjectRef>,
): ProjectRef[] | null => {
  const affectedProjects = new Map<string, ProjectRef>();
  for (const directory of directories) {
    const project = resolveProjectForSessionDirectory(directory, projects);
    if (!project) return null;
    affectedProjects.set(project.id, project);
  }
  return [...affectedProjects.values()];
};

/**
 * Refresh the published worktree topology for every registered project a
 * `worktree-changed` event touches, bypassing the listing cache. Used by the
 * surfaces that keep no separate raw topology (hosted mobile, mini chat).
 *
 * Each project is listed on its own; a failed listing keeps that project's
 * last known worktrees and does not block the others. Nothing is published
 * once `isCancelled` reports true or when the result equals the current map.
 */
export const refreshWorktreeTopologyForChange = async (
  projects: ReadonlyArray<ProjectRef>,
  directories: readonly string[],
  isCancelled: () => boolean = () => false,
): Promise<void> => {
  const runtimeKey = getRuntimeKey();
  const affectedProjects = resolveProjectsForWorktreeChange(directories);
  if (affectedProjects.length === 0) return;
  const revision = ++refreshRevision;
  const repositoryKey = (root: string) => JSON.stringify([runtimeKey, root]);
  const initialTopology = useSessionUIStore.getState().availableWorktreesByProject;
  pendingRefreshes += 1;
  for (const project of affectedProjects) {
    const key = repositoryKey(getRepositoryWorktreeRoot(initialTopology, project.path, []));
    latestRefreshByRepository.set(key, revision);
  }
  try {
    const listings = await Promise.all(affectedProjects.map(async (project) => {
      try {
        return { project, worktrees: await listProjectWorktrees(project, { force: true }) };
      } catch {
        return null;
      }
    }));
    if (isCancelled() || runtimeKey !== getRuntimeKey()) return;

    const published = useSessionUIStore.getState().availableWorktreesByProject;
    let worktreesByProject: Map<string, WorktreeMetadata[]> | null = null;
    for (const listing of listings) {
      if (!listing) continue;
      const key = repositoryKey(getRepositoryWorktreeRoot(worktreesByProject ?? published, listing.project.path, listing.worktrees));
      if ((latestRefreshByRepository.get(key) ?? 0) > revision) continue;
      latestRefreshByRepository.set(key, revision);
      worktreesByProject = replaceRepositoryWorktrees(
        worktreesByProject ?? published,
        listing.project.path,
        listing.worktrees,
      );
    }
    if (!worktreesByProject) return;

    const partitioned = preserveClientTrackedWorktreeStatus(
      partitionWorktreesByRegisteredProject(projects, worktreesByProject),
      published,
    );
    if (worktreeMapsEqual(partitioned, published)) return;
    useSessionUIStore.setState({
      availableWorktrees: [...partitioned.values()].flat(),
      availableWorktreesByProject: partitioned,
    });
  } finally {
    // Retain the newer repository revision while any older batch can return,
    // including one started through another registered checkout of that repo.
    pendingRefreshes -= 1;
    if (pendingRefreshes === 0) latestRefreshByRepository.clear();
  }
};

/** A native session move may enter a linked worktree not discovered yet. */
export const refreshWorktreeTopologyForSessionDirectories = async (
  projects: ReadonlyArray<ProjectRef>,
  directories: readonly string[],
  isCancelled: () => boolean = () => false,
): Promise<void> => {
  const affectedProjects = resolveProjectsForSessionDirectories(directories, projects) ?? projects;
  await refreshWorktreeTopologyForChange(projects, affectedProjects.map((project) => project.path), isCancelled);
};
