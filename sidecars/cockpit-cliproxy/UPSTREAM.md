# CLIProxyAPI upstream sync

The bundled source under `third_party/CLIProxyAPI` is synchronized to the
`CLIProxyAPI` `v7.2.155` source tree with Cockpit compatibility shims. The outer
sidecar module declares the same upstream version and continues to use the local
`replace` target.

## Codex sync scope

- Upstream repository: `https://github.com/router-for-me/CLIProxyAPI`
- Synchronized module baseline: `v7.2.155` (`7fac6b15`)
- Codex Live baseline: the upstream Live implementation plus later capability,
  client-secret, WebSocket, media-relay, and TCP-proxy commits
- Cockpit integration routes: `POST /v1/live`, `GET /v1/live/:call_id`,
  `POST /v1/realtime/calls`, and `GET /v1/realtime/calls/:call_id`

Cockpit uses the upstream Codex executor, Responses WebSocket, model catalog,
auth scheduler, Live request/sideband/realtime implementation, and supporting
config/proxy utilities. The outer relay keeps API-key account scoping and rejects
provider-gateway profiles because Codex Live requires a ChatGPT OAuth credential.

## Codex/API policy convergence

The local `CLIProxyAPI` v7.2.157 tree (`09a29bd3`) is the reference for Codex and
Responses API behavior. This is a targeted policy alignment, not a full upgrade
of the bundled v7.2.155 dependency tree.

- Removed the local device/session/full fingerprint rewrite and automatic host
  projection; old account fields are retained only for import/export compatibility.
- Removed the local official-client restriction and third-party-client exceptions,
  including global/per-account controls and runtime metadata projection. Clients
  still require the normal API key and remain subject to account-scoping rules.
- Removed the API-Service-only capacity wrapper, error-code rewriting and
  transient-request cooldown bypass. The v7.2.157 bootstrap capacity detection,
  `model_not_found` account rotation, optional model-level cooling, and permanent
  OAuth failure handling are used instead.
- Removed extra host/WebSocket input namespace deletion and the retired Rust
  gateway dispatch/rejected-field retry path. Removed the uncalled handler-level
  encrypted-content retry helper; request history is not stripped and replayed.
  Protocol translators,
  signature validation, token accounting and upstream session safety remain intact.
- Retained Cockpit token authority, scoped keys, instance-specific gateways,
  Responses Lite integration, and Agent Identity as local extensions.
  Agent Identity has no equivalent in the reference tree and is not removed
  without an explicit decision about existing accounts.

## Targeted compatibility sync (2026-09)

The current local CLIProxyAPI and sub2api sources were reviewed for failure
boundary and proxy behavior. The synchronized fixes are intentionally limited
to compatibility seams: cancellation-aware HTTP/SOCKS proxy dialing, HTTPS
proxy CONNECT support for Codex WebSockets, route-aware WebSocket reuse and
ping writes, service-tier normalization, structured Responses stream errors,
and image-stream failure/truncation propagation. This is not a dependency
version bump; the v7.2.155 baseline and Cockpit-specific account scoping remain
unchanged.

The current Codex executor also derives the native OAuth routing hint after
translation, so a normalized `service_tier` cannot disagree with the header.
The larger upstream request-capability refactor is intentionally not copied
until the corresponding translator and thinking APIs are synchronized together.
- Synchronized the v7.2.157 Responses transport fixes: official nested SSE error
  payloads with preserved sequence numbers, split-CRLF framing, WebSocket prewarm
  follow-up merging, and named `function_call_output` passthrough.
- Synchronized Codex schema/header compatibility and model additions: unsupported
  Unicode property regexes are removed from tool schemas, `$CPA-SESSION-ID`
  custom headers resolve from the canonical request session, and the
  `gpt-image-2.5-flare` / `gpt-image-2.5-sunburst` image variants are accepted.
  Cockpit keeps `gpt-5.6-luna` and `gpt-image-2.5` as its local image defaults.
