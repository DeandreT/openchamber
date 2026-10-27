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
 * Commands: launch, wait, rows, inflight, rects, targets, textBoxes, paintRegions, observe, fonts,
 * click, key, type, text, screenshot, close.
 */
import { spawn } from "node:child_process"
import { writeFileSync } from "node:fs"
import { createInterface } from "node:readline"

import { CdpClient, reservePort, wait } from "../perf/cdp.mjs"
import { isComposerSheen } from "./composer_sheen.mjs"

let child = null
let client = null
// Fetch and XHR requests the page started and that have not ended. Streams
// that stay open by design (EventSource) are not counted.
const inflight = new Map()
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

const rectOf = (selector, button) => `(() => {
  const element = ${button === undefined
    ? `document.querySelector(${JSON.stringify(selector)})`
    : `[...document.querySelectorAll(${JSON.stringify(selector ?? "button")})].find(element => element.innerText.trim() === ${JSON.stringify(button)})`}
  if (!element) return null
  const rect = element.getBoundingClientRect()
  return [rect.x, rect.y, rect.width, rect.height]
})()`

const handlers = {
  async launch({ app, env, scale, width, height, timeoutMs = 120_000 }) {
    const port = await reservePort()
    // The basic password store keeps Chromium off the desktop's secret
    // service, which would ask the user to unlock their keyring.
    const args = [`--remote-debugging-port=${port}`, `--force-device-scale-factor=${scale}`, "--password-store=basic", "--ozone-platform=x11", "--disable-features=MacAppCodeSignClone"]
    child = spawn(app, args, { env, stdio: "ignore", detached: true })
    child.unref()
    const page = await pageTarget(port, Date.now() + timeoutMs)
    client = new CdpClient(page.webSocketDebuggerUrl)
    await client.connect()
    await client.send("Runtime.enable")
    client.on("Network.requestWillBeSent", ({ requestId, type, request }) => {
      if (TRACKED_REQUESTS.has(type)) {
        const url = new URL(request.url)
        const route = url.protocol === "http:" || url.protocol === "https:"
          ? url.pathname.split("/").slice(0, 3).join("/")
          : url.protocol
        inflight.set(requestId, route)
      }
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
    return { count: inflight.size, routes: [...new Set(inflight.values())] }
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
      rects[name] = await evaluate(rectOf(spec.selector, spec.button))
    }
    return { rects }
  },
  /**
   * The boxes of the visible text on screen: every line box of every
   * rendered text node, and every text field (placeholders are not text
   * nodes). The glyph-edge exception applies only inside them.
   */
  async textBoxes() {
    return { boxes: await evaluate(`(() => {
      const boxes = []
      const inView = (rect) => rect.width > 0 && rect.height > 0 && rect.right > 0 && rect.bottom > 0
        && rect.left < innerWidth && rect.top < innerHeight
      const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT)
      const range = document.createRange()
      for (let node = walker.nextNode(); node; node = walker.nextNode()) {
        if (!node.data.trim()) continue
        const parent = node.parentElement
        if (!parent?.checkVisibility({ opacityProperty: true, visibilityProperty: true })) continue
        range.selectNodeContents(node)
        for (const rect of range.getClientRects()) {
          if (inView(rect)) boxes.push([rect.x, rect.y, rect.width, rect.height])
        }
      }
      for (const field of document.querySelectorAll('input, textarea')) {
        if (!field.checkVisibility({ opacityProperty: true, visibilityProperty: true })) continue
        const rect = field.getBoundingClientRect()
        if (inView(rect)) boxes.push([rect.x, rect.y, rect.width, rect.height])
      }
      return boxes
    })()`) }
  },
  /**
   * Clicks the centre of `selector`. With `within`, the element under that
   * point must be inside `within` before the press: a list that is still
   * settling can move another row under a centre read a moment earlier.
   */
  async click({ selector, button, within, timeoutMs = 5_000 }) {
    const deadline = Date.now() + timeoutMs
    let x
    let y
    for (;;) {
      const rect = await evaluate(rectOf(selector, button))
      if (!rect) throw new Error(`no element for ${button ?? selector}`)
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
  /** Bounds for the approved shadow/gradient noise exception, in CSS pixels. */
  async paintRegions() {
    return await evaluate(`(() => {
      const isComposerSheen = ${isComposerSheen.toString()}
      const box = (rect) => [rect.x, rect.y, rect.width, rect.height]
      const visible = (element) => element.checkVisibility({ opacityProperty: true, visibilityProperty: true })
        && element.getBoundingClientRect().width > 0 && element.getBoundingClientRect().height > 0
      const protectedBoxes = [...document.querySelectorAll('button, input, textarea, select, a, svg, img, video, canvas, [contenteditable="true"]')]
        .filter(visible).map((element) => box(element.getBoundingClientRect()))
      const regions = []
      for (const element of document.querySelectorAll('.oc-composer-sheen, aside[aria-label="Work status"]')) {
        if (!visible(element)) continue
        const rect = element.getBoundingClientRect()
        protectedBoxes.push(box(rect))
        const value = getComputedStyle(element).boxShadow
        let start = 0, depth = 0
        const shadows = []
        for (let i = 0; i < value.length; i++) {
          if (value[i] === '(') depth++
          if (value[i] === ')') depth--
          if (value[i] === ',' && depth === 0) { shadows.push(value.slice(start, i)); start = i + 1 }
        }
        shadows.push(value.slice(start))
        for (const shadow of shadows) {
          if (shadow.includes('inset') || shadow.trim() === 'none') continue
          const lengths = [...shadow.matchAll(/(-?[0-9]*\\.?[0-9]+)px/g)].map((match) => Number(match[1]))
          if (lengths.length < 2) continue
          const [x, y, blur = 0, spread = 0] = lengths
          const radius = Math.max(0, 1.5 * blur + spread)
          regions.push({ kind: 'shadow', box: [rect.x + x - radius, rect.y + y - radius, rect.width + 2 * radius, rect.height + 2 * radius] })
        }
      }
      for (const element of document.querySelectorAll('.chat-scroll-fade')) {
        if (visible(element)) regions.push({ kind: 'gradient', box: box(element.getBoundingClientRect()) })
      }
      const composerGradientRegions = []
      const composerGradientDefinitions = []
      const composerGradientProtected = [...document.querySelectorAll('button, input, textarea, select, a, svg, img, video, canvas, [role="button"], [role="slider"], [role="switch"], [role="menuitem"], .cm-cursor, .cm-selectionBackground, .cm-activeLine')]
        .filter(visible).map((element) => box(element.getBoundingClientRect()))
      const selection = getSelection()
      for (let index = 0; selection && index < selection.rangeCount; index++) {
        for (const rect of selection.getRangeAt(index).getClientRects()) composerGradientProtected.push(box(rect))
      }
      for (const element of document.querySelectorAll('.dark .oc-composer-sheen')) {
        if (!visible(element)) continue
        const style = getComputedStyle(element)
        composerGradientDefinitions.push(style.backgroundImage)
        // This exception is specific to the current vertical 50% sheen.
        if (!isComposerSheen(style.backgroundImage)) continue
        const rect = element.getBoundingClientRect()
        const radii = ['borderTopLeftRadius', 'borderTopRightRadius', 'borderBottomLeftRadius', 'borderBottomRightRadius'].map((property) => style[property])
        if (radii.some((radius) => !/^[0-9.]+px$/.test(radius))) continue
        composerGradientRegions.push([rect.x, rect.y, rect.width, rect.height / 2])
        const edge = 2
        composerGradientProtected.push(
          [rect.x, rect.y, rect.width, edge],
          [rect.x, rect.bottom - edge, rect.width, edge],
          [rect.x, rect.y, edge, rect.height],
          [rect.right - edge, rect.y, edge, rect.height],
        )
        for (const [property, right, bottom] of [
          ['borderTopLeftRadius', false, false], ['borderTopRightRadius', true, false],
          ['borderBottomLeftRadius', false, true], ['borderBottomRightRadius', true, true],
        ]) {
          const radius = Math.max(edge, parseFloat(style[property]) + edge)
          composerGradientProtected.push([right ? rect.right - radius : rect.x, bottom ? rect.bottom - radius : rect.y, radius, radius])
        }
      }
      return { regions, protectedBoxes, composerGradientRegions, composerGradientProtected, composerGradientDefinitions, composerGradientProtectionComplete: true }
    })()`)
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
