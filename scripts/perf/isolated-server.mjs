#!/usr/bin/env node
/**
 * An OpenChamber server for perf captures that shares nothing with the user's
 * own app: its own HOME (OpenCode's sessions database), its own
 * OPENCHAMBER_DATA_DIR, an environment rebuilt from scratch so no
 * OPENCHAMBER_* / OPENCODE_* variable from the desktop app leaks in, no UI
 * password, and the fixture provider registered as the default model.
 *
 * As a command (`bun run profile:serve`) it starts the fixture provider and the
 * server, checks the served bundle, and runs until interrupted, then stops
 * exactly what it started. `profile:compare` uses the same functions.
 *
 * Layout under --root: `state/` (HOME and data dir; the server's whole
 * state), `project/` (the directory sessions belong to), and `seed/` (a
 * snapshot of `state/`, restored with --fresh).
 */

import { execFileSync, spawn } from "node:child_process"
import { cpSync, createWriteStream, existsSync, mkdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs"
import { connect } from "node:net"
import { platform } from "node:os"
import { basename, dirname, join, resolve } from "node:path"
import { fileURLToPath } from "node:url"
import process from "node:process"

import { reservePort, wait } from "./cdp.mjs"
import { AGENT_READ_FILES, agentReadFileContent, fixtureProviderConfig, INTERACTION_SHELL_COMMAND } from "./fixture-provider.mjs"
import { stopProcessTree } from "./process-tree.mjs"

export const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..")
const FIXTURE_SCRIPT = join(REPO_ROOT, "scripts/perf/fixture-provider.mjs")
const MAC_BUNDLED_OPENCODE = "/Applications/OpenChamber.app/Contents/Resources/opencode-cli/opencode"

const HELP = `Usage: bun run profile:serve -- [options]

Starts the fixture provider and an isolated OpenChamber server on a build, and
keeps them running until Ctrl-C, which stops both. Point the profile:*
commands at the printed URL with --dir <root>/project.

Options:
  --repo <checkout>        Checkout whose server and CLI run (default: this one)
  --dist <directory>       Built UI to serve (default: <repo>/packages/web/dist;
                           the diagnostic build is <repo>/tmp/web-dist-diag)
  --root <directory>       State, project and seed directories
                           (default: tmp/perf-serve)
  --fresh                  Restore state from <root>/seed before starting, or
                            start empty when there is no seed
  --interactions           Fresh permission/question fixture; requires a new root
                           directly under /tmp/opencode and an explicit binary.
                           Uses owned free ports; cannot combine with --fresh.
  --port <port>            Server port (default: 4799)
  --fixture-port <port>    Fixture provider port (default: 4798); reused when
                           something already listens there
  --opencode-binary <path> OpenCode CLI (default: $OPENCODE_BINARY, else the
                           one bundled in /Applications/OpenChamber.app on macOS,
                           else the server's own resolution)
  --help                   Show this help`

const portInUse = (port) => new Promise((resolvePort) => {
  const socket = connect({ port, host: "127.0.0.1" })
  socket.once("connect", () => { socket.destroy(); resolvePort(true) })
  socket.once("error", () => resolvePort(false))
})

/**
 * The environment every perf child gets: the parent's, minus anything that
 * would point it at the user's app, plus the given overrides.
 */
export const isolatedEnv = (overrides = {}) => {
  const env = {}
  for (const [key, value] of Object.entries(process.env)) {
    if (/^(OPENCHAMBER_|OPENCODE_|ELECTRON_)/.test(key)) continue
    env[key] = value
  }
  return { ...env, ...overrides }
}

const defaultOpencodeBinary = () => process.env.OPENCODE_BINARY
  || (platform() === "darwin" && existsSync(MAC_BUNDLED_OPENCODE) ? MAC_BUNDLED_OPENCODE : null)

const waitForPort = async (port, timeoutMs, child) => {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    if (child && child.exitCode !== null) return false
    if (await portInUse(port)) return true
    await wait(250)
  }
  return false
}

