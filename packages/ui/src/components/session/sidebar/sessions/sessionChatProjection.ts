import type { Session } from '@/lib/opencode/model';
import { getChatsRootFromDirectory, isChatDirectoryPath } from '@/lib/chatDirectories';
import { normalizePath } from '@/lib/pathNormalization';
import { isBtwSession } from '@/lib/sessionBtwMetadata';
import { buildActiveSessionNode } from '../list/sessionCollection';
import type { SessionGroup } from '../types';
import type { SessionOwnershipIndex } from './sessionOwnership';

// Chats also keeps root conversations reachable while no configured project
// owns them. Membership is display-only; records and ownership stay unchanged.
export const projectSidebarChats = (
  sessions: readonly Session[],
  ownership: SessionOwnershipIndex,
  isVSCode: boolean,
) => {
  const activeSessions = sessions.filter((session) => !session.time.archived && !isBtwSession(session));
  const chatSessionIds = new Set<string>();
  if (!isVSCode) {
    const children = new Map<string, Session[]>();
    const pending: Session[] = [];
    for (const session of activeSessions) {
      if (session.parentID) {
        const siblings = children.get(session.parentID) ?? [];
        siblings.push(session);
        children.set(session.parentID, siblings);
      } else if (isChatDirectoryPath(session.directory) || !ownership.bySessionId.has(session.id)) {
        pending.push(session);
      }
    }
    // Descendants stay with their root, including those in another directory.
    // Missing, archived and /btw parents never promote children into Chats.
    while (pending.length > 0) {
      const session = pending.pop();
      if (!session || chatSessionIds.has(session.id)) continue;
      chatSessionIds.add(session.id);
      for (const child of children.get(session.id) ?? []) pending.push(child);
    }
  }
  const isChatRoot = (session: Session) => !session.parentID && chatSessionIds.has(session.id);
  let projectSessionsByProject = ownership.sessionsByProject;
  for (const [projectId, bucket] of ownership.sessionsByProject) {
    if (!bucket.some(isChatRoot)) continue;
    if (projectSessionsByProject === ownership.sessionsByProject) projectSessionsByProject = new Map(ownership.sessionsByProject);
    projectSessionsByProject.set(projectId, bucket.filter((session) => !isChatRoot(session)));
  }
  return {
    chatSessions: activeSessions.filter((session) => chatSessionIds.has(session.id)),
    // Chats rows do not expand. Owned descendants retain their project entry
    // so moving their root into the fallback cannot hide existing navigation.
    projectSessions: activeSessions.filter((session) => !isChatDirectoryPath(session.directory)
      && (!chatSessionIds.has(session.id) || ownership.bySessionId.has(session.id))),
    projectSessionsByProject,
  };
};

export const buildSidebarChatGroup = (
  chatSessions: readonly Session[],
  childrenMap: ReadonlyMap<string, readonly Session[]>,
  knownChatsRoot: string | null,
): SessionGroup | null => {
  const chatsRoot = knownChatsRoot
    ?? chatSessions.map((session) => getChatsRootFromDirectory(session.directory)).find(Boolean)
    ?? null;
  if (!chatsRoot && chatSessions.length === 0) return null;
  const directories = new Set([
    chatsRoot,
    ...chatSessions.map((session) => normalizePath(session.directory)),
  ]);
  return {
    id: 'managed-chats',
    label: '',
    branch: null,
    description: null,
    isMain: true,
    worktree: null,
    directory: chatsRoot,
    folderScopeKey: chatsRoot,
    folderScopes: [...directories].filter((directory): directory is string => Boolean(directory))
      .map((directory) => ({ scopeKey: directory, directory })),
    draftTarget: 'chat',
    sessions: chatSessions.filter((session) => !session.parentID)
      .map((session) => buildActiveSessionNode(childrenMap, session)),
  };
};
