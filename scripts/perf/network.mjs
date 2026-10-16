/**
 * Request and byte accounting shared by the switch and startup profilers.
 *
 * A latency figure without the bytes behind it cannot tell "the server was
 * slow" from "the page asked for a megabyte". Every request is recorded with
 * the bytes that crossed the wire (`encoded`, headers and compression
 * included) and the bytes the page had to parse (`decoded`), and grouped by
 * endpoint pattern so ids do not split one route into a hundred rows.
 */

import { round } from "./metrics.mjs"
import { captureError } from "./cdp.mjs"

// Path segments that identify an entity rather than a route.
const ID_SEGMENT = [
  /^(ses|msg|prt|prj|per|que|tool|call|evt|wrk|usr)_[A-Za-z0-9]+$/,
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i,
  /^[0-9a-f]{16,}$/i,
  /^\d+$/,
  /^path_[A-Za-z0-9_-]+$/,
]

/**
 * Collapses a request URL into the route it hits. Same-origin requests keep
 * only their path; hashed build assets collapse to their extension, because
 * the hash changes with every build and two builds must still compare.
 */
export const endpointPattern = (url, baseUrl) => {
  let parsed
  try {
    parsed = new URL(url)
  } catch {
    return url
  }
  if (parsed.protocol === "data:" || parsed.protocol === "blob:") return `${parsed.protocol}…`
  let base = null
  try { base = new URL(baseUrl).origin } catch { base = null }
  const prefix = parsed.origin === base ? "" : parsed.origin
  if (parsed.pathname.startsWith("/assets/")) {
    const extension = parsed.pathname.split(".").pop()
    return `${prefix}/assets/*.${extension}`
  }
  const path = parsed.pathname.split("/").map((segment) => (ID_SEGMENT.some((pattern) => pattern.test(segment)) ? ":id" : segment)).join("/")
  return `${prefix}${path}`
}

/**
 * Subscribes to the CDP Network domain (which the caller must enable) and
 * keeps one record per request. WebSocket frames are counted separately: the
 * realtime channel carries events, not requests, but its bytes still arrive.
 * Frame payloads are kept only on request: a long stream carries thousands.
 */
export const createNetworkRecorder = (client, { keepSocketPayloads = false } = {}) => {
  const requests = new Map()
  const socketFrames = []
  const record = (requestId) => requests.get(requestId)
  client.on("Network.requestWillBeSent", (params) => {
    // A redirect reuses the request id; the record follows the final URL.
    const previous = requests.get(params.requestId)
    requests.set(params.requestId, {
      url: params.request.url,
      method: params.request.method,
      type: params.type ?? previous?.type ?? null,
      wallTime: params.wallTime * 1000,
      timestamp: params.timestamp,
      status: null,
      fromCache: false,
      fromServiceWorker: false,
      streamedEncoded: 0,
      decoded: 0,
      encoded: null,
      finishedAt: null,
      failed: false,
    })
  })
  client.on("Network.requestServedFromCache", (params) => {
    const entry = record(params.requestId)
    if (entry) entry.fromCache = true
  })
  client.on("Network.responseReceived", (params) => {
    const entry = record(params.requestId)
    if (!entry) return
    entry.status = params.response.status
    entry.type = params.type ?? entry.type
    if (params.response.fromDiskCache) entry.fromCache = true
    if (params.response.fromServiceWorker) entry.fromServiceWorker = true
  })
  client.on("Network.dataReceived", (params) => {
    const entry = record(params.requestId)
    if (!entry) return
    entry.decoded += Number(params.dataLength ?? 0)
    entry.streamedEncoded += Number(params.encodedDataLength ?? 0)
  })
  client.on("Network.loadingFinished", (params) => {
    const entry = record(params.requestId)
    if (!entry) return
    entry.encoded = Number(params.encodedDataLength ?? 0)
    entry.finishedAt = params.timestamp
  })
  client.on("Network.loadingFailed", (params) => {
    const entry = record(params.requestId)
    if (!entry) return
    entry.failed = true
    entry.finishedAt = params.timestamp
  })
  client.on("Network.webSocketFrameReceived", (params) => {
    const payload = String(params.response?.payloadData ?? "")
    socketFrames.push({ timestamp: params.timestamp, bytes: payload.length, ...(keepSocketPayloads ? { payload } : {}) })
  })

  return {
    requests,
    socketFrames,
    /** Requests that started between two epoch milliseconds, inclusive. */
    startedBetween: (fromMs, toMs) => [...requests.values()].filter((entry) => entry.wallTime >= fromMs && entry.wallTime <= toMs),
  }
}

