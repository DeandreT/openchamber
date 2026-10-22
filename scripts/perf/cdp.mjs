/**
 * Shared Chrome DevTools Protocol helpers for OpenChamber performance tooling.
 *
 * `scripts/profile-browser.mjs` and `scripts/profile-idle.mjs` both drive Chrome
 * over CDP. Launching, target discovery, and the minimal protocol client live
 * here so both entry points stay thin and behave identically.
 */

import { spawn } from "node:child_process"
import { createServer } from "node:net"
import { existsSync, mkdtempSync } from "node:fs"
import { rm } from "node:fs/promises"
import { platform, tmpdir } from "node:os"
import { join, resolve } from "node:path"
import process from "node:process"
import { stopProcessTree } from "./process-tree.mjs"

export const wait = (milliseconds) => new Promise((resolveWait) => setTimeout(resolveWait, milliseconds))

const chromeCandidates = () => {
  if (platform() === "darwin") {
    return [
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
      "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
      "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ]
  }
  if (platform() === "win32") {
    return [
      join(process.env.PROGRAMFILES ?? "", "Google/Chrome/Application/chrome.exe"),
      join(process.env["PROGRAMFILES(X86)"] ?? "", "Google/Chrome/Application/chrome.exe"),
      join(process.env.LOCALAPPDATA ?? "", "Google/Chrome/Application/chrome.exe"),
    ]
  }
  return ["/usr/bin/google-chrome", "/usr/bin/google-chrome-stable", "/usr/bin/chromium", "/usr/bin/chromium-browser"]
}

export const resolveChrome = (explicit) => {
  if (explicit) {
    const candidate = resolve(explicit)
    if (!existsSync(candidate)) throw new Error(`Chrome executable not found: ${candidate}`)
    return candidate
  }
  const candidate = chromeCandidates().find((path) => path && existsSync(path))
  if (!candidate) throw new Error("Chrome/Chromium was not found. Pass its path with --chrome.")
  return candidate
}

export const reservePort = () => new Promise((resolvePort, reject) => {
  const server = createServer()
  server.unref()
  server.on("error", reject)
  server.listen(0, "127.0.0.1", () => {
    const address = server.address()
    if (!address || typeof address === "string") {
      server.close()
      reject(new Error("Could not reserve a Chrome debugging port"))
      return
    }
    const port = address.port
    server.close(() => resolvePort(port))
  })
})

const waitForJson = async (url, timeoutMs = 15_000) => {
  const deadline = Date.now() + timeoutMs
  let lastError
  while (Date.now() < deadline) {
    try {
      const response = await fetch(url)
      if (response.ok) return await response.json()
    } catch (error) {
      lastError = error
    }
    await wait(100)
  }
  throw new Error(`Chrome debugging endpoint did not start: ${lastError?.message ?? url}`)
}

export const createPageTarget = async (port) => {
  const baseUrl = `http://127.0.0.1:${port}`
  await waitForJson(`${baseUrl}/json/version`)

  try {
    const response = await fetch(`${baseUrl}/json/new?${encodeURIComponent("about:blank")}`, { method: "PUT" })
    if (response.ok) {
      const target = await response.json()
      if (target?.type === "page" && target.webSocketDebuggerUrl) return target
    }
  } catch {
    // Some Chromium variants do not expose /json/new; use their startup page.
  }

  const targets = await waitForJson(`${baseUrl}/json`)
  const target = targets.find((entry) => entry.type === "page" && entry.webSocketDebuggerUrl)
  if (!target) throw new Error("Chrome did not expose or create a page target")
  return target
}

/**
 * The page target of a browser that is already running, such as the desktop
 * shell launched with `--remote-debugging-port`: its application window, not
 * a DevTools page and not a blank tab.
 */
export const findPageTarget = async (port) => {
  const targets = await waitForJson(`http://127.0.0.1:${port}/json`)
  const pages = targets.filter((entry) => entry.type === "page" && entry.webSocketDebuggerUrl && !entry.url.startsWith("devtools://"))
  const target = pages.find((entry) => entry.url !== "about:blank") ?? pages[0]
  if (!target) throw new Error(`No page target is exposed on port ${port}`)
  return target
}

/**
 * Chrome throttles timers and stops producing frames for windows it considers
 * backgrounded or occluded. A profiling run must never silently measure a
 * throttled renderer, so occlusion and background throttling are disabled for
 * every launch: without these the idle report shows zero layouts per second
 * regardless of how much work the page actually schedules.
 */
const ANTI_THROTTLING_ARGS = [
  "--disable-background-timer-throttling",
  "--disable-backgrounding-occluded-windows",
  "--disable-renderer-backgrounding",
  "--disable-features=CalculateNativeWinOcclusion,IntensiveWakeUpThrottling",
]

const removeTemporaryProfile = async (dir) => {
  // Bun 1.4.3 does not honor fs.rm's retry options on a Windows file lock.
  // Keep the same bounded, linear backoff for both supported runtimes.
  for (let attempt = 0; ; attempt += 1) {
    try {
      await rm(dir, { recursive: true, force: true })
      return
    } catch (error) {
      if (attempt === 20 || !["EBUSY", "EPERM", "ENOTEMPTY", "EMFILE", "ENFILE"].includes(error.code)) throw error
      await wait((attempt + 1) * 100)
    }
  }
}

/**
 * The Chrome profile for one run. `--profile-dir` keeps a directory the caller
 * owns; without it every run gets a fresh temporary profile, removed once
 * Chrome exits. The app keeps sidebar state, the last session, panel widths and
 * the rail order in localStorage per origin, so a shared default profile
 * carried one run's (and one build's) state into the next.
 */
export const resolveProfileDir = (explicit, name) => {
  if (explicit) return { dir: resolve(explicit), temporary: false, remove: async () => {} }
  const dir = mkdtempSync(join(tmpdir(), `openchamber-perf-${name}-`))
  return {
    dir,
    temporary: true,
    remove: () => removeTemporaryProfile(dir),
  }
}

export const launchChrome = ({ chrome, profileDir, port, headless, extraArgs = [] }) => {
  const args = [
    `--remote-debugging-port=${port}`,
    `--user-data-dir=${profileDir}`,
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-background-networking",
    ...ANTI_THROTTLING_ARGS,
    ...extraArgs,
    "about:blank",
  ]
  if (headless) args.unshift("--headless=new", "--disable-gpu")
  // Chrome refuses to start as root with its sandbox on, which is how
  // containers and CI runners usually run it.
  if (process.getuid?.() === 0) args.unshift("--no-sandbox")
  return spawn(chrome, args, { stdio: "ignore" })
}

/** Close the owned browser even when its Windows launcher has already exited. */
export const stopChrome = async (chromeProcess, client) => {
  // Attached desktop windows belong to the caller, not the profiler.
  if (!chromeProcess) return
  if (client) await client.send("Browser.close", {}, { timeoutMs: 5000 }).catch(() => {})
  if (chromeProcess.exitCode !== null || chromeProcess.signalCode !== null) return
  // Browser.close schedules shutdown. Killing it here interrupts child cleanup.
  await new Promise((resolveExit) => {
    const onExit = () => { clearTimeout(timer); resolveExit() }
    const timer = setTimeout(() => {
      chromeProcess.off("exit", onExit)
      resolveExit()
    }, 5000)
    chromeProcess.once("exit", onExit)
  })
  if (chromeProcess.exitCode === null) await stopProcessTree(chromeProcess)
  chromeProcess.unref?.()
}

export class CdpClient {
  constructor(url) {
    this.socket = new WebSocket(url)
    this.nextId = 1
    this.pending = new Map()
    this.listeners = new Map()
  }

  async connect() {
    await new Promise((resolveConnect, reject) => {
      this.socket.addEventListener("open", resolveConnect, { once: true })
      this.socket.addEventListener("error", reject, { once: true })
    })
    // A dropped connection fails what is waiting on it instead of leaving it
    // pending until the send timeout.
    this.socket.addEventListener("close", () => {
      for (const [id, pending] of this.pending) {
        this.pending.delete(id)
        pending.reject(new Error("CDP connection closed"))
      }
    })
    this.socket.addEventListener("message", (event) => {
      const message = JSON.parse(String(event.data))
      if (message.id) {
        const pending = this.pending.get(message.id)
        if (!pending) return
        this.pending.delete(message.id)
        if (message.error) pending.reject(new Error(message.error.message))
        else pending.resolve(message.result ?? {})
        return
      }
      for (const listener of this.listeners.get(message.method) ?? []) listener(message.params ?? {})
    })
  }

  // Bounded: a reply that never arrives once left a capture waiting for half
  // an hour with its own --timeout unable to fire. Generous enough for
  // Profiler.stop and Tracing.end on long captures.
  send(method, params = {}, { timeoutMs = 300_000 } = {}) {
    return this.request({ method, params }, timeoutMs)
  }

  // A command for a target attached to this browser connection with
  // `Target.setAutoAttach({ flatten: true })`.
  sendSession(sessionId, method, params = {}, { timeoutMs = 300_000 } = {}) {
    return this.request({ sessionId, method, params }, timeoutMs)
  }

  request(message, timeoutMs) {
    const id = this.nextId++
    return new Promise((resolveSend, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id)
        reject(new Error(`CDP ${message.method} got no reply within ${Math.round(timeoutMs / 1000)}s`))
      }, timeoutMs)
      const settle = (callback) => (value) => { clearTimeout(timer); callback(value) }
      this.pending.set(id, { resolve: settle(resolveSend), reject: settle(reject) })
      this.socket.send(JSON.stringify({ id, ...message }))
    })
  }

  on(method, listener) {
    const listeners = this.listeners.get(method) ?? new Set()
    listeners.add(listener)
    this.listeners.set(method, listeners)
    return () => listeners.delete(listener)
  }

  onClose(listener) {
    this.socket.addEventListener("close", listener)
    return () => this.socket.removeEventListener("close", listener)
  }

  once(method, timeoutMs = 15_000) {
    return new Promise((resolveEvent, reject) => {
      const timeout = setTimeout(() => {
        unsubscribe()
        reject(new Error(`Timed out waiting for ${method}`))
      }, timeoutMs)
      const unsubscribe = this.on(method, (params) => {
        clearTimeout(timeout)
        unsubscribe()
        resolveEvent(params)
      })
    })
  }

  close() {
    this.socket.close()
  }
}