/** Starts the fixture provider unless one already listens on `port`. */
export const startFixture = async ({ port, logFile }) => {
  if (await portInUse(port)) return { pid: null, reused: true, stop: async () => undefined }
  const child = spawn(process.execPath, [FIXTURE_SCRIPT, String(port)], {
    detached: true,
    windowsHide: true,
    stdio: ["ignore", logFile ? "pipe" : "ignore", logFile ? "pipe" : "ignore"],
    env: isolatedEnv(),
  })
  if (logFile) {
    const log = createWriteStream(logFile)
    child.stdout.pipe(log)
    child.stderr.pipe(log)
  }
  if (!(await waitForPort(port, 15_000, child))) {
    await stopProcessTree(child)
    throw new Error(`The fixture provider did not start on port ${port}${logFile ? `; see ${logFile}` : ""}.`)
  }
  return { pid: child.pid, reused: false, stop: () => stopProcessTree(child) }
}

// Port zero is bound by the owned child itself. IPC acknowledges that exact
// listener; a pre-existing process can never satisfy readiness.
const startInteractionProvider = async ({ state, project, logFile, signal }) => {
  signal?.throwIfAborted()
  const log = createWriteStream(logFile)
  const child = spawn(process.execPath, [FIXTURE_SCRIPT, "0", "--interactions"], {
    cwd: project, detached: true,
    env: isolatedServerEnv({ state, dist: "", config: {}, inherited: { PATH: process.env.PATH, LANG: "C.UTF-8", SHELL: "/bin/sh" } }),
    stdio: ["ignore", "pipe", "pipe", "ipc"],
  })
  child.stdout.pipe(log, { end: false })
  child.stderr.pipe(log, { end: false })
  child.once("close", () => log.end())
  try {
    const port = await new Promise((resolvePort, reject) => {
      const timer = setTimeout(() => finish(new Error("Interaction provider did not acknowledge its listener")), 15000)
      const exited = () => finish(new Error("Interaction provider exited before readiness"))
      const aborted = () => finish(signal.reason)
      const received = (message) => {
        if (Number.isInteger(message?.port) && message.port > 0) finish(null, message.port)
        else finish(new Error("Invalid interaction provider readiness"))
      }
      const finish = (error, value) => {
        clearTimeout(timer)
        child.off("error", finish)
        child.off("exit", exited)
        child.off("message", received)
        signal?.removeEventListener("abort", aborted)
        if (error) reject(error)
        else resolvePort(value)
      }
      child.once("error", finish)
      child.once("exit", exited)
      child.once("message", received)
      signal?.addEventListener("abort", aborted, { once: true })
    })
    return { port, pid: child.pid, stop: () => stopProcessTree(child) }
  } catch (error) {
    await stopProcessTree(child)
    throw error
  }
}

/** The main chunk a build's index.html loads, or null. */
const builtMainChunk = (dist) => {
  const file = join(dist, "index.html")
  if (!existsSync(file)) return null
  return readFileSync(file, "utf8").match(/src="(\/assets\/main-[^"]+)"/)?.[1] ?? null
}

/** The main chunk the running server serves, bypassing every cache. */
const servedMainChunk = async (url, signal) => {
  const response = await fetch(new URL("/", url), { headers: { "cache-control": "no-cache" }, signal })
  return (await response.text()).match(/src="(\/assets\/main-[^"]+)"/)?.[1] ?? null
}

const writeSettings = (state, project) => {
  const file = join(state, "oc-data", "settings.json")
  if (existsSync(file)) return
  const id = `path_${Buffer.from(project).toString("base64url")}`
  const now = Date.now()
  writeFileSync(file, JSON.stringify({
    homeDirectory: join(state, "home"),
    projects: [{ id, path: project, label: basename(project), addedAt: now, lastOpenedAt: now, sidebarCollapsed: false }],
    activeProjectId: id,
    lastDirectory: project,
    defaultModel: "perf/stream-300cps",
  }, null, 2))
}

/** The OpenCode configuration an isolated server runs with: the fixture
 * provider on `fixturePort` as the default model, nothing that reaches out. */
