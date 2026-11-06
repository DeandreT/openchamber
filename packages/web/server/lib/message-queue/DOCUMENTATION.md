# Message Queue

## Purpose

Owns the messages a user queued while a session was busy, and sends them the
moment the session goes idle. The queue lives in the web server so a closed
tab, a locked phone, or a dropped connection no longer strands it. Structural
template: `permission-auto-accept` — the server is authoritative, the shared UI
renders a projection, and VS Code (which has no server of its own) keeps its
UI-side queue and foreground auto-send hook.

## Files

- `runtime.js` — `createMessageQueueRuntime(...)` (state, persistence,
  dispatch loop, event handling) and `registerMessageQueueRoutes(app, runtime)`.
- `runtime.test.js` — delivery, idleness gates, retries, held delivery, persistence,
  concurrency with in-flight sends, slash commands, and project knowledge.

Wiring: created in `server/index.js` after the global event hub and the
session-knowledge runtime; routes registered in
`opencode/feature-routes-runtime.js` (before the generic OpenCode proxy) with
JSON bodies enabled in `opencode/core-routes.js`; stopped by
`opencode/shutdown-runtime.js`.

## Auto routing

`resolveAutoSelection` (the routing runtime) runs right before the send when
the queued send config names `openchamber/auto`, and answers with the real
model and agent. OpenCode 2.x holds both on the session, so the queue applies
them with the `POST /api/session/:id/model` and `/agent` calls it already
makes. The queue captures the sentinel like any other send config. The request
text it routes on carries the same image attachment fact as a direct send
(`attachmentHint`).

## Item

An item is what the UI would have sent itself, captured at queue time so the
send never re-resolves mutable UI state:

```
{
  id, createdAt,
  content,        // raw text for display and editing
  contextPreview?, // bounded display-only summary captured by the UI
  text,           // text to deliver (agent mention stripped, file mentions resolved); defaults to content
  agentMention?,  // delivered as an `agent` part
  attachments: [{ id, filename, mimeType, size, source, serverPath?, agentPath?, dataUrl }],
  context: [      // what the composer had attached, in send order
    { kind: 'context', text, metadata, instructions? },  // a draft chip or linked issue/PR; metadata is the UI's structured payload
    { kind: 'instruction', text },                       // derived from the text (skill instruction)
    { kind: 'synthetic', text },                         // handed to the composer by another surface
  ],
  sendConfig: { providerID, modelID, agent?, variant? },  // required
  goal?: { objective?, tokenBudget }  // queued with "run as goal" armed
}
```

`goal` is the composer's armed goal, taken off the composer when the message
is queued so it belongs to this message and no other. `objective` is present
only when the arming flow supplied its own (the plan flow); otherwise the
delivered `text` is the objective. A goal needs message text. Snapshots keep
`goal`, so the queue row can show it.

The server is a courier for `context`: it validates the shape (a kind it
knows, a `metadata` object on `context` entries) and delivers each entry as a
synthetic text part, an entry's `instructions` going out as its own part just
before it and its `metadata` riding the part verbatim so the timeline renders
the context block back. The payload inside `metadata` is the UI's contract
(`lib/messages/contextParts.ts`), parsed by the UI on the way back.

An attachment's `agentPath` — the file's on-disk location the desktop client
resolved at intake — travels to OpenCode as the prompt file's `name`, the
field the model reads as the file identity, falling back to `filename` when
absent. `filename` itself still rides the attachment unchanged for the UI's
own display.

`parseQueuedItemInput` rejects anything the server could not deliver later
(no text, attachments, or context; missing model; malformed attachment or
context entry). Public snapshots and broadcasts strip the payloads —
attachment `dataUrl` (megabytes of base64) and `context` (a PR diff, say) —
so they do not ride every update; the only way to get them back is a `take`.

Snapshots retain `contextPreview`, capped at 100 characters plus an ellipsis.
It carries the attached comment or context label when `content` is empty and
never replaces editable text or delivered parts. Older items without a summary
derive one from the attached comment metadata or the first non-instruction
context text. This optional field needs no queue-file migration.

## Scheduled items

Scheduled tasks use the same items with an optional `scheduledTask` field
containing `projectId` and `taskId`. Reuse does not change the target's goal or
auto-accept policy.
Only the server can enqueue this provenance. Enqueue deduplicates the project
and task across all session queues. A scheduled item can omit its model to
keep the target session selection. Normal composer items still require their
captured model. The UI retains both kinds in the same projection.

Scheduled send guards and terminal result callbacks apply only to scheduled
items. A definitive before-send target or task refusal with 404 or 409 removes
that item and lets the next item proceed. This includes removed or retargeted
tasks and moved targets. Other validation errors and all send failures use the
existing queue retry policy, including model-switch 404 or 409 responses.
Successful send reports `sent` without waiting for an assistant reply. Removing,
clearing or evicting a waiting scheduled item reports `cancelled`. Scheduled
items can be reordered and removed, but cannot be taken for editing or manual
send. Bulk take returns only ordinary items and leaves scheduled items queued.
Disable, delete and retarget use `cancelScheduledTask`. Existing composer
persistence, retries, directory query and delivery gates remain unchanged.

## Queue file

`<data-dir>/message-queue.json` (`OPENCHAMBER_DATA_DIR` or
`~/.config/openchamber`): `{ version, revision, sessions: { [sessionId]:
{ directory, items } } }`, written atomically (temp file + rename) through a
serialized write chain. A missing file is an empty queue. A malformed file is
a failure, not an empty queue: it is moved aside as
`message-queue.json.corrupt-<timestamp>` before the runtime starts empty, so
the next write cannot overwrite the user's data. A failed read leaves writes
disabled until a later load succeeds. `revision` is a global monotonic counter
bumped on every mutation; clients use it to reject stale snapshots.

The legacy in-flight item (`sendingId`), retry backoff, and abort timestamps
stay in memory. Opt-in request receipts persist a separate delivery state
whose `uncertain` value requires explicit retry after restart.

## Delivery loop

1. `start()` subscribes to the global upstream hub and loads the file; on
   load and on every hub `connect` it arms every session that has items.
2. `session.status` for a queued session: `idle` arms a short quiet timer
   (500 ms, coalescing the burst around a turn boundary), `busy`/`retry`
   clears it. A `message.updated` for a completed assistant reply arms as
   well, so a missed idle event cannot strand the queue. `session.deleted`
   drops the session's queue. An assistant `MessageAbortedError` records an
   abort.
3. `tick(sessionId)` bails when the queue is empty, an item is in flight, or
   the session is held. It re-arms after a 2 s post-abort hold (the UI's
   old behavior: a stop is not immediately followed by the next prompt) or
   while the head item is in retry backoff.
