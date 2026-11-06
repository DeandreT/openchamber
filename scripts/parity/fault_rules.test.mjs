import assert from "node:assert/strict"
import { test } from "node:test"

import { claimFault, pathPattern } from "./fault_rules.mjs"

test("path globs match like steps.fault_matches: * crosses segments, queries are ignored", () => {
  const pattern = pathPattern("/api/session/*/message")
  assert.ok(pattern.test("/api/session/ses_1/message"))
  assert.ok(pattern.test("/api/session/a/b/message"))
  assert.ok(!pattern.test("/api/session/ses_1/messages"))
  assert.ok(pathPattern("/api/a.b+c").test("/api/a.b+c"))
  assert.ok(!pathPattern("/api/a.b").test("/api/aXb"))
})

test("a rule is claimed at its own stage and method until its hits run out", () => {
  const rules = [{ id: "f", method: "POST", path: "/api/*/prompt", when: "after", action: "drop", remaining: 1 }]
  const hits = []
  const request = { method: "POST", url: "http://127.0.0.1:9/api/s/prompt?x=1", stage: "after" }
  assert.equal(claimFault(rules, hits, { ...request, stage: "before" }), null)
  assert.equal(claimFault(rules, hits, { ...request, method: "GET" }), null)
  assert.equal(claimFault(rules, hits, request).id, "f")
  assert.equal(claimFault(rules, hits, request), null)
  assert.deepEqual(hits, [{ id: "f", method: "POST", path: "/api/s/prompt", when: "after", action: "drop" }])
})
