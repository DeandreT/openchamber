import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test"
import type { SyncEvent } from "@/lib/opencode/events"
import type { Session } from "@/lib/opencode/model"
import { sessionEvents } from "@/lib/sessionEvents"

let currentSessions: Session[] = []
const upsertedSessions: Session[] = []
const removedSessionIds: string[] = []
let mutationCalls = 0
let runtimeKey = "runtime-a"
let runtimeWillChange: (() => void) | null = null
let onMutation: (() => void) | null = null
let unsubscribeMoves: (() => void) | undefined
const movedSessions: string[] = []

mock.module("@/stores/useGlobalSessionsStore", () => ({
  mergeSessionDirectoryMetadata: (session: Session) => session,
  isGlobalSessionRecencyOnlyUpdate: (existing: Session, incoming: Session) => (
    existing.title === incoming.title
    && existing.directory === incoming.directory
    && existing.projectID === incoming.projectID
    && existing.time?.updated !== incoming.time?.updated
  ),
  useGlobalSessionsStore: {
    getState: () => ({
      activeSessions: currentSessions,
      archivedSessions: [] as Session[],
      entityById: new Map(currentSessions.map((session) => [session.id, session])),
      upsertSession: (session: Session) => {
        upsertedSessions.push(session)
      },
      upsertSessions: (sessions: Session[]) => {
        upsertedSessions.push(...sessions)
      },
      removeSessions: (ids: string[]) => {
        removedSessionIds.push(...ids)
      },
      applySessionMutations: (mutations: Array<
        { type: "upsert"; session: Session } | { type: "remove"; sessionId: string }
      >) => {
        mutationCalls += 1
        for (const mutation of mutations) {
          if (mutation.type === "upsert") upsertedSessions.push(mutation.session)
          else removedSessionIds.push(mutation.sessionId)
        }
        onMutation?.()
      },
    }),
  },
}))
mock.module("@/lib/runtime-switch", () => ({
  getRuntimeKey: () => runtimeKey,
  subscribeRuntimeEndpointWillChange: (callback: () => void) => {
    runtimeWillChange = callback
    return () => undefined
  },
}))
import {
  applySessionEventsToGlobalSessions,
  applySessionEventToGlobalSessions,
} from "../session-event-router"
import {
  registerBulkArchiveEchoes,
  releaseBulkArchiveEchoes,
  shouldConsumeBulkArchiveEcho,
} from "../bulk-archive-echo"

const buildSession = (title: string, time: Session["time"]): Session => ({
  id: "ses_1",
  title,
  time,
} as Session)

// v2 reports a changed session as a patch against the record the store holds.
const buildEvent = (session: Session): SyncEvent => ({
  type: "session.patched",
  properties: {
    sessionID: session.id,
    patch: { title: session.title, time: session.time },
  },
})

const buildDeleteEvent = (sessionId: string): SyncEvent => ({
  type: "session.deleted",
  properties: { sessionID: sessionId },
})

const buildLifecycleEvent = (type: "session.idle" | "session.error", sessionId: string): SyncEvent => (
  type === "session.idle"
    ? { type, properties: { sessionID: sessionId } }
    : { type, properties: { sessionID: sessionId, error: { type: "UnknownError", message: "failed" } } }
)

