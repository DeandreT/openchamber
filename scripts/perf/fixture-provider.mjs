#!/usr/bin/env node
/**
 * A deterministic model provider for streaming captures.
 *
 * A real model is a poor stimulus for a comparison: the same prompt returns a
 * different length at a different speed every time, and what a streamed
 * response costs depends on both. This server speaks the OpenAI-compatible
 * chat-completions protocol and always streams the same markdown document, at
 * the rate the requested model name asks for, so two captures differ only in
 * the application under test.
 *
 * OpenCode still does all of its real work. It receives provider chunks, turns
 * them into parts and deltas, and emits its usual event stream, so OpenChamber
 * sees exactly what it would see from a hosted model that happened to be this
 * fast.
 *
 * Model names select the speed: `stream-300cps` streams 300 characters per
 * second. Hosted models sit between roughly 100 and 1500. `code-300cps`
 * streams a different document at the same kind of rate: a short introduction
 * and one 240-line TypeScript block, because what a growing code fence costs
 * (highlighting, lexing) does not show in fences of five lines. `think-30s` stays
 * silent for thirty seconds and then answers in one word, which holds the app
 * in its "working" state with nothing streaming: the cost of the busy
 * indicator and of whatever else runs while an agent thinks or uses a tool.
 * `agent-20tools-300cps` behaves like an agent: twenty steps that each say a
 * line and call the `glob` tool, then the document. OpenCode runs the tools
 * for real, so the turn on screen carries twenty tool parts by the time the
 * answer streams, which is the shape a long agentic turn has and what the
 * cost of re-rendering a whole turn per delta depends on. `agent-*reads-*`
 * reads large files instead, and `agent-100tools-x5-*` makes its calls five
 * at a time, so one step's message carries five tool parts.
 * `unicode-300cps` streams prose with em dashes, curly quotes and Cyrillic
 * paragraphs and a 150-line TypeScript fence whose comments and strings carry
 * the same characters. A string with any character above Latin-1 is stored by
 * V8 as two bytes per character, which changes what concatenation, slicing,
 * regex lexing and hashing cost; the ASCII documents never reach that path.
 * `stream-20000cps`, `code-20000cps`, `unicode-20000cps` and the
 * `agent-*tools-20000cps` models exist only to seed long sessions quickly.
 *
 * Feature models seed the transcript's rarer rows for visual checks:
 * `reasoning-300cps` thinks aloud before it answers, `failtool-300cps` lists
 * files and then edits one that does not exist, so its second call fails, and
 * `rich-300cps` answers with math, a Mermaid diagram and a diff fence that fit
 * one screen, and `diff-300cps` writes a
 * file and then edits it, so the turn carries both diffs.
 *
 * With `--workflows`, `workflow` and `workflow-<n>s` (each turn starts after
 * n seconds) drive the session goal and review loops deterministically from
 * what the conversation says (`planWorkflow`), and the goal's progress check
 * gets the JSON answer its report lines call for (`auditAnswer`).
 */

import { createServer } from "node:http"
import { resolve } from "node:path"
import { fileURLToPath } from "node:url"
import process from "node:process"

const FIXTURE_PROVIDER_ID = "perf"
const FIXTURE_RATES = [100, 300, 600, 1200]
const FIXTURE_CODE_RATES = [300, 1200]
const FIXTURE_UNICODE_RATES = [300, 1200]
// Not a streaming stimulus: answers in about a second, so a session with a
// hundred turns of realistic history can be built in minutes (see
// seed-long-session.mjs).
const FIXTURE_SEED_RATE = 20000
const FIXTURE_THINK_SECONDS = [30]
const FIXTURE_AGENT_STEPS = [20, 40]
const FIXTURE_AGENT_PARALLEL = [{ calls: 100, parallel: 5 }]
// The seeding rate grows agentic history quickly (`profile:session --grow`).
const FIXTURE_AGENT_RATES = [300, FIXTURE_SEED_RATE]
const THINK_ANSWER = "Done."
const FIXTURE_FEATURE_RATES = [300, FIXTURE_SEED_RATE]
const FIXTURE_FEATURES = ["reasoning", "failtool", "diff", "rich"]
const CHUNK_CHARACTERS = 4

export const INTERACTION_SHELL_COMMAND = "printf 'native-permission-ok\\n'"
export const INTERACTION_QUESTIONS = [
  { header: "Choice", question: "Choose a fixture option.", options: [
    { label: "Alpha", description: "First option" }, { label: "Beta", description: "Second option" },
  ] },
  { header: "Multiple", question: "Choose fixture checks.", multiple: true, options: [
    { label: "One", description: "First check" }, { label: "Two", description: "Second check" },
  ] },
  { header: "Custom", question: "Enter fixture text.", options: [] },
]