export const isolatedOpencodeConfig = (fixturePort, { interactions = false } = {}) => interactions ? {
  $schema: "https://opencode.ai/config.json",
  providers: {
    perf: {
      name: "Interaction fixture",
      package: "@opencode/ai/providers/openai-compatible",
      settings: { baseURL: `http://127.0.0.1:${fixturePort}/v1`, apiKey: "unused" },
      models: Object.fromEntries(["interaction-shell", "interaction-question", "stream-1200cps"].map((id) => [id, {
        name: id, capabilities: { tools: true, input: ["text"], output: ["text"] },
      }])),
    },
  },
  model: "perf/interaction-shell",
  default_agent: "build",
  agents: Object.fromEntries(["title", "summary", "compaction"].map((id) => [id, { model: "perf/stream-1200cps" }])),
  update: "disable",
  snapshots: false,
  shell: "/bin/sh",
  policies: [
    { action: "provider.use", resource: "*", effect: "deny" },
    { action: "provider.use", resource: "perf", effect: "allow" },
  ],
  permissions: [
    { action: "*", resource: "*", effect: "deny" },
    { action: "question", resource: "*", effect: "allow" },
    { action: "shell", resource: INTERACTION_SHELL_COMMAND, effect: "ask" },
  ],
} : ({
  ...fixtureProviderConfig(fixturePort),
  model: "perf/stream-300cps",
  small_model: "perf/stream-1200cps",
  autoupdate: false,
  share: "disabled",
})

/** Isolate both Node's Windows home lookup and OpenCode's XDG paths. */
export const isolatedServerEnv = ({ state, dist, config, opencodeBinary, inherited = process.env }) => {
  const home = join(state, "home")
  const env = {}
  // Keep OS process-launch essentials, without provider credentials or app state.
  for (const [key, value] of Object.entries(inherited)) {
    if (/^(path|pathext|systemroot|windir|comspec|shell|lang|user|username|temp|tmp|tmpdir)$/i.test(key)) env[key] = value
  }
  Object.assign(env, {
    HOME: home,
    USERPROFILE: home,
    APPDATA: join(home, "AppData", "Roaming"),
    LOCALAPPDATA: join(home, "AppData", "Local"),
    XDG_CONFIG_HOME: join(state, "config"),
    XDG_DATA_HOME: join(state, "data"),
    XDG_CACHE_HOME: join(state, "cache"),
    XDG_STATE_HOME: join(state, "xdg-state"),
    OPENCHAMBER_DATA_DIR: join(state, "oc-data"),
    OPENCHAMBER_DIST_DIR: dist,
    OPENCODE_CONFIG_CONTENT: JSON.stringify(config),
  })
  if (opencodeBinary) env.OPENCODE_BINARY = opencodeBinary
  return env
}

/**
 * Creates the project directory as its own Git repository, so OpenCode does
 * not resolve it to a repository that happens to contain it.
 */
export const ensureProject = (project) => {
  mkdirSync(project, { recursive: true })
  if (!existsSync(join(project, ".git"))) execFileSync("git", ["init", "-q"], { cwd: project })
  for (const file of AGENT_READ_FILES) {
    mkdirSync(dirname(join(project, file)), { recursive: true })
    writeFileSync(join(project, file), agentReadFileContent(file))
  }
}

/** Replaces `state` with `seed`, or empties it when there is no seed. */
export const restoreState = (state, seed) => {
  rmSync(state, { recursive: true, force: true })
  if (seed && existsSync(seed)) cpSync(seed, state, { recursive: true })
}

/**
 * Starts `node <repo>/packages/web/bin/cli.js serve` on `dist` with an
 * isolated home under `state`. Resolves once it answers HTTP, with the served
 * main chunk checked against the build's index.html.
 */
