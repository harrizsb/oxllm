# Story e03s01: Append provider-specific upstream headers and JSON body parameters

**1. Story ID:** e03s01 · **2. Title:** Append provider-specific upstream headers and JSON body parameters · **3. Type:** feat · **4. Risk:** P2 (config surface + request shaping; no routing change) · **5. Context (domain):** some upstreams require vendor headers (for example `X-TokenTable-Modalities: text`) or body flags (for example `"modalities": ["text"]`) per provider. Today the gateway can only set `user_agent`; operators cannot express the rest, so requests to such upstreams fail with 4xx.

**6. Zoom-out (mandate):**
- *ProviderConfig* — Purpose: parse one `[[providers]]` block. Callers: Config::load_from_file, apply pipeline, /validate. Contracts: `deny_unknown_fields`; every field validated before publish.
- *attempt_chat_completion* — Purpose: one shared upstream chat attempt. Callers: POST /v1/chat/completions loop and POST /admin/ping. Contracts: model rewrite, auth, UA, telemetry parity.
- *create_embeddings* — Purpose: upstream embeddings loop. Callers: POST /v1/embeddings. Contracts: model rewrite, auth, per-provider counters.

**7. Requirements (deltas):**

#### ADDED: `ProviderConfig.headers`
**Before:** Only `user_agent` exists; no way to attach vendor headers.
**After:** Optional map of header name to value, written property-like in TOML:
`headers = { X-TokenTable-Modalities = "text" }`. Validation rejects: invalid header names (non-token), invalid values (control characters, CR/LF), and reserved overrides (`authorization`, `content-type`, `user-agent`, `host`, `content-length`) case-insensitive. Valid pairs are pre-built into `HeaderName`/`HeaderValue` at state load and appended to chat and embeddings upstream requests after the gateway-owned headers. Absent or empty map changes nothing.

#### ADDED: `ProviderConfig.extra_body`
**Before:** Client and gateway fully own the JSON body; vendor flags like `"modalities": ["text"]` cannot be expressed.
**After:** Optional TOML table of JSON values merged into the upstream chat and embeddings bodies:
`extra_body = { modalities = ["text"] }`. Validation rejects the keys `model` and `stream` (gateway-owned) and any TOML datetime (not JSON-representable). Merge is insert-only for gateway keys: client-supplied values for a provider key are replaced by the provider value, but the gateway-owned `model` rewrite always wins (applied after merge). Absent or empty table changes nothing.

**8. Success criteria:** (a) a provider with `headers` and `extra_body` sees both applied on chat, embeddings, and admin ping requests; (b) provider `extra_body` wins over a colliding client key, `model` always reflects the routed target model, `stream` always reflects the client request; (c) invalid header names/values, reserved header overrides, `model`/`stream` keys, and TOML datetimes are rejected at validation with clear diagnostics; (d) providers without the new fields behave byte-identically to before; (e) fmt/clippy/tests green; release binary <15 MB.

**9. Implementation steps:** see e03s01-tasks.yaml tasks 1–5.

**10. Verification script (manual):** run `cargo run -- serve` with a mock upstream; send a chat request with a colliding client key and confirm the upstream sees the provider value, the routed model, and the configured header; send an embeddings request and repeat; POST /admin/ping and repeat; POST /validate with a config containing an invalid header and a `model` extra_body key and confirm rejection.

**11. Test plan:** unit tests in oxllm-core for parsing, defaults, and each rejection path; integration tests in oxllm with a capturing mock upstream: chat forwards header + body, client collision resolved in favor of the provider, model stays routed, stream stays client-owned, embeddings parity, ping parity, UA + headers + extra_body coexist.

**12. Security considerations:** header names/values validated with the `http` crate parsers, blocking request smuggling via CR/LF; reserved overrides rejected so auth and framing stay gateway-owned; `model`/`stream` stay gateway/client-owned so routing and streaming semantics cannot be subverted by config; values are operator-supplied from the 0600 config file behind the tailnet boundary; no secrets are logged — headers are applied but never emitted at info level.

**13. Observability:** no new metrics; existing per-provider counters, request log, and telemetry unchanged. Debug-level tracing already covers routing; header values are deliberately not logged.

**14. Out of scope:** per-virtual-model headers, response header filtering, header templating/secrets interpolation beyond existing `${VAR}` env expansion, streaming semantics changes, dashboard editor schema changes (raw TOML editor already accepts the new keys).

**15. Risks:** a bad operator value could corrupt upstream requests — mitigated by fail-closed validation at load and at /validate; serde map ordering must not affect behavior — merge is per-key idempotent.

**16. Dependencies:** none. **17. Acceptance criteria (Gherkin):**
```gherkin
Scenario: Provider headers and body flags reach the upstream
  Given a provider configured with headers = { X-TokenTable-Modalities = "text" } and extra_body = { modalities = ["text"] }
  When a chat completion, an embedding, or an admin ping is routed to it
  Then the upstream request carries the X-TokenTable-Modalities header
  And the upstream JSON body contains modalities = ["text"]

Scenario: Provider values win over colliding client keys, gateway keys stay owned
  Given the client sends modalities = ["audio"] in its request body
  When the request is routed to a provider with extra_body = { modalities = ["text"] }
  Then the upstream sees modalities = ["text"]
  And the upstream model field equals the routed target model
  And the upstream stream field equals the client request stream value

Scenario: Invalid configuration is rejected before publish
  Given a config with an invalid header name, a header value containing a newline, a reserved header override, an extra_body model or stream key, or a TOML datetime value
  When the config is loaded or validated
  Then validation fails with a diagnostic naming the provider and the offending key
  And nothing is applied

Scenario: Providers without the new fields are unchanged
  Given a provider block without headers or extra_body
  When requests are routed to it
  Then upstream requests are byte-identical to the pre-e03 behavior
```
**18. Definition of done:** all tasks passing; verification evidence in specs/verifications/; tests cover every rejection path. **19. Open questions:** none (merge semantics locked: provider wins over client collisions; model/stream protected). **20. References:** specs/planning-context.yaml; CLAUDE.md; e02s01 spec for the shared attempt path.