// What the goal and review loops send, as the server and the magic prompts
// word them. The workflow model answers by these, so one model can be the
// goal agent, the implementer and the reviewer.
const AUDIT_MARKER = "You check the progress of an AI coding agent"
const GOAL_INTRO_MARKER = "Goal mode is active for this session"
const GOAL_CONTINUATION_MARKER = "The session goal is still open"
const HANDOFF_MARKER = "Prepare a handoff for another agent"
const REVIEW_MARKERS = ["Please review the changes described in this handoff", "Please review the current workspace changes"]
const RE_REVIEW_MARKER = "has responded to the previous review feedback"
const FEEDBACK_MARKER = "Another agent reviewed your changes"
export const FINAL_REVIEW_CLEAN = "FINAL_REVIEW_STATUS: no_remaining_findings"
export const FINAL_REVIEW_DISPUTED = "FINAL_REVIEW_STATUS: disputed"

/** The text of a chat-completions message, whether a string or parts. */
const messageText = (message) => {
  const content = message?.content
  if (Array.isArray(content)) return content.map((part) => String(part?.text ?? "")).join("\n")
  return String(content ?? "")
}

const goalReport = ({ done, remaining, needs }) => [
  `Fixture goal turn finished.`,
  "",
  `- Done: ${done}`,
  "- Verified: not verified",
  `- Remaining: ${remaining}`,
  `- Needs from the user: ${needs}`,
].join("\n")

/**
 * The workflow model's answer, from the conversation alone. The objective
 * steers a goal: `[turns:n]` takes n turns (two by default) and `[blocked]`
 * stops on something only the user can give. A review prompt gets findings
 * first and a clean final line on the re-review; `[clean]` anywhere makes the
 * first review clean and `[dispute]` ends the re-review disputed.
 */
export const planWorkflow = (messages) => {
  const users = messages.filter((message) => message?.role === "user").map(messageText)
  const all = users.join("\n")
  const last = users.at(-1) ?? ""
  if (last.includes(HANDOFF_MARKER)) {
    return "Handoff: the fixture change renames the greeting and adds its test. Intent: keep the greeting short."
  }
  if (last.includes(RE_REVIEW_MARKER)) {
    return all.includes("[dispute]")
      ? `The rename is still open.\n\nDisputed findings:\n- Rename: the implementer keeps the old name; I still prefer the new one.\n\n${FINAL_REVIEW_DISPUTED}`
      : `Every earlier finding is fixed.\n\n${FINAL_REVIEW_CLEAN}`
  }
  if (REVIEW_MARKERS.some((marker) => last.includes(marker))) {
    return all.includes("[clean]")
      ? `Nothing to fix.\n\n${FINAL_REVIEW_CLEAN}`
      : "Finding (non-blocker): the greeting test does not cover the empty name."
  }
  if (last.includes(FEEDBACK_MARKER)) return "Fixed the finding: the test now covers the empty name."
  // A session can hold several goals in turn: only the latest one counts.
  const goalStart = users.findLastIndex((text) => text.includes(GOAL_INTRO_MARKER))
  const goal = users.slice(Math.max(0, goalStart))
  if (goal.some((text) => text.includes(GOAL_INTRO_MARKER) || text.includes(GOAL_CONTINUATION_MARKER))) {
    const text = goal.join("\n")
    if (text.includes("[blocked]")) return goalReport({ done: "the first step", remaining: "nothing", needs: "an API key for the fixture service" })
    const turns = Math.max(1, Number(/\[turns:(\d+)\]/.exec(text)?.[1] ?? 2))
    const turn = goal.filter((item) => item.includes(GOAL_CONTINUATION_MARKER)).length + 1
    return turn >= turns
      ? goalReport({ done: `all ${turns} steps`, remaining: "nothing", needs: "nothing" })
      : goalReport({ done: `step ${turn} of ${turns}`, remaining: `steps ${turn + 1} to ${turns}`, needs: "nothing" })
  }
  return THINK_ANSWER
}

/** The progress check's JSON for the report in its `<answer>`, or null for another request. */
export const auditAnswer = (messages) => {
  const prompt = messages.map(messageText).find((text) => text.includes(AUDIT_MARKER))
  if (!prompt) return null
  const answer = /<answer>\n?([\s\S]*?)\n?<\/answer>/.exec(prompt)?.[1] ?? ""
  const said = (label) => {
    const value = new RegExp(`${label}:\\s*(.*)`, "i").exec(answer)?.[1]?.trim() ?? ""
    return value.length > 0 && !/^nothing\b/i.test(value)
  }
  const remaining = said("Remaining")
  return JSON.stringify({ all_done: /Done:/i.test(answer) && !remaining, remaining, needs_user: said("Needs from the user") })
}