/**
 * Totals and per-endpoint groups for a set of recorded requests. `encoded`
 * falls back to the streamed byte count for a request that has not finished
 * (an event stream, or one still in flight), which is reported as unfinished.
 */
export const summarizeRequests = (entries, baseUrl) => {
  const groups = new Map()
  let encodedBytes = 0
  let decodedBytes = 0
  let unfinished = 0
  let cached = 0
  let failed = 0
  for (const entry of entries) {
    const encoded = entry.encoded ?? entry.streamedEncoded
    encodedBytes += encoded
    decodedBytes += entry.decoded
    if (entry.finishedAt === null) unfinished += 1
    if (entry.failed) failed += 1
    if (entry.fromCache || entry.fromServiceWorker) cached += 1
    const pattern = `${entry.method} ${endpointPattern(entry.url, baseUrl)}`
    const group = groups.get(pattern) ?? { endpoint: pattern, count: 0, encodedBytes: 0, decodedBytes: 0 }
    group.count += 1
    group.encodedBytes += encoded
    group.decodedBytes += entry.decoded
    groups.set(pattern, group)
  }
  return {
    count: entries.length,
    encodedBytes,
    decodedBytes,
    encodedKb: round(encodedBytes / 1024, 1),
    decodedKb: round(decodedBytes / 1024, 1),
    unfinished,
    failed,
    cached,
    byEndpoint: [...groups.values()].sort((left, right) => right.decodedBytes - left.decodedBytes || right.count - left.count),
  }
}

/** Bounded main-document evidence without URLs, headers or payloads. */
export const createNavigationRecorder = (client, expectedUrl) => {
  const expected = new URL(expectedUrl)
  const documents = new Map()
  const frames = []
  const lifecycle = []
  let mainFrameId = null
  let droppedDocuments = 0
  const location = (url) => {
    try {
      const parsed = new URL(url)
      return { protocol: parsed.protocol, sameOrigin: parsed.origin === expected.origin, expectedPath: parsed.pathname === expected.pathname }
    } catch { return { invalid: true } }
  }
  const append = (list, value) => {
    if (list.length === 24) list.shift()
    list.push(value)
  }
  const subscriptions = [
    client.on("Page.frameNavigated", ({ frame }) => {
      if (!frame.parentId) mainFrameId = frame.id
      append(frames, { frameId: frame.id, loaderId: frame.loaderId, main: !frame.parentId, location: location(frame.url) })
    }),
    client.on("Page.lifecycleEvent", (event) => {
      if (event.name !== "load" && event.name !== "DOMContentLoaded" && event.name !== "init") return
      append(lifecycle, { name: event.name, frameId: event.frameId, loaderId: event.loaderId, timestamp: event.timestamp })
    }),
    client.on("Network.requestWillBeSent", (event) => {
      if (event.type !== "Document") return
      if (!documents.has(event.requestId) && documents.size === 24) {
        documents.delete(documents.keys().next().value)
        droppedDocuments += 1
      }
      documents.set(event.requestId, {
        requestId: event.requestId, frameId: event.frameId, loaderId: event.loaderId,
        location: location(event.request.url), status: null, failed: false, terminalEvents: [],
        redirected: Boolean(event.redirectResponse),
      })
    }),
    client.on("Network.responseReceived", (event) => {
      const entry = documents.get(event.requestId)
      if (entry) entry.status = event.response.status
    }),
    client.on("Network.loadingFinished", (event) => {
      const entry = documents.get(event.requestId)
      if (entry) append(entry.terminalEvents, { type: "finished", encodedBytes: event.encodedDataLength, timestamp: event.timestamp })
    }),
    client.on("Network.loadingFailed", (event) => {
      const entry = documents.get(event.requestId)
      if (entry) {
        entry.failed = true
        append(entry.terminalEvents, {
          type: "failed", cancelled: Boolean(event.canceled), error: captureError(event.errorText), timestamp: event.timestamp,
        })
      }
    }),
  ]
  return {
    read: () => ({ mainFrameId, droppedDocuments, documents: [...documents.values()].map((entry) => ({ ...entry, main: mainFrameId === null ? null : entry.frameId === mainFrameId })), frames: [...frames], lifecycle: [...lifecycle] }),
    stop: () => subscriptions.forEach((unsubscribe) => unsubscribe()),
  }
}
