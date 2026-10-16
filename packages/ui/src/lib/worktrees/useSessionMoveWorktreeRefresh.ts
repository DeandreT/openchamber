import { useEffect } from 'react';
import { getRuntimeKey } from '@/lib/runtime-switch';
import { sessionEvents } from '@/lib/sessionEvents';
import type { ProjectRef } from './worktreeManager';
import { refreshWorktreeTopologyForSessionDirectories } from './worktreeTopologyRefresh';

/** Mobile and mini chat keep their picker topology current after native moves. */
export const useSessionMoveWorktreeRefresh = (
  projects: ReadonlyArray<ProjectRef>,
  enabled = true,
): void => {
  useEffect(() => {
    if (!enabled || projects.length === 0) return;
    const runtimeKey = getRuntimeKey();
    let cancelled = false;
    const isCancelled = () => cancelled || runtimeKey !== getRuntimeKey();
    const unsubscribe = sessionEvents.onMoved((directories) => {
      if (isCancelled()) return;
      void refreshWorktreeTopologyForSessionDirectories(projects, directories, isCancelled);
    });
    return () => {
      cancelled = true;
      unsubscribe();
    };
  }, [enabled, projects]);
};