// Read-only lookups over the session's directory: small results, no
// permission prompt, and a different one on each step.
const AGENT_GLOBS = ["packages/*/package.json", "scripts/perf/*.mjs", "*.md", ".agents/skills/*/SKILL.md"]
const AGENT_STEP_TEXT = (step, total) => `Step ${step} of ${total}: listing files that the answer should mention.\n\n`

// What an `agent-*reads-*` model reads instead of globbing: source files of
// about 1,500 lines each, so every step carries the full file output a real
// agent's reads do. `isolated-server.mjs` writes them into the project.
export const AGENT_READ_FILES = ["perf-read/alpha.ts", "perf-read/beta.ts", "perf-read/gamma.ts", "perf-read/delta.ts"]
const AGENT_READ_FILE_LINES = 1500

export const agentReadFileContent = (name) => Array.from({ length: AGENT_READ_FILE_LINES / 6 }, (_, index) => [
  `/** Resolves owner ${index} of ${name} and caches the answer for the next lookup. */`,
  `export function resolveOwner${index}(entries: Map<string, { ownerId: string; revision: number }>, key: string) {`,
  `  const entry = entries.get(key + ":${index}")`,
  "  if (!entry) return undefined",
  `  return { ownerId: entry.ownerId, revision: entry.revision + ${index}, source: "${name}" }`,
  "}",
].join("\n")).join("\n")

const SECTION = (index) => `## ${index}. Executing a call in a bytecode virtual machine

A bytecode virtual machine executes a function call by creating a new **activation record**, binding the
arguments, and transferring control to the callee's first instruction. The caller's state is preserved so that
execution resumes at the instruction after the call once the callee returns. Round ${index} of this document
repeats the explanation so that the response is long enough to measure.

1. The compiler emits a \`CALL\` instruction carrying the argument count.
2. The interpreter pops the callee and its arguments from the operand stack.
3. A frame is pushed holding the return address, the base pointer and the locals.
4. The dispatch loop continues at the callee's entry point.

\`\`\`python
def call(vm, argc):
    callee = vm.stack[-argc - 1]
    frame = Frame(callee, base=len(vm.stack) - argc, return_ip=vm.ip)
    vm.frames.append(frame)
    vm.ip = callee.entry
\`\`\`

\`\`\`rust
fn call(&mut self, argc: usize) -> Result<(), VmError> {
    let base = self.stack.len() - argc;
    let callee = self.stack[base - 1].as_function()?;
    self.frames.push(Frame { base, return_ip: self.ip, function: callee.clone() });
    self.ip = callee.entry;
    Ok(())
}
\`\`\`

\`\`\`typescript
function call(vm: Vm, argc: number): void {
  const base = vm.stack.length - argc
  const callee = asFunction(vm.stack[base - 1])
  vm.frames.push({ base, returnIp: vm.ip, fn: callee })
  vm.ip = callee.entry
}
\`\`\`

| Aspect | Stack machine | Register machine |
|---|---|---|
| Operands | implicit, on the operand stack | explicit register indices |
| Instruction size | small | larger |
| Instruction count | higher | lower |
| Call setup | arguments already in place | arguments copied into a window |

Returning reverses the sequence: the result is moved to where the caller expects it, the frame is popped, and
the instruction pointer is restored from the saved return address.

`

const FIXTURE_DOCUMENT = [1, 2, 3].map(SECTION).join("")

const FENCE = "```"

// Plain string concatenation in the sample: nested template literals would
// need escaping here and add nothing to what is measured.
const CODE_FUNCTION = (index) => [
  `/** Resolves the owner of entry ${index} and caches the answer. */`,
  `export function resolveOwner${index}(entries: Map<string, Entry>, key: string): Owner | undefined {`,
  `  const cacheKey = key + ":${index}"`,
  "  const cached = ownerCache.get(cacheKey)",
  "  if (cached) return cached",
  "  const entry = entries.get(key)",
  `  if (!entry || entry.revision < ${index}) return undefined`,
  `  const owner = { id: entry.ownerId, label: "owner-" + entry.ownerId, depth: ${index % 7} }`,
  "  ownerCache.set(cacheKey, owner)",
  "  return owner",
  "}",
  "",
].join("\n")

const FIXTURE_CODE_DOCUMENT = [
  "## Ownership index",
  "",
  "The index below resolves an owner once per key and revision, then serves every later read from the cache.",
  "",
  `${FENCE}typescript`,
  "type Entry = { ownerId: string; revision: number }",
  "type Owner = { id: string; label: string; depth: number }",
  "",
  "const ownerCache = new Map<string, Owner>()",
  "",
  Array.from({ length: 22 }, (_, index) => CODE_FUNCTION(index + 1)).join("\n") + FENCE,
  "",
  "Each function is independent, so the cache can be cleared per revision without touching the others.",
  "",
].join("\n")

