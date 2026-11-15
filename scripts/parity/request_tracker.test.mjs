import assert from "node:assert/strict"
import test from "node:test"
import { RequestTracker } from "./request_tracker.mjs"

test("navigation drops abandoned work but preserves incoming and unknown owners", () => {
  const tracker = new RequestTracker()
  tracker.seed({ frame: { id: "main", loaderId: "old" } })
  tracker.start({ requestId: "old", frameId: "main", loaderId: "old" }, "/old")
  tracker.start({ requestId: "new", frameId: "main", loaderId: "new" }, "/new")
  tracker.start({ requestId: "unknown", frameId: "main" }, "/unknown")
  tracker.start({ requestId: "other", frameId: "child", loaderId: "child" }, "/child")
  tracker.navigate({ id: "main", loaderId: "new" })
  assert.deepEqual(tracker.routes(), ["/new", "/unknown", "/child"])
  tracker.start({ requestId: "late-old", frameId: "main", loaderId: "old" }, "/late")
  assert.equal(tracker.size, 3)
  for (const id of ["new", "unknown", "other"]) tracker.finish(id)
  assert.equal(tracker.size, 0)
})

test("same-document signals and late frame snapshots cannot discard current work", () => {
  const tracker = new RequestTracker()
  tracker.navigate({ id: "main", loaderId: "new" })
  tracker.start({ requestId: "active", frameId: "main", loaderId: "new" }, "/active")
  tracker.seed({ frame: { id: "main", loaderId: "old" } })
  tracker.navigate({ id: "main", loaderId: "new" })
  tracker.navigate({ id: "main" })
  assert.equal(tracker.size, 1)
})

test("detachment retires only the detached frame", () => {
  const tracker = new RequestTracker()
  tracker.start({ requestId: "main", frameId: "main", loaderId: "a" }, "/main")
  tracker.start({ requestId: "child", frameId: "child" }, "/child")
  tracker.start({ requestId: "unknown" }, "/unknown")
  tracker.detach(undefined, "remove")
  tracker.detach("child", "unknown")
  assert.equal(tracker.size, 3)
  tracker.detach("child", "remove")
  tracker.start({ requestId: "late", frameId: "child", loaderId: "old" }, "/late")
  assert.deepEqual(tracker.routes(), ["/main", "/unknown"])
})

test("process swaps preserve incoming-document work", () => {
  const tracker = new RequestTracker()
  tracker.seed({ frame: { id: "main", loaderId: "old" } })
  tracker.start({ requestId: "old", frameId: "main", loaderId: "old" }, "/old")
  tracker.start({ requestId: "new", frameId: "main", loaderId: "new" }, "/new")
  tracker.start({ requestId: "unknown", frameId: "main" }, "/unknown")
  tracker.detach("main", "swap")
  tracker.start({ requestId: "late", frameId: "main", loaderId: "old" }, "/late")
  assert.deepEqual(tracker.routes(), ["/new", "/unknown"])
  tracker.navigate({ id: "main", loaderId: "new" })
  assert.equal(tracker.size, 2)
})
