#!/usr/bin/env node
/**
 * Electron side of the parity runner.
 *
 * Launches a packaged Electron build with an environment the runner supplies,
 * holds one CDP connection to its application page and answers one JSON
 * command per stdin line with one JSON reply per stdout line. Input goes
 * through `Input.dispatch*Event`, so pointer and keyboard actions take the
 * same hit-testing and focus paths as a user's. Nothing here calls
 * application code directly.
 *
 * Commands: launch, wait, rows, inflight, rects, targets, observe, fonts, click, key,
 * type, text, screenshot, close.
 */
import { spawn } from "node:child_process"
import { writeFileSync } from "node:fs"
import { createInterface } from "node:readline"

import { CdpClient, reservePort, wait } from "../perf/cdp.mjs"

let child = null
let client = null
// Fetch and XHR requests the page started and that have not ended. Streams
// that stay open by design (EventSource) are not counted.
const inflight = new Set()
const TRACKED_REQUESTS = new Set(["Fetch", "XHR"])

const evaluate = async (expression) => {
  const result = await client.send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true })
  if (result.exceptionDetails) throw new Error(result.exceptionDetails.exception?.description ?? "evaluation failed")
  return result.result.value
}

const pageTarget = async (port, deadline) => {
  while (Date.now() < deadline) {
    try {
      const targets = await (await fetch(`http://127.0.0.1:${port}/json`)).json()
      const page = targets.find((target) => target.type === "page" && target.url.startsWith("openchamber-ui://app/index.html"))
      if (page?.webSocketDebuggerUrl) return page
    } catch {
    }
    await wait(100)
  }
  throw new Error("the application page never appeared")
}

/**
 * The sidebar rows in display order, with the state a row shows. A sticky
 * header leaves an empty placeholder in the list and paints over it, so such
 * a row reads the header painted at its position. Disclosure state comes from
 * the row's own toggle, never from a menu trigger inside it.
 */
const ROWS = `(() => [...document.querySelectorAll('[data-sidebar-row-key]')]
  .sort((left, right) => left.getBoundingClientRect().top - right.getBoundingClientRect().top)
  .map((element) => {
    const rect = element.getBoundingClientRect()
    const painted = element.innerText.trim()
      ? element
      : document.elementFromPoint(rect.x + rect.width / 2, rect.y + 14)?.closest('button') ?? element
    const session = element.querySelector('[data-session-row]')
    const toggles = [painted, ...painted.querySelectorAll('[aria-expanded]')]
      .filter((node) => node.hasAttribute('aria-expanded') && !node.hasAttribute('aria-haspopup'))
    const label = painted.innerText.split('\\n').map((line) => line.trim()).find(Boolean) ?? ''
    return {
      key: element.getAttribute('data-sidebar-row-key'),
      label,
      selected: session?.getAttribute('aria-current') === 'page',
      expanded: toggles.length ? toggles[0].getAttribute('aria-expanded') === 'true' : null,
      rect: [rect.x, rect.y, rect.width, rect.height],
    }
  }))()`

const rectOf = (selector) => `(() => {
  const element = document.querySelector(${JSON.stringify(selector)})
  if (!element) return null
  const rect = element.getBoundingClientRect()
  return [rect.x, rect.y, rect.width, rect.height]
})()`