// Prose a Ukrainian-speaking user actually receives: typographic punctuation
// mixed into English, and whole Cyrillic paragraphs.
const UNICODE_PROSE = (index) => `## ${index}. Чому рядок «важить» удвічі більше — and why it matters

The renderer receives every delta as a JavaScript string. A string that holds only Latin-1 characters is stored
one byte per character; the moment a single “curly quote”, an em dash — like this one — or a Cyrillic letter
arrives, the whole string becomes two bytes per character. Section ${index} says it again so the response is long.

Коли модель відповідає українською, кожен фрагмент тексту містить кирилицю, тож усі операції над рядком —
конкатенація, пошук, розбиття на блоки Markdown — працюють із дворазовим представленням. Це не помилка,
а властивість рушія V8, і її варто вимірювати на реальному тексті, а не на ASCII.

- “Smart quotes” and ‘single ones’ … ellipses, and non-breaking spaces between numbers and units: 42 ms.
- Списки з тире — звичайна річ: «лапки-ялинки», апостроф у слові «пам’ять», і знак № ${index}.
- Mixed: the cache key «owner:${index}» is hashed — not compared — on every read.

`

const UNICODE_CODE_FUNCTION = (index) => [
  `/** Повертає власника запису ${index} — і кешує відповідь. */`,
  `export function ownerLabel${index}(entries: Map<string, Entry>, key: string): string {`,
  `  const cacheKey = key + "→${index}"`,
  "  const cached = labelCache.get(cacheKey)",
  "  if (cached !== undefined) return cached",
  "  const entry = entries.get(key)",
  `  // Немає запису — повертаємо «невідомо», а не порожній рядок.`,
  `  if (!entry) return "невідомо — ${index}"`,
  `  const label = "Власник “" + entry.ownerId + "” (рівень ${index % 5})"`,
  "  labelCache.set(cacheKey, label)",
  "  return label",
  "}",
  "",
].join("\n")

const FIXTURE_UNICODE_DOCUMENT = [
  UNICODE_PROSE(1),
  UNICODE_PROSE(2),
  "Нижче — індекс власників, який тримає підписи в кеші:",
  "",
  `${FENCE}typescript`,
  "type Entry = { ownerId: string; revision: number }",
  "",
  "// Кеш підписів: ключ — «запис→номер», значення — готовий рядок.",
  "const labelCache = new Map<string, string>()",
  "",
  Array.from({ length: 12 }, (_, index) => UNICODE_CODE_FUNCTION(index + 1)).join("\n") + FENCE,
  "",
  UNICODE_PROSE(3),
].join("\n")

// Every Markdown construct the chat renders, for side-by-side visual checks
// of two renderers rather than for timing.
const FIXTURE_SHOWCASE_DOCUMENT = [
  "# Showcase heading",
  "",
  "A paragraph with **bold**, *italic*, ~~struck~~, `inline code`, a [named link](https://example.com/page) and a bare https://example.com/docs URL.",
  "",
  "## Lists and tasks",
  "",
  "- First item",
  "- Second item with a nested list",
  "  - Nested one",
  "  - Nested two",
  "",
  "1. Ordered one",
  "2. Ordered two",
  "",
  "- [x] Done task",
  "- [ ] Open task",
  "",
  "> A quote with **bold** text.",
  ">",
  "> A second quoted paragraph.",
  "",
  "## Table",
  "",
  "| Name | Value | Note |",
  "| :--- | ---: | :---: |",
  "| alpha | 1 | `code` |",
  "| beta | 22 | a longer note that wraps when the column is narrow enough to need it |",
  "",
  "## Math",
  "",
  "Inline $E = mc^2$ and \\(a^2 + b^2 = c^2\\); currency stays text: $5 and $10.",
  "",
  "$$\\int_0^1 x^2\\,dx = \\frac{1}{3}$$",
  "",
  "\\[",
  "\\sum_{i=1}^{n} i = \\frac{n(n+1)}{2}",
  "\\]",
  "",
  "## Disclosure",
  "",
  "<details><summary>Click to expand</summary>",
  "",
  "Hidden **content** with a list:",
  "",
  "- one",
  "- two",
  "",
  "</details>",
  "",
  "## Diagram",
  "",
  `${FENCE}mermaid`,
  "graph TD",
  "  A[Start] --> B{Check}",
  "  B -->|yes| C[Done]",
  "  B -->|no| A",
  FENCE,
  "",
  "## Code",
  "",
  `${FENCE}python`,
  "def greet(name: str) -> str:",
  '    return f"Hello, {name}!"',
  FENCE,
  "",
  FENCE,
  "plain fence without a language",
  FENCE,
  "",
  "Raw HTML stays text: <div class=\"fixed\">inert</div>",
  "",
  "---",
  "",
  "Final line after a rule.",
  "",
].join("\n")