export const evaluateValue = async (client, expression) => {
  const result = await client.send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true })
  return result.result?.value ?? null
}

// Diagnostic text contains protocol failures only. Browser exception messages
// and arbitrary server text can contain document content or credentials.
export const captureError = (error) => {
  const message = error instanceof Error ? error.message : String(error)
  const networkCode = message.match(/\bnet::ERR_[A-Z0-9_]+\b/)?.[0]
  if (networkCode) return networkCode
  if (error instanceof Error && /^E[A-Z0-9_]+$/.test(error.code ?? "")) return error.code
  if (/^(CDP [A-Za-z.]+ got no reply within \d+s|CDP connection closed|Timed out waiting for [A-Za-z.]+|Navigation command failed|Page evaluation failed)$/.test(message)) return message
  return error instanceof Error ? error.name : "Capture error"
}

/** Preserve result-level failures and observe both waits from the outset.
 * The load event remains the profiler's existing, uncorrelated load signal.
 * Frame/loader evidence is recorded separately; it is not proof of readiness.
 */
export const navigateForCapture = async (client, method, params, diagnostic, { waitForLoad = true, timeoutMs = 60_000, recorder = null } = {}) => {
  let finished = false
  let timer
  let removeLoad = () => {}
  let removeClose = () => {}
  diagnostic.command = method
  diagnostic.result = null
  diagnostic.loadEvent = null
  try {
    const loaded = waitForLoad ? new Promise((resolveLoad) => {
      removeLoad = client.on("Page.loadEventFired", (event) => {
        if (finished || diagnostic.loadEvent) return
        diagnostic.loadEvent = { timestamp: event.timestamp ?? null }
        resolveLoad()
      })
    }) : Promise.resolve()
    const closed = new Promise((_, reject) => {
      removeClose = client.onClose(() => reject(new Error("CDP connection closed")))
    })
    const timeout = new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`Timed out waiting for ${waitForLoad ? "Page.loadEventFired" : method}`)), timeoutMs)
    })
    const command = Promise.resolve().then(() => {
      recorder?.recordCommand("command-sent", diagnostic)
      return client.send(method, params, { timeoutMs })
    }).then((result) => {
      if (finished) return
      diagnostic.result = {
        frameId: result.frameId ?? null,
        loaderId: result.loaderId ?? null,
        errorText: result.errorText ? captureError(result.errorText) : null,
        isDownload: result.isDownload ?? false,
      }
      recorder?.recordCommand("command-result", diagnostic)
      if (result.errorText) throw new Error(result.errorText.match(/\bnet::ERR_[A-Z0-9_]+\b/)?.[0] ?? "Navigation command failed")
    })
    await Promise.race([Promise.all([command, loaded]), closed, timeout])
  } catch (error) {
    diagnostic.error = captureError(error)
    recorder?.recordCommand("navigation-error", diagnostic)
    throw error
  } finally {
    finished = true
    clearTimeout(timer)
    removeLoad()
    removeClose()
  }
}

