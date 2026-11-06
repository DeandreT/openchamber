#!/usr/bin/env node
/**
 * Builds a long session for switch and render captures.
 *
 * Session-switch cost depends on how much history a session carries, and a
 * development workspace rarely has a session with a hundred turns of code and
 * prose. This creates one through the supported `openchamber session` CLI
 * against the fixture provider, so the history is identical every time it is
 * rebuilt: turns cycle through the prose, code-fence and non-Latin fixture
 * documents at seeding speed.
 *
 *   node scripts/perf/seed-long-session.mjs --port 4599 --dir <project> --turns 120 --title "perf: long 120"
 *
 * The server must have been started with the fixture provider's configuration
 * (see DOCUMENTATION.md). Prints the session id, and with --json the summary.
 *
 * Two options add rows the plain turns never produce, for visual checks:
 * --image sends one more prompt with a small PNG attached, and --compact
 * compacts the session once the turns are in. Without them a seed is
 * unchanged.
 */

import { spawn } from "node:child_process"
import { randomBytes } from "node:crypto"
import { mkdirSync, writeFileSync } from "node:fs"
import { dirname, join, resolve } from "node:path"
import { fileURLToPath } from "node:url"
import process from "node:process"

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..")
const cliPath = join(repoRoot, "packages/web/bin/cli.js")

const HELP = `Usage: node scripts/perf/seed-long-session.mjs --port <port> --dir <path> [options]

Options:
  --port <port>     OpenChamber server port (required)
  --dir <path>      Project directory the session belongs to (required)
  --turns <n>       Number of prompt/response turns (default: 120)
  --title <text>    Session title (default: "perf: long <turns>")
  --models <list>   Comma-separated models cycled per turn
                    (default: perf/stream-20000cps,perf/code-20000cps,perf/unicode-20000cps)
  --session <id>    Append turns to this session instead of creating one
  --image           Then send a prompt with a PNG attachment
  --compact         Then compact the session
  --json            Print a JSON summary
  --help            Show this help`

const parseArgs = (argv) => {
  const options = {
    port: null,
    dir: null,
    turns: 120,
    title: null,
    models: ["perf/stream-20000cps", "perf/code-20000cps", "perf/unicode-20000cps"],
    session: null,
    image: false,
    compact: false,
    json: false,
  }
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index]
    if (value === "--help") { console.log(HELP); process.exit(0) }
    else if (value === "--port") options.port = argv[++index]
    else if (value === "--dir") options.dir = resolve(argv[++index])
    else if (value === "--turns") options.turns = Number(argv[++index])
    else if (value === "--title") options.title = argv[++index]
    else if (value === "--models") options.models = String(argv[++index]).split(",").map((model) => model.trim()).filter(Boolean)
    else if (value === "--session") options.session = argv[++index]
    else if (value === "--image") options.image = true
    else if (value === "--compact") options.compact = true
    else if (value === "--json") options.json = true
    else throw new Error(`Unknown option: ${value}`)
  }
  if (!options.port || !options.dir) throw new Error("--port and --dir are required")
  if (!Number.isInteger(options.turns) || options.turns < 1) throw new Error("--turns must be a positive integer")
  return options
}

const runSessionCli = (args, timeoutMs = 120_000) => new Promise((resolveRun, reject) => {
  const child = spawn(process.execPath, [cliPath, "session", ...args, "--json"], { stdio: ["ignore", "pipe", "pipe"] })
  let stdout = ""
  let stderr = ""
  const timer = setTimeout(() => { child.kill("SIGTERM"); reject(new Error(`session ${args[0]} timed out`)) }, timeoutMs)
  child.stdout.on("data", (chunk) => { stdout += chunk })
  child.stderr.on("data", (chunk) => { stderr += chunk })
  child.on("error", (error) => { clearTimeout(timer); reject(error) })
  child.on("close", (code) => {
    clearTimeout(timer)
    if (code !== 0) { reject(new Error(`session ${args[0]} exited with ${code}: ${stderr.trim() || stdout.trim()}`)); return }
    try { resolveRun(JSON.parse(stdout)) } catch { reject(new Error(`session ${args[0]} returned unparseable output: ${stdout.slice(0, 300)}`)) }
  })
})