- Synchronized the xAI (Grok) chat path with the reference tree: the HTTP chat
  executor, auth helpers and the exported `util.InlineLocalRefs` now match
  `CLIProxyAPI` HEAD (`xai_executor.go`, `xai_executor_request.go`,
  `xai_executor_response.go`, `xai_executor_execute.go`, `xai_executor_stream.go`,
  `xai_executor_media.go`, `internal/auth/xai/*`). This brings the Codex-facing
  compatibility fixes with it: MCP-namespaced `automation_update`
  (`mcp__codex_app__automation_update`, `codex_apps__automation_update`),
  `$ref`/union tool-schema simplification with local `$ref` inlining, the
  200-tool limit with namespace folding + dispatcher tool restoration,
  image/video requests following the OAuth chat base URL, forced
  image-generation tool choices, and `previous_response_id` passthrough on
  `/responses/compact`.
  The bundled `internal/auth/xai/pkce.go` was removed because the reference tree
  dropped PKCE types for the device-code flow.
  Later WebSocket and request-compatibility updates are recorded below; the
  bundled dependency remains v7 rather than importing the entire v8 runtime.

## Grok protocol alignment (2026-10-16)

Reference: `origin/main` at `5be2cdab0869cbaa3d401891f740aebf7e15aa99`.
Read the pinned tree with `git show`; an older local checkout is not the reference.

- Go refreshes the advertised Grok protocol version from the public npm registry
  in the background with a supported fallback, a minimum of 1.0.13, bounded
  requests, and lifecycle cancellation. Invalid or obsolete header overrides
  cannot downgrade requests. Rust uses the same validation and bounded cached
  lookup via the selected transport, without depending on an installed CLI.
- Both paths normalize unsupported web-search fields, forced hosted searches,
  client functions named web_search, and Responses tool schemas. Rust also
  normalizes xAI reasoning text events and status-specific OAuth/quota errors.
- Grok agent-message input adaptation is independent of the optional client
  optimization switch. Completed plaintext collaboration calls carry explicit
  `encrypted_function_args: []`; existing encrypted evidence remains intact.
  Rust also supports inline compaction controls and the official integer-valued
  tool schema fields, without rounding large IDs or modifying custom tool text.
- Compact controls lower their tool history before dropping unsupported fields.
  A valid compact state replaces its old prefix only on the upstream wire; the
  local source history remains available. Rust uses CPA's encrypted-state shape
  checks and does not assign ownership to state omitted from the outgoing request.
  Shape validation does not establish provider provenance or decryptability.
- Go applies payload overrides at the final upstream boundary, restores sparse
  apply_patch events without losing buffered arguments, and rejects truncated
  calls. WebSocket control writes do not wait for data writes; re-registering an
  executor sharing the same session store preserves its active connections.
- Cockpit retains client tool-search execution, account-scoped identities and
  history, automatic continuation, bounded state, and non-destructive replay.
  These are intentional local capabilities, not deletions to match CPA's tool
  filtering. Independent TTS additions and CPA's internal v8 runtime abstractions
  are not imported as part of this Responses compatibility update.

## Portable agent messages and terminal events

Keep the agent-message normalization from Cockpit PR #2609: private routing
metadata belongs in an appended input_text block, not at the top level of a
standard Responses message. Both the provider gateway and xAI executor tests
cover preservation of the original message and its routing information.
Non-streaming Codex execution also recognizes response.done through its
completed, failed or incomplete status (PR #2332), without treating an unknown
terminal status as success.

## Request route observation

Cockpit records the connection actually used by each request. The Codex uTLS
HTTP/2 transport must participate in that observation explicitly: its direct
`http2.ClientConn.RoundTrip` path does not emit the standard transport's
`httptrace.GotConn` callback. Keep this hook and the fallback transport's route
metadata when synchronizing upstream transport changes. Route lookup stays
asynchronous and never substitutes the account's current selection for an
unrecorded historical connection.

## Update procedure

1. Compare the next upstream tag against the targeted `v7.2.157` policy baseline,
   while separately assessing a full dependency-tree upgrade from `v7.2.155`.
   Begin with
   `internal/runtime/executor/codex*`, `internal/client/codex`, `sdk/cliproxy/auth`,
   `sdk/api/handlers`, `internal/config`, `internal/registry`, and `sdk/proxyutil`.
2. Port the complete dependency closure instead of copying a single changed file.
3. Preserve Cockpit-specific executor identity, token ownership, account routing,
   quota, and request-policy compatibility shims.
4. Run the focused Codex executor/Live tests, sidecar tests, and a sidecar build.
