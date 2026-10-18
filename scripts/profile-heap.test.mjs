import assert from "node:assert/strict"
import { test } from "node:test"

import { planHeapSessions } from "./profile-heap.mjs"

test("a session repeated across sidebar sections does not consume another visit", () => {
  assert.deepEqual(planHeapSessions(["pinned", "long", "pinned", "short", "other"], 3), ["pinned", "long", "short"])
})

test("the plan retains sidebar order and reports fewer sessions when exhausted", () => {
  assert.deepEqual(planHeapSessions([null, "b", "", "a", "b"], 20), ["b", "a"])
  assert.deepEqual(planHeapSessions([], 20), [])
})