// A 24 x 24 gradient: small enough to commit inline, large enough to show.
const ATTACHMENT_PNG = Buffer.from("iVBORw0KGgoAAAANSUhEUgAAABgAAAAYCAIAAABvFaqvAAADnUlEQVR42hXUqxpFERSF0Z1lWZZlWZZlWZZlWf6zLMuyLHuO43iA8VmXub7vQ3zID/WhP8yH/XAf/iN8xI/0kT/KR/1oH3z0j/ExP9bH/jgf9+P7BEIgBUqgBUZgBU7gBUEQBUmQBUVQBU2AoAuGYAqWYAuO4IoHSYRESpRES4zESpzES4IkSpIkS4qkSpoESZcMyZQsyZYcyZUPUgiFVCiFVhiFVTiFVwRFVCRFVhRFVTQFiq4YiqlYiq04iqsepBEaqVEarTEaq3EarwmaqEmarCmaqmkaNF0zNFOzNFtzNFc/yCAM0qAM2mAM1uAM3hAM0ZAM2VAM1dAMGLphGKZhGbbhGK55kEVYpEVZtMVYrMVZvCVYoiVZsqVYqqVZsHTLsEzLsmzLsVz7IIdwSIdyaIdxWIdzeEdwREdyZEdxVEdz4OiO4ZiO5diO47juQR7hkR7l0R7jsR7n8Z7giZ7kyZ7iqZ7mwdM9wzM9y7M9x3P9gwIiIAMqoAMmYAMu4AMhEAMpkAMlUAMtQKAHRmAGVmAHTuCGB0VEREZUREdMxEZcxEdCJEZSJEdKpEZahEiPjMiMrMiOnMiND0qIhEyohE6YhE24hE+EREykRE6URE20BImeGImZWImdOImbHpQRGZlRGZ0xGZtxGZ8JmZhJmZwpmZppGTI9MzIzszI7czI3P6ggCrKgCrpgCrbgCr4QCrGQCrlQCrXQChR6YRRmYRV24RRueVBFVGRFVXTFVGzFVXwlVGIlVXKlVGqlVaj0yqjMyqrsyqnc+qCGaMiGauiGadiGa/hGaMRGauRGadRGa9DojdGYjdXYjdO47UH/w/Ai/cL4YvQC8Fb3Ld1blzfoN6LX3NeWV9D7ykP+r8OACQs2HLjvTH0d0ZEd1dEd07Ed1/Gd0Imd1Mmd0qmd1v9M74zO7KzO7pzO7Q8aiIEcqIEemIEduIEfhEEcpEEelEEdtPH/TB+MwRyswR6cwR0PmoiJnKiJnpiJnbiJn4RJnKRJnpRJnbT5L6lPxmRO1mRPzuTOBy3EQi7UQi/Mwi7cwi/CIi7SIi/Koi7a+jemL8ZiLtZiL87irgdtxEZu1EZvzMZu3MZvwiZu0iZvyqZu2v63t2/GZm7WZm/O5u4HHcRBHtRBH8zBHtzBH8IhHtIhH8qhHtr5D6kfxmEe1mEfzuGeB13ERV7URV/MxV7cxV/CJV7SJV/KpV7a/Y+6X8ZlXtZlX87lXn7fPm2umg+lOgAAAABJRU5ErkJggg==", "base64")
const ATTACHMENT_FILE = "perf-media/gradient.png"
const BASE62 = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"