// What the feature models say and do. The reasoning streams before the answer
// as the provider's `reasoning_content`.
const FEATURE_REASONING = [
  "**Planning the answer**",
  "",
  "The request asks for an explanation, so I will first check what the transcript already shows,",
  "then pick the two parts worth expanding and keep the rest short.",
  "",
  "- read the question again",
  "- name the parts in order",
].join("\n")
const FEATURE_ANSWER = {
  reasoning: "The plan above names the parts; this answer follows it.",
  failtool: "The file does not exist, so the edit failed and the answer stops here.",
  diff: "The file now holds the renamed function.",
  rich: [
    "Inline math $E = mc^2$ beside money that stays text: $5 and $10.",
    "",
    "$$\\int_0^1 x^2\\,dx = \\frac{1}{3}$$",
    "",
    "```mermaid",
    "graph LR",
    "  A[Start] --> B{Check}",
    "  B -->|yes| C[Done]",
    "```",
    "",
    "```diff",
    "-const name = 'old'",
    "+const name = 'new'",
    "```",
  ].join("\n"),
}
export const FEATURE_DIFF_FILE = "perf-diff/sample.ts"
const FEATURE_DIFF_CONTENT = "export function greet(name: string): string {\n  return `Hello, ${name}`\n}\n"
const featureCalls = (feature, step) => {
  if (feature === "failtool" && step === 0) {
    return [{ id: "call_fixture_listing", name: "glob", arguments: { pattern: "perf-read/*.ts" } }]
  }
  if (feature === "failtool" && step === 1) {
    return [{
      id: "call_fixture_missing",
      name: "edit",
      arguments: { path: "perf-missing/does-not-exist.ts", oldString: "before", newString: "after" },
    }]
  }
  if (feature === "diff" && step === 0) {
    return [{ id: "call_fixture_write", name: "write", arguments: { path: FEATURE_DIFF_FILE, content: FEATURE_DIFF_CONTENT } }]
  }
  if (feature === "diff" && step === 1) {
    return [{
      id: "call_fixture_edit",
      name: "edit",
      arguments: { path: FEATURE_DIFF_FILE, oldString: "export function greet(", newString: "export function welcome(" },
    }]
  }
  return []
}

// OpenCode asks for a compaction summary in a fixed Markdown template and
// rejects one without its headings; only `seed-long-session.mjs --compact`
// asks for one.
const COMPACTION_SUMMARY = [
  "## Objective",
  "- Seed a session whose history ends in a compaction.",
  "## Requirements",
  "- (none)",
  "## Decisions",
  "- Keep the fixture deterministic.",
  "## Work State",
  "### Completed",
  "- Every seeded turn",
  "### Active",
  "- (none)",
  "### Blocked",
  "- (none)",
  "## Next Move",
  "1. (none)",
  "## Relevant Files",
  "(none)",
  "## Important Context",
  "- (none)",
].join("\n")
const isCompactionRequest = (messages) => messages.some((message) => {
  const content = messageText(message)
  return content.includes("<template>") && content.includes("## Objective")
})

const parseModel = (model) => {
  const feature = /(reasoning|failtool|diff|rich)-(\d+)cps/.exec(String(model ?? ""))
  if (feature) {
    return { delayMs: 0, charactersPerSecond: Math.max(1, Number(feature[2])), document: FEATURE_ANSWER[feature[1]], toolSteps: 0, feature: feature[1] }
  }
  const think = /think-(\d+)s/.exec(String(model ?? ""))
  if (think) return { delayMs: Number(think[1]) * 1000, charactersPerSecond: 1200, document: THINK_ANSWER, toolSteps: 0 }
  const agent = /agent-(\d+)(tools|reads)(?:-x(\d+))?-(\d+)cps/.exec(String(model ?? ""))
  if (agent) {
    return {
      delayMs: 0,
      charactersPerSecond: Math.max(1, Number(agent[4])),
      document: FIXTURE_DOCUMENT,
      toolSteps: Number(agent[1]),
      reads: agent[2] === "reads",
      parallel: Math.max(1, Number(agent[3] ?? 1)),
    }
  }
  const rate = /(stream|code|unicode|showcase)-(\d+)cps/.exec(String(model ?? ""))
  const documents = { code: FIXTURE_CODE_DOCUMENT, unicode: FIXTURE_UNICODE_DOCUMENT, showcase: FIXTURE_SHOWCASE_DOCUMENT }
  return {
    delayMs: 0,
    charactersPerSecond: rate ? Math.max(1, Number(rate[2])) : 300,
    document: documents[rate?.[1]] ?? FIXTURE_DOCUMENT,
    toolSteps: 0,
  }
}