export const startIsolatedServer = async ({ repo, dist, state, project, port, fixturePort, opencodeBinary = defaultOpencodeBinary(), logFile, interactions = false, signal }) => {
  signal?.throwIfAborted()
  if (await portInUse(port)) throw new Error(`Port ${port} is already in use; stop that server or pass another port.`)
  signal?.throwIfAborted()
  const built = builtMainChunk(dist)
  if (!built) throw new Error(`${dist}/index.html has no main chunk; build it first.`)
  mkdirSync(join(state, "home"), { recursive: true })
  mkdirSync(join(state, "oc-data"), { recursive: true })
  if (!interactions) ensureProject(project)
  writeSettings(state, project)
  if (interactions) {
    const settingsFile = join(state, "oc-data", "settings.json")
    const settings = JSON.parse(readFileSync(settingsFile, "utf8"))
    settings.defaultModel = "perf/interaction-shell"
    writeFileSync(settingsFile, JSON.stringify(settings, null, 2))
  }
  const config = isolatedOpencodeConfig(fixturePort, { interactions })
  const env = isolatedServerEnv({ state, dist, config, opencodeBinary,
    ...(interactions ? { inherited: { PATH: process.env.PATH, LANG: "C.UTF-8", SHELL: "/bin/sh", TMPDIR: join(state, "tmp") } } : {}),
  })
  const child = spawn(process.execPath, [join(repo, "packages/web/bin/cli.js"), "serve", "--port", String(port), "--foreground"], {
    cwd: project,
    env,
    // Its own process group, so stopping it stops the OpenCode it manages.
    detached: true,
    windowsHide: true,
    stdio: ["ignore", logFile ? "pipe" : "ignore", logFile ? "pipe" : "ignore"],
  })
  let spawnError
  child.once("error", (error) => { spawnError = error })
  if (logFile) {
    const log = createWriteStream(logFile)
    child.stdout.pipe(log)
    child.stderr.pipe(log)
  }
  const url = `http://127.0.0.1:${port}`
  const stop = () => stopProcessTree(child)
  const deadline = Date.now() + 120_000
  let ready = false
  while (Date.now() < deadline && child.exitCode === null && !spawnError && !signal?.aborted) {
    ready = await fetch(`${url}/`, { signal }).then((response) => response.ok, () => false)
    if (ready) break
    await wait(1000)
  }
  if (!ready) {
    await stop()
    signal?.throwIfAborted()
    if (spawnError) throw spawnError
    throw new Error(`The server did not answer on ${url}${logFile ? `; see ${logFile}` : ""}.`)
  }
  let served
  try {
    served = await servedMainChunk(url, signal)
  } catch (error) {
    await stop()
    throw error
  }
  const bundle = { dist, built, served, matches: built === served }
  if (!bundle.matches) {
    await stop()
    throw new Error(`The server serves ${served} but ${dist} built ${built}; the page would not run this build.`)
  }
  return { url, pid: child.pid, bundle, stop }
}

/** Fresh interaction runs have no seed, repository discovery, or reusable listener. */
export const startInteractionServer = async ({ root, dist, opencodeBinary, repo = REPO_ROOT, signal }) => {
  signal?.throwIfAborted()
  root = resolve(root)
  if (platform() !== "linux" || dirname(root) !== "/tmp/opencode" || realpathSync(dirname(root)) !== "/tmp/opencode") {
    throw new Error("Interaction root must be a new directory directly under /tmp/opencode on Linux")
  }
  if (!opencodeBinary || !existsSync(opencodeBinary)) throw new Error("Interaction fixture requires an explicit OpenCode binary")
  // V2 reads ancestors even across repository boundaries. Refuse ambient
  // configuration and instructions rather than merging them into this test.
  for (const directory of ["/", "/tmp", "/tmp/opencode"]) {
    for (const name of ["opencode.json", "opencode.jsonc", ".opencode", "AGENTS.md", "CLAUDE.md"]) {
      if (existsSync(join(directory, name))) throw new Error(`Interaction ancestor contains ${join(directory, name)}`)
    }
  }
  if (existsSync("/etc/opencode")) throw new Error("Interaction fixture refuses system OpenCode configuration")
  mkdirSync(root, { mode: 0o700 })
  const state = join(root, "state")
  const project = join(root, "project")
  for (const directory of [project, ...["home", "config", "data", "cache", "xdg-state", "tmp", "oc-data"].map((name) => join(state, name))]) {
    mkdirSync(directory, { recursive: true, mode: 0o700 })
  }
  const fixture = await startInteractionProvider({ state, project, logFile: join(root, "fixture.log"), signal })
  let server
  try {
    server = await startIsolatedServer({ repo, dist, state, project, port: await reservePort(), fixturePort: fixture.port,
      opencodeBinary, logFile: join(root, "server.log"), interactions: true, signal })
  } catch (error) {
    await fixture.stop()
    throw error
  }
  return { ...server, root, state, project, fixturePort: fixture.port, fixturePid: fixture.pid,
    stop: async () => { try { return await server.stop() } finally { await fixture.stop() } },
  }
}

/** Signal ownership is scoped to this callback, including pending startup.
 * Callers stop their own ongoing work when the supplied signal aborts. */