// Every attempt owns a result, including partial measurements and failures.
// Cleanup proceeds independently and cannot replace the original capture error.
export const recordCaptureAttempt = async ({ summary, run, onFailure, cleanup, persist }) => {
  let failure = null
  summary.valid = false
  summary.cleanupErrors = []
  try {
    await run()
    summary.valid = true
  } catch (error) {
    failure = error
    summary.error = captureError(error)
    try { await onFailure() } catch (snapshotError) { summary.snapshotError = captureError(snapshotError) }
  } finally {
    for (const release of cleanup) {
      try { await release() } catch (error) {
        summary.cleanupErrors.push(captureError(error))
        failure ??= error
      }
    }
    try { await persist(summary) } catch (error) {
      if (failure) throw new AggregateError([failure, error], "Capture failed and its result could not be saved")
      throw error
    }
  }
  if (failure) throw failure
}

/** One bounded, content-free snapshot before a failed capture tears down. */
export const capturePageState = async (client, expectedUrl, recorderName = null) => {
  try {
    const result = await client.send("Runtime.evaluate", {
      expression: `(() => {
        const expected = new URL(${JSON.stringify(expectedUrl)});
        return {
          protocol: location.protocol,
          sameOrigin: location.origin === expected.origin,
          expectedPath: location.pathname === expected.pathname,
          browserError: location.protocol === 'chrome-error:',
          readyState: document.readyState,
          domNodes: document.getElementsByTagName('*').length,
          scripts: document.scripts.length,
          recorderPresent: ${recorderName ? `Boolean(window[${JSON.stringify(recorderName)}])` : "null"}
        };
      })()`,
      returnByValue: true,
    }, { timeoutMs: 3_000 })
    if (result.exceptionDetails) return { evaluationError: "Page evaluation failed" }
    return result.result?.value ?? { evaluationError: "No page-state value" }
  } catch (error) {
    return { evaluationError: captureError(error) }
  }
}