/**
 * What this request should answer. An agent model counts the tool results
 * since the last user message: while calls remain it says a line and makes
 * the next `parallel` of them in one step, and once every call has run it
 * streams the document. Counting from the last prompt makes every turn of a
 * long session agentic, not only the first.
 */
const planResponse = (model, body, interactions = false, workflows = false) => {
  const messages = Array.isArray(body.messages) ? body.messages : []
  if (workflows) {
    const audit = auditAnswer(messages)
    if (audit) return { delayMs: 0, charactersPerSecond: 20000, text: audit, toolCalls: [] }
    const workflow = /^workflow(?:-(\d+)s)?$/.exec(String(model ?? ""))
    if (workflow) {
      return { delayMs: Number(workflow[1] ?? 0) * 1000, charactersPerSecond: 1200, text: planWorkflow(messages), toolCalls: [] }
    }
  }
  const { delayMs, charactersPerSecond, document, toolSteps, reads, parallel = 1, feature } = parseModel(model)
  const lastPrompt = messages.findLastIndex((message) => message?.role === "user")
  const toolResults = messages.slice(lastPrompt + 1).filter((message) => message?.role === "tool").length
  if (isCompactionRequest(messages)) return { delayMs: 0, charactersPerSecond: Math.max(charactersPerSecond, 2000), text: COMPACTION_SUMMARY, toolCalls: [] }
  if (String(model).startsWith("interaction-")) {
    if (!interactions) throw new Error("Interaction fixtures require --interactions")
    const name = { "interaction-shell": "shell", "interaction-question": "question" }[model]
    if (!name) throw new Error(`Unknown interaction fixture: ${model}`)
    if (!body.tools?.some((tool) => tool.type === "function" && tool.function?.name === name)) {
      throw new Error(`Interaction fixture requires the ${name} tool in the request`)
    }
    if (lastPrompt < 0) throw new Error("Interaction fixture requires a user message")
    return {
      delayMs: 0, charactersPerSecond: 20000,
      text: toolResults ? "Fixture interaction complete." : "",
      toolCalls: toolResults ? [] : [{
        id: `call_fixture_${name}_1`, name,
        arguments: name === "shell"
          ? { command: INTERACTION_SHELL_COMMAND, timeout: 5000, background: false }
          : { questions: INTERACTION_QUESTIONS },
      }],
    }
  }
  if (feature) {
    const calls = featureCalls(feature, toolResults)
    return {
      delayMs, charactersPerSecond,
      reasoning: feature === "reasoning" ? FEATURE_REASONING : "",
      text: calls.length > 0 ? "" : document,
      toolCalls: calls,
    }
  }
  if (toolSteps > 0 && toolResults < toolSteps) {
    const calls = Array.from({ length: Math.min(parallel, toolSteps - toolResults) }, (_, offset) => {
      const call = toolResults + offset
      return reads
        ? { id: `call_fixture_${call + 1}`, name: "read", arguments: { path: AGENT_READ_FILES[call % AGENT_READ_FILES.length] } }
        : { id: `call_fixture_${call + 1}`, name: "glob", arguments: { pattern: AGENT_GLOBS[call % AGENT_GLOBS.length] } }
    })
    return { delayMs, charactersPerSecond, text: AGENT_STEP_TEXT(toolResults + 1, toolSteps), toolCalls: calls }
  }
  return { delayMs, charactersPerSecond, text: document, toolCalls: [] }
}

const chunkFrame = (model, delta, finishReason = null) => ({
  id: "chatcmpl-fixture",
  object: "chat.completion.chunk",
  created: 0,
  model,
  choices: [{ index: 0, delta, finish_reason: finishReason }],
})

const sseFrame = (payload) => `data: ${JSON.stringify(payload)}\n\n`

const readBody = (request) => new Promise((resolveBody) => {
  let body = ""
  request.on("data", (chunk) => { body += chunk })
  request.on("end", () => {
    try {
      resolveBody(JSON.parse(body || "{}"))
    } catch {
      resolveBody({})
    }
  })
})

const usageFor = (text) => ({ prompt_tokens: 1, completion_tokens: Math.ceil(text.length / 4), total_tokens: 1 + Math.ceil(text.length / 4) })