describe("applySessionEventToGlobalSessions", () => {
  beforeEach(() => {
    runtimeWillChange?.()
    runtimeKey = "runtime-a"
    currentSessions = []
    upsertedSessions.length = 0
    removedSessionIds.length = 0
    mutationCalls = 0
    onMutation = null
    movedSessions.length = 0
    unsubscribeMoves = sessionEvents.onMoved((directories) => {
      for (const directory of directories) {
        expect(upsertedSessions.some((session) => session.directory === directory)).toBe(true)
      }
      movedSessions.push(...directories)
    })
  })
  afterEach(() => unsubscribeMoves?.())

  test("skips stale global session.patched echoes after a newer rename", () => {
    currentSessions = [buildSession("New Title", { created: 1, updated: 20 })]

    applySessionEventToGlobalSessions(buildEvent(buildSession("Old Title", { created: 1, updated: 10 })))

    expect(upsertedSessions).toEqual([])
  })

  test("commits only the latest recency update when a session becomes idle", () => {
    currentSessions = [buildSession("Initial", { created: 1, updated: 10 })]

    applySessionEventToGlobalSessions(buildEvent(buildSession("Initial", { created: 1, updated: 20 })))
    applySessionEventToGlobalSessions(buildEvent(buildSession("Initial", { created: 1, updated: 30 })))

    expect(upsertedSessions).toEqual([])
    applySessionEventToGlobalSessions(buildLifecycleEvent("session.idle", "ses_1"))
    expect(upsertedSessions.map((session) => session.time.updated)).toEqual([30])
  })

  test("applies substantive session updates immediately", () => {
    currentSessions = [buildSession("Initial", { created: 1, updated: 10 })]

    applySessionEventToGlobalSessions(buildEvent(buildSession("Renamed", { created: 1, updated: 20 })))

    expect(upsertedSessions.map((session) => session.title)).toEqual(["Renamed"])
  })

  test("applies a metadata broadcast to the global record at once", () => {
    currentSessions = [buildSession("Initial", { created: 1, updated: 10 })]

    applySessionEventToGlobalSessions({
      type: "session.patched",
      properties: { sessionID: "ses_1", patch: { metadata: { openchamber: { work: { state: "open" } } } } },
    })

    expect(upsertedSessions.map((session) => session.metadata)).toEqual([{ openchamber: { work: { state: "open" } } }])
    expect(upsertedSessions[0]?.title).toBe("Initial")
  })

  test("cancels a pending global update when the session is deleted", () => {
    currentSessions = [buildSession("Initial", { created: 1, updated: 10 })]

    applySessionEventToGlobalSessions(buildEvent(buildSession("Initial", { created: 1, updated: 20 })))
    applySessionEventToGlobalSessions(buildDeleteEvent("ses_1"))
    applySessionEventToGlobalSessions(buildLifecycleEvent("session.idle", "ses_1"))

    expect(upsertedSessions).toEqual([])
    expect(removedSessionIds).toEqual(["ses_1"])
  })

  test("discards pending global updates when the runtime changes", () => {
    currentSessions = [buildSession("Initial", { created: 1, updated: 10 })]
    applySessionEventToGlobalSessions(buildEvent(buildSession("Initial", { created: 1, updated: 20 })))

    runtimeKey = "runtime-b"
    runtimeWillChange?.()
    applySessionEventToGlobalSessions(buildLifecycleEvent("session.idle", "ses_1"))

    expect(upsertedSessions).toEqual([])
  })

  test("commits an ordered event batch once", () => {
    const events = Array.from({ length: 1_000 }, (_, index) => ({
      type: "session.created",
      properties: {
        info: {
          id: `ses_${index}`,
          title: `Session ${index}`,
          time: { created: index, updated: index },
        },
      },
    } as SyncEvent))

    applySessionEventsToGlobalSessions(events)

    expect(mutationCalls).toBe(1)
    expect(upsertedSessions).toHaveLength(1_000)
    expect(movedSessions).toEqual([])
  })

  test("announces a native move after publishing its destination, without repeating an echo", () => {
    currentSessions = [{ ...buildSession("Moving", { created: 1, updated: 10 }), directory: "/chats/session" }]
    const move: SyncEvent = {
      type: "session.patched",
      properties: { sessionID: "ses_1", patch: { directory: "/worktrees/feature", projectID: "repo", time: { updated: 20 } } },
    }

    applySessionEventToGlobalSessions(move)
    expect(movedSessions).toEqual(["/worktrees/feature"])

    currentSessions = [...upsertedSessions]
    applySessionEventToGlobalSessions(move)
    expect(movedSessions).toHaveLength(1)
  })

  test("stale moves and unknown partial records announce no topology change", () => {
    const move: SyncEvent = {
      type: "session.patched",
      properties: { sessionID: "ses_1", patch: { directory: "/stale", time: { updated: 10 } } },
    }
    applySessionEventToGlobalSessions(move)
    currentSessions = [{ ...buildSession("Newer", { created: 1, updated: 20 }), directory: "/current" }]
    applySessionEventToGlobalSessions(move)
    expect(movedSessions).toEqual([])
    expect(upsertedSessions).toEqual([])
  })

  test("coalesces moves within one batch and drops a move followed by deletion", () => {
    currentSessions = [{ ...buildSession("Moving", { created: 1, updated: 1 }), directory: "/source" }]
    const moveTo = (directory: string, updated: number): SyncEvent => ({
      type: "session.patched",
      properties: { sessionID: "ses_1", patch: { directory, time: { updated } } },
    })

    applySessionEventsToGlobalSessions([moveTo("/intermediate", 2), moveTo("/destination", 3)])
    expect(mutationCalls).toBe(1)
    expect(movedSessions).toEqual(["/destination"])

    movedSessions.length = 0
    applySessionEventsToGlobalSessions([moveTo("/deleted", 4), buildDeleteEvent("ses_1")])
    expect(movedSessions).toEqual([])
  })

  test("a runtime switch during publication cannot announce a move in the next runtime", () => {
    currentSessions = [{ ...buildSession("Moving", { created: 1, updated: 1 }), directory: "/source" }]
    onMutation = () => { runtimeKey = "runtime-b" }

    applySessionEventToGlobalSessions({
      type: "session.patched",
      properties: { sessionID: "ses_1", patch: { directory: "/destination" } },
    })

    expect(movedSessions).toEqual([])
  })

  test("a moved subtree requests destination discovery once per published batch", () => {
    currentSessions = Array.from({ length: 200 }, (_, index) => ({
      ...buildSession("Moving", { created: 1, updated: 1 }), id: `ses_${index}`, directory: "/source",
    }))
    const moves: SyncEvent[] = currentSessions.map((session) => ({
      type: "session.patched",
      properties: { sessionID: session.id, patch: { directory: "/destination", time: { updated: 2 } } },
    }))

    applySessionEventsToGlobalSessions(moves)

    expect(mutationCalls).toBe(1)
    expect(upsertedSessions).toHaveLength(200)
    expect(movedSessions).toEqual(["/destination"])
  })

  test("consumes only the matching bulk archive echo", () => {
    registerBulkArchiveEchoes(runtimeKey, [{ id: "ses_1", archivedAt: 20 }], 100)

    expect(shouldConsumeBulkArchiveEcho(buildEvent(buildSession("Initial", {
      created: 1,
      updated: 20,
      archived: 20,
    })), runtimeKey, 101)).toBe(true)
    expect(shouldConsumeBulkArchiveEcho(buildEvent(buildSession("Initial", {
      created: 1,
      updated: 21,
      archived: 21,
    })), runtimeKey, 101)).toBe(false)
    expect(shouldConsumeBulkArchiveEcho(buildEvent(buildSession("Initial", {
      created: 1,
      updated: 20,
      archived: 20,
    })), "runtime-b", 101)).toBe(false)
  })

  test("does not consume an expired or released bulk archive echo", () => {
    registerBulkArchiveEchoes(runtimeKey, [{ id: "ses_1", archivedAt: 20 }], 100)
    const event = buildEvent(buildSession("Initial", { created: 1, updated: 20, archived: 20 }))

    expect(shouldConsumeBulkArchiveEcho(event, runtimeKey, 30_101)).toBe(false)

    registerBulkArchiveEchoes(runtimeKey, [{ id: "ses_1", archivedAt: 20 }], 100)
    releaseBulkArchiveEchoes(runtimeKey, ["ses_1"])
    expect(shouldConsumeBulkArchiveEcho(event, runtimeKey, 101)).toBe(false)
  })
})