/** A client message id in OpenCode's `msg_` form: time-ordered, then random. */
const messageId = () => {
  const time = (BigInt(Date.now()) * 4096n + 1n) & 0xffffffffffffn
  const random = [...randomBytes(14)].map((byte) => BASE62[byte % BASE62.length]).join("")
  return `msg_${time.toString(16).padStart(12, "0")}${random}`
}

/** POSTs a session operation to the server's OpenCode routes. */
const sessionOperation = async (port, sessionId, operation, body) => {
  const response = await fetch(`http://localhost:${port}/api/session/${encodeURIComponent(sessionId)}/${operation}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  })
  if (!response.ok) throw new Error(`session ${operation} failed with ${response.status}: ${(await response.text()).slice(0, 300)}`)
}

// The operation returns once it is admitted; the run it starts may not have
// begun yet, so idle has to hold on two reads a second apart.
const waitIdle = async (base, sessionId) => {
  const deadline = Date.now() + 120_000
  let idleReads = 0
  while (Date.now() < deadline) {
    await new Promise((resolveWait) => setTimeout(resolveWait, 1000))
    const status = await runSessionCli(["status", ...base, "--session", sessionId])
    idleReads = status?.sessionStatus?.type === "idle" ? idleReads + 1 : 0
    if (idleReads >= 2) return
  }
  throw new Error("the session did not become idle")
}

const main = async () => {
  const options = parseArgs(process.argv.slice(2))
  const base = ["--dir", options.dir, "--port", String(options.port)]
  let sessionId = options.session
  if (!sessionId) {
    const created = await runSessionCli(["create", ...base, "--title", options.title ?? `perf: long ${options.turns}`])
    sessionId = created.sessionId
  }
  const startedAt = Date.now()
  for (let turn = 1; turn <= options.turns; turn += 1) {
    const model = options.models[(turn - 1) % options.models.length]
    await runSessionCli(["send", ...base, "--session", sessionId, "--prompt", `Turn ${turn}: explain the next part.`, "--model", model, "--wait", "--timeout", "120"])
    if (!options.json && (turn % 10 === 0 || turn === options.turns)) console.log(`${turn}/${options.turns} turns`)
  }
  if (options.image) {
    const file = join(options.dir, ATTACHMENT_FILE)
    mkdirSync(dirname(file), { recursive: true })
    writeFileSync(file, ATTACHMENT_PNG)
    // The prompt runs on the model the last turn selected.
    await sessionOperation(options.port, sessionId, "prompt", {
      id: messageId(),
      text: "What does the attached image show?",
      files: [{ uri: `file://${file}`, name: "gradient.png" }],
    })
    await waitIdle(base, sessionId)
  }
  if (options.compact) {
    await sessionOperation(options.port, sessionId, "compact", {})
    await waitIdle(base, sessionId)
  }
  // A provider that rejected the requests would leave a session of user
  // messages only, which renders nothing like real history. The CLI pages
  // messages (`--all` still returns 10, and a large `--limit` trips the
  // server's cap), so this checks the most recent turns, which a broken
  // provider could not have answered either.
  const checked = Math.min(options.turns, 20)
  const messages = await runSessionCli(["messages", ...base, "--session", sessionId, "--limit", String(checked), "--role", "assistant"])
  const assistant = (messages?.messages ?? []).length
  if (assistant < checked) throw new Error(`Only ${assistant} of the last ${checked} turns got an assistant reply; check the fixture provider.`)
  // Only the last turns are checked, so the count says so: a bare "20" next
  // to 120 turns reads as 100 missing replies.
  const summary = { sessionId, turns: options.turns, lastTurnsChecked: checked, lastTurnsAnswered: assistant, seconds: Math.round((Date.now() - startedAt) / 1000) }
  if (options.json) console.log(JSON.stringify(summary))
  else console.log(`Session ${sessionId}: ${options.turns} turns in ${summary.seconds}s; the last ${checked} all answered`)
}

main().catch((error) => {
  console.error(error instanceof Error ? error.message : error)
  process.exit(1)
})