const handlers = {
  async launch({ app, env, scale, width, height, timeoutMs = 120_000 }) {
    const port = await reservePort()
    // The basic password store keeps Chromium off the desktop's secret
    // service, which would ask the user to unlock their keyring.
    const args = [`--remote-debugging-port=${port}`, `--force-device-scale-factor=${scale}`, "--password-store=basic"]
    child = spawn(app, args, { env, stdio: "ignore", detached: true })
    child.unref()
    const page = await pageTarget(port, Date.now() + timeoutMs)
    client = new CdpClient(page.webSocketDebuggerUrl)
    await client.connect()
    await client.send("Runtime.enable")
    client.on("Network.requestWillBeSent", ({ requestId, type }) => {
      if (TRACKED_REQUESTS.has(type)) inflight.add(requestId)
    })
    client.on("Network.loadingFinished", ({ requestId }) => inflight.delete(requestId))
    client.on("Network.loadingFailed", ({ requestId }) => inflight.delete(requestId))
    await client.send("Network.enable")
    // Electron's DevTools endpoint cannot size the window. The default main
    // window has the standard client size; when it differs (a stored size, a
    // window manager constraint), emulate the viewport and say so.
    const inner = await evaluate("[window.innerWidth, window.innerHeight]")
    let emulated = false
    if (inner[0] !== width || inner[1] !== height) {
      await client.send("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: scale, mobile: false })
      emulated = true
    }
    return { pid: child.pid, port, window: inner, emulated, viewport: await evaluate("[window.innerWidth, window.innerHeight]") }
  },
  async wait({ expression, timeoutMs = 60_000 }) {
    const deadline = Date.now() + timeoutMs
    while (Date.now() < deadline) {
      try {
        if (await evaluate(expression)) {
          // A painted frame after the condition held.
          await evaluate("new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve(true))))")
          return { met: true }
        }
      } catch {
      }
      await wait(50)
    }
    throw new Error(`condition not met within ${timeoutMs} ms: ${expression}`)
  },
  async rows() {
    return { rows: await evaluate(ROWS) }
  },
  /** How many fetch and XHR requests are still in flight. */
  async inflight() {
    return { count: inflight.size }
  },
  async rects({ selectors }) {
    const rects = {}
    for (const [name, selector] of Object.entries(selectors)) rects[name] = await evaluate(rectOf(selector))
    return { rects }
  },
  /** Rectangles of controls named by a CSS selector or by a button's exact text. */
  async targets({ specs }) {
    const rects = {}
    for (const [name, spec] of Object.entries(specs)) {
      rects[name] = await evaluate(spec.button === undefined ? rectOf(spec.selector) : `(() => {
        const element = [...document.querySelectorAll('button')].find((button) => button.innerText.trim() === ${JSON.stringify(spec.button)})
        if (!element) return null
        const rect = element.getBoundingClientRect()
        return [rect.x, rect.y, rect.width, rect.height]
      })()`)
    }
    return { rects }
  },
  /**
   * Clicks the centre of `selector`. With `within`, the element under that
   * point must be inside `within` before the press: a list that is still
   * settling can move another row under a centre read a moment earlier.
   */
  async click({ selector, within, timeoutMs = 5_000 }) {
    const deadline = Date.now() + timeoutMs
    let x
    let y
    for (;;) {
      const rect = await evaluate(rectOf(selector))
      if (!rect) throw new Error(`no element for ${selector}`)
      x = rect[0] + rect[2] / 2
      y = rect[1] + rect[3] / 2
      if (!within) break
      const hit = await evaluate(`!!document.elementFromPoint(${x}, ${y})?.closest(${JSON.stringify(within)})`)
      if (hit) break
      if (Date.now() > deadline) throw new Error(`the centre of ${selector} stays outside ${within}`)
      await wait(100)
    }
    await client.send("Input.dispatchMouseEvent", { type: "mouseMoved", x, y })
    await client.send("Input.dispatchMouseEvent", { type: "mousePressed", x, y, button: "left", clickCount: 1 })
    await client.send("Input.dispatchMouseEvent", { type: "mouseReleased", x, y, button: "left", clickCount: 1 })
    return { at: [x, y] }
  },
  async key({ key, code, keyCode, modifiers = 0 }) {
    for (const type of ["rawKeyDown", "keyUp"]) {
      await client.send("Input.dispatchKeyEvent", { type, key, code, windowsVirtualKeyCode: keyCode, modifiers })
    }
    return {}
  },
  async type({ text }) {
    await client.send("Input.insertText", { text })
    return {}
  },
  /** Read-only observation, such as layout and computed styles. */
  async observe({ expression }) {
    return { value: await evaluate(expression) }
  },
  async text({ selector }) {
    return { text: await evaluate(`document.querySelector(${JSON.stringify(selector)})?.innerText ?? null`) }
  },
  /** The platform font faces Chromium drew a node's text with, and their glyph counts. */
  async fonts({ selector }) {
    await client.send("DOM.enable")
    await client.send("CSS.enable")
    const { root } = await client.send("DOM.getDocument", { depth: 0 })
    const { nodeId } = await client.send("DOM.querySelector", { nodeId: root.nodeId, selector })
    if (!nodeId) throw new Error(`no element for ${selector}`)
    const { fonts } = await client.send("CSS.getPlatformFontsForNode", { nodeId })
    return { fonts }
  },
  async screenshot({ path }) {
    const { data } = await client.send("Page.captureScreenshot", { format: "png" })
    writeFileSync(path, Buffer.from(data, "base64"))
    return { path }
  },
  async close() {
    try { client?.close() } catch {}
    if (child) {
      try { process.kill(-child.pid, "SIGTERM") } catch {}
      await wait(2000)
      try { process.kill(-child.pid, "SIGKILL") } catch {}
    }
    return {}
  },
}

const lines = createInterface({ input: process.stdin })
for await (const line of lines) {
  if (!line.trim()) continue
  const { id, cmd, ...params } = JSON.parse(line)
  try {
    const result = await handlers[cmd](params)
    process.stdout.write(`${JSON.stringify({ id, ok: true, ...result })}\n`)
  } catch (error) {
    process.stdout.write(`${JSON.stringify({ id, ok: false, error: String(error?.message ?? error) })}\n`)
  }
  if (cmd === "close") break
}