4. Idleness is re-verified against OpenCode before sending, because
   a prompt into a running turn steers into it instead of starting the
   next one: `GET /api/session/active` must not list the session, and the
   trailing message must not be an unfinished assistant reply (that route
   lists only running sessions, so a missed busy event leaves no entry while
   a turn still streams). An unfinished reply created before this
   runtime started does not block: its run died with the previous server and
   will never complete, so a restored queue would wait on it forever. A reply
   with no `created` time still blocks. A failed fetch is unknown, never idle:
   the tick re-arms with backoff.
   The turn must also be over for the session's subagents: a parent idles
   while a background subagent works and runs again when OpenCode hands the
   result back. While `../opencode/session-activity.js` finds a running descendant
   the head waits (the rerun's idle event re-arms it, a 5 s recheck covers a
   missed one); a failed check is unknown and backs off like the rest.
   The descendant check follows live active sessions through at most 8 parent
   links, including idle intermediate subagents; a deeper chain counts as
   unrelated, as in the UI. Unreadable lineage, cycles and the shared fetch
   deadline keep the queue held with the existing retry policy. It does not scan historical child pages.
5. The head is marked in flight (broadcast), then sent. The captured model and
   agent are switched onto the session first (`POST /session/:id/model`,
   `/agent`), because v2 holds both on the session rather than in the body; a
   captured `openchamber/auto` is resolved into a real pair by the routing
   runtime beforehand. The session's project knowledge is written into its
   instruction entries (`sessionKnowledgeRuntime.sync`, which never fails the
   send). Then the captured context goes ahead as synthetic messages
   (`POST /session/:id/synthetic` with `resume: false`, an attached item's
   metadata riding along). An item with a `goal` starts it the way a
   scheduled goal does: `createSessionGoal` (`../session-goal/create.js`)
   saves the goal through the injected `persistSessionGoal` before anything is
   admitted, so a failed save leaves nothing for the retry to duplicate, and
   the goal briefing goes as the last synthetic message. Then the message itself:
   - text starting with `/` that names a command in OpenCode's `/command`
     list (skills included) goes to `POST /session/:id/command` (body fields
     `name` and `text` since OpenCode 2.0.8) with its file attachments. The
     command route takes files only, which is why the context went ahead of
     it; sending "/name args" as a prompt instead would skip the template
     OpenCode expands only on that route. The command lookup runs before
     anything is admitted, so a failed lookup fails the send without leaving
     context behind for the retry to duplicate;
   - otherwise `POST /session/:id/prompt` with the user's text, files and
     agent mention.
   Success removes the item, persists, broadcasts, and calls `onPromptSent`.
   Failure keeps the item, backs off
   2 s → 60 s (doubling per consecutive failure of that item), and re-arms.
6. The next item goes out after the next busy → idle cycle.

A 2xx HTML app-shell response is a failed dispatch, not acceptance. The queued item stays persisted and uses the existing retry backoff.

## Held delivery

### Native prefix reservations

`POST /api/message-queue/sessions/:id/reserve-prefix` atomically reserves a
leading run of item IDs for one native prompt. The body carries `id`, the
stable `msg_` prompt identity, `itemIds`, captured `composer`, `sendConfig`
and optional `delivery: "steer"`. A stale prefix, an active delivery, or an
item carrying a goal, scheduled provenance or payload beyond native journal
limits returns 409 without taking anything. The operation never skips items.
`preview-prefix` accepts the same body and returns the candidate without changing
ownership. Native journals that complete prompt before acquiring the reservation.

The original items remain in the queue. The same queue file retains the
reservation beside them, and the response waits for its atomic write. Its
`reservation` and full `items` form an immutable receipt. While a reservation
exists, repeating the exact request returns that receipt. A different identity
or payload returns 409. The caller retains the original request through a lost
answer or restart, then
uses the receipt's prompt ID and derives context IDs from it. A failed write
returns failure and retains the in-memory exclusion until a retry persists it.

The owner excludes automatic delivery and destructive queue mutations while
the prefix is reserved. Dispatch probes recheck the reservation after their
awaits, so a probe started before reservation cannot deliver past it. Appends
remain available within the existing queue limit; reserved sessions and items
cannot be evicted to make room. Existing routes retain their behavior on
unreserved sessions and return 409 on reserved sessions.

The owner observes the exact prompt ID through OpenCode's inbox and then
exact-ID history. Only a matching user admission releases the prefix and
removes its original items. Missing, malformed and failed observations leave
the reservation intact. Checks use the existing bounded retry backoff and
session events; no timeout expires the exclusion. A native client can recover
a lost receipt by retrying its journaled send under the same IDs.
This also preserves ordering when the client closes between taking ownership
and sending. Session deletion clears the reservation with the queue. Do not
downgrade the server while a reservation is pending, since older servers do
not enforce this persisted ownership field.

### Auto-review holds