/**
 * Streams text against a wall-clock schedule rather than a fixed interval, so
 * timer drift cannot change the delivered rate between runs. Resolves once the
 * whole text is out, or rejects when the client went away first.
 */
const streamText = (response, model, text, { delayMs, charactersPerSecond }, field = "content") => new Promise((resolveStream, reject) => {
  const startedAt = Date.now() + delayMs
  let sent = 0
  const timer = setInterval(() => {
    const elapsedSeconds = Math.max(0, Date.now() - startedAt) / 1000
    const due = Math.min(text.length, Math.floor(elapsedSeconds * charactersPerSecond))
    while (sent < due) {
      const next = Math.min(due, sent + CHUNK_CHARACTERS)
      response.write(sseFrame(chunkFrame(model, { [field]: text.slice(sent, next) })))
      sent = next
    }
    if (sent < text.length) return
    clearInterval(timer)
    resolveStream()
  }, 5)
  response.on("close", () => {
    clearInterval(timer)
    reject(new Error("client closed the stream"))
  })
})

const streamCompletion = async (response, model, plan, waitForTurnRelease) => {
  response.write(sseFrame(chunkFrame(model, { role: "assistant", content: "" })))
  if (plan.delayMs > 0 && waitForTurnRelease) {
    if (!await waitForTurnRelease(response)) return
    plan = { ...plan, delayMs: 0 }
  }
  try {
    if (plan.reasoning) await streamText(response, model, plan.reasoning, plan, "reasoning_content")
    await streamText(response, model, plan.text, plan.reasoning ? { ...plan, delayMs: 0 } : plan)
  } catch {
    return
  }
  if (plan.toolCalls.length > 0) {
    // The shape the OpenAI protocol streams calls in: each call's name with
    // its first frame under its own index, the arguments as they are
    // produced, and a matching finish reason.
    plan.toolCalls.forEach(({ id, name, arguments: args }, index) => {
      response.write(sseFrame(chunkFrame(model, { tool_calls: [{ index, id, type: "function", function: { name, arguments: "" } }] })))
      response.write(sseFrame(chunkFrame(model, { tool_calls: [{ index, function: { arguments: JSON.stringify(args) } }] })))
    })
    response.write(sseFrame({ ...chunkFrame(model, {}, "tool_calls"), usage: usageFor(plan.text) }))
  } else {
    response.write(sseFrame({ ...chunkFrame(model, {}, "stop"), usage: usageFor(plan.text) }))
  }
  response.end("data: [DONE]\n\n")
}

export const startFixtureProvider = (port, { interactions = false, workflows = false } = {}) => new Promise((resolveServer, reject) => {
  let holdTurns = false
  const waitingTurns = new Set()
  const waitForTurnRelease = (response) => new Promise((done) => {
    const closed = () => finish(false)
    const finish = (released) => {
      waitingTurns.delete(release)
      response.off("close", closed)
      done(released)
    }
    const release = () => finish(true)
    waitingTurns.add(release)
    response.once("close", closed)
  })
  const server = createServer(async (request, response) => {
    if (request.url === "/__fixture/turn-gate") {
      if (request.method === "PUT") {
        const body = await readBody(request)
        if (body?.held !== true && body?.held !== false) {
          response.writeHead(400).end()
          return
        }
        holdTurns = body.held
        if (!holdTurns) for (const release of [...waitingTurns]) release()
      } else if (request.method !== "GET") {
        response.writeHead(405).end()
        return
      }
      response.writeHead(200, { "content-type": "application/json" })
      response.end(JSON.stringify({ held: holdTurns, waiting: waitingTurns.size }))
      return
    }
    if (request.method !== "POST" || !request.url?.endsWith("/chat/completions")) {
      response.writeHead(404).end()
      return
    }
    const body = await readBody(request)
    if (String(body.model).startsWith("interaction-") && !body.stream) {
      response.writeHead(400, { "content-type": "application/json" })
      response.end(JSON.stringify({ error: { message: "Interaction fixtures require streaming", type: "invalid_request_error" } }))
      return
    }
    // The goal's progress check is answered as its report says.
    const audit = workflows ? auditAnswer(Array.isArray(body.messages) ? body.messages : []) : null
    if (audit && !body.stream) {
      response.writeHead(200, { "content-type": "application/json" })
      response.end(JSON.stringify({
        id: "chatcmpl-fixture",
        object: "chat.completion",
        created: 0,
        model: body.model,
        choices: [{ index: 0, message: { role: "assistant", content: audit }, finish_reason: "stop" }],
        usage: usageFor(audit),
      }))
      return
    }
    // Title generation and other auxiliary calls are not part of the stimulus.
    if (!body.stream) {
      response.writeHead(200, { "content-type": "application/json" })
      response.end(JSON.stringify({
        id: "chatcmpl-fixture",
        object: "chat.completion",
        created: 0,
        model: body.model,
        choices: [{ index: 0, message: { role: "assistant", content: "Streaming capture" }, finish_reason: "stop" }],
        usage: usageFor("Streaming capture"),
      }))
      return
    }
    let plan
    try {
      plan = planResponse(body.model, body, interactions, workflows)
    } catch (error) {
      response.writeHead(400, { "content-type": "application/json" })
      response.end(JSON.stringify({ error: { message: error.message, type: "invalid_request_error" } }))
      return
    }
    response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache", connection: "keep-alive" })
    void streamCompletion(response, body.model, plan, holdTurns ? waitForTurnRelease : null)
  })
  server.on("error", reject)
  server.listen(port, "127.0.0.1", () => resolveServer(server))
})

