/** Fetch/XHR work owned by a live frame document. */
export class RequestTracker {
  #requests = new Map()
  #loaders = new Map()
  #retired = new Map()
  #removed = new Set()

  get size() { return this.#requests.size }

  routes() { return [...new Set([...this.#requests.values()].map((request) => request.route))] }

  start({ requestId, frameId, loaderId }, route) {
    if (frameId && this.#removed.has(frameId)) return
    if (frameId && loaderId && this.#retired.get(frameId)?.has(loaderId)) return
    this.#requests.set(requestId, { route, frameId, loaderId })
  }

  finish(requestId) { this.#requests.delete(requestId) }

  seed({ frame, childFrames = [] }) {
    if (frame.id && frame.loaderId && !this.#removed.has(frame.id) && !this.#loaders.has(frame.id)) {
      this.#loaders.set(frame.id, frame.loaderId)
    }
    for (const child of childFrames) this.seed(child)
  }

  navigate({ id, loaderId }) {
    if (!id || !loaderId) return
    this.#removed.delete(id)
    const previous = this.#loaders.get(id)
    if (previous && previous !== loaderId) this.#retire(id, previous)
    this.#loaders.set(id, loaderId)
    for (const [requestId, request] of this.#requests) {
      if (request.frameId === id && request.loaderId && request.loaderId !== loaderId) {
        this.#retire(id, request.loaderId)
        this.#requests.delete(requestId)
      }
    }
  }

  detach(frameId, reason) {
    if (!frameId || (reason !== "remove" && reason !== "swap")) return
    const loader = this.#loaders.get(frameId)
    if (loader) this.#retire(frameId, loader)
    for (const [requestId, request] of this.#requests) {
      if (request.frameId !== frameId) continue
      // A process swap can overlap requests from the incoming document.
      if (reason === "swap" && (!loader || request.loaderId !== loader)) continue
      if (request.loaderId) this.#retire(frameId, request.loaderId)
      this.#requests.delete(requestId)
    }
    this.#loaders.delete(frameId)
    if (reason !== "swap") {
      this.#retired.delete(frameId)
      this.#removed.add(frameId)
    }
  }

  #retire(frameId, loaderId) {
    let loaders = this.#retired.get(frameId)
    if (!loaders) this.#retired.set(frameId, loaders = new Set())
    loaders.add(loaderId)
  }
}