An auto-review run (`../auto-review`) bounces the reviewed session through
idle between its steps, and a queued follow-up must not land in the middle.
The runtime takes `isDeliveryHeld(sessionId)`, the auto-review runtime's
`holdsDelivery`, and the tick delivers nothing while it is true. The run calls
`release(sessionId)` when it ends, which arms a dispatch. `deliveryHoldReady`
(the auto-review's `startupLoad`) gates the first `reconcileAll` after start:
until the auto-review has loaded its runs every session is held, and a tick
that bailed then has nothing to re-arm it.

## Routes (`/api/message-queue`)

Normal authenticated OpenChamber runtime routes; never on browser URL-token
allowlists.

| Route | Purpose |
|---|---|
| `GET /api/message-queue` | Full snapshot `{ revision, sessions[] }` |
| `POST .../sessions/:id/items` | Append `{ directory, item }`; returns `{ revision, session, itemId }` and arms a dispatch (the session may already be idle) |
| `DELETE .../sessions/:id/items/:itemId` | Remove; `409` while that item is in flight |
| `POST .../sessions/:id/items/:itemId/take` | Remove and return the full item (payloads included); `404`/`409` |
| `POST .../sessions/:id/take` | Remove and return every item not in flight, in order |
| `POST .../sessions/:id/preview-prefix` | Read an eligible leading prefix and complete captured send without changing ownership |
| `POST .../sessions/:id/reserve-prefix` | Persist an eligible leading prefix and its captured send under a stable prompt ID; repeat the exact request to recover its receipt |
| `PUT .../sessions/:id/order` | `{ itemIds }` must be a complete permutation |
| `DELETE .../sessions/:id` | Clear; the in-flight item stays |

Every mutation broadcasts `openchamber:message-queue.updated` with
`{ revision, session }` to all connected clients (SSE and WS), so several
devices on one server see one queue. SSE uses the shared control stream at
`/api/openchamber/events`; `/api/global/event` carries no OpenChamber events.
The UI subscribes independently of its OpenCode transport and re-reads the
snapshot whenever either stream reconnects. The session in that payload always names
its `directory`, including the broadcast that removes the last item: the UI
keys its projection by directory, and a broadcast without one left the
delivered message on screen (a session's directory is remembered until the
session is deleted or evicted).

Limits: 20 items per session, 50 sessions (oldest evicted, never one with an
item in flight), 200k characters of content; attachment payloads are bounded
by the route family's 50 MB JSON limit.

## Durable request protocol

### Ownership

The queue owns request receipts, captured prompt and context identities, and
attachment bytes accepted through the opt-in protocol. A native caller owns its
immutable captured draft, request ID and local attachment spool until it receives
an authoritative enqueue receipt. For take, the server owns the full transferred
items until the caller explicitly confirms durable local journal ownership.
These routes use the existing authenticated route family and JSON parser. They
are never accepted with browser URL-token authentication.

Web, Electron, hosted mobile and Capacitor retain the existing queue protocol
unless a caller selects these endpoints. VS Code's foreground queue does not
use this runtime. The native client journal and transport belong to their native
modules and do not run here.

### Request and response shapes

All new endpoints start with
`/api/message-queue/sessions/:sessionId/requests/:requestId`. Request IDs contain
8 to 128 ASCII letters, digits, underscores or hyphens. IDs are unique within
the server data directory across all sessions and both operation kinds. A request
keeps its original exact directory and session even after the queue empties or
the session is deleted.

| Endpoint | Body or query | Result |
|---|---|---|
| `POST .../enqueue` | `{ directory, item }` | Durable enqueue receipt |
| `POST .../take` | `{ directory, itemIds }` | Durable transfer receipt and full ordered `items` |
| `GET ...` | Required query `directory` | Exact receipt lookup, including unacknowledged transfer payloads |
| `POST .../acknowledge` | `{ directory }` | Confirm local journal ownership of a take and release its server payload |
| `POST .../retry` | `{ directory, attempt }` | Explicitly authorize the next attempt of one uncertain enqueue |

POST bodies reject unknown top-level fields and all query parameters. Lookup
rejects extra query fields and non-empty bodies. Every response disables caching.
Normal responses contain `{ receipt, state, revision }`. Queue projections are
read separately through the existing snapshot and update routes. Receipt replies
never include the current queue, so later queue growth cannot exceed a receipt's
recovery bound. Enqueue
responses also contain `attempt`, initially `0`, and an optional `blocked` reason.
An unacknowledged take also contains `items` with attachment `dataUrl` and captured
context. Acknowledged transfers return no `items`.

The immutable enqueue receipt is:

```js
{
  version: 1, kind: 'enqueue', requestId, sessionId, directory,
  requestHash, acceptedAt, itemId, messageId, contextIds,
}
```

The immutable take receipt has `kind: 'take'` and ordered `itemIds` in place of
`itemId`, `messageId` and `contextIds`. `requestHash` is the SHA-256 digest of
the operation, owner and complete canonical JSON request. Object-key order is
irrelevant; omitted fields, explicit nulls, whitespace inside values and all item
fields participate in comparison. Repeat the same operation and body under the
same request ID to recover its receipt. A different body, operation or owner
returns `409` with `code: 'queue_request_conflict'`.

Lookup returns `404` with `code: 'queue_request_unknown'` when no receipt exists.
This means unknown. The client retains its captured draft and spool and may
explicitly repeat the original request under the same ID. A new identity would
be a new operation and cannot resolve the original uncertainty. Lookup of an
already persisted receipt is read-only. When an earlier checkpoint failed,
lookup retries that checkpoint without requiring the original request body.
An active or still-failing checkpoint returns `503` with
`code: 'queue_request_pending'`. A later lookup or exact operation repeat can
recover it.

### Invariants

- The enqueue response and first delivery wait for durable ownership of the
  receipt and full attachment bytes. The original inline opt-in format requires
  inline `dataUrl` bytes, including attachments with `source: 'server'` and
  `serverPath`. Manifest items own private uploaded bytes. Both send real bytes
  to OpenCode through prepared data URIs; neither assumes a server-path read.
- Opt-in enqueue supports ordinary captured prompts. Goal and scheduled-task
  ownership remain with their existing protocols. Each request and complete
  take response is bounded to 4 MiB, independently of the current queue.
  Inline take checks the reconstructed native prompt before moving any items. Combined
  text is at most 1 MiB of UTF-8, files, mentions and expanded context total at
  most 4096 entries, and identifiers, references and metadata obey the native
  journal's existing limits. Oversized input leaves queue ownership unchanged.
  The manifest format below separates these metadata bounds from file bytes.
  `payload-v1` also separates the full structured capture from its compact
  request and receipt, within the original 50 MiB capture budget.
- An accepted request ID is never evicted or reused. Its immutable receipt
  survives dispatch, removal, clear, take and session deletion. Terminal enqueue
  receipts retain no attachment payload, context or draft text. Only pending
  transfers retain full items, until explicit journal-ownership acknowledgement.
- Opt-in requests reject full queues without evicting another item. Legacy
  appends cannot evict an opt-in item or change a queue's directory while it
  contains one. Existing all-legacy limits and eviction behavior are unchanged.
- Enqueue states are `queued`, `uncertain`, `dispatched`, `removed`, `taken` and
  `deleted`. Take states are `transferring` and `acknowledged`. Public queue items
  for this protocol also expose `queueDelivery: { requestId, state, attempt,
  blocked? }`. Legacy items keep their existing shape.
- The metadata-only manifest size preflight runs before uncertainty or any
  OpenCode call. Before delivery preparation or an OpenCode write, an eligible
  item durably records `uncertain`. A failed write sends nothing. Transport, admission, preparation
  and checkpoint failures keep the original identity held. Idle events,
  reconnect and restart never automatically resubmit an uncertain item.
- Retry uses `attempt: previousAttempt + 1`. Repeating an already-authorized
  attempt returns current state and cannot authorize another send. The caller
  must journal the attempt before requesting it. Every attempt uses the same
  captured prompt and context IDs and the first resolved model/agent selection.
- An explicit retry first observes the exact prompt ID in OpenCode inbox and
  exact-ID history. Existing valid user admission completes the queue item
  without resending. Otherwise context IDs receive the same checks before
  posting their original IDs. Malformed or failed observations keep delivery
  held. Valid empty inbox plus exact history `404` permits only the explicitly
  authorized same-ID request; it does not grant permission to mint new IDs.
  Every durable completion, observing an already-admitted head, or removing an uncertain head arms its
  successor and rechecks live idleness, since neither action guarantees another
  busy-to-idle event. If the completion checkpoint fails, a successful catch-path
  or lookup checkpoint recovery arms the successor only after completion is
  durable. An uncertain delivery remains held.
- Actual prompt and synthetic responses must contain the matching ID, session,
  kind, timestamp and queued delivery. OpenCode may prepare their text and
  attachments through plugins, so observations compare admission identity.
- Uncertain items cannot be transferred, taken by legacy routes or absorbed
  into a prefix with a new prompt identity. They retain their original retry
  path. Unrelated queues continue independently.
- Take moves the selected items into the durable transfer receipt before
  returning them. Lost responses and restart replay those full items until
  acknowledgement. Acknowledgement asserts that the caller has durably stored
  the whole payload; a lost acknowledgement response can be repeated safely.
- Context custody is separate from immutable receipt history. The optional
  `contextOwners` root maps each synthetic ID to its exact session, directory and
  ordered claims. A claim names an enqueue request, a Take request with its source
  item ID, or a prefix prompt ID. Only the final claim owns custody.
  A transferring Take retains the enqueue claim. Durable acknowledgement appends
  Take custody and discards its payload in the same snapshot. Replaying an old
  acknowledgement cannot rewind a later claim.
- Re-enqueue and composer-prefix reuse require the current same-owner acknowledged
  Take claim. Selected queued contexts may move directly from enqueue to prefix.
  Active, uncertain, unacknowledged and foreign claims remain excluded, and a
  prompt ID cannot become a context ID. Prefix claims persist after admission and
  session deletion. Failed checkpoints retain exclusion until the exact owning
  operation recovers durably.
- `prefixPromptOwners` retains the exact session, directory and `reserved` or
  `consumed` state for every typed prefix prompt, including prefixes with no
  context items. Reservation and prompt fencing share a checkpoint; admission or
  deletion consumes the fence without removing it. A retained prefix prompt ID
  cannot become a context or a new typed or legacy prefix prompt.
- A snapshot without `contextOwners` derives only unambiguous custody from unique
  enqueue identities, exact acknowledged Takes, retained legacy transfer payloads
  and active typed reservations. An older acknowledged legacy Take with no payload
  cannot establish context cardinality; its deterministic ID namespace remains
  refused while unrelated queues remain usable. Malformed provenance and duplicate
  historical IDs without a valid chain refuse loading without overwriting the root.
  Absent prompt-fence metadata derives only from retained prefix claims and active
  typed reservations; it cannot reconstruct vanished zero-context history. An
  initial transferring legacy Take claim must occur in its exact retained item's
  context index. After that legacy payload is acknowledged and discarded, only its
  exact owner/item-derived namespace is provable.
  Do not downgrade after transfer chains are used: older loaders reject reused
  receipt IDs or cannot enforce completed-prefix exclusion.
- Invalid stored receipts, owner mismatches and missing retained transfer
  payloads reject loading and prevent overwrite. The stored root must be a
  versioned queue object with a valid revision and session map; parseable null,
  arrays and other malformed roots grant no empty-queue authority. Each stored
  session must be an object with a valid session ID, a non-empty directory and
  an item array. A reservation cannot survive without its retained items.
  These checks precede empty-session filtering, including manifest reservations
  converted from legacy items that have no enqueue receipt. Failure preserves
  the original file and denies mutations until explicit repair; unrelated valid
  sessions are retained in that file. A malformed JSON file is kept
  in the existing corrupt-file backup. Legacy recovery may continue, but a
  persisted `receiptRecoveryRequired` marker refuses opt-in requests until the
  retained receipt history is recovered. Corruption cannot create fresh enqueue
  authority under an old ID.

The pinned OpenCode prompt and synthetic endpoints accept an `id` and return
observable inbox admissions. Its command endpoint accepts no caller identity and
returns no admission receipt. Recognized slash commands therefore remain held
with `blocked: 'command_identity_unsupported'` and are never posted through that
endpoint by this protocol. Existing legacy command delivery is unchanged.
Opt-in command discovery uses the captured `location[directory]` query accepted
by OpenCode 2.x. Malformed command catalogs keep delivery held and cannot turn a
command into an ordinary prompt.

### Decisions

#### Why receipts share the queue snapshot

A single snapshot commits item removal and receipt ownership together. It also
contains inline attachment bytes or references to blobs synced before that
snapshot. Separate item and receipt ledgers would leave a crash window between
their ownership transitions. Once the ledger exists, all subsequent snapshots sync the file before
rename and sync the containing directory on platforms supporting directory sync.
Windows uses the file sync and atomic replacement; directory sync is unavailable
through this implementation.

#### Why terminal receipts keep a digest

The digest verifies an immutable repeat after the full item is owned by OpenCode
or a native journal. Keeping full attachments indefinitely would retain
conversation payloads solely for deduplication. Receipts have no automatic expiry
because an old client can still explicitly recover the original request.

#### Why context custody has an ordered claim chain

Receipt history must retain original context IDs after an acknowledged Take gives
them to a composer. An ordered claim chain permits that exact handoff and records
the next consumer atomically. Its retained prefix claim prevents a completed send
from granting those IDs to a second consumer after its reservation disappears.

#### Why delivery uncertainty is written before preparation

A server can stop after OpenCode admits a message and before the response or
checkpoint is saved. Recording uncertainty first makes that restart require the
same explicit retry used for a lost response. The initial attempt and every
authorized retry have stable prompt/context IDs; legacy backoff remains unchanged.

## Manifest attachments and full prompt parts

### Upload and byte ownership

The additive `manifest-v1` format keeps prepared attachments out of JSON
requests, queue snapshots and transfer receipts. The existing 50 MiB JSON parser
is unchanged. Prepared files are individually bounded to 20 MiB, matching the
shared document-image preparation limit. Two 20 MiB images therefore travel as
two binary requests and one small manifest, without base64 expansion at ingress.
All manifests, captured metadata and full transfer/prefix receipts retain the
4 MiB encoded JSON bound. The entry-count bound remains 4096. The byte limit
applies independently to each file.

Upload routes share ordinary queue authentication and are never URL-token
allowlisted. They add no listener, provider, destination or enterprise-mode
exception. Web, Electron, hosted mobile and Capacitor may explicitly use them;
their existing clients are unchanged. VS Code's foreground queue does not use
these routes.

The attachment route starts with
`/api/message-queue/sessions/:sessionId/requests/:requestId/attachments/:attachmentId`.
Its only query field is the required exact `directory`. Attachment IDs contain
1 to 128 ASCII letters, digits, underscores or hyphens.

| Method | Request | Response |
|---|---|---|
| `PUT` | Raw `application/octet-stream`, no content encoding except `identity` | `{ attachment, state: 'available' }` after bytes and root receipt are durable |
| `GET` | No body | At most 20 MiB of original bytes, `application/octet-stream`, attachment disposition, `nosniff`, no-store, `X-OpenChamber-SHA256` |
| `DELETE` | No body; explicit release | `{ attachment, state: 'released' }` once durable ownership checks and unlink complete |

PUT requires these headers:

```text
X-OpenChamber-Filename: <encodeURIComponent(filename)>
X-OpenChamber-Mime-Type: image/png
X-OpenChamber-Size: <exact raw decimal byte count>
X-OpenChamber-SHA256: <lowercase 64-character SHA-256>
```

The filename is display metadata, never a filesystem path. Empty names, path
separators, dot paths and control characters are refused. MIME values must be
media-type tokens. Streamed size, declared size and SHA-256 must agree. A declared
oversize request is refused before storage; chunked overruns stop at the same
20 MiB bound. The earlier JSON middleware passes this binary content type
untouched, so upload needs no larger global body limit.

The returned attachment descriptor is:

```js
{
  blobId, requestId, attachmentId, filename, mimeType, size, sha256,
}
```

`blobId` is derived from the exact session, directory, upload request and
attachment ID. Repeating that upload requires identical metadata and bytes.
The blob envelope also retains this immutable header, so a crash after file
commit but before root-index commit cannot reinterpret the same upload identity.
An accepted enqueue request cannot acquire another upload under that identity.
Each upload request ID retains one exact session and directory, including before
an enqueue is accepted. Concurrent operations on an actively streaming attachment
return `409` with `code: 'queue_request_pending'`; repeating after it finishes
recovers the available descriptor. New enqueue or take captures under that upload
request ID return the same pending error until all its uploads finish. Other
request IDs remain available.

Private envelopes live under the canonical configured data directory in
`message-queue-blobs`. The root must be a real directory; POSIX ownership and
permissions are owner-only. Files are exclusive-created regular files with one link. Opaque
owner-derived names are the only path authority. Reads refuse symlinks, hard
links, changed identities, wrong lengths and incorrect SHA-256 values. The
configured data directory has the same trusted application-owner boundary as
the queue snapshot, not a client-provided workspace root. File and containing
directory sync precede acceptance on platforms supporting directory sync.
Windows relies on the configured data directory's owner-private ACL and uses
file sync and atomic replacement without directory sync.

The queue snapshot's optional `blobs` index commits upload identity and release
state beside queue ownership. Ownership acquisition/release shares a local
exclusion, while root writes retain the existing serial write chain. One queue
runtime owns each configured data directory; root-write serialization is local
to that runtime. Network
streams hold only their attachment identity and cannot stall unrelated durable
queue requests. An upload with an unknown enqueue outcome stays retained. There is no
age-based collection, history deletion or sweep of other temporary files.

Each queue item or pending take owns its referenced blobs independently of the
upload request ID. Several items in the same session and directory may reference
one immutable blob. Release checks every queue item and every unacknowledged
take. A reserved prefix keeps its original items and therefore its byte ownership.
An uncertain item remains a reference until an explicit terminal mutation.
Release persists all current ownership transitions and a non-reusable tombstone
before unlinking bytes. Failed checkpoints or unlinks can be retried. No upload
identity can recreate explicitly released bytes. Only the minimal immutable
upload metadata remains for conflict detection.

### Manifest enqueue

Use the existing durable enqueue endpoint with this strict item shape:

```js
{
  directory,
  item: {
    format: 'manifest-v1', content, text,
    attachmentManifest: [
      { blobId, requestId, attachmentId, filename, mimeType, size, sha256,
        agentPath?, description?, mention?: { start, end, text } },
    ],
    context: [ /* existing context/instruction/synthetic entries */ ],
    contextPreview?,
    sendConfig: { providerID, modelID, agent?, variant? },
    promptParts: {
      agents: [{ name, mention?: { start, end, text } }],
      skills: [{ id, mention?: { start, end, text } }],
      metadata?: { /* JSON values */ },
       context: [{ id?, text, description?, metadata?: { /* JSON values */ } }],
       fileReferences?: [{ uri, name, description?, mention?: { start, end, text } }],
    },
  },
}
```

`context`, `promptParts`, and its arrays may be omitted when empty. The format
rejects unknown fields, inline attachments, malformed JSON metadata and malformed
agent/skill/mention records. File descriptions and mentions belong to this
captured prompt, not to mutable upload metadata. Upload owner, digest, byte size,
filename and MIME must match exactly. Enqueue verifies retained bytes before its
atomic receipt acknowledgement. Failed validation leaves other queues intact.

Each manifest use may retain an optional `agentPath`. OpenCode receives
`agentPath ?? filename` as that occurrence's model-facing name. A present value
must be a nonempty string; null and other types are refused. Prepared-file names
retain their existing vocabulary, including relative identities, Windows and UNC
paths, literal POSIX backslashes, tabs and long strings under the aggregate
budgets. Values remain literal, with no trimming, URI decoding, separator or
dot-segment normalization. Omission adds no field or default to existing captures.

`agentPath` belongs to the captured use, alongside description and mention. It is
absent from immutable upload metadata, blob descriptors and payload chunks.
Several uses of one blob can carry different model-facing names without changing
its byte roots or file proof. The field grants no filesystem read, upload or
acknowledgement authority. Legacy-to-manifest conversion retains the original
identity and chooses a safe basename for upload, or a bounded internal label
when the basename cannot satisfy the existing strict upload-name rules.

The effective manifest name counts once per delivered occurrence in structured
content and exact outgoing JSON sizes. Historical typed legacy counters stay
unchanged so their retained root indexes remain valid. Restore and explicit retry
keep original capture and frame bytes; they never insert the field. Older strict
consumers refuse new-field captures without silently dropping the identity.

The existing enqueue receipt and retry policy are unchanged. Context IDs cover
legacy context/instruction entries first, then `promptParts.context`. Explicit
context IDs are retained; missing ones are minted before the prompt ID and all
are checked for conflicts against receipts and pending manifest reservations.
Reservation prompt and context IDs remain fenced after restart. Repeating an
existing receipt retains its original recovery behavior. The whole raw manifest participates in the immutable
request digest. Model/agent selection is captured and the first resolved Auto
selection remains frozen across retries.

### Why manifest model names are per-use fields

One immutable upload can appear under different model-facing identities in a
captured prompt. Keeping those identities with each use preserves byte ownership
and the strict upload basename without changing storage paths or upload metadata.

### Server-file references

`promptParts.fileReferences` is an optional ordered list of strict records
`{ uri, name, description?, mention?: { start, end, text } }`. It is accepted in
manifest items, typed legacy captures inside `payload-v1`, and typed prefix
composer parts. Omitted and empty lists normalize to an absent field. Existing
captures, normalized defaults and verified root-index hashes remain unchanged.

Each URI starts with literal `file:///` and names an absolute path with no host,
query or fragment. Network URLs, data URLs, extra authority slashes, backslashes,
raw whitespace, control characters, malformed percent escapes and encoded control
characters are refused. The original captured path is percent-decoded and checked
before URL dot-segment normalization, including segments that a later `..` would
remove. A URI is at most 65536 UTF-8 bytes. Names are nonempty, contain no controls
and fit 4096 UTF-8 bytes. Mention bounds are nonnegative safe
integers with `end >= start`; their text and optional descriptions remain captured
verbatim under the existing aggregate metadata budgets. Unknown fields are refused.

References grant no attachment-byte ownership, blob-index entry, file-existence
proof or acknowledgement authority. This owner never reads their paths. Each
occurrence counts toward the 4096-entry bound and logical structured-byte budget.
Its complete encoded record counts toward exact prompt and admission sizes.
Reference-only captures are valid. Automatic delivery appends references after
that source item's prepared byte attachments, preserving occurrence order,
descriptions and mentions in official OpenCode `files` records. Native prefix
adapters retain the same per-source ordering when combining captures.

Supported full non-blob take adapters preserve reference-only `promptParts` and
can requeue them through the existing ordinary enqueue route. This compatibility
shape permits no typed agents, skills, metadata or context identities; those keep
their existing typed handoff rules. Ordinary requests without the field retain
their original serialization and behavior. The old native inline take and prefix
formats refuse items containing typed prompt parts because their consumers cannot
preserve them. Native callers use manifest or payload handoffs for references.
Shared web, Electron, hosted-mobile and Capacitor UI still cannot take blob-backed
items through their legacy full-take routes. VS Code retains its foreground queue.

Automatic dispatch reads verified owned bytes and posts official OpenCode 2.2.0
`files`, `agents`, `skills` and `metadata` fields. It posts synthetic `text`,
`description` and `metadata` under the captured context IDs before the prompt.
No new OpenCode request field is invented. The size policy below keeps known
unattempted refusals queued. Other byte, preparation and admission failures keep
the existing explicit uncertain-retry behavior. Aggregate OpenCode
requests remain subject to the upstream server and provider limits.

Manifest prompt bodies stream JSON with an exact encoded `Content-Length`.
The length and complete metadata plan are computed before reading file bytes.
Fetch pulls at most 64 KiB of encoded JSON per chunk and retains only the current
verified file buffer, bounded to 20 MiB. Base64 is encoded in 48 KiB raw chunks;
no array of complete data URIs or full attachment JSON string is constructed.
Repeated references remain separate official file parts and do not multiply
preparation memory. Enqueue and capture verify each distinct immutable blob once
per item. Auto routing reads only captured file names and media types.

The existing 15-second per-RPC deadline covers stream reads, encoding, sending
and response handling. Refusal, completion or abort cancels the body producer.
The existing 20 MiB file and 40 MiB document-image budgets are unchanged. Two
complete 40 MiB document-image bundles fit within the outgoing policy.

### Queue-server prompt size policy

`MANIFEST_PROMPT_BYTE_LIMIT` in `prompt-body.js` is `128 * 1024 * 1024` encoded
JSON bytes. It applies to each manifest-backed prompt sent by the queue server.
The serializer's exact `contentLength` includes JSON syntax, UTF-8 metadata,
escaping and every file's base64 expansion. Equality is accepted; one byte over
is refused. The check allocates no attachment bytes or complete base64 strings.

Fresh oversized enqueues retain their item, attachments and immutable receipt.
They return the existing response shape with `state: 'queued'`, `attempt: 0`
and the additive `blocked: 'prompt_body_too_large'` value. The queue projection
exposes the same reason through `queueDelivery.blocked`. Exact repeats, lookup
and restart retain this error. Restored unattempted items are checked before
write-ahead uncertainty, routing, model changes, synthetic context or prompt
admission. The stream constructor also enforces the same ceiling.

A size-blocked head holds its own queue's FIFO position without automatic retry.
Unrelated sessions continue. Its owner can use manifest take to edit the complete
capture, explicitly remove it, or reorder the queue. Taking, removing or moving
that blocked head resumes eligible successors through the live-idleness guards.
Native clients accept this new blocked value and must permit edit/take of an
unattempted queued item. Ordinary retry cannot override the policy. Older
uncertain deliveries retain uncertainty and same-ID observation recovery; a
retry can confirm an already-admitted prompt but cannot post new oversized data.

This ceiling does not apply a hypothetical base64 calculation to native prefix
handoffs. A native adapter that durably stages verified files and sends compact
file URIs validates its actual outgoing body. Preview, reservation and manifest
take continue to enforce their metadata and entry limits. Legacy dispatch, the
50 MiB legacy HTTP JSON limit and the 4 MiB manifest metadata limit are unchanged.

The pinned [OpenCode 2.2.0 server](https://github.com/anomalyco/opencode/blob/v2.2.0/packages/server/src/process.ts)
uses Node HTTP and Effect 4.0.1. Its [route setup](https://github.com/anomalyco/opencode/blob/v2.2.0/packages/server/src/routes.ts)
does not supply a body-size override. Effect's
[`MaxBodySize`](https://unpkg.com/effect@4.0.1/src/http/HttpIncomingMessage.ts)
defaults to unset; its [Node request adapter](https://unpkg.com/@effect/platform-node@4.0.1/src/NodeHttpIncomingMessage.ts)
passes that setting to an array-buffer reader before JSON decoding. This supplies
no finite aggregate HTTP budget. The queue's explicit 128 MiB outgoing policy
bounds its manifest requests before that reader, while streaming bounds local
preparation. Upstream deployments and providers may enforce stricter limits.

### Manifest take and queue-first prefix capture

Durable take accepts the additive `format: 'manifest-v1'` alongside `directory`
and `itemIds`. Its full `items` are manifest items with no inline file bytes.
The caller downloads each descriptor using its original upload `requestId` and
`attachmentId`, checks SHA-256, and durably journals/spools everything before
acknowledging the take. A lost response or restart replays the same manifest.
Only explicit acknowledgement removes transfer payloads from its receipt.
Returned items also carry their existing `id`, `createdAt`, optional source
enqueue `requestId`, and an empty `attachments` array. A later enqueue copies only
the documented strict input fields and retains the descriptor's original upload
identities.

Legacy inline byte attachments can be captured into this format. Their actual
data URLs are decoded, uploaded into private storage and committed before the
original items are removed. URI-only legacy attachments are refused because
this module has no server-path read contract. Oversized manifest receipts fail
before removing anything. Old take/bulk-take routes retain their legacy behavior
for legacy items and cannot hand a manifest item to a client that would discard
its references.

Manifest take counts files, agent mentions, skills and expanded synthetic
contexts across all selected items before any conversion or ownership change.
The complete handoff admits at most 4096 entries. Exact take replay and lookup
recovery of a failed checkpoint re-arm successors through the same live-idleness
checks as the initial durable take.

Queue-first native send continues to use `preview-prefix` then `reserve-prefix`,
not take. Add `format: 'manifest-v1'`, required exact `directory`, and optional
composer `promptParts` to the existing captured reservation request:

```js
{ id, itemIds, composer, sendConfig, format: 'manifest-v1', directory,
  delivery?: 'steer', promptParts? }
```

`directory` is a non-empty string of at most 16384 characters with no leading or
trailing whitespace. It must match the queue's actual captured owner before
preview or reservation, after byte preparation, and before returning an existing
reservation. A mismatch returns 409 with `code: 'queue_request_conflict'` without
changing ownership or replacing identities. Missing or malformed input returns
400. The check applies to host requests even without a directory header; existing
isolated-runtime header checks remain in force.

A manifest or payload prefix request whose items no longer lead the queue, or
are no longer eligible, while the session holds no reservation returns 409 with
`code: 'queue_prefix_stale'`. Nothing is reserved for that request and nothing
can be, so the native client treats it as a definite refusal. A send or transfer
in flight for the session, or a mismatch while a reservation exists, keeps the
plain 409 without a code: those states can still change.

The returned `reservation` and its persisted fingerprint retain `directory`.
Loading refuses missing or mismatched manifest reservation owners and preserves
the queue file for explicit recovery. An empty queue remains unavailable and
cannot acquire an owner from the caller's body. Legacy prefix requests keep their
existing shape and behavior. Responses contain
`{ revision, reservation, items, contextIds }`, with no growing queue snapshot.
They retain every queued item's full prompt parts and captured configuration.
The backend does not merge different items' top-level metadata or selections;
the native adapter owns construction of its complete captured prompt.

Preview prepares stable manifests without acquiring the prefix. Actual reserve
keeps the same item IDs in the queue, commits any inline-to-manifest conversion
with the reservation, and acknowledges only after byte ownership and reservation
are durable. Preparation excludes dispatch and destructive mutations; an invalid
or oversized capture creates no reservation. Repeating the captured request
recovers the same manifest and context IDs. Admission reconciliation alone
releases the prefix, preserving the existing FIFO barrier across lost responses
and restart. A plain manifest take still arms successors and is an edit/handoff
operation, never a replacement for that admission barrier.

The native adapter must journal the returned complete prefix and exact context
IDs, verify its destination byte ownership and preserve all returned prompt
parts before issuing the reserved prompt. Manifest handoffs use the 4 MiB total
metadata/receipt bound; the original inline journal's 64 KiB URI and 1 MiB text
limits do not apply to this format. The adapter validates its own complete
captured prompt before acknowledging any handoff.

### Blob-backed structured captures

`payload-v1` carries large context and metadata through the same durable blob
store. Legacy and inline `manifest-v1` shapes and their limits stay unchanged.
For example, a legacy item containing 9 synthetic entries of 1 MiB each uses a
payload-backed take or prefix capture to retain its complete context.

The uploaded bytes are one strict UTF-8 JSON document. The full manifest form is:

```js
{ directory, item: {
  format: 'manifest-v1', content, text, attachmentManifest,
  context?, contextPreview?, sendConfig, promptParts?,
} }
```

This inner item uses the strict manifest item schema above, including actual
attachment descriptors, context instructions and metadata, agents, skills,
mentions and explicit context identities. Unknown fields are refused at every
typed level. Metadata retains arbitrary JSON values. The existing empty-array
defaults apply when optional arrays are omitted. An empty item is permitted only
as a prefix composer. It cannot be enqueued alone.

The additive typed legacy form preserves original-request capacity without
adding a manifest envelope to an item at the 50 MiB boundary:

```js
{ directory, item: {
   content?, text?, agentMention?, contextPreview?, context?, sendConfig,
   promptParts?: { fileReferences?: [{ uri, name, description?, mention? }] },
  attachments?: [{ id?, filename, mimeType, size?, source?, serverPath?, dataUrl }],
} }
```

This form has no inner `format` field. It rejects every unknown field, goal and
scheduled-task provenance. `content` defaults to an empty string, `text` defaults
to `content`, arrays default to empty, and legacy file `size` and `source` default
to `0` and `local`. `serverPath` remains captured metadata and never authorizes a
path read. Each file must contain actual, validated inline data-URL bytes within
the existing 20 MiB prepared-file bound. Those bytes are owned by the capture's
chunks. The manifest form continues to use separately uploaded attachments.
Both forms retain context instructions and JSON metadata without truncation.

Legacy conversion omits redundant defaults and moves attachment IDs of at most
256 characters into verified root bindings. Longer IDs remain in the chunked
capture. Numeric metadata uses the shortest round-tripping JSON spelling, so
original `1e20` values do not consume extra capture capacity after legacy parsing.
The complete encoded length is checked before concatenating the capture.

Split the complete encoded document into non-empty chunks of at most 20 MiB.
Upload each using the existing binary attachment endpoint and ordinary
authentication. A capture contains at most 4096 chunks. No chunk may repeat a
blob identity; repeated content in separately owned chunks still counts in full.
The complete encoded document is at most `50 * 1024 * 1024` bytes, including its
owner and JSON syntax. This is the existing original-request JSON budget. In the
manifest form, binary attachments remain separate uploads with their existing
per-blob limit. Typed legacy inline bytes count toward the original capture's
50 MiB budget.

The durable enqueue body is:

```js
{ directory, item: {
  format: 'payload-v1',
  payloadManifest: {
    version: 1, encoding: 'utf-8-json', totalBytes, sha256,
    chunks: [{ ordinal, blobId, requestId, attachmentId, filename, mimeType,
      size, sha256 }],
  },
} }
```

Ordinals are contiguous and zero-based in array order. Chunk lengths must sum
exactly to `totalBytes`. Lengths, the aggregate budget and duplicate identities
are checked before allocating or reading the complete capture. Each owned chunk
must match its stored descriptor and SHA-256; the complete byte stream must match
the outer SHA-256. UTF-8 decoding is fatal on malformed bytes. A BOM, malformed
JSON, invalid item schema or mismatched inner directory refuses capture before
enqueue ownership. There are no URLs, path reads or unvalidated source fields.
The immutable request hash covers the owner and complete pointer, including every
chunk descriptor and the whole-capture digest.

The server derives a verified compact item before acknowledging enqueue:

```js
{ id, createdAt, requestId?, format: 'payload-v1', payloadManifest, payloadIndexHash,
  payloadIndex: {
    directory, sendConfig, preview, contextPreview?, entryCount, contextIds,
    attachmentBlobIds, promptBytes, admissionBytes, structuredBytes,
    legacyAttachmentIds?,
  },
}
```

`payloadIndex` and its SHA-256 integrity binding `payloadIndexHash` are
server-generated and refused in enqueue input. Startup checks the binding over
the schema-normalized manifest and index before granting ownership authority.
The private queue root remains trusted application-owned storage. `preview` is
a display summary of at most 103 characters; it never replaces the captured
content or text. `contextIds` contains the complete frozen synthetic identity
sequence, including instructions before their context and then official prompt
contexts. Existing enqueue receipts supply their original IDs. Conversion of a
legacy item derives stable IDs from its exact owner and item identity. Explicit
IDs remain unchanged. `attachmentBlobIds` contains all distinct attachment roots
found inside a manifest-form capture. Typed legacy attachments have no separate
blob roots; their capture chunks own their inline bytes. `legacyAttachmentIds`
is present only for the typed legacy form and has one slot per file. A string
binds the file's original short ID; a null slot retains its longer ID from the
capture. A fresh typed legacy upload lacking an ID gets a deterministic ID from
its whole-capture digest and file ordinal. The adapter must apply these bindings
when materializing an edit or send. Re-encoding an edited draft includes its
materialized IDs in the newly uploaded typed capture.

`promptBytes` is the exact encoded queue-server prompt length, including every
base64 file occurrence. `admissionBytes` is the maximum encoded body size across
that prompt and every synthetic context request with its frozen ID. Both are
measured before allocating complete outgoing JSON. `structuredBytes` counts
delivered UTF-8 text, descriptions, compactly encoded JSON metadata, agents,
skills, mentions and file names, excluding binary bytes and protocol envelopes.

The pointer and index stay within 4 MiB. The queue snapshot and pending transfer
store only this compact item. Startup validates schemas, owners and blob indexes
without reading or expanding capture chunks. Lookup returns the existing compact
receipt. A missing or corrupt blob therefore fails its active operation without
discarding other queues. No expanded capture is cached. Payload materialization
is serialized per runtime and retains at most one original capture of 50 MiB,
its bounded decoding/parsing copies, and the current blob buffer of at most
20 MiB. The materialized schema and frozen index are compared again before use.
Attachment preparation and its streaming RPC deadline retain their existing
bounds.

Payload enqueue uses the same durable receipt, exact-repeat and explicit
uncertain-retry protocol. Its exact encoded 128 MiB admission preflight runs before
any context or prompt POST, including when numeric metadata would expand on
serialization. An amplified attachment alias list stays queued with
`blocked: 'prompt_body_too_large'` and `attempt: 0`. Synthetic content and metadata
are also bounded by the original 50 MiB structured capture. Retained bytes are
verified before any payload-backed send. A corrupt capture keeps that item's
identity uncertain and requires explicit retry after repair; healthy sessions
continue independently.

Durable take accepts `{ directory, itemIds, format: 'payload-v1' }`. It converts
legacy or inline manifest items into typed captures, and retains payload-backed
items unchanged. Failed conversion leaves the originals queued. Responses,
lookup and exact take replay contain compact items with their verified indexes,
never expanded context JSON. The caller downloads every chunk and attachment
using its original upload identities, verifies descriptors, order and digests,
and durably spools the capture before acknowledging the transfer. Source enqueue
receipts retain their original message and context IDs through this handoff.
Legacy and `manifest-v1` takes refuse payload-backed items.

Legacy conversion keeps the typed legacy form, including its original inline
file metadata and bytes. It does not add full manifest descriptors to a capture
already at its original-request boundary. Native adapters parse the documented
inner union, restore the root identity bindings, and stage inline files into
their existing local attachment spool. Full manifest captures use their original
owned attachment descriptors and downloads.

Payload prefix requests use a typed composer pointer:

```js
{ format: 'payload-v1', directory, id, itemIds,
  composer: { format: 'payload-v1', payloadManifest }, delivery?: 'steer' }
```

The composer's captured item supplies `sendConfig`, text, context, attachments
and prompt parts. There are no separate mutable composer strings or configuration
fields in this format. Responses remain
`{ revision, reservation, items, contextIds }`. `reservation.composer` and every
returned item contain their compact pointer and verified index. The combined
prefix has at most 4096 entries and at most 50 MiB of logical structured content,
counting every selected item and composer occurrence. Backend capture preserves
each item's configuration and top-level metadata independently; the native
adapter constructs the actual outgoing prompt. Its actual encoded request must
obey the 128 MiB guard, including the file URIs it really sends. No hypothetical
base64 limit is applied to native staged-file handoffs.

The exact prefix directory checks, durable reservation fingerprint and admission
barrier apply unchanged. The fingerprint covers the composer pointer, and the
stored reservation additionally retains its verified composer index. Preview
may prepare chunk uploads while the original items remain queued. Reserve
atomically commits compact items and the reservation. Frozen context IDs are
returned in item order and then composer order, and fence future captures after
restart. Known admission of the reserved prompt is the only normal release.

Chunks and hidden attachment roots remain protected by queued and uncertain
items, pending takes and reserved composers across restart. Explicit release
checks those root indexes before tombstoning and unlinking. Unacknowledged takes
retain every root even after their source queue empties. Unknown uploads have no
automatic replay or age collection. Prepared conversion uploads can be reused
by an exact capture retry; failed captures never transfer the original items.

The native adapter stores local spool descriptors and these compact indexes
within its 16 MiB journal. It validates and materializes the typed capture only
for the active edit or send, binding contexts and legacy attachments to the
returned frozen IDs. Keeping
both the expanded request and an expanded rollback draft in the journal would
duplicate large captures and is not this protocol's ownership contract.

### Decisions

#### Why uploads use binary envelopes

Separate bounded binary uploads preserve prepared bytes without base64 expansion
against the existing JSON limit. An immutable owner header in each envelope
rejects conflicting repeats after a crash between the blob and root commits.
Publication uses a no-replace link of the synced staging file and removes its
staging link before acceptance. Existing envelopes cannot be overwritten by a
conflicting writer. The configured filesystem must support same-directory hard
links; retained files with extra links fail closed, including an interrupted
publication that needs explicit storage recovery.

#### Why release needs an explicit ownership check

An old native journal may recover a lost response without another enqueue.
Age cannot establish that its bytes are unused. Release checks all queue and
pending-take references, then persists a non-reusable tombstone before unlinking.

#### Why structured captures have a separate format and root index

Typed chunked captures preserve large context and metadata within the existing
50 MiB original-request limit while receipts remain bounded to 4 MiB. A verified
root index protects hidden attachment ownership and frozen identities during
restart and release without expanding every capture into memory.

The focused contract check uses the web package's Vitest runner:
`bun run --cwd packages/web test server/lib/message-queue/receipts.test.mjs server/lib/message-queue/prompt-body.test.mjs server/lib/message-queue/structured-payloads.test.mjs`.
The existing `runtime.test.js` remains the owning package's legacy regression
suite. A mocked OpenCode contract does not establish the behavior of an actual
OpenCode deployment; runtime integration must exercise its real admissions.

## UI ownership

`packages/ui/src/stores/messageQueueStore.ts` is the projection: see its
section in `packages/ui/src/stores/DOCUMENTATION.md`. A session in an isolated
space is queued in this module inside that space, not on the host: the host's
runtime sees the host's events only and never delivers into a space. The host
forwards the space's `openchamber:message-queue.updated` changes to the
browsers, see "Sessions and events" in
`packages/web/server/lib/spaces/DOCUMENTATION.md`. VS Code intentionally
does not use this module; with all OpenChamber webviews closed, queued
messages there are not delivered.