/** OpenCode configuration that registers this provider; pass it as `OPENCODE_CONFIG_CONTENT`. */
export const FIXTURE_WORKFLOW_MODELS = ["workflow", "workflow-1s"]

export const fixtureProviderConfig = (port, { interactions = false, workflows = false } = {}) => ({
  provider: {
    [FIXTURE_PROVIDER_ID]: {
      npm: "@ai-sdk/openai-compatible",
      name: "Performance fixture",
      options: { baseURL: `http://127.0.0.1:${port}/v1`, apiKey: "unused" },
      models: Object.fromEntries([
        ...(interactions ? ["interaction-shell", "interaction-question"].map((id) => [id, { name: id, tool_call: true }]) : []),
        ...(workflows ? FIXTURE_WORKFLOW_MODELS.map((id) => [id, { name: id }]) : []),
        ...FIXTURE_RATES.map((rate) => [`stream-${rate}cps`, { name: `Fixture stream, ${rate} characters/s` }]),
        ...FIXTURE_CODE_RATES.map((rate) => [`code-${rate}cps`, { name: `Fixture code block, ${rate} characters/s` }]),
        ...FIXTURE_UNICODE_RATES.map((rate) => [`unicode-${rate}cps`, { name: `Fixture non-Latin prose and code, ${rate} characters/s` }]),
        ...["stream", "code", "unicode", "showcase"].map((kind) => [`${kind}-${FIXTURE_SEED_RATE}cps`, { name: `Fixture ${kind}, seeding speed` }]),
        ...FIXTURE_THINK_SECONDS.map((seconds) => [`think-${seconds}s`, { name: `Fixture silence, ${seconds}s` }]),
        ...FIXTURE_FEATURES.flatMap((feature) => FIXTURE_FEATURE_RATES.map((rate) => [
          `${feature}-${rate}cps`,
          { name: `Fixture ${feature}, ${rate} characters/s`, tool_call: feature !== "reasoning", reasoning: feature === "reasoning" },
        ])),
        ...FIXTURE_AGENT_STEPS.flatMap((steps) => FIXTURE_AGENT_RATES.flatMap((rate) => [
          [`agent-${steps}tools-${rate}cps`, { name: `Fixture agent, ${steps} tool calls then ${rate} characters/s`, tool_call: true }],
          [`agent-${steps}reads-${rate}cps`, { name: `Fixture agent, ${steps} file reads then ${rate} characters/s`, tool_call: true }],
        ])),
        // Parallel calls: a step that makes several calls at once is one
        // message with that many tool parts.
        ...FIXTURE_AGENT_PARALLEL.flatMap(({ calls, parallel }) => FIXTURE_AGENT_RATES.map((rate) => [
          `agent-${calls}tools-x${parallel}-${rate}cps`,
          { name: `Fixture agent, ${calls} tool calls ${parallel} at a time, then ${rate} characters/s`, tool_call: true },
        ])),
      ]),
    },
  },
})

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const port = Number(process.argv[2] ?? 4601)
  const interactions = process.argv.includes("--interactions")
  const workflows = process.argv.includes("--workflows")
  if (process.argv.includes("--print-config")) {
    console.log(JSON.stringify(fixtureProviderConfig(port, { interactions, workflows })))
  } else {
    const server = await startFixtureProvider(port, { interactions, workflows })
    console.log(`Fixture provider listening on http://127.0.0.1:${server.address().port}/v1 (${FIXTURE_DOCUMENT.length} characters per response)`)
    console.log(`Models: ${Object.keys(fixtureProviderConfig(port, { interactions, workflows }).provider[FIXTURE_PROVIDER_ID].models).map((id) => `${FIXTURE_PROVIDER_ID}/${id}`).join(", ")}`)
    process.send?.({ port: server.address().port })
  }
}