export const withInteractionServer = async (options, run) => {
  const controller = new AbortController()
  let interrupt
  const interrupted = new Promise((resolveInterrupt) => { interrupt = resolveInterrupt })
  const onSignal = (name) => {
    const error = new Error(`Interaction fixture interrupted by ${name}`)
    error.code = "INTERACTION_INTERRUPTED"
    controller.abort(error)
    interrupt(error)
  }
  const onInt = () => onSignal("SIGINT")
  const onTerm = () => onSignal("SIGTERM")
  process.on("SIGINT", onInt)
  process.on("SIGTERM", onTerm)
  let server
  try {
    server = await startInteractionServer({ ...options, signal: controller.signal })
    controller.signal.throwIfAborted()
    return await Promise.race([
      run(server, controller.signal),
      interrupted.then((error) => { throw error }),
    ])
  } finally {
    try { await server?.stop() } finally {
      process.off("SIGINT", onInt)
      process.off("SIGTERM", onTerm)
    }
  }
}

const parseArgs = (argv) => {
  const options = { repo: REPO_ROOT, dist: null, root: join(REPO_ROOT, "tmp/perf-serve"), fresh: false, port: 4799, fixturePort: 4798, opencodeBinary: defaultOpencodeBinary() }
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index]
    if (value === "--help") { console.log(HELP); process.exit(0) }
    else if (value === "--repo") options.repo = resolve(argv[++index])
    else if (value === "--dist") options.dist = resolve(argv[++index])
    else if (value === "--root") options.root = resolve(argv[++index])
    else if (value === "--fresh") options.fresh = true
    else if (value === "--interactions") options.interactions = true
    else if (value === "--port") options.port = Number(argv[++index])
    else if (value === "--fixture-port") options.fixturePort = Number(argv[++index])
    else if (value === "--opencode-binary") {
      options.opencodeBinary = resolve(argv[++index])
      options.opencodeBinaryProvided = true
    }
    else throw new Error(`Unknown option: ${value}`)
  }
  options.dist = options.dist ?? join(options.repo, "packages/web/dist")
  return options
}

const main = async () => {
  const options = parseArgs(process.argv.slice(2))
  if (options.interactions) {
    if (!options.opencodeBinaryProvided) throw new Error("--interactions requires an explicit --opencode-binary flag")
    if (options.fresh) throw new Error("--interactions cannot restore a seed")
    try {
      await withInteractionServer(options, async (server, signal) => {
        console.log(JSON.stringify({ url: server.url, root: server.root, state: server.state, project: server.project, fixturePort: server.fixturePort, pid: server.pid, fixturePid: server.fixturePid }))
        await new Promise((done) => signal.addEventListener("abort", done, { once: true }))
      })
    } catch (error) {
      if (error.code !== "INTERACTION_INTERRUPTED") throw error
    }
    return
  }
  const state = join(options.root, "state")
  // The fixture log is opened before startServer creates anything under the
  // root, so a first run on a new --root needs the directory made here.
  mkdirSync(options.root, { recursive: true })
  if (options.fresh) restoreState(state, join(options.root, "seed"))
  const fixture = await startFixture({ port: options.fixturePort, logFile: join(options.root, "fixture.log") })
  let server
  try {
    server = await startIsolatedServer({
      repo: options.repo, dist: options.dist, state, project: join(options.root, "project"),
      port: options.port, fixturePort: options.fixturePort, opencodeBinary: options.opencodeBinary,
      logFile: join(options.root, "server.log"),
    })
  } catch (error) {
    await fixture.stop()
    throw error
  }
  console.log(`Server ${server.url} (pid ${server.pid}) serving ${server.bundle.served} from ${options.dist}`)
  console.log(`Fixture provider on ${options.fixturePort}${fixture.reused ? " (already running, not started here)" : ` (pid ${fixture.pid})`}`)
  console.log(`Project: ${join(options.root, "project")}  State: ${state}  Log: ${join(options.root, "server.log")}`)
  console.log("Ctrl-C stops both.")
  const shutdown = async () => {
    const children = await server.stop()
    await fixture.stop()
    console.log(`Stopped server ${server.pid}${children.length ? ` and its children ${children.join(", ")}` : ""}${fixture.reused ? "" : `, fixture ${fixture.pid}`}.`)
    process.exit(0)
  }
  process.once("SIGINT", shutdown)
  process.once("SIGTERM", shutdown)
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    console.error(`profile:serve failed: ${error.message}`)
    process.exitCode = 1
  })
}
