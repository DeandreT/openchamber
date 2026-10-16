#!/usr/bin/env node
/**
 * JS heap and DOM size after hovering, then opening, N sidebar sessions.
 *
 * Memory regressions in the sidebar and session cache show up as what the page
 * retains once the user has moved through several sessions, not in any single
 * switch. Three readings, each after two forced garbage collections and a
 * settle: the app loaded, after resting the pointer on each of the first N
 * rows (the hover-prefetch path), and after clicking through the same rows.
 *
 * The rows are the first N in sidebar order, so two builds visit the same
 * sessions only when they serve the same server state.
 */

import { mkdirSync } from "node:fs"
import { rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join, resolve } from "node:path"
import process from "node:process"
import { fileURLToPath } from "node:url"

import { CdpClient, captureError, capturePageState, createPageTarget, evaluateValue, launchChrome, navigateForCapture, reservePort, resolveChrome, stopChrome, wait } from "./perf/cdp.mjs"
import { metricMap, round } from "./perf/metrics.mjs"
import { createNavigationRecorder } from "./perf/network.mjs"
import { expandProjects, expandSessionLists } from "./perf/scenario.mjs"

const HELP = `Usage: bun run profile:heap -- [options]

Reads the JS heap, DOM nodes and listeners after GC: loaded, after hovering
the first N sidebar session rows, and after opening each of them.

Options:
  --url <url>              OpenChamber URL (default: http://localhost:3000)
  --count <n>              Sidebar rows to visit (default: 20)
  --hover <ms>             Pointer rest per row (default: 600)
  --dwell <ms>             Time on each opened session (default: 2000)
  --settle <seconds>       Wait after load (default: 12)
  --output <directory>     Writes heap-summary.json (default: artifacts/heap-profile-<time>)
  --label <text>           Human label stored in the summary
  --chrome <path>          Chrome/Chromium executable
  --profile-dir <path>     Chrome profile; emptied before and removed after the
                           run (default: a fresh one under the OS temp directory)
  --headed                 Show the browser (default: headless)
  --help                   Show this help

Needs a running OpenChamber server; see scripts/perf/DOCUMENTATION.md.`

const parseArgs = (argv) => {
  const options = { url: "http://localhost:3000", count: 20, hover: 600, dwell: 2000, settle: 12, output: null, label: null, chrome: null, profileDir: null, headless: true }
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index]
    if (value === "--help") { console.log(HELP); process.exit(0) }
    else if (value === "--url") options.url = argv[++index]
    else if (value === "--count") options.count = Number(argv[++index])
    else if (value === "--hover") options.hover = Number(argv[++index])
    else if (value === "--dwell") options.dwell = Number(argv[++index])
    else if (value === "--settle") options.settle = Number(argv[++index])
    else if (value === "--output") options.output = argv[++index]
    else if (value === "--label") options.label = argv[++index]
    else if (value === "--chrome") options.chrome = argv[++index]
    else if (value === "--profile-dir") options.profileDir = resolve(argv[++index])
    else if (value === "--headed") options.headless = false
    else throw new Error(`Unknown option: ${value}`)
  }
  if (!Number.isInteger(options.count) || options.count < 1) throw new Error("--count must be a positive integer")
  return options
}

// A failed scenario still owns a result. Cleanup failures are separate from the
// original capture error, and every cleanup stage gets its chance to run.
export const recordHeapAttempt = async ({ summary, run, onFailure, cleanup, persist }) => {
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

const main = async () => {
  const options = parseArgs(process.argv.slice(2))
  const output = resolve(options.output ?? join("artifacts", `heap-profile-${new Date().toISOString().replace(/[:.]/g, "-")}`))
  const profileDir = options.profileDir ?? join(tmpdir(), `openchamber-perf-heap-${process.pid}`)
  mkdirSync(output, { recursive: true })
  const summary = { recordedAt: new Date().toISOString(), label: options.label, count: options.count, rows: 0, hovered: 0, opened: 0, phase: "setup", navigation: [], readings: {} }
  let client
  let chrome
  let navigationRecorder
  const run = async () => {
    // Retained documents in a reused profile inflate the heap of later runs.
    await rm(profileDir, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 })
    mkdirSync(profileDir, { recursive: true })
    const port = await reservePort()
    chrome = launchChrome({ chrome: resolveChrome(options.chrome), profileDir, port, headless: options.headless })
    const target = await createPageTarget(port)
    client = new CdpClient(target.webSocketDebuggerUrl)
    await client.connect()
    await Promise.all([client.send("Page.enable"), client.send("Runtime.enable"), client.send("HeapProfiler.enable"), client.send("Performance.enable"), client.send("Network.enable")])
    navigationRecorder = createNavigationRecorder(client, options.url)
    await client.send("Page.setLifecycleEventsEnabled", { enabled: true })
    await client.send("Emulation.setDeviceMetricsOverride", { width: 1600, height: 1000, deviceScaleFactor: 1, mobile: false })
    summary.phase = "initial-navigation"
    const initialNavigation = { phase: summary.phase }
    summary.navigation.push(initialNavigation)
    await navigateForCapture(client, "Page.navigate", { url: options.url }, initialNavigation)
    summary.phase = "expand-projects"
    await expandProjects(client)
    summary.phase = "reload"
    const reload = { phase: summary.phase }
    summary.navigation.push(reload)
    await navigateForCapture(client, "Page.reload", {}, reload)
    summary.phase = "settle"
    console.log(`Loaded ${options.url}; settling for ${options.settle}s.`)
    await wait(options.settle * 1000)
    await expandSessionLists(client)
    await wait(2000)

    const reading = async (name) => {
      summary.phase = `reading-${name}`
      await client.send("HeapProfiler.collectGarbage")
      await wait(2000)
      await client.send("HeapProfiler.collectGarbage")
      const usage = await client.send("Runtime.getHeapUsage")
      const metrics = metricMap((await client.send("Performance.getMetrics")).metrics)
      summary.readings[name] = {
        usedMb: round(usage.usedSize / 1048576, 1),
        totalMb: round(usage.totalSize / 1048576, 1),
        domNodes: metrics.Nodes,
        listeners: metrics.JSEventListeners,
      }
      const entry = summary.readings[name]
      console.log(`${name.padEnd(12)} heap ${entry.usedMb} MB  DOM nodes ${entry.domNodes}  listeners ${entry.listeners}`)
    }
    const rows = (await evaluateValue(client, `[...document.querySelectorAll('[data-session-row]')].map((el) => el.getAttribute('data-session-row'))`)) ?? []
    const plan = rows.slice(0, options.count)
    summary.rows = plan.length
    if (plan.length === 0) throw new Error("The sidebar rendered no session rows; the scenario never ran.")
    if (plan.length < options.count) console.warn(`WARNING: only ${plan.length} sidebar rows; asked for ${options.count}. Compare only runs with the same row count.`)
    const pointAt = async (id) => {
      const box = await evaluateValue(client, `(() => {
        const el = document.querySelector('[data-session-row="${id}"]')
        if (!el) return null
        el.scrollIntoView({ block: "center" })
        const rect = el.getBoundingClientRect()
        return { x: rect.x + 60, y: rect.y + rect.height / 2 }
      })()`)
      if (box) await client.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: box.x, y: box.y })
      return box
    }

    await reading("loaded")
    summary.phase = "hover"
    for (const id of plan) {
      if (await pointAt(id)) summary.hovered += 1
      await wait(options.hover)
    }
    await client.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: 1200, y: 500 })
    await reading("afterHover")
    summary.phase = "open-sessions"
    for (const id of plan) {
      const box = await pointAt(id)
      if (!box) continue
      await wait(300)
      await client.send("Input.dispatchMouseEvent", { type: "mousePressed", x: box.x, y: box.y, button: "left", clickCount: 1 })
      await client.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: box.x, y: box.y, button: "left", clickCount: 1 })
      summary.opened += 1
      await wait(options.dwell)
    }
    if (summary.opened < plan.length) console.warn(`WARNING: ${plan.length - summary.opened} rows disappeared before they could be opened.`)
    await reading("afterClicks")
    summary.phase = "complete"
  }
  await recordHeapAttempt({
    summary,
    run,
    onFailure: async () => {
      if (client) summary.pageState = await capturePageState(client, options.url)
    },
    cleanup: [
      () => {
        if (navigationRecorder) summary.documents = navigationRecorder.read()
        navigationRecorder?.stop()
      },
      () => stopChrome(chrome, client),
      () => client?.close(),
      async () => {
        await wait(500)
        await rm(profileDir, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 })
      },
    ],
    persist: (result) => writeFile(join(output, "heap-summary.json"), JSON.stringify(result, null, 2)),
  })
  console.log(`Saved to ${output}`)
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main().catch((error) => {
  console.error(`Heap profiling failed: ${error.message}`)
  process.exitCode = 1
})
